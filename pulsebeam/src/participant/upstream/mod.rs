mod audio;
mod data;
mod video;

use crate::keys::TrackKey;
use crate::{
    entity::{TrackId, TrackKind},
    log::{LogCtx, plog_warn},
    rtp::{EncodingId as Rid, MediaSectionId as Mid, RtpPacket, SenderReport, Ssrc},
    track::UpstreamTrack,
};
use ahash::{HashMap, HashMapExt};
pub(crate) use audio::UpstreamAudio;
pub(crate) use data::UpstreamData;
use tokio::time::Instant;
pub(crate) use video::UpstreamVideo;

pub(crate) const MAX_UPSTREAM_SLOT_PER_TYPE: usize = 2;
pub(crate) const MAX_UPSTREAM_ENCODED_STREAMS: usize =
    MAX_UPSTREAM_SLOT_PER_TYPE * (1 + crate::track::MAX_SIMULCAST_LAYERS);

#[derive(Clone, Copy)]
pub(crate) struct IncomingRtpRoute {
    pub(crate) ssrc: Ssrc,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) upstream_slot: UpstreamSlotKey,
    pub(crate) track_id: TrackId,
    pub(crate) fanout: Option<TrackKey>,
}

#[derive(Default)]
pub(crate) struct UpstreamRouteTable {
    pub(crate) ssrcs: Vec<Ssrc>,
    pub(crate) routes: Vec<IncomingRtpRoute>,
}

const _: () = assert!(std::mem::size_of::<Ssrc>() == 4);

impl UpstreamRouteTable {
    fn index_of(&self, ssrc: Ssrc) -> Option<usize> {
        self.ssrcs.iter().position(|&known| known == ssrc)
    }
    pub(crate) fn get(&self, ssrc: Ssrc) -> Option<IncomingRtpRoute> {
        self.routes.get(self.index_of(ssrc)?).copied()
    }
    pub(crate) fn insert(&mut self, route: IncomingRtpRoute) {
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
        if let Some(index) = self.index_of(route.ssrc) {
            if let Some(slot) = self.routes.get_mut(index) {
                *slot = route;
            }
            return;
        }
        if self.routes.len() >= MAX_UPSTREAM_ENCODED_STREAMS {
            debug_assert!(
                false,
                "more encoded streams than MAX_UPSTREAM_ENCODED_STREAMS allows"
            );
            metrics::counter!("upstream_route_table_full").increment(1);
            return;
        }
        self.ssrcs.push(route.ssrc);
        self.routes.push(route);
    }
    pub(crate) fn remove(&mut self, ssrc: Ssrc) {
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
        let Some(index) = self.index_of(ssrc) else {
            return;
        };
        self.ssrcs.swap_remove(index);
        self.routes.swap_remove(index);
    }
    pub(crate) fn clear(&mut self) {
        self.ssrcs.clear();
        self.routes.clear();
    }
    pub(crate) fn remove_track(&mut self, track_id: TrackId) {
        let mut index = 0;
        while index < self.routes.len() {
            if self
                .routes
                .get(index)
                .is_some_and(|route| route.track_id == track_id)
            {
                self.ssrcs.swap_remove(index);
                self.routes.swap_remove(index);
            } else {
                index = index.saturating_add(1);
            }
        }
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
    }
    pub(crate) fn bind_fanout(&mut self, track_id: TrackId, fanout: TrackKey) {
        for route in &mut self.routes {
            if route.track_id == track_id {
                route.fanout = Some(fanout);
            }
        }
    }
}

pub(crate) struct UpstreamSlot {
    mid: Mid,
    track: UpstreamTrack,
    descriptor: crate::track::Track,
    in_topology: bool,
}

pub(crate) struct UpstreamMedia {
    ctx: LogCtx,
    kind: TrackKind,
    published_tracks: Vec<UpstreamSlot>,
}

impl UpstreamMedia {
    fn new(ctx: LogCtx, kind: TrackKind) -> Self {
        Self {
            ctx,
            kind,
            published_tracks: Vec::new(),
        }
    }
    fn add_published_track(
        &mut self,
        mid: Mid,
        track: UpstreamTrack,
        descriptor: crate::track::Track,
    ) -> bool {
        debug_assert_eq!(track.meta.id.kind(), self.kind);
        if self.published_tracks.iter().any(|s| s.mid == mid) {
            plog_warn!(self.ctx, "duplicated slot mid={}.", mid);
            return false;
        }
        if self.published_tracks.len() >= MAX_UPSTREAM_SLOT_PER_TYPE {
            return false;
        }
        self.published_tracks.push(UpstreamSlot {
            mid,
            track,
            descriptor,
            in_topology: false,
        });
        true
    }
    fn slot_for_mid(&self, mid: Mid) -> Option<(usize, TrackId)> {
        self.published_tracks
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.mid == mid)
            .map(|(index, slot)| (index, slot.track.meta.id))
    }
    fn handle_incoming_rtp(
        &mut self,
        index: usize,
        mid: Mid,
        rid: Option<&Rid>,
        rtp: RtpPacket,
        sr: Option<SenderReport>,
    ) -> crate::track::ProcessedRtp {
        let Some(slot) = self.published_tracks.get_mut(index) else {
            debug_assert!(false, "cached upstream slot index is out of bounds");
            return crate::track::ProcessedRtp {
                first: None,
                remaining: Vec::new(),
                request_keyframe: false,
                valid_route: false,
            };
        };
        debug_assert_eq!(slot.mid, mid);
        if slot.mid != mid {
            plog_warn!(self.ctx, %mid, ?rid, "Dropping incoming RTP packet; cached published track changed");
            return crate::track::ProcessedRtp {
                first: None,
                remaining: Vec::new(),
                request_keyframe: false,
                valid_route: false,
            };
        }
        let mut rtp = rtp;
        rtp.extensions.rid = rid.map(|rid| crate::rtp::EncodingId::from(&**rid));
        slot.track.process(rid, rtp, sr)
    }
    fn announce_state_mut(&mut self, mid: Mid) -> Option<(&crate::track::Track, &mut bool)> {
        let slot = self.published_tracks.iter_mut().find(|s| s.mid == mid)?;
        Some((&slot.descriptor, &mut slot.in_topology))
    }
    fn mid_for_track_id(&self, track_id: TrackId) -> Option<Mid> {
        self.published_tracks
            .iter()
            .find(|t| t.track.meta.id == track_id)
            .map(|t| t.mid)
    }
    fn poll_slow(&mut self, now: Instant) {
        for slot in &mut self.published_tracks {
            slot.track.poll_stats(now);
        }
    }
}

