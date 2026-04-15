use std::array;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use str0m::rtp::{SeqNo, Ssrc};
use tokio::time::Instant;

use crate::{
    control::controller::MAX_SEND_AUDIO_SLOTS,
    entity::TrackId,
    rtp::{self, RtpPacket, timeline::Timeline},
};

/// Number of output slots (loudest speakers) produced by the selector.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

// EMA: α = 0.2 → ~5-frame (100 ms) time constant.
const EMA_SILENCE: f32 = -127.0;
const EMA_ALPHA: f32 = 0.2;
const EMA_HOLD: f32 = 1.0 - EMA_ALPHA;

// Minimum time a slot must hold its current track before it can be evicted.
const SWITCH_HOLD: Duration = Duration::from_millis(300);

struct TrackState {
    /// EMA of the RFC 6464 level; 0 = loudest, −127 = silence.
    ema: f32,
    /// Index into `slots`, `None` when not in the top-N.
    slot: Option<usize>,
}

impl TrackState {
    fn new() -> Self {
        Self {
            ema: EMA_SILENCE,
            slot: None,
        }
    }
}

struct SlotState {
    /// `None` when the slot is unoccupied
    track_id: Option<TrackId>,
    timeline: Timeline,
    switch_seq: SeqNo,
    pending_marker: bool,
    last_ssrc: Option<Ssrc>,
    /// When this slot last switched to its current track.
    switched_at: Instant,
}

impl SlotState {
    fn new_empty(now: Instant) -> Self {
        Self {
            track_id: None,
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            switch_seq: SeqNo::from(0u64),
            pending_marker: false,
            last_ssrc: None,
            switched_at: now,
        }
    }

    fn assign(&mut self, track_id: TrackId, pkt: &RtpPacket, now: Instant) {
        self.track_id = Some(track_id);
        self.timeline.rebase_audio(pkt);
        self.switch_seq = pkt.seq_no;
        self.pending_marker = true;
        self.last_ssrc = Some(pkt.ssrc);
        self.switched_at = now;
    }

    fn release(&mut self) {
        self.track_id = None;
    }

    fn process(&mut self, mut pkt: RtpPacket, now: Instant) -> Option<RtpPacket> {
        // Mid-stream SSRC change (e.g. ICE restart on publisher).
        if self.last_ssrc != Some(pkt.ssrc) {
            self.assign(self.track_id?, &pkt, now);
        }

        // Drop packets that arrived before the switch point.
        if pkt.seq_no < self.switch_seq {
            return None;
        }

        if self.pending_marker {
            pkt.marker = true;
            self.pending_marker = false;
        }

        Some(self.timeline.rewrite(pkt))
    }
}

/// Per-shard, per-room Top-N audio selector.
///
/// Tracks auto-register on their first [`push_rtp`] call and must be
/// explicitly evicted via [`remove_track`] when their publisher leaves.
pub struct TopNAudioSelector {
    tracks: HashMap<TrackId, TrackState>,
    /// Fixed N=5 slots; index is the downstream slot identifier.
    slots: [SlotState; SELECTOR_SLOTS],
}

impl Default for TopNAudioSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl TopNAudioSelector {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            tracks: HashMap::new(),
            slots: array::from_fn(|_| SlotState::new_empty(now)),
        }
    }

    /// Hot-path entry point.
    ///
    /// Updates the RFC 6464 EMA when the audio level extension is present,
    /// promotes the track into the top-N if it outbids the quietest slot, and
    /// rewrites the packet through the slot's [`Timeline`].
    ///
    /// Returns `Some((slot_idx, rewritten_pkt))` when the track is selected,
    /// `None` otherwise.
    #[inline]
    pub fn push_rtp(
        &mut self,
        track_id: TrackId,
        pkt: &RtpPacket,
        now: Instant,
    ) -> Option<(usize, RtpPacket)> {
        // Phase 1: update EMA.
        let state = self.tracks.entry(track_id).or_insert_with(TrackState::new);
        if let Some(level) = pkt.ext_vals.audio_level {
            state.ema = EMA_HOLD * state.ema + EMA_ALPHA * (level as f32);
        }
        let new_ema = state.ema;
        let current_slot = state.slot;

        // Phase 2: slot arbitration.
        let slot_idx = match current_slot {
            Some(idx) => idx,
            None => self.try_claim_slot(track_id, new_ema, pkt, now)?,
        };

        // Phase 3: rewrite through the slot's timeline.
        let out = self.slots[slot_idx].process(pkt.clone(), now)?;
        Some((slot_idx, out))
    }

    /// Evict a track that has left the room.
    pub fn remove_track(&mut self, id: TrackId) {
        if let Some(state) = self.tracks.remove(&id)
            && let Some(idx) = state.slot
        {
            self.slots[idx].release();
        }
    }

    /// Tries to assign an unselected track to a slot.
    ///
    /// Uses a free slot if available; otherwise evicts the quietest occupied
    /// slot when the challenger's EMA is strictly greater.
    fn try_claim_slot(
        &mut self,
        track_id: TrackId,
        ema: f32,
        pkt: &RtpPacket,
        now: Instant,
    ) -> Option<usize> {
        if let Some(idx) = self.slots.iter().position(|s| s.track_id.is_none()) {
            self.slots[idx].assign(track_id, pkt, now);
            self.tracks.get_mut(&track_id)?.slot = Some(idx);
            return Some(idx);
        }

        // Find the evictable slot with the lowest current EMA.
        // Slots within SWITCH_HOLD of their last switch are not eligible.
        let mut min_idx = 0;
        let mut min_ema = f32::MAX;
        let mut evicted_id = None;
        for (i, slot) in self.slots.iter().enumerate() {
            let Some(tid) = slot.track_id else { continue };
            if now.duration_since(slot.switched_at) < SWITCH_HOLD {
                continue;
            }
            let e = self.tracks.get(&tid).map(|t| t.ema).unwrap_or(EMA_SILENCE);
            if e < min_ema {
                min_ema = e;
                min_idx = i;
                evicted_id = Some(tid);
            }
        }

        if ema <= min_ema {
            return None;
        }

        // Reuse the existing slot — preserves the Timeline so the
        // output seq/timestamp sequence remains continuous for the subscriber.
        self.tracks.get_mut(&evicted_id?)?.slot = None;
        self.slots[min_idx].assign(track_id, pkt, now);
        self.tracks.get_mut(&track_id)?.slot = Some(min_idx);
        Some(min_idx)
    }
}
