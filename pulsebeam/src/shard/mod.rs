mod demux;

use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap, VecDeque},
};

use ahash::HashMap;
use pulsebeam_runtime::net::{self, RecvPacketBatch, UnifiedSocketReader, UnifiedSocketWriter};
use tokio::time::Instant;

use crate::{entity::ParticipantId, participant::ParticipantCore, shard::demux::Demuxer};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

pub struct Shard {
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,

    udp_socket_rx: UnifiedSocketReader,
    udp_socket_tx: UnifiedSocketWriter,
}

impl Shard {
    pub async fn run(&self) -> Result<(), ShardError> {
        let mut recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let mut timer_wheel = BinaryHeap::new();
        let mut participant_exited = BTreeSet::new();

        loop {
            self.udp_socket_rx.readable().await?;
            // TODO: how do we handle time here, do we need to be accurate?
            let now = Instant::now();

            let count = self.udp_socket_rx.try_recv_batch(&mut recv_batch)?;

            for batch in recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };

                let Some(participant) = self.participants.get(&participant_id) else {
                    continue;
                };

                if let Some(deadline) = participant.handle_udp_packet_batch(batch, now) {
                    timer_wheel.push(Reverse(deadline));
                } else {
                    participant_exited.insert(participant_id);
                }
            }
        }
    }
}
