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
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use tokio::time::Instant;

pub struct DownstreamAllocator {
    available_bandwidth: BitrateController,

    pub audio: AudioAllocator,
    pub video: VideoAllocator,
}

impl DownstreamAllocator {
    pub fn new() -> Self {
        Self {
            available_bandwidth: BitrateController::default(),
            audio: AudioAllocator::new(),
            video: VideoAllocator::default(),
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => self.audio.add_track(track),
            MediaKind::Video => self.video.add_track(track),
        }
        self.update_allocations();
    }

    pub fn remove_track(&mut self, track: &TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => self.audio.remove_track(&track.meta.id),
            MediaKind::Video => self.video.remove_track(&track.meta.id),
        }
        self.update_allocations();
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => self.audio.add_slot(mid),
            MediaKind::Video => self.video.add_slot(mid),
        }
        self.update_allocations();
    }

    /// Handle BWE and compute both current and desired bitrate in one pass.
    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) -> Option<(Bitrate, Bitrate)> {
        self.available_bandwidth.update(available_bandwidth);
        self.update_allocations()
    }

    pub fn update_allocations(&mut self) -> Option<(Bitrate, Bitrate)> {
        self.video
            .update_allocations(self.available_bandwidth.current())
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        self.video.handle_keyframe_request(req);
    }

    pub fn poll_slow(&mut self, now: Instant) {
        self.video.poll_slow(now);
    }

    pub fn poll_fast(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        if let Poll::Ready(item) = self.audio.poll_next(cx) {
            return Poll::Ready(item);
        }

        self.video.poll_fast(cx)
    }
}

impl Stream for DownstreamAllocator {
    type Item = (Mid, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_fast(cx)
    }
}
