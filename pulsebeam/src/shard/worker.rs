use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
    rand::Rng,
};
use tokio::time::Instant;

use str0m::media::MediaKind;

use crate::{
    entity::{self, ParticipantId, RoomId},
    participant::ParticipantConfig,
    rtp::RtpPacket,
    track::{GlobalKeyframeRequest, StreamId, Track},
};

use super::core::{CrossShardSend, ShardCore};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    RemoveParticipant(ParticipantId),
    Cluster(ClusterCommand),
}

#[derive(Debug, Clone)]
pub enum ClusterCommand {
    PublishTrack(Track, RoomId),
    RequestKeyframe(GlobalKeyframeRequest),
    RegisterParticipant {
        ufrag: String,
        shard_id: usize,
        participant_id: entity::ParticipantId,
    },
    UnregisterParticipant {
        participant_id: ParticipantId,
    },
    UnpublishTracks {
        origin: ParticipantId,
        track_ids: Vec<crate::entity::TrackId>,
    },
}

pub enum CrossShardEvent {
    /// Subscriber shard → Publisher shard: begin forwarding this stream to `from_shard_id`.
    StreamSubscribed {
        stream_id: StreamId,
        kind: MediaKind,
        from_shard_id: usize,
    },
    /// Subscriber shard → Publisher shard: no more local subscribers; stop forwarding.
    StreamUnsubscribed {
        stream_id: StreamId,
        from_shard_id: usize,
    },
    /// Publisher shard → Subscriber shards: carry a video RTP packet across the shard boundary.
    RtpPublished { stream_id: StreamId, pkt: RtpPacket },
    /// Publisher shard → all other shards in the same room: carry an audio RTP packet.
    AudioRtpPublished {
        room_id: RoomId,
        origin: ParticipantId,
        stream_id: StreamId,
        pkt: RtpPacket,
    },
    /// Shard → all others: this shard now has at least one member of `room_id`.
    RoomMemberJoined {
        room_id: RoomId,
        from_shard_id: usize,
    },
    /// Shard → all others: this shard no longer has any members of `room_id`.
    RoomMemberLeft {
        room_id: RoomId,
        from_shard_id: usize,
    },
    /// Subscriber shard → Publisher shard: keyframe request.
    KeyframeRequested(GlobalKeyframeRequest),
    /// A UDP packet batch arrived on this shard but the participant lives elsewhere.
    UdpPacket {
        participant_id: ParticipantId,
        batch: RecvPacketBatch,
    },
}

#[derive(Debug)]
pub enum ShardEvent {
    TrackPublished(Track),
    ParticipantExited(ParticipantId),
    KeyframeRequest(GlobalKeyframeRequest),
}

struct ShardRouter {
    shard_id: usize,
    cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
}

impl CrossShardSend for ShardRouter {
    fn send(&self, shard_id: usize, ev: CrossShardEvent) {
        if shard_id == self.shard_id {
            return;
        }
        let _ = self.cross_shard_event_txs[shard_id].try_send(ev);
    }

    fn broadcast<F: Fn() -> CrossShardEvent>(&self, make_ev: F) {
        for (shard_id, tx) in self.cross_shard_event_txs.iter().enumerate() {
            if shard_id == self.shard_id {
                continue;
            }
            let _ = tx.try_send(make_ev());
        }
    }

    fn shard_id(&self) -> usize {
        self.shard_id
    }
}

pub struct ShardWorker {
    core: ShardCore,
    recv_batch: Vec<RecvPacketBatch>,
    udp_socket: UnifiedSocket,
    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEvent>,
    cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
    router: ShardRouter,
}

impl ShardWorker {
    pub fn new(
        shard_id: usize,
        udp_socket: UnifiedSocket,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEvent>,
        cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
        cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
        rng: Rng,
    ) -> Self {
        let core = ShardCore::new(shard_id, udp_socket.max_gso_segments(), rng);
        let router = ShardRouter {
            shard_id,
            cross_shard_event_txs,
        };
        Self {
            core,
            recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            udp_socket,
            command_rx,
            event_tx,
            cross_shard_event_rx,
            router,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = self.router.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        loop {
            // Compute the deadline before the select so no borrow of self.core
            // outlives this block.
            let deadline = self.core.next_timer_deadline();
            let wait = async move {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    // No pending timers: park forever until socket wakes us.
                    None => std::future::pending::<()>().await,
                }
            };

            // Block until at least one source is ready.
            tokio::select! {
                biased;
                Ok(_) = self.udp_socket.readable() => {}
                Some(cmd) = self.command_rx.recv() => {
                    self.core.on_command(cmd, &self.router);
                }
                Some(ev) = self.cross_shard_event_rx.recv() => {
                    self.core.pending_cross_shard.push_back(ev);
                }
                _ = wait => {}
                else => break,
            }

            while let Ok(cmd) = self.command_rx.try_recv() {
                self.core.on_command(cmd, &self.router);
            }
            while let Ok(ev) = self.cross_shard_event_rx.try_recv() {
                self.core.pending_cross_shard.push_back(ev);
            }

            let now = Instant::now();

            self.core.flush_cross_shard(now, &self.router);
            self.core.fire_timers(now);

            let _ = self
                .udp_socket
                .try_recv_batch(&mut self.recv_batch);
            for batch in self.recv_batch.drain(..) {
                self.core.on_udp_batch(batch, &self.router);
            }

            self.core.poll_input(now);
            self.core.flush_rtp_events(&self.router);
            self.core.poll_fanout(now);
            self.core.flush_participant_events(&self.router);
            self.core.flush_egress(&self.udp_socket);
            self.core.flush_close_peers(&mut self.udp_socket);

            while let Some(event) = self.core.shard_events.pop_front() {
                match self.event_tx.try_send(event) {
                    Err(mailbox::TrySendError::Full(e)) => {
                        tracing::warn!("shard event channel is full, piling up shard events");
                        self.core.shard_events.push_front(e);
                        break;
                    }
                    Err(mailbox::TrySendError::Closed(e)) => {
                        tracing::warn!("shard event channel is closed, piling up shard events");
                        self.core.shard_events.push_front(e);
                        break;
                    }
                    Ok(_) => {}
                }
            }
        }

        Ok(())
    }
}
