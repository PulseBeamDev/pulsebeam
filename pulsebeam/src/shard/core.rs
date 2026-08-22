use pulsebeam_runtime::{
    mailbox,
    net::{self, UnifiedSocket},
};
use std::collections::VecDeque;
use tokio::time::Instant;

use crate::clock::WallAnchor;
use crate::route::{Envelope, TransportHandle};
use crate::shard::events::{
    ParticipantEvent, ParticipantLifecycleEvent, ParticipantSubscriptionEvent,
};
use crate::{
    keys::ParticipantKey,
    participant::{
        ParticipantConfig,
        batcher::{AppendStatus, Batcher, GsoSendBatch, NetworkEgress, OwnedPacketQueue},
    },
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::ParticipantRegistry,
        router::{Origin, ShardRuntime},
        timer::TimerWheel,
    },
};

pub(crate) use super::router::ShardTransport;
use super::worker::{MediaPayload, Reverse, ShardCommand, ShardEvent, ShardFrame};

const PARTICIPANT_CAPACITY_HINT: usize = 64;

fn record_routing_drop(lane: &'static str, stage: &'static str, origin: &'static str) {
    metrics::counter!(
        "routing_drop",
        "lane" => lane,
        "stage" => stage,
        "origin" => origin
    )
    .increment(1);
    #[cfg(feature = "sim")]
    crate::sim_metrics::record_routing_drop(lane, stage, origin);
}

struct ShardNetworkEgress<'a> {
    udp: &'a mut GsoSendBatch,
    tcp: &'a mut Batcher,
    udp_socket: &'a mut UnifiedSocket,
    tcp_socket: &'a mut net::tcp::TcpTransport,
}

impl NetworkEgress for ShardNetworkEgress<'_> {
    fn append_udp(&mut self, packets: &mut OwnedPacketQueue) -> AppendStatus {
        if packets.is_empty() {
            return AppendStatus::Drained;
        }
        if self.udp.is_full() {
            return AppendStatus::Full;
        }
        let _ = self.udp.append_from(packets);
        if packets.is_empty() {
            AppendStatus::Drained
        } else {
            AppendStatus::Full
        }
    }

    fn append_tcp(&mut self, batcher: &mut Batcher) -> AppendStatus {
        if batcher.is_empty() {
            return AppendStatus::Drained;
        }
        self.tcp.append_batcher(batcher);
        AppendStatus::Drained
    }

    fn flush(&mut self) -> bool {
        let udp_progress = self.udp.flush(self.udp_socket);
        let tcp_progress = self.tcp.flush(self.tcp_socket);
        udp_progress || tcp_progress
    }
}

pub(crate) struct ShardCore {
    pub(crate) shard_id: crate::id::ShardId,
    view: crate::view::ShardView,
    view_rx: mailbox::Receiver<Box<crate::view::ControlBatch>>,
    pending_view_delta: Option<Box<crate::view::ControlBatch>>,
    pending_participant_effects: VecDeque<(ParticipantKey, crate::participant::ParticipantEffect)>,
    registry: ParticipantRegistry,
    pub(super) runtime: ShardRuntime,
    plans: crate::plan::FlatPlanPublisher,
    plan_reader: crate::plan::PlanReader,
    timers: TimerWheel,
    dirty: DirtyTracker,
    udp_send_batch: GsoSendBatch,
    tcp_send_batcher: Batcher,
    pipeline: EventPipeline,
    wall: WallAnchor,
}

fn is_retire(op: &crate::view::ViewOp) -> bool {
    matches!(
        op,
        crate::view::ViewOp::RetireRoute { .. }
            | crate::view::ViewOp::RetireTransport { .. }
            | crate::view::ViewOp::RemoveParticipant { .. }
            | crate::view::ViewOp::RemoveTrackRuntime { .. }
            | crate::view::ViewOp::RemoveUnreliableRuntime { .. }
            | crate::view::ViewOp::RemoveReliableRuntime { .. }
    )
}

