use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue},
        negotiator::{Negotiator, NegotiatorError},
        router::ShardRouter,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    shard::{
        demux::extract_stun_server_ufrag,
        ShardContext,
        worker::{ClusterCommand, ShardCommand, ShardEvent},
    },
};
use futures_util::{Future, StreamExt, stream::FuturesUnordered};
use pulsebeam_core::net::TcpStream;
use pulsebeam_runtime::mailbox;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::io::AsyncReadExt;
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
const TCP_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum allowed size for the first STUN frame (same as MAX_FRAME_SIZE in tcp.rs).
const MAX_FIRST_FRAME_SIZE: usize = 1500;

/// Result of the async first-frame read done before routing a TCP connection.
struct PendingTcpConn {
    stream: TcpStream,
    peer_addr: SocketAddr,
    payload: Vec<u8>,
    server_ufrag: Option<String>,
}

pub struct ControllerActor {
    router: ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    eq: ControllerEventQueue,
    tcp_listener: pulsebeam_core::net::TcpListener,
    /// Maps server ICE ufrag → shard_id so that TCP connections can be routed to
    /// the correct shard instead of relying on hash(peer_addr).
    ufrag_shard: std::collections::HashMap<String, usize>,
    /// Reverse index for cleanup on participant deletion.
    participant_ufrag: std::collections::HashMap<ParticipantId, String>,
    /// In-flight futures that read the first STUN frame from a newly accepted
    /// TCP stream so we can extract the ICE ufrag for routing.
    pending_tcp: FuturesUnordered<Pin<Box<dyn Future<Output = Option<PendingTcpConn>> + Send>>>,
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
            ufrag_shard: Default::default(),
            participant_ufrag: Default::default(),
            pending_tcp: FuturesUnordered::new(),
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
                    // Clean up ufrag maps before delegating to core.
                    if let ShardEvent::ParticipantExited(ref id) = ev {
                        self.remove_ufrag(id);
                    }
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

                // A pending first-frame read completed (or timed out).
                Some(maybe_conn) = self.pending_tcp.next() => {
                    if let Some(conn) = maybe_conn {
                        self.route_tcp_connection(conn);
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
                // Clean up our ufrag index before core removes the participant.
                self.remove_ufrag(&m.participant_id);
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

    /// Remove the ufrag index entries for a participant that is leaving.
    fn remove_ufrag(&mut self, id: &ParticipantId) {
        if let Some(ufrag) = self.participant_ufrag.remove(id) {
            self.ufrag_shard.remove(&ufrag);
        }
    }

    /// Push an async first-frame read onto `pending_tcp` so we can extract the
    /// ICE ufrag before routing the connection to a shard.
    fn queue_pending_tcp(&mut self, stream: TcpStream, peer_addr: SocketAddr) {
        let fut: Pin<Box<dyn Future<Output = Option<PendingTcpConn>> + Send>> =
            Box::pin(read_first_tcp_frame(stream, peer_addr));
        self.pending_tcp.push(fut);
    }

    /// Route a TCP connection whose first STUN frame has already been read.
    ///
    /// We look up `server_ufrag` in `ufrag_shard` to find the correct shard.
    /// If the ufrag is unknown (connection arrived before the participant was
    /// registered, or it's garbage traffic) we still route — we fall back to
    /// hash(peer_addr) which keeps single-shard deployments working.
    fn route_tcp_connection(&mut self, conn: PendingTcpConn) {
        let shard_id = conn
            .server_ufrag
            .as_deref()
            .and_then(|u| self.ufrag_shard.get(u).copied())
            .or_else(|| self.router.try_route(&conn.peer_addr));

        let Some(shard_id) = shard_id else {
            tracing::warn!(peer_addr = %conn.peer_addr, "No shard available for TCP connection");
            return;
        };

        self.eq.send(
            shard_id,
            ShardCommand::AddTcpConnection {
                stream: conn.stream,
                peer_addr: conn.peer_addr,
                initial_payload: conn.payload,
            },
        );
    }

    pub fn handle_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let (rtc, answer) = self.negotiator.create_answer(offer)?;
        let routing_key = self.core.routing_key(&state.room_id);
        let shard_id = self
            .router
            .try_route(&routing_key)
            .ok_or(ControllerError::ServiceUnavailable)?;
        let mut cfg = self.core.create_participant(rtc, state, shard_id);
        let ufrag = cfg.ufrag();

        // Register ufrag → shard mapping so that TCP connections can be routed
        // to the shard that owns this participant instead of relying on hash(peer_addr).
        self.ufrag_shard.insert(ufrag.clone(), shard_id);
        self.participant_ufrag.insert(cfg.participant_id, ufrag.clone());

        self.eq.broadcast(ClusterCommand::RegisterParticipant {
            shard_id,
            participant_id: cfg.participant_id,
            ufrag,
        });
        self.eq.send(shard_id, ShardCommand::AddParticipant(cfg));
        Ok(answer)
    }
}

/// Async helper: read the first RFC 4571 frame from a newly accepted TCP
/// stream so that the controller can extract the ICE ufrag for routing.
///
/// Returns `None` on timeout or I/O error so the connection is silently dropped.
async fn read_first_tcp_frame(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
) -> Option<PendingTcpConn> {
    let result = tokio::time::timeout(TCP_FIRST_FRAME_TIMEOUT, async {
        // Read the 2-byte RFC 4571 length prefix.
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await?;
        let len = u16::from_be_bytes(header) as usize;

        if len == 0 || len > MAX_FIRST_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("first TCP frame length {len} out of range"),
            ));
        }

        // Read the payload.
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await?;
        Ok(payload)
    })
    .await;

    match result {
        Ok(Ok(payload)) => {
            let server_ufrag = extract_stun_server_ufrag(&payload);
            Some(PendingTcpConn {
                stream,
                peer_addr,
                payload,
                server_ufrag,
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(%peer_addr, error = ?e, "Error reading first TCP frame");
            None
        }
        Err(_timeout) => {
            tracing::warn!(%peer_addr, "TCP first-frame read timed out");
            None
        }
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;
