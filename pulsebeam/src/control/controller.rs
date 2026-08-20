use std::io;
use std::time::Duration;

use ahash::{HashMap, HashMapExt};

use crate::control::state::ControlPlaneState;
use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue, RoomPlacement},
        lanes::{Lanes, StreamLane, Subscriber, WildcardSubscriber},
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
    /// Data and reliable stream routing. One type per lane rather than three
    /// fields duplicated per lane, so the two cannot drift.
    lanes: Lanes,
    pending_track_subscriptions: HashMap<crate::entity::TrackId, Vec<PendingTrackSubscription>>,
    pending_track_counts: HashMap<ParticipantId, usize>,
    #[cfg(not(feature = "sim"))]
    steering: Option<crate::ebpf::Steering>,
    /// Subscriptions whose route could not be allocated, waiting for a retry.
    ///
    /// A subscribe carries its own retry trigger only if something later
    /// re-emits it, and nothing does: the subscriber's shard already believes
    /// it subscribed. Without this the first failed allocation leaves that
    /// participant with no video for the rest of its session.
    deferred_subscribes: std::collections::VecDeque<DeferredSubscribe>,
}

#[derive(Debug, Clone)]
struct DeferredSubscribe {
    shard_id: crate::id::ShardId,
    subscriber: ParticipantId,
    subscriber_key: crate::shard::participants::ParticipantKey,
    slot: crate::keys::DownstreamSlotKey,
    track: crate::track::TrackMeta,
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
            lanes: Lanes::new(),
            pending_track_subscriptions: HashMap::new(),
            pending_track_counts: HashMap::new(),
            #[cfg(not(feature = "sim"))]
            steering: None,
            deferred_subscribes: std::collections::VecDeque::new(),
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

    /// Buggify's exhaustion fault fires independently per call, so a few
    /// attempts clear a transient one almost certainly; a genuinely full
    /// namespace fails every attempt just as fast, so the retry costs nothing
    /// in that case either.
    const ROUTE_ALLOCATION_ATTEMPTS: u32 = 10;

    /// Bounded so a node that is genuinely out of routes sheds the backlog
    /// instead of growing one. A subscriber dropped here is no worse off than
    /// it was before the queue existed.
    const MAX_DEFERRED_SUBSCRIBES: usize = 1024;

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

    fn defer_subscribe(&mut self, deferred: DeferredSubscribe) {
        metrics::counter!("subscribe_deferred").increment(1);
        if self.deferred_subscribes.len() >= Self::MAX_DEFERRED_SUBSCRIBES {
            metrics::counter!("subscribe_deferred_dropped").increment(1);
            self.deferred_subscribes.pop_front();
        }
        self.deferred_subscribes.push_back(deferred);
    }

    /// Retry every deferred subscription once.
    ///
    /// One pass, not a loop until success: a retry that fails is re-deferred
    /// by the same path that deferred it originally, so the next tick picks it
    /// up. Draining the queue into a local first is what keeps that from
    /// spinning within one tick.
    async fn retry_deferred_subscribes(&mut self) {
        if self.deferred_subscribes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.deferred_subscribes);
        for deferred in pending {
            let DeferredSubscribe {
                shard_id,
                subscriber,
                subscriber_key,
                slot,
                track,
            } = deferred;
            // A participant that left, or a track that was retired, takes its
            // deferred work with it rather than being resurrected here.
            if self.core.registry.get_participant(&subscriber).is_none()
                || !self.track_bindings.contains_key(&track.id)
            {
                continue;
            }
            self.on_track_subscribed(shard_id, subscriber, subscriber_key, slot, track)
                .await;
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
                shard,
            } => {
                debug_assert_eq!(shard, shard_id);
                let Ok(shard_index) = u16::try_from(shard.index()) else {
                    debug_assert!(false, "a shard id must fit the steering map value");
                    return None;
                };
                self.pin_flow_to_owner(source, destination, shard_index);
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
