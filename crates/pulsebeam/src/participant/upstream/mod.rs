#[cfg(test)]
mod binding_tests;

use crate::keys::TrackHandle;
use crate::media::{Mid, Ssrc};
use crate::{
    entity::{PublicationKey, TrackKind},
    log::{LogCtx, plog_warn},
    rtp::RtpPacket,
    track::UpstreamTrack,
};
use pulsebeam_runtime::time::Instant;
use slotmap::Key as _;
use std::collections::BTreeMap;

pub(crate) const MAX_UPSTREAM_SLOT_PER_TYPE: usize = 32;
pub(crate) const MAX_UPSTREAM_ENCODED_STREAMS: usize =
    MAX_UPSTREAM_SLOT_PER_TYPE * (1 + crate::track::MAX_SIMULCAST_LAYERS);

#[derive(Clone, Copy)]
pub(crate) struct IncomingRtpRoute {
    pub(crate) ssrc: Ssrc,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<crate::media::Rid>,
    pub(crate) encoding: crate::track::SourceEncoding,
    pub(crate) upstream_slot: UpstreamSlotKey,
    pub(crate) publication_id: PublicationKey,
    pub(crate) fanout: Option<TrackHandle>,
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
    pub(crate) fn remove_track(&mut self, publication_id: PublicationKey) {
        let mut index = 0;
        while index < self.routes.len() {
            if self
                .routes
                .get(index)
                .is_some_and(|route| route.publication_id == publication_id)
            {
                self.ssrcs.swap_remove(index);
                self.routes.swap_remove(index);
            } else {
                index = index.saturating_add(1);
            }
        }
        debug_assert_eq!(self.ssrcs.len(), self.routes.len());
    }
    pub(crate) fn bind_fanout(&mut self, publication_id: PublicationKey, fanout: TrackHandle) {
        for route in &mut self.routes {
            if route.publication_id == publication_id {
                route.fanout = Some(fanout);
            }
        }
    }
}

pub(crate) struct UpstreamSlot {
    media_index: Option<u32>,
    mid: Mid,
    track: UpstreamTrack,
    descriptor: Option<Box<crate::track::Track>>,
    in_topology: bool,
}

pub(crate) struct UpstreamMedia {
    ctx: LogCtx,
    kind: TrackKind,
    published_tracks: BTreeMap<usize, UpstreamSlot>,
    next_slot: usize,
    negotiated_slots: Vec<usize>,
}

impl UpstreamMedia {
    fn new(ctx: LogCtx, kind: TrackKind) -> Self {
        Self {
            ctx,
            kind,
            published_tracks: BTreeMap::new(),
            next_slot: 0,
            negotiated_slots: Vec::new(),
        }
    }
    fn add_published_track(
        &mut self,
        media_index: u32,
        mid: Mid,
        create: impl FnOnce() -> (UpstreamTrack, crate::track::Track),
    ) -> bool {
        if self
            .published_tracks
            .values()
            .any(|s| s.media_index.is_some() && s.mid == mid)
        {
            plog_warn!(self.ctx, "duplicated publisher slot mid={}.", mid);
            return false;
        }
        if self.negotiated_slots.len() >= MAX_UPSTREAM_SLOT_PER_TYPE {
            return false;
        }
        let Some(index) = self.allocate_slot() else {
            return false;
        };
        let (track, descriptor) = create();
        debug_assert_eq!(track.meta.id.kind(), self.kind);
        self.published_tracks.insert(
            index,
            UpstreamSlot {
                media_index: Some(media_index),
                mid,
                track,
                descriptor: Some(Box::new(descriptor)),
                in_topology: false,
            },
        );
        self.negotiated_slots.push(index);
        true
    }

    fn allocate_slot(&mut self) -> Option<usize> {
        let index = self.next_slot;
        self.next_slot = self.next_slot.checked_add(1)?;
        Some(index)
    }

