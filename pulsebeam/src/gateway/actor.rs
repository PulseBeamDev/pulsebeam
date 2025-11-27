use crate::{entity::ParticipantId, gateway::demux::Demuxer};
use futures::{StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, mailbox, net, rt};
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
    recv_batches: Vec<net::RecvPacketBatch>,
    storage: net::RecvBatchStorage,
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
    // Standard batch size for socket syscalls (e.g. RecvMmsg)
    const BATCH_SIZE: usize = 64;

    // Cooperative Multitasking Budget:
    // How many batches to process before forcibly yielding to the Tokio runtime.
    // 16 * 64 = 1024 packets.
    const BUDGET_BATCHES: usize = 1;

    // Threshold for "Emergency Yielding".
    // If we drop this many packets in a single loop, downstream is overwhelmed.
    // We yield immediately to let them catch up.
    const DROP_THRESHOLD: usize = 100;

    pub fn new(id: usize, socket: Arc<net::UnifiedSocket>) -> Self {
        let recv_batch = Vec::with_capacity(Self::BATCH_SIZE);
        let gro_segments = socket.gro_segments();
        let storage = net::RecvBatchStorage::new(gro_segments);

        Self {
            id,
            socket,
            recv_batches: recv_batch,
            storage,
            demuxer: Demuxer::new(),
        }
    }

    async fn read_socket(&mut self) -> io::Result<()> {
        let mut batches_processed = 0;
        let mut total_drops_in_loop = 0;

        loop {
            if batches_processed >= Self::BUDGET_BATCHES {
                rt::yield_now().await;
                batches_processed = 0;
                total_drops_in_loop = 0;
            }

            self.recv_batches.clear();

            let mut batch = net::RecvPacketBatcher::new(&mut self.storage);
            match self
                .socket
                .try_recv_batch(&mut batch, &mut self.recv_batches)
            {
                Ok(_) => {
                    // Data received, continue to process
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(e) => return Err(e),
            }

            for batch in self.recv_batches.drain(..) {
                let success = self.demuxer.demux(batch);
                if !success {
                    total_drops_in_loop += 1;
                }
            }

            if total_drops_in_loop > Self::DROP_THRESHOLD {
                rt::yield_now().await;
                batches_processed = 0;
                total_drops_in_loop = 0;
            }

            batches_processed += 1;
        }

        Ok(())
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
