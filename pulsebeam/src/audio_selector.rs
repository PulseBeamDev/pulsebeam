use std::time::Duration;

use ahash::HashMap;
use tokio::time::Instant;

use crate::{
    control::controller::MAX_SEND_AUDIO_SLOTS,
    rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline},
    track::StreamId,
};

pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

// Increased to 1000ms. A 150ms physical window is too tight for bad internet connections.
const EVICTION_WINDOW: Duration = Duration::from_millis(1000);
const GC_WINDOW: Duration = Duration::from_secs(10);
const HYSTERESIS_BONUS_DB: f32 = 5.0;

const TAU_FAST_MS: f32 = 50.0;
const TAU_FLOOR_MS: f32 = 2000.0;
const VAD_THRESHOLD_RATIO: f32 = 3.981;

struct SpeakerMetadata {
    last_playout_time: Instant,
    last_arrival_ts: Instant, // Used exclusively to prevent jitter-based evictions
    e_fast: f32,
    e_floor: f32,
}

impl SpeakerMetadata {
    #[inline]
    fn score_db(&self) -> f32 {
        if self.e_floor <= 0.0 {
            return 0.0;
        }
        let ratio = self.e_fast / self.e_floor;
        if ratio <= 1.0 {
            0.0
        } else {
            10.0 * ratio.log10()
        }
    }
}

struct SlotTimeline {
    timeline: Timeline,
    pending_marker: bool,
}

#[derive(Default)]
pub struct TopNAudioSelector {
    leaderboard: [Option<StreamId>; SELECTOR_SLOTS],
    timelines: [Option<SlotTimeline>; SELECTOR_SLOTS],
    registry: HashMap<StreamId, SpeakerMetadata>,
    global_arrival_ts: Option<Instant>,
}

impl TopNAudioSelector {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn filter(&mut self, stream_id: StreamId, pkt: &mut RtpPacket) -> Option<usize> {
        let power = rfc6464_to_power(pkt.ext_vals.audio_level.unwrap_or(-127));

        // 1. Advance the PHYSICAL server timeline (Immune to all RTP clock drift/jitter)
        let global_arrival_ts = match self.global_arrival_ts {
            Some(prev) => prev.max(pkt.arrival_ts),
            None => pkt.arrival_ts,
        };
        self.global_arrival_ts = Some(global_arrival_ts);

        // 2. State & VAD Update
        let meta = self
            .registry
            .entry(stream_id)
            .or_insert_with(|| SpeakerMetadata {
                last_playout_time: pkt.playout_time,
                last_arrival_ts: pkt.arrival_ts,
                e_fast: power,
                e_floor: power,
            });

        meta.last_arrival_ts = pkt.arrival_ts;

        if let Some(dt) = pkt
            .playout_time
            .checked_duration_since(meta.last_playout_time)
        {
            let dt_ms = dt.as_millis() as f32;
            if dt_ms > 0.0 {
                let alpha_fast = 1.0 - (-dt_ms / TAU_FAST_MS).exp();
                meta.e_fast += alpha_fast * (power - meta.e_fast);

                let is_speech = meta.e_fast > meta.e_floor * VAD_THRESHOLD_RATIO;
                let tau_floor = if is_speech {
                    TAU_FLOOR_MS * 10.0
                } else {
                    TAU_FLOOR_MS
                };

                let alpha_floor = 1.0 - (-dt_ms / tau_floor).exp();
                meta.e_floor += alpha_floor * (power - meta.e_floor);
            }
        }
        meta.last_playout_time = meta.last_playout_time.max(pkt.playout_time);

        // 3. Slot Assignment

        // Tier 1 — Veteran: Already has a slot, keep flowing.
        if let Some(pos) = self.leaderboard.iter().position(|s| *s == Some(stream_id)) {
            self.rewrite_slot(pos, false, pkt);
            return Some(pos);
        }

