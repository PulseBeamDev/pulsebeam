mod audio;
mod data;
mod video;

use crate::keys::TrackHandle;
use crate::participant::intent::NativePublication;
use crate::{
    entity::{TrackId, TrackKind},
    log::{LogCtx, plog_warn},
    rtp::RtpPacket,
    track::UpstreamTrack,
};
use ahash::{HashMap, HashMapExt};
pub(crate) use audio::UpstreamAudio;
pub(crate) use data::UpstreamData;
use str0m::media::Mid;
use str0m::rtp::Ssrc;
use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;
pub(crate) use video::UpstreamVideo;

pub(crate) const MAX_UPSTREAM_SLOT_PER_TYPE: usize = crate::control::MAX_RTP_SLOTS_PER_TYPE;
pub(crate) const MAX_UPSTREAM_ENCODED_STREAMS: usize =
    MAX_UPSTREAM_SLOT_PER_TYPE * (1 + crate::track::MAX_SIMULCAST_LAYERS);

#[derive(Clone, Copy)]
pub(crate) struct IncomingRtpRoute {
    pub(crate) ssrc: Ssrc,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<str0m::media::Rid>,
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

pub(crate) struct UpstreamSlot {
    media_index: u32,
    mid: Mid,
    track: UpstreamTrack,
    descriptor: crate::track::Track,
    in_topology: bool,
    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    native_binding: Option<NativeSenderBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
)]
struct NativeSenderBinding {
    kind: TrackKind,
    label: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
)]
pub(crate) enum NativePublicationError {
    #[error("publication replacement is only available to native connections")]
    NotNative,
    #[error("native sender {sender_index} cannot change its established identity")]
    Relabel { sender_index: u32 },
    #[error("native identity {kind:?}/{label} cannot move to sender {sender_index}")]
    Move {
        sender_index: u32,
        kind: TrackKind,
        label: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
)]
pub(crate) struct NativePublicationPreview {
    publications: Vec<NativePublication>,
}

#[allow(
    dead_code,
    reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
)]
pub(crate) enum NativePublicationEvent {
    Publish(crate::track::Track),
    Unpublish(TrackId),
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
        media_index: u32,
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
            media_index,
            mid,
            track,
            descriptor,
            in_topology: false,
            native_binding: None,
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
    fn track_for_sender_index(&self, media_index: u32) -> Option<TrackId> {
        self.published_tracks
            .iter()
            .find(|slot| slot.media_index == media_index)
            .map(|slot| slot.track.meta.id)
    }
    fn handle_incoming_rtp(
        &mut self,
        index: usize,
        mid: Mid,
        rid: Option<&str0m::media::Rid>,
        rtp: RtpPacket,
        sr: Option<SenderInfo>,
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
        rtp.ext_vals.rid = rid.cloned();
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
    track_handles: HashMap<TrackId, TrackHandle>,
}
pub type UpstreamAllocator = Upstream;

impl Upstream {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            audio: UpstreamAudio::new(ctx),
            video: UpstreamVideo::new(ctx),
            data: UpstreamData::new(),
            routes: UpstreamRouteTable::default(),
            track_handles: HashMap::new(),
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
        rid: Option<&str0m::media::Rid>,
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

