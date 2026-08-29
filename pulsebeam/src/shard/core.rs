use pulsebeam_runtime::{
    mailbox,
    net::{self, UnifiedSocket},
};
use slotmap::SecondaryMap;
use tokio::time::Instant;

use crate::clock::WallAnchor;
use crate::route::{Envelope, TransportHandle};
use crate::shard::events::{ParticipantBindingEvent, ParticipantEvent, ParticipantLifecycleEvent};
use crate::{
    keys::ParticipantKey,
    participant::{
        ParticipantConfig,
        batcher::{
            AppendStatus, Batcher, DepartureReceipt, GsoSendBatch, NetworkEgress, OwnedPacketQueue,
        },
    },
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::ParticipantRegistry,
        router::{Origin, ShardRuntime},
        timer::TimerWheel,
        updates::ShardUpdateApplication,
    },
};

pub(crate) use super::router::ShardTransport;
use super::worker::{MediaPayload, ShardCommand, ShardEvent, ShardFrame};

const PARTICIPANT_CAPACITY_HINT: usize = 64;
const MAX_DEPARTURES_PER_FLUSH: usize = net::BATCH_SIZE * 64;

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
    departures: &'a mut Vec<DepartureReceipt>,
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
        let departures = &mut *self.departures;
        let udp_progress = self.udp.flush(self.udp_socket, |receipt| {
            if departures.len() >= MAX_DEPARTURES_PER_FLUSH {
                debug_assert!(
                    false,
                    "one shard flush exceeded its departure receipt bound"
                );
            } else {
                departures.push(receipt);
            }
        });
        let tcp_progress = self.tcp.flush(self.tcp_socket, |receipt| {
            if departures.len() >= MAX_DEPARTURES_PER_FLUSH {
                debug_assert!(
                    false,
                    "one shard flush exceeded its departure receipt bound"
                );
            } else {
                departures.push(receipt);
            }
        });
        udp_progress || tcp_progress
    }
}

pub(crate) struct ShardCore {
    execution: ShardExecution,
    updates: ShardUpdateApplication,
}

pub(crate) struct ShardExecution {
    pub(crate) shard_id: crate::id::ShardId,
    transports: crate::shard_update::TransportImage,
    registry: ParticipantRegistry,
    pub(super) runtime: ShardRuntime,
    plans: SecondaryMap<crate::keys::TrackKey, crate::shard_update::TrackPlan>,
    timers: TimerWheel,
    dirty: DirtyTracker,
    udp_send_batch: GsoSendBatch,
    tcp_send_batcher: Batcher,
    departures: Vec<DepartureReceipt>,
    pipeline: EventPipeline,
    wall: WallAnchor,
}

impl ShardCore {
    pub(crate) fn new(
        shard_id: impl Into<crate::id::ShardId>,
        max_gso_segments: usize,
        shard_count: usize,
        wall: WallAnchor,
        update_rx: mailbox::Receiver<Box<crate::shard_update::ShardUpdate>>,
    ) -> Self {
        let shard_id = shard_id.into();
        Self {
            execution: ShardExecution::new(shard_id, max_gso_segments, shard_count, wall),
            updates: ShardUpdateApplication::new(shard_id, update_rx),
        }
    }

    pub(crate) fn apply_updates(&mut self, budget: usize) -> usize {
        self.updates.apply(&mut self.execution, budget)
    }

    pub(crate) async fn update_readable(&mut self) -> Option<()> {
        self.updates.readable().await
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        router: &impl ShardTransport,
    ) -> Option<()> {
        let result = self.execution.on_command(cmd, router);
        self.updates
            .apply_pending_participant_effects(&mut self.execution);
        result
    }

    pub(crate) fn on_shard_frames(
        &mut self,
        frames: impl IntoIterator<Item = ShardFrame>,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        self.execution.on_shard_frames(frames, now, router);
    }

    pub(crate) fn on_udp_batch_routed(
        &mut self,
        batch: net::RecvPacketBatch,
        router: &impl ShardTransport,
    ) {
        self.execution.on_udp_batch_routed(batch, router);
    }

    pub(crate) fn flush_stream_buffers(
        &mut self,
        router: &impl ShardTransport,
        budget: usize,
    ) -> usize {
        self.execution.flush_stream_buffers(router, budget)
    }

