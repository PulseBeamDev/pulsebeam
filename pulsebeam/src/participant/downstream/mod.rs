mod audio;
mod video;

use crate::audio_selector::AudioSelectorSubscription;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::{SlotConfig, VideoAllocator};
use crate::rtp::RtpPacket;
use crate::track::TrackReceiver;
use std::task::Poll;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use tokio::time::Instant;
pub use video::Intent;

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    available_bandwidth: Bitrate,

    pub audio: AudioAllocator,
    pub video: VideoAllocator,
    yield_audio: bool,
}

impl DownstreamAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            available_bandwidth: Bitrate::kbps(300),
            audio: AudioAllocator::new(),
            video: VideoAllocator::new(manual_sub),
            yield_audio: false,
            dirty_allocation: false,
        }
    }

    /// Replace the audio allocator's input receivers with those from the
    /// room-level audio selector subscription.
    ///
    /// Must be called once after the room hands the subscription to this
    /// participant (via `ParticipantControlMessage::AudioSubscription`).
    pub fn set_audio_subscription(&mut self, sub: AudioSelectorSubscription) {
        self.audio.set_subscription(sub);
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        match track.meta.kind {
            // Audio is now managed by the room-level TopNAudioSelector;
            // individual audio TrackReceivers are not added here.
            MediaKind::Audio => {}
            MediaKind::Video => self.video.add_track(track),
        }
        self.dirty_allocation = true;
    }

    pub fn remove_track(&mut self, track: &TrackReceiver) {
        match track.meta.kind {
            // Removal of audio tracks is handled by the room-level selector.
            MediaKind::Audio => {}
            MediaKind::Video => self.video.remove_track(&track.meta.id),
        }
        self.dirty_allocation = true;
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => self.audio.add_slot(mid),
            MediaKind::Video => self.video.add_slot(mid, SlotConfig::default()),
        }
        self.dirty_allocation = true;
    }

    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) {
        self.available_bandwidth = available_bandwidth;
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, bwe: &mut Bwe) {
        self.dirty_allocation = false;
        let desired = self.video.update_allocations(self.available_bandwidth);
        bwe.set_desired_bitrate(desired);
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        self.video.handle_keyframe_request(req);
    }

    pub fn poll_slow(&mut self, now: Instant, bwe: &mut Bwe) {
        self.video.poll_slow(now);
        self.update_allocations(bwe);
    }

    /// Await the next outbound RTP packet, fairly alternating between audio and video.
    pub async fn next(&mut self) -> (Mid, RtpPacket) {
        std::future::poll_fn(|cx| {
            // Alternate which stream gets priority each call so neither starves.
            // Both arms are polled every call — no `tokio::select!` future is
            // constructed, no extra waker registrations, no heap allocation.
            if self.yield_audio {
                if let Poll::Ready(pkt) = self.video.poll_next(cx) {
                    self.yield_audio = false;
                    return Poll::Ready(pkt);
                }
                if let Poll::Ready(pkt) = self.audio.poll_next(cx) {
                    self.yield_audio = true;
                    return Poll::Ready(pkt);
                }
            } else {
                if let Poll::Ready(pkt) = self.audio.poll_next(cx) {
                    self.yield_audio = true;
                    return Poll::Ready(pkt);
                }
                if let Poll::Ready(pkt) = self.video.poll_next(cx) {
                    self.yield_audio = false;
                    return Poll::Ready(pkt);
                }
            }
            Poll::Pending
        })
        .await
    }
}