    fn slot_for_mid(&self, mid: Mid) -> Option<(usize, PublicationKey)> {
        self.negotiated_slots.iter().find_map(|&index| {
            let slot = self.published_tracks.get(&index)?;
            (slot.mid == mid).then_some((index, slot.track.meta.id))
        })
    }
    fn handle_incoming_rtp(
        &mut self,
        index: usize,
        mid: Mid,
        rid: Option<&crate::media::Rid>,
        rtp: RtpPacket,
    ) -> crate::track::ProcessedRtp {
        let Some(slot) = self.published_tracks.get_mut(&index) else {
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
        rtp.ext_vals.rid = rid.cloned();
        slot.track.monitor.process(rid, rtp)
    }
    fn announce_state_mut(&mut self, mid: Mid) -> Option<(&crate::track::Track, &mut bool)> {
        let slot = self
            .published_tracks
            .values_mut()
            .find(|s| s.media_index.is_some() && s.mid == mid)?;
        Some((slot.descriptor.as_deref()?, &mut slot.in_topology))
    }
    fn mid_for_track_id(&self, publication_id: PublicationKey) -> Option<Mid> {
        self.published_tracks
            .values()
            .find(|t| t.media_index.is_some() && t.track.meta.id == publication_id)
            .map(|t| t.mid)
    }
    fn poll_slow(&mut self, now: Instant) {
        for index in &self.negotiated_slots {
            if let Some(slot) = self.published_tracks.get_mut(index) {
                slot.track.poll_stats(now);
            }
        }
    }
}

fn track_key(publication_id: PublicationKey) -> (u8, [u8; 16]) {
    let kind = match publication_id.kind() {
        TrackKind::Audio => 0,
        TrackKind::Video => 1,
        TrackKind::Data => 2,
    };
    (kind, *publication_id.as_bytes())
}

pub struct Upstream {
    audio: UpstreamMedia,
    video: UpstreamMedia,
    pub(crate) routes: UpstreamRouteTable,
    managed_slots: BTreeMap<(u8, [u8; 16]), UpstreamSlotKey>,
    track_handles: BTreeMap<(u8, [u8; 16]), TrackHandle>,
    fanouts: BTreeMap<u64, PublicationKey>,
}
pub type UpstreamAllocator = Upstream;

impl Upstream {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            audio: UpstreamMedia::new(ctx, TrackKind::Audio),
            video: UpstreamMedia::new(ctx, TrackKind::Video),
            routes: UpstreamRouteTable::default(),
            managed_slots: BTreeMap::new(),
            track_handles: BTreeMap::new(),
            fanouts: BTreeMap::new(),
        }
    }
    pub fn add_published_track(
        &mut self,
        media_index: u32,
        mid: Mid,
        kind: TrackKind,
        create: impl FnOnce() -> (UpstreamTrack, crate::track::Track),
    ) -> bool {
        match kind {
            TrackKind::Audio => self.audio.add_published_track(media_index, mid, create),
            TrackKind::Video => self.video.add_published_track(media_index, mid, create),
            TrackKind::Data => false,
        }
    }

    pub fn slot_for_mid(&self, mid: Mid) -> Option<(UpstreamSlotKey, PublicationKey)> {
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
        rid: Option<&crate::media::Rid>,
        rtp: RtpPacket,
    ) -> crate::track::ProcessedRtp {
        match slot {
            UpstreamSlotKey::Audio(index) => self.audio.handle_incoming_rtp(index, mid, rid, rtp),
            UpstreamSlotKey::Video(index) => self.video.handle_incoming_rtp(index, mid, rid, rtp),
        }
    }
    pub fn announce_state_mut(&mut self, mid: Mid) -> Option<(&crate::track::Track, &mut bool)> {
        self.audio
            .announce_state_mut(mid)
            .or_else(|| self.video.announce_state_mut(mid))
    }
    pub fn mid_for_track_id(&self, publication_id: PublicationKey) -> Option<Mid> {
        self.audio
            .mid_for_track_id(publication_id)
            .or_else(|| self.video.mid_for_track_id(publication_id))
    }
    pub fn poll_slow(&mut self, now: Instant) {
        self.audio.poll_slow(now);
        self.video.poll_slow(now);
    }

    pub(crate) fn insert_managed_track(
        &mut self,
        mid: Mid,
        kind: TrackKind,
        create: impl FnOnce() -> (UpstreamTrack, crate::track::Track),
    ) -> Option<(UpstreamSlotKey, crate::track::Track)> {
        let (media, slot_key): (_, fn(usize) -> UpstreamSlotKey) = match kind {
            TrackKind::Audio => (&mut self.audio, UpstreamSlotKey::Audio),
            TrackKind::Video => (&mut self.video, UpstreamSlotKey::Video),
            TrackKind::Data => return None,
        };
        let index = media.allocate_slot()?;
        let (track, descriptor) = create();
        debug_assert_eq!(track.meta.id.kind(), kind);
        let id = track_key(track.meta.id);
        let slot = slot_key(index);
        let std::collections::btree_map::Entry::Vacant(entry) = self.managed_slots.entry(id) else {
            return None;
        };
        entry.insert(slot);
        media.published_tracks.insert(
            index,
            UpstreamSlot {
                media_index: None,
                mid,
                track,
                descriptor: None,
                in_topology: false,
            },
        );
        Some((slot, descriptor))
    }

    pub(crate) fn remove_managed_track(
        &mut self,
        key: UpstreamSlotKey,
        publication_id: PublicationKey,
    ) {
        let slots = match key {
            UpstreamSlotKey::Audio(_) => &mut self.audio.published_tracks,
            UpstreamSlotKey::Video(_) => &mut self.video.published_tracks,
        };
        let index = match key {
            UpstreamSlotKey::Audio(index) | UpstreamSlotKey::Video(index) => index,
        };
        if slots
            .get(&index)
            .is_some_and(|slot| slot.media_index.is_none() && slot.track.meta.id == publication_id)
            && self.managed_slots.get(&track_key(publication_id)) == Some(&key)
        {
            slots.remove(&index);
            self.managed_slots.remove(&track_key(publication_id));
            self.routes.remove_track(publication_id);
        }
    }

    pub(crate) fn owns_managed_track(&self, publication_id: PublicationKey) -> bool {
        self.managed_slots.contains_key(&track_key(publication_id))
    }

    pub(crate) fn track_mut(&mut self, key: UpstreamSlotKey) -> Option<&mut UpstreamTrack> {
        let slot = match key {
            UpstreamSlotKey::Audio(index) => self.audio.published_tracks.get_mut(&index),
            UpstreamSlotKey::Video(index) => self.video.published_tracks.get_mut(&index),
        }?;
        Some(&mut slot.track)
    }

    pub(crate) fn track_fanout(&self, publication_id: PublicationKey) -> Option<TrackHandle> {
        self.track_handles.get(&track_key(publication_id)).copied()
    }
    pub(crate) fn track_for_fanout(&self, fanout: TrackHandle) -> Option<PublicationKey> {
        self.fanouts.get(&fanout.data().as_ffi()).copied()
    }
    pub(crate) fn bind_track_handle(&mut self, publication_id: PublicationKey, key: TrackHandle) {
        if let Some(previous) = self.track_handles.insert(track_key(publication_id), key)
            && previous != key
        {
            self.fanouts.remove(&previous.data().as_ffi());
        }
        if let Some(previous) = self.fanouts.insert(key.data().as_ffi(), publication_id)
            && previous != publication_id
        {
            self.track_handles.remove(&track_key(previous));
            self.routes.remove_track(previous);
        }
        self.routes.bind_fanout(publication_id, key);
    }
    pub(crate) fn unbind_track_handle(&mut self, publication_id: PublicationKey, key: TrackHandle) {
        if self.track_handles.get(&track_key(publication_id)) == Some(&key) {
            self.track_handles.remove(&track_key(publication_id));
            self.fanouts.remove(&key.data().as_ffi());
            self.routes.remove_track(publication_id);
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