impl ShardCore {
    pub(crate) fn new(
        shard_id: impl Into<crate::id::ShardId>,
        max_gso_segments: usize,
        shard_count: usize,
        wall: WallAnchor,
        view_rx: mailbox::Receiver<Box<crate::view::ControlBatch>>,
    ) -> Self {
        let shard_id = shard_id.into();
        debug_assert!(shard_count > 0);
        // A node cannot bind more sockets than `PackedRoute` can address, and
        // the route's shard field is 12 bits — so this cannot overflow. Clamp
        // rather than panic: a shard that mis-sizes its own steering table
        // should drop packets it cannot own, not take the process down.
        let shard_count = u16::try_from(shard_count).unwrap_or(u16::MAX);
        debug_assert!(shard_count > 0, "a node always has at least one shard");
        let runtime = ShardRuntime::new(shard_id);
        let plans = crate::plan::FlatPlanPublisher::new();
        let plan_reader = plans.reader();
        let view = crate::view::ShardView {
            shard: shard_id,
            ..Default::default()
        };
        Self {
            shard_id,
            view,
            view_rx,
            pending_view_delta: None,
            pending_participant_effects: VecDeque::new(),
            registry: ParticipantRegistry::new(shard_id, max_gso_segments, shard_count),
            runtime,
            plans,
            plan_reader,
            timers: TimerWheel::new(PARTICIPANT_CAPACITY_HINT),
            dirty: DirtyTracker::with_capacity(PARTICIPANT_CAPACITY_HINT),
            udp_send_batch: GsoSendBatch::preallocated(),
            tcp_send_batcher: Batcher::with_capacity(max_gso_segments),
            pipeline: EventPipeline::with_capacity(PARTICIPANT_CAPACITY_HINT),
            wall,
        }
    }

    pub(crate) fn apply_view_deltas(&mut self, budget: usize) -> usize {
        debug_assert!(budget > 0);
        debug_assert_eq!(self.view.shard, self.shard_id);
        let mut applied = 0;
        while applied < budget {
            if self.pending_view_delta.is_none() {
                let Ok(delta) = self.view_rx.try_recv() else {
                    break;
                };
                self.pending_view_delta = Some(delta);
            }
            let Some(delta) = self.pending_view_delta.take() else {
                debug_assert!(false, "a readable view delta must be retained");
                break;
            };
            if !delta.validate_for(self.shard_id, self.view.generation) {
                debug_assert_eq!(delta.shard, self.shard_id);
                debug_assert!(
                    delta.generation > self.view.generation,
                    "view generations arrive strictly newer"
                );
                continue;
            }
            for op in delta
                .lifecycle
                .iter()
                .filter(|op| matches!(op, crate::view::ViewOp::InsertParticipant))
            {
                self.apply_lifecycle_op(op);
            }
            self.apply_pending_participant_effects();
            for (participant, effect) in &delta.participant_effects {
                let Some(meta) = self.registry.resolve_mut(*participant) else {
                    self.pending_participant_effects
                        .push_back((*participant, effect.clone()));
                    continue;
                };
                meta.apply(effect.clone());
            }
            for op in delta.lifecycle.iter().filter(|op| {
                !is_retire(op) && !matches!(op, crate::view::ViewOp::InsertParticipant)
            }) {
                self.apply_lifecycle_op(op);
            }
            self.plans.append(delta.plans);
            self.plans.publish();
            for op in delta.lifecycle.iter().filter(|op| is_retire(op)) {
                self.apply_lifecycle_op(op);
            }
            self.view.generation = delta.generation;
            self.apply_pending_participant_effects();
            applied = applied.saturating_add(1);
        }
        applied
    }

    fn apply_pending_participant_effects(&mut self) {
        let pending = std::mem::take(&mut self.pending_participant_effects);
        for (participant, effect) in pending {
            let Some(meta) = self.registry.resolve_mut(participant) else {
                self.pending_participant_effects
                    .push_back((participant, effect));
                continue;
            };
            meta.apply(effect);
        }
    }

    fn drain_participant_network(
        &mut self,
        key: ParticipantKey,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        let registry = &mut self.registry;
        let udp = &mut self.udp_send_batch;
        let tcp = &mut self.tcp_send_batcher;
        let Some(participant) = registry.resolve_mut(key) else {
            return;
        };
        let mut egress = ShardNetworkEgress {
            udp,
            tcp,
            udp_socket,
            tcp_socket,
        };
        participant.drain_network(&mut egress);
    }

