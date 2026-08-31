use super::UpstreamMedia;
use crate::{
    entity::TrackId,
    log::LogCtx,
    rtp::{EncodingId as Rid, MediaSectionId as Mid, PacketForwardingState, SenderReport, Ssrc},
    track::UpstreamTrack,
};
use tokio::time::Instant;

pub(crate) struct UpstreamVideo {
    media: UpstreamMedia,
}
impl UpstreamVideo {
    pub(super) fn new(ctx: LogCtx) -> Self {
        Self {
            media: UpstreamMedia::new(ctx, crate::entity::TrackKind::Video),
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
        rid: Option<&Rid>,
        rtp: PacketForwardingState,
        ssrc: Ssrc,
        sr: Option<SenderReport>,
    ) -> crate::track::ProcessedRtp {
        self.media
            .handle_incoming_rtp(index, mid, rid, rtp, ssrc, sr)
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
