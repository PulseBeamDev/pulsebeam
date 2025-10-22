use std::collections::HashMap;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use str0m::media::Mid;

use crate::track::{KeyframeRequest, TrackSender};

/// Manages all upstream tracks published by this participant.
///
/// This allocator's primary responsibilities are:
/// 1.  Owning the `TrackSender` for each track published by the client.
/// 2.  Receiving raw incoming RTP packets from the `ParticipantCore`.
/// 3.  Pushing each packet into the appropriate per-simulcast-layer `JitterBuffer`.
/// 4.  Being polled regularly to drain ready packets from the jitter buffers and
///     forward them into the track's SPMC channel for consumption by downstream allocators.
/// 5.  Aggregating keyframe requests from subscribers to be sent back to the publisher.
pub struct UpstreamAllocator {
    published_tracks: HashMap<Mid, TrackSender>,
}

impl UpstreamAllocator {
    pub fn new() -> Self {
        Self {
            published_tracks: HashMap::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, track: TrackSender) {
        self.published_tracks
            .insert(track.meta.id.origin_mid, track);
    }

    /// Handles an incoming RTP packet from the RTC engine.
    ///
    /// This method finds the correct simulcast layer and pushes the packet
    /// into its jitter buffer.
    pub fn handle_incoming_rtp(
        &mut self,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        mut rtp: RtpPacket,
    ) {
        if let Some(track) = self.published_tracks.get_mut(&mid) {
            rtp.header.ext_vals.rid = rid.cloned();
            track.push(rid, rtp);
        } else {
            tracing::warn!(%mid, ?rid, "Dropping incoming RTP packet; no published track found");
        }
    }

    /// Polls all jitter buffers for all tracks to release ready packets.
    pub fn poll(&mut self, now: Instant) {
        for track in self.published_tracks.values_mut() {
            track.poll(now);
        }
    }

    /// Drains pending keyframe requests from all managed tracks.
    pub fn drain_keyframe_requests(&mut self) -> Vec<KeyframeRequest> {
        let mut requests = Vec::new();
        for track in self.published_tracks.values_mut() {
            for sender in &mut track.simulcast {
                if let Some(req) = sender.get_keyframe_request() {
                    requests.push(req);
                }
            }
        }
        requests
    }
}