    fn apply_lifecycle_op(&mut self, op: &crate::view::ViewOp) {
        debug_assert!(op.is_owned_by(self.shard_id));
        match op {
            crate::view::ViewOp::InstallRoute { binding } => {
                self.view.routes.install(binding.clone());
            }
            crate::view::ViewOp::RetireRoute { handle } => self.view.routes.retire(*handle),
            crate::view::ViewOp::InstallTransport { binding } => {
                self.view.transports.install(*binding);
            }
            crate::view::ViewOp::RetireTransport { handle } => {
                self.view.transports.retire(*handle);
            }
            crate::view::ViewOp::InsertParticipant => {}
            crate::view::ViewOp::RemoveParticipant { key } => {
                self.timers.cancel(*key);
                let _ = self.registry.remove_key(*key);
                self.pending_participant_effects
                    .retain(|(participant, _)| *participant != *key);
            }
            crate::view::ViewOp::InsertTrackRuntime { .. } => {
                self.runtime.apply_view_op(op);
            }
            crate::view::ViewOp::RemoveTrackRuntime { .. }
            | crate::view::ViewOp::RemoveUnreliableRuntime { .. }
            | crate::view::ViewOp::RemoveReliableRuntime { .. } => self.runtime.apply_view_op(op),
            crate::view::ViewOp::InsertUnreliableRuntime { publisher, key, id } => {
                self.runtime.apply_view_op(op);
                if let Some(publisher) = publisher {
                    let Some(meta) = self.registry.resolve_mut(*publisher) else {
                        debug_assert!(
                            false,
                            "a data publisher must be live shard={} key={:?} stream={:?}",
                            self.shard_id, publisher, id
                        );
                        return;
                    };
                    meta.apply(crate::participant::ParticipantEffect::DataPublished {
                        topic: id.topic.clone(),
                        stream: *key,
                    });
                }
            }
            crate::view::ViewOp::InsertReliableRuntime { publisher, key, id } => {
                self.runtime.apply_view_op(op);
                if let Some(publisher) = publisher {
                    let Some(meta) = self.registry.resolve_mut(*publisher) else {
                        debug_assert!(
                            false,
                            "a reliable data publisher must be live shard={} key={:?} stream={:?}",
                            self.shard_id, publisher, id
                        );
                        return;
                    };
                    meta.apply(
                        crate::participant::ParticipantEffect::ReliableDataPublished {
                            topic: id.topic.clone(),
                            stream: *key,
                        },
                    );
                }
            }
            crate::view::ViewOp::BindSubscribedData {
                participant,
                stream,
                channel,
            } => {
                let Some(meta) = self.registry.resolve_mut(*participant) else {
                    debug_assert!(false, "a data binding must name a live participant");
                    return;
                };
                meta.apply(crate::participant::ParticipantEffect::DataSubscribed {
                    stream: *stream,
                    channel: *channel,
                });
            }
            crate::view::ViewOp::BindSubscribedReliable {
                participant,
                stream,
                channel,
            } => {
                let Some(meta) = self.registry.resolve_mut(*participant) else {
                    debug_assert!(
                        false,
                        "a reliable data binding must name a live participant"
                    );
                    return;
                };
                meta.apply(
                    crate::participant::ParticipantEffect::ReliableDataSubscribed {
                        stream: *stream,
                        channel: *channel,
                    },
                );
            }
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
        plans: &crate::plan::FlatPlans,
        router: &impl ShardTransport,
    ) {
        debug_assert_eq!(env.ty, crate::route::EnvelopeType::Media);
        #[allow(clippy::cast_possible_truncation)]
        let link_seq = (env.extension >> 32) as u32;
        #[allow(clippy::cast_possible_truncation)]
        let playout_ntp32 = env.extension as u32;
        let handle = env.handle;
        let view = &self.view;
        let Some(binding) = view.routes.resolve_binding(handle) else {
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
            (
                action @ (crate::route::RouteAction::Video { .. }
                | crate::route::RouteAction::Audio { .. }),
                MediaPayload::Track(packet),
            ) => {
                let key = match action {
                    crate::route::RouteAction::Video { local_track } => local_track.raw(),
                    crate::route::RouteAction::Audio { track } => track.raw(),
                    _ => {
                        debug_assert!(false, "a media route must carry a track action");
                        return;
                    }
                };
                let Some(mut pkt) = packet.into_rtp() else {
                    return;
                };
                let Some(plan) = plans.get(crate::plan::PlanKey::Track(key)) else {
                    record_routing_drop("track", "plan", "remote");
                    return;
                };
                pkt.playout_time = self.wall.to_instant(playout);
                pkt.arrival_ts = now;
                pkt.rehome_extensions();
                let mut ctx = crate::shard::router::ForwardingContext {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    wall: &self.wall,
                    router,
                };
                self.runtime
                    .route_rtp_with_plan(key, Origin::Remote, *pkt, plan, &mut ctx);
            }
            (crate::route::RouteAction::Unreliable { stream }, MediaPayload::Data(bytes)) => {
                let Some(plan) = plans.get(crate::plan::PlanKey::Unreliable(stream)) else {
                    record_routing_drop("data", "plan", "remote");
                    return;
                };
                let mut ctx = crate::shard::router::ForwardingContext {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    wall: &self.wall,
                    router,
                };
                self.runtime.route_unreliable_with_plan(
                    stream,
                    Origin::Remote,
                    bytes,
                    plan,
                    &mut ctx,
                );
            }
            (crate::route::RouteAction::Reliable { stream }, MediaPayload::Data(bytes)) => {
                let Some(plan) = plans.get(crate::plan::PlanKey::Reliable(stream)) else {
                    record_routing_drop("reliable", "plan", "remote");
                    return;
                };
                let mut ctx = crate::shard::router::ForwardingContext {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    wall: &self.wall,
                    router,
                };
                self.runtime.route_reliable_with_plan(
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

    #[cfg(test)]
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
        self.on_owned_udp_batch(batch, handle, self.shard_id);
    }

    pub(crate) fn on_udp_batch_routed(
        &mut self,
        batch: net::RecvPacketBatch,
        router: &impl ShardTransport,
    ) {
        let Some(handle) = self.registry.demux(&batch) else {
            return;
        };
        if handle.shard() != self.shard_id {
            router.send_frame(
                handle.shard(),
                ShardFrame::Ingress {
                    batch,
                    handle,
                    source_shard: self.shard_id,
                },
            );
            metrics::counter!("shard_wrong_owner_forward").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("shard_wrong_owner_forward");
            return;
        }
        self.on_owned_udp_batch(batch, handle, self.shard_id);
    }

    fn on_owned_udp_batch(
        &mut self,
        batch: net::RecvPacketBatch,
        handle: TransportHandle,
        source_shard: crate::id::ShardId,
    ) {
        debug_assert_eq!(handle.shard(), self.shard_id);
        let Some(key) = self.view.transports.resolve(handle) else {
            return;
        };
        let Some(participant) = self.registry.resolve_mut(key) else {
            record_routing_drop("transport", "runtime", "local");
            return;
        };
        participant.input(crate::participant::ParticipantInput::Network {
            batch,
            source_shard,
        });
        self.dirty.mark(key, participant);
    }

    pub(crate) fn flush_stream_buffers(
        &mut self,
        router: &impl ShardTransport,
        budget: usize,
    ) -> usize {
        debug_assert!(budget > 0);
        let plan_reader = self.plan_reader.clone();
        let Some(plans) = plan_reader.enter() else {
            return 0;
        };
        let mut processed = 0;
        while processed < budget {
            let Some(ev) = self.pipeline.pop_track() else {
                break;
            };
            processed = processed.saturating_add(1);
            let Some(plan) = plans.get(crate::plan::PlanKey::Track(ev.key)) else {
                record_routing_drop("track", "plan", "local");
                continue;
            };
            let mut ctx = crate::shard::router::ForwardingContext {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                wall: &self.wall,
                router,
            };
            let key = ev.key;
            let Some(packet) = ev.into_rtp() else {
                continue;
            };
            self.runtime
                .route_rtp_with_plan(key, Origin::Local, *packet, plan, &mut ctx);
        }
        while processed < budget {
            let Some(ev) = self.pipeline.pop_data_sctp() else {
                break;
            };
            processed = processed.saturating_add(1);
            let Some(stream) = ev.stream else {
                record_routing_drop("data", "runtime", "local");
                continue;
            };
            let Some(plan) = plans.get(crate::plan::PlanKey::Unreliable(stream)) else {
                record_routing_drop("data", "plan", "local");
                continue;
            };
            let mut ctx = crate::shard::router::ForwardingContext {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                wall: &self.wall,
                router,
            };
            self.runtime
                .route_unreliable_with_plan(stream, Origin::Local, ev.pkt, plan, &mut ctx);
        }
        while processed < budget {
            let Some(ev) = self.pipeline.pop_reliable_data_sctp() else {
                break;
            };
            processed = processed.saturating_add(1);
            let Some(stream) = ev.stream else {
                record_routing_drop("reliable", "runtime", "local");
                continue;
            };
            let Some(plan) = plans.get(crate::plan::PlanKey::Reliable(stream)) else {
                record_routing_drop("reliable", "plan", "local");
                continue;
            };
            let mut ctx = crate::shard::router::ForwardingContext {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                wall: &self.wall,
                router,
            };
            self.runtime
                .route_reliable_with_plan(stream, Origin::Local, ev.pkt, plan, &mut ctx);
        }
        processed
    }

    pub(crate) fn flush_participant_events(
        &mut self,
        router: &impl ShardTransport,
        budget: usize,
    ) -> usize {
        debug_assert!(budget > 0);
        let mut processed = 0;
        while processed < budget {
            let Some(event) = self.pipeline.pop_participant_event() else {
                break;
            };
            processed = processed.saturating_add(1);
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
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Connected {
                    participant_key,
                    source,
                    destination,
                    source_shard,
                }) => {
                    let Some(handle) = self.registry.authenticated_handle(participant_key) else {
                        debug_assert!(false, "authenticated participant must still be registered");
                        continue;
                    };
                    self.registry.authenticate_addr(source, handle);
                    self.pipeline
                        .push_shard_event(ShardEvent::TransportAuthenticated {
                            source,
                            destination,
                            source_shard,
                            handle,
                            shard: self.shard_id,
                        });
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                    participant_key,
                }) => {
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantClosed {
                            participant: participant_id,
                            key: participant_key,
                        });
                }
                ParticipantEvent::Control(ev) => {
                    self.pipeline.push_shard_event(ev);
                }
                ParticipantEvent::Internal(ev) => match ev {
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
                        let plan_reader = self.plan_reader.clone();
                        let Some(plans) = plan_reader.enter() else {
                            continue;
                        };
                        let Some(plan) = plans.get(crate::plan::PlanKey::Reliable(stream)) else {
                            debug_assert!(false, "reliable control has no compiled plan");
                            continue;
                        };
                        self.runtime.route_reliable_control(bytes, plan, router);
                    }
                },
            }
        }
        processed
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.pipeline.has_pending()
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    fn send_keyframe_request(
        &mut self,
        fanout: Option<crate::shard::router::TrackKey>,
        req: crate::track::GlobalKeyframeRequest,
        router: &impl ShardTransport,
    ) {
        let Some(fanout) = fanout else {
            record_routing_drop("keyframe", "runtime", "local");
            return;
        };
        let plan_reader = self.plan_reader.clone();
        let Some(plans) = plan_reader.enter() else {
            record_routing_drop("keyframe", "plan", "local");
            return;
        };
        let Some(plan) = plans.get(crate::plan::PlanKey::Track(fanout)) else {
            record_routing_drop("keyframe", "plan", "local");
            return;
        };
        let Some(reverse) = plan.reverse_route else {
            record_routing_drop("keyframe", "reverse", "local");
            return;
        };
        let Some((_, encodings)) = self.runtime.track_descriptor(fanout) else {
            record_routing_drop("keyframe", "descriptor", "local");
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
            record_routing_drop("keyframe", "encoding", "local");
            return;
        };
        let Ok(layer) = u8::try_from(layer) else {
            record_routing_drop("keyframe", "encoding", "local");
            return;
        };
        router.send_frame(
            reverse.shard(),
            ShardFrame::Reverse {
                env: Envelope::feedback(reverse),
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
                ack,
            } => {
                #[cfg(feature = "sim")]
                if crate::sim_metrics::take_materialization_failure() {
                    crate::sim_metrics::record_routing_counter("materialization_failed");
                    let _ = ack.send(false);
                    return Some(());
                }
                let materialized = self.add_participant(key, transport, *config);
                let _ = ack.send(materialized);
            }
            ShardCommand::AdoptTcpConnection { .. } => {
                debug_assert!(false, "TCP handoff is consumed by the worker");
            }
            ShardCommand::AuthenticateTransport { source, handle } => {
                self.registry.authenticate_addr(source, handle);
                metrics::counter!("demux_flow_authenticated").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("demux_flow_authenticated");
            }
        }
        let _ = router;
        Some(())
    }

