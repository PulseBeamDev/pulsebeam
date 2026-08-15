use bytes::Bytes;
use pulsebeam_runtime::rand::Rng;
use pulsebeam_runtime::{
    mailbox,
    net::{self, UnifiedSocket},
};
use tokio::time::Instant;

use crate::clock::WallAnchor;
use crate::route::{Envelope, RouteHandle};
use crate::shard::events::{
    AudioRtpEvent, ClientIntent, ParticipantEvent, ParticipantLifecycleEvent,
    ParticipantSubscriptionEvent,
};
use crate::{
    entity::{TrackId, TrackKind},
    id::AudioSelectorSlotId,
    keys::{DownstreamSlotKey, ParticipantKey},
    participant::{ParticipantConfig, batcher::GsoSendBatch},
    rtp::RtpPacket,
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::ParticipantRegistry,
        router::{Origin, RoutingContext, ShardRuntime},
        timer::TimerWheel,
    },
};
use str0m::media::Rid;

pub(crate) use super::router::ShardTransport;
use super::worker::{MediaPayload, Reverse, ShardCommand, ShardEvent, ShardFrame};

const PARTICIPANT_CAPACITY_HINT: usize = 64;

struct DispatchCtx<'a, R: ShardTransport> {
    registry: &'a mut ParticipantRegistry,
    dirty: &'a mut DirtyTracker,
    router: &'a R,
    wall: &'a WallAnchor,
}

impl<'a, R: ShardTransport> ShardTransport for DispatchCtx<'a, R> {
    fn send_media(&self, dst: crate::id::ShardId, env: Envelope, payload: MediaPayload) {
        self.router.send_media(dst, env, payload);
    }

    fn send_frame(&self, dst: crate::id::ShardId, frame: ShardFrame) {
        self.router.send_frame(dst, frame);
    }
}

impl<'a, R: ShardTransport> DispatchCtx<'a, R> {
    fn notify_keyframe_request(
        &mut self,
        participant: ParticipantKey,
        track_id: TrackId,
        rid: Option<Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        if let Some(meta) = self.registry.resolve_mut(participant) {
            meta.handle_remote_keyframe_request((track_id, rid), kind);
            self.dirty.mark(participant, meta);
        }
    }

    fn deliver_reliable_control(
        &mut self,
        publisher: ParticipantKey,
        topic: &crate::track::Topic,
        bytes: &[u8],
    ) {
        if let Some(meta) = self.registry.resolve_mut(publisher) {
            meta.on_deliver_reliable_control(topic, bytes);
            self.dirty.mark(publisher, meta);
        }
    }
}

impl<'a, R: ShardTransport> RoutingContext for DispatchCtx<'a, R> {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantKey,
        slot: DownstreamSlotKey,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
    ) {
        if let Some(participant) = self.registry.resolve_mut(subscriber) {
            participant.on_forward_rtp(slot, pkt, cache);
            self.dirty.mark(subscriber, participant);
        }
    }

    fn update_layer_states(
        &mut self,
        subscriber: ParticipantKey,
        slot: DownstreamSlotKey,
        states: &crate::track::TrackStates,
    ) {
        if let Some(participant) = self.registry.resolve_mut(subscriber) {
            participant.update_layer_states(slot, states);
        }
    }

    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantKey,
        slot_idx: AudioSelectorSlotId,
        origin: crate::entity::AudioOrigin,
        pkt: &RtpPacket,
    ) {
        if let Some(participant) = self.registry.resolve_mut(subscriber) {
            participant.on_forward_audio_rtp(slot_idx, origin, pkt);
            self.dirty.mark(subscriber, participant);
        }
    }

    fn forward_sctp(
        &mut self,
        subscriber: ParticipantKey,
        channel: str0m::channel::ChannelId,
        pkt: &[u8],
    ) {
        if let Some(participant) = self.registry.resolve_mut(subscriber) {
            participant.on_forward_sctp(channel, pkt);
            self.dirty.mark(subscriber, participant);
        }
    }

    fn wall(&self) -> &WallAnchor {
        self.wall
    }

    fn forward_reliable_sctp(
        &mut self,
        subscriber: ParticipantKey,
        channel: str0m::channel::ChannelId,
        frame: &[u8],
    ) {
        if let Some(participant) = self.registry.resolve_mut(subscriber) {
            participant.on_forward_reliable_sctp(channel, frame);
            self.dirty.mark(subscriber, participant);
        }
    }
}

