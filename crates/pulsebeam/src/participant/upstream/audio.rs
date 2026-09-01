use super::UpstreamMedia;
use crate::{entity::TrackId, log::LogCtx, rtp::RtpPacket, track::UpstreamTrack};
use str0m::media::Mid;
use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

pub(crate) struct UpstreamAudio {
    media: UpstreamMedia,
}
impl UpstreamAudio {
    pub(super) fn new(ctx: LogCtx) -> Self {
        Self {
            media: UpstreamMedia::new(ctx, crate::entity::TrackKind::Audio),
        }
    }
    pub(super) fn add_published_track(
        &mut self,
        mid: Mid,
        track: UpstreamTrack,
        descriptor: crate::track::Track,
    ) -> bool {
        self.media.add_published_track(mid, track, descriptor)
    }
    pub(super) fn slot_for_mid(&self, mid: Mid) -> Option<(usize, TrackId)> {
        self.media.slot_for_mid(mid)
    }
    pub(super) fn handle_incoming_rtp(
        &mut self,
        index: usize,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        rtp: RtpPacket,
        sr: Option<SenderInfo>,
    ) -> crate::track::ProcessedRtp {
        self.media.handle_incoming_rtp(index, mid, rid, rtp, sr)
    }
    pub(super) fn announce_state_mut(
        &mut self,
        mid: Mid,
    ) -> Option<(&crate::track::Track, &mut bool)> {
        self.media.announce_state_mut(mid)
    }
    pub(super) fn mid_for_track_id(&self, id: TrackId) -> Option<Mid> {
        self.media.mid_for_track_id(id)
    }
    pub(super) fn poll_slow(&mut self, now: Instant) {
        self.media.poll_slow(now);
    }
}
