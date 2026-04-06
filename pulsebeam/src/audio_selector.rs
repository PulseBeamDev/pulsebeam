//! Room-level Top-N audio selector.
//!
//! A synchronous, pure-push component owned by the shard worker:
//!
//! 1. Tracks are registered via [`TopNAudioSelector::add_track`].
//! 2. Packets are pushed via [`TopNAudioSelector::push_rtp`]; the selector
//!    routes each packet to the slot assigned to that track, then returns
//!    `Some((slot_idx, packet))` so the caller can forward to all subscribers.
//! 3. [`TopNAudioSelector::poll_slow`] re-ranks the top-N speakers.  Call it
//!    on a ~200 ms cadence from the shard worker's slow path.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::{
    control::controller::MAX_SEND_AUDIO_SLOTS,
    entity::TrackId,
    rtp::{AudioRtpPacket, RtpPacket, monitor::StreamState},
    track::{Track, TrackMeta},
};

/// Number of output slots produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Minimum interval between Top-N re-rank passes.
const RERANK_INTERVAL: Duration = Duration::from_millis(200);

// ── Scoring metadata ─────────────────────────────────────────────────────────

struct InputTrack {
    state: StreamState,
    meta: TrackMeta,
    /// Which output slot this track is currently assigned to.
    slot: Option<usize>,
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
    pub fn add_track(&mut self, track: Track) {
        let id = track.meta.id;
        if self.tracks.contains_key(&id) {
            return;
        }

        // TODO: allow simulcast audio
        let Some(layer) = track.layers.first() else {
            return;
        };

        self.tracks.insert(
            id,
            InputTrack {
                state: layer.state.clone(),
                meta: track.meta,
                slot: None,
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

    /// Hot-path entry point.  Returns `Some((slot_idx, packet))` when the
    /// packet should be forwarded to all subscribers on that slot, or `None`
    /// if the track has no active slot assignment.
    #[inline]
    pub fn push_rtp(&self, track_id: TrackId, pkt: RtpPacket) -> Option<(usize, AudioRtpPacket)> {
        let track = self.tracks.get(&track_id)?;
        let slot_idx = track.slot?;
        Some((
            slot_idx,
            AudioRtpPacket {
                participant_id: track.meta.origin_participant,
                track_id,
                packet: pkt,
            },
        ))
    }

    /// Slow-path: re-rank the top-N speakers if the interval has elapsed.
    /// Call from the shard worker's 100 ms slow-poll bucket.
    pub fn poll_slow(&mut self, now: Instant) {
        if let Some(last) = self.last_rerank
            && now.duration_since(last) < RERANK_INTERVAL
        {
            return;
        }
        self.last_rerank = Some(now);
        self.rerank();
    }

    // ── Re-ranking ────────────────────────────────────────────────────────────

    fn rerank(&mut self) {
        if self.tracks.is_empty() {
            return;
        }

        let mut scored: Vec<(TrackId, f32)> = self
            .tracks
            .iter()
            .map(|(&id, t)| {
                let envelope = t.state.audio_envelope();
                let silence_penalty = {
                    let secs = t.state.silence_duration().as_secs_f32();
                    -(secs / 2.0).min(1.0) * 0.2
                };
                (id, envelope + silence_penalty)
            })
            .collect();

        scored.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        let top_n: Vec<TrackId> = scored
            .iter()
            .take(SELECTOR_SLOTS)
            .map(|(id, _)| *id)
            .collect();

        // Pass 1 — retain existing valid assignments.
        let mut unassigned: Vec<TrackId> = top_n.clone();
        for (slot_idx, slot) in self.slots.iter_mut().enumerate() {
            if let Some(id) = slot.track_id {
                if let Some(pos) = unassigned.iter().position(|x| *x == id) {
                    unassigned.remove(pos);
                } else {
                    // Fell out of top-N → vacate.
                    slot.track_id = None;
                    if let Some(t) = self.tracks.get_mut(&id) {
                        t.slot = None;
                    }
                }
            }
            let _ = slot_idx;
        }

        // Pass 2 — fill empty slots.
        let mut iter = unassigned.into_iter();
        for (slot_idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.track_id.is_none()
                && let Some(id) = iter.next()
            {
                slot.track_id = Some(id);
                if let Some(t) = self.tracks.get_mut(&id) {
                    t.slot = Some(slot_idx);
                }
            }
        }

        tracing::trace!(
            tracks = self.tracks.len(),
            assigned = self.slots.iter().filter(|s| s.track_id.is_some()).count(),
            "audio selector re-ranked"
        );
    }
}
