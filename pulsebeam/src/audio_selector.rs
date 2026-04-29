use std::time::Duration;

use tokio::time::Instant;

use crate::{
    control::MAX_SEND_AUDIO_SLOTS,
    rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline},
    track::StreamId,
};

pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// -40 dBov → linear power: 10^(-40/10) = 0.0001
const SPEECH_THRESHOLD: f32 = 1e-4;
/// Slot is considered dead if no packet has arrived within this window.
const DEAD_TIMEOUT: Duration = Duration::from_millis(2000);
/// Slot is considered resting (DTX) if no *loud* packet has arrived within this window.
const DTX_REST_TIMEOUT: Duration = Duration::from_millis(500);
/// After a slot is stolen, this window protects the new owner from being immediately
/// evicted by delayed packets from the previous owner.
const NEWBORN_IMMUNITY: Duration = Duration::from_millis(300);
/// +5 dB power ratio: 10^(5/10) ≈ 3.1623. The incoming packet must be this much
/// louder than the quietest active slot to steal it during active contention.
const HYSTERESIS_MULTIPLIER: f32 = 3.1623;

struct SlotTimeline {
    timeline: Timeline,
    pending_marker: bool,
}

struct SlotState {
    owner: Option<StreamId>,
    /// Wall-clock time of the most recent packet (loud or quiet). `None` means the
    /// slot has never been used.
    last_arrival_ts: Option<Instant>,
    /// Wall-clock time of the most recent packet that exceeded SPEECH_THRESHOLD.
    /// `None` means the slot has never received a loud packet.
    last_loud_ts: Option<Instant>,
    /// Wall-clock time until which this slot cannot be stolen (newborn immunity).
    immunity_expiry: Instant,
    /// Power of the most recent packet, used for Active Contention (Priority C).
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

