use std::time::Duration;

use pulsebeam_runtime::rand::RngCore;
use tokio::time::Instant;

use crate::{
    control::MAX_SEND_AUDIO_SLOTS,
    id::AudioSelectorSlotId,
    rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline},
    track::StreamId,
};

pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Slot is considered dead if no packet has arrived within this window.
const DEAD_TIMEOUT: Duration = Duration::from_millis(2000);
/// After a slot is stolen, this window protects the new owner from being immediately
/// evicted by delayed packets from the previous owner.
const NEWBORN_IMMUNITY: Duration = Duration::from_millis(300);
/// Half-life for contention power decay. This keeps ranking relative, but avoids
/// slot thrash from single low-power packets (e.g. transient DTX/silence frames).
const POWER_HALF_LIFE: Duration = Duration::from_millis(300);

struct SlotTimeline {
    timeline: Timeline,
    pending_marker: bool,
}

struct SlotState {
    owner: Option<StreamId>,
    /// Wall-clock time of the most recent packet. `None` means the
    /// slot has never been used.
    last_arrival_ts: Option<Instant>,
    /// Wall-clock time until which this slot cannot be stolen (newborn immunity).
    immunity_expiry: Instant,
    /// Power of the most recent packet, used for relative contention ranking.
    last_power: f32,
    slot_timeline: SlotTimeline,
}

impl SlotState {
    /// A slot is "dead" if it has no owner, or its owner has been silent for DEAD_TIMEOUT.
    #[inline]
    fn is_dead(&self, now: Instant) -> bool {
        match (self.owner, self.last_arrival_ts) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(_), Some(ts)) => now.duration_since(ts) > DEAD_TIMEOUT,
        }
    }

    /// Returns true if this slot is currently immune to being stolen.
    #[inline]
    fn is_immune(&self, now: Instant) -> bool {
        now < self.immunity_expiry
    }
}

pub struct TopNAudioSelector {
    slots: [SlotState; SELECTOR_SLOTS],
}

impl TopNAudioSelector {
    pub fn new<R: RngCore>(rng: &mut R) -> Self {
        // immunity_expiry is set to Instant::now() at construction; any packet arriving
        // later will have now >= construction_time, so is_immune() returns false.
        let init_instant = Instant::now();
        Self {
            slots: std::array::from_fn(|_| SlotState {
                owner: None,
                last_arrival_ts: None,
                immunity_expiry: init_instant,
                last_power: 0.0,
                slot_timeline: SlotTimeline {
                    timeline: Timeline::new(AUDIO_FREQUENCY, rng),
                    pending_marker: false,
                },
            }),
        }
    }

    #[inline]
    pub fn filter(
        &mut self,
        stream_id: StreamId,
        pkt: &mut RtpPacket,
    ) -> Option<AudioSelectorSlotId> {
        // Step 1: Parse relative power and wall-clock arrival time.
        let Some(audio_level) = pkt.ext_vals.audio_level else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                stream_id = %stream_id.0,
                "audio selector dropped packet due to missing audio level"
            );
            return None;
        };
        let power = rfc6464_to_power(audio_level);

        let now = pkt.arrival_ts;

        // Step 1.5: Decay all slot powers to keep ranking fresh over time.
        for slot in &mut self.slots {
            slot.last_power = decayed_power(slot.last_power, slot.last_arrival_ts, now);
        }

        // Step 2: Owner fast-path — update timestamps and forward.
        let owner_slot = self.slots.iter().position(|s| s.owner == Some(stream_id));
        if let Some(idx) = owner_slot {
            let slot = &mut self.slots[idx];
            slot.last_arrival_ts = Some(now);
            // Peak-hold with decay: a single quiet packet must not instantly demote rank.
            slot.last_power = slot.last_power.max(power);
            Self::rewrite_slot_timeline(&mut slot.slot_timeline, false, pkt);
            return Some(AudioSelectorSlotId::new(idx));
        }

        // Step 3: Slot stealing, in priority order among non-immune slots.

        // Priority A: Dead slot (no owner, or silent for >DEAD_TIMEOUT).
        let victim = self
            .slots
            .iter()
            .position(|s| !s.is_immune(now) && s.is_dead(now));

        // Priority B: Active contention — strict relative rank by power.
        let victim = victim.or_else(|| {
            let (quietest_idx, quietest_slot) = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_immune(now))
                .min_by(|(_, a), (_, b)| {
                    a.last_power
                        .partial_cmp(&b.last_power)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })?;
            if power > quietest_slot.last_power {
                Some(quietest_idx)
            } else {
                tracing::debug!(
                    target: crate::log::TARGET_AUDIO,
                    stream_id = %stream_id.0,
                    incoming_power = power,
                    quietest_power = quietest_slot.last_power,
                    quietest_slot = quietest_idx,
                    "audio selector dropped packet in contention"
                );
                None
            }
        });

        // Step 5: Execute the steal.
        let victim_idx = victim?;
        let slot = &mut self.slots[victim_idx];
        slot.owner = Some(stream_id);
        slot.last_arrival_ts = Some(now);
        slot.immunity_expiry = now + NEWBORN_IMMUNITY;
        slot.last_power = power;
        Self::rewrite_slot_timeline(&mut slot.slot_timeline, true, pkt);
        Some(AudioSelectorSlotId::new(victim_idx))
    }

    #[inline]
    fn rewrite_slot_timeline(slot: &mut SlotTimeline, switched: bool, pkt: &mut RtpPacket) {
        if switched {
            slot.timeline.rebase_audio(pkt);
            slot.pending_marker = true;
        }
        slot.timeline.rewrite(pkt);
        if slot.pending_marker {
            pkt.marker = true;
            slot.pending_marker = false;
        }
    }

    pub fn remove_track(&mut self, id: StreamId) {
        for slot in &mut self.slots {
            if slot.owner == Some(id) {
                slot.owner = None;
                break;
            }
        }
    }

    pub fn cleanup(&mut self) -> Option<()> {
        let now = Instant::now();
        for slot in &mut self.slots {
            if slot.owner.is_some() && slot.is_dead(now) {
                slot.owner = None;
            }
        }
        Some(())
    }
}