pub(crate) struct ShardCore {
    pub(crate) shard_id: crate::id::ShardId,
    view: crate::view::ShardView,
    view_rx: mailbox::Receiver<Box<crate::view::ShardViewDelta>>,
    registry: ParticipantRegistry,
    pub(super) runtime: ShardRuntime,
    timers: TimerWheel,
    dirty: DirtyTracker,
    udp_send_batch: GsoSendBatch,
    pipeline: EventPipeline,
    rng: Rng,
    wall: WallAnchor,
}

impl ShardCore {
    pub(crate) fn new(
        shard_id: impl Into<crate::id::ShardId>,
        max_gso_segments: usize,
        mut rng: Rng,
        wall: WallAnchor,
        view_rx: mailbox::Receiver<Box<crate::view::ShardViewDelta>>,
    ) -> Self {
        let shard_id = shard_id.into();
        let runtime = ShardRuntime::new(shard_id, &mut rng);
        let view = crate::view::ShardView {
            shard: shard_id,
            ..Default::default()
        };
        Self {
            shard_id,
            view,
            view_rx,
            registry: ParticipantRegistry::new(shard_id, max_gso_segments),
            runtime,
            timers: TimerWheel::new(PARTICIPANT_CAPACITY_HINT),
            dirty: DirtyTracker::with_capacity(PARTICIPANT_CAPACITY_HINT),
            udp_send_batch: GsoSendBatch::preallocated(),
            pipeline: EventPipeline::with_capacity(PARTICIPANT_CAPACITY_HINT),
            rng,
            wall,
        }
    }

    pub(crate) fn apply_view_deltas(&mut self) {
        while let Ok(delta) = self.view_rx.try_recv() {
            debug_assert_eq!(delta.shard, self.shard_id);
            for op in &delta.ops {
                if let crate::view::ViewOp::RemoveTrackRuntime { key } = op
                    && let Some(track) = self.runtime.track_publication(*key)
                {
                    self.registry.unpublish_track(&track.meta.id);
                }
                self.runtime.apply_view_op(op);
                match op {
                    crate::view::ViewOp::InsertTrackRuntime { key, descriptor } => {
                        self.registry.publish_track(&descriptor.publication);
                        if let Some(participant) = descriptor.participant
                            && let Some(meta) = self.registry.resolve_mut(participant)
                        {
                            meta.bind_published_track(descriptor.id, *key);
                        }
                    }
                    crate::view::ViewOp::InsertDataRuntime { publisher, key, id } => {
                        if let Some(meta) = self.registry.resolve_mut(*publisher) {
                            meta.bind_published_data_stream(&id.topic, *key);
                        }
                    }
                    crate::view::ViewOp::InsertReliableRuntime { publisher, key, id } => {
                        if let Some(meta) = self.registry.resolve_mut(*publisher) {
                            meta.bind_published_reliable_stream(&id.topic, *key);
                        }
                    }
                    crate::view::ViewOp::SetTrackPlan { key, plan } => {
                        if let Some(previous) = self.view.tracks.resolve(*key) {
                            for &(participant, _) in &previous.local_subscribers {
                                self.registry.unbind_subscribed_track(
                                    participant,
                                    previous.track_id,
                                    *key,
                                );
                            }
                        }
                        for &(participant, _) in &plan.local_subscribers {
                            self.registry
                                .bind_subscribed_track(participant, plan.track_id, *key);
                        }
                    }
                    crate::view::ViewOp::RemoveTrackPlan { key } => {
                        if let Some(previous) = self.view.tracks.resolve(*key) {
                            for &(participant, _) in &previous.local_subscribers {
                                self.registry.unbind_subscribed_track(
                                    participant,
                                    previous.track_id,
                                    *key,
                                );
                            }
                        }
                    }
                    crate::view::ViewOp::InstallRoute { .. }
                    | crate::view::ViewOp::RetireRoute { .. }
                    | crate::view::ViewOp::InstallTransport { .. }
                    | crate::view::ViewOp::RetireTransport { .. }
                    | crate::view::ViewOp::InsertParticipant { .. }
                    | crate::view::ViewOp::RemoveParticipant { .. }
                    | crate::view::ViewOp::RemoveTrackRuntime { .. }
                    | crate::view::ViewOp::RemoveDataRuntime { .. }
                    | crate::view::ViewOp::RemoveReliableRuntime { .. }
                    | crate::view::ViewOp::RemoveAudioPlan { .. }
                    | crate::view::ViewOp::SetAudioPlan { .. }
                    | crate::view::ViewOp::SetDataPlan { .. }
                    | crate::view::ViewOp::RemoveDataPlan { .. }
                    | crate::view::ViewOp::SetReliablePlan { .. }
                    | crate::view::ViewOp::RemoveReliablePlan { .. } => {}
                }
                if let crate::view::ViewOp::RemoveParticipant { key } = op {
                    self.timers.cancel(*key);
                    let _ = self.registry.remove_key(*key);
                }
            }
            delta.apply(&mut self.view);
        }
    }

