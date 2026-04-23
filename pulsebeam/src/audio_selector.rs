use std::array;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::{control::controller::MAX_SEND_AUDIO_SLOTS, rtp::RtpPacket, track::StreamId};

/// Number of output slots (loudest speakers) produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Streams silent for longer than this are evicted from the leaderboard (VAD/DTX handling).
const EVICTION_WINDOW: Duration = Duration::from_millis(100);

/// An incumbent whose `playout_time` falls within this window of `global_clock`
/// is protected from eviction (fairness for jittery / slow senders).
const VETERAN_WINDOW: Duration = Duration::from_millis(150);

/// Challenger score must exceed the weakest incumbent's score by at least this
/// many units before it can replace it (5 dB equivalent hysteresis in score space).
const HYSTERESIS_BONUS: f32 = 5.0;

// SNR EMA alphas:
//   e_fast tracks the current packet energy with a fast-reacting EMA (α = 0.7).
//   e_floor tracks the background noise floor with a very slow EMA (α = 0.01).
//   score = e_fast − e_floor  (positive when speaker is above their own noise floor)
const E_FAST_ALPHA: f32 = 0.7;
const E_FLOOR_ALPHA: f32 = 0.01;

/// Per-stream speaker state kept in the registry.
struct SpeakerMetadata {
    last_playout_time: Instant,
    /// Fast EMA of energy — reacts quickly to speech activity.
    e_fast: f32,
    /// Slow EMA of energy — tracks per-stream noise floor.
    e_floor: f32,
    /// `true` while this stream occupies a leaderboard slot.
    is_veteran: bool,
}

impl SpeakerMetadata {
    /// SNR-normalised score: how far above the stream's own noise floor it currently is.
    #[inline]
    fn score(&self) -> f32 {
        self.e_fast - self.e_floor
    }
}

/// Per-shard, per-room Top-5 inline audio filter.
///
/// Call [`filter`] on every incoming audio packet metadata. The function
/// returns `Some(slot_idx)` when the packet should be forwarded and `None`
/// when it must be dropped.  All four algorithm phases execute synchronously
/// with **no heap allocation on the hot path** and **no payload buffering**.
pub struct TopNAudioSelector {
    /// Top-5 leaderboard; index 0 = loudest speaker, index 4 = weakest.
    /// Re-sorted descending by score after every challenger eviction.
    leaderboard: [Option<StreamId>; SELECTOR_SLOTS],
    /// Metadata for every audio stream observed in this room.
    registry: HashMap<StreamId, SpeakerMetadata>,
    /// Maximum `playout_time` observed across all streams; drives eviction.
    global_clock: Option<Instant>,
}

impl Default for TopNAudioSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl TopNAudioSelector {
    pub fn new() -> Self {
        Self {
            leaderboard: array::from_fn(|_| None),
            registry: HashMap::new(),
            global_clock: None,
        }
    }

