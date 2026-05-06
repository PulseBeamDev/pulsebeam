use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};

use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue},
        negotiator::{Negotiator, NegotiatorError},
        router::ShardRouter,
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    shard::{
        ShardContext,
        demux::extract_stun_server_ufrag,
        worker::{ClusterCommand, ShardCommand, ShardEvent},
    },
};
use pulsebeam_runtime::mailbox;
use pulsebeam_runtime::net::tcp::BufferedTcpStream;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::sync::oneshot;

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

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long we wait for the first STUN frame from a newly accepted TCP connection.
/// Two seconds is ample for any legitimate client; a tight window limits how long
/// a slow-loris attacker can occupy a pending slot.
const TCP_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum number of TCP connections waiting for their first STUN frame at once.
/// Prevents memory exhaustion from a flood of half-open connections: each
/// in-flight future holds a kernel socket + a 2-second timer.
#[cfg(not(test))]
const MAX_PENDING_TCP: usize = 1_024;
#[cfg(test)]
const MAX_PENDING_TCP: usize = 4;
/// Maximum pending TCP connections from a single source IP address.
/// A single slow-loris source cannot monopolise the entire `MAX_PENDING_TCP`
/// budget; legitimate clients behind NAT rarely open more than a handful of
/// concurrent TCP connections simultaneously.
const MAX_PENDING_TCP_PER_IP: usize = 16;
/// Maximum additional `pending_tcp` completions drained per `select!` iteration.
/// When many futures complete simultaneously (e.g., a timeout flood), processing
/// them one per outer loop turn would keep the `pending_tcp` arm biased-selected
/// repeatedly, starving the TCP-accept and command arms.  By draining up to this
/// many extra completions inline — without re-entering `select!` — we bound the
/// latency impact while still clearing the backlog quickly.
const PENDING_TCP_DRAIN_BUDGET: usize = 63;

/// Result of the async first-frame read done before routing a TCP connection.
struct PendingTcpConn {
    stream: BufferedTcpStream,
    peer_addr: SocketAddr,
    server_ufrag: Option<String>,
}

