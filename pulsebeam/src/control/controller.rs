use std::time::Duration;
use std::{collections::HashMap, io};

use crate::control::state::ControlPlaneState;
use crate::{
    control::{
        core::{ControllerCore, RoomPlacement},
        lanes::{Lanes, StreamLane},
        negotiator::{Negotiator, NegotiatorError},
        outbox::{ControllerEvent, ControllerEventQueue},
        pending::{PendingStream, PendingStreams, PendingSubscription, PendingSubscriptions},
        router::ShardRouter,
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    route::{ReverseTarget, RouteAction, RouteHandle, TransportHandle},
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_runtime::mailbox;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

mod lifecycle;
mod participants;
mod routes;
mod stream_lifecycle;
mod track_lifecycle;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

#[derive(Debug, derive_more::From)]
pub enum ControllerCommand {
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

#[derive(Debug)]
pub struct CreateParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] NegotiatorError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("participant connection generation is stale")]
    StaleConnection,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);

struct PlanRequest {
    shard: crate::id::ShardId,
    key: crate::plan::PlanKey,
    plan: Option<crate::plan::FlatTrackPlan>,
}

struct GenerationOps {
    participant_effects: Vec<(
        crate::id::ShardId,
        crate::shard::participants::ParticipantKey,
        crate::participant::ParticipantEffect,
    )>,
    lifecycle: Vec<(crate::id::ShardId, crate::view::ViewOp)>,
    plans: Vec<PlanRequest>,
}

impl GenerationOps {
    fn lifecycle(ops: Vec<(crate::id::ShardId, crate::view::ViewOp)>) -> Self {
        Self {
            participant_effects: Vec::new(),
            lifecycle: ops,
            plans: Vec::new(),
        }
    }

    fn participant_effect(
        mut self,
        shard: crate::id::ShardId,
        participant: crate::shard::participants::ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) -> Self {
        self.participant_effects.push((shard, participant, effect));
        self
    }

    fn push_participant_effect(
        &mut self,
        shard: crate::id::ShardId,
        participant: crate::shard::participants::ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) {
        self.participant_effects.push((shard, participant, effect));
    }

    fn plan(
        mut self,
        shard: crate::id::ShardId,
        key: crate::plan::PlanKey,
        plan: crate::plan::FlatTrackPlan,
    ) -> Self {
        self.plans.push(PlanRequest {
            shard,
            key,
            plan: Some(plan),
        });
        self
    }

    fn remove_plan(mut self, shard: crate::id::ShardId, key: crate::plan::PlanKey) -> Self {
        self.plans.push(PlanRequest {
            shard,
            key,
            plan: None,
        });
        self
    }

    fn push_remove_plan(&mut self, shard: crate::id::ShardId, key: crate::plan::PlanKey) {
        self.plans.push(PlanRequest {
            shard,
            key,
            plan: None,
        });
    }

    fn extend_lifecycle(
        &mut self,
        ops: impl IntoIterator<Item = (crate::id::ShardId, crate::view::ViewOp)>,
    ) {
        self.lifecycle.extend(ops);
    }

    fn extend(&mut self, other: Self) {
        self.participant_effects.extend(other.participant_effects);
        self.lifecycle.extend(other.lifecycle);
        self.plans.extend(other.plans);
    }
}

fn source_authentication_command(
    source_shard: crate::id::ShardId,
    source: std::net::SocketAddr,
    handle: TransportHandle,
) -> (crate::id::ShardId, ShardCommand) {
    (
        source_shard,
        ShardCommand::AuthenticateTransport { source, handle },
    )
}

