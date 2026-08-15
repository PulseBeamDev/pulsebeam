use std::io;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};

use crate::control::state::ControlPlaneState;
use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue, RoomPlacement},
        negotiator::{Negotiator, NegotiatorError},
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

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

struct TrackBinding {
    meta: crate::track::TrackMeta,
    publication: crate::track::Track,
    publisher_participant: crate::shard::participants::ParticipantKey,
    encodings: Vec<Option<str0m::media::Rid>>,
    states: crate::track::TrackStates,
    publisher_shard: crate::id::ShardId,
    publisher_fanout: crate::shard::router::TrackKey,
    reverse_route: Option<RouteHandle>,
    fanouts: HashMap<crate::id::ShardId, crate::shard::router::TrackKey>,
    audio_fanouts: HashMap<crate::id::ShardId, crate::shard::router::TrackKey>,
    audio_routes: HashMap<crate::id::ShardId, RouteHandle>,
}

/// How many not-yet-published tracks one participant may have outstanding.
///
/// Generous against any real client — a subscription for a track that has not
/// been announced yet is a race, not a workflow — and small enough that the
/// list cannot be inflated by a client naming ids that will never exist.
const MAX_PENDING_SUBSCRIPTIONS_PER_PARTICIPANT: usize = 64;

struct PendingTrackSubscription {
    shard_id: crate::id::ShardId,
    subscriber: ParticipantId,
    subscriber_key: crate::shard::participants::ParticipantKey,
    slot: crate::keys::DownstreamSlotKey,
    track: crate::track::TrackMeta,
}

struct StreamBinding {
    publisher_shard: crate::id::ShardId,
    publisher: crate::shard::participants::ParticipantKey,
    key: Option<crate::shard::router::RuntimeStreamKey>,
    reverse_route: Option<RouteHandle>,
    subscribers: HashMap<
        crate::id::ShardId,
        HashMap<
            ParticipantId,
            (
                crate::shard::participants::ParticipantKey,
                str0m::channel::ChannelId,
            ),
        >,
    >,
    destination_keys: HashMap<crate::id::ShardId, crate::shard::router::RuntimeStreamKey>,
    routes: HashMap<crate::id::ShardId, RouteHandle>,
}

#[derive(Clone, Copy)]
enum StreamLane {
    Data,
    Reliable,
}

impl StreamBinding {
    fn new(
        publisher_shard: crate::id::ShardId,
        publisher: crate::shard::participants::ParticipantKey,
    ) -> Self {
        Self {
            publisher_shard,
            publisher,
            key: None,
            reverse_route: None,
            subscribers: HashMap::new(),
            destination_keys: HashMap::new(),
            routes: HashMap::new(),
        }
    }

    fn add_subscriber(
        &mut self,
        shard: crate::id::ShardId,
        participant: ParticipantId,
        key: crate::shard::participants::ParticipantKey,
        channel: str0m::channel::ChannelId,
    ) {
        self.subscribers
            .entry(shard)
            .or_default()
            .insert(participant, (key, channel));
    }

