use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

use crate::{
    entity::{TrackId, TrackKind},
    log::{LogCtx, plog_warn},
    rtp::RtpPacket,
    track::UpstreamTrack,
};
use str0m::media::Mid;

pub(crate) const MAX_UPSTREAM_SLOT_PER_TYPE: usize = 2;
pub(crate) const MAX_UPSTREAM_ENCODED_STREAMS: usize =
    MAX_UPSTREAM_SLOT_PER_TYPE * (1 + crate::track::MAX_SIMULCAST_LAYERS);

struct UpstreamSlot {
    mid: Mid,
    track: UpstreamTrack,
    /// What was last handed to the shard, so only changes are published.
    last_published_stats: crate::track::TrackStates,
}

impl PartialEq for UpstreamSlot {
    fn eq(&self, other: &Self) -> bool {
        self.mid == other.mid
    }
}

impl Eq for UpstreamSlot {}

pub struct UpstreamAllocator {
    ctx: LogCtx,
    published_tracks: Vec<UpstreamSlot>,
}

impl UpstreamAllocator {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            published_tracks: Vec::new(),
        }
    }

    /// Adds a new locally published track that will receive RTP packets.
    pub fn add_published_track(&mut self, mid: Mid, track: UpstreamTrack) -> bool {
        if self.published_tracks.iter().any(|s| s.mid == mid) {
            plog_warn!(self.ctx, "duplicated slot mid={}.", mid);
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
            // Unreachable: an `UpstreamTrack` only comes from `new_video` or
            // `new_audio`, each of which asserts its own kind, and both callers
            // of this function pass one of those. Data channels never become an
            // upstream track at all — they go through `DataTrackIntent`.
            TrackKind::Data => unreachable!("data channels do not create upstream tracks"),
        }

        let slot = UpstreamSlot {
            mid,
            track,
            last_published_stats: Vec::new(),
        };
        self.published_tracks.push(slot);
        true
    }

    pub fn slot_for_mid(&self, mid: Mid) -> Option<(usize, TrackId)> {
        self.published_tracks
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.mid == mid)
            .map(|(index, slot)| (index, slot.track.meta.id))
    }

    pub fn handle_incoming_rtp(
        &mut self,
        slot_index: usize,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        rtp: &mut RtpPacket,
        sr: Option<SenderInfo>,
    ) -> bool {
        let Some(slot) = self.published_tracks.get_mut(slot_index) else {
            debug_assert!(false, "cached upstream slot index is out of bounds");
            return false;
        };
        debug_assert_eq!(slot.mid, mid);
        if slot.mid != mid {
            plog_warn!(self.ctx, %mid, ?rid, "Dropping incoming RTP packet; cached published track changed");
            return false;
        }

        rtp.ext_vals.rid = rid.cloned();
        slot.track.process(rid, rtp, sr)
    }

    /// Measurement handles for a published track, to hand along the media path.
    /// The controller never receives these.
    pub fn layer_states_for(&self, track_id: TrackId) -> crate::track::TrackStates {
        self.published_tracks
            .iter()
            .find(|t| t.track.meta.id == track_id)
            .map(|t| t.track.layer_states())
            .unwrap_or_default()
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

    /// Snapshots that have moved since they were last handed out.
    ///
    /// Emitted on change rather than on a schedule. The rate filters only move
    /// on the slow poll, but `process_packet` flips activity and health per
    /// packet — a shared handle showed that instantly, and publishing only
    /// every slow tick delayed it by up to that interval. That lag is enough to
    /// change an allocation decision: a viewer reconfiguring priorities reached
    /// a lower layer and the estimate collapsed with it.
    ///
    /// The check is a handful of `Copy` comparisons per published track, so it
    /// is cheap enough to run on the fast path where the change happens.
    pub fn take_changed_stats(&mut self) -> Vec<(TrackId, crate::track::TrackStates)> {
        let mut changed = Vec::new();
        for slot in &mut self.published_tracks {
            let current = slot.track.layer_states();
            if slot.last_published_stats != current {
                slot.last_published_stats.clone_from(&current);
                changed.push((slot.track.meta.id, current));
            }
        }
        changed
    }
}
