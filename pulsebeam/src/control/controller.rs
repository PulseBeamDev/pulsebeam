use std::io;
use std::time::Duration;

use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue},
        negotiator::{Negotiator, NegotiatorError},
        router::ShardRouter,
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    shard::{
        ShardContext,
        worker::{ClusterCommand, ShardCommand, ShardEventWrapper},
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
}

impl ControllerActor {
    pub fn new(
        mut rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<Candidate>,
        tcp_listener: pulsebeam_core::net::TcpListener,
    ) -> Self {
        let router = ShardRouter::new(shard_contexts, &mut rng);

        Self {
            router,
            core: ControllerCore::new(),
            negotiator: Negotiator::new(candidates),
            eq: ControllerEventQueue::default(),
            tcp_listener: Some(tcp_listener),
            cluster_id: 0,
            node_id: 0,
        }
    }

    #[fastrace::trace]
    pub async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEventWrapper>,
        shutdown: CancellationToken,
    ) {
        // Spawn the TCP acceptor onto the current LocalSet / LocalRuntime.
        // It owns the listener, enforces caps, reads the first STUN frame from
        // each connection, and sends results back through the mailbox.
        let listener = self
            .tcp_listener
            .take()
            .expect("ControllerActor::run called twice");
        let acceptor = TcpAcceptorHandle::spawn(listener, shutdown.child_token());
        let mut pending_rx = acceptor.event_rx;

        let mut poll_interval = tokio::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(e) = shard_event_rx.recv() => {
                    self.core.process_shard_event(e, &mut self.eq);
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

                Some(cmd) = command_rx.recv() => {
                    self.process_command(cmd);
                }

                _ = shutdown.cancelled() => {
                    break;
                }

                else => break,
            }

            self.drain_core_events().await;
        }
    }

    pub fn process_command(&mut self, cmd: ControllerCommand) {
        match cmd {
            ControllerCommand::CreateParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(m.state, m.offer)
                    .map(|res| CreateParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }

            ControllerCommand::DeleteParticipant(m) => {
                self.core
                    .delete_participant(&m.participant_id, &mut self.eq);
            }
            ControllerCommand::PatchParticipant(m, reply_tx) => {
                let answer = self
                    .handle_patch_participant(m.state, m.offer)
                    .map(|res| PatchParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }
        }
    }

    async fn drain_core_events(&mut self) {
        while let Some(ev) = self.eq.pop() {
            match ev {
                ControllerEvent::ShardCommandBroadcasted(cmd) => self.router.broadcast(cmd).await,
                ControllerEvent::ShardCommandSent(shard_id, cmd) => {
                    self.router.send(shard_id, cmd).await
                }
            }
        }
    }

    pub fn handle_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        self.core
            .delete_participant(&state.participant_id, &mut self.eq);
        self.handle_create_participant(state, offer)
    }

    /// Route an accepted TCP connection to the shard that owns its participant.
    ///
    /// Routing priority:
    /// 1. If the ufrag decodes to a valid `IceUfrag` **for this cluster and node**,
    ///    use the encoded `shard_id` directly — O(1), no lookup.
    /// 2. If the ufrag decodes but targets a *different* cluster or node, the
    ///    connection was misrouted (load-balancer bug) or crafted by an attacker.
    ///    Drop it immediately.
    /// 3. If the ufrag cannot be decoded (old client format during a rolling
    ///    upgrade), fall back to hash(peer_addr) routing.
    /// 4. If no ufrag at all, fall back to hash(peer_addr) routing.
    fn route_tcp_connection(&mut self, conn: PendingTcpConn) {
        let Some(ufrag) = conn.server_ufrag.and_then(|ufrag| IceUfrag::decode(&ufrag)) else {
            crate::log::warn!("invalid ufrag, disconnecting due to likely a malicous actor");
            return;
        };

        if ufrag.cluster_id != self.cluster_id || ufrag.node_id != self.node_id {
            crate::log::warn!(
                peer_addr = %conn.peer_addr,
                ufrag_cluster = ufrag.cluster_id,
                ufrag_node    = ufrag.node_id,
                our_cluster   = self.cluster_id,
                our_node      = self.node_id,
                "TCP connection ufrag targets a different node, dropping"
            );
            return; // stream dropped here, OS socket closed
        }

        self.eq.send(
            ufrag.shard_id,
            ShardCommand::AddTcpConnection {
                stream: conn.stream,
                peer_addr: conn.peer_addr,
            },
        );
    }

    pub fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        // Determine shard first so we can encode it into the ICE ufrag.
        let routing_key = self.core.routing_key(&state.room_id);
        let shard_id = self
            .router
            .try_route(&routing_key)
            .ok_or(ControllerError::ServiceUnavailable)?;

        // Encode routing metadata into the ICE ufrag.  The shard worker and
        // demuxer can decode shard_id / participant_id directly from STUN
        // binding requests — no distributed lookup needed.
        let ufrag = IceUfrag::new(
            self.cluster_id,
            self.node_id,
            shard_id,
            state.participant_id,
        );
        let creds = ufrag.into_ice_creds(&mut pulsebeam_runtime::rand::os_rng());

        let (rtc, answer) = self.negotiator.create_answer(offer, creds)?;
        let cfg = self.core.create_participant(rtc, state, shard_id);

        self.eq.broadcast(ClusterCommand::RegisterParticipant {
            shard_id,
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        });
        self.eq.send(shard_id, ShardCommand::AddParticipant(cfg));
        Ok(answer)
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ShardId;
    use crate::{
        control::{tcp_acceptor::PendingTcpConn, ufrag::IceUfrag},
        entity::ParticipantId,
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

    fn dummy_pid() -> ParticipantId {
        ParticipantId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
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
        ControllerActor::new(seeded_rng(42), shard_contexts, vec![], listener)
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

    #[test]
    fn test_route_valid_ufrag_routes_to_correct_shard() {
        run_local(async {
            let mut actor = make_actor(3).await;
            let (_client, stream) = make_buffered().await;
            let peer_addr = "1.2.3.4:5000".parse().unwrap();

            // Encode for cluster=0, node=0 (actor defaults), shard=2
            let ufrag = IceUfrag::new(0, 0, 2, dummy_pid()).encode();
            let conn = PendingTcpConn {
                stream,
                peer_addr,
                server_ufrag: Some(ufrag),
            };
            actor.route_tcp_connection(conn);

            let event = actor.eq.pop().expect("event must be queued");
            match event {
                ControllerEvent::ShardCommandSent(
                    shard_id,
                    ShardCommand::AddTcpConnection { peer_addr: pa, .. },
                ) => {
                    assert_eq!(
                        shard_id,
                        ShardId::new(2),
                        "must route to the shard encoded in the ufrag"
                    );
                    assert_eq!(pa, peer_addr);
                }
                _ => panic!("unexpected event: {event:?}"),
            }
        });
    }

    #[test]
    fn test_route_wrong_cluster_drops_connection() {
        run_local(async {
            let mut actor = make_actor(2).await;
            // actor.cluster_id = 0, ufrag encodes cluster_id = 1
            let (_client, stream) = make_buffered().await;
            let conn = PendingTcpConn {
                stream,
                peer_addr: "1.2.3.4:5001".parse().unwrap(),
                server_ufrag: Some(IceUfrag::new(1, 0, 0, dummy_pid()).encode()),
            };
            actor.route_tcp_connection(conn);
            assert!(
                actor.eq.pop().is_none(),
                "wrong-cluster connection must be silently dropped"
            );
        });
    }

    #[test]
    fn test_route_wrong_node_drops_connection() {
        run_local(async {
            let mut actor = make_actor(2).await;
            // actor.node_id = 0, ufrag encodes node_id = 7
            let (_client, stream) = make_buffered().await;
            let conn = PendingTcpConn {
                stream,
                peer_addr: "1.2.3.4:5002".parse().unwrap(),
                server_ufrag: Some(IceUfrag::new(0, 7, 0, dummy_pid()).encode()),
            };
            actor.route_tcp_connection(conn);
            assert!(
                actor.eq.pop().is_none(),
                "wrong-node connection must be silently dropped"
            );
        });
    }
}
