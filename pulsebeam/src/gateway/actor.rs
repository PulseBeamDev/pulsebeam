use crate::gateway::demux::Demuxer;
use pulsebeam_runtime::actor::{ActorKind, ActorStatus, RunnerConfig};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::{actor, net};
use std::io;
use std::rc::Rc;
use tokio::task::JoinSet;

#[derive(Clone, Debug)]
pub enum GatewayControlMessage {
    AddParticipant(
        String,
        Rc<pulsebeam_runtime::sync::spsc::Sender<net::RecvPacketBatch>>,
    ),
    RemoveParticipant(String),
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
            self.worker_tasks.spawn_local(worker_task);
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
            worker
                .tx
                .send(msg.clone())
                .await
                .expect("worker is alive until the controller has drained");
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
                let _ = self.read_socket().await;
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
            GatewayControlMessage::AddParticipant(ufrag, handle) => {
                self.demuxer.register_ice_ufrag(ufrag.as_bytes(), handle);
            }
            GatewayControlMessage::RemoveParticipant(ufrag) => {
                self.demuxer.unregister(&mut self.socket, ufrag.as_bytes());
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
        self.recv_batches.clear();
        self.socket.try_recv_batch(&mut self.recv_batches)?;

        for batch in self.recv_batches.drain(..) {
            let src = batch.src;
            if !self.demuxer.demux(&mut self.socket, batch) {
                // In case there's a malicious actor, close immediately as there's no
                // associated participant.
                self.socket.close_peer(&src);
            }
        }
        Ok(())
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
