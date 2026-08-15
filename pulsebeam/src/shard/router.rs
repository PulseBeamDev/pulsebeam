use bytes::Bytes;
use slotmap::SecondaryMap;
use str0m::channel::ChannelId;
use str0m::media::Rid;

use crate::audio_selector::TopNAudioSelector;
use crate::clock::WallAnchor;
use crate::entity::{AudioOrigin, ParticipantId, TrackId};
use crate::id::{AudioSelectorSlotId, ShardId};
use crate::keys::{DownstreamSlotKey, ParticipantKey};
use crate::route::{Envelope, RouteAction, RouteHandle, RouteRuntime};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use crate::track::Topic;

use super::events::AudioRtpEvent;
use super::worker::{MediaPayload, Reverse, ShardFrame};

pub(crate) use crate::keys::{DataStreamKey, ReliableStreamKey, TrackKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DataStreamId {
    pub room_id: crate::entity::RoomId,
    pub publisher_id: ParticipantId,
    pub topic: Topic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeStreamKey {
    Data(DataStreamKey),
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
        slot_idx: AudioSelectorSlotId,
        origin: AudioOrigin,
        pkt: &RtpPacket,
    );
    fn forward_sctp(&mut self, subscriber: ParticipantKey, channel: ChannelId, pkt: &[u8]);
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

struct StreamRuntime {
    link_seq: u32,
}

pub(crate) struct ReliableRuntime {
    id: DataStreamId,
    publisher: ParticipantKey,
    link_seq: u32,
}

pub(crate) struct ShardRuntime {
    tracks: SecondaryMap<TrackKey, TrackRuntime>,
    data: SecondaryMap<DataStreamKey, StreamRuntime>,
    reliable: SecondaryMap<ReliableStreamKey, ReliableRuntime>,
    audio_selector: TopNAudioSelector,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId, rng: &mut impl pulsebeam_runtime::rand::RngCore) -> Self {
        Self {
            tracks: SecondaryMap::new(),
            data: SecondaryMap::new(),
            reliable: SecondaryMap::new(),
            audio_selector: TopNAudioSelector::new(rng),
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn retire_track(&mut self, key: TrackKey) {
        let _ = self.tracks.remove(key);
    }

    pub(crate) fn track_publication(&self, key: TrackKey) -> Option<&crate::track::Track> {
        self.tracks.get(key).map(|track| &track.publication)
    }

    pub(crate) fn retire_data_stream(&mut self, key: DataStreamKey) {
        let _ = self.data.remove(key);
    }

    pub(crate) fn retire_reliable_stream(&mut self, key: ReliableStreamKey) {
        let _ = self.reliable.remove(key);
    }

    pub(crate) fn apply_view_op(&mut self, op: &crate::view::ViewOp) {
        match op {
            crate::view::ViewOp::RetireRoute { route, epoch } => {
                let retired = self.routes.retire(RouteHandle::new(*route, *epoch));
                debug_assert!(
                    retired
                        || self
                            .routes
                            .entry(RouteHandle::new(*route, *epoch))
                            .is_none()
                );
            }
            crate::view::ViewOp::InsertTrackRuntime { key, descriptor } => {
                let previous = self.tracks.insert(
                    *key,
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
                    debug_assert_eq!(
                        previous.id, descriptor.id,
                        "a runtime key cannot change its logical track"
                    );
                    if previous.id == descriptor.id {
                        let Some(current) = self.tracks.get_mut(*key) else {
                            debug_assert!(false, "inserted track runtime must remain addressable");
                            return;
                        };
                        current.cache = previous.cache;
                        current.link_seq = previous.link_seq;
                    }
                }
            }
            crate::view::ViewOp::RemoveTrackRuntime { key } => self.retire_track(*key),
            crate::view::ViewOp::InsertDataRuntime { key, .. } => {
                let _ = self.data.insert(*key, StreamRuntime { link_seq: 0 });
            }
            crate::view::ViewOp::RemoveDataRuntime { key } => self.retire_data_stream(*key),
            crate::view::ViewOp::InsertReliableRuntime { key, id, publisher } => {
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
            | crate::view::ViewOp::SetTrackPlan { .. }
            | crate::view::ViewOp::RemoveTrackPlan { .. }
            | crate::view::ViewOp::SetAudioPlan { .. }
            | crate::view::ViewOp::RemoveAudioPlan { .. }
            | crate::view::ViewOp::SetDataPlan { .. }
            | crate::view::ViewOp::RemoveDataPlan { .. }
            | crate::view::ViewOp::SetReliablePlan { .. }
            | crate::view::ViewOp::RemoveReliablePlan { .. } => {}
        }
    }

    pub(crate) fn track_descriptor(&self, key: TrackKey) -> Option<(TrackId, &[Option<Rid>])> {
        self.tracks
            .get(key)
            .map(|track| (track.id, track.encodings.as_slice()))
    }

    pub(crate) fn track_origin(&self, key: TrackKey) -> Option<ParticipantKey> {
        self.tracks.get(key).map(|track| track.origin_key)
    }

    #[inline]
    pub fn route_video_with_plan(
        &mut self,
        fanout: TrackKey,
        pkt: RtpPacket,
        plan: &crate::view::TrackForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        let track_id = self.tracks.get(fanout).map(|track| track.id);
        let Some(track) = self.tracks.get_mut(fanout) else {
            debug_assert!(false, "compiled video key must resolve to runtime state");
            return;
        };
        debug_assert_eq!(track_id, Some(plan.track_id));
        let rid = pkt.ext_vals.rid;
        let seq = pkt.seq_no;
        let cache = track.cache.get_or_insert_with(TrackStreamCache::new);
        let too_old = cache.push(pkt);
        let Some(packet) = too_old
            .as_ref()
            .or_else(|| cache.encoding(rid).and_then(|stream| stream.get(seq)))
        else {
            debug_assert!(false, "a cached packet must be readable");
            return;
        };
        for &(subscriber, slot) in &plan.local_subscribers {
            ctx.forward_video_rtp(subscriber, slot, packet, Some(cache));
        }
        let playout = ctx.wall().to_ntp(packet.playout_time);
        let link_seq = &mut track.link_seq;
        for remote in &plan.remote_routes {
            let env = Envelope::media(
                RouteHandle::new(remote.route, remote.epoch),
                *link_seq,
                playout.middle32(),
            );
            *link_seq = link_seq.wrapping_add(1);
            ctx.send_media(
                remote.shard_id,
                env,
                MediaPayload::Video(Box::new(packet.to_transit())),
            );
        }
    }

    pub fn route_audio_with_plan(
        &mut self,
        track: TrackKey,
        origin: Origin,
        mut event: AudioRtpEvent,
        plan: &crate::view::AudioForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert_eq!(plan.track_id, event.stream_id.0);
        debug_assert!(origin.is_local() || event.origin_key.is_none());
        let Some(slot_idx) = self
            .audio_selector
            .filter((track, event.stream_id.1), &mut event.pkt)
        else {
            return;
        };
        for &subscriber in &plan.local_subscribers {
            if Some(subscriber) == event.origin_key {
                continue;
            }
            ctx.forward_audio_rtp(
                subscriber,
                slot_idx,
                AudioOrigin {
                    participant: event.origin,
                    track: event.stream_id.0,
                },
                &event.pkt,
            );
        }
        if origin.is_local() {
            let playout = ctx.wall().to_ntp(event.pkt.playout_time);
            let Some(runtime) = self.tracks.get_mut(track) else {
                debug_assert!(false, "audio key must resolve to runtime state");
                return;
            };
            for remote in &plan.remote_routes {
                let env = Envelope::media(
                    RouteHandle::new(remote.route, remote.epoch),
                    runtime.link_seq,
                    playout.middle32(),
                );
                runtime.link_seq = runtime.link_seq.wrapping_add(1);
                ctx.send_media(
                    remote.shard_id,
                    env,
                    MediaPayload::Audio(Box::new(event.pkt.to_transit())),
                );
            }
        }
    }

    pub fn route_data_with_plan(
        &mut self,
        stream: DataStreamKey,
        origin: Origin,
        packet: Bytes,
        plan: &crate::view::StreamForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(runtime) = self.data.get_mut(stream) else {
            debug_assert!(false, "data key must resolve to runtime state");
            return;
        };
        for &(subscriber, channel) in &plan.local_subscribers {
            ctx.forward_sctp(subscriber, channel, &packet);
        }
        if origin.is_local() {
            let playout = ctx.wall().ntp();
            for remote in &plan.remote_routes {
                let env = Envelope::media(
                    RouteHandle::new(remote.route, remote.epoch),
                    runtime.link_seq,
                    playout.middle32(),
                );
                runtime.link_seq = runtime.link_seq.wrapping_add(1);
                ctx.send_media(remote.shard_id, env, MediaPayload::Data(packet.clone()));
            }
        }
    }

    pub fn route_reliable_data_with_plan(
        &mut self,
        stream: ReliableStreamKey,
        origin: Origin,
        frame: Bytes,
        plan: &crate::view::StreamForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(!frame.is_empty());
        let Some(_runtime) = self.reliable.get(stream) else {
            debug_assert!(false, "reliable key must resolve to runtime state");
            return;
        };
        for &(subscriber, channel) in &plan.local_subscribers {
            ctx.forward_reliable_sctp(subscriber, channel, &frame);
        }
        if origin.is_local() {
            let playout = ctx.wall().ntp();
            let Some(runtime) = self.reliable.get_mut(stream) else {
                debug_assert!(false, "reliable key must remain live during dispatch");
                return;
            };
            for remote in &plan.remote_routes {
                let env = Envelope::media(
                    RouteHandle::new(remote.route, remote.epoch),
                    runtime.link_seq,
                    playout.middle32(),
                );
                runtime.link_seq = runtime.link_seq.wrapping_add(1);
                ctx.send_media(remote.shard_id, env, MediaPayload::Data(frame.clone()));
            }
        }
    }

    pub fn route_reliable_control(
        &self,
        bytes: Bytes,
        plan: &crate::view::StreamForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(target) = plan.reverse_route else {
            debug_assert!(false, "reliable control has no compiled reverse route");
            return;
        };
        ctx.send_frame(
            target.shard_id,
            ShardFrame::Reverse {
                env: Envelope::feedback(RouteHandle::new(target.route, target.epoch)),
                body: Reverse::DataAck(bytes),
            },
        );
    }

    pub fn apply_stats(
        &mut self,
        fanout: TrackKey,
        stats: crate::track::TrackStates,
        plan: &crate::view::TrackForwardingPlan,
        ctx: &mut impl RoutingContext,
    ) {
        let track_id = self.tracks.get(fanout).map(|track| track.id);
        let Some(track) = self.tracks.get_mut(fanout) else {
            debug_assert!(false, "stats key must resolve to runtime state");
            return;
        };
        debug_assert_eq!(track_id, Some(plan.track_id));
        track.layer_states = stats;
        let states = &track.layer_states;
        for &(subscriber, slot) in &plan.local_subscribers {
            ctx.update_layer_states(subscriber, slot, states);
        }
        for remote in &plan.remote_routes {
            ctx.send_frame(
                remote.shard_id,
                ShardFrame::Telemetry {
                    env: Envelope::telemetry(RouteHandle::new(remote.route, remote.epoch)),
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
            crate::route::ReverseTarget::Track { track } => self.track_origin(track),
            crate::route::ReverseTarget::Topic { stream } => {
                self.reliable.get(stream).map(|runtime| runtime.publisher)
            }
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
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0), &mut rng);
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantKey, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let track_id =
            ParticipantId::from_bytes([7; 16]).derive_track_id(TrackKind::Video, "track");

        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key,
            descriptor: descriptor(track_id, origin_key, "q"),
        });
        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key,
            descriptor: descriptor(track_id, origin_key, "f"),
        });

        assert_eq!(
            runtime.track_descriptor(key).unwrap().1,
            &[Some(Rid::from("f"))]
        );
    }
}
