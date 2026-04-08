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
    entity::{ParticipantId, ParticipantKey},
    participant::{ParticipantConfig, ParticipantCore, ParticipantEvent, ParticipantEvents},
    shard::{demux::Demuxer, scheduler::ParticipantScheduler, timer::TimerWheel},
    track::{StreamId, StreamWriter, Track},
};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(Default)]
struct Routing {
    subscribers: IndexSet<ParticipantKey>,
}

pub struct Router<'a> {
    participant_key: ParticipantKey,
    routes: &'a mut HashMap<StreamId, Routing>,
}

impl<'a> Router<'a> {
    pub fn subscribe(&mut self, stream_id: StreamId) {
        let routing = self.routes.entry(stream_id).or_default();
        routing.subscribers.insert(self.participant_key);
    }

    pub fn unsubscribe(&mut self, stream_id: &StreamId) {
        let Some(routing) = self.routes.get_mut(stream_id) else {
            return;
        };
        routing.subscribers.swap_remove(&self.participant_key);
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
    scheduler: ParticipantScheduler,
    routing: HashMap<StreamId, Routing>,

    recv_batch: Vec<RecvPacketBatch>,
    timers: TimerWheel,
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
        let events = ParticipantEvents::default();
        let shard_events = VecDeque::with_capacity(1024);

        Self {
            shard_id,
            demuxer: Demuxer::default(),
            scheduler: ParticipantScheduler::new(),
            routing: HashMap::default(),

            recv_batch,
            timers,
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

            self.timers.drain_expired(now, |key| {
                if let Some(p) = self.scheduler.participants[key as usize].as_mut() {
                    p.on_timeout(now);
                    self.scheduler.input_dirty.set(key as usize, true);
                }
            });

            let count = self
                .udp_socket
                .try_recv_batch(&mut self.recv_batch)
                .unwrap_or_default();
            for batch in self.recv_batch.drain(..count) {
                let Some(key) = self.demuxer.demux(&batch) else {
                    continue;
                };
                let Some(p) = self.scheduler.participants[key as usize].as_mut() else {
                    continue;
                };
                p.on_ingress(batch);
                self.scheduler.input_dirty.set(key as usize, true);
            }

            // Poll only participants touched this tick, collect their events.
            for slot in self.scheduler.input_dirty.iter_ones() {
                let Some(participant) = self.scheduler.participants[slot].as_mut() else {
                    continue;
                };
                let mut router = Router {
                    participant_key: slot as ParticipantKey,
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

                        for key in route.subscribers.clone() {
                            let Some(sub) = self.scheduler.participants[key as usize].as_mut()
                            else {
                                continue;
                            };
                            let mut writer = StreamWriter(&mut sub.rtc);
                            sub.downstream.on_forward_rtp(&stream_id, &pkt, &mut writer);
                            self.scheduler.fanout_dirty.set(key as usize, true);
                        }
                    }
                    ParticipantEvent::KeyframeRequest {
                        origin: origin_participant,
                        stream_id,
                        kind,
                    } => {
                        if let Some(slot) = self.scheduler.get_slot(&origin_participant)
                            && let Some(publisher) =
                                self.scheduler.participants[slot as usize].as_mut()
                        {
                            // Same-shard: handle directly without going through controller.
                            publisher.handle_remote_keyframe_request(stream_id, kind);
                            self.scheduler.fanout_dirty.set(slot as usize, true);
                        } else {
                            // Cross-shard: forward to controller.
                            self.shard_events.push_back(ShardEvent::KeyframeRequest {
                                origin_participant,
                                stream_id,
                                kind,
                            });
                        }
                    }
                    ParticipantEvent::NewDeadline((deadline, key)) => {
                        self.timers.schedule(key, deadline);
                    }
                    ParticipantEvent::PublishedTrack(track) => {
                        self.shard_events
                            .push_back(ShardEvent::TrackPublished(track));
                    }
                    ParticipantEvent::Exited(key) => {
                        let participant_id = self.scheduler.participants[key as usize]
                            .as_ref()
                            .map(|p| p.participant_id);
                        self.remove_participant(key);
                        self.timers.cancel(key);
                        self.shard_events
                            .push_back(ShardEvent::ParticipantExited(participant_id.unwrap()));
                    }
                }
            }

            for slot in self.scheduler.fanout_dirty.iter_ones() {
                let Some(participant) = self.scheduler.participants[slot].as_mut() else {
                    continue;
                };
                let mut router = Router {
                    participant_key: slot as ParticipantKey,
                    routes: &mut self.routing,
                };
                participant.poll(now, &mut self.events, &mut router);
            }

            // Flush egress for all dirty participants in one pass.
            for slot in self
                .scheduler
                .input_dirty
                .iter_ones()
                .chain(self.scheduler.fanout_dirty.iter_ones())
            {
                let Some(participant) = self.scheduler.participants[slot].as_mut() else {
                    continue;
                };
                participant.udp_batcher.flush(&self.udp_socket);
                // TODO: TCP
            }
            self.scheduler.input_dirty = bitvec::array::BitArray::ZERO;
            self.scheduler.fanout_dirty = bitvec::array::BitArray::ZERO;

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
                if let Some(key) = self.add_participant(cfg) {
                    // Mark dirty so the initial DTLS/ICE output is flushed this tick.
                    self.scheduler.input_dirty.set(key as usize, true);
                }
            }
            ShardCommand::PublishTrack(track, participants) => {
                for participant_id in &participants {
                    let Some(slot) = self.scheduler.get_slot(participant_id) else {
                        tracing::debug!(%participant_id, track = %track.meta.id, "PublishTrack: participant not on this shard (may have exited)");
                        continue;
                    };
                    let Some(p) = self.scheduler.participants[slot as usize].as_mut() else {
                        continue;
                    };

                    tracing::debug!(%participant_id, track = %track.meta.id, "delivering published track to subscriber");
                    p.on_tracks_published(&[track.clone()]);
                    self.scheduler.input_dirty.set(slot as usize, true);
                }
            }
            ShardCommand::RequestKeyframe(participant_id, stream_id, kind) => {
                let Some(slot) = self.scheduler.get_slot(&participant_id) else {
                    tracing::warn!(%participant_id, ?stream_id, "RequestKeyframe: publisher participant not on this shard");
                    return;
                };
                let Some(p) = self.scheduler.participants[slot as usize].as_mut() else {
                    return;
                };
                p.handle_remote_keyframe_request(stream_id, kind);
                self.scheduler.input_dirty.set(slot as usize, true);
            }
        }
    }

    fn add_participant(&mut self, cfg: ParticipantConfig) -> Option<ParticipantKey> {
        let participant_id = cfg.participant_id;
        // Evict any existing participant with the same id.
        if let Some(existing_key) = self.scheduler.get_slot(&participant_id) {
            self.remove_participant(existing_key);
            self.timers.cancel(existing_key);
        }

        let mut participant = ParticipantCore::new(cfg, self.udp_socket.max_gso_segments(), 1);
        let ufrag = participant.ufrag();
        let key = self.scheduler.insert(participant_id, participant)?;
        self.demuxer.register_ice_ufrag(ufrag.as_bytes(), key);
        tracing::info!(%participant_id, "participant added to shard");
        Some(key)
    }

    fn remove_participant(&mut self, key: ParticipantKey) -> Option<ParticipantCore> {
        let mut participant = self.scheduler.remove(key)?;
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
        for addr in &addrs {
            self.udp_socket.close_peer(addr);
        }
        Some(participant)
    }
}
