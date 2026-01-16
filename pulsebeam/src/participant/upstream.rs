use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::StreamMap;

use crate::{rtp::RtpPacket, track::KeyframeRequestStream};
use str0m::media::{Mid, Rid};

use crate::track::TrackSender;

const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);
type StreamKey = (Mid, Option<Rid>);

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
    pub keyframe_request_streams: StreamMap<StreamKey, KeyframeRequestStream>,
}

impl UpstreamAllocator {
    pub fn new() -> Self {
        Self {
            published_tracks: Vec::new(),
            keyframe_request_streams: StreamMap::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, mid: Mid, mut track: TrackSender) {
        if track.meta.kind.is_video() {
            for sender in track.simulcast.iter_mut() {
                let key = (mid, sender.rid);
                self.keyframe_request_streams
                    .insert(key, sender.keyframe_request_stream(KEYFRAME_DEBOUNCE));
            }
        }

        let slot = UpstreamSlot { mid, track };
        if !self.published_tracks.contains(&slot) {
            self.published_tracks.push(slot);
        }
    }

    pub fn handle_incoming_rtp(
        &mut self,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        mut rtp: RtpPacket,
    ) {
        if let Some(slot) = self.published_tracks.iter_mut().find(|t| t.mid == mid) {
            rtp.raw_header.ext_vals.rid = rid.cloned();
            slot.track.forward(rid, rtp);
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
