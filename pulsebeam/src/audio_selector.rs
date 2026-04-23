use std::cmp::Reverse;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::{control::controller::MAX_SEND_AUDIO_SLOTS, entity::TrackId, rtp::RtpPacket};

/// Number of output slots (loudest speakers) produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Streams silent for longer than this are evicted from the leaderboard (VAD/DTX handling).
const EVICTION_WINDOW: Duration = Duration::from_millis(100);

/// An incumbent whose `playout_time` falls within this window of `global_clock`
/// is protected from eviction (fairness for jittery / slow senders).
const VETERAN_WINDOW: Duration = Duration::from_millis(150);

/// Challenger must exceed the weakest incumbent's smoothed energy by at least
/// this many units before it can replace it (5 dB equivalent hysteresis).
const HYSTERESIS_BONUS: u8 = 5;

// EMA: E_smooth = E_old × 0.7 + E_new × 0.3   (integer form: multiply by 10)
const EMA_OLD: u32 = 7;
const EMA_NEW: u32 = 3;

/// Per-stream speaker state kept in the registry.
struct SpeakerMetadata {
    last_playout_time: Instant,
    /// Smoothed energy value; higher = louder (inverse of RFC 6464 attenuation).
    smoothed_energy: u8,
    /// `true` while this stream occupies a leaderboard slot.
    is_veteran: bool,
}

/// Per-shard, per-room Top-5 inline audio filter.
///
/// Call [`filter`] on every incoming audio packet metadata. The function
/// returns `Some(slot_idx)` when the packet should be forwarded and `None`
/// when it must be dropped.  All four algorithm phases execute synchronously
/// with **no heap allocation on the hot path** and **no payload buffering**.
pub struct TopNAudioSelector {
    /// Top-5 leaderboard; index 0 = loudest speaker, index 4 = weakest.
    /// Re-sorted descending by `smoothed_energy` after every challenger eviction.
    leaderboard: [Option<TrackId>; SELECTOR_SLOTS],
    /// Metadata for every audio stream observed in this room.
    registry: HashMap<TrackId, SpeakerMetadata>,
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
            leaderboard: [None; SELECTOR_SLOTS],
            registry: HashMap::new(),
            global_clock: None,
        }
    }

    /// Inline hot-path filter — runs all four algorithm phases synchronously.
    ///
    /// Returns `Some(slot_idx)` when the packet should be forwarded through
    /// leaderboard position `slot_idx`, `None` when it should be dropped.
    #[inline]
    pub fn filter(&mut self, stream_id: TrackId, pkt: &RtpPacket) -> Option<usize> {
        let playout_time = pkt.playout_time;
        // RFC 6464 (str0m): 0 = loudest, -127 = silence → invert so higher value = louder.
        let energy = rfc6464_to_energy(pkt.ext_vals.audio_level.unwrap_or(-127));

        // ── Phase 1: Clock sync ───────────────────────────────────────────────
        let global_clock = match self.global_clock {
            Some(prev) if prev >= playout_time => prev,
            _ => {
                self.global_clock = Some(playout_time);
                playout_time
            }
        };

        // ── Phase 2: State update (EMA + last_playout_time) ───────────────────
        let meta = self
            .registry
            .entry(stream_id)
            .or_insert_with(|| SpeakerMetadata {
                last_playout_time: playout_time,
                smoothed_energy: energy,
                is_veteran: false,
            });
        meta.smoothed_energy =
            ((meta.smoothed_energy as u32 * EMA_OLD + energy as u32 * EMA_NEW) / 10) as u8;
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
        // recent enough, forward immediately regardless of current energy rank.
        if let Some(pos) = self.leaderboard.iter().position(|s| *s == Some(stream_id)) {
            let fresh = global_clock
                .checked_sub(VETERAN_WINDOW)
                .map_or(true, |threshold| playout_time >= threshold);
            if fresh {
                return Some(pos);
            }
        }

        // Tier 2a — Challenger: fill an empty slot without any energy comparison.
        if let Some(pos) = self.leaderboard.iter().position(|s| s.is_none()) {
            self.leaderboard[pos] = Some(stream_id);
            if let Some(m) = self.registry.get_mut(&stream_id) {
                m.is_veteran = true;
            }
            return Some(pos);
        }

        // Tier 2b — Challenger: beat the weakest incumbent with hysteresis bonus.
        // Split the borrow so the closure can read the registry while we
        // hold a shared reference to the leaderboard.
        let (weakest_pos, weakest_energy) = {
            let reg = &self.registry;
            self.leaderboard
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let id = (*s)?;
                    let e = reg.get(&id)?.smoothed_energy;
                    Some((i, e))
                })
                .min_by_key(|&(_, e)| e)?
        };

        let challenger_energy = self.registry.get(&stream_id)?.smoothed_energy;
        if challenger_energy > weakest_energy.saturating_add(HYSTERESIS_BONUS) {
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

            // Re-sort descending by energy so position 0 = loudest speaker.
            // Explicit field split: leaderboard mutably, registry immutably.
            let (lb, reg) = (&mut self.leaderboard, &self.registry);
            lb.sort_unstable_by_key(|s| {
                Reverse(
                    s.and_then(|id| reg.get(&id))
                        .map(|m| m.smoothed_energy)
                        .unwrap_or(0),
                )
            });

            let new_pos = self
                .leaderboard
                .iter()
                .position(|s| *s == Some(stream_id))?;
            return Some(new_pos);
        }

        None
    }

    /// Remove a track from the leaderboard and the registry when its publisher leaves.
    pub fn remove_track(&mut self, id: TrackId) {
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
/// -127 = silence) to an energy value in `[0, 127]` where higher means louder,
/// suitable for EMA accumulation and hysteresis comparison.
#[inline(always)]
fn rfc6464_to_energy(level: i8) -> u8 {
    ((level as i16) + 127).clamp(0, 127) as u8
}