    pub(crate) async fn view_readable(&mut self) -> Option<()> {
        self.view_rx.readable().await
    }

    fn on_media_frame(
        &mut self,
        env: Envelope,
        payload: MediaPayload,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        debug_assert_eq!(env.ty, crate::route::EnvelopeType::Media);
        #[allow(clippy::cast_possible_truncation)]
        let link_seq = (env.extension >> 32) as u32;
        #[allow(clippy::cast_possible_truncation)]
        let playout_ntp32 = env.extension as u32;
        let handle = RouteHandle::new(env.route, env.epoch);
        let view = &self.view;
        let Some(binding) = view.routes.resolve_binding(env.route, env.epoch) else {
            return;
        };
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_cross_shard_media();
        let action = binding.action;

        let entry = self.runtime.routes.accounting_mut(handle, self.wall.ntp());
        entry.observe(link_seq);
        let Ok(playout) = entry.expander.expand(playout_ntp32) else {
            return;
        };

        match (action, payload) {
            (crate::route::RouteAction::Video { local_track }, MediaPayload::Video(mut pkt)) => {
                let Some(plan) = view.tracks.resolve(local_track) else {
                    metrics::counter!("remote_video_before_plan").increment(1);
                    #[cfg(feature = "sim")]
                    crate::sim_metrics::record_routing_counter("remote_video_before_plan");
                    return;
                };
                pkt.playout_time = self.wall.to_instant(playout);
                pkt.arrival_ts = now;
                pkt.rehome_extensions();
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.runtime
                    .route_video_with_plan(local_track, *pkt, plan, &mut ctx);
            }
            (crate::route::RouteAction::Audio { track }, MediaPayload::Audio(mut pkt)) => {
                let Some(plan) = view.audio.resolve(track) else {
                    metrics::counter!("remote_audio_before_plan").increment(1);
                    #[cfg(feature = "sim")]
                    crate::sim_metrics::record_routing_counter("remote_audio_before_plan");
                    return;
                };
                pkt.playout_time = self.wall.to_instant(playout);
                pkt.arrival_ts = now;
                pkt.rehome_extensions();
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.runtime.route_audio_with_plan(
                    track,
                    Origin::Remote,
                    AudioRtpEvent {
                        stream_id: (plan.track_id, None),
                        pkt: *pkt,
                        origin: plan.origin,
                        origin_key: None,
                        fanout: Some(track),
                    },
                    plan,
                    &mut ctx,
                );
            }
            (crate::route::RouteAction::Data { stream }, MediaPayload::Data(bytes)) => {
                let Some(plan) = view.data.resolve(stream) else {
                    metrics::counter!("remote_data_before_plan").increment(1);
                    #[cfg(feature = "sim")]
                    crate::sim_metrics::record_routing_counter("remote_data_before_plan");
                    return;
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.runtime
                    .route_data_with_plan(stream, Origin::Remote, bytes, plan, &mut ctx);
            }
            (crate::route::RouteAction::Reliable { stream }, MediaPayload::Data(bytes)) => {
                let Some(plan) = view.reliable.resolve(stream) else {
                    metrics::counter!("remote_reliable_before_plan").increment(1);
                    #[cfg(feature = "sim")]
                    crate::sim_metrics::record_routing_counter("remote_reliable_before_plan");
                    return;
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.runtime.route_reliable_data_with_plan(
                    stream,
                    Origin::Remote,
                    bytes,
                    plan,
                    &mut ctx,
                );
            }
            _ => debug_assert!(false, "route action and payload type differ"),
        }
    }

    pub(crate) fn on_udp_batch(&mut self, batch: net::RecvPacketBatch) {
        let Some(handle) = self.registry.demux(&batch) else {
            return;
        };
        // Not an error: SO_REUSEPORT picks the receiving socket by hashing the
        // 4-tuple, which has nothing to do with which shard owns the route, so
        // a datagram for another shard arriving here is ordinary. Resolving a
        // route is not a claim to own it.
        if handle.shard() != self.shard_id {
            metrics::counter!("shard_wrong_owner_drop").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("shard_wrong_owner_drop");
            return;
        }
        let Some(key) = self.view.transports.resolve(handle) else {
            return;
        };
        let Some(participant) = self.registry.resolve_mut(key) else {
            debug_assert!(false, "transport view points at no participant");
            return;
        };
        participant.on_ingress(batch);
        self.dirty.mark(key, participant);
    }

    pub(crate) fn flush_stream_buffers(&mut self, router: &impl ShardTransport) {
        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
            wall: &self.wall,
        };
        while let Some(ev) = self.pipeline.pop_audio_rtp() {
            debug_assert_eq!(ev.stream_id.0.kind(), TrackKind::Audio);
            let Some(track) = ev.fanout else {
                metrics::counter!("audio_before_runtime").increment(1);
                continue;
            };
            let view = &self.view;
            let Some(plan) = view.audio.resolve(track) else {
                metrics::counter!("audio_before_plan").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("audio_before_plan");
                continue;
            };
            self.runtime
                .route_audio_with_plan(track, Origin::Local, ev, plan, &mut ctx);
        }
        while let Some(ev) = self.pipeline.pop_video_rtp() {
            debug_assert_eq!(ev.stream_id.0.kind(), TrackKind::Video);
            let Some(fanout) = ev.fanout else {
                metrics::counter!("video_before_runtime").increment(1);
                continue;
            };
            let view = &self.view;
            let Some(plan) = view.tracks.resolve(fanout) else {
                metrics::counter!("video_before_plan").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("video_before_plan");
                continue;
            };
            self.runtime
                .route_video_with_plan(fanout, ev.pkt, plan, &mut ctx);
        }
        while let Some(ev) = self.pipeline.pop_data_sctp() {
            let Some(stream) = ev.stream else {
                metrics::counter!("data_before_runtime").increment(1);
                continue;
            };
            let view = &self.view;
            let Some(plan) = view.data.resolve(stream) else {
                metrics::counter!("data_before_plan").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("data_before_plan");
                continue;
            };
            self.runtime.route_data_with_plan(
                stream,
                Origin::Local,
                Bytes::from(ev.pkt),
                plan,
                &mut ctx,
            );
        }
        while let Some(ev) = self.pipeline.pop_reliable_data_sctp() {
            let Some(stream) = ev.stream else {
                metrics::counter!("reliable_before_runtime").increment(1);
                continue;
            };
            let view = &self.view;
            let Some(plan) = view.reliable.resolve(stream) else {
                metrics::counter!("reliable_before_plan").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("reliable_before_plan");
                continue;
            };
            self.runtime.route_reliable_data_with_plan(
                stream,
                Origin::Local,
                Bytes::from(ev.pkt),
                plan,
                &mut ctx,
            );
        }
    }

    pub(crate) fn flush_participant_events(&mut self, router: &impl ShardTransport) {
        while let Some(event) = self.pipeline.pop_participant_event() {
            match event {
                ParticipantEvent::Subscription(ev) => match ev {
                    ParticipantSubscriptionEvent::Subscribed {
                        track,
                        subscriber,
                        subscriber_key,
                        slot,
                    } => self.pipeline.push_shard_event(ShardEvent::TrackSubscribed {
                        subscriber,
                        subscriber_key,
                        slot,
                        track,
                    }),
                    ParticipantSubscriptionEvent::Unsubscribed {
                        track,
                        subscriber,
                        slot,
                        ..
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::TrackUnsubscribed {
                            subscriber,
                            slot,
                            track,
                        }),
                },
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                    participant_key,
                }) => {
                    self.remove_participant(participant_key);
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantClosed {
                            participant: participant_id,
                        });
                }
                ParticipantEvent::Control(ev) => match ev {
                    ClientIntent::TrackPublished(mut track, states) => {
                        track.reverse = None;
                        self.pipeline.push_shard_event(ShardEvent::TrackPublished {
                            track: Box::new(track),
                            states,
                        });
                    }
                    ClientIntent::TrackUnpublished { origin, track_id } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::TrackUnpublished { origin, track_id });
                    }
                    ClientIntent::DataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::DataTopicPublished {
                                room_id,
                                publisher,
                                topic,
                            });
                    }
                    ClientIntent::DataTopicUnpublished {
                        room_id,
                        publisher,
                        topic,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::DataTopicUnpublished {
                            room_id,
                            publisher,
                            topic,
                        }),
                    ClientIntent::DataTopicSubscribed {
                        room_id,
                        subscriber,
                        topic,
                        publisher,
                        channel,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::DataTopicSubscribed {
                            room_id,
                            subscriber,
                            topic,
                            publisher,
                            channel,
                        }),
                    ClientIntent::DataTopicUnsubscribed {
                        room_id,
                        subscriber,
                        topic,
                        publisher,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::DataTopicUnsubscribed {
                            room_id,
                            subscriber,
                            topic,
                            publisher,
                        }),
                    ClientIntent::ReliableDataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::ReliableDataTopicPublished {
                                room_id,
                                publisher,
                                topic,
                            });
                    }
                    ClientIntent::ReliableDataTopicUnpublished {
                        room_id,
                        publisher,
                        topic,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::ReliableDataTopicUnpublished {
                            room_id,
                            publisher,
                            topic,
                        }),
                    ClientIntent::ReliableDataTopicSubscribed {
                        room_id,
                        subscriber,
                        topic,
                        channel,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::ReliableDataTopicSubscribed {
                            room_id,
                            subscriber,
                            topic,
                            channel,
                        }),
                    ClientIntent::ReliableDataTopicUnsubscribed {
                        room_id,
                        subscriber,
                        topic,
                    } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::ReliableDataTopicUnsubscribed {
                                room_id,
                                subscriber,
                                topic,
                            });
                    }
                },
                ParticipantEvent::Internal(ev) => match ev {
                    crate::shard::events::ShardInternalEvent::TrackStatsUpdated {
                        track_id,
                        fanout,
                        states,
                    } => {
                        self.apply_local_track_stats(fanout, track_id, states, router);
                    }
                    crate::shard::events::ShardInternalEvent::KeyframeRequested {
                        request,
                        fanout,
                    } => {
                        self.send_keyframe_request(fanout, request, router);
                    }
                    crate::shard::events::ShardInternalEvent::ReliableControlReceived {
                        stream,
                        bytes,
                    } => {
                        let Some(stream) = stream else {
                            debug_assert!(false, "reliable control has no compiled stream key");
                            continue;
                        };
                        let view = &self.view;
                        let Some(plan) = view.reliable.resolve(stream) else {
                            debug_assert!(false, "reliable control has no compiled plan");
                            continue;
                        };
                        let mut ctx = DispatchCtx {
                            registry: &mut self.registry,
                            dirty: &mut self.dirty,
                            router,
                            wall: &self.wall,
                        };
                        self.runtime
                            .route_reliable_control(Bytes::from(bytes), plan, &mut ctx);
                    }
                },
            }
        }
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    fn apply_local_track_stats(
        &mut self,
        fanout: Option<crate::shard::router::TrackKey>,
        track_id: crate::entity::TrackId,
        states: crate::track::TrackStates,
        router: &impl ShardTransport,
    ) {
        let Some(fanout) = fanout else {
            metrics::counter!("track_stats_before_runtime").increment(1);
            return;
        };
        let Some((runtime_track_id, _)) = self.runtime.track_descriptor(fanout) else {
            metrics::counter!("track_stats_before_runtime").increment(1);
            return;
        };
        debug_assert_eq!(runtime_track_id, track_id);
        let Some(plan) = self.view.tracks.resolve(fanout) else {
            metrics::counter!("track_stats_before_plan").increment(1);
            return;
        };
        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
            wall: &self.wall,
        };
        self.runtime.apply_stats(fanout, states, plan, &mut ctx);
    }

    fn send_keyframe_request(
        &mut self,
        fanout: Option<crate::shard::router::TrackKey>,
        req: crate::track::GlobalKeyframeRequest,
        router: &impl ShardTransport,
    ) {
        let Some(fanout) = fanout else {
            metrics::counter!("keyframe_before_runtime").increment(1);
            return;
        };
        let Some(plan) = self.view.tracks.resolve(fanout) else {
            metrics::counter!("keyframe_before_plan").increment(1);
            return;
        };
        let Some(reverse) = plan.reverse_route else {
            metrics::counter!("keyframe_before_reverse_route").increment(1);
            return;
        };
        let Some((_, encodings)) = self.runtime.track_descriptor(fanout) else {
            metrics::counter!("keyframe_before_descriptor").increment(1);
            return;
        };
        let mut layer = None;
        for (index, rid) in encodings.iter().enumerate() {
            if *rid == req.stream_id.1 {
                layer = Some(index);
                break;
            }
        }
        let Some(layer) = layer else {
            metrics::counter!("keyframe_unknown_rid").increment(1);
            return;
        };
        let Ok(layer) = u8::try_from(layer) else {
            metrics::counter!("keyframe_encoding_overflow").increment(1);
            return;
        };
        router.send_frame(
            reverse.shard_id,
            ShardFrame::Reverse {
                env: Envelope::feedback(RouteHandle::new(reverse.route, reverse.epoch)),
                body: Reverse::Keyframe {
                    layer,
                    kind: req.kind,
                },
            },
        );
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        router: &impl ShardTransport,
    ) -> Option<()> {
        match cmd {
            ShardCommand::MaterializeParticipant {
                key,
                transport,
                config,
            } => {
                self.add_participant(key, transport, *config);
            }
            ShardCommand::AdoptTcpConnection { .. } => {
                debug_assert!(false, "TCP handoff is consumed by the worker");
            }
        }
        let _ = router;
        Some(())
    }

    pub(crate) fn on_shard_frame(
        &mut self,
        frame: ShardFrame,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        match frame {
            ShardFrame::Media { env, payload } => self.on_media_frame(env, payload, now, router),
            ShardFrame::Reverse { env, body } => self.on_reverse_frame(env, body, router),
            ShardFrame::Telemetry { env, stats } => {
                let view = &self.view;
                let Some(crate::route::RouteAction::Video { local_track }) =
                    view.routes.resolve(env.route, env.epoch).copied()
                else {
                    return;
                };
                let Some(plan) = view.tracks.resolve(local_track) else {
                    metrics::counter!("stats_before_plan").increment(1);
                    return;
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.runtime.apply_stats(local_track, stats, plan, &mut ctx);
            }
        }
    }

    fn on_reverse_frame(&mut self, env: Envelope, body: Reverse, router: &impl ShardTransport) {
        debug_assert_eq!(env.ty, crate::route::EnvelopeType::Feedback);
        let Some(action) = self.view.routes.resolve(env.route, env.epoch).copied() else {
            return;
        };
        let Some((origin, target)) = self.runtime.resolve_reverse(action) else {
            return;
        };
        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
            wall: &self.wall,
        };
        match (target, body) {
            (crate::route::ReverseTarget::Track { track }, Reverse::Keyframe { layer, kind }) => {
                let Some((track_id, encodings)) = self.runtime.track_descriptor(track) else {
                    metrics::counter!("reverse_before_runtime").increment(1);
                    return;
                };
                let Some(rid) = encodings.get(usize::from(layer)).copied() else {
                    metrics::counter!("reverse_unknown_layer").increment(1);
                    return;
                };
                ctx.notify_keyframe_request(origin, track_id, rid, kind);
            }
            (crate::route::ReverseTarget::Topic { stream }, Reverse::DataAck(bytes)) => {
                let Some(topic) = self.runtime.reliable_topic(stream).cloned() else {
                    return;
                };
                ctx.deliver_reliable_control(origin, &topic, &bytes);
            }
            _ => {}
        }
    }

    fn add_participant(
        &mut self,
        key: ParticipantKey,
        transport: crate::route::TransportHandle,
        cfg: ParticipantConfig,
    ) {
        debug_assert_eq!(transport.shard(), self.shard_id);
        if !self.registry.insert(key, cfg, transport, &mut self.rng) {
            return;
        }
        if let Some(participant) = self.registry.resolve_mut(key) {
            self.dirty.mark(key, participant);
        }
    }

    fn remove_participant(&mut self, key: ParticipantKey) -> Option<()> {
        self.timers.cancel(key);
        self.registry.remove_key(key)?;
        Some(())
    }

    pub(crate) fn participant_count(&self) -> usize {
        self.registry.len()
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.timers.next_deadline()
    }

    pub(crate) fn fire_timers(&mut self, now: Instant) {
        let registry = &mut self.registry;
        let dirty = &mut self.dirty;
        self.timers.drain_expired(now, |key| {
            if let Some(participant) = registry.resolve_mut(key) {
                participant.on_timeout(now);
                dirty.mark(key, participant);
            }
        });
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        now: Instant,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        debug_assert!(self.udp_send_batch.is_empty());
        self.dirty.begin_phase();
        while let Some(key) = self.dirty.next() {
            let Some(participant) = self.registry.resolve_mut(key) else {
                continue;
            };
            participant.queued_dirty = false;
            let who = crate::shard::events::SinkIdentity {
                id: participant.participant_id,
                key,
                room_id: participant.room_id,
            };
            let mut sink = self.pipeline.participant_sink(who);
            if let Some(deadline) = participant.poll(now, &mut sink) {
                self.timers.schedule(key, deadline);
            }
            while self
                .udp_send_batch
                .append_from(&mut participant.udp_packets)
            {
                if self.udp_send_batch.is_full() {
                    self.udp_send_batch.flush(udp_socket);
                }
            }
            participant.tcp_batcher.flush_tcp(tcp_socket);
        }
        self.dirty.finish_phase();
        self.udp_send_batch.flush(udp_socket);
    }

    pub(crate) fn flush_close_peers(
        &mut self,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        for addr in self.registry.drain_pending_close() {
            udp_socket.close_peer(&addr);
            tcp_socket.close_peer(&addr);
        }
    }
}