#[inline(always)]
fn rfc6464_to_power(level: i8) -> f32 {
    let clamped = level.clamp(-127, 0) as f32;
    10.0_f32.powf(clamped / 10.0)
}

#[inline(always)]
fn decayed_power(power: f32, last_arrival_ts: Option<Instant>, now: Instant) -> f32 {
    let Some(last) = last_arrival_ts else {
        return 0.0;
    };
    let dt = now
        .checked_duration_since(last)
        .unwrap_or(Duration::from_millis(0));
    if dt.is_zero() {
        return power;
    }
    let half_life = POWER_HALF_LIFE.as_secs_f32();
    if half_life <= 0.0 {
        return power;
    }
    let decay = 2.0_f32.powf(-(dt.as_secs_f32() / half_life));
    power * decay
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use str0m::rtp::ExtensionValues;
    use tokio::time::Instant;

    use crate::entity::{ParticipantId, TrackKind};
    use crate::rtp::RtpPacket;
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};

    const TICK_MS: u64 = 20;

    fn test_rng() -> impl RngCore {
        seeded_rng(42)
    }

    fn new_sel() -> TopNAudioSelector {
        TopNAudioSelector::new(&mut test_rng())
    }

    fn make_stream() -> StreamId {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        (
            ParticipantId::new(&mut seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed)))
                .derive_track_id(TrackKind::Audio, "test"),
            None,
        )
    }

    /// Build a packet with the given audio level arriving at `arrival_ts`.
    fn pkt_at(base: Instant, offset_ms: u64, level: i8) -> RtpPacket {
        let mut ext_vals = ExtensionValues::default();
        ext_vals.audio_level = Some(level);
        RtpPacket {
            ext_vals,
            arrival_ts: base + Duration::from_millis(offset_ms),
            playout_time: base + Duration::from_millis(offset_ms),
            ..Default::default()
        }
    }

    #[test]
    fn first_packet_after_slot_claim_has_marker() {
        let mut sel = new_sel();
        let s = make_stream();
        let base = Instant::now();

        let mut first = pkt_at(base, 0, -20);
        let slot = sel
            .filter(s, &mut first)
            .expect("packet should be selected");
        assert!(
            first.marker,
            "first packet on a newly claimed slot must set marker"
        );

        let mut second = pkt_at(base, TICK_MS, -20);
        let slot2 = sel
            .filter(s, &mut second)
            .expect("packet should be selected");
        assert_eq!(slot, slot2, "owner should remain in same slot");
        assert!(
            !second.marker,
            "subsequent packets from the same owner must not keep forcing marker"
        );
    }

    #[test]
    fn marker_is_set_when_slot_owner_switches() {
        let mut sel = new_sel();
        let a = make_stream();
        let b = make_stream();
        let base = Instant::now();

        let mut first = pkt_at(base, 0, -20);
        let slot = sel
            .filter(a, &mut first)
            .expect("first packet should be selected");
        assert!(first.marker);

        // After DEAD_TIMEOUT the old owner is considered dead, so the new owner can steal the slot.
        let mut switched = pkt_at(base, 2500, -10);
        let slot2 = sel
            .filter(b, &mut switched)
            .expect("second owner packet should be selected");
        assert_eq!(slot, slot2, "new owner should reuse the dead slot");
        assert!(
            switched.marker,
            "first packet after a slot owner switch must set marker"
        );
    }

    /// Helper: send `n` packets at the provided level, return the StreamId.
    fn add_stream_at_level(
        sel: &mut TopNAudioSelector,
        base: Instant,
        start_ms: u64,
        level: i8,
    ) -> StreamId {
        let id = make_stream();
        for t in 0..5u64 {
            sel.filter(id, &mut pkt_at(base, start_ms + t * TICK_MS, level));
        }
        id
    }

    /// Fill all SELECTOR_SLOTS with streams at a fixed level.
    fn fill_slots_at_level(
        sel: &mut TopNAudioSelector,
        base: Instant,
        start_ms: u64,
        level: i8,
    ) -> Vec<StreamId> {
        (0..SELECTOR_SLOTS)
            .map(|_| add_stream_at_level(sel, base, start_ms, level))
            .collect()
    }

    fn slot_owners(sel: &TopNAudioSelector) -> Vec<Option<StreamId>> {
        sel.slots.iter().map(|s| s.owner).collect()
    }

    // ── 1. Basic admission ────────────────────────────────────────────────────

    /// A single packet from an unknown stream should claim an empty slot.
    #[test]
    fn packet_claims_empty_slot() {
        let mut sel = new_sel();
        let base = Instant::now();
        let id = make_stream();
        let result = sel.filter(id, &mut pkt_at(base, 0, 0));
        assert!(result.is_some(), "packet must claim an empty slot");
        assert!(slot_owners(&sel).contains(&Some(id)));
    }

    /// A very quiet packet still claims an empty slot when capacity exists.
    #[test]
    fn quiet_packet_claims_empty_slot() {
        let mut sel = new_sel();
        let base = Instant::now();
        let id = make_stream();
        let result = sel.filter(id, &mut pkt_at(base, 0, -127));
        assert!(result.is_some(), "quiet packet must claim an empty slot");
        assert!(slot_owners(&sel).contains(&Some(id)));
    }

    /// After owning a slot, a quiet packet from the owner must still be forwarded.
    #[test]
    fn owner_quiet_packet_forwarded() {
        let mut sel = new_sel();
        let base = Instant::now();
        let id = make_stream();
        sel.filter(id, &mut pkt_at(base, 0, 0));
        let result = sel.filter(id, &mut pkt_at(base, 100, -127));
        assert!(
            result.is_some(),
            "quiet packet from owner must be forwarded"
        );
    }

    // ── 2. Stream occupies exactly one slot ───────────────────────────────────

    #[test]
    fn stream_id_occupies_single_slot() {
        let base = Instant::now();
        let mut sel = new_sel();
        let id = make_stream();
        for t in 0..8u64 {
            sel.filter(id, &mut pkt_at(base, t * TICK_MS, 0));
        }
        let count = sel.slots.iter().filter(|s| s.owner == Some(id)).count();
        assert_eq!(count, 1, "same StreamId must occupy exactly one slot");
    }

    // ── 3. Slot stealing — Priority A (dead slot) ─────────────────────────────

    /// A newcomer steals a dead slot (silent for >DEAD_TIMEOUT).
    #[test]
    fn newcomer_steals_dead_slot() {
        let base = Instant::now();
        let mut sel = new_sel();
        fill_slots_at_level(&mut sel, base, 0, -20);
        // All incumbents last active at base + 4*TICK_MS = base+80ms.
        // Advance time past DEAD_TIMEOUT (2000ms).
        let newcomer = make_stream();
        // Arrive at base + 3000ms: all incumbents have been silent for >2000ms.
        let result = sel.filter(newcomer, &mut pkt_at(base, 3000, 0));
        assert!(result.is_some(), "newcomer must steal a dead slot");
        assert!(slot_owners(&sel).contains(&Some(newcomer)));
    }

    // ── 4. Slot stealing — Relative active contention ─────────────────────────

    /// A louder newcomer must evict the quietest active non-immune slot.
    #[test]
    fn louder_newcomer_steals_active_slot() {
        let mut sel = new_sel();
        let base = Instant::now();
        let incumbents = fill_slots_at_level(&mut sel, base, 0, -20);
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, -20));
        }

        let newcomer = make_stream();
        let result = sel.filter(newcomer, &mut pkt_at(base, 300, -10));
        assert!(
            result.is_some(),
            "louder newcomer must steal an active slot"
        );
        assert!(slot_owners(&sel).contains(&Some(newcomer)));
    }

    /// A newcomer with equal power must not steal under contention.
    #[test]
    fn equal_power_challenger_is_blocked() {
        let base = Instant::now();
        let mut sel = new_sel();
        let incumbents = fill_slots_at_level(&mut sel, base, 0, -20);
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, -20));
        }

        let challenger = make_stream();
        let result = sel.filter(challenger, &mut pkt_at(base, 300, -20));
        assert!(result.is_none(), "equal-power challenger must be blocked");
    }

    #[test]
    fn transient_quiet_packet_does_not_cause_immediate_steal() {
        let base = Instant::now();
        let mut sel = new_sel();

        // Force contention by filling all slots with strong incumbents.
        let incumbents = fill_slots_at_level(&mut sel, base, 0, -5);
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 400, -5));
        }

        let incumbent = incumbents[0];
        let challenger = make_stream();

        // Incumbent has one quiet packet after being strong.
        sel.filter(incumbent, &mut pkt_at(base, 500, -127));

        // Shortly after, a weaker challenger should not steal due to peak-hold decay.
        let result = sel.filter(challenger, &mut pkt_at(base, 520, -20));
        assert!(
            result.is_none(),
            "weaker challenger must not steal after one quiet packet"
        );
        assert!(
            slot_owners(&sel).contains(&Some(incumbent)),
            "incumbent should retain ownership"
        );
    }

    // ── 5. Newborn immunity ───────────────────────────────────────────────────

    /// Delayed packets from the evicted stream must NOT re-steal the slot during
    /// the NEWBORN_IMMUNITY window.
    #[test]
    fn newborn_immunity_blocks_evicted_owner() {
        let base = Instant::now();
        let mut sel = new_sel();
        let evicted_candidates = fill_slots_at_level(&mut sel, base, 0, -20);
        let newcomer = make_stream();
        let stolen_idx = sel
            .filter(newcomer, &mut pkt_at(base, 500, -10))
            .expect("newcomer must steal a slot");

        // 50ms later (well within 300ms immunity), the evicted stream sends a stronger packet.
        // It must not steal back the slot while immunity is active.
        let evicted_id = evicted_candidates[stolen_idx.index()];
        let result = sel.filter(evicted_id, &mut pkt_at(base, 550, 0));
        assert_eq!(
            sel.slots[stolen_idx.index()].owner,
            Some(newcomer),
            "newcomer must retain its immune slot"
        );
        let _ = result;
    }

    // ── 6. remove_track ───────────────────────────────────────────────────────

    #[test]
    fn remove_track_frees_slot() {
        let mut sel = new_sel();
        let base = Instant::now();
        let id = make_stream();
        sel.filter(id, &mut pkt_at(base, 0, 0));
        assert!(slot_owners(&sel).contains(&Some(id)));
        sel.remove_track(id);
        assert!(!slot_owners(&sel).contains(&Some(id)));
    }

    // ── 7. cleanup ────────────────────────────────────────────────────────────

    #[test]
    fn cleanup_evicts_dead_slots() {
        let base = Instant::now();
        let mut sel = new_sel();
        // Inject a stream that received its last packet far in the past.
        // We manipulate last_arrival_ts directly to simulate time passage.
        let id = make_stream();
        sel.filter(id, &mut pkt_at(base, 0, 0));
        // Manually set last_arrival_ts to a time >DEAD_TIMEOUT ago.
        let long_ago = Instant::now()
            .checked_sub(DEAD_TIMEOUT + Duration::from_millis(1))
            .unwrap_or(Instant::now());
        for slot in &mut sel.slots {
            if slot.owner == Some(id) {
                slot.last_arrival_ts = Some(long_ago);
            }
        }
        sel.cleanup();
        assert!(
            !slot_owners(&sel).contains(&Some(id)),
            "dead slot must be evicted by cleanup"
        );
    }
}
