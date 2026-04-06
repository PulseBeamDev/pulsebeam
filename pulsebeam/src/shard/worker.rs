use std::{cmp::Reverse, collections::BinaryHeap};

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::{
    mailbox,
    net::{self, UnifiedSocket},
};
use tokio::time::Instant;

use crate::{
    entity::ParticipantId,
    participant::{ParticipantConfig, ParticipantCore, ParticipantEvent, ParticipantEvents},
    shard::demux::Demuxer,
    track::{StreamId, TrackMeta},
};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

struct Routing {
    subscribers: IndexSet<ParticipantId>,
}

type TimerEntry = Reverse<(Instant, ParticipantId)>;

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    PublishTrack(Vec<ParticipantId>),
}

#[derive(Debug)]
pub enum ShardEvent {
    TrackPublished(TrackMeta),
    ParticipantExited(ParticipantId),
}

pub struct ShardWorker {
    shard_id: usize,
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    routing: HashMap<StreamId, Routing>,

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
        Self {
            shard_id,
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            routing: HashMap::default(),
            udp_socket,
            command_rx,
            event_tx,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = self.shard_id))]
    pub async fn run(mut self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        let mut recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let mut timer_wheel: BinaryHeap<TimerEntry> = BinaryHeap::new();
        let mut input_dirty: IndexSet<ParticipantId> = IndexSet::default();
        let mut fanout_dirty: IndexSet<ParticipantId> = IndexSet::default();
        let mut events = ParticipantEvents::default();

        loop {
            let next_deadline = timer_wheel.peek().map(|Reverse((t, _))| *t);
            let wait = async move {
                match next_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    // No pending timers: park forever until socket wakes us.
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                biased;
                _ = wait => {}
                Ok(_res) = self.udp_socket.readable() => {}
                Some(cmd) = self.command_rx.recv() => {
                    self.on_command(cmd, &mut input_dirty);
                }
                else => break,
            }

            let now = Instant::now();

            while let Some(Reverse((deadline, participant_id))) = timer_wheel.peek().copied() {
                if deadline > now {
                    break; // nothing else has expired yet
                }
                timer_wheel.pop();

                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue; // already removed
                };
                participant.on_timeout(now);
                input_dirty.insert(participant_id);
            }

            let count = self
                .udp_socket
                .try_recv_batch(&mut recv_batch)
                .unwrap_or_default();
            for batch in recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.on_ingress(batch);
                input_dirty.insert(participant_id);
            }

            // Poll only participants touched this tick, collect their events.
            for &participant_id in &input_dirty {
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.poll(now, &mut events);
            }

            // Drain all events produced this tick before flushing egress,
            // so RTP forwards from this tick are batched into the same flush.
            while let Some(event) = events.pop_front() {
                match event {
                    ParticipantEvent::PublishedRtp(stream_id) => {
                        let Some(route) = self.routing.get(&stream_id) else {
                            continue;
                        };

                        for participant_id in &route.subscribers {
                            let Some(sub) = self.participants.get_mut(&participant_id) else {
                                continue;
                            };
                            sub.on_forward_rtp(&stream_id);
                            fanout_dirty.insert(*participant_id);
                        }
                    }
                    ParticipantEvent::NewDeadline(entry) => {
                        timer_wheel.push(Reverse(entry));
                    }
                    ParticipantEvent::PublishedTrack(track) => {
                        if let Err(e) = self
                            .event_tx
                            .try_send(ShardEvent::TrackPublished(track.meta))
                        {
                            tracing::warn!("event_tx full, dropping TrackPublished: {e:?}");
                        }
                    }
                    ParticipantEvent::Exited(participant_id) => {
                        self.remove_participant(&participant_id);
                        input_dirty.swap_remove(&participant_id);
                        if let Err(e) = self
                            .event_tx
                            .try_send(ShardEvent::ParticipantExited(participant_id))
                        {
                            tracing::warn!("event_tx full, dropping ParticipantExited: {e:?}");
                        }
                    }
                }
            }

            // Flush egress for all dirty participants in one pass.
            // Exited participants were swap_removed above so this is safe.
            for participant_id in input_dirty.drain(..).chain(fanout_dirty.drain(..)) {
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.udp_batcher.flush(&self.udp_socket);
                // TODO: TCP
            }
        }

        Ok(())
    }

    fn on_command(&mut self, cmd: ShardCommand, dirty: &mut IndexSet<ParticipantId>) {
        match cmd {
            ShardCommand::AddParticipant(cfg) => {
                let participant_id = cfg.participant_id;
                self.add_participant(participant_id, cfg);
                // Mark dirty so the initial DTLS/ICE output is flushed this tick.
                dirty.insert(participant_id);
            }
            ShardCommand::PublishTrack(participants) => {
                // TODO:
            }
        }
    }

    fn add_participant(&mut self, participant_id: ParticipantId, cfg: ParticipantConfig) {
        // TODO: handle replacing participants
        self.remove_participant(&participant_id);

        // TODO: update tcp gso size
        let mut participant = ParticipantCore::new(cfg, self.udp_socket.max_gso_segments(), 1);
        self.demuxer
            .register_ice_ufrag(participant.ufrag().as_bytes(), participant_id);
        self.participants.insert(participant_id, participant);
        tracing::info!(%participant_id, "participant added to shard");
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ParticipantCore> {
        let mut participant = self.participants.remove(participant_id)?;
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
        for addr in &addrs {
            self.udp_socket.close_peer(addr);
        }
        Some(participant)
    }
}
