use slotmap::SecondaryMap;
use str0m::channel::ChannelId;
use str0m::media::Rid;

use crate::clock::WallAnchor;
use crate::entity::{AudioOrigin, ParticipantId, TrackId};
use crate::id::ShardId;
use crate::keys::{AudioTrackKey, DownstreamSlotKey, ParticipantKey, VideoTrackKey};
use crate::route::{Envelope, RouteAction, RouteRuntime};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use crate::track::Topic;

use super::events::AudioRtpEvent;
use super::worker::{MediaPayload, Reverse, ShardFrame};

pub(crate) use crate::keys::{ReliableStreamKey, TrackKey, UnreliableStreamKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DataStreamId {
    pub room_id: crate::entity::RoomId,
    pub publisher_id: ParticipantId,
    pub topic: Topic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeStreamKey {
    Unreliable(UnreliableStreamKey),
    Reliable(ReliableStreamKey),
}

impl DataStreamId {
    pub fn new(room_id: crate::entity::RoomId, publisher_id: ParticipantId, topic: Topic) -> Self {
        Self {
            room_id,
            publisher_id,
            topic,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
    Remote,
}

impl Origin {
    fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }
}

pub(crate) trait ShardTransport {
    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload);
    fn send_frame(&self, dst: ShardId, frame: ShardFrame);
}

pub(crate) trait RoutingContext: ShardTransport {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantKey,
        slot: DownstreamSlotKey,
        track: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
    );
    fn update_layer_states(
        &mut self,
        subscriber: ParticipantKey,
        slot: DownstreamSlotKey,
        states: &crate::track::TrackStates,
    );
    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantKey,
        origin: AudioOrigin,
        pkt: &RtpPacket,
    );
    fn forward_unreliable_sctp(
        &mut self,
        subscriber: ParticipantKey,
        channel: ChannelId,
        pkt: &[u8],
    );
    fn wall(&self) -> &WallAnchor;
    fn forward_reliable_sctp(
        &mut self,
        subscriber: ParticipantKey,
        channel: ChannelId,
        frame: &[u8],
    );
}

struct TrackRuntime {
    id: TrackId,
    origin_key: ParticipantKey,
    publication: crate::track::Track,
    encodings: Vec<Option<Rid>>,
    layer_states: crate::track::TrackStates,
    cache: Option<TrackStreamCache>,
    link_seq: u32,
}

struct UnreliableRuntime {
    id: DataStreamId,
    publisher: Option<ParticipantKey>,
    link_seq: u32,
}

/// The same stream, plus what the feedback path needs to address its publisher.
pub(crate) struct ReliableRuntime {
    id: DataStreamId,
    publisher: Option<ParticipantKey>,
    link_seq: u32,
}

/// Hand a packet to every local member of the audiences a plan names.
///
/// The only thing that varies per kind is `deliver`, and the delivery key it
/// receives — a downstream slot, an SCTP channel, or nothing at all.
fn fanout_local<D: Copy, G>(
    plan: &crate::view::ForwardingPlan<G>,
    groups: &crate::view::GroupImage<D, G>,
    mut deliver: impl FnMut(ParticipantKey, D),
) {
    for group in &plan.groups {
        for (subscriber, delivery) in groups.members(*group) {
            deliver(subscriber, delivery);
        }
    }
}

/// Forward a packet to the shards a plan routes to, numbering each hop so the
/// destination can tell loss from reordering.
fn fanout_remote<G>(
    plan: &crate::view::ForwardingPlan<G>,
    link_seq: &mut u32,
    playout: u32,
    mut payload: impl FnMut() -> MediaPayload,
    ctx: &mut impl ShardTransport,
) {
    for remote in &plan.remote_routes {
        let env = Envelope::media(remote.handle, *link_seq, playout);
        *link_seq = link_seq.wrapping_add(1);
        ctx.send_media(remote.handle.shard(), env, payload());
    }
}

