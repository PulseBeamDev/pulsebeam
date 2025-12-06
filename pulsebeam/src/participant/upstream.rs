use futures::{StreamExt, stream::SelectAll};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

use crate::{rtp::RtpPacket, track::KeyframeRequestStream};
use str0m::media::Mid;

use crate::track::TrackSender;

const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(300);

pub struct UpstreamAllocator {
    published_tracks: HashMap<Mid, TrackSender>,
    pub keyframe_request_streams: SelectAll<KeyframeRequestStream>,
}

impl UpstreamAllocator {
    pub fn new() -> Self {
        Self {
            published_tracks: HashMap::new(),
            keyframe_request_streams: SelectAll::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, mid: Mid, track: TrackSender) {
        if track.meta.kind.is_video() {
            for sender in &track.simulcast {
                self.keyframe_request_streams
                    .push(sender.keyframe_request_stream(KEYFRAME_DEBOUNCE));
            }
        }
        self.published_tracks.insert(mid, track);
    }

    pub fn handle_incoming_rtp(
        &mut self,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        mut rtp: RtpPacket,
    ) {
        if let Some(track) = self.published_tracks.get_mut(&mid) {
            rtp.raw_header.ext_vals.rid = rid.cloned();
            track.push(rid, rtp);
        } else {
            tracing::warn!(%mid, ?rid, "Dropping incoming RTP packet; no published track found");
        }
    }

    pub fn poll_stats(&mut self, now: Instant) {
        self.published_tracks
            .values_mut()
            .for_each(|track| track.poll_stats(now));
    }
}
