use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::track::{KeyframePoll, TrackSender};
use str0m::media::{KeyframeRequest, MediaKind, Mid};

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
    /// Wake handle shared by every [`KeyframeRequester`] for the active video track.
    pub keyframe_notify: Option<Arc<Notify>>,
    /// Per-simulcast-layer poll handles; drained by the upstream actor.
    pub keyframe_polls: Vec<KeyframePoll>,
}

impl UpstreamAllocator {
    pub fn new() -> Self {
        Self {
            published_tracks: Vec::new(),
            keyframe_notify: None,
            keyframe_polls: Vec::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, mid: Mid, mut track: TrackSender) -> bool {
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

                // Take ownership of the keyframe infrastructure.
                // MAX_UPSTREAM_SLOT_PER_TYPE == 1, so there is at most one video track.
                self.keyframe_notify = Some(track.keyframe_notify.clone());
                self.keyframe_polls.extend(track.keyframe_polls.drain(..));
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

    /// Await a keyframe notification.
    ///
    /// Returns immediately (via `std::future::pending`) when no video track is
    /// registered, so this can be used unconditionally in a `select!` branch.
    pub async fn notified(&self) {
        match &self.keyframe_notify {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    }

    /// Drain all pending keyframe requests, applying per-layer leading-edge debounce.
    ///
    /// Call this immediately after [`notified`] resolves. It is O(n) in the number
    /// of simulcast layers (typically 1–3) with no dynamic dispatch or allocation.
    pub fn drain_keyframe_requests(
        &mut self,
        now: Instant,
    ) -> impl Iterator<Item = KeyframeRequest> + '_ {
        self.keyframe_polls
            .iter_mut()
            .filter_map(move |p| p.take_pending(now))
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
