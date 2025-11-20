mod audio;
mod video;

use crate::participant::bitrate::BitrateController;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::VideoAllocator;
use crate::rtp::RtpPacket;
use crate::track::TrackReceiver;
use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use tokio::time::Instant;

pub struct DownstreamAllocator {
    available_bandwidth: BitrateController,

    audio: AudioAllocator,
    video: VideoAllocator,
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
    }

    pub fn remove_track(&mut self, track: &TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => self.audio.remove_track(&track.meta.id),
            MediaKind::Video => self.video.remove_track(&track.meta.id),
        }
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => self.audio.add_slot(mid),
            MediaKind::Video => self.video.add_slot(mid),
        }
    }

    /// Handle BWE and compute both current and desired bitrate in one pass.
    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) -> (Bitrate, Bitrate) {
        self.available_bandwidth
            .update(available_bandwidth, Instant::now());
        self.update_allocations()
    }

    pub fn update_allocations(&mut self) -> (Bitrate, Bitrate) {
        self.video
            .update_allocations(self.available_bandwidth.current())
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) {
        self.video.handle_keyframe_request(req);
    }
}

impl Stream for DownstreamAllocator {
    type Item = (Mid, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Poll::Ready(item) = this.audio.poll_next(cx) {
            return Poll::Ready(item);
        }

        this.video.poll_next(cx)
    }
}
