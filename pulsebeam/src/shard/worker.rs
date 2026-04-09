use std::collections::VecDeque;

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
};
use str0m::media::KeyframeRequestKind;
use tokio::time::Instant;

use crate::{
    entity::ParticipantId,
    participant::{
        ParticipantConfig, ParticipantCore, ParticipantEvent, ParticipantEvents, RouteUpdater,
    },
    shard::{demux::Demuxer, timer::TimerWheel},
    track::{StreamId, StreamWriter, Track},
};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(Default)]
struct Routing {
    subscribers: IndexSet<ParticipantId>,
}

pub struct Router<'a> {
    participant_id: &'a ParticipantId,
    routes: &'a mut HashMap<StreamId, Routing>,
}

impl RouteUpdater for Router<'_> {
    fn subscribe(&mut self, stream_id: StreamId) {
        let routing = self.routes.entry(stream_id).or_default();
        routing.subscribers.insert(*self.participant_id);
    }

    fn unsubscribe(&mut self, stream_id: &StreamId) {
        let Some(routing) = self.routes.get_mut(stream_id) else {
            return;
        };
        routing.subscribers.swap_remove(self.participant_id);
    }
}

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    PublishTrack(Track, Vec<ParticipantId>),
    RequestKeyframe(ParticipantId, StreamId, KeyframeRequestKind),
}

#[derive(Debug)]
pub enum ShardEvent {
    TrackPublished(Track),
    ParticipantExited(ParticipantId),
    KeyframeRequest {
        origin_participant: ParticipantId,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    },
}

pub struct ShardWorker {
    shard_id: usize,
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    routing: HashMap<StreamId, Routing>,

    recv_batch: Vec<RecvPacketBatch>,
    timers: TimerWheel,
    input_dirty: IndexSet<ParticipantId>,
    fanout_dirty: IndexSet<ParticipantId>,
    events: ParticipantEvents,
    shard_events: VecDeque<ShardEvent>,

    udp_socket: UnifiedSocket,

    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEvent>,
}

impl ShardWorker {
    pub fn new(
        shard_id: usize,
        udp_socket: UnifiedSocket,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEvent>,
    ) -> Self {
        let recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let timers = TimerWheel::default();
        let input_dirty: IndexSet<ParticipantId> = IndexSet::default();
        let fanout_dirty: IndexSet<ParticipantId> = IndexSet::default();
        let events = ParticipantEvents::default();
        let shard_events = VecDeque::with_capacity(1024);

        Self {
            shard_id,
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            routing: HashMap::default(),

            recv_batch,
            timers,
            input_dirty,
            fanout_dirty,
            events,
            shard_events,

            udp_socket,
            command_rx,
            event_tx,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = self.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        loop {
            let wait = async {
                match self.timers.next_deadline() {
                    Some(d) => tokio::time::sleep_until(d).await,
                    // No pending timers: park forever until socket wakes us.
                    None => std::future::pending::<()>().await,
                }
            };

            // Block until at least one source is ready.
            tokio::select! {
                biased;
                _ = wait => {}
                Ok(_) = self.udp_socket.readable() => {}
                Some(cmd) = self.command_rx.recv() => {
                    self.on_command(cmd);
                }
                else => break,
            }

            let now = Instant::now();

            self.timers.drain_expired(now, |participant_id| {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_timeout(now);
                    self.input_dirty.insert(participant_id);
                }
            });

            let count = self
                .udp_socket
                .try_recv_batch(&mut self.recv_batch)
                .unwrap_or_default();
            for batch in self.recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.on_ingress(batch);
                self.input_dirty.insert(participant_id);
            }

            // Poll only participants touched this tick, collect their events.
            for participant_id in &self.input_dirty {
                let Some(participant) = self.participants.get_mut(participant_id) else {
                    continue;
                };
                let mut router = Router {
                    participant_id,
                    routes: &mut self.routing,
                };
                participant.poll(now, &mut self.events, &mut router);
            }

            // Drain all events produced this tick before flushing egress,
            // so RTP forwards from this tick are batched into the same flush.
            while let Some(event) = self.events.pop_front() {
                match event {
                    ParticipantEvent::PublishedRtp(stream_id, pkt) => {
                        let Some(route) = self.routing.get(&stream_id) else {
                            continue;
                        };

                        for participant_id in route.subscribers.clone() {
                            let Some(sub) = self.participants.get_mut(&participant_id) else {
                                continue;
                            };
                            let mut writer = StreamWriter(&mut sub.rtc);
                            sub.downstream.on_forward_rtp(&stream_id, &pkt, &mut writer);
                            self.fanout_dirty.insert(participant_id);
                        }
                    }
                    ParticipantEvent::KeyframeRequest {
                        origin: origin_participant,
                        stream_id,
                        kind,
                    } => {
                        self.shard_events.push_back(ShardEvent::KeyframeRequest {
                            origin_participant,
                            stream_id,
                            kind,
                        });
                    }
                    ParticipantEvent::NewDeadline((deadline, pid)) => {
                        self.timers.schedule(pid, deadline);
                    }
                    ParticipantEvent::PublishedTrack(track) => {
                        self.shard_events
                            .push_back(ShardEvent::TrackPublished(track));
                    }
                    ParticipantEvent::Exited(participant_id) => {
                        self.remove_participant(&participant_id);
                        self.timers.cancel(&participant_id);
                        self.input_dirty.swap_remove(&participant_id);
                        self.shard_events
                            .push_back(ShardEvent::ParticipantExited(participant_id));
                    }
                }
            }

            for participant_id in &self.fanout_dirty {
                let Some(participant) = self.participants.get_mut(participant_id) else {
                    continue;
                };
                let mut router = Router {
                    participant_id,
                    routes: &mut self.routing,
                };
                participant.poll(now, &mut self.events, &mut router);
            }

            // Flush egress for all dirty participants in one pass.
            // Exited participants were swap_removed above so this is safe.
            for participant_id in self
                .input_dirty
                .drain(..)
                .chain(self.fanout_dirty.drain(..))
            {
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.udp_batcher.flush(&self.udp_socket);
                // TODO: TCP
            }

            while let Some(event) = self.shard_events.pop_front() {
                match self.event_tx.try_send(event) {
                    Err(mailbox::TrySendError::Full(e)) => {
                        tracing::warn!("shard event channel is full, piling up shard events");
                        self.shard_events.push_front(e)
                    }
                    Err(mailbox::TrySendError::Closed(e)) => {
                        tracing::warn!("shard event channel is closed, piling up shard events");
                        self.shard_events.push_front(e)
                    }
                    Ok(_) => {}
                }
            }
        }

