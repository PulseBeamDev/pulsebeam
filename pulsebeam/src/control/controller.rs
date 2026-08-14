use std::io;
use std::time::Duration;

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
    route::{RouteHandle, TransportHandle},
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventWrapper},
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
    /// One writer per shard. Never shared, never locked, and never handed to
    /// a shard: the one-publish-per-generation budget is only checkable
    /// because there is exactly one caller.
    views: Vec<crate::view::ShardViewWriter>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        mut rng: pulsebeam_runtime::rand::Rng,
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
        let router = ShardRouter::new(shard_contexts, &mut rng);

        Self {
            router,
            core: ControllerCore::with_placement(room_shard_slot, placement),
            negotiator: Negotiator::new(candidates),
            eq: ControllerEventQueue::new(shard_count),
            tcp_listener: Some(tcp_listener),
            cluster_id: 0,
            node_id: 0,
            state: ControlPlaneState::new(shard_count),
            views,
        }
    }

    fn view_mut(&mut self, shard_id: crate::id::ShardId) -> Option<&mut crate::view::ShardViewWriter> {
        self.views.get_mut(shard_id.index())
    }

    pub async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEventWrapper>,
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
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(e) = shard_event_rx.recv() => {
                    if let Some(e) = self.handle_route_event(e).await {
                        self.core.process_shard_event(e, &mut self.eq);
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
                self.core
                    .delete_participant(&m.participant_id, &mut self.eq);
                self.retire_participant_transport(&m.participant_id).await;
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
                    self.router.send(shard_id, cmd).await;
                }
            }
        }
    }

    pub async fn handle_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        self.core
            .delete_participant(&state.participant_id, &mut self.eq);
        self.retire_participant_transport(&state.participant_id).await;
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
            ShardCommand::AddTcpConnection {
                stream: conn.stream,
                peer_addr: conn.peer_addr,
            },
        );
    }

    /// Buggify's exhaustion fault fires independently per call, so a few
    /// attempts clear a transient one almost certainly; a genuinely full
    /// namespace fails every attempt just as fast, so the retry costs nothing
    /// in that case either.
    const TRANSPORT_ALLOCATION_ATTEMPTS: u32 = 10;

    /// Route lifecycle, as its own generation.
    ///
    /// Returns the event untouched when it is not a route event, so the
    /// ordinary topology projection still sees everything else. Routes are
    /// separated out because a grant is only safe to hand back once the
    /// owning shard has acknowledged the view that carries it, and that is a
    /// barrier the synchronous projection cannot wait on.
    async fn handle_route_event(&mut self, e: ShardEventWrapper) -> Option<ShardEventWrapper> {
        let shard_id = e.from_shard_id;
        match e.ev {
            ShardEvent::RouteNeeded { request, action } => {
                let handle = self.grant_route(shard_id, action).await;
                self.router
                    .send(shard_id, ShardCommand::RouteGranted { request, handle })
                    .await;
                None
            }
            ShardEvent::RouteReleased { handle } => {
                self.release_route(shard_id, handle).await;
                None
            }
            ev => Some(ShardEventWrapper {
                from_shard_id: shard_id,
                ev,
            }),
        }
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
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }
        let handle = match self.state.reserve_endpoint(shard_id, now) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!(%shard_id, ?err, "endpoint route allocation failed");
                self.state.abort(now);
                return None;
            }
        };

        let published = self.state.pending().map(|tx| tx.generation).and_then(|generation| {
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
            view.publish()
        });
        let Some(published) = published else {
            self.state.abort(now);
            return None;
        };
        if !self.await_generation(shard_id, published).await {
            self.state.abort(now);
            return None;
        }
        if self.state.commit().is_err() {
            self.state.abort(now);
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
        let published = self.state.pending().map(|tx| tx.generation).and_then(|generation| {
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
        let Some(published) = published else {
            self.state.abort(now);
            return;
        };
        if !self.await_generation(shard_id, published).await {
            self.state.abort(now);
            return;
        }
        if self.state.commit().is_err() {
            self.state.abort(now);
            return;
        }
        self.state
            .release_endpoint(shard_id, handle.route.slot(), now);
    }

    /// Stage a participant's transport route as one lifecycle generation.
    ///
    /// The control plane allocates the address, the owning shard prepares
    /// only the runtime binding it must build on its own core, and the route
    /// becomes resolvable when the view carrying it publishes. Nothing is
    /// advertised until the shard acknowledges that generation, so the ufrag
    /// this returns always names a route the owner can already resolve.
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
            self.state.abort(now);
            return None;
        };

        let Some(participant_key) = self.prepare_transport(shard_id, participant_id, handle).await
        else {
            self.state.abort(now);
            return None;
        };

        let Some(generation) = self.publish_pending(shard_id, handle, participant_key) else {
            self.state.abort(now);
            return None;
        };

        if !self.await_generation(shard_id, generation).await {
            self.state.abort(now);
            return None;
        }

        if self.state.commit().is_err() {
            self.state.abort(now);
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
        let (reply_tx, reply_rx) = oneshot::channel();
        self.router
            .send(
                shard_id,
                ShardCommand::PrepareTransport {
                    participant_id,
                    handle,
                    reply: reply_tx,
                },
            )
            .await;
        reply_rx.await.ok().flatten()
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

    /// The generation barrier. The shard replies once it has observed a view
    /// at least this new; until then nothing built in this generation is
    /// externally visible.
    async fn await_generation(&mut self, shard_id: crate::id::ShardId, generation: u64) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.router
            .send(
                shard_id,
                ShardCommand::AckGeneration {
                    generation,
                    reply: reply_tx,
                },
            )
            .await;
        let Ok(observed) = reply_rx.await else {
            tracing::warn!(%shard_id, generation, "shard did not acknowledge its view generation");
            return false;
        };
        if let Some(view) = self.view_mut(shard_id) {
            view.observe_ack(observed);
            return view.is_acknowledged(generation);
        }
        false
    }

    /// Retire whatever transport route a participant holds, if the registry
    /// still knows about one.
    async fn retire_participant_transport(&mut self, participant_id: &ParticipantId) {
        let Some((shard_id, handle)) = self.core.registry.transport_of(participant_id) else {
            return;
        };
        self.retire_transport(shard_id, handle).await;
    }

    /// Retire a transport route as its own generation, so the route is gone
    /// from the published view before the allocator may hand its slot out.
    async fn retire_transport(&mut self, shard_id: crate::id::ShardId, handle: TransportHandle) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return;
        }
        let generation = self.state.pending().map(|tx| tx.generation);
        let published = generation.and_then(|generation| {
            let view = self.view_mut(shard_id)?;
            view.stage(
                generation,
                crate::view::ViewOp::RetireTransport { handle },
            );
            view.publish()
        });
        let Some(published) = published else {
            self.state.abort(now);
            return;
        };
        if !self.await_generation(shard_id, published).await {
            self.state.abort(now);
            return;
        }
        let _ = self.state.commit();
        self.state
            .release_transport(shard_id, handle.route.slot(), now);
    }

    pub async fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        // Determine shard first so we can encode it into the ICE ufrag.
        let (slot, placement) = self.core.room_slot(&state.room_id);
        let shard_id = match placement {
            RoomPlacement::Hashed => {
                let routing_key = format!("{}-{}", state.room_id, slot);
                self.router
                    .try_route(&routing_key)
                    .ok_or(ControllerError::ServiceUnavailable)?
            }
            RoomPlacement::RoundRobin => {
                crate::id::ShardId::new(slot.checked_rem(self.router.shard_count()).unwrap_or(0))
            }
        };

        // The transport route has to exist, be published and be acknowledged
        // before the ufrag can carry it, so the whole lifecycle generation
        // runs before negotiation rather than after.
        let Some((handle, binding)) = self
            .stage_transport(shard_id, state.participant_id)
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
        let creds = ufrag.into_ice_creds(&mut pulsebeam_runtime::rand::os_rng());

        let negotiated = self.negotiator.create_answer(offer, creds);
        let (rtc, answer) = match negotiated {
            Ok(negotiated) => negotiated,
            Err(err) => {
                // The route is published and acknowledged but nothing will
                // ever populate it now. Retiring it is a generation of its
                // own — the route must be absent from the published view
                // before its slot can go back to the allocator — and the
                // shard still holds the key it reserved, so both have to be
                // unwound, not just one.
                self.retire_transport(shard_id, handle).await;
                self.router
                    .send(
                        shard_id,
                        ShardCommand::CancelReservation {
                            participant_id: state.participant_id,
                        },
                    )
                    .await;
                return Err(err.into());
            }
        };
        let cfg = self
            .core
            .create_participant(rtc, state, shard_id, Some(handle));
        self.core
            .registry
            .bind_participant(&cfg.participant_id, binding);

        self.eq.broadcast(|| ShardCommand::RegisterParticipant {
            shard_id,
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        });
        self.eq
            .send(shard_id, ShardCommand::AddParticipant(Box::new(cfg)));
        Ok(answer)
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
                    ShardCommand::AddTcpConnection { peer_addr: pa, .. },
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