pub(crate) struct ShardRuntime {
    tracks: SecondaryMap<TrackKey, TrackRuntime>,
    unreliable: SecondaryMap<UnreliableStreamKey, UnreliableRuntime>,
    reliable: SecondaryMap<ReliableStreamKey, ReliableRuntime>,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            tracks: SecondaryMap::new(),
            unreliable: SecondaryMap::new(),
            reliable: SecondaryMap::new(),
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn retire_track(&mut self, key: TrackKey) {
        let removed = self.tracks.remove(key);
        debug_assert!(removed.is_some(), "a track runtime must retire once");
    }

    pub(crate) fn track_publication(&self, key: TrackKey) -> Option<&crate::track::Track> {
        self.tracks.get(key).map(|track| &track.publication)
    }

    pub(crate) fn retire_data_stream(&mut self, key: UnreliableStreamKey) {
        let removed = self.unreliable.remove(key);
        debug_assert!(removed.is_some(), "an unreliable runtime must retire once");
    }

    pub(crate) fn retire_reliable_stream(&mut self, key: ReliableStreamKey) {
        let removed = self.reliable.remove(key);
        debug_assert!(removed.is_some(), "a reliable runtime must retire once");
    }

    pub(crate) fn apply_view_op(&mut self, op: &crate::view::ViewOp) {
        match op {
            crate::view::ViewOp::RetireRoute { handle } => {
                let retired = self.routes.retire(*handle);
                debug_assert!(retired || self.routes.entry(*handle).is_none());
            }
            crate::view::ViewOp::InsertTrackRuntime { key, descriptor } => {
                if !matches!(
                    (key, descriptor.id.kind()),
                    (
                        crate::keys::TrackRuntimeKey::Video(_),
                        crate::entity::TrackKind::Video
                    ) | (
                        crate::keys::TrackRuntimeKey::Audio(_),
                        crate::entity::TrackKind::Audio
                    )
                ) {
                    debug_assert!(false, "a track runtime key must match its publication kind");
                    return;
                }
                let key = key.raw();
                if let Some(previous) = self.tracks.get(key) {
                    debug_assert_eq!(
                        previous.id, descriptor.id,
                        "a runtime key cannot change its logical track"
                    );
                    debug_assert_eq!(
                        previous.origin_key, descriptor.origin_key,
                        "a runtime key cannot change its publisher binding"
                    );
                    if previous.id != descriptor.id {
                        return;
                    }
                }
                let previous = self.tracks.insert(
                    key,
                    TrackRuntime {
                        id: descriptor.id,
                        origin_key: descriptor.origin_key,
                        publication: descriptor.publication.clone(),
                        encodings: descriptor.encodings.clone(),
                        layer_states: descriptor.states.clone(),
                        cache: None,
                        link_seq: 0,
                    },
                );
                if let Some(previous) = previous {
                    let Some(current) = self.tracks.get_mut(key) else {
                        debug_assert!(false, "inserted track runtime must remain addressable");
                        return;
                    };
                    current.cache = previous.cache;
                    current.link_seq = previous.link_seq;
                }
            }
            crate::view::ViewOp::RemoveTrackRuntime { key } => self.retire_track(key.raw()),
            crate::view::ViewOp::InsertUnreliableRuntime { key, id, publisher } => {
                if let Some(previous) = self.unreliable.get(*key) {
                    debug_assert_eq!(
                        previous.id, *id,
                        "an unreliable runtime key cannot change its logical stream"
                    );
                    debug_assert_eq!(
                        previous.publisher, *publisher,
                        "an unreliable runtime key cannot change its publisher"
                    );
                    return;
                }
                let _ = self.unreliable.insert(
                    *key,
                    UnreliableRuntime {
                        id: id.clone(),
                        publisher: *publisher,
                        link_seq: 0,
                    },
                );
            }
            crate::view::ViewOp::RemoveUnreliableRuntime { key } => self.retire_data_stream(*key),
            crate::view::ViewOp::InsertReliableRuntime { key, id, publisher } => {
                if let Some(previous) = self.reliable.get(*key) {
                    debug_assert_eq!(
                        previous.id, *id,
                        "a reliable runtime key cannot change its logical stream"
                    );
                    debug_assert_eq!(
                        previous.publisher, *publisher,
                        "a reliable runtime key cannot change its publisher"
                    );
                    return;
                }
                let _ = self.reliable.insert(
                    *key,
                    ReliableRuntime {
                        id: id.clone(),
                        publisher: *publisher,
                        link_seq: 0,
                    },
                );
            }
            crate::view::ViewOp::RemoveReliableRuntime { key } => self.retire_reliable_stream(*key),
            crate::view::ViewOp::InstallRoute { .. }
            | crate::view::ViewOp::InstallTransport { .. }
            | crate::view::ViewOp::RetireTransport { .. }
            | crate::view::ViewOp::InsertParticipant { .. }
            | crate::view::ViewOp::RemoveParticipant { .. }
            | crate::view::ViewOp::SetPlan { .. }
            | crate::view::ViewOp::RemovePlan { .. }
            | crate::view::ViewOp::InsertVideoMember { .. }
            | crate::view::ViewOp::InsertAudioMember { .. }
            | crate::view::ViewOp::InsertDataMember { .. }
            | crate::view::ViewOp::RemoveVideoMember { .. }
            | crate::view::ViewOp::RemoveAudioMember { .. }
            | crate::view::ViewOp::RemoveDataMember { .. }
            | crate::view::ViewOp::BindSubscribedTrack { .. }
            | crate::view::ViewOp::UnbindSubscribedTrack { .. }
            | crate::view::ViewOp::AnnounceTrack { .. }
            | crate::view::ViewOp::WithdrawTrack { .. } => {}
        }
    }