    pub(crate) fn on_shard_frames(
        &mut self,
        frames: impl IntoIterator<Item = ShardFrame>,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        let reader = self.plan_reader.clone();
        let Some(plans) = reader.enter() else {
            return;
        };
        for frame in frames {
            self.on_shard_frame_with_plans(frame, now, plans.as_ref(), router);
        }
    }

    fn on_shard_frame_with_plans(
        &mut self,
        frame: ShardFrame,
        now: Instant,
        plans: &crate::plan::FlatPlans,
        router: &impl ShardTransport,
    ) {
        match frame {
            ShardFrame::Ingress {
                batch,
                handle,
                source_shard,
            } => {
                // A datagram that reached the node on another shard's socket.
                // Ordinary while a flow is bootstrapping — the steering map is
                // a cache, and a miss lands on whatever the kernel's tuple hash
                // picked. A rate that does not fall once flows are established
                // means the map is not being populated.
                debug_assert_ne!(
                    source_shard, self.shard_id,
                    "a shard cannot forward to itself"
                );
                metrics::counter!("shard_ingress_forwarded").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("shard_ingress_forwarded");
                if self.view.transports.resolve(handle).is_some() {
                    self.registry.learn_addr(batch.src, handle);
                }
                self.on_owned_udp_batch(batch, handle, source_shard);
            }
            ShardFrame::Media { env, payload } => {
                self.on_media_frame(env, payload, now, plans, router);
            }
            ShardFrame::Reverse { env, body } => self.on_reverse_frame(env, body),
        }
    }

