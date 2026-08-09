use std::{marker::PhantomData, pin::Pin, sync::Arc};

use crate::clock::WallAnchor;
use crate::route::{Envelope, RouteId};

use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
    rand::Rng,
};
use tokio::time::{Instant, Sleep};

use crate::{
    entity::{self, ParticipantId, RoomId, TrackId},
    id::ShardId,
    participant::ParticipantConfig,
    rtp::RtpPacket,
    shard::metrics::ShardMetrics,
    track::{GlobalKeyframeRequest, Topic, Track, TrackMeta},
};

use super::core::{ShardCore, ShardTransport};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Manager hung up")]
    ManagerDisconnected,
}

/// Everything the controller sends a shard. This is the control plane: it is
/// reliable, it is semantic, and it is the only source of topology — shards
/// never send each other any of this.
#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    RemoveParticipant(ParticipantId),
    AddTcpConnection {
        stream: pulsebeam_runtime::net::tcp::BufferedTcpStream,
        peer_addr: std::net::SocketAddr,
    },
    RegisterParticipant {
        shard_id: ShardId,
        room_id: RoomId,
        participant_id: entity::ParticipantId,
    },
    UnregisterParticipant {
        shard_id: ShardId,
        room_id: RoomId,
        participant_id: ParticipantId,
    },
    PublishTrack(Track, RoomId),
    UnpublishTracks {
        room_id: RoomId,
        origin: ParticipantId,
        track_ids: Vec<TrackId>,
    },
    /// A [`Topology`] one shard raised, relayed by the controller to the shards
    /// it picked. The controller adds nothing but the origin.
    Relay {
        from_shard_id: ShardId,
        topology: Topology,
    },
}

/// A topology change one shard announces and the controller relays verbatim.
///
/// Both directions carry the same value, so the controller's whole job for
/// these is choosing recipients — it never re-encodes, and there is no second
/// enum to keep in step.
#[derive(Debug, Clone)]
pub enum Topology {
    /// The destination installed a route and is handing over the sender handle.
    /// Only on receiving this may the publisher emit media to it.
    TrackSubscribed {
        track: TrackMeta,
        route: RouteId,
        epoch: u16,
    },
    /// No more local subscribers; stop forwarding. Carries the route it is
    /// retiring so teardown is idempotent: a stale unsubscribe overtaken by a
    /// fresh subscription names the old incarnation and is ignored, instead of
    /// tearing down the new one.
    TrackUnsubscribed {
        track: TrackMeta,
        route: RouteId,
        epoch: u16,
    },
    /// The destination installed a route for this concrete stream, or (with no
    /// publisher) registered a wildcard interest in the topic.
    DataTopicSubscribed {
        room_id: RoomId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        route: Option<RouteId>,
        epoch: u16,
    },
    DataTopicUnsubscribed {
        room_id: RoomId,
        topic: Topic,
        publisher: Option<ParticipantId>,
    },
    /// A data publisher appeared, so destinations holding a wildcard
    /// subscription for the topic can resolve it into a concrete route.
    DataTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    ReliableTopicSubscribed {
        room_id: RoomId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        route: Option<RouteId>,
        epoch: u16,
    },
    /// Drop every handle held for this topic; the destination retired its routes.
    ReliableTopicUnsubscribed { room_id: RoomId, topic: Topic },
    ReliableTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
}

/// Payload carried under an [`Envelope`]. Still typed this pass; byte
/// serialization arrives with the UDP transport.
pub enum MediaPayload {
    Video(RtpPacket),
    Audio(RtpPacket),
    /// SCTP bytes for a client data channel. Which lane the client asked for is
    /// in the destination's route entry, not here — the destination already
    /// knows it, and it describes the client's channel, not this hop.
    Data(Vec<u8>),
}

