use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
};

use ahash::HashMap;
use pulsebeam_runtime::net::{self, UnifiedSocketReader, UnifiedSocketWriter};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, TrackId},
    participant::{ParticipantCore, ParticipantEvents},
    shard::demux::Demuxer,
    track::{StreamId, TrackReceiver},
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

pub struct ShardWorker {
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    routing: HashMap<StreamId, Routing>,

    udp_socket_rx: UnifiedSocketReader,
    udp_socket_tx: UnifiedSocketWriter,
}

impl ShardWorker {
    pub async fn run(&mut self) -> Result<(), ShardError> {
        let mut recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let mut timer_wheel = BinaryHeap::new();
        let mut participant_exited = BTreeSet::new();
        let mut events = ParticipantEvents::default();
        let mut exited = VecDeque::new();
        let mut dirty = BTreeSet::new();

        loop {
            self.udp_socket_rx.readable().await?;
            // TODO: how do we handle time here, do we need to be accurate?
            let now = Instant::now();

            let count = self.udp_socket_rx.try_recv_batch(&mut recv_batch)?;

            for batch in recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };

                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };

                participant.on_ingress(batch, now, &mut events);
                dirty.insert(participant_id);
            }

            while let Some(stream_id) = events.published_rtp.pop_front() {
                let Some(route) = self.routing.get(&stream_id) else {
                    continue;
                };

                for participant_id in &route.subscibers {
                    let Some(sub) = self.participants.get_mut(&participant_id) else {
                        continue;
                    };

                    sub.on_forward_rtp(&stream_id, &mut events);
                    dirty.insert(*participant_id);
                }
            }

            while let Some(entry) = events.next_deadlines.pop_front() {
                timer_wheel.push(entry);
            }

            while let Some(participant_id) = events.exited.pop_front() {
                // TODO: participant cleanup
            }
        }
    }
}