        Ok(())
    }

    fn on_command(&mut self, cmd: ShardCommand) {
        match cmd {
            ShardCommand::AddParticipant(cfg) => {
                let participant_id = cfg.participant_id;
                self.add_participant(participant_id, cfg);
                // Mark dirty so the initial DTLS/ICE output is flushed this tick.
                self.input_dirty.insert(participant_id);
            }
            ShardCommand::PublishTrack(track, participants) => {
                for participant_id in &participants {
                    let Some(p) = self.participants.get_mut(participant_id) else {
                        tracing::debug!(%participant_id, track = %track.meta.id, "PublishTrack: participant not on this shard (may have exited)");
                        continue;
                    };

                    tracing::debug!(%participant_id, track = %track.meta.id, "delivering published track to subscriber");
                    let mut router = Router {
                        participant_id,
                        routes: &mut self.routing,
                    };
                    p.on_tracks_published(&[track.clone()], &mut router);
                    self.input_dirty.insert(*participant_id);
                }
            }
            ShardCommand::RequestKeyframe(participant_id, stream_id, kind) => {
                let Some(p) = self.participants.get_mut(&participant_id) else {
                    tracing::warn!(%participant_id, ?stream_id, "RequestKeyframe: publisher participant not on this shard");
                    return;
                };
                p.handle_remote_keyframe_request(stream_id, kind);
                self.input_dirty.insert(participant_id);
            }
        }
    }

    fn add_participant(&mut self, participant_id: ParticipantId, cfg: ParticipantConfig) {
        self.remove_participant(&participant_id);

        let mut router = Router {
            participant_id: &participant_id,
            routes: &mut self.routing,
        };
        let mut participant = ParticipantCore::new(cfg, self.udp_socket.max_gso_segments(), 1, &mut router);
        self.demuxer
            .register_ice_ufrag(participant.ufrag().as_bytes(), participant_id);

        self.participants.insert(participant_id, participant);
        tracing::info!(%participant_id, "participant added to shard");
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ParticipantCore> {
        let mut participant = self.participants.remove(participant_id)?;
        // Clean up the shard routing table before teardown.
        // participant is already removed from self.participants so there is no aliasing.
        let mut router = Router {
            participant_id,
            routes: &mut self.routing,
        };
        participant.downstream.unsubscribe_all(&mut router);
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
        for addr in &addrs {
            self.udp_socket.close_peer(addr);
        }
        Some(participant)
    }

}
