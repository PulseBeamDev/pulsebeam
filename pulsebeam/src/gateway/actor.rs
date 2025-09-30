use crate::{
    gateway::demux::{DemuxResult, Demuxer},
    participant,
};
use futures::{StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::{actor, net};
use std::{io, sync::Arc};

#[derive(Debug, Clone)]
pub enum GatewayControlMessage {
    AddParticipant(String, participant::ParticipantHandle),
    RemoveParticipant(String),
}

pub struct GatewayMessageSet;

impl actor::MessageSet for GatewayMessageSet {
    type HighPriorityMsg = GatewayControlMessage;
    type LowPriorityMsg = ();
    type Meta = usize;
    type ObservableState = ();
}

pub struct GatewayActor {
    sockets: Vec<Arc<net::UnifiedSocket>>,
    workers: Vec<GatewayWorkerHandle>,

    worker_tasks: FuturesUnordered<actor::JoinHandle<GatewayMessageSet>>,
}

impl actor::Actor<GatewayMessageSet> for GatewayActor {
    fn meta(&self) -> usize {
        0
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<GatewayMessageSet>,
    ) -> Result<(), actor::ActorError> {
        for (id, socket) in self.sockets.iter().enumerate() {
            let (worker_handle, worker_join) =
                actor::spawn_default(GatewayWorkerActor::new(id, socket.clone()));
            self.worker_tasks.push(worker_join);
            self.workers.push(worker_handle);
        }

        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Some((id, _)) = self.worker_tasks.next() => {
                tracing::info!("gateway worker-{id} has exited");
                break;
            }
        });

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
        msg: GatewayControlMessage,
    ) -> () {
        for worker in &mut self.workers {
            worker.hi_tx.send(msg.clone()).await;
        }
    }
}

impl GatewayActor {
    const BATCH_SIZE: usize = 64;

    pub fn new(sockets: Vec<Arc<net::UnifiedSocket>>) -> Self {
        let workers = Vec::with_capacity(sockets.len());
        Self {
            sockets,
            workers,
            worker_tasks: FuturesUnordered::new(),
        }
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayMessageSet>;

pub struct GatewayWorkerActor {
    id: usize,
    socket: Arc<net::UnifiedSocket>,
    demuxer: Demuxer,
    recv_batch: Vec<net::RecvPacket>,
}

impl actor::Actor<GatewayMessageSet> for GatewayWorkerActor {
    fn meta(&self) -> usize {
        self.id
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<GatewayMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Ok(_) = self.socket.readable() => {
                if let Err(err) = self.read_socket() {
                    tracing::error!("failed to read socket: {err}");
                    break;
                }
            }
        });

        Ok(())
    }
}

impl GatewayWorkerActor {
    const BATCH_SIZE: usize = 64;
    pub fn new(id: usize, socket: Arc<net::UnifiedSocket>) -> Self {
        // Pre-allocate receive batch with MTU-sized buffers
        let recv_batch = Vec::with_capacity(Self::BATCH_SIZE);

        Self {
            id,
            socket,
            recv_batch,
            demuxer: Demuxer::new(),
        }
    }

    fn read_socket(&mut self) -> io::Result<()> {
        // the loop after reading should always clear the buffer
        assert!(self.recv_batch.is_empty());
        let batch_size = self.recv_batch.capacity();
        let count = self
            .socket
            .try_recv_batch(&mut self.recv_batch, batch_size)?;

        tracing::trace!("received {count} packets from socket");
        for packet in self.recv_batch.iter() {
            match self.demuxer.demux(packet.src, &packet.buf) {
                DemuxResult::Participant(handle) => todo!(),
                DemuxResult::Rejected(reason) => tracing::debug!("rejected packet: {reason:?}"),
            }
        }
        self.recv_batch.clear();

        Ok(())
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
