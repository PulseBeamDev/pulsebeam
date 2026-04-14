//! Room-level Top-N audio selector.
//!
//! A synchronous, pure-push component owned by the shard worker:
//!
//! 1. Tracks are registered via [`TopNAudioSelector::add_track`].
//! 2. Packets are pushed via [`TopNAudioSelector::push_rtp`]; the selector
//!    updates the RFC 6464 audio-level EMA for that track and returns
//!    `Some(slot_idx)` so the caller knows which output slot to forward on.
//! 3. [`TopNAudioSelector::poll_slow`] re-ranks the top-N speakers and returns
//!    `true` if any slot assignment changed.  Call on a ~200 ms cadence.
//! 4. [`TopNAudioSelector::current_assignments`] returns the current
//!    `(slot_idx, ParticipantId)` mapping for signaling.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::{
    control::controller::MAX_SEND_AUDIO_SLOTS,
    entity::{ParticipantId, TrackId},
    rtp::RtpPacket,
    track::TrackMeta,
};

/// Number of output slots produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Minimum interval between Top-N re-rank passes.
const RERANK_INTERVAL: Duration = Duration::from_millis(200);

// ── Scoring metadata ─────────────────────────────────────────────────────────

struct InputTrack {
    meta: TrackMeta,
    /// Which output slot this track is currently assigned to.
    slot: Option<usize>,
    /// Exponential moving average of the RFC 6464 dBov level.
    /// Scale: 0 = loudest (digital full scale), -127 = silence.
    /// Higher (closer to 0) means louder → ranked first.
    audio_level_ema: f32,
}

// ── Output slot ───────────────────────────────────────────────────────────────

struct OutputSlot {
    /// Which input track is currently assigned to this slot.
    track_id: Option<TrackId>,
}

// ── Selector ─────────────────────────────────────────────────────────────────

/// Pure-push Top-N audio selector.
///
/// Owned by the shard worker.  All operations are synchronous; no async
/// machinery, no channels.
pub struct TopNAudioSelector {
    slots: Vec<OutputSlot>,
    tracks: HashMap<TrackId, InputTrack>,
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
            slots: (0..SELECTOR_SLOTS)
                .map(|_| OutputSlot { track_id: None })
                .collect(),
            tracks: HashMap::new(),
            last_rerank: None,
        }
    }

    /// Register a newly published audio track.
    pub fn add_track(&mut self, meta: TrackMeta) {
        let id = meta.id;
        if self.tracks.contains_key(&id) {
            return;
        }
        self.tracks.insert(
            id,
            InputTrack {
                meta,
                slot: None,
                audio_level_ema: -127.0, // start at silence
            },
        );
    }

    /// Unregister a track that has been unpublished.
    pub fn remove_track(&mut self, id: TrackId) {
        if let Some(track) = self.tracks.remove(&id)
            && let Some(slot_idx) = track.slot
        {
            self.slots[slot_idx].track_id = None;
        }
    }

    /// Hot-path entry point.  Updates the RFC 6464 audio-level EMA for the
    /// track and returns `Some(slot_idx)` if the track has an active slot
    /// assignment, or `None` if it is currently unselected.
    #[inline]
    pub fn push_rtp(&mut self, track_id: TrackId, pkt: &RtpPacket) -> Option<usize> {
        let track = self.tracks.get_mut(&track_id)?;
        // RFC 6464: ext_vals.audio_level is i8, 0 = loudest, -127 = silence.
        if let Some(level) = pkt.ext_vals.audio_level {
            track.audio_level_ema = 0.8 * track.audio_level_ema + 0.2 * (level as f32);
        }
        track.slot
    }

    /// Returns the current `(slot_idx, ParticipantId)` pairs for all occupied slots.
    pub fn current_assignments(&self) -> Vec<(usize, ParticipantId)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let track_id = slot.track_id?;
                let participant_id = self.tracks.get(&track_id)?.meta.origin;
                Some((i, participant_id))
            })
            .collect()
    }

    /// Slow-path: re-rank the top-N speakers if the interval has elapsed.
    /// Returns `true` if any slot assignment changed.
    /// Call from the shard worker's slow-poll path (~200 ms cadence).
    pub fn poll_slow(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last_rerank
            && now.duration_since(last) < RERANK_INTERVAL
        {
            return false;
        }
        self.last_rerank = Some(now);
        self.rerank()
    }

    // ── Re-ranking ────────────────────────────────────────────────────────────

    /// Re-rank tracks by loudness.  Returns `true` if any slot assignment changed.
    fn rerank(&mut self) -> bool {
        if self.tracks.is_empty() {
            return false;
        }

        // Sort descending by audio_level_ema — higher (closer to 0) = louder = ranked first.
        let mut scored: Vec<(TrackId, f32)> = self
            .tracks
            .iter()
            .map(|(&id, t)| (id, t.audio_level_ema))
            .collect();
        scored.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        let top_n: Vec<TrackId> = scored
            .iter()
            .take(SELECTOR_SLOTS)
            .map(|(id, _)| *id)
            .collect();

        let mut changed = false;

        // Pass 1 — retain existing valid assignments; evict tracks that fell out of top-N.
        let mut unassigned: Vec<TrackId> = top_n.clone();
        for slot in self.slots.iter_mut() {
            if let Some(id) = slot.track_id {
                if let Some(pos) = unassigned.iter().position(|x| *x == id) {
                    unassigned.remove(pos);
                } else {
                    // Fell out of top-N → vacate.
                    slot.track_id = None;
                    if let Some(t) = self.tracks.get_mut(&id) {
                        t.slot = None;
                    }
                    changed = true;
                }
            }
        }

        // Pass 2 — fill empty slots with newly top-N tracks.
        let mut iter = unassigned.into_iter();
        for (slot_idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.track_id.is_none()
                && let Some(id) = iter.next()
            {
                slot.track_id = Some(id);
                if let Some(t) = self.tracks.get_mut(&id) {
                    t.slot = Some(slot_idx);
                }
                changed = true;
            }
        }

        if changed {
            tracing::trace!(
                tracks = self.tracks.len(),
                assigned = self.slots.iter().filter(|s| s.track_id.is_some()).count(),
                "audio selector re-ranked"
            );
        }
        changed
    }
}
