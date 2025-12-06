use crate::{entity::ParticipantId, gateway::demux::Demuxer};
use futures::{StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, net};
use std::{io, sync::Arc};

#[derive(Clone)]
pub enum GatewayControlMessage {
    AddParticipant(
        Arc<ParticipantId>,
        String,
        mailbox::Sender<net::RecvPacketBatch>,
    ),
    RemoveParticipant(Arc<ParticipantId>),
}

pub struct GatewayMessageSet;

impl actor::MessageSet for GatewayMessageSet {
    type Msg = GatewayControlMessage;
    type Meta = String;
    type ObservableState = ();
}

pub struct GatewayActor {
    sockets: Vec<Arc<net::UnifiedSocket>>,
    workers: Vec<GatewayWorkerHandle>,

    worker_tasks: FuturesUnordered<actor::JoinHandle<GatewayMessageSet>>,
}

impl actor::Actor<GatewayMessageSet> for GatewayActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> ActorKind {
        "gateway_controller"
    }

    fn meta(&self) -> String {
        "main".to_string()
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

    async fn on_msg(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
        msg: GatewayControlMessage,
    ) -> () {
        for worker in &mut self.workers {
            worker.tx.send(msg.clone()).await;
        }
    }
}

impl GatewayActor {
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
    batcher: net::RecvPacketBatcher,
    recv_batches: Vec<net::RecvPacketBatch>,
}

impl actor::Actor<GatewayMessageSet> for GatewayWorkerActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> ActorKind {
        "gateway_worker"
    }

    fn meta(&self) -> String {
        self.id.to_string()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<GatewayMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Ok(_) = self.socket.readable() => {
                self.read_socket().await;
            }
        });

        Ok(())
    }

    async fn on_msg(
        &mut self,
        _ctx: &mut actor::ActorContext<GatewayMessageSet>,
        msg: <GatewayMessageSet as actor::MessageSet>::Msg,
    ) -> () {
        match msg {
            GatewayControlMessage::AddParticipant(participant_id, ufrag, handle) => {
                self.demuxer
                    .register_ice_ufrag(participant_id, ufrag.as_bytes(), handle);
            }
            GatewayControlMessage::RemoveParticipant(participant_id) => {
                self.demuxer.unregister(&participant_id);
            }
        };
    }
}

impl GatewayWorkerActor {
    pub fn new(id: usize, socket: Arc<net::UnifiedSocket>) -> Self {
        let recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let batcher = net::RecvPacketBatcher::new();

        Self {
            id,
            socket,
            batcher,
            recv_batches: recv_batch,
            demuxer: Demuxer::new(),
        }
    }

    async fn read_socket(&mut self) -> io::Result<()> {
        let mut budget: usize = 256;
        while budget > 0 {
            self.recv_batches.clear();
            match self
                .socket
                .try_recv_batch(&mut self.batcher, &mut self.recv_batches)
            {
                Ok(_) => {
                    for batch in self.recv_batches.drain(..) {
                        let count = if batch.stride > 0 {
                            std::cmp::max(1, batch.len / batch.stride)
                        } else {
                            1
                        };
                        budget = budget.saturating_sub(count);
                        self.demuxer.demux(batch).await;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Socket empty, wait until ready again.
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }

        tokio::task::yield_now().await;
        Ok(())
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