    pub(crate) fn track_fanout(&self, track_id: TrackId) -> Option<TrackHandle> {
        self.track_handles.get(&track_id).copied()
    }
    pub(crate) fn track_for_fanout(&self, fanout: TrackHandle) -> Option<TrackId> {
        self.track_handles
            .iter()
            .find_map(|(track_id, key)| (*key == fanout).then_some(*track_id))
    }
    pub(crate) fn bind_track_handle(&mut self, track_id: TrackId, key: TrackHandle) {
        self.track_handles.insert(track_id, key);
        self.routes.bind_fanout(track_id, key);
    }
    pub(crate) fn unbind_track_handle(&mut self, track_id: TrackId, key: TrackHandle) {
        if self.track_handles.get(&track_id) == Some(&key) {
            self.track_handles.remove(&track_id);
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

    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    pub(crate) fn preview_native_publications(
        &self,
        publications: &[NativePublication],
    ) -> Result<NativePublicationPreview, NativePublicationError> {
        let mut accepted = Vec::new();
        let mut sender_indices = std::collections::HashSet::new();
        let mut identities = std::collections::HashSet::new();

        for publication in publications {
            let Some(sender_index) = publication.sender_index else {
                continue;
            };
            let Some(kind @ (TrackKind::Audio | TrackKind::Video)) = publication.kind else {
                continue;
            };
            if publication.label.is_empty() || publication.label.len() > 64 {
                continue;
            }
            let Some(slot) = self.native_slot(sender_index) else {
                continue;
            };
            if slot.track.meta.id.kind() != kind {
                continue;
            }
            let identity = (kind, publication.label.as_str());
            if !sender_indices.insert(sender_index) || !identities.insert(identity) {
                continue;
            }

            if let Some(binding) = &slot.native_binding
                && (binding.kind != kind || binding.label != publication.label)
            {
                return Err(NativePublicationError::Relabel { sender_index });
            }
            if self
                .native_binding_for(kind, &publication.label)
                .is_some_and(|index| index != sender_index)
            {
                return Err(NativePublicationError::Move {
                    sender_index,
                    kind,
                    label: publication.label.clone(),
                });
            }
            accepted.push(publication.clone());
        }

        Ok(NativePublicationPreview {
            publications: accepted,
        })
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    pub(crate) fn commit_native_publications(
        &mut self,
        preview: NativePublicationPreview,
    ) -> Vec<NativePublicationEvent> {
        let active: std::collections::HashSet<_> = preview
            .publications
            .iter()
            .filter_map(|publication| publication.sender_index)
            .collect();
        let mut events = Vec::new();
        let mut bound = false;

        for slot in self
            .audio
            .media
            .published_tracks
            .iter_mut()
            .chain(self.video.media.published_tracks.iter_mut())
        {
            if slot.in_topology && !active.contains(&slot.media_index) {
                slot.in_topology = false;
                events.push(NativePublicationEvent::Unpublish(slot.descriptor.id()));
            }
        }

        for publication in preview.publications {
            let sender_index = publication
                .sender_index
                .expect("previewed publication has sender");
            let kind = publication
                .kind
                .expect("previewed publication has media kind");
            let slot = self
                .native_slot_mut(sender_index, kind)
                .expect("previewed publication resolves to native slot");
            if slot.native_binding.is_none() {
                let meta = crate::track::TrackMeta::labeled_media(
                    slot.track.meta.room_id,
                    slot.track.meta.shard_id,
                    slot.track.meta.origin,
                    kind,
                    publication.label.clone(),
                );
                slot.track.meta = meta.clone();
                slot.descriptor.replace_meta(meta);
                slot.native_binding = Some(NativeSenderBinding {
                    kind,
                    label: publication.label,
                });
                bound = true;
            }
            if !slot.in_topology {
                slot.in_topology = true;
                events.push(NativePublicationEvent::Publish(slot.descriptor.clone()));
            }
        }
        if bound {
            self.routes.clear();
        }
        events
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    fn native_slot(&self, sender_index: u32) -> Option<&UpstreamSlot> {
        self.audio
            .media
            .published_tracks
            .iter()
            .chain(self.video.media.published_tracks.iter())
            .find(|slot| slot.media_index == sender_index)
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    fn native_slot_mut(&mut self, sender_index: u32, kind: TrackKind) -> Option<&mut UpstreamSlot> {
        let media = match kind {
            TrackKind::Audio => &mut self.audio.media,
            TrackKind::Video => &mut self.video.media,
            TrackKind::Data => return None,
        };
        media
            .published_tracks
            .iter_mut()
            .find(|slot| slot.media_index == sender_index)
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling transaction consumes native sender bindings in Plan 07"
    )]
    fn native_binding_for(&self, kind: TrackKind, label: &str) -> Option<u32> {
        self.audio
            .media
            .published_tracks
            .iter()
            .chain(self.video.media.published_tracks.iter())
            .find(|slot| {
                slot.native_binding
                    .as_ref()
                    .is_some_and(|binding| binding.kind == kind && binding.label == label)
            })
            .map(|slot| slot.media_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{ParticipantId, RoomExternalId, RoomId, TrackKind};
    use crate::track::{self, TrackMeta};

    fn upstream_with_slots() -> (Upstream, ParticipantId, RoomId) {
        let participant_id = ParticipantId::new();
        let room_id = RoomId::from_external(&RoomExternalId::new("test").unwrap());
        let mut upstream = Upstream::new(LogCtx {
            room_id,
            participant_id,
        });
        for (index, kind) in [
            (0, TrackKind::Audio),
            (1, TrackKind::Video),
            (2, TrackKind::Audio),
        ] {
            let mid = Mid::from(format!("{kind:?}-{index}").as_str());
            let meta = TrackMeta {
                room_id,
                shard_id: crate::id::ShardId::from(0),
                id: participant_id.derive_track_id(kind, &mid),
                origin: participant_id,
                label: None,
            };
            let (sender, descriptor) = match kind {
                TrackKind::Audio => track::new_audio(mid, meta),
                TrackKind::Video => track::new_video(mid, meta, Vec::new()),
                TrackKind::Data => unreachable!(),
            };
            assert!(upstream.add_published_track(index, mid, sender, descriptor));
        }
        (upstream, participant_id, room_id)
    }

    fn publication(sender_index: u32, kind: TrackKind, label: &str) -> NativePublication {
        NativePublication {
            sender_index: Some(sender_index),
            kind: Some(kind),
            label: label.to_owned(),
        }
    }

    #[test]
    fn sender_coordinates_round_trip_at_32_slots() {
        let participant_id = ParticipantId::new();
        let room_id = RoomId::from_external(&RoomExternalId::new("test").unwrap());
        let mut upstream = Upstream::new(LogCtx {
            room_id,
            participant_id,
        });

        let mut last = None;
        for media_index in 100..132 {
            let mid = Mid::from(format!("audio-{media_index}").as_str());
            let track_id = participant_id.derive_track_id(TrackKind::Audio, &mid);
            let (sender, descriptor) = track::new_audio(
                mid,
                TrackMeta {
                    room_id,
                    shard_id: crate::id::ShardId::from(0),
                    id: track_id,
                    origin: participant_id,
                    label: None,
                },
            );
            assert!(upstream.add_published_track(media_index, mid, sender, descriptor));
            last = Some((media_index, track_id));
        }

        let (media_index, track_id) = last.unwrap();
        assert_eq!(upstream.track_for_sender_index(media_index), Some(track_id));
    }

    #[test]
    fn native_sender_bindings_first_commit_derives_labeled_track_id() {
        let participant_id = ParticipantId::new();
        let room_id = RoomId::from_external(&RoomExternalId::new("test").unwrap());
        let mut upstream = Upstream::new(LogCtx {
            room_id,
            participant_id,
        });
        let mid = Mid::from("audio-0");
        let initial_id = participant_id.derive_track_id(TrackKind::Audio, &mid);
        let (sender, descriptor) = track::new_audio(
            mid,
            TrackMeta {
                room_id,
                shard_id: crate::id::ShardId::from(0),
                id: initial_id,
                origin: participant_id,
                label: None,
            },
        );
        assert!(upstream.add_published_track(0, mid, sender, descriptor));

        let preview = upstream
            .preview_native_publications(&[crate::participant::intent::NativePublication {
                sender_index: Some(0),
                kind: Some(TrackKind::Audio),
                label: "microphone".to_owned(),
            }])
            .unwrap();
        assert_eq!(upstream.track_for_sender_index(0), Some(initial_id));

        let events = upstream.commit_native_publications(preview);
        let track_id = participant_id.derive_track_id(TrackKind::Audio, "microphone");
        assert_eq!(upstream.track_for_sender_index(0), Some(track_id));
        assert!(
            matches!(events.as_slice(), [NativePublicationEvent::Publish(track)] if track.id() == track_id && track.meta().label.as_deref() == Some("microphone"))
        );
    }

    #[test]
    fn native_sender_bindings_republish_after_omission_keeps_track_id() {
        let (mut upstream, participant_id, _) = upstream_with_slots();
        let first = upstream
            .preview_native_publications(&[publication(0, TrackKind::Audio, "mic")])
            .unwrap();
        upstream.commit_native_publications(first);
        let id = participant_id.derive_track_id(TrackKind::Audio, "mic");

        assert!(
            matches!(upstream.commit_native_publications(upstream.preview_native_publications(&[]).unwrap()).as_slice(), [NativePublicationEvent::Unpublish(track_id)] if *track_id == id)
        );
        assert!(
            matches!(upstream.commit_native_publications(upstream.preview_native_publications(&[publication(0, TrackKind::Audio, "mic")]).unwrap()).as_slice(), [NativePublicationEvent::Publish(track)] if track.id() == id)
        );
        assert_eq!(upstream.track_for_sender_index(0), Some(id));
    }

    #[test]
    fn native_sender_bindings_allow_same_label_across_kinds_and_64_bytes() {
        let (mut upstream, participant_id, _) = upstream_with_slots();
        let label = "x".repeat(64);
        let preview = upstream
            .preview_native_publications(&[
                publication(0, TrackKind::Audio, &label),
                publication(1, TrackKind::Video, &label),
            ])
            .unwrap();
        assert_eq!(upstream.commit_native_publications(preview).len(), 2);
        assert_eq!(
            upstream.track_for_sender_index(0),
            Some(participant_id.derive_track_id(TrackKind::Audio, &label))
        );
        assert_eq!(
            upstream.track_for_sender_index(1),
            Some(participant_id.derive_track_id(TrackKind::Video, &label))
        );

        let too_long = publication(0, TrackKind::Audio, &"x".repeat(65));
        assert!(
            upstream
                .preview_native_publications(&[too_long])
                .unwrap()
                .publications
                .is_empty()
        );
    }

    #[test]
    fn native_sender_bindings_skip_invalid_and_later_candidate_duplicates() {
        let (upstream, _, _) = upstream_with_slots();
        let preview = upstream
            .preview_native_publications(&[
                NativePublication {
                    sender_index: None,
                    kind: Some(TrackKind::Audio),
                    label: "missing".to_owned(),
                },
                publication(0, TrackKind::Video, "wrong-kind"),
                publication(0, TrackKind::Audio, "first"),
                publication(0, TrackKind::Audio, "later-index"),
                publication(1, TrackKind::Video, "first"),
                publication(2, TrackKind::Audio, "first"),
                NativePublication {
                    sender_index: Some(1),
                    kind: Some(TrackKind::Data),
                    label: "data".to_owned(),
                },
            ])
            .unwrap();
        assert_eq!(
            preview.publications,
            vec![
                publication(0, TrackKind::Audio, "first"),
                publication(1, TrackKind::Video, "first"),
            ]
        );
    }

    #[test]
    fn native_sender_bindings_relabel_and_move_are_atomic_after_unpublish() {
        let (mut upstream, _, _) = upstream_with_slots();
        let preview = upstream
            .preview_native_publications(&[publication(0, TrackKind::Audio, "mic")])
            .unwrap();
        upstream.commit_native_publications(preview);
        upstream.commit_native_publications(upstream.preview_native_publications(&[]).unwrap());
        let bindings_before: Vec<_> = upstream
            .audio
            .media
            .published_tracks
            .iter()
            .chain(upstream.video.media.published_tracks.iter())
            .map(|slot| {
                (
                    slot.media_index,
                    slot.native_binding.clone(),
                    slot.in_topology,
                )
            })
            .collect();
        let routes_before = upstream.routes.routes.len();

        assert_eq!(
            upstream.preview_native_publications(&[publication(0, TrackKind::Audio, "other")]),
            Err(NativePublicationError::Relabel { sender_index: 0 })
        );
        assert_eq!(
            upstream.preview_native_publications(&[publication(2, TrackKind::Audio, "mic")]),
            Err(NativePublicationError::Move {
                sender_index: 2,
                kind: TrackKind::Audio,
                label: "mic".to_owned()
            })
        );
        assert_eq!(
            bindings_before,
            upstream
                .audio
                .media
                .published_tracks
                .iter()
                .chain(upstream.video.media.published_tracks.iter())
                .map(|slot| (
                    slot.media_index,
                    slot.native_binding.clone(),
                    slot.in_topology
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(routes_before, upstream.routes.routes.len());
    }

    #[test]
    fn native_sender_bindings_clear_routes_and_are_idempotent() {
        let (mut upstream, _, _) = upstream_with_slots();
        upstream.cache_route(IncomingRtpRoute {
            ssrc: Ssrc::from(7),
            mid: Mid::from("Audio-0"),
            rid: None,
            upstream_slot: UpstreamSlotKey::Audio(0),
            track_id: upstream.track_for_sender_index(0).unwrap(),
            fanout: None,
        });
        let input = [publication(0, TrackKind::Audio, "mic")];
        assert_eq!(
            upstream
                .commit_native_publications(upstream.preview_native_publications(&input).unwrap())
                .len(),
            1
        );
        assert!(upstream.routes.routes.is_empty());
        assert!(
            upstream
                .commit_native_publications(upstream.preview_native_publications(&input).unwrap())
                .is_empty()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamSlotKey {
    Audio(usize),
    Video(usize),
}