/// Everything one shard sends another: the data plane.
///
/// Best-effort by construction. Cross-node this becomes a UDP datagram, so
/// nothing that must not be dropped is representable here — topology travels
/// the control plane ([`ShardCommand`] / [`ShardEvent`]) and never this way.
pub enum ShardFrame {
    /// Forward payload, addressed by the destination's own route. Carries no
    /// semantic ids: everything needed to deliver it lives in the destination's
    /// compiled route entry.
    Media {
        env: Envelope,
        payload: MediaPayload,
    },
    /// Upstream feedback toward the publisher. Latest-wins: a newer request for
    /// the same stream supersedes an older one, so both dropping and
    /// superseding one are free.
    Feedback(GlobalKeyframeRequest),
    /// Reverse payload on a data topic, delivered back to its publisher.
    ReverseData {
        publisher: ParticipantId,
        topic: Topic,
        bytes: Vec<u8>,
    },
    /// A datagram batch that landed on the wrong shard's socket. Node-local
    /// with no cross-node analogue — a node demuxes its own participants — so
    /// it is addressed semantically and never leaves the box.
    Ingress {
        participant_id: ParticipantId,
        batch: RecvPacketBatch,
    },
}

#[derive(Debug)]
pub struct ShardEventWrapper {
    pub from_shard_id: ShardId,
    pub ev: ShardEvent,
}

/// Everything a shard tells the controller. The other half of the control
/// plane; like [`ShardCommand`] it is reliable and semantic.
#[derive(Debug)]
pub enum ShardEvent {
    /// A local participant published a track. The controller owns the room, so
    /// it turns this into `PublishTrack` for the shards that need to know.
    TrackPublished(Track),
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
    },
    ParticipantExited(ParticipantId),
    /// A topology change for the controller to relay, unchanged, to whichever
    /// shards it decides are concerned.
    Relay(Topology),
}

#[derive(Clone)]
pub struct ShardContext {
    pub command_tx: mailbox::Sender<ShardCommand>,
    pub metrics: Arc<ShardMetrics>,
}

/// Carries the best-effort lane over in-process channels. Cross-node this
/// becomes a UDP datagram of `[envelope || payload]`; it is the only
/// implementation until then.
struct ChannelTransport {
    shard_id: ShardId,
    frame_txs: Vec<mailbox::Sender<ShardFrame>>,
}

impl ChannelTransport {
    fn enqueue(&self, dst: ShardId, ev: ShardFrame) -> bool {
        if dst == self.shard_id {
            return true;
        }
        self.frame_txs[dst.index()].try_send(ev).is_ok()
    }
}

impl ShardTransport for ChannelTransport {
    fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload) {
        // Dropping under backpressure is the media contract: this lane is
        // lossy by design, and `link_seq` makes the loss visible downstream.
        let _ = self.enqueue(dst, ShardFrame::Media { env, payload });
    }

    fn send_frame(&self, dst: ShardId, frame: ShardFrame) {
        // Same best-effort contract as media, but these are rare enough that a
        // drop is worth seeing: it means the queue is saturated by media.
        if !self.enqueue(dst, frame) {
            tracing::debug!(
                from = %self.shard_id,
                %dst,
                "dropped a cross-shard frame; the queue is full"
            );
        }
    }
}

pub struct ShardWorker {
    core: ShardCore,
    recv_batch: Vec<RecvPacketBatch>,
    udp_socket: UnifiedSocket,
    tcp_socket: net::tcp::TcpTransport,
    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEventWrapper>,
    frame_rx: mailbox::Receiver<ShardFrame>,
    router: ChannelTransport,
    metrics: Arc<ShardMetrics>,

    // Mark !Send
    _marker: PhantomData<*mut ()>,
}