#[cfg(test)]
mod wrong_owner_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::control::ufrag::IceUfrag;
    use crate::id::ShardId;
    use crate::route::TransportRoute;
    use pulsebeam_runtime::net::{RecvPacketBatch, Transport, UdpMode};

    const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
    const BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
    const USERNAME_TYPE: [u8; 2] = [0x00, 0x06];

    fn stun_binding_request(server_ufrag: &str) -> Vec<u8> {
        let username = format!("{server_ufrag}:client");
        let value = username.as_bytes();
        let padded = value.len().next_multiple_of(4);
        let attr_total = 4 + padded;

        let mut buf = Vec::with_capacity(20 + attr_total);
        buf.extend_from_slice(&BINDING_REQUEST);
        buf.extend_from_slice(&u16::try_from(attr_total).unwrap().to_be_bytes());
        buf.extend_from_slice(&MAGIC_COOKIE);
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(&USERNAME_TYPE);
        buf.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(value);
        buf.resize(buf.len() + (padded - value.len()), 0);
        buf
    }

    /// `SO_REUSEPORT` picks the receiving socket by hashing the 4-tuple, which
    /// has nothing to do with which shard the ufrag names, so a datagram for
    /// another shard arriving here is ordinary traffic. It must be dropped and
    /// counted — never asserted on, and never re-enqueued.
    ///
    /// This was a `debug_assert_eq!`, which aborted under the sim and test
    /// profiles and made the counted drop three lines below it unreachable.
    /// Re-adding it makes this test abort rather than fail.
    #[tokio::test(start_paused = true)]
    async fn a_datagram_for_another_shard_is_dropped_not_asserted() {
        let shard = ShardId::new(0);
        let (_writer, view_rx) = crate::view::new_shard_view(shard);
        let mut core = ShardCore::new(
            shard,
            1,
            pulsebeam_runtime::rand::seeded_rng(1),
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            view_rx,
        );

        let foreign = TransportRoute::new(ShardId::new(3), 41);
        let data = stun_binding_request(&IceUfrag::new(0, 0, foreign, 9).encode());
        let len = data.len();
        core.on_udp_batch(RecvPacketBatch {
            src: "203.0.113.7:40000".parse().unwrap(),
            dst: "198.51.100.1:3478".parse().unwrap(),
            buf: data,
            stride: len,
            len,
            transport: Transport::Udp(UdpMode::Scalar),
            offset: 0,
        });

        assert_eq!(
            core.participant_count(),
            0,
            "a foreign-shard datagram must not create or touch local state"
        );
    }
}
