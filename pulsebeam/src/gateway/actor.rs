use crate::{entity::ParticipantId, gateway::demux::Demuxer};
use pulsebeam_runtime::actor::{ActorKind, ActorStatus, RunnerConfig};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, net};
use std::{io, sync::Arc};
use tokio::task::JoinSet;

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
    sockets: Vec<net::UnifiedSocketReader>,
    workers: Vec<GatewayWorkerHandle>,

    worker_tasks: JoinSet<(String, ActorStatus)>,
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
        for (id, socket) in self.sockets.drain(..).enumerate() {
            let (worker_handle, worker_task) =
                actor::prepare(GatewayWorkerActor::new(id, socket), RunnerConfig::default());
            self.worker_tasks.spawn(worker_task);
            self.workers.push(worker_handle);
        }

        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Some(Ok((id, _))) = self.worker_tasks.join_next() => {
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
    pub fn new(sockets: Vec<net::UnifiedSocketReader>) -> Self {
        let workers = Vec::with_capacity(sockets.len());
        Self {
            sockets,
            workers,
            worker_tasks: JoinSet::new(),
        }
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayMessageSet>;

pub struct GatewayWorkerActor {
    id: usize,
    socket: net::UnifiedSocketReader,
    demuxer: Demuxer,
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
            _ = self.socket.readable() => {
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
    pub fn new(id: usize, socket: net::UnifiedSocketReader) -> Self {
        let recv_batch = Vec::with_capacity(net::BATCH_SIZE);

        Self {
            id,
            socket,
            recv_batches: recv_batch,
            demuxer: Demuxer::new(),
        }
    }

    async fn read_socket(&mut self) -> io::Result<()> {
        const COOP_BUDGET: usize = 128;
        let mut spent_budget: usize = 0;

        loop {
            self.recv_batches.clear();

            match self.socket.try_recv_batch(&mut self.recv_batches) {
                Ok(_) => {
                    for batch in self.recv_batches.drain(..) {
                        // Calculate logical packets (GRO awareness)
                        let cost = if batch.stride > 0 {
                            std::cmp::max(1, batch.len / batch.stride)
                        } else {
                            1
                        };

                        let src = batch.src;
                        if !self.demuxer.demux(batch).await {
                            self.socket.close_peer(&src);
                        }

                        spent_budget += cost;

                        if spent_budget >= COOP_BUDGET {
                            tokio::task::yield_now().await;
                            spent_budget = 0;
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