    fn remove_subscriber(&mut self, participant: &ParticipantId) -> bool {
        let mut removed = false;
        self.subscribers.retain(|_, subscribers| {
            removed |= subscribers.remove(participant).is_some();
            !subscribers.is_empty()
        });
        removed
    }
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

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    /// Who consumes what, and therefore which routes exist. The decision the
    /// shard used to make by counting its own subscribers.
    subscriptions: crate::control::subscriptions::TrackSubscriptions,
    /// One writer per shard. Never shared, never locked, and never handed to
    /// a shard: the one-publish-per-generation budget is only checkable
    /// because there is exactly one caller.
    views: Vec<crate::view::ShardViewWriter>,
    track_bindings: HashMap<crate::entity::TrackId, TrackBinding>,
    data_bindings: HashMap<crate::shard::router::DataStreamId, StreamBinding>,
    reliable_bindings: HashMap<crate::shard::router::DataStreamId, StreamBinding>,
    pending_track_subscriptions: HashMap<crate::entity::TrackId, Vec<PendingTrackSubscription>>,
    pending_track_counts: HashMap<ParticipantId, usize>,
    data_pending: HashMap<
        crate::shard::router::DataStreamId,
        HashMap<
            crate::id::ShardId,
            HashMap<
                ParticipantId,
                (
                    crate::shard::participants::ParticipantKey,
                    str0m::channel::ChannelId,
                ),
            >,
        >,
    >,
    reliable_pending: HashMap<
        crate::shard::router::DataStreamId,
        HashMap<
            crate::id::ShardId,
            HashMap<
                ParticipantId,
                (
                    crate::shard::participants::ParticipantKey,
                    str0m::channel::ChannelId,
                ),
            >,
        >,
    >,
    data_wildcards: HashMap<
        (RoomId, crate::track::Topic),
        HashMap<
            ParticipantId,
            (
                crate::id::ShardId,
                crate::shard::participants::ParticipantKey,
                str0m::channel::ChannelId,
            ),
        >,
    >,
    reliable_wildcards: HashMap<
        (RoomId, crate::track::Topic),
        HashMap<
            ParticipantId,
            (
                crate::id::ShardId,
                crate::shard::participants::ParticipantKey,
                str0m::channel::ChannelId,
            ),
        >,
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
            subscriptions: crate::control::subscriptions::TrackSubscriptions::new(),
            views,
            track_bindings: HashMap::new(),
            data_bindings: HashMap::new(),
            reliable_bindings: HashMap::new(),
            pending_track_subscriptions: HashMap::new(),
            pending_track_counts: HashMap::new(),
            data_pending: HashMap::new(),
            reliable_pending: HashMap::new(),
            data_wildcards: HashMap::new(),
            reliable_wildcards: HashMap::new(),
            #[cfg(not(feature = "sim"))]
            steering: None,
        }
    }

    #[cfg(not(feature = "sim"))]
    pub(crate) fn set_steering(&mut self, steering: crate::ebpf::Steering) {
        debug_assert!(self.steering.is_none());
        self.steering = Some(steering);
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
                                tracing::warn!(%shard_id, "shard command mailbox is closed");
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

    fn stream_bindings(
        &self,
        lane: StreamLane,
    ) -> &HashMap<crate::shard::router::DataStreamId, StreamBinding> {
        match lane {
            StreamLane::Data => &self.data_bindings,
            StreamLane::Reliable => &self.reliable_bindings,
        }
    }

    fn stream_bindings_mut(
        &mut self,
        lane: StreamLane,
    ) -> &mut HashMap<crate::shard::router::DataStreamId, StreamBinding> {
        match lane {
            StreamLane::Data => &mut self.data_bindings,
            StreamLane::Reliable => &mut self.reliable_bindings,
        }
    }

    fn stream_pending_mut(
        &mut self,
        lane: StreamLane,
    ) -> &mut HashMap<
        crate::shard::router::DataStreamId,
        HashMap<
            crate::id::ShardId,
            HashMap<
                ParticipantId,
                (
                    crate::shard::participants::ParticipantKey,
                    str0m::channel::ChannelId,
                ),
            >,
        >,
    > {
        match lane {
            StreamLane::Data => &mut self.data_pending,
            StreamLane::Reliable => &mut self.reliable_pending,
        }
    }

    fn stream_plan(
        &self,
        binding: &StreamBinding,
        destination: crate::id::ShardId,
    ) -> crate::view::StreamForwardingPlan {
        let local_subscribers = binding
            .subscribers
            .get(&destination)
            .map(|subscribers| subscribers.values().copied().collect())
            .unwrap_or_default();
        let remote_routes = if destination == binding.publisher_shard {
            binding
                .routes
                .iter()
                .map(|(shard_id, handle)| crate::view::RemoteRoutePlan {
                    shard_id: *shard_id,
                    route: handle.route,
                    epoch: handle.epoch,
                })
                .collect()
        } else {
            Vec::new()
        };
        let reverse_route = binding
            .reverse_route
            .map(|handle| crate::view::RemoteRoutePlan {
                shard_id: binding.publisher_shard,
                route: handle.route,
                epoch: handle.epoch,
            });
        crate::view::StreamForwardingPlan {
            local_subscribers,
            remote_routes,
            reverse_route,
        }
    }

    fn stream_action(
        lane: StreamLane,
        key: crate::shard::router::RuntimeStreamKey,
    ) -> Option<RouteAction> {
        match (lane, key) {
            (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(stream)) => {
                Some(RouteAction::Data { stream })
            }
            (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(stream)) => {
                Some(RouteAction::Reliable { stream })
            }
            _ => None,
        }
    }

    fn prepare_stream_key(
        &mut self,
        destination: crate::id::ShardId,
        id: &crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> Option<crate::shard::router::RuntimeStreamKey> {
        match lane {
            StreamLane::Data => self
                .state
                .mint_data(destination, id.clone())
                .map(crate::shard::router::RuntimeStreamKey::Data),
            StreamLane::Reliable => self
                .state
                .mint_reliable(destination, id.clone())
                .map(crate::shard::router::RuntimeStreamKey::Reliable),
        }
    }

    fn retire_stream_runtime(
        &mut self,
        destination: crate::id::ShardId,
        key: crate::shard::router::RuntimeStreamKey,
        lane: StreamLane,
    ) {
        match (lane, key) {
            (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                self.state.remove_data(destination, key);
            }
            (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                self.state.remove_reliable(destination, key);
            }
            _ => debug_assert!(false, "stream key and lane disagree"),
        }
    }

    async fn on_stream_ready(
        &mut self,
        shard_id: crate::id::ShardId,
        id: crate::shard::router::DataStreamId,
        key: crate::shard::router::RuntimeStreamKey,
    ) {
        let lane = match key {
            crate::shard::router::RuntimeStreamKey::Data(_) => StreamLane::Data,
            crate::shard::router::RuntimeStreamKey::Reliable(_) => StreamLane::Reliable,
        };
        let pending = self.stream_pending_mut(lane).remove(&id);
        let Some(publisher) = self
            .core
            .registry
            .get_participant(&id.publisher_id)
            .and_then(|meta| meta.binding)
        else {
            debug_assert!(false, "a stream publisher must have a participant key");
            return;
        };
        let binding = self
            .stream_bindings_mut(lane)
            .entry(id.clone())
            .or_insert_with(|| StreamBinding::new(shard_id, publisher));
        debug_assert_eq!(binding.publisher_shard, shard_id);
        binding.key = Some(key);
        if let Some(pending) = pending {
            for (destination, subscribers) in pending {
                for (participant, (participant_key, channel)) in subscribers {
                    binding.add_subscriber(destination, participant, participant_key, channel);
                }
            }
        }

        if matches!(lane, StreamLane::Reliable) && binding.reverse_route.is_none() {
            let Some(stream) = (match key {
                crate::shard::router::RuntimeStreamKey::Reliable(stream) => Some(stream),
                _ => None,
            }) else {
                debug_assert!(false, "reliable readiness must carry a reliable key");
                return;
            };
            let Some(route) = self
                .grant_route(
                    shard_id,
                    RouteAction::Reverse {
                        target: ReverseTarget::Topic { stream },
                    },
                )
                .await
            else {
                return;
            };
            let Some(binding) = self.stream_bindings_mut(lane).get_mut(&id) else {
                debug_assert!(false, "stream binding must survive reverse route install");
                return;
            };
            binding.reverse_route = Some(route);
        }

        let wildcard = match lane {
            StreamLane::Data => self.data_wildcards.get(&(id.room_id, id.topic.clone())),
            StreamLane::Reliable => self.reliable_wildcards.get(&(id.room_id, id.topic.clone())),
        };
        if let Some(wildcard) = wildcard {
            let wildcard = wildcard.clone();
            let Some(binding) = self.stream_bindings_mut(lane).get_mut(&id) else {
                return;
            };
            for (participant, (destination, participant_key, channel)) in wildcard {
                binding.add_subscriber(destination, participant, participant_key, channel);
            }
        }
        self.reconcile_stream(id, lane).await;
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "data and reliable subscriptions share one lifecycle path, and these fields are the complete event identity"
    )]
    async fn on_stream_subscription(
        &mut self,
        shard_id: crate::id::ShardId,
        room_id: RoomId,
        subscriber: ParticipantId,
        subscriber_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
        channel: str0m::channel::ChannelId,
        lane: StreamLane,
    ) {
        let mut ids = Vec::new();
        if let Some(publisher) = publisher {
            let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic.clone());
            if let Some(binding) = self.stream_bindings_mut(lane).get_mut(&id) {
                binding.add_subscriber(shard_id, subscriber, subscriber_key, channel);
                ids.push(id);
            } else {
                self.stream_pending_mut(lane)
                    .entry(id)
                    .or_default()
                    .entry(shard_id)
                    .or_default()
                    .insert(subscriber, (subscriber_key, channel));
            }
        } else {
            self.stream_wildcards_mut(lane, room_id, topic.clone())
                .insert(subscriber, (shard_id, subscriber_key, channel));
            ids = self
                .stream_bindings(lane)
                .keys()
                .filter(|id| id.room_id == room_id && id.topic == topic)
                .cloned()
                .collect();
            for id in &ids {
                if let Some(binding) = self.stream_bindings_mut(lane).get_mut(id) {
                    binding.add_subscriber(shard_id, subscriber, subscriber_key, channel);
                }
            }
        }
        for id in ids {
            self.reconcile_stream(id, lane).await;
        }
    }

    async fn on_stream_unsubscription(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
        lane: StreamLane,
    ) {
        let ids: Vec<_> = if let Some(publisher) = publisher {
            vec![crate::shard::router::DataStreamId::new(
                room_id,
                publisher,
                topic.clone(),
            )]
        } else {
            self.stream_bindings(lane)
                .keys()
                .filter(|id| id.room_id == room_id && id.topic == topic)
                .cloned()
                .collect()
        };
        if publisher.is_none() {
            self.stream_wildcards_mut(lane, room_id, topic)
                .remove(&subscriber);
        }
        let mut changed = Vec::new();
        for id in ids {
            if let Some(binding) = self.stream_bindings_mut(lane).get_mut(&id) {
                binding.remove_subscriber(&subscriber);
                changed.push(id.clone());
            }
            if publisher.is_some()
                && let Some(pending) = self.stream_pending_mut(lane).get_mut(&id)
            {
                pending.retain(|_, subscribers| {
                    subscribers.remove(&subscriber);
                    !subscribers.is_empty()
                });
            }
        }
        for id in changed {
            self.reconcile_stream(id, lane).await;
        }
    }

    fn stage_remove_stream_plan(
        view: &mut crate::view::ShardViewWriter,
        generation: u64,
        lane: StreamLane,
        key: crate::shard::router::RuntimeStreamKey,
    ) {
        match (lane, key) {
            (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                view.stage(generation, crate::view::ViewOp::RemoveDataPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveDataRuntime { key });
            }
            (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                view.stage(generation, crate::view::ViewOp::RemoveReliablePlan { key });
                view.stage(
                    generation,
                    crate::view::ViewOp::RemoveReliableRuntime { key },
                );
            }
            _ => debug_assert!(false, "stream key and lane disagree"),
        }
    }

    async fn retire_stream_destinations(
        &mut self,
        id: &crate::shard::router::DataStreamId,
        lane: StreamLane,
        stale: &[(
            crate::id::ShardId,
            crate::shard::router::RuntimeStreamKey,
            RouteHandle,
        )],
    ) -> bool {
        let Some(binding) = self.stream_bindings(lane).get(id) else {
            debug_assert!(false, "stale routes must belong to a stream binding");
            return false;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let mut source_plan = source_key.map(|_| self.stream_plan(binding, publisher_shard));
        if let Some(plan) = source_plan.as_mut() {
            plan.remote_routes
                .retain(|route| !stale.iter().any(|(shard, _, _)| route.shard_id == *shard));
        }

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };
        for (destination, key, route) in stale {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "a stream route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            Self::stage_remove_stream_plan(view, generation, lane, *key);
        }
        if let Some((key, plan)) = source_key.zip(source_plan) {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a stream publisher must name a local view");
                self.abort_transaction(now);
                return false;
            };
            match (lane, key) {
                (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                    view.stage(generation, crate::view::ViewOp::SetDataPlan { key, plan });
                }
                (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetReliablePlan { key, plan },
                    );
                }
                _ => debug_assert!(false, "stream key and lane disagree"),
            }
        }

        let mut published = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                published.push(shard);
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a published stream retirement must commit");
            self.abort_transaction(now);
            return false;
        }

        for (destination, _, route) in stale {
            let Some(binding) = self.stream_bindings_mut(lane).get_mut(id) else {
                debug_assert!(false, "stream binding must survive route retirement");
                return false;
            };
            binding.routes.remove(destination);
            binding.destination_keys.remove(destination);
            self.state
                .release_endpoint(*destination, route.route.slot(), now);
        }
        true
    }

    async fn retire_stream_binding(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        let Some(binding) = self.stream_bindings(lane).get(&id) else {
            self.stream_pending_mut(lane).remove(&id);
            return true;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let destination_keys = binding.destination_keys.clone();
        let routes: Vec<_> = binding
            .routes
            .iter()
            .filter_map(|(destination, route)| {
                let Some(key) = destination_keys.get(destination).copied() else {
                    debug_assert!(false, "a stream route must have a destination key");
                    return None;
                };
                Some((*destination, key, *route))
            })
            .collect();
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };
        for (destination, key, route) in &routes {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "a stream route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            Self::stage_remove_stream_plan(view, generation, lane, *key);
        }
        if let Some(key) = source_key {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a stream publisher must name a local view");
                self.abort_transaction(now);
                return false;
            };
            Self::stage_remove_stream_plan(view, generation, lane, key);
        }
        let mut published = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                published.push(shard);
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a published stream retirement must commit");
            self.abort_transaction(now);
            return false;
        }
        for (destination, _, route) in &routes {
            self.state
                .release_endpoint(*destination, route.route.slot(), now);
        }
        if let Some(key) = source_key {
            self.retire_stream_runtime(publisher_shard, key, lane);
        }
        for (destination, key) in destination_keys {
            self.retire_stream_runtime(destination, key, lane);
        }
        self.stream_bindings_mut(lane).remove(&id);
        true
    }

    fn stream_wildcards_mut(
        &mut self,
        lane: StreamLane,
        room_id: RoomId,
        topic: crate::track::Topic,
    ) -> &mut HashMap<
        ParticipantId,
        (
            crate::id::ShardId,
            crate::shard::participants::ParticipantKey,
            str0m::channel::ChannelId,
        ),
    > {
        match lane {
            StreamLane::Data => self.data_wildcards.entry((room_id, topic)).or_default(),
            StreamLane::Reliable => self.reliable_wildcards.entry((room_id, topic)).or_default(),
        }
    }

    async fn reconcile_stream(&mut self, id: crate::shard::router::DataStreamId, lane: StreamLane) {
        let stale: Vec<_> = self
            .stream_bindings(lane)
            .get(&id)
            .map(|binding| {
                binding
                    .routes
                    .iter()
                    .filter_map(|(destination, route)| {
                        if binding.subscribers.contains_key(destination) {
                            return None;
                        }
                        let key = binding.destination_keys.get(destination).copied()?;
                        Some((*destination, key, *route))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let retired = !stale.is_empty();
        if retired && !self.retire_stream_destinations(&id, lane, &stale).await {
            debug_assert!(false, "stream route retirement must complete");
            return;
        }
        let destinations: Vec<_> = self
            .stream_bindings(lane)
            .get(&id)
            .map(|binding| binding.subscribers.keys().copied().collect())
            .unwrap_or_default();
        let Some(publisher_shard) = self
            .stream_bindings(lane)
            .get(&id)
            .map(|binding| binding.publisher_shard)
        else {
            return;
        };
        let mut added = false;
        for destination in destinations {
            if destination == publisher_shard
                || self
                    .stream_bindings(lane)
                    .get(&id)
                    .is_some_and(|binding| binding.routes.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_stream_key(destination, &id, lane) else {
                continue;
            };
            let Some(action) = Self::stream_action(lane, key) else {
                debug_assert!(false, "stream preparation returned the wrong lane");
                continue;
            };
            let Some(binding) = self.stream_bindings(lane).get(&id) else {
                return;
            };
            let plan = self.stream_plan(binding, destination);
            let Some(route) = self
                .grant_route_with_plan(destination, action, lane, plan.clone())
                .await
            else {
                continue;
            };
            let Some(binding) = self.stream_bindings_mut(lane).get_mut(&id) else {
                return;
            };
            binding.destination_keys.insert(destination, key);
            binding.routes.insert(destination, route);
            added = true;
        }
        if !retired || added {
            self.publish_stream_views(id, lane).await;
        }
    }

    async fn publish_stream_views(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) {
        let Some(binding) = self.stream_bindings(lane).get(&id) else {
            return;
        };
        let publisher_key = binding.publisher;
        let binding_data = (
            binding.publisher_shard,
            binding.key,
            binding.destination_keys.clone(),
            binding.routes.clone(),
        );
        let mut targets = Vec::new();
        if let Some(key) = binding_data.1 {
            targets.push((
                binding_data.0,
                key,
                self.stream_plan(binding, binding_data.0),
                None,
            ));
        }
        for (destination, key) in binding_data.2 {
            let Some(route) = binding_data.3.get(&destination).copied() else {
                continue;
            };
            targets.push((
                destination,
                key,
                self.stream_plan(binding, destination),
                Some(route),
            ));
        }
        if targets.is_empty() {
            return;
        }
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return;
        };
        for (shard, key, plan, route) in targets {
            let Some(view) = self.view_mut(shard) else {
                self.abort_transaction(now);
                return;
            };
            match (lane, key) {
                (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::InsertDataRuntime {
                            key,
                            id: id.clone(),
                            publisher: publisher_key,
                        },
                    );
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetDataPlan {
                            key,
                            plan: plan.clone(),
                        },
                    );
                    if let Some(route) = route {
                        view.stage(
                            generation,
                            crate::view::ViewOp::InstallRoute {
                                route: route.route,
                                binding: crate::view::RouteBinding {
                                    epoch: route.epoch,
                                    action: RouteAction::Data { stream: key },
                                },
                            },
                        );
                    }
                }
                (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::InsertReliableRuntime {
                            key,
                            id: id.clone(),
                            publisher: publisher_key,
                        },
                    );
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetReliablePlan {
                            key,
                            plan: plan.clone(),
                        },
                    );
                    if let Some(route) = route {
                        view.stage(
                            generation,
                            crate::view::ViewOp::InstallRoute {
                                route: route.route,
                                binding: crate::view::RouteBinding {
                                    epoch: route.epoch,
                                    action: RouteAction::Reliable { stream: key },
                                },
                            },
                        );
                    }
                }
                _ => debug_assert!(false, "stream key and lane disagree"),
            }
        }
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                affected.push(shard);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
        }
    }

    /// Buggify's exhaustion fault fires independently per call, so a few
    /// attempts clear a transient one almost certainly; a genuinely full
    /// namespace fails every attempt just as fast, so the retry costs nothing
    /// in that case either.
    const TRANSPORT_ALLOCATION_ATTEMPTS: u32 = 10;

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
                source: _authenticated_source,
                destination: _authenticated_destination,
                shard,
            } => {
                debug_assert_eq!(shard, shard_id);
                #[cfg(not(feature = "sim"))]
                if let Some(steering) = self.steering.as_mut() {
                    let flow =
                        crate::ebpf::flow_key(_authenticated_source, _authenticated_destination);
                    let Ok(shard_index) = u16::try_from(shard.index()) else {
                        debug_assert!(false, "a shard id must fit the eBPF map value");
                        return None;
                    };
                    if let Err(error) = steering.install_flow(flow, shard_index) {
                        tracing::warn!(%error, %shard, "failed to install authenticated eBPF flow");
                    }
                }
                None
            }
            ShardEvent::TrackSubscribed {
                subscriber,
                subscriber_key,
                slot,
                track,
            } => {
                if !self.track_bindings.contains_key(&track.id) {
                    // Track ids come from the client, so a subscription may
                    // name something that does not exist and never will. Cap
                    // how many a single participant can park here; the whole
                    // list is dropped when it disconnects.
                    let held = self
                        .pending_track_counts
                        .get(&subscriber)
                        .copied()
                        .unwrap_or_default();
                    if held >= MAX_PENDING_SUBSCRIPTIONS_PER_PARTICIPANT {
                        metrics::counter!("pending_subscription_rejected").increment(1);
                        return None;
                    }
                    self.pending_track_subscriptions
                        .entry(track.id)
                        .or_default()
                        .push(PendingTrackSubscription {
                            shard_id,
                            subscriber,
                            subscriber_key,
                            slot,
                            track,
                        });
                    let held = self.pending_track_counts.entry(subscriber).or_default();
                    *held = held.saturating_add(1);
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
                topic,
            } => {
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                let key = self.state.mint_data(shard_id, id.clone())?;
                self.on_stream_ready(
                    shard_id,
                    id,
                    crate::shard::router::RuntimeStreamKey::Data(key),
                )
                .await;
                None
            }
            ShardEvent::ReliableDataTopicPublished {
                room_id,
                publisher,
                topic,
            } => {
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                let key = self.state.mint_reliable(shard_id, id.clone())?;
                self.on_stream_ready(
                    shard_id,
                    id,
                    crate::shard::router::RuntimeStreamKey::Reliable(key),
                )
                .await;
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
                    StreamLane::Data,
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
                    StreamLane::Data,
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
                topic,
            } => {
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                if !self.retire_stream_binding(id, StreamLane::Data).await {
                    debug_assert!(false, "data stream retirement must complete");
                }
                None
            }
            ShardEvent::ReliableDataTopicUnpublished {
                room_id,
                publisher,
                topic,
            } => {
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                if !self.retire_stream_binding(id, StreamLane::Reliable).await {
                    debug_assert!(false, "reliable stream retirement must complete");
                }
                None
            }
            ShardEvent::ParticipantClosed {
                participant: participant_id,
            } => {
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
                let fanout = self.prepare_track_key(shard_id, track_id, track.meta.origin)?;
                let track_id = track.meta.id;
                self.track_bindings.insert(
                    track_id,
                    TrackBinding {
                        meta: track.meta.clone(),
                        publication: track.as_ref().clone(),
                        publisher_participant: self
                            .core
                            .registry
                            .get_participant(&track.meta.origin)
                            .and_then(|meta| meta.binding)?,
                        encodings: track.layers.iter().map(|layer| layer.rid).collect(),
                        states,
                        publisher_shard: shard_id,
                        publisher_fanout: fanout,
                        reverse_route: None,
                        fanouts: HashMap::new(),
                        audio_fanouts: HashMap::new(),
                        audio_routes: HashMap::new(),
                    },
                );
                let announced = self.on_track_published(shard_id, *track, fanout).await;
                if let Some(track) = &announced {
                    if let Some(binding) = self.track_bindings.get_mut(&track_id) {
                        binding.reverse_route = track.reverse;
                        binding.publication = track.clone();
                    }
                } else {
                    self.track_bindings.remove(&track_id);
                }
                if announced.is_some() {
                    self.install_video_runtimes(track_id).await;
                    self.install_audio_routes(track_id).await;
                    if !self.publish_track_plans(track_id).await {
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

    async fn drain_pending_track_subscriptions(&mut self, track_id: crate::entity::TrackId) {
        let pending = self
            .pending_track_subscriptions
            .remove(&track_id)
            .unwrap_or_default();
        for subscription in pending {
            if let Some(count) = self.pending_track_counts.get_mut(&subscription.subscriber) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.pending_track_counts.remove(&subscription.subscriber);
                }
            }
            self.on_track_subscribed(
                subscription.shard_id,
                subscription.subscriber,
                subscription.subscriber_key,
                subscription.slot,
                subscription.track,
            )
            .await;
        }
    }

    fn remove_pending_track_subscription(
        &mut self,
        track_id: crate::entity::TrackId,
        subscriber: ParticipantId,
        slot: crate::keys::DownstreamSlotKey,
    ) {
        let mut removed = false;
        if let Some(pending) = self.pending_track_subscriptions.get_mut(&track_id) {
            pending.retain(|entry| {
                let matches = entry.subscriber == subscriber && entry.slot == slot;
                removed |= matches;
                !matches
            });
        }
        if self
            .pending_track_subscriptions
            .get(&track_id)
            .is_some_and(Vec::is_empty)
        {
            self.pending_track_subscriptions.remove(&track_id);
        }
        if removed && let Some(count) = self.pending_track_counts.get_mut(&subscriber) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pending_track_counts.remove(&subscriber);
            }
        }
    }

    async fn install_video_runtimes(&mut self, track_id: crate::entity::TrackId) {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            debug_assert!(false, "video runtime installation requires a track binding");
            return;
        };
        let publisher_shard = binding.publisher_shard;
        let origin = binding.meta.origin;
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return;
        };
        let destinations: Vec<_> = self
            .core
            .registry
            .participants_in_room(&room_id)
            .into_iter()
            .map(|(_, shard, _)| shard)
            .filter(|shard| *shard != publisher_shard)
            .collect();
        for destination in destinations {
            if self
                .track_bindings
                .get(&track_id)
                .is_some_and(|binding| binding.fanouts.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                debug_assert!(false, "a subscriber shard must accept a track runtime");
                continue;
            };
            let Some(binding) = self.track_bindings.get_mut(&track_id) else {
                debug_assert!(
                    false,
                    "track binding disappeared during runtime installation"
                );
                return;
            };
            binding.fanouts.insert(destination, key);
        }
    }

    async fn retire_track_binding(&mut self, track_id: crate::entity::TrackId) -> bool {
        let video_routes = self.subscriptions.remove_stream(&track_id);
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return true;
        };
        let publisher_shard = binding.publisher_shard;
        let publisher_fanout = binding.publisher_fanout;
        let reverse_route = binding.reverse_route;
        let fanouts = binding.fanouts.clone();
        let audio_fanouts = binding.audio_fanouts.clone();
        let audio_routes = binding.audio_routes.clone();

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };

        let mut endpoint_releases = Vec::new();
        for retired in &video_routes {
            let Some(view) = self.view_mut(retired.destination) else {
                debug_assert!(false, "a video route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: retired.route.route,
                    epoch: retired.route.epoch,
                },
            );
            if let Some(key) = fanouts.get(&retired.destination).copied() {
                view.stage(generation, crate::view::ViewOp::RemoveTrackPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
            endpoint_releases.push((retired.destination, retired.route));
        }
        for (destination, route) in &audio_routes {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "an audio route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            if let Some(key) = audio_fanouts.get(destination).copied() {
                view.stage(generation, crate::view::ViewOp::RemoveAudioPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
            endpoint_releases.push((*destination, *route));
        }
        if let Some(route) = reverse_route {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a reverse route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            endpoint_releases.push((publisher_shard, route));
        }

        let Some(view) = self.view_mut(publisher_shard) else {
            debug_assert!(false, "a track publisher must name a local view");
            self.abort_transaction(now);
            return false;
        };
        view.stage(
            generation,
            crate::view::ViewOp::RemoveTrackPlan {
                key: publisher_fanout,
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::RemoveTrackRuntime {
                key: publisher_fanout,
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::RemoveAudioPlan {
                key: publisher_fanout,
            },
        );
        for (&destination, &key) in &fanouts {
            if destination == publisher_shard {
                continue;
            }
            if let Some(view) = self.view_mut(destination) {
                view.stage(generation, crate::view::ViewOp::RemoveTrackPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
        }
        for (&destination, &key) in &audio_fanouts {
            if let Some(view) = self.view_mut(destination) {
                view.stage(generation, crate::view::ViewOp::RemoveAudioPlan { key });
            }
        }

        let mut published = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                published.push(shard);
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a published track retirement must commit");
            self.abort_transaction(now);
            return false;
        }
        for (shard, route) in endpoint_releases {
            self.state.release_endpoint(shard, route.route.slot(), now);
        }
        self.state.remove_track(publisher_shard, publisher_fanout);
        for (&destination, &key) in &fanouts {
            self.state.remove_track(destination, key);
        }
        for (&destination, &key) in &audio_fanouts {
            self.state.remove_track(destination, key);
        }
        self.track_bindings.remove(&track_id);
        true
    }

    /// A shard reported a newly published track.
    ///
    /// Its reverse route is opened here, before anything learns the track
    /// exists — a subscriber that heard about it first would have nowhere to
    /// send a keyframe request. Returns the descriptor with the handle
    /// stamped on, for the ordinary topology projection to distribute.
    async fn on_track_published(
        &mut self,
        shard_id: crate::id::ShardId,
        mut track: crate::track::Track,
        fanout: crate::shard::router::TrackKey,
    ) -> Option<crate::track::Track> {
        let handle = self
            .grant_route(
                shard_id,
                crate::route::RouteAction::Reverse {
                    target: crate::route::ReverseTarget::Track { track: fanout },
                },
            )
            .await?;
        track.reverse = Some(handle);
        Some(track)
    }

    fn prepare_track_key(
        &mut self,
        shard_id: crate::id::ShardId,
        track_id: crate::entity::TrackId,
        origin: ParticipantId,
    ) -> Option<crate::shard::router::TrackKey> {
        self.state.mint_track(shard_id, track_id, origin)
    }

    async fn install_audio_routes(&mut self, track_id: crate::entity::TrackId) {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return;
        };
        let origin = binding.meta.origin;
        let publisher_shard = binding.publisher_shard;
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return;
        };
        let mut local_subscribers: HashMap<
            crate::id::ShardId,
            Vec<crate::shard::participants::ParticipantKey>,
        > = HashMap::new();
        for (participant, shard, key) in self.core.registry.participants_in_room(&room_id) {
            if participant != origin
                && let Some(key) = key
            {
                local_subscribers.entry(shard).or_default().push(key);
            }
        }
        let destinations: Vec<_> = local_subscribers.keys().copied().collect();
        for destination in destinations {
            if destination == publisher_shard {
                continue;
            }
            if self
                .track_bindings
                .get(&track_id)
                .is_some_and(|binding| binding.audio_fanouts.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                continue;
            };
            let plan = crate::view::AudioForwardingPlan {
                track_id,
                origin,
                local_subscribers: local_subscribers
                    .get(&destination)
                    .cloned()
                    .unwrap_or_default(),
                remote_routes: Vec::new(),
                reverse_route: None,
            };
            let Some(route) = self
                .grant_route_binding(
                    destination,
                    RouteAction::Audio { track: key },
                    None,
                    Some(plan),
                    None,
                    None,
                )
                .await
            else {
                continue;
            };
            let Some(binding) = self.track_bindings.get_mut(&track_id) else {
                return;
            };
            binding.audio_fanouts.insert(destination, key);
            binding.audio_routes.insert(destination, route);
        }
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return;
        };
        let remote_routes = binding
            .audio_routes
            .iter()
            .map(|(shard_id, route)| crate::view::RemoteRoutePlan {
                shard_id: *shard_id,
                route: route.route,
                epoch: route.epoch,
            })
            .collect();
        let source_plan = crate::view::AudioForwardingPlan {
            track_id,
            origin,
            local_subscribers: local_subscribers
                .get(&publisher_shard)
                .cloned()
                .unwrap_or_default(),
            remote_routes,
            reverse_route: binding
                .reverse_route
                .map(|route| crate::view::RemoteRoutePlan {
                    shard_id: publisher_shard,
                    route: route.route,
                    epoch: route.epoch,
                }),
        };
        let mut targets = vec![(publisher_shard, binding.publisher_fanout, source_plan, None)];
        for (destination, key) in &binding.audio_fanouts {
            let Some(route) = binding.audio_routes.get(destination).copied() else {
                continue;
            };
            targets.push((
                *destination,
                *key,
                crate::view::AudioForwardingPlan {
                    track_id,
                    origin,
                    local_subscribers: local_subscribers
                        .get(destination)
                        .cloned()
                        .unwrap_or_default(),
                    remote_routes: Vec::new(),
                    reverse_route: None,
                },
                Some(route),
            ));
        }
        self.publish_audio_plans(targets).await;
    }

    async fn publish_audio_plans(
        &mut self,
        targets: Vec<(
            crate::id::ShardId,
            crate::shard::router::TrackKey,
            crate::view::AudioForwardingPlan,
            Option<RouteHandle>,
        )>,
    ) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return;
        };
        for (shard, key, plan, route) in targets {
            let Some(binding) = self.track_bindings.get(&plan.track_id) else {
                self.abort_transaction(now);
                return;
            };
            let publisher_fanout = binding.publisher_fanout;
            let descriptor = crate::view::TrackDescriptor {
                id: binding.meta.id,
                origin_key: binding.publisher_participant,
                participant: (shard == binding.publisher_shard)
                    .then_some(binding.publisher_participant),
                encodings: binding.encodings.clone(),
                states: binding.states.clone(),
                publication: binding.publication.clone(),
                audience: self.track_audience_on_shard(binding.meta.origin, shard),
            };
            let Some(view) = self.view_mut(shard) else {
                self.abort_transaction(now);
                return;
            };
            if key != publisher_fanout {
                view.stage(
                    generation,
                    crate::view::ViewOp::InsertTrackRuntime { key, descriptor },
                );
            }
            view.stage(
                generation,
                crate::view::ViewOp::SetAudioPlan {
                    key,
                    plan: plan.clone(),
                },
            );
            if let Some(route) = route {
                view.stage(
                    generation,
                    crate::view::ViewOp::InstallRoute {
                        route: route.route,
                        binding: crate::view::RouteBinding {
                            epoch: route.epoch,
                            action: RouteAction::Audio { track: key },
                        },
                    },
                );
            }
        }
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                affected.push(shard);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
        }
    }

    /// A shard reported a new local consumer for a track.
    ///
    /// The shard did not ask for anything. This decides whether that shard
    /// now needs a route, installs one if so, and tells the publisher's shard
    /// to start forwarding — the three things the shard used to do by asking.
    async fn on_track_subscribed(
        &mut self,
        shard_id: crate::id::ShardId,
        subscriber: ParticipantId,
        subscriber_key: crate::shard::participants::ParticipantKey,
        slot: crate::keys::DownstreamSlotKey,
        track: crate::track::TrackMeta,
    ) {
        let Some(subscriber_room) = self
            .core
            .registry
            .get_participant(&subscriber)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a subscription must come from a live participant");
            return;
        };
        if subscriber_room != track.room_id {
            metrics::counter!("track_subscription_room_rejected").increment(1);
            return;
        }
        let Some(origin_room) = self
            .core
            .registry
            .get_participant(&track.origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a live origin");
            return;
        };
        if origin_room != track.room_id {
            debug_assert!(false, "track metadata room must match its origin");
            return;
        }
        let fanout = {
            let Some(binding) = self.track_bindings.get(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            if shard_id == binding.publisher_shard {
                binding.publisher_fanout
            } else if let Some(&fanout) = binding.fanouts.get(&shard_id) {
                fanout
            } else {
                let Some(fanout) = self.prepare_track_key(shard_id, track.id, track.origin) else {
                    debug_assert!(false, "a subscriber shard must accept a track runtime");
                    return;
                };
                let Some(binding) = self.track_bindings.get_mut(&track.id) else {
                    return;
                };
                binding.fanouts.insert(shard_id, fanout);
                fanout
            }
        };
        let change = self.subscriptions.subscribe(
            shard_id,
            track.id,
            subscriber,
            subscriber_key,
            slot,
            track.shard_id,
        );
        {
            let Some(binding) = self.track_bindings.get_mut(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            binding.fanouts.insert(shard_id, fanout);
        }

        if change == crate::control::subscriptions::InterestChange::Install {
            let Some((_, plan)) = self.track_plan(track.id, shard_id) else {
                debug_assert!(false, "a first subscription must have a compiled plan");
                return;
            };
            let Some(handle) = self.install_video_route(shard_id, fanout, plan).await else {
                self.subscriptions
                    .unsubscribe(shard_id, &track.id, &subscriber);
                if let Some(binding) = self.track_bindings.get_mut(&track.id) {
                    binding.fanouts.remove(&shard_id);
                }
                return;
            };
            self.subscriptions.installed(shard_id, track.id, handle);
        }

        if !self.publish_track_plans(track.id).await {
            debug_assert!(false, "track plan publication must complete");
        }
    }

    /// The inverse. Only the last consumer on a shard retires its route, and
    /// the publisher is told to stop before the route leaves the view.
    async fn on_track_unsubscribed(
        &mut self,
        shard_id: crate::id::ShardId,
        subscriber: ParticipantId,
        track: crate::track::TrackMeta,
    ) {
        let crate::control::subscriptions::InterestChange::Retire { route } = self
            .subscriptions
            .unsubscribe(shard_id, &track.id, &subscriber)
        else {
            let _ = self.publish_track_plans(track.id).await;
            return;
        };
        if !self.retire_video_route(shard_id, route, track.id).await {
            debug_assert!(false, "track route retirement must complete");
        }
    }

    fn track_plan(
        &self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
    ) -> Option<(
        crate::shard::router::TrackKey,
        crate::view::TrackForwardingPlan,
    )> {
        let binding = self.track_bindings.get(&track_id)?;
        let fanout = if shard_id == binding.publisher_shard {
            binding.publisher_fanout
        } else {
            *binding.fanouts.get(&shard_id)?
        };
        let mut local_subscribers = Vec::new();
        let mut remote_routes = Vec::new();
        for (destination, route, subscribers) in self.subscriptions.plan_destinations(&track_id) {
            if destination == shard_id {
                local_subscribers.extend(subscribers);
            }
            if shard_id == binding.publisher_shard
                && destination != shard_id
                && let Some(route) = route
            {
                remote_routes.push(crate::view::RemoteRoutePlan {
                    shard_id: destination,
                    route: route.route,
                    epoch: route.epoch,
                });
            }
        }
        Some((
            fanout,
            crate::view::TrackForwardingPlan {
                track_id: binding.meta.id,
                origin: binding.meta.origin,
                local_subscribers,
                remote_routes,
                reverse_route: binding
                    .reverse_route
                    .map(|route| crate::view::RemoteRoutePlan {
                        shard_id: binding.publisher_shard,
                        route: route.route,
                        epoch: route.epoch,
                    }),
            },
        ))
    }

    fn track_audience_on_shard(
        &self,
        origin: ParticipantId,
        shard: crate::id::ShardId,
    ) -> Vec<crate::shard::participants::ParticipantKey> {
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return Vec::new();
        };
        self.core
            .registry
            .participants_in_room(&room_id)
            .into_iter()
            .filter(|(_, owner, key)| *owner == shard && key.is_some())
            .filter_map(|(_, _, key)| key)
            .collect()
    }

    async fn publish_track_plans(&mut self, track_id: crate::entity::TrackId) -> bool {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return false;
        };
        let publisher_shard = binding.publisher_shard;
        let fanout_shards: Vec<_> = binding.fanouts.keys().copied().collect();
        let mut plans = Vec::new();
        for shard_id in fanout_shards {
            if let Some(plan) = self.track_plan(track_id, shard_id) {
                plans.push((shard_id, plan));
            }
        }
        if let Some(plan) = self.track_plan(track_id, publisher_shard) {
            plans.push((publisher_shard, plan));
        }
        if plans.is_empty() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return false;
        };
        for (shard_id, (fanout, plan)) in plans {
            let Some(binding) = self.track_bindings.get(&track_id) else {
                self.abort_transaction(now);
                return false;
            };
            let descriptor = crate::view::TrackDescriptor {
                id: binding.meta.id,
                origin_key: binding.publisher_participant,
                participant: (shard_id == binding.publisher_shard)
                    .then_some(binding.publisher_participant),
                encodings: binding.encodings.clone(),
                states: binding.states.clone(),
                publication: binding.publication.clone(),
                audience: self.track_audience_on_shard(binding.meta.origin, shard_id),
            };
            let Some(view) = self.view_mut(shard_id) else {
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::InsertTrackRuntime {
                    key: fanout,
                    descriptor,
                },
            );
            view.stage(
                generation,
                crate::view::ViewOp::SetTrackPlan { key: fanout, plan },
            );
        }
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let shard_id = crate::id::ShardId::new(index);
            if let Some(view) = self.view_mut(shard_id)
                && view.publish().is_some()
            {
                affected.push(shard_id);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return false;
        }
        true
    }

    async fn install_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        fanout: crate::shard::router::TrackKey,
        plan: crate::view::TrackForwardingPlan,
    ) -> Option<RouteHandle> {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }
        let Ok(handle) = self.state.reserve_endpoint(shard_id, now) else {
            self.abort_transaction(now);
            return None;
        };
        let generation = self.state.pending()?.generation;
        let Some(view) = self.view_mut(shard_id) else {
            self.abort_transaction(now);
            return None;
        };
        view.stage(
            generation,
            crate::view::ViewOp::InstallRoute {
                route: handle.route,
                binding: crate::view::RouteBinding {
                    epoch: handle.epoch,
                    action: crate::route::RouteAction::Video {
                        local_track: fanout,
                    },
                },
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::SetTrackPlan { key: fanout, plan },
        );
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let Some(view) = self.view_mut(target)
                && view.publish().is_some()
            {
                affected.push(target);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        Some(handle)
    }

    async fn retire_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: RouteHandle,
        track_id: crate::entity::TrackId,
    ) -> bool {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return false;
        };
        if let Some(view) = self.view_mut(shard_id) {
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: handle.route,
                    epoch: handle.epoch,
                },
            );
        }
        let mut affected = vec![shard_id];
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let Some(plan) = self.track_plan(track_id, target)
                && let Some(view) = self.view_mut(target)
            {
                view.stage(
                    generation,
                    crate::view::ViewOp::SetTrackPlan {
                        key: plan.0,
                        plan: plan.1,
                    },
                );
                if target != shard_id {
                    affected.push(target);
                }
            }
        }
        affected.sort_by_key(|shard| shard.index());
        affected.dedup();
        let mut published = Vec::new();
        for target in affected {
            if let Some(view) = self.view_mut(target)
                && view.publish().is_some()
            {
                published.push(target);
            }
        }
        if self.state.commit().is_err() {
            return false;
        }
        self.state
            .release_endpoint(shard_id, handle.route.slot(), now);
        true
    }

    /// Allocate, publish and confirm one endpoint route.
    ///
    /// The action is opaque here: it holds keys that mean something only on
    /// the shard that asked, and the control plane's job is to give them an
    /// address and put them in that shard's view — not to interpret them.
    async fn grant_route(
        &mut self,
        shard_id: crate::id::ShardId,
        action: crate::route::RouteAction,
    ) -> Option<RouteHandle> {
        self.grant_route_binding(shard_id, action, None, None, None, None)
            .await
    }

    async fn grant_route_with_plan(
        &mut self,
        shard_id: crate::id::ShardId,
        action: RouteAction,
        lane: StreamLane,
        plan: crate::view::StreamForwardingPlan,
    ) -> Option<RouteHandle> {
        match lane {
            StreamLane::Data => {
                self.grant_route_binding(shard_id, action, None, None, Some(plan), None)
                    .await
            }
            StreamLane::Reliable => {
                self.grant_route_binding(shard_id, action, None, None, None, Some(plan))
                    .await
            }
        }
    }

    async fn grant_route_binding(
        &mut self,
        shard_id: crate::id::ShardId,
        action: crate::route::RouteAction,
        video_plan: Option<crate::view::TrackForwardingPlan>,
        audio_plan: Option<crate::view::AudioForwardingPlan>,
        data_plan: Option<crate::view::StreamForwardingPlan>,
        reliable_plan: Option<crate::view::StreamForwardingPlan>,
    ) -> Option<RouteHandle> {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }
        let handle = match self.state.reserve_endpoint(shard_id, now) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!(%shard_id, ?err, "endpoint route allocation failed");
                self.abort_transaction(now);
                return None;
            }
        };

        let published = self
            .state
            .pending()
            .map(|tx| tx.generation)
            .and_then(|generation| {
                let view = self.view_mut(shard_id)?;
                view.stage(
                    generation,
                    crate::view::ViewOp::InstallRoute {
                        route: handle.route,
                        binding: crate::view::RouteBinding {
                            epoch: handle.epoch,
                            action,
                        },
                    },
                );
                match (action, video_plan, audio_plan, data_plan, reliable_plan) {
                    (RouteAction::Video { local_track }, Some(plan), _, _, _) => {
                        view.stage(
                            generation,
                            crate::view::ViewOp::SetTrackPlan {
                                key: local_track,
                                plan,
                            },
                        );
                    }
                    (RouteAction::Audio { track }, _, Some(plan), _, _) => {
                        view.stage(
                            generation,
                            crate::view::ViewOp::SetAudioPlan { key: track, plan },
                        );
                    }
                    (RouteAction::Data { stream }, _, _, Some(plan), _) => {
                        view.stage(
                            generation,
                            crate::view::ViewOp::SetDataPlan { key: stream, plan },
                        );
                    }
                    (RouteAction::Reliable { stream }, _, _, _, Some(plan)) => {
                        view.stage(
                            generation,
                            crate::view::ViewOp::SetReliablePlan { key: stream, plan },
                        );
                    }
                    (RouteAction::Reverse { .. }, None, None, None, None) => {}
                    _ => debug_assert!(false, "route action and compiled plan disagree"),
                }
                view.publish()
            });
        let Some(_) = published else {
            self.abort_transaction(now);
            return None;
        };
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        Some(handle)
    }

    /// Take a route out of the published view, then return its slot.
    ///
    /// The order is the whole point: a slot handed back before the route is
    /// absent from the view could be granted again while a packet addressed
    /// to its predecessor is still arriving.
    async fn release_route(&mut self, shard_id: crate::id::ShardId, handle: RouteHandle) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return;
        }
        let published = self
            .state
            .pending()
            .map(|tx| tx.generation)
            .and_then(|generation| {
                let view = self.view_mut(shard_id)?;
                view.stage(
                    generation,
                    crate::view::ViewOp::RetireRoute {
                        route: handle.route,
                        epoch: handle.epoch,
                    },
                );
                view.publish()
            });
        let Some(_) = published else {
            self.abort_transaction(now);
            return;
        };
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return;
        }
        self.state
            .release_endpoint(shard_id, handle.route.slot(), now);
    }

    /// Stage a participant's transport route as one lifecycle generation.
    ///
    /// The control plane allocates the address, the owning shard prepares
    /// only the runtime binding it must build on its own core, and the route
    /// becomes resolvable when the view carrying it is applied on the owning
    /// shard. The caller advertises the route after queuing the delta; a first
    /// packet racing that apply is a counted, recoverable drop.
    ///
    /// `drain_core_events` runs first so a `RemoveParticipant` queued by a
    /// preceding delete (a reconnect's teardown-then-recreate) reaches the
    /// shard's mailbox before this does; otherwise the prepare below could
    /// race the old entry under the same id and trip the registry's
    /// duplicate-reservation assertion.
    async fn stage_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        participant_id: ParticipantId,
    ) -> Option<(TransportHandle, crate::shard::participants::ParticipantKey)> {
        self.drain_core_events().await;

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }

        // Bounded retries for a transient allocation failure. Every other
        // install failure here recovers on a later externally-triggered retry
        // (the next subscribe, the next publish); connection setup has no such
        // later trigger — a client gets this one attempt before it sees the
        // join itself fail — so the retry has to happen here.
        let mut reserved = None;
        for attempt in 1..=Self::TRANSPORT_ALLOCATION_ATTEMPTS {
            match self.state.reserve_transport(shard_id, now) {
                Ok(handle) => {
                    reserved = Some(handle);
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        %participant_id,
                        attempt,
                        "transport route allocation failed, retrying"
                    );
                }
            }
        }
        let Some(handle) = reserved else {
            self.abort_transaction(now);
            return None;
        };

        let Some(participant_key) = self
            .prepare_transport(shard_id, participant_id, handle)
            .await
        else {
            self.abort_transaction(now);
            return None;
        };

        let Some(_) = self.publish_pending(shard_id, handle, participant_key) else {
            self.abort_transaction(now);
            return None;
        };

        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        Some((handle, participant_key))
    }

    /// Ask the owning shard to build the runtime binding this route will
    /// point at. The shard decides nothing here — it reserves a key on its
    /// own core and reports it back.
    async fn prepare_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        participant_id: ParticipantId,
        handle: TransportHandle,
    ) -> Option<crate::shard::participants::ParticipantKey> {
        debug_assert_eq!(handle.shard(), shard_id);
        self.state.mint_participant(shard_id, participant_id)
    }

    /// Compile and publish the staged generation. One publish, one shard.
    fn publish_pending(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: TransportHandle,
        participant: crate::shard::participants::ParticipantKey,
    ) -> Option<u64> {
        let generation = self.state.pending().map(|tx| tx.generation)?;
        let view = self.view_mut(shard_id)?;
        view.stage(
            generation,
            crate::view::ViewOp::InsertParticipant { key: participant },
        );
        view.stage(
            generation,
            crate::view::ViewOp::InstallTransport {
                route: handle.route,
                binding: crate::view::TransportBinding {
                    epoch: handle.epoch,
                    participant,
                },
            },
        );
        view.publish()
    }

    /// Release everything a departing participant was consuming.
    ///
    /// Its subscriptions go with it, and any route that only it kept alive is
    /// retired — otherwise a shard keeps a route for a stream nobody there
    /// receives any more, and the slot never returns to the allocator.
    async fn retire_participant_streams(&mut self, participant_id: &ParticipantId) {
        for lane in [StreamLane::Data, StreamLane::Reliable] {
            let ids: Vec<_> = self.stream_bindings(lane).keys().cloned().collect();
            let mut changed = Vec::new();
            for id in ids {
                if self
                    .stream_bindings_mut(lane)
                    .get_mut(&id)
                    .is_some_and(|binding| binding.remove_subscriber(participant_id))
                {
                    changed.push(id);
                }
            }
            self.stream_pending_mut(lane).retain(|_, pending| {
                pending.retain(|_, subscribers| {
                    subscribers.remove(participant_id);
                    !subscribers.is_empty()
                });
                !pending.is_empty()
            });
            match lane {
                StreamLane::Data => {
                    self.data_wildcards.retain(|_, subscribers| {
                        subscribers.remove(participant_id);
                        !subscribers.is_empty()
                    });
                }
                StreamLane::Reliable => {
                    self.reliable_wildcards.retain(|_, subscribers| {
                        subscribers.remove(participant_id);
                        !subscribers.is_empty()
                    });
                }
            }
            for id in changed {
                self.reconcile_stream(id, lane).await;
            }
        }
    }

    async fn retire_participant_tracks(&mut self, participant_id: &ParticipantId) {
        let tracks: Vec<_> = self
            .track_bindings
            .iter()
            .filter(|(_, binding)| binding.meta.origin == *participant_id)
            .map(|(track_id, _)| *track_id)
            .collect();
        for track_id in tracks {
            if !self.retire_track_binding(track_id).await {
                debug_assert!(false, "publisher track retirement must complete");
                continue;
            }
            let _ = self.core.registry.remove_track(*participant_id, track_id);
        }
    }

    async fn retire_participant_subscriptions(&mut self, participant_id: &ParticipantId) {
        for retired in self.subscriptions.remove_participant(participant_id) {
            self.release_route(retired.destination, retired.route).await;
        }
        // A subscription naming a track that was never published waits here
        // for a publish that may never come. Without this it outlives the
        // subscriber: track ids are client-supplied, so a client could join,
        // subscribe to invented ids, disconnect, and leave the entries behind
        // for the life of the process — unbounded memory, and an O(pending)
        // scan on every subsequent publish.
        for pending in self.pending_track_subscriptions.values_mut() {
            pending.retain(|entry| entry.subscriber != *participant_id);
        }
        self.pending_track_subscriptions
            .retain(|_, entries| !entries.is_empty());
        self.pending_track_counts.remove(participant_id);
    }

    /// Retire whatever transport route a participant holds, if the registry
    /// still knows about one.
    async fn retire_participant_transport(&mut self, participant_id: &ParticipantId) {
        let Some((shard_id, handle)) = self.core.registry.transport_of(participant_id) else {
            return;
        };
        let key = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding);
        self.retire_transport(shard_id, handle, key).await;
        if let Some(key) = key {
            self.state.remove_participant(shard_id, key);
        }
    }

    /// Retire a transport route as its own generation, so the route is gone
    /// from the published view before the allocator may hand its slot out.
    async fn retire_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: TransportHandle,
        key: Option<crate::shard::participants::ParticipantKey>,
    ) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return;
        }
        let generation = self.state.pending().map(|tx| tx.generation);
        let published = generation.and_then(|generation| {
            let view = self.view_mut(shard_id)?;
            view.stage(generation, crate::view::ViewOp::RetireTransport { handle });
            if let Some(key) = key {
                view.stage(generation, crate::view::ViewOp::RemoveParticipant { key });
            }
            view.publish()
        });
        let Some(_) = published else {
            self.abort_transaction(now);
            return;
        };
        let _ = self.state.commit();
        self.state
            .release_transport(shard_id, handle.route.slot(), now);
    }

    pub async fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let room_id = state.room_id;
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
        let Some((handle, binding)) = self.stage_transport(shard_id, state.participant_id).await
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
                self.retire_transport(shard_id, handle, Some(binding)).await;
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

        self.reconcile_room_tracks(room_id).await;

        self.eq.send(
            shard_id,
            ShardCommand::MaterializeParticipant {
                key: binding,
                transport: handle,
                config: Box::new(cfg),
            },
        );
        Ok(answer)
    }

    async fn reconcile_room_tracks(&mut self, room_id: crate::entity::RoomId) {
        let track_ids: Vec<_> = self
            .track_bindings
            .iter()
            .filter_map(|(track_id, binding)| {
                self.core
                    .registry
                    .get_participant(&binding.meta.origin)
                    .is_some_and(|participant| participant.room_id == room_id)
                    .then_some(*track_id)
            })
            .collect();
        for track_id in track_ids {
            self.install_video_runtimes(track_id).await;
            self.install_audio_routes(track_id).await;
            if !self.publish_track_plans(track_id).await {
                debug_assert!(false, "room track reconciliation must publish");
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
}