pub struct Upstream {
    pub(crate) audio: UpstreamAudio,
    pub(crate) video: UpstreamVideo,
    pub(crate) data: UpstreamData,
    pub(crate) routes: UpstreamRouteTable,
    track_keys: HashMap<TrackId, TrackKey>,
}
pub type UpstreamAllocator = Upstream;

impl Upstream {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            audio: UpstreamAudio::new(ctx),
            video: UpstreamVideo::new(ctx),
            data: UpstreamData::new(),
            routes: UpstreamRouteTable::default(),
            track_keys: HashMap::new(),
        }
    }
    pub fn add_published_track(
        &mut self,
        mid: Mid,
        track: UpstreamTrack,
        descriptor: crate::track::Track,
    ) -> bool {
        match track.meta.id.kind() {
            TrackKind::Audio => self.audio.add_published_track(mid, track, descriptor),
            TrackKind::Video => self.video.add_published_track(mid, track, descriptor),
            TrackKind::Data => {
                pulsebeam_runtime::fatal!("a data channel reached upstream track construction")
            }
        }
    }
    pub fn slot_for_mid(&self, mid: Mid) -> Option<(UpstreamSlotKey, TrackId)> {
        self.audio
            .slot_for_mid(mid)
            .map(|(index, id)| (UpstreamSlotKey::Audio(index), id))
            .or_else(|| {
                self.video
                    .slot_for_mid(mid)
                    .map(|(index, id)| (UpstreamSlotKey::Video(index), id))
            })
    }
    pub fn handle_incoming_rtp(
        &mut self,
        slot: UpstreamSlotKey,
        mid: Mid,
        rid: Option<&Rid>,
        rtp: RtpPacket,
        sr: Option<SenderReport>,
    ) -> crate::track::ProcessedRtp {
        match slot {
            UpstreamSlotKey::Audio(index) => {
                self.audio.handle_incoming_rtp(index, mid, rid, rtp, sr)
            }
            UpstreamSlotKey::Video(index) => {
                self.video.handle_incoming_rtp(index, mid, rid, rtp, sr)
            }
        }
    }
    pub fn announce_state_mut(&mut self, mid: Mid) -> Option<(&crate::track::Track, &mut bool)> {
        self.audio
            .announce_state_mut(mid)
            .or_else(|| self.video.announce_state_mut(mid))
    }
    pub fn mid_for_track_id(&self, track_id: TrackId) -> Option<Mid> {
        self.audio
            .mid_for_track_id(track_id)
            .or_else(|| self.video.mid_for_track_id(track_id))
    }
    pub fn poll_slow(&mut self, now: Instant) {
        self.audio.poll_slow(now);
        self.video.poll_slow(now);
    }

    pub(crate) fn track_fanout(&self, track_id: TrackId) -> Option<TrackKey> {
        self.track_keys.get(&track_id).copied()
    }
    pub(crate) fn track_for_fanout(&self, fanout: TrackKey) -> Option<TrackId> {
        self.track_keys
            .iter()
            .find_map(|(track_id, key)| (*key == fanout).then_some(*track_id))
    }
    pub(crate) fn bind_track_key(&mut self, track_id: TrackId, key: TrackKey) {
        self.track_keys.insert(track_id, key);
        self.routes.bind_fanout(track_id, key);
    }
    pub(crate) fn unbind_track_key(&mut self, track_id: TrackId, key: TrackKey) {
        if self.track_keys.get(&track_id) == Some(&key) {
            self.track_keys.remove(&track_id);
            self.routes.remove_track(track_id);
        }
    }
    pub(crate) fn route_for_ssrc(&self, ssrc: Ssrc) -> Option<IncomingRtpRoute> {
        self.routes.get(ssrc)
    }
    pub(crate) fn cache_route(&mut self, route: IncomingRtpRoute) {
        self.routes.insert(route);
    }
    pub(crate) fn remove_route(&mut self, ssrc: Ssrc) {
        self.routes.remove(ssrc);
    }
    pub(crate) fn clear_routes(&mut self) {
        self.routes.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamSlotKey {
    Audio(usize),
    Video(usize),
}
