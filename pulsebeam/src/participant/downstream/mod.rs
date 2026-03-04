mod audio;
mod video;

use crate::bitrate::BitrateController;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::VideoAllocator;
use crate::rtp::RtpPacket;
use crate::track::TrackReceiver;
use pulsebeam_runtime::sync::bit_signal::BitSignal;
use std::sync::Arc;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use tokio::time::Instant;
pub use video::Intent;

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    available_bandwidth: BitrateController,
    desired_bitrate: BitrateController,

    pub audio: AudioAllocator,
    pub video: VideoAllocator,
    yield_audio: bool,
}

impl DownstreamAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            available_bandwidth: BitrateController::new(
                crate::bitrate::BitrateControllerConfig::available_bandwidth(),
            ),
            desired_bitrate: BitrateController::new(
                crate::bitrate::BitrateControllerConfig::desired_bitrate(),
            ),
            audio: AudioAllocator::new(),
            video: VideoAllocator::new(manual_sub),
            yield_audio: false,
            dirty_allocation: false,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => self.audio.add_track(track),
            MediaKind::Video => self.video.add_track(track),
        }
        self.dirty_allocation = true;
    }

    pub fn remove_track(&mut self, track: &TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => self.audio.remove_track(&track.meta.id),
            MediaKind::Video => self.video.remove_track(&track.meta.id),
        }
        self.dirty_allocation = true;
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => self.audio.add_slot(mid),
            MediaKind::Video => self.video.add_slot(mid),
        }
        self.dirty_allocation = true;
    }

    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) {
        self.available_bandwidth.update(available_bandwidth);
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, bwe: &mut Bwe) {
        self.dirty_allocation = false;
        let Some((_current_alloc, desired_alloc)) = self
            .video
            .update_allocations(self.available_bandwidth.current())
        else {
            return;
        };

        let filtered_desired = self.desired_bitrate.update(desired_alloc);
        bwe.set_desired_bitrate(filtered_desired);
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        self.video.handle_keyframe_request(req);
    }

    pub fn poll_slow(&mut self, now: Instant, bwe: &mut Bwe) {
        self.video.poll_slow(now);
        self.update_allocations(bwe);
    }

    /// Register a shard's BitSignal on all audio and video inputs/slots so
    /// the shard task is woken as soon as any packet is ready to forward.
    pub fn attach_shard_signal(&mut self, signal: Arc<BitSignal>, bits: u64) {
        self.audio.attach_shard_signal(signal.clone(), bits);
        self.video.attach_shard_signal(signal, bits);
    }

    /// Non-blocking drain: returns the first ready RTP packet across audio and
    /// video without registering any wakers or allocating an EventListener.
    /// Alternates priority via `yield_audio` for fairness.
    pub fn try_next(&mut self) -> Option<(Mid, RtpPacket)> {
        if self.yield_audio {
            if let Some(pkt) = self.video.try_next() {
                self.yield_audio = false;
                return Some(pkt);
            }
            if let Some(pkt) = self.audio.try_next() {
                self.yield_audio = true;
                return Some(pkt);
            }
        } else {
            if let Some(pkt) = self.audio.try_next() {
                self.yield_audio = true;
                return Some(pkt);
            }
            if let Some(pkt) = self.video.try_next() {
                self.yield_audio = false;
                return Some(pkt);
            }
        }
        None
    }

}