    pub(crate) fn flush_participant_events(
        &mut self,
        router: &impl ShardTransport,
        budget: usize,
    ) -> usize {
        self.execution.flush_participant_events(router, budget)
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.execution.has_pending_events()
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.execution.pop_shard_event()
    }

    pub(crate) fn participant_count(&self) -> usize {
        self.execution.participant_count()
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.execution.next_timer_deadline()
    }

    pub(crate) fn fire_timers(&mut self, now: Instant) {
        self.execution.fire_timers(now);
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        now: Instant,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
        budget: usize,
    ) -> usize {
        self.execution
            .poll_and_flush_dirty(now, udp_socket, tcp_socket, budget)
    }

    pub(crate) fn flush_close_peers(
        &mut self,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        self.execution.flush_close_peers(udp_socket, tcp_socket);
    }
}

impl ShardExecution {
    fn new(
        shard_id: crate::id::ShardId,
        max_gso_segments: usize,
        shard_count: usize,
        wall: WallAnchor,
    ) -> Self {
        debug_assert!(
            std::mem::size_of::<Self>() < 16 * 1024,
            "shard execution state must keep bounded packet buffers on the heap"
        );
        debug_assert!(shard_count > 0);
        // A node cannot bind more sockets than `PackedRoute` can address, and
        // the route's shard field is 12 bits — so this cannot overflow. Clamp
        // rather than panic: a shard that mis-sizes its own steering table
        // should drop packets it cannot own, not take the process down.
        let shard_count = u16::try_from(shard_count).unwrap_or(u16::MAX);
        debug_assert!(shard_count > 0, "a node always has at least one shard");
        let runtime = ShardRuntime::new(shard_id);
        Self {
            shard_id,
            transports: Default::default(),
            registry: ParticipantRegistry::new(shard_id, max_gso_segments, shard_count),
            runtime,
            plans: SecondaryMap::new(),
            timers: TimerWheel::new(PARTICIPANT_CAPACITY_HINT),
            dirty: DirtyTracker::with_capacity(PARTICIPANT_CAPACITY_HINT),
            udp_send_batch: GsoSendBatch::preallocated(),
            tcp_send_batcher: Batcher::with_capacity(max_gso_segments),
            departures: Vec::with_capacity(MAX_DEPARTURES_PER_FLUSH),
            pipeline: EventPipeline::with_capacity(PARTICIPANT_CAPACITY_HINT),
            wall,
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
        let departures = &mut self.departures;
        let Some(participant) = registry.resolve_mut(key) else {
            return;
        };
        let mut egress = ShardNetworkEgress {
            udp,
            tcp,
            udp_socket,
            tcp_socket,
            departures,
        };
        participant.drain_network(&mut egress);
    }

    pub(super) fn apply_lifecycle_op(&mut self, op: &crate::shard_update::ShardUpdateOp) {
        debug_assert!(op.is_owned_by(self.shard_id));
        match op {
            crate::shard_update::ShardUpdateOp::InstallRoute { handle, action } => {
                self.runtime.routes.install_action(*handle, *action);
            }
            crate::shard_update::ShardUpdateOp::RetireRoute { handle } => {
                let _ = self.runtime.routes.retire(*handle);
            }
            crate::shard_update::ShardUpdateOp::InstallTransport { binding } => {
                self.transports.install(*binding);
            }
            crate::shard_update::ShardUpdateOp::RetireTransport { handle } => {
                self.transports.retire(*handle);
            }
            crate::shard_update::ShardUpdateOp::InsertParticipant
            | crate::shard_update::ShardUpdateOp::Placeholder => {}
            crate::shard_update::ShardUpdateOp::RemoveParticipant { key } => {
                self.timers.cancel(*key);
                let _ = self.registry.remove_key(*key);
            }
            crate::shard_update::ShardUpdateOp::InsertTrackRuntime { runtime, .. } => {
                self.runtime.apply_update_op(op);
                if let (Some(publisher), Some(effect)) =
                    (runtime.publisher, runtime.publisher_effect.as_ref())
                {
                    let Some(meta) = self.registry.resolve_mut(publisher) else {
                        debug_assert!(
                            false,
                            "a published topic must be live shard={} key={:?}",
                            self.shard_id, publisher
                        );
                        return;
                    };
                    meta.apply(effect.clone());
                }
            }
            crate::shard_update::ShardUpdateOp::RemoveTrackRuntime { .. } => {
                self.runtime.apply_update_op(op);
            }
        }
    }

    pub(super) fn apply_participant_effect(
        &mut self,
        participant: ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) -> bool {
        let Some(meta) = self.registry.resolve_mut(participant) else {
            return false;
        };
        meta.apply(effect);
        self.dirty.mark(participant, meta);
        true
    }

    pub(super) fn apply_plan(&mut self, operation: &crate::shard_update::TrackPlanUpdate) -> usize {
        match &operation.plan {
            Some(plan) => {
                debug_assert!(plan.is_valid());
                self.plans.insert(operation.key, plan.clone());
                plan.local
                    .len()
                    .saturating_add(plan.remote.len())
                    .saturating_add(usize::from(plan.reverse_route.is_some()))
            }
            None => {
                debug_assert!(self.plans.contains_key(operation.key));
                usize::from(self.plans.remove(operation.key).is_some())
            }
        }
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
        let handle = env.handle;
        let Some(action) = self.runtime.routes.resolve(handle) else {
            return;
        };
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_cross_shard_media();
        let entry = self.runtime.routes.accounting_mut(handle, self.wall.ntp());
        entry.observe(link_seq);
        let Ok(playout) = entry.expander.expand(playout_ntp32) else {
            return;
        };

        let crate::route::RouteAction::Forward { target: key } = action else {
            debug_assert!(false, "a media envelope must resolve to a forward route");
            return;
        };
        let Some(plan) = self.plans.get(key) else {
            record_routing_drop("packet", "plan", "remote");
            return;
        };
        let mut payload = payload;
        payload.key = key;
        payload.set_remote_timing(self.wall.to_instant(playout), now);
        let mut ctx = crate::shard::router::ForwardingContext {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            wall: &self.wall,
            router,
        };
        self.runtime
            .route_packet_with_plan(key, Origin::Remote, payload, plan, &mut ctx);
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
        let Some(key) = self.transports.resolve(handle) else {
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
        let plans = &self.plans;
        let mut processed = 0;
        while processed < budget {
            let Some(packet) = self.pipeline.pop_packet() else {
                break;
            };
            processed = processed.saturating_add(1);
            let key = packet.key;
            let Some(plan) = plans.get(key) else {
                record_routing_drop("packet", "plan", "local");
                continue;
            };
            let mut ctx = crate::shard::router::ForwardingContext {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                wall: &self.wall,
                router,
            };
            self.runtime
                .route_packet_with_plan(key, Origin::Local, packet, plan, &mut ctx);
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
                ParticipantEvent::Binding(ev) => match ev {
                    ParticipantBindingEvent::Activated { track, subscriber } => self
                        .pipeline
                        .push_shard_event(ShardEvent::TrackSubscribed { subscriber, track }),
                    ParticipantBindingEvent::Deactivated {
                        track, subscriber, ..
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::TrackUnsubscribed { subscriber, track }),
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
                }) => {
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantClosed {
                            participant: participant_id,
                        });
                }
                ParticipantEvent::Control(ev) => {
                    self.pipeline.push_shard_event(ev);
                }
                ParticipantEvent::Internal(ev) => match ev {
                    crate::shard::events::ShardInternalEvent::ReverseRequested {
                        stream,
                        packet,
                    } => {
                        let Some(plan) = self.plans.get(stream) else {
                            debug_assert!(false, "reverse packet has no owned track plan");
                            continue;
                        };
                        self.runtime.route_reverse(packet, plan, router);
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
        for frame in frames {
            self.on_shard_frame(frame, now, router);
        }
    }

    fn on_shard_frame(&mut self, frame: ShardFrame, now: Instant, router: &impl ShardTransport) {
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
                if self.transports.resolve(handle).is_some() {
                    self.registry.learn_addr(batch.src, handle);
                }
                self.on_owned_udp_batch(batch, handle, source_shard);
            }
            ShardFrame::Media { env, payload } => {
                self.on_media_frame(env, payload, now, router);
            }
            ShardFrame::Reverse { env, packet } => self.on_reverse_frame(env, packet, now),
        }
    }

    fn on_reverse_frame(
        &mut self,
        env: Envelope,
        packet: crate::participant::reverse::ReversePacket,
        now: Instant,
    ) {
        debug_assert_eq!(env.ty, crate::route::EnvelopeType::Feedback);
        if !self
            .runtime
            .routes
            .accept_reverse(env.handle, packet.dedup(), now)
        {
            return;
        }
        let Some(action) = self.runtime.routes.resolve(env.handle) else {
            return;
        };
        let Some((origin, target)) = self.runtime.resolve_reverse(action) else {
            return;
        };
        if let Some(meta) = self.registry.resolve_mut(origin) {
            meta.input(crate::participant::ParticipantInput::Reverse {
                stream: target,
                packet,
            });
            self.dirty.mark(origin, meta);
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
        debug_assert!(self.departures.is_empty());
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
        {
            let departures = &mut self.departures;
            self.udp_send_batch.flush(udp_socket, |receipt| {
                if departures.len() >= MAX_DEPARTURES_PER_FLUSH {
                    debug_assert!(
                        false,
                        "one shard flush exceeded its departure receipt bound"
                    );
                } else {
                    departures.push(receipt);
                }
            });
            self.tcp_send_batcher.flush(tcp_socket, |receipt| {
                if departures.len() >= MAX_DEPARTURES_PER_FLUSH {
                    debug_assert!(
                        false,
                        "one shard flush exceeded its departure receipt bound"
                    );
                } else {
                    departures.push(receipt);
                }
            });
        }
        while let Some(receipt) = self.departures.pop() {
            if let Some(participant) = self.registry.resolve_mut(receipt.participant) {
                participant.report_departure(
                    receipt.send_id,
                    receipt.congestion_tracked,
                    receipt.timing,
                    now,
                );
            }
        }
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
        let (_writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );

        let foreign = TransportRoute::new(ShardId::new(3), 41);
        let data = stun_binding_request(&IceUfrag::new(0, 0, foreign, 9).encode());
        let len = data.len();
        core.execution.on_udp_batch(RecvPacketBatch {
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
        let (_writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
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
        let (mut writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let mut track_keys = slotmap::SlotMap::<crate::keys::TrackKey, ()>::with_key();
        let track = track_keys.insert(());
        let route = crate::route::RouteHandle::new(crate::route::RouteId::new(shard, 7), 1);

        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::InstallRoute {
                handle: route,
                action: crate::route::RouteAction::Forward { target: track },
            },
        );
        let plans = vec![crate::shard_update::TrackPlanUpdate {
            key: track,
            plan: Some(crate::shard_update::TrackPlan::default()),
        }];
        writer.stage_plans(1, plans);
        assert_eq!(writer.publish(), Some(1));

        assert_eq!(core.apply_updates(1), 1);
        assert!(core.execution.runtime.routes.resolve(route).is_some());
        let plans = &core.execution.plans;
        assert!(plans.get(track).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn route_retirement_waits_for_the_bounded_plan_cursor() {
        let shard = ShardId::new(0);
        let (mut writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let route = crate::route::RouteHandle::new(crate::route::RouteId::new(shard, 8), 1);
        let mut track_keys = slotmap::SlotMap::<crate::keys::TrackKey, ()>::with_key();
        let plans = (0..=crate::shard::worker::SHARD_PLAN_OPERATION_BUDGET)
            .map(|_| crate::shard_update::TrackPlanUpdate {
                key: track_keys.insert(()),
                plan: Some(crate::shard_update::TrackPlan::default()),
            })
            .collect();

        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::InstallRoute {
                handle: route,
                action: crate::route::RouteAction::Forward {
                    target: track_keys.insert(()),
                },
            },
        );
        writer.stage_plans(1, plans);
        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::RetireRoute { handle: route },
        );
        assert_eq!(writer.publish(), Some(1));

        assert_eq!(core.apply_updates(1), 0);
        assert!(core.execution.runtime.routes.resolve(route).is_some());

        assert_eq!(core.apply_updates(1), 1);
        assert!(core.execution.runtime.routes.resolve(route).is_none());
    }
}
