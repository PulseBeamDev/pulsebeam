use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};

use crate::entity::{ParticipantId, TrackId};
use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::{StreamId, Track};
use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

pub struct AudioAllocator {
    participant_id: ParticipantId,
    track_ids: HashSet<TrackId>,
    slots: Vec<AudioSlot>,
    mid_to_idx: HashMap<Mid, usize>,
    stream_to_idx: HashMap<StreamId, usize>,
}

impl AudioAllocator {
    pub fn new(participant_id: ParticipantId) -> Self {
        Self {
            participant_id,
            track_ids: HashSet::new(),
            slots: Vec::new(),
            mid_to_idx: HashMap::new(),
            stream_to_idx: HashMap::new(),
        }
    }

    pub fn add_track(&mut self, layer: Track) {
        let track_id = layer.meta.id;
        if self.track_ids.contains(&track_id) {
            return;
        }
        self.track_ids.insert(track_id);
        let stream_id = (track_id, None);
        let occupied: std::collections::HashSet<usize> =
            self.stream_to_idx.values().copied().collect();
        if let Some(slot_idx) = (0..self.slots.len()).find(|i| !occupied.contains(i)) {
            self.stream_to_idx.insert(stream_id, slot_idx);
        }
    }

    pub fn contains_track(&self, track_id: TrackId) -> bool {
        self.track_ids.contains(&track_id)
    }

    pub fn add_slot(&mut self, mid: Mid, pt: Pt, ssrc: Ssrc) -> usize {
        let idx = self.slots.len();
        self.slots.push(AudioSlot::new(mid, pt, ssrc));
        self.mid_to_idx.insert(mid, idx);
        // Assign any track that didn't get a slot because slots weren't ready yet.
        for &track_id in &self.track_ids {
            let stream_id = (track_id, None);
            if let std::collections::hash_map::Entry::Vacant(e) =
                self.stream_to_idx.entry(stream_id)
            {
                e.insert(idx);
                break;
            }
        }
        idx
    }

    pub fn assign_stream(&mut self, stream_id: StreamId, slot_idx: usize) {
        self.stream_to_idx.insert(stream_id, slot_idx);
    }

    pub fn unassign_stream(&mut self, stream_id: &StreamId) {
        self.stream_to_idx.remove(stream_id);
    }

    #[inline]
    pub fn on_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: RtpPacket,
    ) -> Option<(Mid, Pt, Ssrc, RtpPacket)> {
        let &idx = self.stream_to_idx.get(stream_id)?;
        let slot = self.slots.get_mut(idx)?;
        Some(slot.process(pkt))
    }
}

struct AudioSlot {
    mid: Mid,
    pt: Pt,
    ssrc: Ssrc,
    timeline: Timeline,
    last_ssrc: Option<Ssrc>,
}

impl AudioSlot {
    fn new(mid: Mid, pt: Pt, ssrc: Ssrc) -> Self {
        Self {
            mid,
            pt,
            ssrc,
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            last_ssrc: None,
        }
    }

    fn process(&mut self, mut pkt: RtpPacket) -> (Mid, Pt, Ssrc, RtpPacket) {
        if self.last_ssrc != Some(pkt.ssrc) {
            self.last_ssrc = Some(pkt.ssrc);
            self.timeline.rebase(&pkt);
        }
        pkt = self.timeline.rewrite(pkt);
        (self.mid, self.pt, self.ssrc, pkt)
    }
}
