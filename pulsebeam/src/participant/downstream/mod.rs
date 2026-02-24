mod audio;
mod video;

use crate::bitrate::BitrateController;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::VideoAllocator;
use crate::rtp::RtpPacket;
use crate::track::TrackReceiver;
use futures_lite::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use tokio::time::Instant;
pub use video::Intent;

/// `#[repr(align(64))]` guarantees `dirty_allocation` — checked on every
/// `poll_fast` iteration — sits at offset 0 of a fresh cache line.  Without
/// this, the compiler may place `dirty_allocation` anywhere in the struct,
/// potentially sharing a line with cold data from an adjacent struct.
#[repr(align(64))]
pub struct DownstreamAllocator {
    /// Set whenever a track is added/removed or BWE changes; cleared by
    /// `update_allocations`.  Placed first so the hot-path check
    /// (`if self.downstream.dirty_allocation`) never fetches a cold line.
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

    pub fn poll_fast(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        if self.yield_audio {
            // Try video first
            if let Poll::Ready(res) = self.video.poll_fast(cx) {
                self.yield_audio = false;
                return Poll::Ready(res);
            }
            // Fallback to audio if video is pending
            if let Poll::Ready(res) = self.audio.poll_next(cx) {
                self.yield_audio = true;
                return Poll::Ready(res);
            }
        } else {
            // Try audio first
            if let Poll::Ready(res) = self.audio.poll_next(cx) {
                self.yield_audio = true;
                return Poll::Ready(res);
            }
            // Fallback to video if audio is pending
            if let Poll::Ready(res) = self.video.poll_fast(cx) {
                self.yield_audio = false;
                return Poll::Ready(res);
            }
        }

        Poll::Pending
    }
}

impl Stream for DownstreamAllocator {
    type Item = (Mid, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_fast(cx)
    }
}
