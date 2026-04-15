//! Per-shard, per-room Top-N audio selector.
//!
//! Each shard instance independently picks the top `SELECTOR_SLOTS` loudest
//! speakers using a decaying EMA of RFC 6464 audio levels.  No cross-shard
//! coordination is required — every shard hears every audio stream (unconditional
//! fanout in `handle_audio_rtp`) and derives its own independent ranking.
//!
//! Pipeline per RTP packet:
//!
//! 1. [`TopNAudioSelector::push_rtp`] updates the EMA for that track and returns
//!    `true` if the track is in the current Top-N selection.  All packets —
//!    including DTX comfort-noise frames — are forwarded for selected tracks so
//!    that transitions remain smooth.
//! 2. [`TopNAudioSelector::poll_slow`] re-ranks all tracks and refreshes the
//!    selected set.  Call from the shard main loop every iteration; the method
//!    internally rate-limits to at most once per [`RERANK_INTERVAL`].
//! 3. [`TopNAudioSelector::remove_track`] evicts a track when its publisher
//!    leaves the room.

use std::cmp::Ordering;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::{control::controller::MAX_SEND_AUDIO_SLOTS, entity::TrackId, rtp::RtpPacket};

/// Number of output slots (loudest speakers) produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Minimum interval between Top-N re-rank passes.
const RERANK_INTERVAL: Duration = Duration::from_millis(200);

// ── Per-track state ───────────────────────────────────────────────────────────

struct TrackState {
    /// Exponential moving average of the RFC 6464 dBov level.
    /// 0 = loudest (0 dBov), −127 = silence; higher → louder → ranked first.
    audio_level_ema: f32,
    /// Whether this track is currently in the Top-N selection.
    /// Only changes during [`TopNAudioSelector::poll_slow`], so the hot path
    /// is a single bool read.
    selected: bool,
}

// ── Selector ─────────────────────────────────────────────────────────────────

/// Per-shard, per-room Top-N audio selector.
///
/// Owned by the shard worker inside `RoomState`.  All operations are
/// synchronous and allocation-free on the hot path once tracks are registered.
///
/// Tracks auto-register on their first [`push_rtp`] call and must be
/// explicitly evicted via [`remove_track`] when their publisher leaves.
pub struct TopNAudioSelector {
    tracks: HashMap<TrackId, TrackState>,
    last_rerank: Option<Instant>,
}

impl Default for TopNAudioSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl TopNAudioSelector {
    pub fn new() -> Self {
        Self {
            tracks: HashMap::new(),
            last_rerank: None,
        }
    }

    /// Hot-path entry point.
    ///
    /// Updates the RFC 6464 audio-level EMA for the track using the `audio_level`
    /// extension value directly.  If no extension is present the EMA is left
    /// unchanged so the current ranking is preserved.
    ///
    /// Returns `true` if the track is in the current Top-N selection, meaning
    /// all of its packets (speech *and* comfort noise) should be forwarded to
    /// local subscribers.  The selected set is refreshed only by [`poll_slow`]
    /// so this read is branch-free on the hot path.
    #[inline]
    pub fn push_rtp(&mut self, track_id: TrackId, pkt: &RtpPacket, _now: Instant) -> bool {
        let state = self.tracks.entry(track_id).or_insert(TrackState {
            audio_level_ema: -127.0,
            selected: false,
        });

        // Update EMA from the RFC 6464 audio-level extension whenever present.
        // No special-casing for DTX/comfort-noise — ranking is purely level-based.
        if let Some(level) = pkt.ext_vals.audio_level {
            state.audio_level_ema = 0.8 * state.audio_level_ema + 0.2 * (level as f32);
        }

        state.selected
    }

    /// Remove a track that has left the room.
    pub fn remove_track(&mut self, id: TrackId) {
        self.tracks.remove(&id);
    }

    /// Slow-path: re-rank all tracks and refresh the Top-N selected set.
    ///
    /// Call from the shard main loop on every iteration.  The method
    /// rate-limits actual work to at most once per [`RERANK_INTERVAL`].
    pub fn poll_slow(&mut self, now: Instant) {
        if let Some(last) = self.last_rerank {
            if now.duration_since(last) < RERANK_INTERVAL {
                return;
            }
        }
        self.last_rerank = Some(now);
        self.rerank();
    }

    fn rerank(&mut self) {
        if self.tracks.is_empty() {
            return;
        }

        // Sort track IDs by descending EMA (loudest first).
        let mut scored: Vec<(TrackId, f32)> = self
            .tracks
            .iter()
            .map(|(&id, t)| (id, t.audio_level_ema))
            .collect();
        scored.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        // Clear all selected flags, then mark the top-N.
        for state in self.tracks.values_mut() {
            state.selected = false;
        }
        for (id, _) in scored.iter().take(SELECTOR_SLOTS) {
            if let Some(state) = self.tracks.get_mut(id) {
                state.selected = true;
            }
        }
    }
}
