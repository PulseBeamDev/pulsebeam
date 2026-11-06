#[cfg(test)]
mod binding_tests;

use crate::keys::TrackHandle;
use crate::media::{Mid, Ssrc};
use crate::rtp::sync::SenderInfo;
use crate::{
    entity::{ConnectionId, TrackId, TrackKind},
    log::{LogCtx, plog_warn},
    rtp::RtpPacket,
    track::UpstreamTrack,
};
use ahash::HashSetExt;
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
    pub(crate) track_id: TrackId,
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
    pub(crate) fn bind_fanout(&mut self, track_id: TrackId, fanout: TrackHandle) {
        for route in &mut self.routes {
            if route.track_id == track_id {
                route.fanout = Some(fanout);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "reported by the native v1 publish-intent path")]
pub(crate) enum SenderLabelError {
    UnknownSender,
    AlreadyBound,
    AlreadyPublished,
    DuplicateLabel,
}

pub(crate) struct UpstreamSlot {
    media_index: Option<u32>,
    mid: Mid,
    track: UpstreamTrack,
    descriptor: Option<Box<crate::track::Track>>,
    in_topology: bool,
    published_once: bool,
}

impl UpstreamSlot {
    fn bind_label(&mut self, id: TrackId, label: &str) {
        self.track.meta.id = id;
        self.track.meta.label = Some(label.to_owned());
        if let Some(descriptor) = &mut self.descriptor {
            descriptor.meta_mut().id = id;
            descriptor.meta_mut().label = Some(label.to_owned());
            if let crate::track::Track::Video(video) = descriptor.as_mut() {
                for layer in &mut video.layers {
                    layer.meta = video.meta.clone();
                }
            }
        }
    }
}

pub(crate) struct UpstreamMedia {
    ctx: LogCtx,
    kind: TrackKind,
    connection_id: ConnectionId,
    published_tracks: BTreeMap<usize, UpstreamSlot>,
    next_slot: usize,
    negotiated_slots: Vec<usize>,
}

impl UpstreamMedia {
    fn new(ctx: LogCtx, kind: TrackKind, connection_id: ConnectionId) -> Self {
        Self {
            ctx,
            kind,
            connection_id,
            published_tracks: BTreeMap::new(),
            next_slot: 0,
            negotiated_slots: Vec::new(),
        }
    }
    fn add_published_track(
        &mut self,
        media_index: u32,
        mid: Mid,
        track: UpstreamTrack,
        descriptor: crate::track::Track,
    ) -> bool {
        debug_assert_eq!(track.meta.id.kind(), self.kind);
        if self
            .published_tracks
            .values()
            .any(|s| s.media_index.is_some() && (s.mid == mid || s.track.meta.id == track.meta.id))
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
        self.published_tracks.insert(
            index,
            UpstreamSlot {
                media_index: Some(media_index),
                mid,
                track,
                descriptor: Some(Box::new(descriptor)),
                in_topology: false,
                published_once: false,
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

    fn add_managed_track(&mut self, mid: Mid, track: UpstreamTrack) -> Option<usize> {
        let index = self.allocate_slot()?;
        self.published_tracks.insert(
            index,
            UpstreamSlot {
                media_index: None,
                mid,
                track,
                descriptor: None,
                in_topology: false,
                published_once: false,
            },
        );
        Some(index)
    }
    fn slot_for_mid(&self, mid: Mid) -> Option<(usize, TrackId)> {
        self.negotiated_slots.iter().find_map(|&index| {
            let slot = self.published_tracks.get(&index)?;
            (slot.mid == mid).then_some((index, slot.track.meta.id))
        })
    }
    fn track_for_sender_index(&self, media_index: u32) -> Option<TrackId> {
        self.published_tracks
            .values()
            .find(|slot| slot.media_index == Some(media_index))
            .map(|slot| slot.track.meta.id)
    }

    fn bind_sender_label(
        &mut self,
        media_index: u32,
        label: &str,
    ) -> Result<(TrackId, TrackId), SenderLabelError> {
        let slot_index = self
            .published_tracks
            .iter()
            .find_map(|(&index, slot)| (slot.media_index == Some(media_index)).then_some(index))
            .ok_or(SenderLabelError::UnknownSender)?;
        let slot = self
            .published_tracks
            .get(&slot_index)
            .ok_or(SenderLabelError::UnknownSender)?;
        if let Some(bound) = &slot.track.meta.label {
            return (bound == label)
                .then_some((slot.track.meta.id, slot.track.meta.id))
                .ok_or(SenderLabelError::AlreadyBound);
        }
        if slot.published_once {
            return Err(SenderLabelError::AlreadyPublished);
        }
        let id = self.connection_id.derive_track_id(self.kind, label);
        if self.published_tracks.iter().any(|(&index, other)| {
            other.media_index.is_some() && index != slot_index && other.track.meta.id == id
        }) {
            return Err(SenderLabelError::DuplicateLabel);
        }
        let slot = self
            .published_tracks
            .get_mut(&slot_index)
            .ok_or(SenderLabelError::UnknownSender)?;
        let previous = slot.track.meta.id;
        slot.bind_label(id, label);
        Ok((previous, id))
    }
    fn plan_sender_labels(
        &self,
        labels: &[(u32, &str)],
    ) -> Result<Vec<(usize, TrackId, String)>, SenderLabelError> {
        let mut replacements = Vec::new();
        for &(media_index, label) in labels {
            let Some(index) = self.published_tracks.iter().find_map(|(&index, slot)| {
                (slot.media_index == Some(media_index)).then_some(index)
            }) else {
                continue;
            };
            let slot = self
                .published_tracks
                .get(&index)
                .ok_or(SenderLabelError::UnknownSender)?;
            if let Some(bound) = &slot.track.meta.label {
                if bound != label {
                    return Err(SenderLabelError::AlreadyBound);
                }
                continue;
            }
            if slot.published_once {
                return Err(SenderLabelError::AlreadyPublished);
            }
            if replacements.iter().any(|(other, _, _)| *other == index) {
                return Err(SenderLabelError::DuplicateLabel);
            }
            replacements.push((
                index,
                self.connection_id.derive_track_id(self.kind, label),
                label.to_owned(),
            ));
        }
        let mut identities = ahash::HashSet::new();
        for (&index, slot) in self
            .published_tracks
            .iter()
            .filter(|(_, slot)| slot.media_index.is_some())
        {
            let id = replacements
                .iter()
                .find(|(other, _, _)| *other == index)
                .map_or(slot.track.meta.id, |(_, id, _)| *id);
            if !identities.insert(id) {
                return Err(SenderLabelError::DuplicateLabel);
            }
        }
        Ok(replacements)
    }

    fn apply_sender_labels(&mut self, replacements: Vec<(usize, TrackId, String)>) -> bool {
        let changed = !replacements.is_empty();
        for (index, id, label) in replacements {
            let Some(slot) = self.published_tracks.get_mut(&index) else {
                return false;
            };
            slot.bind_label(id, &label);
        }
        changed
    }

    fn sender_report(&mut self, mid: Mid, report: SenderInfo, arrival: Instant) {
        if let Some(slot) = self
            .published_tracks
            .values_mut()
            .find(|slot| slot.media_index.is_some() && slot.mid == mid)
        {
            slot.track.sender_report(report, arrival);
        }
    }

    fn handle_incoming_rtp(
        &mut self,
        index: usize,
        mid: Mid,
        rid: Option<&crate::media::Rid>,
        rtp: RtpPacket,
        sr: Option<SenderInfo>,
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
        slot.track.process(rid, rtp, sr)
    }
    fn announce_state_mut(
        &mut self,
        mid: Mid,
        active: bool,
    ) -> Option<(&crate::track::Track, &mut bool)> {
        let slot = self
            .published_tracks
            .values_mut()
            .find(|s| s.media_index.is_some() && s.mid == mid)?;
        if active {
            slot.published_once = true;
        }
        Some((slot.descriptor.as_deref()?, &mut slot.in_topology))
    }
    fn mid_for_track_id(&self, track_id: TrackId) -> Option<Mid> {
        self.published_tracks
            .values()
            .find(|t| t.media_index.is_some() && t.track.meta.id == track_id)
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

fn track_key(track_id: TrackId) -> (u8, [u8; 16]) {
    let kind = match track_id.kind() {
        TrackKind::Audio => 0,
        TrackKind::Video => 1,
        TrackKind::Data => 2,
    };
    (kind, *track_id.as_bytes())
}

pub struct Upstream {
    audio: UpstreamMedia,
    video: UpstreamMedia,
    pub(crate) routes: UpstreamRouteTable,
    managed_slots: BTreeMap<(u8, [u8; 16]), UpstreamSlotKey>,
    track_handles: BTreeMap<(u8, [u8; 16]), TrackHandle>,
    fanouts: BTreeMap<u64, TrackId>,
}
pub type UpstreamAllocator = Upstream;

impl Upstream {
    pub(crate) fn new(ctx: LogCtx, connection_id: ConnectionId) -> Self {
        Self {
            audio: UpstreamMedia::new(ctx, TrackKind::Audio, connection_id),
            video: UpstreamMedia::new(ctx, TrackKind::Video, connection_id),
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
        track: UpstreamTrack,
        descriptor: crate::track::Track,
    ) -> bool {
        match track.meta.id.kind() {
            TrackKind::Audio => self
                .audio
                .add_published_track(media_index, mid, track, descriptor),
            TrackKind::Video => self
                .video
                .add_published_track(media_index, mid, track, descriptor),
            TrackKind::Data => {
                pulsebeam_runtime::fatal!("a data channel reached upstream track construction")
            }
        }
    }
    pub(crate) fn track_for_sender_index(&self, media_index: u32) -> Option<TrackId> {
        self.audio
            .track_for_sender_index(media_index)
            .or_else(|| self.video.track_for_sender_index(media_index))
    }

    #[allow(dead_code, reason = "called by the native v1 publish-intent path")]
    pub(crate) fn bind_sender_label(
        &mut self,
        media_index: u32,
        kind: TrackKind,
        label: &str,
    ) -> Result<TrackId, SenderLabelError> {
        let (previous, id) = match kind {
            TrackKind::Audio => self.audio.bind_sender_label(media_index, label),
            TrackKind::Video => self.video.bind_sender_label(media_index, label),
            TrackKind::Data => return Err(SenderLabelError::UnknownSender),
        }?;
        // A sender can receive RTP before its intent. Its cached route still
        // carries the provisional MID-derived identity until rebuilt.
        if previous != id {
            self.routes.clear();
        }
        Ok(id)
    }
    pub(crate) fn bind_sender_labels_atomically(
        &mut self,
        labels: &[(u32, TrackKind, &str)],
    ) -> Result<(), SenderLabelError> {
        let mut audio = Vec::new();
        let mut video = Vec::new();
        for &(index, kind, label) in labels {
            match kind {
                TrackKind::Audio => audio.push((index, label)),
                TrackKind::Video => video.push((index, label)),
                TrackKind::Data => return Err(SenderLabelError::UnknownSender),
            }
        }
        let audio = self.audio.plan_sender_labels(&audio)?;
        let video = self.video.plan_sender_labels(&video)?;
        if self.audio.apply_sender_labels(audio) | self.video.apply_sender_labels(video) {
            self.routes.clear();
        }
        Ok(())
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
    pub(crate) fn sender_report(&mut self, mid: Mid, report: SenderInfo, arrival: Instant) {
        self.audio.sender_report(mid, report, arrival);
        self.video.sender_report(mid, report, arrival);
    }

    pub fn handle_incoming_rtp(
        &mut self,
        slot: UpstreamSlotKey,
        mid: Mid,
        rid: Option<&crate::media::Rid>,
        rtp: RtpPacket,
        sr: Option<SenderInfo>,
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
    pub fn announce_state_mut(
        &mut self,
        mid: Mid,
        active: bool,
    ) -> Option<(&crate::track::Track, &mut bool)> {
        self.audio
            .announce_state_mut(mid, active)
            .or_else(|| self.video.announce_state_mut(mid, active))
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

    pub(crate) fn insert_managed_track(
        &mut self,
        mid: Mid,
        track: UpstreamTrack,
    ) -> Option<UpstreamSlotKey> {
        let id = track_key(track.meta.id);
        if self.managed_slots.contains_key(&id) {
            return None;
        }
        let slot = match track.meta.id.kind() {
            TrackKind::Audio => self
                .audio
                .add_managed_track(mid, track)
                .map(UpstreamSlotKey::Audio),
            TrackKind::Video => self
                .video
                .add_managed_track(mid, track)
                .map(UpstreamSlotKey::Video),
            TrackKind::Data => None,
        }?;
        self.managed_slots.insert(id, slot);
        Some(slot)
    }

    pub(crate) fn remove_managed_track(&mut self, key: UpstreamSlotKey, track_id: TrackId) {
        let slots = match key {
            UpstreamSlotKey::Audio(_) => &mut self.audio.published_tracks,
            UpstreamSlotKey::Video(_) => &mut self.video.published_tracks,
        };
        let index = match key {
            UpstreamSlotKey::Audio(index) | UpstreamSlotKey::Video(index) => index,
        };
        if slots
            .get(&index)
            .is_some_and(|slot| slot.media_index.is_none() && slot.track.meta.id == track_id)
            && self.managed_slots.get(&track_key(track_id)) == Some(&key)
        {
            slots.remove(&index);
            self.managed_slots.remove(&track_key(track_id));
            self.routes.remove_track(track_id);
        }
    }

    pub(crate) fn owns_managed_track(&self, track_id: TrackId) -> bool {
        self.managed_slots.contains_key(&track_key(track_id))
    }

    pub(crate) fn track_mut(&mut self, key: UpstreamSlotKey) -> Option<&mut UpstreamTrack> {
        let slot = match key {
            UpstreamSlotKey::Audio(index) => self.audio.published_tracks.get_mut(&index),
            UpstreamSlotKey::Video(index) => self.video.published_tracks.get_mut(&index),
        }?;
        Some(&mut slot.track)
    }

    pub(crate) fn track_fanout(&self, track_id: TrackId) -> Option<TrackHandle> {
        self.track_handles.get(&track_key(track_id)).copied()
    }
    pub(crate) fn track_for_fanout(&self, fanout: TrackHandle) -> Option<TrackId> {
        self.fanouts.get(&fanout.data().as_ffi()).copied()
    }
    pub(crate) fn bind_track_handle(&mut self, track_id: TrackId, key: TrackHandle) {
        if let Some(previous) = self.track_handles.insert(track_key(track_id), key)
            && previous != key
        {
            self.fanouts.remove(&previous.data().as_ffi());
        }
        if let Some(previous) = self.fanouts.insert(key.data().as_ffi(), track_id)
            && previous != track_id
        {
            self.track_handles.remove(&track_key(previous));
            self.routes.remove_track(previous);
        }
        self.routes.bind_fanout(track_id, key);
    }
    pub(crate) fn unbind_track_handle(&mut self, track_id: TrackId, key: TrackHandle) {
        if self.track_handles.get(&track_key(track_id)) == Some(&key) {
            self.track_handles.remove(&track_key(track_id));
            self.fanouts.remove(&key.data().as_ffi());
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
