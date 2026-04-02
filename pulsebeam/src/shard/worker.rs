use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap, VecDeque},
};

use ahash::HashMap;
use pulsebeam_runtime::{
    mailbox,
    net::{self, UnifiedSocket},
};
use tokio::time::Instant;

use crate::{
    entity::ParticipantId,
    participant::{ParticipantConfig, ParticipantCore, ParticipantEvents},
    shard::demux::Demuxer,
    track::{StreamId, TrackMeta},
};

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

struct Routing {
    subscibers: BTreeSet<ParticipantId>,
}

type TimerEntry = Reverse<(Instant, ParticipantId)>;

pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
}

pub enum ShardEvent {
    TrackPublished(TrackMeta),
}

pub struct ShardWorker {
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    routing: HashMap<StreamId, Routing>,

    udp_socket: UnifiedSocket,

    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEvent>,
}

impl ShardWorker {
    pub fn new(
        udp_socket: UnifiedSocket,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEvent>,
    ) -> Self {
        Self {
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            routing: HashMap::default(),

            udp_socket,

            command_rx,
            event_tx,
        }
    }

    pub async fn run(mut self) -> Result<(), ShardError> {
        let mut recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let mut timer_wheel = BinaryHeap::new();
        let mut events = ParticipantEvents::default();
        let mut dirty = VecDeque::new();

        loop {
            let wait = async {
                if let Some(Reverse((deadline, _))) = timer_wheel.peek() {
                    tokio::time::sleep_until(*deadline).await;
                } else {
                    // No pending timers: park forever until socket wakes us.
                    std::future::pending::<()>().await;
                }
            };

            tokio::select! {
                biased;
                _ = wait => {}
                res = self.udp_socket.readable() => { res?; }
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
                dirty.push_back(participant_id);
            }

            let count = self.udp_socket.try_recv_batch(&mut recv_batch)?;
            for batch in recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };

                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };

                participant.on_ingress(batch, now, &mut events);
                dirty.push_back(participant_id);
            }

            while let Some(stream_id) = events.published_rtp.pop_front() {
                let Some(route) = self.routing.get(&stream_id) else {
                    continue;
                };

                for participant_id in &route.subscibers {
                    let Some(sub) = self.participants.get_mut(participant_id) else {
                        continue;
                    };

                    sub.on_forward_rtp(&stream_id, &mut events);
                    dirty.push_back(*participant_id);
                }
            }

            while let Some(entry) = events.next_deadlines.pop_front() {
                timer_wheel.push(Reverse(entry));
            }

            while let Some(participant_id) = events.exited.pop_front() {
                self.remove_participant(participant_id);
            }

            while let Some(participant_id) = dirty.pop_front() {
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };

                participant.udp_batcher.flush(&self.udp_socket);
                // TODO: TCP
            }

            // Control plane events
            while let Some(track) = events.published_tracks.pop_front() {
                self.event_tx
                    .send(ShardEvent::TrackPublished(track.meta.clone()));
            }
        }
    }

    fn add_participant(&mut self, _participant_id: ParticipantId) {
        todo!();
    }

    fn remove_participant(&mut self, participantid: ParticipantId) -> Option<ParticipantCore> {
        let mut participant = self.participants.remove(&participantid)?;
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
        for addr in &addrs {
            self.udp_socket.close_peer(addr);
        }
        todo!();
        Some(participant)
    }
}