    /// Inline hot-path filter — runs all four algorithm phases synchronously.
    ///
    /// Returns `Some(slot_idx)` when the packet should be forwarded through
    /// leaderboard position `slot_idx`, `None` when it must be dropped.
    #[inline]
    pub fn filter(&mut self, stream_id: StreamId, pkt: &RtpPacket) -> Option<usize> {
        let playout_time = pkt.playout_time;
        // RFC 6464 (str0m): 0 = loudest, -127 = silence → invert so higher = louder.
        let energy = rfc6464_to_energy(pkt.ext_vals.audio_level.unwrap_or(-127));

        // ── Phase 1: Clock sync ───────────────────────────────────────────────
        let global_clock = match self.global_clock {
            Some(prev) if prev >= playout_time => prev,
            _ => {
                self.global_clock = Some(playout_time);
                playout_time
            }
        };

        // ── Phase 2: State update (SNR EMAs + last_playout_time) ──────────────
        let meta = self
            .registry
            .entry(stream_id)
            .or_insert_with(|| SpeakerMetadata {
                last_playout_time: playout_time,
                e_fast: energy,
                e_floor: energy,
                is_veteran: false,
            });
        meta.e_fast = E_FAST_ALPHA * energy + (1.0 - E_FAST_ALPHA) * meta.e_fast;
        meta.e_floor = E_FLOOR_ALPHA * energy + (1.0 - E_FLOOR_ALPHA) * meta.e_floor;
        meta.last_playout_time = playout_time;

        // ── Phase 3: Lazy eviction ────────────────────────────────────────────
        // Remove leaderboard members that have been silent longer than EVICTION_WINDOW.
        if let Some(threshold) = global_clock.checked_sub(EVICTION_WINDOW) {
            for slot in &mut self.leaderboard {
                let Some(id) = *slot else { continue };
                let stale = self
                    .registry
                    .get(&id)
                    .map_or(false, |m| m.last_playout_time < threshold);
                if stale {
                    if let Some(m) = self.registry.get_mut(&id) {
                        m.is_veteran = false;
                    }
                    *slot = None;
                }
            }
        }

        // ── Phase 4: Forwarding decision (two-tier gate) ──────────────────────

        // Tier 1 — Veteran protection.
        // If the stream is already on the leaderboard and its playout_time is
        // recent enough, forward immediately regardless of current score rank.
        if let Some(pos) = self.leaderboard.iter().position(|s| *s == Some(stream_id)) {
            let fresh = global_clock
                .checked_sub(VETERAN_WINDOW)
                .map_or(true, |threshold| playout_time >= threshold);
            if fresh {
                return Some(pos);
            }
        }

        // Tier 2a — Challenger: fill an empty slot without any score comparison.
        if let Some(pos) = self.leaderboard.iter().position(|s| s.is_none()) {
            self.leaderboard[pos] = Some(stream_id);
            if let Some(m) = self.registry.get_mut(&stream_id) {
                m.is_veteran = true;
            }
            return Some(pos);
        }

        // Tier 2b — Challenger: beat the weakest incumbent with hysteresis bonus.
        let (weakest_pos, weakest_score) = {
            let reg = &self.registry;
            self.leaderboard
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let id = (*s)?;
                    let score = reg.get(&id)?.score();
                    Some((i, score))
                })
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?
        };

        let challenger_score = self.registry.get(&stream_id)?.score();
        if challenger_score > weakest_score + HYSTERESIS_BONUS {
            // Demote the weakest incumbent.
            if let Some(evicted) = self.leaderboard[weakest_pos] {
                if let Some(m) = self.registry.get_mut(&evicted) {
                    m.is_veteran = false;
                }
            }
            self.leaderboard[weakest_pos] = Some(stream_id);
            if let Some(m) = self.registry.get_mut(&stream_id) {
                m.is_veteran = true;
            }

            // Re-sort descending by score so position 0 = loudest speaker.
            let (lb, reg) = (&mut self.leaderboard, &self.registry);
            lb.sort_unstable_by(|a, b| {
                let sa = a
                    .and_then(|id| reg.get(&id))
                    .map(|m| m.score())
                    .unwrap_or(f32::NEG_INFINITY);
                let sb = b
                    .and_then(|id| reg.get(&id))
                    .map(|m| m.score())
                    .unwrap_or(f32::NEG_INFINITY);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });

            let new_pos = self
                .leaderboard
                .iter()
                .position(|s| *s == Some(stream_id))?;
            return Some(new_pos);
        }

        None
    }

    /// Remove a track from the leaderboard and registry when its publisher leaves.
    /// Accepts a `StreamId` directly.
    pub fn remove_track(&mut self, id: StreamId) {
        self.registry.remove(&id);
        for slot in &mut self.leaderboard {
            if *slot == Some(id) {
                *slot = None;
                break;
            }
        }
    }
}