pub struct ControllerActor {
    router: ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    eq: ControllerEventQueue,
    tcp_listener: pulsebeam_core::net::TcpListener,
    /// Routing parameters encoded into every ICE ufrag.  Single-node deployments
    /// use 0 for both; set via `NodeBuilder` when multi-node support lands.
    cluster_id: u16,
    node_id: u16,
    /// In-flight futures reading the first STUN frame from newly accepted streams.
    /// Each future carries `peer_addr` in its output so the per-IP counter can
    /// always be decremented on completion, whether framing succeeded or not.
    pending_tcp: FuturesUnordered<
        Pin<Box<dyn Future<Output = (SocketAddr, Option<PendingTcpConn>)> + Send>>,
    >,
    /// Per-source-IP count of in-flight pending TCP futures.
    /// Bounds per-IP slow-loris exposure independently of the global cap.
    pending_tcp_per_ip: HashMap<IpAddr, usize>,
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
            tcp_listener,
            cluster_id: 0,
            node_id: 0,
            pending_tcp: FuturesUnordered::new(),
            pending_tcp_per_ip: HashMap::new(),
        }
    }

    pub async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEvent>,
    ) {
        let mut poll_interval = tokio::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(ev) = shard_event_rx.recv() => {
                    self.core.process_shard_event(ev, &mut self.eq);
                }

                _ = self.core.next_expired() => {}

                _ = poll_interval.tick() => {
                    self.router.poll_loads();
                }

                res = self.tcp_listener.accept() => {
                    match res {
                        Ok((stream, peer_addr)) => {
                            self.queue_pending_tcp(stream, peer_addr);
                        }
                        Err(err) => {
                            tracing::warn!(error = ?err, "TCP accept failed");
                        }
                    }
                }

                Some((peer_addr, maybe_conn)) = self.pending_tcp.next() => {
                    self.on_pending_resolved(peer_addr, maybe_conn);
                    // Drain already-ready completions without re-entering select!.
                    // Prevents a simultaneous timeout flood from keeping the biased
                    // select locked on this arm for O(N) outer-loop iterations,
                    // which would starve the TCP-accept and command arms.
                    let mut budget = PENDING_TCP_DRAIN_BUDGET;
                    while budget > 0 {
                        budget -= 1;
                        match self.pending_tcp.next().now_or_never() {
                            Some(Some((pa, mc))) => self.on_pending_resolved(pa, mc),
                            _ => break, // nothing ready right now
                        }
                    }
                }

                Some(cmd) = command_rx.recv() => {
                    self.process_command(cmd);
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
        let shard_id = match conn.server_ufrag.as_deref() {
            Some(u) => match IceUfrag::decode(u) {
                Some(decoded) => {
                    if decoded.cluster_id != self.cluster_id || decoded.node_id != self.node_id {
                        tracing::warn!(
                            peer_addr = %conn.peer_addr,
                            ufrag_cluster = decoded.cluster_id,
                            ufrag_node    = decoded.node_id,
                            our_cluster   = self.cluster_id,
                            our_node      = self.node_id,
                            "TCP connection ufrag targets a different node, dropping"
                        );
                        return; // stream dropped here, OS socket closed
                    }
                    Some(decoded.shard_id as usize)
                }
                // Unrecognised format (e.g. old client during rolling upgrade).
                None => self.router.try_route(&conn.peer_addr),
            },
            None => self.router.try_route(&conn.peer_addr),
        };

        let Some(shard_id) = shard_id else {
            tracing::warn!(peer_addr = %conn.peer_addr, "No shard available for TCP connection");
            return;
        };

        self.eq.send(
            shard_id,
            ShardCommand::AddTcpConnection {
                stream: conn.stream,
                peer_addr: conn.peer_addr,
            },
        );
    }

    /// Decrements the per-IP pending counter and routes the connection on success.
    /// Called from `run()` whenever a pending future resolves (success or failure).
    fn on_pending_resolved(&mut self, peer_addr: SocketAddr, maybe_conn: Option<PendingTcpConn>) {
        let ip = peer_addr.ip();
        if let Some(c) = self.pending_tcp_per_ip.get_mut(&ip) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.pending_tcp_per_ip.remove(&ip);
            }
        }
        if let Some(conn) = maybe_conn {
            self.route_tcp_connection(conn);
        }
    }

    fn queue_pending_tcp(&mut self, stream: pulsebeam_core::net::TcpStream, peer_addr: SocketAddr) {
        // Total-queue cap: drop connections when the pending queue is full.
        // Dropping `stream` closes the OS socket immediately (TCP RST/FIN),
        // so the peer gets an explicit signal rather than a silent timeout.
        if self.pending_tcp.len() >= MAX_PENDING_TCP {
            tracing::warn!(
                %peer_addr,
                limit = MAX_PENDING_TCP,
                "Pending TCP limit reached, dropping connection"
            );
            return;
        }
        // Per-IP cap: a single slow-loris source cannot monopolise the budget.
        let ip = peer_addr.ip();
        let ip_count = self.pending_tcp_per_ip.entry(ip).or_insert(0);
        if *ip_count >= MAX_PENDING_TCP_PER_IP {
            tracing::warn!(
                %peer_addr,
                limit = MAX_PENDING_TCP_PER_IP,
                "Per-IP pending TCP limit reached, dropping connection"
            );
            return;
        }
        *ip_count += 1;

        let fut: Pin<Box<dyn Future<Output = (SocketAddr, Option<PendingTcpConn>)> + Send>> =
            Box::pin(async move {
                let result = match BufferedTcpStream::read_first_frame(
                    stream,
                    TCP_FIRST_FRAME_TIMEOUT,
                )
                .await
                {
                    Ok((stream, payload)) => {
                        let server_ufrag = extract_stun_server_ufrag(&payload);
                        Some(PendingTcpConn {
                            stream,
                            peer_addr,
                            server_ufrag,
                        })
                    }
                    Err(e) => {
                        tracing::warn!(%peer_addr, error = ?e, "TCP first-frame read failed");
                        None
                    }
                };
                (peer_addr, result)
            });
        self.pending_tcp.push(fut);
    }

    fn remove_ufrag(&mut self, _id: &ParticipantId) {}

    /// Returns the number of in-flight pending TCP connections from `ip`.
    /// Exposed for unit tests that exercise per-IP tracking without going through `run()`.
    #[cfg(test)]
    fn pending_count_for_ip(&self, ip: IpAddr) -> usize {
        self.pending_tcp_per_ip.get(&ip).copied().unwrap_or(0)
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
            shard_id as u8,
            state.participant_id,
        );
        let creds = ufrag.into_ice_creds(&mut pulsebeam_runtime::rand::os_rng());

        let (rtc, answer) = self.negotiator.create_answer(offer, creds)?;
        let cfg = self.core.create_participant(rtc, state, shard_id);

        self.eq.broadcast(ClusterCommand::RegisterParticipant {
            shard_id,
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
    use crate::{
        control::ufrag::IceUfrag,
        entity::ParticipantId,
        shard::{ShardContext, metrics::ShardMetrics},
    };
    use pulsebeam_core::net::TcpListener;
    use pulsebeam_runtime::{mailbox, rand::seeded_rng};
    use std::sync::Arc;

    fn dummy_pid() -> ParticipantId {
        ParticipantId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    }

    async fn make_actor(num_shards: usize) -> ControllerActor {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
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
    /// Returns the raw stream (for queue_pending_tcp) and keeps the client alive.
    async fn accept_one() -> (
        tokio::net::TcpStream,
        pulsebeam_core::net::TcpStream,
        std::net::SocketAddr,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let client = client.unwrap();
        let (server, peer_addr) = accepted.unwrap();
        (client, server, peer_addr)
    }

    /// Wrap a raw server-side stream as a `BufferedTcpStream` for route tests.
    async fn make_buffered() -> (tokio::net::TcpStream, BufferedTcpStream) {
        let (_client, server, _peer) = accept_one().await;
        // _client is held alive for the duration of the caller's test
        (_client, BufferedTcpStream::new(server))
    }

    // ── pending_tcp cap ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_pending_tcp_cap_drops_excess_connections() {
        let mut actor = make_actor(1).await;

        // Fill to the limit; hold client sides alive so OS doesn't reset them.
        let mut clients = Vec::new();
        for _ in 0..MAX_PENDING_TCP {
            let (client, server, peer_addr) = accept_one().await;
            clients.push(client);
            actor.queue_pending_tcp(server, peer_addr);
        }
        assert_eq!(actor.pending_tcp.len(), MAX_PENDING_TCP);

        // The next connection must be dropped immediately (queue stays at cap).
        let (client, server, peer_addr) = accept_one().await;
        clients.push(client);
        actor.queue_pending_tcp(server, peer_addr);
        assert_eq!(
            actor.pending_tcp.len(),
            MAX_PENDING_TCP,
            "excess connection must be dropped, not queued"
        );
    }

    #[tokio::test]
    async fn test_pending_tcp_accepts_after_cap_frees_up() {
        let mut actor = make_actor(1).await;

        // Fill to the limit.
        let mut clients = Vec::new();
        for _ in 0..MAX_PENDING_TCP {
            let (client, server, peer_addr) = accept_one().await;
            clients.push(client);
            actor.queue_pending_tcp(server, peer_addr);
        }
        assert_eq!(actor.pending_tcp.len(), MAX_PENDING_TCP);

        // Drain all futures by dropping the clients (EOF → futures resolve to None).
        clients.clear();
        while let Some((peer_addr, maybe_conn)) = actor.pending_tcp.next().await {
            actor.on_pending_resolved(peer_addr, maybe_conn);
            if actor.pending_tcp.is_empty() {
                break;
            }
        }

        // Now a new connection should be accepted.
        let (client, server, peer_addr) = accept_one().await;
        actor.queue_pending_tcp(server, peer_addr);
        drop(client);
        assert_eq!(
            actor.pending_tcp.len(),
            1,
            "new connection should be queued after cap freed up"
        );
    }

    #[tokio::test]
    async fn test_per_ip_counter_increments_and_decrements() {
        let mut actor = make_actor(1).await;
        let (c1, s1, addr1) = accept_one().await;
        let (c2, s2, addr2) = accept_one().await;
        let ip = addr1.ip(); // both 127.0.0.1
        actor.queue_pending_tcp(s1, addr1);
        actor.queue_pending_tcp(s2, addr2);
        assert_eq!(
            actor.pending_count_for_ip(ip),
            2,
            "two in-flight from same IP"
        );

        // Resolve by dropping clients (EOF) and draining through on_pending_resolved.
        drop(c1);
        drop(c2);
        while let Some((peer_addr, maybe_conn)) = actor.pending_tcp.next().await {
            actor.on_pending_resolved(peer_addr, maybe_conn);
            if actor.pending_tcp.is_empty() {
                break;
            }
        }
        assert_eq!(actor.pending_count_for_ip(ip), 0, "counter must reach zero");
    }

    // ── route_tcp_connection ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_route_valid_ufrag_routes_to_correct_shard() {
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
                assert_eq!(shard_id, 2, "must route to the shard encoded in the ufrag");
                assert_eq!(pa, peer_addr);
            }
            _ => panic!("unexpected event: {event:?}"),
        }
    }

    #[tokio::test]
    async fn test_route_wrong_cluster_drops_connection() {
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
    }

    #[tokio::test]
    async fn test_route_wrong_node_drops_connection() {
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
    }

    #[tokio::test]
    async fn test_route_no_ufrag_falls_back_to_hash() {
        let mut actor = make_actor(2).await;
        let (_client, stream) = make_buffered().await;
        let conn = PendingTcpConn {
            stream,
            peer_addr: "1.2.3.4:5003".parse().unwrap(),
            server_ufrag: None,
        };
        actor.route_tcp_connection(conn);
        let event = actor.eq.pop().expect("hash-fallback must produce an event");
        assert!(
            matches!(
                event,
                ControllerEvent::ShardCommandSent(_, ShardCommand::AddTcpConnection { .. })
            ),
            "must route to some shard via hash(peer_addr)"
        );
    }

    #[tokio::test]
    async fn test_route_garbage_ufrag_falls_back_to_hash() {
        let mut actor = make_actor(2).await;
        let (_client, stream) = make_buffered().await;
        let conn = PendingTcpConn {
            stream,
            peer_addr: "1.2.3.4:5004".parse().unwrap(),
            server_ufrag: Some("garbage!notbase32".to_string()),
        };
        actor.route_tcp_connection(conn);
        let event = actor.eq.pop().expect("hash-fallback must produce an event");
        assert!(
            matches!(
                event,
                ControllerEvent::ShardCommandSent(_, ShardCommand::AddTcpConnection { .. })
            ),
            "undecodable ufrag must fall back to hash routing"
        );
    }
}