    fn on_reverse_frame(&mut self, env: Envelope, body: Reverse) {
        debug_assert_eq!(env.ty, crate::route::EnvelopeType::Feedback);
        let Some(action) = self.view.routes.resolve(env.handle).copied() else {
            return;
        };
        let Some((origin, target)) = self.runtime.resolve_reverse(action) else {
            return;
        };
        match (target, body) {
            (crate::route::ReverseTarget::Track { track }, Reverse::Keyframe { layer, kind }) => {
                let Some((track_id, encodings)) = self.runtime.track_descriptor(track.raw()) else {
                    record_routing_drop("reverse", "runtime", "remote");
                    return;
                };
                let Some(rid) = encodings.get(usize::from(layer)).copied() else {
                    record_routing_drop("reverse", "encoding", "remote");
                    return;
                };
                if let Some(meta) = self.registry.resolve_mut(origin) {
                    meta.input(crate::participant::ParticipantInput::Keyframe {
                        stream_id: (track_id, rid),
                        kind,
                    });
                    self.dirty.mark(origin, meta);
                }
            }
            (crate::route::ReverseTarget::Topic { stream }, Reverse::DataAck(bytes)) => {
                let Some(topic) = self.runtime.reliable_topic(stream).cloned() else {
                    return;
                };
                if let Some(meta) = self.registry.resolve_mut(origin) {
                    meta.input(crate::participant::ParticipantInput::ReliableControl {
                        topic: &topic,
                        bytes: &bytes,
                    });
                    self.dirty.mark(origin, meta);
                }
            }
            _ => {}
        }
    }

