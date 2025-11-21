use crate::{entity::ParticipantId, gateway::demux::Demuxer};
use futures::{StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, net};
use std::{io, sync::Arc};

#[derive(Clone)]
pub enum GatewayControlMessage {
    AddParticipant(Arc<ParticipantId>, String, mailbox::Sender<net::RecvPacket>),
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

    fn meta(&self) -> String {
        "gateway-controller".to_string()
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
    recv_batch: Vec<net::RecvPacket>,
    storage: net::RecvBatchStorage,
}

impl actor::Actor<GatewayMessageSet> for GatewayWorkerActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn meta(&self) -> String {
        format!("gateway-{}", self.id)
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<GatewayMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
        select: {
            Ok(_) = self.socket.readable() => {
                if let Err(err) = self.read_socket().await {
                    tracing::error!("failed to read socket: {err}");
                    break;
                }
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
    const BATCH_SIZE: usize = 64;
    pub fn new(id: usize, socket: Arc<net::UnifiedSocket>) -> Self {
        // Pre-allocate receive batch with MTU-sized buffers
        let recv_batch = Vec::with_capacity(Self::BATCH_SIZE);
        let storage = net::RecvBatchStorage::new(socket.gro_segments());

        Self {
            id,
            socket,
            recv_batch,
            storage,
            demuxer: Demuxer::new(),
        }
    }

    async fn read_socket(&mut self) -> io::Result<()> {
        // the loop after reading should always clear the buffer
        assert!(self.recv_batch.is_empty());

        let mut batch = net::RecvPacketBatch::new(&mut self.storage);
        self.socket
            .try_recv_batch(&mut batch, &mut self.recv_batch)?;

        for packet in self.recv_batch.drain(..) {
            self.demuxer.demux(packet).await;
        }
        self.recv_batch.clear();

        Ok(())
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
