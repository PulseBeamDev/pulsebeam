mod audio;
mod video;

use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::VideoAllocator;
use crate::participant::event::EventQueue;
use crate::rtp::RtpPacket;
use crate::track::{StreamId, StreamWriter, Track, TrackLayer};
use pulsebeam_runtime::rand::RngCore;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;
pub use video::Intent;

const MIN_BANDWIDTH: Bitrate = Bitrate::kbps(300);
const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);

#[derive(Clone)]
pub struct SlotConfig {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub pt: Pt,
    pub kind: MediaKind,
}

impl Default for SlotConfig {
    fn default() -> Self {
        Self {
            mid: Mid::from("0"),
            rid: None,
            ssrc: 0u32.into(),
            pt: 100u8.into(),
            kind: MediaKind::Video,
        }
    }
}

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    available_bandwidth: Bitrate,
    pub video: VideoAllocator,
    audio: AudioAllocator,
}

impl DownstreamAllocator {
    pub fn new(_participant_id: ParticipantId, manual_sub: bool, rng: &mut impl RngCore) -> Self {
        Self {
            available_bandwidth: MIN_BANDWIDTH,
            video: VideoAllocator::new(manual_sub, rng),
            audio: AudioAllocator::new(),
            dirty_allocation: false,
        }
    }

    pub fn add_track(&mut self, track: Track) {
        if track.meta.kind.is_video() {
            self.video.add_track(track);
            self.dirty_allocation = true;
        }
        // Audio tracks need no static registration; slots are claimed dynamically.
    }

    pub(super) fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let removed = self.video.remove_track(track_id);
        if removed {
            self.dirty_allocation = true;
        }
        removed
    }

    pub fn add_slot(&mut self, slot: SlotConfig) {
        match slot.kind {
            MediaKind::Video => {
                self.video.add_slot(slot.mid, slot);
            }
            MediaKind::Audio => {
                self.audio.add_slot(slot.mid, slot.pt, slot.ssrc);
            }
        }
        self.dirty_allocation = true;
    }

    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) {
        self.available_bandwidth = available_bandwidth.max(MIN_BANDWIDTH).min(MAX_BANDWIDTH);
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, bwe: &mut Bwe) {
        self.dirty_allocation = false;
        let desired = self.video.update_allocations(self.available_bandwidth);
        bwe.set_desired_bitrate(desired);
    }

    pub fn reconcile_routes(&mut self, now: Instant, events: &mut EventQueue) {
        self.video.reconcile_routes(now, events);
    }

    pub fn poll_slow(&mut self, now: Instant, bwe: &mut Bwe, events: &mut EventQueue) {
        self.update_allocations(bwe);
        self.video.poll_slow(now, self.available_bandwidth, events);
    }

    pub fn unsubscribe_all(&mut self) -> Vec<(StreamId, usize)> {
        self.video.unsubscribe_all()
    }

    #[inline]
    pub fn on_forward_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.video.on_rtp(stream_id, pkt, writer);
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        slot_idx: usize,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.audio.on_rtp(slot_idx, pkt, writer);
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) -> Option<&TrackLayer> {
        self.video.handle_keyframe_request(req)
    }
}