    fn add_participant(
        &mut self,
        key: ParticipantKey,
        transport: crate::route::TransportHandle,
        cfg: ParticipantConfig,
    ) -> bool {
        debug_assert_eq!(transport.shard(), self.shard_id);
        if !self.registry.insert(key, cfg, transport) {
            return false;
        }
        if let Some(participant) = self.registry.resolve_mut(key) {
            self.dirty.mark(key, participant);
        }
        self.apply_pending_participant_effects();
        true
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
                participant.input(crate::participant::ParticipantInput::Timeout(now));
                dirty.mark(key, participant);
            }
        });
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        now: Instant,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
        budget: usize,
    ) -> usize {
        debug_assert!(budget > 0);
        debug_assert!(self.udp_send_batch.is_empty());
        self.dirty.begin_phase();
        let mut processed = 0;
        while processed < budget {
            let Some(key) = self.dirty.next() else {
                break;
            };
            processed = processed.saturating_add(1);
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
            self.drain_participant_network(key, udp_socket, tcp_socket);
        }
        if !self.dirty.exhausted() {
            self.dirty.finish_partial();
        } else {
            self.dirty.finish_phase();
        }
        self.udp_send_batch.flush(udp_socket);
        self.tcp_send_batcher.flush(tcp_socket);
        processed
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

    /// The compatibility entry point has no transport for forwarding, so a
    /// foreign datagram is counted and discarded. Workers use
    /// `on_udp_batch_routed`, which forwards the same datagram to its owner.
    #[tokio::test(start_paused = true)]
    async fn a_datagram_for_another_shard_is_dropped_not_asserted() {
        let shard = ShardId::new(0);
        let (_writer, view_rx) = crate::view::new_shard_view(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
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

    struct CaptureTransport {
        frames: std::cell::RefCell<Vec<(ShardId, ShardFrame)>>,
    }

    impl ShardTransport for CaptureTransport {
        fn send_media(&self, _dst: ShardId, _env: Envelope, _payload: MediaPayload) {}

        fn send_frame(&self, dst: ShardId, frame: ShardFrame) {
            self.frames.borrow_mut().push((dst, frame));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_datagram_for_another_shard_is_forwarded_to_its_owner() {
        let shard = ShardId::new(0);
        let (_writer, view_rx) = crate::view::new_shard_view(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            view_rx,
        );
        let router = CaptureTransport {
            frames: std::cell::RefCell::new(Vec::new()),
        };
        let foreign = TransportRoute::new(ShardId::new(3), 41);
        let data = stun_binding_request(&IceUfrag::new(0, 0, foreign, 9).encode());
        let len = data.len();

        core.on_udp_batch_routed(
            RecvPacketBatch {
                src: "203.0.113.7:40000".parse().unwrap(),
                dst: "198.51.100.1:3478".parse().unwrap(),
                buf: data,
                stride: len,
                len,
                transport: Transport::Udp(UdpMode::Scalar),
                offset: 0,
            },
            &router,
        );

        let mut frames = router.frames.borrow_mut();
        assert_eq!(frames.len(), 1);
        let (dst, frame) = frames.pop().expect("wrong-owner ingress must be forwarded");
        assert_eq!(dst, ShardId::new(3));
        assert!(matches!(frame, ShardFrame::Ingress { source_shard, .. } if source_shard == shard));
    }

    #[tokio::test(start_paused = true)]
    async fn a_view_delta_is_applied_as_one_consistent_state_transition() {
        let shard = ShardId::new(0);
        let (mut writer, view_rx) = crate::view::new_shard_view(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            view_rx,
        );
        let mut track_keys = slotmap::SlotMap::<crate::keys::TrackKey, ()>::with_key();
        let track = track_keys.insert(());
        let route = crate::route::RouteHandle::new(crate::route::RouteId::new(shard, 7), 1);

        writer.stage(
            1,
            crate::view::ViewOp::InstallRoute {
                binding: crate::view::RouteBinding {
                    handle: route,
                    action: crate::route::RouteAction::Video {
                        local_track: crate::keys::VideoTrackKey::new(track),
                    },
                },
            },
        );
        let mut plans = crate::plan::PlanBatch::default();
        plans.push(crate::plan::PlanChange {
            key: crate::plan::PlanKey::Track(track),
            create: true,
            remove: false,
            local: crate::plan::MembershipDelta::default(),
            remote: crate::plan::MembershipDelta::default(),
            reverse: crate::plan::ReverseRouteChange::Unchanged,
        });
        writer.stage_plans(1, plans);
        assert_eq!(writer.publish(), Some(1));

        assert_eq!(core.apply_view_deltas(1), 1);
        assert!(core.view.routes.resolve(route).is_some());
        let plan_reader = core.plan_reader.clone();
        let plans = plan_reader.enter().expect("the plan reader is alive");
        assert!(plans.get(crate::plan::PlanKey::Track(track)).is_some());
    }
}