    /// A slot is "resting" (DTX mode) if its owner has sent no loud packet recently.
    #[inline]
    fn is_resting(&self, now: Instant) -> bool {
        match self.last_loud_ts {
            None => true,
            Some(ts) => now.duration_since(ts) > DTX_REST_TIMEOUT,
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

impl Default for TopNAudioSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl TopNAudioSelector {
    pub fn new() -> Self {
        // immunity_expiry is set to Instant::now() at construction; any packet arriving
        // later will have now >= construction_time, so is_immune() returns false.
        let init_instant = Instant::now();
        Self {
            slots: std::array::from_fn(|_| SlotState {
                owner: None,
                last_arrival_ts: None,
                last_loud_ts: None,
                immunity_expiry: init_instant,
                last_power: 0.0,
                slot_timeline: SlotTimeline {
                    timeline: Timeline::new(AUDIO_FREQUENCY),
                    pending_marker: false,
                },
            }),
        }
    }

    #[inline]
    pub fn filter(&mut self, stream_id: StreamId, pkt: &mut RtpPacket) -> Option<usize> {
        // Step 1: Parse power and wall-clock arrival time.
        let power = rfc6464_to_power(pkt.ext_vals.audio_level.unwrap_or(-127));
        let is_loud = power > SPEECH_THRESHOLD;
        let now = pkt.arrival_ts;

        // Step 2: Gatekeeper — non-owners that are not loud are dropped immediately.
        let owner_slot = self.slots.iter().position(|s| s.owner == Some(stream_id));
        if owner_slot.is_none() && !is_loud {
            return None;
        }

        // Step 3: Owner fast-path — update timestamps and forward.
        if let Some(idx) = owner_slot {
            let slot = &mut self.slots[idx];
            slot.last_arrival_ts = Some(now);
            slot.last_power = power;
            if is_loud {
                slot.last_loud_ts = Some(now);
            }
            Self::rewrite_slot_timeline(&mut slot.slot_timeline, false, pkt);
            return Some(idx);
        }

        // Step 4: Slot stealing — we have a loud non-owner. Find a victim.
        // Filter out immune slots first, then apply priority order.

        // Priority A: Dead slot (no owner, or silent for >DEAD_TIMEOUT).
        let victim = self
            .slots
            .iter()
            .position(|s| !s.is_immune(now) && s.is_dead(now));

        // Priority B: Resting (DTX) slot — pick the one that has been resting longest.
        let victim = victim.or_else(|| {
            self.slots
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_immune(now) && s.is_resting(now))
                .min_by_key(|(_, s)| s.last_loud_ts)
                .map(|(i, _)| i)
        });

        // Priority C: Active contention — find the quietest non-immune slot and compare.
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
            if power > quietest_slot.last_power * HYSTERESIS_MULTIPLIER {
                Some(quietest_idx)
            } else {
                None
            }
        });

        // Step 5: Execute the steal.
        let victim_idx = victim?;
        let slot = &mut self.slots[victim_idx];
        slot.owner = Some(stream_id);
        slot.last_arrival_ts = Some(now);
        slot.last_loud_ts = Some(now);
        slot.immunity_expiry = now + NEWBORN_IMMUNITY;
        slot.last_power = power;
        Self::rewrite_slot_timeline(&mut slot.slot_timeline, true, pkt);
        Some(victim_idx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use str0m::rtp::ExtensionValues;
    use tokio::time::Instant;

    use crate::entity::ParticipantId;
    use crate::rtp::RtpPacket;
    use str0m::media::MediaKind;

    const TICK_MS: u64 = 20;

    fn make_stream() -> StreamId {
        (
            ParticipantId::new().derive_track_id(MediaKind::Audio, "test"),
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

    /// Helper: send `n` loud packets spaced TICK_MS apart, return the StreamId.
    fn add_loud_stream(sel: &mut TopNAudioSelector, base: Instant, start_ms: u64) -> StreamId {
        let id = make_stream();
        for t in 0..5u64 {
            sel.filter(id, &mut pkt_at(base, start_ms + t * TICK_MS, 0));
        }
        id
    }

    /// Fill all SELECTOR_SLOTS with active loud streams.
    /// All streams send their last packet at `base + start_ms + 4*TICK_MS`.
    fn fill_slots(sel: &mut TopNAudioSelector, base: Instant, start_ms: u64) -> Vec<StreamId> {
        (0..SELECTOR_SLOTS)
            .map(|_| add_loud_stream(sel, base, start_ms))
            .collect()
    }

    fn slot_owners(sel: &TopNAudioSelector) -> Vec<Option<StreamId>> {
        sel.slots.iter().map(|s| s.owner).collect()
    }

    // ── 1. Basic admission ────────────────────────────────────────────────────

    /// A single loud packet from an unknown stream should claim an empty slot.
    #[test]
    fn loud_packet_claims_empty_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();
        let result = sel.filter(id, &mut pkt_at(base, 0, 0));
        assert!(result.is_some(), "loud packet must claim an empty slot");
        assert!(slot_owners(&sel).contains(&Some(id)));
    }

    /// A quiet (sub-threshold) packet from a non-owner must be dropped at the gatekeeper.
    #[test]
    fn quiet_non_owner_dropped() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();
        // -127 dBov → power = 10^(-127/10) ≈ 2e-13, well below SPEECH_THRESHOLD (1e-4)
        let result = sel.filter(id, &mut pkt_at(base, 0, -127));
        assert!(result.is_none(), "quiet non-owner must be dropped");
        assert!(!slot_owners(&sel).contains(&Some(id)));
    }

    /// After owning a slot, a quiet (DTX) packet from the owner must still be forwarded.
    #[test]
    fn owner_dtx_packet_forwarded() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();
        // Claim a slot with a loud packet.
        sel.filter(id, &mut pkt_at(base, 0, 0));
        // Then send a DTX (quiet) packet — must be forwarded since id is the owner.
        let result = sel.filter(id, &mut pkt_at(base, 100, -127));
        assert!(result.is_some(), "DTX packet from owner must be forwarded");
    }

    // ── 2. Stream occupies exactly one slot ───────────────────────────────────

    #[test]
    fn stream_id_occupies_single_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();
        for t in 0..8u64 {
            sel.filter(id, &mut pkt_at(base, t * TICK_MS, 0));
        }
        let count = sel.slots.iter().filter(|s| s.owner == Some(id)).count();
        assert_eq!(count, 1, "same StreamId must occupy exactly one slot");
    }

    // ── 3. Slot stealing — Priority A (dead slot) ─────────────────────────────