/// Convert an RFC 6464 audio level as represented by str0m (`i8`, 0 = loudest,
/// -127 = silence) to a linear energy value in `[0.0, 127.0]` where higher
/// means louder.
#[inline(always)]
fn rfc6464_to_energy(level: i8) -> f32 {
    ((level as i16) + 127).clamp(0, 127) as f32
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
            sel.filter(id, &pkt_at(base, start_ms + t * TICK_MS, level));
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
        let result = sel.filter(quiet, &pkt_at(base, 135, -120));
        assert!(result.is_none(), "quiet challenger must be dropped from a full leaderboard");

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
            sel.filter(interrupter, &pkt_at(base, 80 + t * TICK_MS, -127));
        }
        // One loud packet: e_fast = 0.7 × 127 ≈ 89, e_floor ≈ 0 → score ≈ 89.
        // global_clock = base+130 ms, threshold = base+30 ms.
        // All incumbents last at base+70 ms > base+30 ms → still fresh, no eviction.
        // 89 > 0 + HYSTERESIS_BONUS(5) → displaces the weakest incumbent.
        let result = sel.filter(interrupter, &pkt_at(base, 130, 0));
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
            sel.filter(challenger, &pkt_at(base, 80 + t * TICK_MS, -27));
        }

        // One packet at energy 103 (level −24):
        //   e_fast  = 0.7 × 103 + 0.3 × 100 = 102.1
        //   e_floor = 0.01 × 103 + 0.99 × 100 = 100.03
        //   score   = 2.07  <  HYSTERESIS_BONUS (5.0)
        // global_clock = base+155 ms, threshold = base+55 ms.
        // Incumbents last at base+70 ms > base+55 ms → still fresh.
        let result = sel.filter(challenger, &pkt_at(base, 155, -24));
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
        let result = sel.filter(stream_a, &pkt_at(base, 120, 0));
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

        sel.filter(id, &pkt_at(base, 100, 0));
        let clock_high = sel.global_clock.expect("clock must be set");

        sel.filter(id, &pkt_at(base, 50, 0));
        let clock_after_old = sel.global_clock.expect("clock must be set");

        assert_eq!(clock_high, clock_after_old, "clock must not regress");

        // Both directions must be forwarded for this veteran stream.
        assert!(sel.filter(id, &pkt_at(base, 100, 0)).is_some());
        assert!(sel.filter(id, &pkt_at(base, 50, 0)).is_some());
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
        // After this: global_clock = base+200 ms, threshold = base+100 ms.
        // stale_id.last (70 ms) < threshold (100 ms) → evicted on next filter call.
        // tracks[1..].last (200 ms) > threshold (100 ms) → fresh.
        for &id in tracks.iter().skip(1) {
            sel.filter(id, &pkt_at(base, 200, 0));
        }

        let newcomer = make_stream();
        let result = sel.filter(newcomer, &pkt_at(base, 210, 0));
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
        sel.filter(probe, &pkt_at(base, 300, 0));

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
            sel.filter(id, &pkt_at(base, t * TICK_MS, 0));
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
            sel.filter(id, &pkt_at(base, 200, 0));
        }

        // Challenger warmed up at energy 100 (level −27).
        // After 15 packets: e_fast ≈ e_floor ≈ 100, score ≈ 0.
        // Warmup at base+205..base+275 ms.
        // global_clock after warmup = base+275 ms; threshold = base+175 ms.
        // Incumbents last at base+200 ms > base+175 ms → fresh ✓.
        let challenger = make_stream();
        for t in 0..15u64 {
            sel.filter(challenger, &pkt_at(base, 205 + t * TICK_MS, -27));
        }

        // Challenger sends one packet at energy 103 (level −24):
        //   score = (0.7 × 103 + 0.3 × 100) − (0.01 × 103 + 0.99 × 100) ≈ 2.07
        // Weakest incumbent score = 0.
        // 2.07 > 0 + 5.0 → false → challenger must be rejected.
        let result = sel.filter(challenger, &pkt_at(base, 280, -24));
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
