use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

use crate::{
    participant::{ParticipantEvent, ParticipantEvents},
    rtp::RtpPacket,
    track::TrackSender,
};
use str0m::media::{MediaKind, Mid};

const MAX_UPSTREAM_SLOT_PER_TYPE: usize = 1;

struct UpstreamSlot {
    mid: Mid,
    track: TrackSender,
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
    pub fn add_published_track(&mut self, mid: Mid, track: TrackSender) -> bool {
        if self.published_tracks.iter().any(|s| s.mid == mid) {
            tracing::warn!("duplicated slot mid={}.", mid);
            return false;
        }

        match track.meta.kind {
            MediaKind::Video => {
                let video_count = self
                    .published_tracks
                    .iter()
                    .filter(|s| s.track.meta.kind == MediaKind::Video)
                    .count();

                if video_count >= MAX_UPSTREAM_SLOT_PER_TYPE {
                    return false;
                }
            }
            MediaKind::Audio => {
                let audio_count = self
                    .published_tracks
                    .iter()
                    .filter(|s| s.track.meta.kind == MediaKind::Audio)
                    .count();

                if audio_count >= MAX_UPSTREAM_SLOT_PER_TYPE {
                    return false;
                }
            }
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
            tracing::warn!(%mid, ?rid, "Dropping incoming RTP packet; no published track found");
        }
    }

    pub fn poll_slow(&mut self, now: Instant) {
        self.published_tracks
            .iter_mut()
            .for_each(|slot| slot.track.poll_stats(now));
    }
}