pub struct ControllerActor {
    router: ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    eq: ControllerEventQueue,
    /// Moved into the TCP acceptor task at the start of `run()`.
    tcp_listener: Option<pulsebeam_core::net::TcpListener>,
    /// Routing parameters encoded into every ICE ufrag.  Single-node deployments
    /// use 0 for both; set via `NodeBuilder` when multi-node support lands.
    cluster_id: u16,
    node_id: u16,
    /// The canonical lifecycle state. Only this actor mutates it, and no
    /// shard ever reads it — a shard reads the view projected from it.
    state: ControlPlaneState,
    /// Video declarations. Always fully concrete: a video subscription *is* a
    /// downstream slot allocation, and a slot belongs to one track, so a
    /// pattern here can never wildcard the name.
    video_patterns: crate::control::patterns::PatternTable<
        crate::entity::TrackId,
        crate::keys::DownstreamSlotKey,
        crate::control::patterns::VideoAudience,
    >,
    /// One writer per shard. Never shared, never locked, and never handed to
    /// a shard: the one-publish-per-generation budget is only checkable
    /// because there is exactly one caller.
    views: Vec<crate::view::ShardViewWriter>,
    compiled_plans: Vec<HashMap<crate::plan::PlanKey, crate::plan::FlatTrackPlan>>,
    /// Every publication on this node, whatever kind.
    catalog: crate::control::publication::Catalog,
    /// Data and reliable stream routing. One type per lane rather than three
    /// fields duplicated per lane, so the two cannot drift.
    lanes: Lanes,
    pending: PendingSubscriptions,
    pending_streams: PendingStreams,
    pending_audio: Vec<crate::entity::TrackId>,
    audio_patterns: crate::control::patterns::PatternTable<
        crate::entity::TrackId,
        (),
        crate::control::patterns::AudioAudience,
    >,
    /// Data declarations, keyed by topic *and* lane.
    ///
    /// Reliability is part of the subject rather than an attribute of the
    /// publication: `Topic::publisher()` can be resolved `.ordered()` or
    /// `.latest()`, and the two are independent namespaces, so the same name
    /// carries both without either claiming it. Putting the lane in the name
    /// keeps that separation while still letting one table serve both, instead
    /// of the paired registries it replaces.
    data_patterns: crate::control::patterns::PatternTable<
        (crate::track::Topic, StreamLane),
        str0m::channel::ChannelId,
        crate::control::patterns::DataAudience,
    >,
    #[cfg(not(feature = "sim"))]
    steering: Option<crate::ebpf::Steering>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<Candidate>,
        tcp_listener: pulsebeam_core::net::TcpListener,
        room_shard_slot: usize,
        placement: RoomPlacement,
        views: Vec<crate::view::ShardViewWriter>,
    ) -> Self {
        let shard_count = shard_contexts.len();
        debug_assert_eq!(
            views.len(),
            shard_count,
            "one view writer per shard, held only here"
        );
        let router = ShardRouter::new(shard_contexts);

        Self {
            router,
            core: ControllerCore::with_placement(room_shard_slot, placement),
            negotiator: Negotiator::new(candidates),
            eq: ControllerEventQueue::new(shard_count),
            tcp_listener: Some(tcp_listener),
            cluster_id: 0,
            node_id: 0,
            state: ControlPlaneState::new(shard_count),
            video_patterns: crate::control::patterns::PatternTable::new(),
            views,
            compiled_plans: (0..shard_count).map(|_| HashMap::new()).collect(),
            catalog: crate::control::publication::Catalog::new(),
            lanes: Lanes::new(),
            pending: PendingSubscriptions::default(),
            pending_streams: PendingStreams::default(),
            pending_audio: Vec::new(),
            audio_patterns: crate::control::patterns::PatternTable::new(),
            data_patterns: crate::control::patterns::PatternTable::new(),
            #[cfg(not(feature = "sim"))]
            steering: None,
        }
    }

    #[cfg(not(feature = "sim"))]
    pub(crate) fn set_steering(&mut self, steering: crate::ebpf::Steering) {
        debug_assert!(self.steering.is_none());
        self.steering = Some(steering);
    }

    /// Pin an authenticated flow to the shard that owns its route, so the
    /// kernel stops handing it to whichever shard the tuple hash picked and
    /// userspace stops forwarding it across cores.
    ///
    /// A failure here costs throughput, not correctness: the flow keeps
    /// arriving on the hashed shard, which resolves it from its own demux cache
    /// and forwards it to the owner.
    #[cfg(not(feature = "sim"))]
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        let Some(steering) = self.steering.as_mut() else {
            return;
        };
        let flow = crate::ebpf::flow_key(source, destination);
        if let Err(error) = steering.install_flow(flow, shard) {
            tracing::warn!(%error, shard, "failed to install authenticated eBPF flow");
        }
    }

    #[cfg(feature = "sim")]
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        pulsebeam_runtime::net::install_steering_flow(source, destination, shard);
    }

    /// Abandon the staged generation on both halves.
    ///
    /// The view writer accumulates a generation's ops before publishing, so
    /// dropping the control-plane transaction without dropping them would
    /// leave the abandoned ops staged — and the next generation would publish
    /// them as if they had been part of it.
    fn abort_transaction(&mut self, now: tokio::time::Instant) {
        for view in &mut self.views {
            view.abort();
        }
        self.state.abort(now);
    }

    fn view_mut(
        &mut self,
        shard_id: crate::id::ShardId,
    ) -> Option<&mut crate::view::ShardViewWriter> {
        self.views.get_mut(shard_id.index())
    }

    fn room_recipients(
        &self,
        room_id: crate::entity::RoomId,
        shard_id: crate::id::ShardId,
    ) -> Vec<crate::shard::participants::ParticipantKey> {
        self.core
            .registry
            .participant_keys_in_room(&room_id, shard_id)
    }

    fn is_current_binding(
        &self,
        participant: &ParticipantId,
        shard: crate::id::ShardId,
        binding: crate::shard::participants::ParticipantKey,
    ) -> bool {
        self.core
            .registry
            .get_participant(participant)
            .is_some_and(|meta| meta.shard_id == shard && meta.binding == Some(binding))
    }

    fn publish_staged_views(&mut self) -> bool {
        let mut published = false;
        for view in &mut self.views {
            if !view.has_staged() {
                continue;
            }
            if view.publish().is_some() {
                published = true;
            } else if published {
                pulsebeam_runtime::fatal!(
                    "a shard view closed after another shard accepted the same generation"
                );
            } else {
                return false;
            }
        }
        true
    }

    pub(crate) async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEventMessage>,
        shutdown: CancellationToken,
    ) {
        // Spawn the TCP acceptor onto the current LocalSet / LocalRuntime.
        // It owns the listener, enforces caps, reads the first STUN frame from
        // each connection, and sends results back through the mailbox.
        let Some(listener) = self.tcp_listener.take() else {
            pulsebeam_runtime::fatal!("ControllerActor::run called twice")
        };
        let acceptor = TcpAcceptorHandle::spawn(
            listener,
            crate::control::tcp_acceptor::TcpAcceptorConfig {
                cluster_id: self.cluster_id,
                node_id: self.node_id,
                shard_count: self.router.shard_count(),
            },
            shutdown.child_token(),
        );
        let mut pending_rx = acceptor.event_rx;

        let mut poll_interval = tokio::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_cmd_at = tokio::time::Instant::now();
        #[cfg(not(feature = "unpace"))]
        let cooldown_duration = SHARD_LOAD_POLL_INTERVAL.checked_div(4).unwrap_or_default();
        #[cfg(feature = "unpace")]
        let cooldown_duration = std::time::Duration::from_secs(0);

        loop {
            #[cfg(feature = "sim")]
            crate::sim_metrics::wait_controller_stall().await;
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(e) = shard_event_rx.recv() => {
                    if let Some(e) = self.handle_route_event(e).await {
                        self.core.process_shard_event(e);
                    }
                }

                _ = self.core.next_expired() => {}

                _ = poll_interval.tick() => {
                    self.router.poll_loads();
                    self.retry_deferred_subscribes().await;
                    self.retry_deferred_streams().await;
                    self.retry_deferred_audio().await;
                }

                Some(ev) = pending_rx.recv() => {
                    if let Some(conn) = ev.result {
                        self.route_tcp_connection(conn);
                    }
                }

                Some(cmd) = recv_command_paced(&mut command_rx, next_cmd_at) => {
                    let is_join = matches!(cmd, ControllerCommand::CreateParticipant(_, _));

                    self.process_command(cmd).await;

                    #[cfg(not(feature = "unpace"))]
                    if is_join {
                        let poll_from = tokio::time::Instant::now();
                    next_cmd_at = poll_from.checked_add(cooldown_duration).unwrap_or(poll_from);
                    }
                }

                _ = shutdown.cancelled() => {
                    break;
                }

                else => break,
            }

            self.drain_core_events().await;
            for view in &mut self.views {
                let _ = view.flush_backlog();
            }
        }
    }

    pub async fn process_command(&mut self, cmd: ControllerCommand) {
        match cmd {
            ControllerCommand::CreateParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(m.state, m.offer)
                    .await
                    .map(|res| CreateParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }

            ControllerCommand::DeleteParticipant(m) => {
                self.retire_participant_tracks(&m.participant_id).await;
                self.retire_participant_streams(&m.participant_id).await;
                self.retire_participant_subscriptions(&m.participant_id)
                    .await;
                self.retire_participant_transport(&m.participant_id).await;
                self.core.delete_participant(&m.participant_id);
            }
            ControllerCommand::PatchParticipant(m, reply_tx) => {
                let answer = self
                    .handle_patch_participant(m.state, m.offer)
                    .await
                    .map(|res| PatchParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }
        }
    }

    async fn drain_core_events(&mut self) {
        while let Some(ev) = self.eq.pop() {
            match ev {
                ControllerEvent::ShardCommandSent(shard_id, cmd) => {
                    match self.router.try_send(shard_id, cmd) {
                        Ok(()) => {}
                        Err(error) => match *error {
                            pulsebeam_runtime::mailbox::TrySendError::Full(cmd) => {
                                self.eq
                                    .push(ControllerEvent::ShardCommandSent(shard_id, cmd));
                                break;
                            }
                            pulsebeam_runtime::mailbox::TrySendError::Closed(_) => {
                                pulsebeam_runtime::fatal!(
                                    "a shard command mailbox closed before its command was delivered"
                                );
                            }
                        },
                    }
                }
            }
        }
    }

    pub async fn handle_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let current = self
            .core
            .registry
            .get_participant(&state.participant_id)
            .and_then(|meta| meta.connection_id);
        if current != state.old_connection_id {
            return Err(ControllerError::StaleConnection);
        }
        self.retire_participant_tracks(&state.participant_id).await;
        self.retire_participant_streams(&state.participant_id).await;
        self.retire_participant_subscriptions(&state.participant_id)
            .await;
        self.retire_participant_transport(&state.participant_id)
            .await;
        self.core.delete_participant(&state.participant_id);
        self.handle_create_participant(state, offer).await
    }

    /// Route an accepted TCP connection to the shard that owns its participant.
    ///
    /// The acceptor has already decoded the ufrag and validated cluster,
    /// node and shard bounds, so this is the handoff and nothing else: one
    /// accepted stream, sent once to the shard its transport route names.
    /// After this the shard owns the connection permanently — there is no
    /// path back through here for a subsequent packet.
    fn route_tcp_connection(&mut self, conn: PendingTcpConn) {
        debug_assert!(
            conn.handle.shard().index() < self.router.shard_count(),
            "the acceptor must reject a shard outside the node before handing off"
        );
        self.eq.send(
            conn.handle.shard(),
            ShardCommand::AdoptTcpConnection {
                stream: conn.stream,
                peer_addr: conn.peer_addr,
            },
        );
    }

    /// Buggify's exhaustion fault fires independently per call, so a few
    /// attempts clear a transient one almost certainly; a genuinely full
    /// namespace fails every attempt just as fast, so the retry costs nothing
    /// in that case either.
    const ROUTE_ALLOCATION_ATTEMPTS: u32 = 10;

    /// Reserve an endpoint route, retrying a transient shortage.
    ///
    /// Exhaustion is either momentary — a slot is in quarantine, or fault
    /// injection fired — or total, and the two need opposite responses. A
    /// bounded retry gets the first right at no cost to the second: a
    /// genuinely full namespace fails every attempt without blocking.
    fn reserve_endpoint_retrying(
        &mut self,
        shard_id: crate::id::ShardId,
        now: tokio::time::Instant,
        family: &'static str,
    ) -> Option<RouteHandle> {
        for attempt in 1..=Self::ROUTE_ALLOCATION_ATTEMPTS {
            match self.state.reserve_endpoint(shard_id, now) {
                Ok(handle) => return Some(handle),
                Err(err) => {
                    tracing::warn!(
                        %shard_id,
                        ?err,
                        attempt,
                        family,
                        "endpoint route allocation failed, retrying"
                    );
                }
            }
        }
        metrics::counter!("route_allocation_failed", "family" => family).increment(1);
        None
    }

    /// Publish one generation of view changes, all-or-nothing.
    ///
    /// Every lifecycle change has this shape — open a generation, stage ops
    /// against the shards it touches, publish each affected view once, commit —
    /// and every copy of it had to unwind by hand on each of the four ways it
    /// can fail. Writing it once is what makes "one publish per shard per
    /// generation" checkable by reading one function.
    ///
    /// `reserve` runs inside the generation because a route may only be
    /// allocated against an open transaction; it hands its handle to `ops`,
    /// which is the only reason the two are not separate calls.
    fn transact<T>(
        &mut self,
        reserve: impl FnOnce(&mut Self, tokio::time::Instant) -> Option<T>,
        ops: impl FnOnce(&Self, &T) -> GenerationOps,
    ) -> Option<T> {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }
        let Some(reserved) = reserve(self, now) else {
            self.abort_transaction(now);
            return None;
        };
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            self.abort_transaction(now);
            return None;
        };
        let generation_ops = ops(self, &reserved);
        for (shard_id, participant, effect) in generation_ops.participant_effects {
            let Some(view) = self.view_mut(shard_id) else {
                self.abort_transaction(now);
                return None;
            };
            view.stage_participant_effect(generation, participant, effect);
        }
        for (shard_id, op) in generation_ops.lifecycle {
            if !op.is_owned_by(shard_id) {
                debug_assert!(false, "a view op must target its owning shard");
                self.abort_transaction(now);
                return None;
            }
            let Some(view) = self.view_mut(shard_id) else {
                self.abort_transaction(now);
                return None;
            };
            view.stage(generation, op);
        }
        let mut staged_plans: HashMap<
            (crate::id::ShardId, crate::plan::PlanKey),
            Option<crate::plan::FlatTrackPlan>,
        > = HashMap::new();
        let mut batches = (0..self.views.len())
            .map(|_| crate::plan::PlanBatch::default())
            .collect::<Vec<_>>();
        for request in generation_ops.plans {
            let Some(shard_plans) = self.compiled_plans.get(request.shard.index()) else {
                debug_assert!(false, "a plan request must target a live shard");
                self.abort_transaction(now);
                return None;
            };
            let old = match staged_plans.get(&(request.shard, request.key)) {
                Some(Some(plan)) => Some(plan),
                Some(None) => None,
                None => shard_plans.get(&request.key),
            };
            let change = crate::plan::PlanChange::between(request.key, old, request.plan.as_ref());
            if change.remove && old.is_none() {
                debug_assert!(false, "a plan cannot be removed before it exists");
                continue;
            }
            if !change.is_empty() {
                let Some(batch) = batches.get_mut(request.shard.index()) else {
                    debug_assert!(false, "a plan request must target a live shard");
                    self.abort_transaction(now);
                    return None;
                };
                batch.push(change);
            }
            staged_plans.insert((request.shard, request.key), request.plan);
        }
        for (index, batch) in batches.into_iter().enumerate() {
            if !batch.is_empty() {
                let Some(view) = self.views.get_mut(index) else {
                    debug_assert!(false, "a compiled plan must target a live view");
                    self.abort_transaction(now);
                    return None;
                };
                view.stage_plans(generation, batch);
            }
        }
        // A shard this generation staged ops for must accept them. An empty
        // delta cannot happen there, so a publish that yields nothing means the
        // shard is gone — and committing anyway would retire the slot of a
        // route the surviving shards still believe in.
        if !self.publish_staged_views() {
            self.abort_transaction(now);
            return None;
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        for ((shard, key), plan) in staged_plans {
            let Some(shard_plans) = self.compiled_plans.get_mut(shard.index()) else {
                debug_assert!(false, "a committed plan must target a live shard");
                continue;
            };
            match plan {
                Some(plan) => {
                    shard_plans.insert(key, plan);
                }
                None => {
                    debug_assert!(shard_plans.remove(&key).is_some());
                }
            }
        }
        Some(reserved)
    }

    /// [`transact`](Self::transact) for a change that allocates nothing.
    fn publish_ops(&mut self, ops: Vec<(crate::id::ShardId, crate::view::ViewOp)>) {
        if ops.is_empty() {
            return;
        }
        self.publish_generation(GenerationOps::lifecycle(ops));
    }

    fn publish_generation(&mut self, generation_ops: GenerationOps) {
        if generation_ops.participant_effects.is_empty()
            && generation_ops.lifecycle.is_empty()
            && generation_ops.plans.is_empty()
        {
            return;
        }
        if self
            .transact(|_, _| Some(()), move |_, ()| generation_ops)
            .is_none()
        {
            pulsebeam_runtime::fatal!(
                "a control-plane view update could not be accepted by every owning shard"
            );
        }
    }

    /// [`transact`](Self::transact) for a change that mints one endpoint route.
    fn publish_with_route(
        &mut self,
        shard_id: crate::id::ShardId,
        family: &'static str,
        ops: impl FnOnce(&Self, &RouteHandle) -> GenerationOps,
    ) -> Option<RouteHandle> {
        self.transact(
            move |actor, now| actor.reserve_endpoint_retrying(shard_id, now, family),
            ops,
        )
    }

    fn defer_subscribe(&mut self, deferred: PendingSubscription) {
        metrics::counter!("subscribe_deferred").increment(1);
        if !self.pending.hold_route(deferred) {
            metrics::counter!("subscribe_deferred_dropped").increment(1);
        }
    }

    /// Retry every deferred subscription once.
    ///
    /// One pass, not a loop until success: a retry that fails is re-deferred
    /// by the same path that deferred it originally, so the next tick picks it
    /// up. Draining the queue into a local first is what keeps that from
    /// spinning within one tick.
    async fn retry_deferred_subscribes(&mut self) {
        let pending = self.pending.take_route_retries();
        for deferred in pending {
            let PendingSubscription {
                shard_id,
                subscriber,
                subscriber_key,
                slot,
                track,
                ..
            } = deferred;
            // A participant that left, or a track that was retired, takes its
            // deferred work with it rather than being resurrected here.
            if self.core.registry.get_participant(&subscriber).is_none()
                || !self.catalog.contains(&track.id)
            {
                continue;
            }
            self.on_track_subscribed(shard_id, subscriber, subscriber_key, slot, track)
                .await;
        }
    }

    fn defer_stream(
        &mut self,
        shard_id: crate::id::ShardId,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) {
        self.pending_streams
            .hold(PendingStream { shard_id, id, lane });
    }

    async fn retry_deferred_streams(&mut self) {
        let pending = self.pending_streams.take();
        for PendingStream { shard_id, id, lane } in pending {
            if self
                .core
                .registry
                .get_participant(&id.publisher_id)
                .is_none()
            {
                continue;
            }
            if !self.on_stream_ready(shard_id, id.clone(), lane).await {
                self.defer_stream(shard_id, id, lane);
            }
        }
    }

    fn defer_audio(&mut self, track_id: crate::entity::TrackId) {
        if !self.pending_audio.contains(&track_id) {
            self.pending_audio.push(track_id);
        }
    }

    async fn retry_deferred_audio(&mut self) {
        let pending = std::mem::take(&mut self.pending_audio);
        for track_id in pending {
            if self
                .catalog
                .get(&track_id)
                .is_some_and(|p| p.kind() == crate::entity::TrackKind::Audio)
            {
                self.install_audio_routes(track_id).await;
            }
        }
    }

    /// Route lifecycle, as its own generation.
    ///
    /// Returns the event untouched when it is not a route event, so the
    /// ordinary topology projection still sees everything else. Route work is
    /// separated because it updates the canonical bindings and view deltas
    /// before the ordinary registry projection observes the resulting fact.
    async fn handle_route_event(&mut self, e: ShardEventMessage) -> Option<ShardEventMessage> {
        let (shard_id, event) = e;
        match event {
            ShardEvent::TransportAuthenticated {
                source,
                destination,
                source_shard,
                handle,
                shard,
            } => {
                debug_assert_eq!(shard, shard_id);
                debug_assert_eq!(handle.shard(), shard);
                let Ok(shard_index) = u16::try_from(shard.index()) else {
                    debug_assert!(false, "a shard id must fit the steering map value");
                    return None;
                };
                self.pin_flow_to_owner(source, destination, shard_index);
                let (source_shard, command) =
                    source_authentication_command(source_shard, source, handle);
                self.eq.send(source_shard, command);
                None
            }
            ShardEvent::TrackSubscribed {
                subscriber,
                subscriber_key,
                slot,
                track,
            } => {
                if !self.catalog.contains(&track.id) {
                    // Track ids come from the client, so a subscription may
                    // name something that does not exist and never will. Cap
                    // how many a single participant can park here; the whole
                    // list is dropped when it disconnects.
                    let pending =
                        PendingSubscription::new(shard_id, subscriber, subscriber_key, slot, track);
                    if !self.pending.hold_publication(pending) {
                        metrics::counter!("pending_subscription_rejected").increment(1);
                        return None;
                    }
                    return None;
                }
                self.on_track_subscribed(shard_id, subscriber, subscriber_key, slot, track)
                    .await;
                None
            }
            ShardEvent::TrackUnsubscribed {
                subscriber,
                slot,
                track,
            } => {
                self.remove_pending_track_subscription(track.id, subscriber, slot);
                self.on_track_unsubscribed(shard_id, subscriber, track)
                    .await;
                None
            }
            ShardEvent::DataTopicPublished {
                room_id,
                publisher,
                publisher_key,
                topic,
            } => {
                if !self.is_current_binding(&publisher, shard_id, publisher_key) {
                    return None;
                }
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                if !self
                    .on_stream_ready(shard_id, id.clone(), StreamLane::Unreliable)
                    .await
                {
                    self.defer_stream(shard_id, id, StreamLane::Unreliable);
                }
                None
            }
            ShardEvent::ReliableDataTopicPublished {
                room_id,
                publisher,
                publisher_key,
                topic,
            } => {
                if !self.is_current_binding(&publisher, shard_id, publisher_key) {
                    return None;
                }
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                if !self
                    .on_stream_ready(shard_id, id.clone(), StreamLane::Reliable)
                    .await
                {
                    self.defer_stream(shard_id, id, StreamLane::Reliable);
                }
                None
            }
            ShardEvent::DataTopicSubscribed {
                room_id,
                subscriber,
                topic,
                publisher,
                channel,
            } => {
                let Some(subscriber_key) = self
                    .core
                    .registry
                    .get_participant(&subscriber)
                    .and_then(|meta| meta.binding)
                else {
                    debug_assert!(false, "a data subscription must name a participant key");
                    return None;
                };
                self.on_stream_subscription(
                    shard_id,
                    room_id,
                    subscriber,
                    subscriber_key,
                    topic,
                    publisher,
                    channel,
                    StreamLane::Unreliable,
                )
                .await;
                None
            }
            ShardEvent::ReliableDataTopicSubscribed {
                room_id,
                subscriber,
                topic,
                channel,
            } => {
                let Some(subscriber_key) = self
                    .core
                    .registry
                    .get_participant(&subscriber)
                    .and_then(|meta| meta.binding)
                else {
                    debug_assert!(false, "a reliable subscription must name a participant key");
                    return None;
                };
                self.on_stream_subscription(
                    shard_id,
                    room_id,
                    subscriber,
                    subscriber_key,
                    topic,
                    None,
                    channel,
                    StreamLane::Reliable,
                )
                .await;
                None
            }
            ShardEvent::DataTopicUnsubscribed {
                room_id,
                subscriber,
                topic,
                publisher,
            } => {
                self.on_stream_unsubscription(
                    room_id,
                    subscriber,
                    topic,
                    publisher,
                    StreamLane::Unreliable,
                )
                .await;
                None
            }
            ShardEvent::ReliableDataTopicUnsubscribed {
                room_id,
                subscriber,
                topic,
            } => {
                self.on_stream_unsubscription(
                    room_id,
                    subscriber,
                    topic,
                    None,
                    StreamLane::Reliable,
                )
                .await;
                None
            }
            ShardEvent::DataTopicUnpublished {
                room_id,
                publisher,
                publisher_key,
                topic,
            } => {
                if !self.is_current_binding(&publisher, shard_id, publisher_key) {
                    return None;
                }
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                self.pending_streams.remove(&id, StreamLane::Unreliable);
                if !self.retire_stream_binding(id, StreamLane::Unreliable).await {
                    debug_assert!(false, "data stream retirement must complete");
                }
                None
            }
            ShardEvent::ReliableDataTopicUnpublished {
                room_id,
                publisher,
                publisher_key,
                topic,
            } => {
                if !self.is_current_binding(&publisher, shard_id, publisher_key) {
                    return None;
                }
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                self.pending_streams.remove(&id, StreamLane::Reliable);
                if !self.retire_stream_binding(id, StreamLane::Reliable).await {
                    debug_assert!(false, "reliable stream retirement must complete");
                }
                None
            }
            ShardEvent::ParticipantClosed {
                participant: participant_id,
                key,
            } => {
                let current = self
                    .core
                    .registry
                    .get_participant(&participant_id)
                    .and_then(|meta| meta.binding);
                if current != Some(key) {
                    return None;
                }
                self.retire_participant_tracks(&participant_id).await;
                self.retire_participant_streams(&participant_id).await;
                self.retire_participant_subscriptions(&participant_id).await;
                self.retire_participant_transport(&participant_id).await;
                #[cfg(feature = "sim")]
                if self.state.arenas.iter().any(|arena| {
                    arena
                        .participants
                        .values()
                        .any(|record| record.id == participant_id)
                }) {
                    crate::sim_metrics::record_routing_counter("materialization_orphan");
                }
                Some((
                    shard_id,
                    ShardEvent::ParticipantClosed {
                        participant: participant_id,
                        key,
                    },
                ))
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                if !self.retire_track_binding(track_id).await {
                    debug_assert!(false, "track route retirement must complete");
                }
                Some((shard_id, ShardEvent::TrackUnpublished { origin, track_id }))
            }
            ShardEvent::TrackPublished { track, states } => {
                let track_id = track.meta.id;
                if self.catalog.contains(&track_id) {
                    debug_assert!(false, "a publication must be announced once");
                    return None;
                }
                let Some(publisher_key) = self
                    .core
                    .registry
                    .get_participant(&track.meta.origin)
                    .and_then(|meta| meta.binding)
                else {
                    debug_assert!(false, "a published track must have a participant key");
                    return None;
                };
                let fanout = self.prepare_track_key(shard_id, track_id, track.meta.origin)?;
                let track_id = track.meta.id;
                let origin_key = match track_id.kind() {
                    crate::entity::TrackKind::Video => {
                        crate::control::publication::RuntimeKey::Video(
                            crate::keys::VideoTrackKey::new(fanout),
                        )
                    }
                    crate::entity::TrackKind::Audio => {
                        crate::control::publication::RuntimeKey::Audio(
                            crate::keys::AudioTrackKey::new(fanout),
                        )
                    }
                    crate::entity::TrackKind::Data => {
                        debug_assert!(false, "data does not publish through the track path");
                        self.state.remove_track(shard_id, fanout);
                        return None;
                    }
                };
                self.catalog
                    .insert(crate::control::publication::Publication {
                        id: track_id,
                        room: track.meta.room_id,
                        publisher: track.meta.origin,
                        publisher_shard: shard_id,
                        publisher_key,
                        origin_key,
                        reverse_route: None,
                        destinations: indexmap::IndexMap::new(),
                        media: match track_id.kind() {
                            crate::entity::TrackKind::Audio => {
                                crate::control::publication::Media::Audio
                            }
                            _ => crate::control::publication::Media::Video {
                                publication: track.as_ref().clone(),
                                encodings: track.layers.iter().map(|layer| layer.rid).collect(),
                                states,
                            },
                        },
                    });
                self.index_publication(track_id);
                let announced = self.on_track_published(shard_id, *track, fanout).await;
                if let Some(track) = &announced {
                    if let Some(publication) = self.catalog.get_mut(&track_id) {
                        publication.reverse_route = track.reverse;
                        if let crate::control::publication::Media::Video {
                            publication: held, ..
                        } = &mut publication.media
                        {
                            *held = track.clone();
                        }
                    }
                } else {
                    self.catalog.remove(&track_id);
                    self.state.remove_track(shard_id, fanout);
                }
                if announced.is_some() {
                    // A track is one kind. Running both installers regardless
                    // granted every video track audio routes that nothing ever
                    // resolves - `route_audio_with_plan` is only reached for
                    // audio RTP - while consuming route slots on every shard
                    // with a listener.
                    match track_id.kind() {
                        crate::entity::TrackKind::Video => {
                            let Some(room_id) = self.catalog.get(&track_id).map(|p| p.room) else {
                                debug_assert!(false, "a published track must have a room");
                                return None;
                            };
                            self.install_video_runtimes_for_room(track_id, room_id)
                                .await;
                        }
                        crate::entity::TrackKind::Audio => {
                            self.install_audio_routes(track_id).await;
                        }
                        crate::entity::TrackKind::Data => {
                            debug_assert!(false, "data does not publish through the track path");
                        }
                    }
                    if !self.publish_publication(track_id).await {
                        debug_assert!(false, "initial track plan publication must complete");
                    }
                    self.drain_pending_track_subscriptions(track_id).await;
                }
                announced.map(|track| {
                    (
                        shard_id,
                        ShardEvent::TrackPublished {
                            track: Box::new(track),
                            states: Vec::new(),
                        },
                    )
                })
            }
        }
    }

    pub async fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let room_id = state.room_id;
        let participant_id = state.participant_id;
        // Determine shard first so we can encode it into the ICE ufrag.
        let (slot, placement) = self.core.room_slot(&state.room_id);
        let shard_id = match placement {
            RoomPlacement::Hashed => self
                .router
                .stable_route(&state.room_id)
                .ok_or(ControllerError::ServiceUnavailable)?,
            RoomPlacement::RoundRobin => {
                crate::id::ShardId::new(slot.checked_rem(self.router.shard_count()).unwrap_or(0))
            }
        };

        // The transport route is allocated and its view delta is queued before
        // negotiation. The shard applies it independently on its next tick.
        let Some((handle, binding)) = self
            .stage_transport(shard_id, state.participant_id, state.room_id)
            .await
        else {
            return Err(ControllerError::ServiceUnavailable);
        };
        debug_assert_eq!(
            handle.shard(),
            shard_id,
            "a route must carry the shard placement chose"
        );

        let ufrag = IceUfrag::new(self.cluster_id, self.node_id, handle.route, handle.epoch);
        let creds = ufrag.into_ice_creds();

        let negotiated = self.negotiator.create_answer(offer, creds);
        let (rtc, answer) = match negotiated {
            Ok(negotiated) => negotiated,
            Err(err) => {
                // The route is published but nothing will
                // ever populate it now. Retiring it is a generation of its
                // own — the route must be absent from the published view
                // before its slot can go back to the allocator — and the
                // shard still holds the key it reserved, so both have to be
                // unwound, not just one.
                self.retire_transport(
                    shard_id,
                    handle,
                    Some(binding),
                    Some((room_id, participant_id)),
                )
                .await;
                self.state.remove_participant(shard_id, binding);
                return Err(err.into());
            }
        };
        let cfg = self
            .core
            .create_participant(rtc, state, shard_id, Some(handle));
        self.core
            .registry
            .bind_participant(&cfg.participant_id, binding);
        let _membership = crate::control::patterns::declare_audience(
            &mut self.audio_patterns,
            crate::control::patterns::Pattern::all(room_id),
            cfg.participant_id,
            crate::control::patterns::Member {
                shard: shard_id,
                key: binding,
                delivery: (),
            },
        );

        let (materialized_tx, materialized_rx) = oneshot::channel();
        if self
            .router
            .send(
                shard_id,
                ShardCommand::MaterializeParticipant {
                    key: binding,
                    transport: handle,
                    config: Box::new(cfg),
                    ack: materialized_tx,
                },
            )
            .await
            .is_err()
        {
            let _ = crate::control::patterns::retract_participant(
                &mut self.audio_patterns,
                &participant_id,
            );
            self.retire_transport(
                shard_id,
                handle,
                Some(binding),
                Some((room_id, participant_id)),
            )
            .await;
            self.core.registry.remove_participant(&participant_id);
            self.state.remove_participant(shard_id, binding);
            return Err(ControllerError::ServiceUnavailable);
        }
        if !materialized_rx.await.unwrap_or(false) {
            let _ = crate::control::patterns::retract_participant(
                &mut self.audio_patterns,
                &participant_id,
            );
            self.retire_transport(
                shard_id,
                handle,
                Some(binding),
                Some((room_id, participant_id)),
            )
            .await;
            self.core.registry.remove_participant(&participant_id);
            self.state.remove_participant(shard_id, binding);
            return Ok(answer);
        }

        self.publish_participant_roster(room_id, participant_id, binding);

        let audio_tracks: Vec<_> = self
            .catalog
            .in_room(room_id, crate::entity::TrackKind::Audio)
            .collect();
        for track_id in audio_tracks {
            self.index_publication(track_id);
        }

        self.reconcile_room_audio(room_id, shard_id).await;

        self.reconcile_room_tracks(room_id, shard_id).await;

        Ok(answer)
    }

    async fn reconcile_room_tracks(
        &mut self,
        room_id: crate::entity::RoomId,
        destination: crate::id::ShardId,
    ) {
        let tracks: Vec<_> = self
            .catalog
            .in_room(room_id, crate::entity::TrackKind::Video)
            .collect();
        for track_id in tracks {
            self.install_video_runtime(track_id, destination).await;
            if !self.publish_publication_to(track_id, destination).await {
                debug_assert!(false, "room track reconciliation must publish");
            }
        }
    }

    /// Give a shard routes to the room's audio, for the case its first member
    /// of the audience just arrived. Every later joiner on that shard is served
    /// by the routes this installed, and costs one membership op.
    async fn reconcile_room_audio(
        &mut self,
        room_id: crate::entity::RoomId,
        destination: crate::id::ShardId,
    ) {
        let group = self
            .audio_patterns
            .group_of(&crate::control::patterns::Pattern::all(room_id));
        let tracks: Vec<_> = group
            .into_iter()
            .flat_map(|group| self.audio_patterns.publications_of(group))
            .collect();
        for track_id in tracks {
            if !self.install_audio_destination(track_id, destination).await {
                self.defer_audio(track_id);
                continue;
            }
            if !self.publish_publication_to(track_id, destination).await {
                debug_assert!(false, "room audio reconciliation must publish");
                continue;
            }
            let Some(publisher_shard) = self.catalog.get(&track_id).map(|p| p.publisher_shard)
            else {
                debug_assert!(false, "room audio track must belong to a publication");
                continue;
            };
            if !self.publish_plan_to(track_id, publisher_shard) {
                debug_assert!(false, "room audio publisher plan must publish");
            }
        }
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;

async fn recv_command_paced(
    rx: &mut mailbox::Receiver<ControllerCommand>,
    allowed_at: tokio::time::Instant,
) -> Option<ControllerCommand> {
    tokio::time::sleep_until(allowed_at).await;
    rx.recv().await
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::id::ShardId;
    use crate::{
        control::tcp_acceptor::PendingTcpConn,
        shard::{ShardContext, metrics::ShardMetrics},
    };
    use pulsebeam_core::net::TcpListener;
    use pulsebeam_runtime::{mailbox, net::tcp::BufferedTcpStream, rand::seeded_rng};
    use std::{net::IpAddr, sync::Arc};

    fn run_local<Fut>(test: Fut)
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        pulsebeam_runtime::testing::run_local(test_host_ip(), test);
    }

    fn test_host_ip() -> IpAddr {
        pulsebeam_runtime::testing::test_host_ip("192.168.250.10")
    }

    fn dummy_route(shard: usize) -> crate::route::TransportRoute {
        crate::route::TransportRoute::new(ShardId::new(shard), 0)
    }

    async fn make_actor(num_shards: usize) -> ControllerActor {
        let listener = TcpListener::bind(std::net::SocketAddr::new(test_host_ip(), 0))
            .await
            .unwrap();
        let shard_contexts: Vec<ShardContext> = (0..num_shards)
            .map(|_| {
                let (tx, _rx) = mailbox::new(128);
                ShardContext {
                    command_tx: tx,
                    metrics: Arc::new(ShardMetrics::new()),
                }
            })
            .collect();
        let views = (0..num_shards)
            .map(|idx| crate::view::new_shard_view(ShardId::new(idx)).0)
            .collect();
        ControllerActor::with_placement(
            seeded_rng(42),
            shard_contexts,
            vec![],
            listener,
            crate::control::core::DEFAULT_ROOM_SHARD_SLOT,
            RoomPlacement::Hashed,
            views,
        )
    }

    /// Accept one server-side TCP stream from a fresh loopback listener.
    async fn accept_one() -> (
        pulsebeam_core::net::TcpStream,
        pulsebeam_core::net::TcpStream,
        std::net::SocketAddr,
    ) {
        let listener = TcpListener::bind(std::net::SocketAddr::new(test_host_ip(), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(
            pulsebeam_core::net::TcpStream::connect(addr),
            listener.accept()
        );
        let client = client.unwrap();
        let (server, peer_addr) = accepted.unwrap();
        (client, server, peer_addr)
    }

    /// Wrap a raw server-side stream as a `BufferedTcpStream` for route tests.
    async fn make_buffered() -> (pulsebeam_core::net::TcpStream, BufferedTcpStream) {
        let (_client, server, _peer) = accept_one().await;
        (_client, BufferedTcpStream::new(server))
    }

    // ── route_tcp_connection ─────────────────────────────────────────────────

    /// Cluster, node and shard-bounds validation now happens in the acceptor,
    /// which has its own tests for it. What is left to check here is that the
    /// controller hands the connection to the shard the transport route names
    /// and does nothing else with it.
    #[test]
    fn a_validated_connection_is_handed_to_the_shard_its_route_names() {
        run_local(async {
            let mut actor = make_actor(3).await;
            let (_client, stream) = make_buffered().await;
            let peer_addr = "1.2.3.4:5000".parse().unwrap();

            let conn = PendingTcpConn {
                stream,
                peer_addr,
                handle: crate::route::TransportHandle::new(dummy_route(2), 0),
            };
            actor.route_tcp_connection(conn);

            let event = actor.eq.pop().expect("event must be queued");
            match event {
                ControllerEvent::ShardCommandSent(
                    shard_id,
                    ShardCommand::AdoptTcpConnection { peer_addr: pa, .. },
                ) => {
                    assert_eq!(shard_id, ShardId::new(2));
                    assert_eq!(pa, peer_addr);
                }
                _ => panic!("unexpected event: {event:?}"),
            }

            assert!(
                actor.eq.pop().is_none(),
                "a connection is handed off once, not repeatedly"
            );
        });
    }

    #[test]
    fn authentication_acknowledgment_targets_the_tuple_hash_shard() {
        let source_shard = ShardId::new(0);
        let owner = ShardId::new(2);
        let handle = crate::route::TransportHandle::new(dummy_route(owner.index()), 7);
        let source = "203.0.113.7:40000".parse().unwrap();

        let (shard, command) = source_authentication_command(source_shard, source, handle);
        assert_eq!(shard, source_shard);
        match command {
            ShardCommand::AuthenticateTransport {
                source: acknowledged_source,
                handle: acknowledged_handle,
            } => {
                assert_eq!(acknowledged_source, source);
                assert_eq!(acknowledged_handle, handle);
            }
            _ => panic!("unexpected source authentication command"),
        }
    }
}