    pub(crate) fn track_descriptor(&self, key: TrackKey) -> Option<(TrackId, &[Option<Rid>])> {
        self.tracks
            .get(key)
            .map(|track| (track.id, track.encodings.as_slice()))
    }

    /// The identity behind a fanout key.
    ///
    /// Forwarding plans carry no identity, so this arena is the only place a
    /// key's track and publisher are recorded.
    pub(crate) fn track_identity(&self, key: TrackKey) -> Option<(TrackId, ParticipantId)> {
        self.tracks
            .get(key)
            .map(|track| (track.id, track.publication.meta.origin))
    }

    #[inline]
    pub fn route_video_with_plan(
        &mut self,
        fanout: VideoTrackKey,
        pkt: RtpPacket,
        plan: &crate::view::VideoPlan,
        groups: &crate::view::GroupImage<DownstreamSlotKey, crate::view::VideoAudience>,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(track) = self.tracks.get_mut(fanout.raw()) else {
            debug_assert!(false, "compiled video key must resolve to runtime state");
            return;
        };
        if track.id.kind() != crate::entity::TrackKind::Video {
            debug_assert!(false, "a video key must resolve to a video publication");
            return;
        }
        let rid = pkt.ext_vals.rid;
        let seq = pkt.seq_no;
        let track_id = track.id;
        let cache = track.cache.get_or_insert_with(TrackStreamCache::new);
        let too_old = cache.push(pkt);
        let Some(packet) = too_old
            .as_ref()
            .or_else(|| cache.encoding(rid).and_then(|stream| stream.get(seq)))
        else {
            debug_assert!(false, "a cached packet must be readable");
            return;
        };
        fanout_local(plan, groups, |subscriber, slot| {
            ctx.forward_video_rtp(subscriber, slot, track_id, packet, Some(cache));
        });
        let playout = ctx.wall().to_ntp(packet.playout_time);
        fanout_remote(
            plan,
            &mut track.link_seq,
            playout.middle32(),
            || MediaPayload::Video(Box::new(packet.to_transit())),
            ctx,
        );
    }