impl ShardWorker {
    pub fn new(
        shard_id: ShardId,
        udp_socket: UnifiedSocket,
        tcp_socket: net::tcp::TcpTransport,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEventWrapper>,
        frame_rx: mailbox::Receiver<ShardFrame>,
        frame_txs: Vec<mailbox::Sender<ShardFrame>>,
        metrics: Arc<ShardMetrics>,
        rng: Rng,
        wall: WallAnchor,
        streams: Arc<crate::stream_registry::StreamRegistry>,
    ) -> Self {
        let core = ShardCore::new(shard_id, udp_socket.max_gso_segments(), rng, wall, streams);
        let router = ChannelTransport {
            shard_id,
            frame_txs,
        };

        Self {
            core,
            recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            udp_socket,
            tcp_socket,
            command_rx,
            event_tx,
            frame_rx,
            router,
            metrics,
            _marker: PhantomData,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = %self.router.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        let sleep = tokio::time::sleep(tokio::time::Duration::MAX);
        tokio::pin!(sleep);

        let mut loop_start = Instant::now();
        loop {
            self.wait_for_inputs(sleep.as_mut()).await?;

            let busy_start = Instant::now();
            self.metrics.record_idle(busy_start - loop_start);

            self.tick(busy_start);
            self.flush_shard_events().await?;

            // TODO: record forwarding latency
            let busy_end = Instant::now();
            loop_start = busy_end;
            let busy_duration = busy_end.duration_since(busy_start);
            self.metrics.record_busy(busy_duration);
        }
    }

    async fn wait_for_inputs(&mut self, mut sleep: Pin<&mut Sleep>) -> Result<(), ShardError> {
        let has_timer = if let Some(d) = self.core.next_timer_deadline() {
            sleep.as_mut().reset(d);
            true
        } else {
            false
        };

        // Block until at least one source is ready.
        tokio::select! {
            biased;
            Ok(_) = self.udp_socket.readable() => {}
            Some(_) = self.frame_rx.readable() => {}
            Ok(_) = self.tcp_socket.readable() => {}
            Some(_) = self.command_rx.readable() => {}
            _ = sleep.as_mut(), if has_timer => {}
            else => return Err(ShardError::ManagerDisconnected),
        }

        Ok(())
    }

    fn tick(&mut self, now: Instant) {
        // phase 1: input
        while let Ok(cmd) = self.command_rx.try_recv() {
            match cmd {
                ShardCommand::AddTcpConnection { stream, peer_addr } => {
                    if let Err(err) = self.tcp_socket.add_connection(stream, peer_addr) {
                        tracing::warn!(%peer_addr, error = ?err, "Failed to add new TCP connection to shard");
                    }
                }
                cmd => {
                    let _ = self.core.on_command(cmd, now, &self.router);
                }
            }
        }
        while let Ok(ev) = self.frame_rx.try_recv() {
            self.core.on_shard_frame(ev, now, &self.router);
        }
        self.core.fire_timers(now);

        let _ = self.udp_socket.try_recv_batch(&mut self.recv_batch);
        let _ = self.tcp_socket.try_recv_batch(&mut self.recv_batch);
        for batch in self.recv_batch.drain(..) {
            self.core.on_udp_batch(batch, &self.router);
        }

        self.core
            .poll_and_flush_dirty(now, &mut self.udp_socket, &mut self.tcp_socket);
        self.core.flush_stream_buffers(&self.router);
        self.core
            .poll_and_flush_dirty(now, &mut self.udp_socket, &mut self.tcp_socket);
        self.core.flush_participant_events(now, &self.router);

        self.core
            .flush_close_peers(&mut self.udp_socket, &mut self.tcp_socket);
    }

    async fn flush_shard_events(&mut self) -> Result<(), ShardError> {
        while let Some(event) = self.core.pop_shard_event() {
            let wrapped = ShardEventWrapper {
                from_shard_id: self.router.shard_id,
                ev: event,
            };
            if let Err(err) = self.event_tx.send(wrapped).await {
                tracing::warn!("shard event channel is closed, exiting: {}", err);
                return Err(ShardError::ManagerDisconnected);
            }
        }

        Ok(())
    }
}