        // Tier 2 — Empty Slot: With 3 participants and 5 slots, everyone lands here.
        if let Some(pos) = self.leaderboard.iter().position(|s| s.is_none()) {
            self.leaderboard[pos] = Some(stream_id);
            self.rewrite_slot(pos, true, pkt);
            return Some(pos);
        }

        // Tier 3 — Contention: Only triggers if 6+ participants are actively speaking
        let challenger_score = self.registry.get(&stream_id)?.score_db();
        let (weakest_pos, weakest_score) = self
            .leaderboard
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, self.registry.get(&(*s)?)?.score_db())))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

        if challenger_score > weakest_score + HYSTERESIS_BONUS_DB {
            self.leaderboard[weakest_pos] = Some(stream_id);
            self.rewrite_slot(weakest_pos, true, pkt);
            return Some(weakest_pos);
        }

        None
    }

    #[inline]
    fn rewrite_slot(&mut self, pos: usize, switched: bool, pkt: &mut RtpPacket) {
        let was_uninit = self.timelines[pos].is_none();
        let slot = self.timelines[pos].get_or_insert_with(|| SlotTimeline {
            timeline: Timeline::new(AUDIO_FREQUENCY),
            pending_marker: false,
        });
        if switched || was_uninit {
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
        self.registry.remove(&id);
        for slot in &mut self.leaderboard {
            if *slot == Some(id) {
                *slot = None;
                break;
            }
        }
    }

    pub fn cleanup(&mut self) -> Option<()> {
        let global_arrival_ts = self.global_arrival_ts?;
        let threshold = global_arrival_ts.checked_sub(EVICTION_WINDOW)?;

        for slot in &mut self.leaderboard {
            let Some(id) = *slot else {
                continue;
            };
            if self
                .registry
                .get(&id)
                .is_some_and(|m| m.last_arrival_ts < threshold)
            {
                *slot = None;
            }
        }

        let gc_threshold = global_arrival_ts.checked_sub(GC_WINDOW)?;
        self.registry
            .retain(|_, m| m.last_arrival_ts >= gc_threshold);
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

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Packet-spacing used by helpers. 5 ms keeps the full warmup window
    /// (15 × 5 = 75 ms) well inside the 100 ms eviction window when
    /// challenge packets are sent soon after fill_leaderboard.
    const TICK_MS: u64 = 5;

    fn make_stream() -> StreamId {
        (
            ParticipantId::new().derive_track_id(MediaKind::Audio, "test"),
            None,
        )
    }

    fn pkt_with(level: i8, playout_time: Instant) -> RtpPacket {
        let mut ext_vals = ExtensionValues::default();
        ext_vals.audio_level = Some(level);
        RtpPacket {
            ext_vals,
            playout_time,
            ..Default::default()
        }
    }

    fn pkt_at(base: Instant, offset_ms: u64, level: i8) -> RtpPacket {
        pkt_with(level, base + Duration::from_millis(offset_ms))
    }

    /// Warm up a stream with `n` packets at `level`, spaced TICK_MS apart,
    /// starting at `start_ms` relative to `base`. Returns the StreamId.
    fn warm_up(
        sel: &mut TopNAudioSelector,
        base: Instant,
        start_ms: u64,
        level: i8,
        n: u64,
    ) -> StreamId {
        let id = make_stream();
        for t in 0..n {
            sel.filter(id, &mut pkt_at(base, start_ms + t * TICK_MS, level));
        }
        id
    }

    /// Fill `n` leaderboard slots with loud (level=0) streams.
    ///
    /// All streams use the SAME compact time window (0 ms … 70 ms) so
    /// global_clock settles at base+70 ms after the call. Challenge packets
    /// can safely arrive at base+80 ms … base+169 ms without triggering
    /// eviction (threshold = global_clock − 100 ms < base+0 ms for those times).
    ///
    /// For eviction tests advance the challenge time to ≥ base+172 ms
    /// (threshold ≥ base+72 ms > base+70 ms) so all incumbents become stale.
    fn fill_leaderboard(sel: &mut TopNAudioSelector, n: usize, base: Instant) -> Vec<StreamId> {
        (0..n)
            .map(|_| {
                // 15 packets × 5 ms = last packet at base+70 ms.
                let id = warm_up(sel, base, 0, 0, 15);
                assert!(
                    sel.leaderboard.contains(&Some(id)),
                    "stream must be on leaderboard after warm-up"
                );
                id
            })
            .collect()
    }

    /// SNR score formula (mirrors the production code) for test assertions.
    fn score_of(sel: &TopNAudioSelector, id: StreamId) -> f32 {
        let m = sel.registry.get(&id).expect("stream must be in registry");
        m.e_fast - m.e_floor
    }

    // ── 1. Functional Ranking & Capacity ─────────────────────────────────────

    /// A 6th stream with a near-zero score cannot enter a full leaderboard.
    ///
    /// With SNR normalization a stream warmed up at constant energy has
    /// e_fast = e_floor = energy, so score = 0. 0 > 0 + HYSTERESIS_BONUS(5) → false.
    #[test]
    fn saturated_room_drops_quiet_stream() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // After fill: global_clock = base+70 ms; all incumbents score ≈ 0.
        fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);

        // Quiet challenger enters at base+80 ms — well within the eviction window
        // so all 5 slots remain occupied.
        let quiet = warm_up(&mut sel, base, 80, -120, 10);

        // One more packet — still within eviction window; score ≈ 0; must be dropped.
        let result = sel.filter(quiet, &mut pkt_at(base, 135, -120));
        assert!(
            result.is_none(),
            "quiet challenger must be dropped from a full leaderboard"
        );

        let occupied = sel.leaderboard.iter().filter(|s| s.is_some()).count();
        assert_eq!(occupied, SELECTOR_SLOTS, "leaderboard must remain full");
    }

    /// A new speaker who was previously silent scores very high on first loud
    /// packet (e_fast ≈ 0.7 × 127 ≈ 89, e_floor ≈ 0, score ≈ 89) and must
    /// evict whichever incumbent has the lowest score.
    #[test]
    fn loud_interrupter_evicts_weakest() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // After fill: global_clock = base+70 ms; all 5 incumbents score ≈ 0.
        let tracks = fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);
        for &id in &tracks {
            assert!(sel.leaderboard.contains(&Some(id)));
        }

        // Interrupter: warm up at silence (score ≈ 0), then fire one loud packet.
        // Silence warmup at base+80..base+125 ms (within eviction window of incumbents).
        let interrupter = make_stream();
        for t in 0..10u64 {
            sel.filter(interrupter, &mut pkt_at(base, 80 + t * TICK_MS, -127));
        }
        // One loud packet: e_fast = 0.7 × 127 ≈ 89, e_floor ≈ 0 → score ≈ 89.
        // global_clock = base+130 ms, threshold = base+30 ms.
        // All incumbents last at base+70 ms > base+30 ms → still fresh, no eviction.
        // 89 > 0 + HYSTERESIS_BONUS(5) → displaces the weakest incumbent.
        let result = sel.filter(interrupter, &mut pkt_at(base, 130, 0));
        assert!(result.is_some(), "new speaker must be accepted");
        assert!(
            sel.leaderboard.contains(&Some(interrupter)),
            "new speaker must be on leaderboard"
        );
    }

    /// A stream that has established a high noise floor can only gain a small
    /// SNR score by going slightly louder. That small score must not clear the
    /// 5-unit hysteresis threshold needed to displace an incumbent.
    ///
    /// This verifies e_floor correctly suppresses background-noise sources.
    #[test]
    fn snr_floor_blocks_marginal_challenger() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        // After fill: global_clock = base+70 ms; all 5 incumbents score ≈ 0.
        fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);

        // Challenger: warmed up at energy 100 (level −27).
        // After warmup: e_fast ≈ e_floor ≈ 100, score ≈ 0.
        // Warmup at base+80..base+150 ms (still within eviction window of incumbents).
        let challenger = make_stream();
        for t in 0..15u64 {
            sel.filter(challenger, &mut pkt_at(base, 80 + t * TICK_MS, -27));
        }

        // One packet at energy 103 (level −24):
        //   e_fast  = 0.7 × 103 + 0.3 × 100 = 102.1
        //   e_floor = 0.01 × 103 + 0.99 × 100 = 100.03
        //   score   = 2.07  <  HYSTERESIS_BONUS (5.0)
        // global_clock = base+155 ms, threshold = base+55 ms.
        // Incumbents last at base+70 ms > base+55 ms → still fresh.
        let result = sel.filter(challenger, &mut pkt_at(base, 155, -24));
        assert!(
            result.is_none(),
            "challenger with SNR score 2.07 (< 5.0 hysteresis) must be blocked"
        );
    }

    // ── 2. Temporal Fairness ─────────────────────────────────────────────────

    /// A veteran stream whose playout_time is 50 ms behind the global clock
    /// must still be forwarded via the Tier-1 veteran fast-pass (window = 150 ms).
    #[test]
    fn late_arrival_grace_period() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();

        // Admit stream_a at base+0..base+70 ms.
        let stream_a = warm_up(&mut sel, base, 0, 0, 15);

        // Advance global clock to base+150 ms via stream_b.
        let stream_b = warm_up(&mut sel, base, 150, 0, 5);
        let _ = stream_b;
        // global_clock = base+170 ms.

        // stream_a sends a packet with playout_time = base+120 ms.
        // Distance behind clock: 170 − 120 = 50 ms < VETERAN_WINDOW (150 ms).
        let result = sel.filter(stream_a, &mut pkt_at(base, 120, 0));
        assert!(
            result.is_some(),
            "veteran 50 ms behind clock must be forwarded via Tier-1"
        );
    }

    /// Out-of-order packets (lower playout_time arriving after a higher one)
    /// must never cause the global clock to regress.
    #[test]
    fn out_of_order_does_not_regress_clock() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();

        sel.filter(id, &mut pkt_at(base, 100, 0));
        let clock_high = sel.global_clock.expect("clock must be set");

        sel.filter(id, &mut pkt_at(base, 50, 0));
        let clock_after_old = sel.global_clock.expect("clock must be set");

        assert_eq!(clock_high, clock_after_old, "clock must not regress");

        // Both directions must be forwarded for this veteran stream.
        assert!(sel.filter(id, &mut pkt_at(base, 100, 0)).is_some());
        assert!(sel.filter(id, &mut pkt_at(base, 50, 0)).is_some());
    }

    // ── 3. VAD & DTX (Eviction) ───────────────────────────────────────────────

    /// A stream that stops sending packets is lazily evicted once the global clock
    /// advances more than EVICTION_WINDOW (100 ms) beyond its last playout_time.
    /// The freed slot must immediately be claimed by a newcomer.
    #[test]
    fn stale_stream_eviction_opens_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let tracks = fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);
        // All 5 incumbents: last_playout = base+70 ms.
        let stale_id = tracks[0];

        // Refresh streams 1–4 (but NOT stale_id) at base+200 ms.
        // After this: global_clock = base+200 ms, threshold = base+200-150 = base+50 ms.
        // stale_id.last (70 ms) > base+50 ms → not yet evicted here.
        // tracks[1..].last (200 ms) → fresh.
        for &id in tracks.iter().skip(1) {
            sel.filter(id, &mut pkt_at(base, 200, 0));
        }

        // Newcomer at base+240 ms → global_clock = base+240 ms, threshold = base+90 ms.
        // stale_id.last (70 ms) < threshold (90 ms) → evicted.
        // tracks[1..].last (200 ms) > threshold (90 ms) → still fresh.
        let newcomer = make_stream();
        let result = sel.filter(newcomer, &mut pkt_at(base, 240, 0));
        assert!(result.is_some(), "newcomer must claim the evicted slot");
        assert!(
            !sel.leaderboard.contains(&Some(stale_id)),
            "stale stream must have been evicted"
        );
    }

    /// After a long silence the global clock jumps far enough that ALL leaderboard
    /// members are simultaneously evicted, leaving slots open for newcomers.
    #[test]
    fn total_silence_clears_leaderboard() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);
        // global_clock = base+70 ms; all incumbents last at base+70 ms.

        // Probe at base+300 ms → global_clock = base+300 ms, threshold = base+200 ms.
        // All incumbents (70 ms) < threshold (200 ms) → ALL evicted.
        let probe = make_stream();
        sel.filter(probe, &mut pkt_at(base, 300, 0));

        let occupied = sel.leaderboard.iter().filter(|s| s.is_some()).count();
        assert!(
            occupied <= 1,
            "all stale incumbents must be evicted; {occupied} occupied"
        );
    }

    // ── 4. Robustness & Identity ──────────────────────────────────────────────

    /// A stream that sends multiple packets must occupy exactly one leaderboard slot.
    #[test]
    fn stream_id_occupies_single_slot() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();
        let id = make_stream();

        for t in 0..8u64 {
            sel.filter(id, &mut pkt_at(base, t * TICK_MS, 0));
        }

        let count = sel.leaderboard.iter().filter(|s| **s == Some(id)).count();
        assert_eq!(count, 1, "same StreamId must occupy exactly one slot");
    }

    /// A challenger whose SNR score exceeds the weakest incumbent's score by
    /// less than HYSTERESIS_BONUS (5.0) must be rejected, preventing rapid
    /// slot-flapping between two streams at similar energy levels.
    #[test]
    fn hysteresis_prevents_flapping() {
        let base = Instant::now();
        let mut sel = TopNAudioSelector::new();

        // 5 incumbents, all stabilised → score ≈ 0.
        // Use the same compact window (base+0..base+70 ms).
        fill_leaderboard(&mut sel, SELECTOR_SLOTS, base);

        // Refresh all incumbents at base+200 ms so they remain fresh
        // throughout the challenger's warmup.
        // After refresh: global_clock = base+200 ms.
        let incumbents: Vec<StreamId> = sel.leaderboard.iter().filter_map(|s| *s).collect();
        for &id in &incumbents {
            sel.filter(id, &mut pkt_at(base, 200, 0));
        }

        // Challenger warmed up at energy 100 (level −27).
        // After 15 packets: e_fast ≈ e_floor ≈ 100, score ≈ 0.
        // Warmup at base+205..base+275 ms.
        // global_clock after warmup = base+275 ms; threshold = base+175 ms.
        // Incumbents last at base+200 ms > base+175 ms → fresh ✓.
        let challenger = make_stream();
        for t in 0..15u64 {
            sel.filter(challenger, &mut pkt_at(base, 205 + t * TICK_MS, -27));
        }

        // Challenger sends one packet at energy 103 (level −24):
        //   score = (0.7 × 103 + 0.3 × 100) − (0.01 × 103 + 0.99 × 100) ≈ 2.07
        // Weakest incumbent score = 0.
        // 2.07 > 0 + 5.0 → false → challenger must be rejected.
        let result = sel.filter(challenger, &mut pkt_at(base, 280, -24));
        assert!(
            result.is_none(),
            "challenger with score ≈ 2.07 must be blocked by hysteresis (bonus = 5.0)"
        );
        // All original incumbents must still hold their slots.
        for &id in &incumbents {
            assert!(
                sel.leaderboard.contains(&Some(id)),
                "incumbent must not have been displaced"
            );
        }
    }
}
