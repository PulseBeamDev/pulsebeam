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

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    RemoveParticipant(ParticipantId),
    AddTcpConnection {
        stream: pulsebeam_runtime::net::tcp::BufferedTcpStream,
        peer_addr: std::net::SocketAddr,
    },
    Cluster(ClusterCommand),
}

#[derive(Debug, Clone)]
pub enum ClusterCommand {
    RequestKeyframe(GlobalKeyframeRequest),
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
    /// Subscriber shard → Publisher shard: the destination installed a route
    /// and is handing over the sender handle. Only on receiving this may the
    /// publisher emit media to `from_shard_id`.
    SubscribeTrack {
        from_shard_id: ShardId,
        track: TrackMeta,
        route: RouteId,
        epoch: u16,
    },
    /// Subscriber shard → Publisher shard: no more local subscribers; stop forwarding.
    UnsubscribeTrack {
        from_shard_id: ShardId,
        track: TrackMeta,
    },
    /// Subscriber shard → Publisher shard: the destination installed a route
    /// for this concrete stream and is handing over the sender handle.
    SubscribeDataTopic {
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        route: Option<RouteId>,
        epoch: u16,
    },
    /// Controller → room shards: a data publisher appeared, so destinations
    /// holding a wildcard subscription for the topic can install a route.
    DataTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    /// Subscriber shard → Publisher shard: hand over the handle for a reliable
    /// stream, or (with no publisher) register interest in the topic.
    SubscribeReliableTopic {
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        route: Option<RouteId>,
        epoch: u16,
    },
    ReliableTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    /// Subscriber shard → Publisher shard: drop every handle held for this
    /// topic; the destination has retired its routes.
    UnsubscribeReliableTopic {
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
    },
    UnsubscribeDataTopic {
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
        publisher: Option<ParticipantId>,
    },
}

/// Payload carried under an [`Envelope`]. Still typed this pass; byte
/// serialization arrives with the UDP transport.
pub enum MediaPayload {
    Video(RtpPacket),
    Audio(RtpPacket),
    Sctp(Vec<u8>),
    ReliableSctp(Vec<u8>),
}

pub enum CrossShardEvent {
    /// Publisher shard → destination shard, addressed by the destination's own
    /// route. Carries no semantic ids: everything needed to deliver it lives in
    /// the destination's compiled route entry.
    Media {
        env: Envelope,
        payload: MediaPayload,
    },
    /// Publisher shard → destination shard: the measurement handles for a
    /// track the destination is about to receive. Sent on the control lane
    /// because it must not be dropped, and shard-to-shard because the
    /// controller must never hold media-path state.
    TrackStates {
        track_id: TrackId,
        states: crate::track::TrackStates,
    },
    /// Subscriber shard → Publisher shard: keyframe request.
    KeyframeRequested(GlobalKeyframeRequest),
    /// A UDP packet batch arrived on this shard but the participant lives elsewhere.
    UdpPacket {
        participant_id: ParticipantId,
        batch: RecvPacketBatch,
    },
    ReliableControlForward {
        publisher: ParticipantId,
        topic: Topic,
        bytes: Vec<u8>,
    },
}

#[derive(Debug)]
pub struct ShardEventWrapper {
    pub from_shard_id: ShardId,
    pub ev: ShardEvent,
}

#[derive(Debug)]
pub enum ShardEvent {
    TrackPublished(Track),
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
    },
    ParticipantExited(ParticipantId),
    KeyframeRequest(GlobalKeyframeRequest),
    /// Subscriber shard → Publisher shard: carries the route the destination
    /// allocated in its own table.
    TrackSubscribed {
        track: TrackMeta,
        route: RouteId,
        epoch: u16,
    },
    /// Subscriber shard → Publisher shard: no more local subscribers; stop forwarding.
    TrackUnsubscribed(TrackMeta),
    DataTopicSubscribed {
        room_id: RoomId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        route: Option<RouteId>,
        epoch: u16,
    },
    /// Publisher shard → controller: announce a data publisher so wildcard
    /// destinations can resolve it into a concrete route.
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
    ReliableTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    ReliableTopicUnsubscribed {
        room_id: RoomId,
        topic: Topic,
    },
    DataTopicUnsubscribed {
        room_id: RoomId,
        topic: Topic,
        publisher: Option<ParticipantId>,
    },
}

#[derive(Clone)]
pub struct ShardContext {
    pub command_tx: mailbox::Sender<ShardCommand>,
    pub metrics: Arc<ShardMetrics>,
}

/// Carries both lanes over in-process channels. Cross-node, `send_media`
/// becomes a UDP datagram of `[envelope || payload]` while `send_control`
/// becomes a reliable gRPC call; this is the only implementation until then.
struct ChannelTransport {
    shard_id: ShardId,
    cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
}

impl ChannelTransport {
    fn enqueue(&self, dst: ShardId, ev: CrossShardEvent) -> bool {
        if dst == self.shard_id {
            return true;
        }
        self.cross_shard_event_txs[dst.index()].try_send(ev).is_ok()
    }
}

impl ShardTransport for ChannelTransport {
    fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload) {
        // Dropping under backpressure is the media contract: this lane is
        // lossy by design, and `link_seq` makes the loss visible downstream.
        let _ = self.enqueue(dst, CrossShardEvent::Media { env, payload });
    }

    fn send_control(&self, dst: ShardId, ev: CrossShardEvent) {
        if !self.enqueue(dst, ev) {
            tracing::warn!(
                from = %self.shard_id,
                %dst,
                "dropped a control message; the cross-shard queue is full"
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
    cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
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
        cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
        cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
        metrics: Arc<ShardMetrics>,
        rng: Rng,
        wall: WallAnchor,
    ) -> Self {
        let core = ShardCore::new(shard_id, udp_socket.max_gso_segments(), rng, wall);
        let router = ChannelTransport {
            shard_id,
            cross_shard_event_txs,
        };

        Self {
            core,
            recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            udp_socket,
            tcp_socket,
            command_rx,
            event_tx,
            cross_shard_event_rx,
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
            Some(_) = self.cross_shard_event_rx.readable() => {}
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
        while let Ok(ev) = self.cross_shard_event_rx.try_recv() {
            self.core.on_cross_shard_event(ev, now, &self.router);
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