    pub fn route_audio_with_plan(
        &mut self,
        track: AudioTrackKey,
        origin: Origin,
        event: AudioRtpEvent,
        plan: &crate::view::AudioPlan,
        groups: &crate::view::GroupImage<(), crate::view::AudioAudience>,
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(origin.is_local() || event.origin_key.is_none());
        let Some(runtime) = self.tracks.get(track.raw()) else {
            debug_assert!(false, "compiled audio key must resolve to runtime state");
            return;
        };
        if runtime.id.kind() != crate::entity::TrackKind::Audio {
            debug_assert!(false, "an audio key must resolve to an audio publication");
            return;
        }
        // The plan names audiences; membership lives in the group image, so a
        // room's roster is resolved by array index rather than carried in every
        // audio plan. `origin_key` is only `Some` where the publisher is local,
        // which is the only shard its own key could appear on.
        let audio_origin = AudioOrigin {
            participant: event.origin,
            track: event.stream_id.0,
        };
        fanout_local(plan, groups, |subscriber, ()| {
            if Some(subscriber) == event.origin_key {
                return;
            }
            ctx.forward_audio_rtp(subscriber, audio_origin, &event.pkt);
        });
        if origin.is_local() {
            let playout = ctx.wall().to_ntp(event.pkt.playout_time);
            let Some(runtime) = self.tracks.get_mut(track.raw()) else {
                debug_assert!(false, "audio key must resolve to runtime state");
                return;
            };
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || MediaPayload::Audio(Box::new(event.pkt.to_transit())),
                ctx,
            );
        }
    }

    pub fn route_unreliable_with_plan(
        &mut self,
        stream: UnreliableStreamKey,
        origin: Origin,
        packet: Vec<u8>,
        plan: &crate::view::StreamPlan,
        groups: &crate::view::GroupImage<ChannelId, crate::view::DataAudience>,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(runtime) = self.unreliable.get_mut(stream) else {
            debug_assert!(false, "data key must resolve to runtime state");
            return;
        };
        fanout_local(plan, groups, |subscriber, channel| {
            ctx.forward_unreliable_sctp(subscriber, channel, &packet);
        });
        if origin.is_local() {
            let playout = ctx.wall().ntp();
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || MediaPayload::Data(packet.clone()),
                ctx,
            );
        }
    }

    pub fn route_reliable_with_plan(
        &mut self,
        stream: ReliableStreamKey,
        origin: Origin,
        frame: Vec<u8>,
        plan: &crate::view::StreamPlan,
        groups: &crate::view::GroupImage<ChannelId, crate::view::DataAudience>,
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(!frame.is_empty());
        let Some(_runtime) = self.reliable.get(stream) else {
            debug_assert!(false, "reliable key must resolve to runtime state");
            return;
        };
        fanout_local(plan, groups, |subscriber, channel| {
            ctx.forward_reliable_sctp(subscriber, channel, &frame);
        });
        if origin.is_local() {
            let playout = ctx.wall().ntp();
            let Some(runtime) = self.reliable.get_mut(stream) else {
                debug_assert!(false, "reliable key must remain live during dispatch");
                return;
            };
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || MediaPayload::Data(frame.clone()),
                ctx,
            );
        }
    }

    pub fn route_reliable_control(
        &self,
        bytes: Vec<u8>,
        plan: &crate::view::StreamPlan,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(target) = plan.reverse_route else {
            debug_assert!(false, "reliable control has no compiled reverse route");
            return;
        };
        ctx.send_frame(
            target.handle.shard(),
            ShardFrame::Reverse {
                env: Envelope::feedback(target.handle),
                body: Reverse::DataAck(bytes),
            },
        );
    }

    pub fn apply_stats(
        &mut self,
        fanout: VideoTrackKey,
        stats: crate::track::TrackStates,
        plan: &crate::view::VideoPlan,
        groups: &crate::view::GroupImage<DownstreamSlotKey, crate::view::VideoAudience>,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(track) = self.tracks.get_mut(fanout.raw()) else {
            debug_assert!(false, "stats key must resolve to runtime state");
            return;
        };
        if track.id.kind() != crate::entity::TrackKind::Video {
            debug_assert!(false, "video stats must resolve to a video publication");
            return;
        }
        track.layer_states = stats;
        let states = &track.layer_states;
        fanout_local(plan, groups, |subscriber, slot| {
            ctx.update_layer_states(subscriber, slot, states);
        });
        for remote in &plan.remote_routes {
            ctx.send_frame(
                remote.handle.shard(),
                ShardFrame::Telemetry {
                    env: Envelope::telemetry(remote.handle),
                    stats: track.layer_states.clone(),
                },
            );
        }
    }

    pub(crate) fn reliable_topic(&self, key: ReliableStreamKey) -> Option<&Topic> {
        self.reliable.get(key).map(|runtime| &runtime.id.topic)
    }

    pub fn resolve_reverse(
        &self,
        action: RouteAction,
    ) -> Option<(ParticipantKey, crate::route::ReverseTarget)> {
        let RouteAction::Reverse { target } = action else {
            debug_assert!(false, "reverse frame resolved a non-reverse route");
            return None;
        };
        let origin = match target {
            crate::route::ReverseTarget::Track { track } => {
                let key = track.raw();
                self.tracks
                    .get(key)
                    .filter(|runtime| runtime.id.kind() == crate::entity::TrackKind::Video)
                    .map(|runtime| runtime.origin_key)
            }
            crate::route::ReverseTarget::Topic { stream } => self
                .reliable
                .get(stream)
                .and_then(|runtime| runtime.publisher),
        }?;
        Some((origin, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{ParticipantId, TrackKind};
    use crate::id::ShardId;
    use crate::track::{Track, TrackMeta};
    use slotmap::SlotMap;

    fn descriptor(
        id: TrackId,
        origin_key: ParticipantKey,
        rid: &'static str,
    ) -> crate::view::TrackDescriptor {
        crate::view::TrackDescriptor {
            id,
            origin_key,
            participant: None,
            encodings: vec![Some(Rid::from(rid))],
            states: Vec::new(),
            publication: Track {
                meta: TrackMeta {
                    room_id: crate::entity::RoomId::from_external(
                        &crate::entity::ExternalRoomId::new("test-room").unwrap(),
                    ),
                    shard_id: ShardId::new(0),
                    id,
                    origin: ParticipantId::from_bytes([7; 16]),
                },
                layers: Vec::new(),
                reverse: None,
            },
        }
    }

    #[test]
    fn reinserting_a_live_track_runtime_replaces_its_encodings() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantKey, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let track_id =
            ParticipantId::from_bytes([7; 16]).derive_track_id(TrackKind::Video, "track");

        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key: crate::keys::TrackRuntimeKey::Video(VideoTrackKey::new(key)),
            descriptor: descriptor(track_id, origin_key, "q"),
        });
        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key: crate::keys::TrackRuntimeKey::Video(VideoTrackKey::new(key)),
            descriptor: descriptor(track_id, origin_key, "f"),
        });

        assert_eq!(
            runtime.track_descriptor(key).unwrap().1,
            &[Some(Rid::from("f"))]
        );
    }

    #[test]
    fn reinserting_a_live_data_runtime_preserves_hop_state() {
        let room = crate::entity::RoomId::from_external(
            &crate::entity::ExternalRoomId::new("test-room").unwrap(),
        );
        let stream = DataStreamId::new(
            room,
            ParticipantId::from_bytes([8; 16]),
            Topic::for_test("topic"),
        );
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut keys = SlotMap::<ReliableStreamKey, ()>::with_key();
        let key = keys.insert(());
        let op = crate::view::ViewOp::InsertReliableRuntime {
            key,
            id: stream,
            publisher: None,
        };

        runtime.apply_view_op(&op);
        runtime.reliable.get_mut(key).unwrap().link_seq = 17;
        runtime.apply_view_op(&op);

        assert_eq!(runtime.reliable.get(key).unwrap().link_seq, 17);
    }
}