    /// A loud newcomer steals a dead slot (silent for >DEAD_TIMEOUT).
    #[test]
    fn loud_newcomer_steals_dead_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        fill_slots(&mut sel, base, 0);
        // All incumbents last active at base + 4*TICK_MS = base+80ms.
        // Advance time past DEAD_TIMEOUT (2000ms).
        let newcomer = make_stream();
        // Arrive at base + 3000ms: all incumbents have been silent for >2000ms.
        let result = sel.filter(newcomer, &mut pkt_at(base, 3000, 0));
        assert!(result.is_some(), "newcomer must steal a dead slot");
        assert!(slot_owners(&sel).contains(&Some(newcomer)));
    }

    // ── 4. Slot stealing — Priority B (DTX resting slot) ─────────────────────

    /// A loud newcomer steals the longest-resting DTX slot when all slots have owners
    /// but none have been loud recently.
    #[test]
    fn loud_newcomer_steals_resting_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // Fill all slots with loud streams (last loud packet at base+80ms).
        let incumbents = fill_slots(&mut sel, base, 0);

        // Keep all incumbents alive with quiet DTX pings at base+200ms so they're
        // not dead, but their last_loud_ts stays at base+80ms (>500ms DTX_REST_TIMEOUT
        // from the steal attempt at base+700ms).
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, -127));
        }

        // At base+700ms all slots are resting (last_loud = base+80ms, 620ms ago > 500ms).
        let newcomer = make_stream();
        let result = sel.filter(newcomer, &mut pkt_at(base, 700, 0));
        assert!(result.is_some(), "newcomer must steal a resting slot");
        assert!(slot_owners(&sel).contains(&Some(newcomer)));
    }

    // ── 5. Slot stealing — Priority C (active contention) ────────────────────

    /// A much louder newcomer (>+5dB) must evict the quietest active slot.
    #[test]
    fn much_louder_newcomer_steals_active_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // All 5 slots active with moderate level (-20 dBov).
        // Send continuous loud packets to keep last_loud_ts fresh (within 500ms).
        let incumbents = fill_slots(&mut sel, base, 0);
        // Refresh at t=200ms to ensure slots are not resting at t=300ms.
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, -20));
        }

        // Newcomer arrives at t=300ms with 0 dBov (full power = 1.0).
        // Incumbents last_power ≈ rfc6464_to_power(-20) = 10^(-2) = 0.01.
        // 1.0 > 0.01 * 3.1623 (≈ 0.032) → steal succeeds.
        let newcomer = make_stream();
        let result = sel.filter(newcomer, &mut pkt_at(base, 300, 0));
        assert!(
            result.is_some(),
            "much louder newcomer must steal an active slot"
        );
        assert!(slot_owners(&sel).contains(&Some(newcomer)));
    }

    /// A newcomer just slightly louder than all incumbents must be blocked by hysteresis.
    #[test]
    fn hysteresis_blocks_marginal_challenger() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // All 5 slots active with level -20 dBov; refresh to keep last_loud_ts fresh.
        let incumbents = fill_slots(&mut sel, base, 0);
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, -20));
        }

        // Challenger at -17 dBov: power = 10^(-17/10) ≈ 0.02.
        // Quietest incumbent power ≈ 0.01.
        // 0.02 > 0.01 * 3.1623 (≈ 0.032)? No (0.02 < 0.032) → blocked.
        let challenger = make_stream();
        let result = sel.filter(challenger, &mut pkt_at(base, 300, -17));
        assert!(
            result.is_none(),
            "marginally louder challenger must be blocked by hysteresis"
        );
    }

    // ── 6. Newborn immunity ───────────────────────────────────────────────────

    /// Delayed packets from the evicted stream must NOT re-steal the slot during
    /// the NEWBORN_IMMUNITY window.
    #[test]
    fn newborn_immunity_blocks_evicted_owner() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // Fill with 5 resting streams (last_loud = base+80ms).
        let evicted_candidates = fill_slots(&mut sel, base, 0);
        for &id in &evicted_candidates {
            sel.filter(id, &mut pkt_at(base, 200, -127));
        }

        // Newcomer steals at base+700ms (slots are resting, >500ms DTX_REST_TIMEOUT).
        let newcomer = make_stream();
        let stolen_idx = sel
            .filter(newcomer, &mut pkt_at(base, 700, 0))
            .expect("newcomer must steal a slot");

        // 50ms later (well within 300ms immunity), the evicted stream sends a loud packet.
        // It should NOT be able to steal back the slot.
        let evicted_id = evicted_candidates[stolen_idx];
        let result = sel.filter(evicted_id, &mut pkt_at(base, 750, 0));
        // evicted_id may own a different slot (not the stolen one), but it must not
        // have displaced the newcomer from the stolen slot.
        assert_eq!(
            sel.slots[stolen_idx].owner,
            Some(newcomer),
            "newcomer must retain its immune slot"
        );
        let _ = result;
    }

    // ── 7. remove_track ───────────────────────────────────────────────────────

    #[test]
    fn remove_track_frees_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();
        sel.filter(id, &mut pkt_at(base, 0, 0));
        assert!(slot_owners(&sel).contains(&Some(id)));
        sel.remove_track(id);
        assert!(!slot_owners(&sel).contains(&Some(id)));
    }

    // ── 8. cleanup ────────────────────────────────────────────────────────────

    #[test]
    fn cleanup_evicts_dead_slots() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
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
