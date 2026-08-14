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
    entity::{ParticipantId, TrackId, TrackKind},
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
        participant_id: ParticipantId,
        track_id: TrackId,
        rid: Option<Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        if let Some((key, participant)) = self.registry.get_mut_with_key(&participant_id) {
            participant.handle_remote_keyframe_request((track_id, rid), kind);
            self.dirty.mark(key, participant);
        }
    }

    fn deliver_reliable_control(
        &mut self,
        publisher: ParticipantId,
        topic: &crate::track::Topic,
        bytes: &[u8],
    ) {
        if let Some((key, participant)) = self.registry.get_mut_with_key(&publisher) {
            participant.on_deliver_reliable_control(topic, bytes);
            self.dirty.mark(key, participant);
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
                    crate::view::ViewOp::InsertDataRuntime { id, key } => {
                        if let Some(meta) = self.registry.get_mut(&id.publisher_id) {
                            meta.bind_published_data_stream(&id.topic, *key);
                        }
                    }
                    crate::view::ViewOp::InsertReliableRuntime { id, key } => {
                        if let Some(meta) = self.registry.get_mut(&id.publisher_id) {
                            meta.bind_published_reliable_stream(&id.topic, *key);
                        }
                    }
                    _ => {}
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
                    .route_video_with_plan(local_track, pkt, plan, &mut ctx);
            }
            (crate::route::RouteAction::Audio { track }, MediaPayload::Audio(mut pkt)) => {
                let Some(plan) = view.audio.resolve(track) else {
                    metrics::counter!("remote_audio_before_plan").increment(1);
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
                        pkt,
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
        debug_assert_eq!(handle.shard(), self.shard_id);
        if handle.shard() != self.shard_id {
            metrics::counter!("shard_wrong_owner_drop").increment(1);
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
                continue;
            };
            self.runtime
                .route_data_with_plan(stream, Origin::Local, ev.pkt, plan, &mut ctx);
        }
        while let Some(ev) = self.pipeline.pop_reliable_data_sctp() {
            let Some(stream) = ev.stream else {
                metrics::counter!("reliable_before_runtime").increment(1);
                continue;
            };
            let view = &self.view;
            let Some(plan) = view.reliable.resolve(stream) else {
                metrics::counter!("reliable_before_plan").increment(1);
                continue;
            };
            self.runtime.route_reliable_data_with_plan(
                stream,
                Origin::Local,
                ev.pkt,
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
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::SubscriptionIntent {
                            intent: ClientIntent::TrackSubscribed {
                                subscriber,
                                subscriber_key,
                                slot,
                                track,
                            },
                        }),
                    ParticipantSubscriptionEvent::Unsubscribed {
                        track,
                        subscriber,
                        slot,
                        ..
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::SubscriptionIntent {
                            intent: ClientIntent::TrackUnsubscribed {
                                subscriber,
                                slot,
                                track,
                            },
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
                        self.pipeline.push_shard_event(ShardEvent::TrackObserved {
                            track: Box::new(track),
                            states,
                        });
                    }
                    ClientIntent::TrackUnpublished { origin, track_id } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::TrackClosed { origin, track_id });
                    }
                    ClientIntent::DataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::DataChannelObserved {
                                intent: ClientIntent::DataTopicPublished {
                                    room_id,
                                    publisher,
                                    topic,
                                },
                            });
                    }
                    ClientIntent::ReliableDataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::DataChannelObserved {
                                intent: ClientIntent::ReliableDataTopicPublished {
                                    room_id,
                                    publisher,
                                    topic,
                                },
                            });
                    }
                    ClientIntent::TrackStatsUpdated { track_id, states } => {
                        self.pipeline
                            .push_shard_event(ShardEvent::TrackStatsObserved {
                                track_id,
                                states: states.clone(),
                            });
                        self.apply_local_track_stats(track_id, states, router);
                    }
                    ClientIntent::KeyframeRequested(req) => {
                        self.send_keyframe_request(req, router);
                    }
                    ClientIntent::ReliableControlReceived { stream, bytes } => {
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
                        self.runtime.route_reliable_control(bytes, plan, &mut ctx);
                    }
                    ev => self
                        .pipeline
                        .push_shard_event(ShardEvent::SubscriptionIntent { intent: ev }),
                },
            }
        }
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    fn apply_local_track_stats(
        &mut self,
        track_id: crate::entity::TrackId,
        states: crate::track::TrackStates,
        router: &impl ShardTransport,
    ) {
        let Some(fanout) = self.runtime.track_key_for_id(track_id) else {
            metrics::counter!("track_stats_before_runtime").increment(1);
            return;
        };
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
        req: crate::track::GlobalKeyframeRequest,
        router: &impl ShardTransport,
    ) {
        let Some(fanout) = self.runtime.track_key_for_id(req.stream_id.0) else {
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
            ShardCommand::MaterializeParticipant { key, config } => {
                self.add_participant(key, *config);
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

    fn add_participant(&mut self, key: ParticipantKey, cfg: ParticipantConfig) {
        let Some(handle) = self.view.transports.handle_for(key, self.shard_id) else {
            debug_assert!(
                false,
                "participant materialization requires an installed transport"
            );
            return;
        };
        if !self.registry.insert(key, cfg, handle, &mut self.rng) {
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
