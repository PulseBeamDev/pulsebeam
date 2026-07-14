use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

use crate::{
    entity::{TrackId, TrackKind},
    rtp::RtpPacket,
    track::UpstreamTrack,
};
use str0m::media::Mid;

const MAX_UPSTREAM_SLOT_PER_TYPE: usize = 2;

struct UpstreamSlot {
    mid: Mid,
    track: UpstreamTrack,
}

impl PartialEq for UpstreamSlot {
    fn eq(&self, other: &Self) -> bool {
        self.mid == other.mid
    }
}

impl Eq for UpstreamSlot {}

pub struct UpstreamAllocator {
    published_tracks: Vec<UpstreamSlot>,
}

impl UpstreamAllocator {
    pub fn new() -> Self {
        Self {
            published_tracks: Vec::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, mid: Mid, track: UpstreamTrack) -> bool {
        if self.published_tracks.iter().any(|s| s.mid == mid) {
            crate::log::warn!("duplicated slot mid={}.", mid);
            return false;
        }

        match track.meta.id.kind() {
            TrackKind::Video => {
                let video_count = self
                    .published_tracks
                    .iter()
                    .filter(|s| s.track.meta.id.kind() == TrackKind::Video)
                    .count();

                if video_count >= MAX_UPSTREAM_SLOT_PER_TYPE {
                    return false;
                }
            }
            TrackKind::Audio => {
                let audio_count = self
                    .published_tracks
                    .iter()
                    .filter(|s| s.track.meta.id.kind() == TrackKind::Audio)
                    .count();

                if audio_count >= MAX_UPSTREAM_SLOT_PER_TYPE {
                    return false;
                }
            }
            TrackKind::Data => todo!("add upstream data track"),
        }

        let slot = UpstreamSlot { mid, track };
        self.published_tracks.push(slot);
        true
    }

    pub fn handle_incoming_rtp(
        &mut self,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        rtp: &mut RtpPacket,
        sr: Option<SenderInfo>,
    ) -> bool {
        if let Some(slot) = self.published_tracks.iter_mut().find(|t| t.mid == mid) {
            rtp.ext_vals.rid = rid.cloned();
            slot.track.process(rid, rtp, sr)
        } else {
            crate::log::warn!(%mid, ?rid, "Dropping incoming RTP packet; no published track found");
            false
        }
    }

    pub fn track_id_for_mid(&self, mid: Mid) -> Option<TrackId> {
        self.published_tracks
            .iter()
            .find(|t| t.mid == mid)
            .map(|t| t.track.meta.id)
    }

    pub fn mid_for_track_id(&self, track_id: TrackId) -> Option<Mid> {
        self.published_tracks
            .iter()
            .find(|t| t.track.meta.id == track_id)
            .map(|t| t.mid)
    }

    pub fn audio_track_ids(&self) -> impl Iterator<Item = TrackId> + '_ {
        self.published_tracks
            .iter()
            .filter(|s| s.track.meta.id.kind() == TrackKind::Audio)
            .map(|s| s.track.meta.id)
    }

    pub fn poll_slow(&mut self, now: Instant) {
        self.published_tracks
            .iter_mut()
            .for_each(|slot| slot.track.poll_stats(now));
    }
}
