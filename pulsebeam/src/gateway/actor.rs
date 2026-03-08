use crate::gateway::demux::Demuxer;
use pulsebeam_runtime::actor::{ActorKind, ActorStatus, RunnerConfig};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::{actor, net};
use std::io;
use tokio::task::JoinSet;

#[derive(Clone)]
pub enum GatewayControlMessage {
    AddParticipant(
        String,
        pulsebeam_runtime::sync::mpsc::Sender<net::RecvPacketBatch>,
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
        // Maximum logical packets to dispatch before yielding for fairness.
        // A yield every ~64 packets keeps latency low for other Tokio tasks.
        const COOP_BUDGET: usize = 64;

        // When the highest observed participant queue fill ratio exceeds this,
        // yield immediately regardless of how much budget remains. This gives
        // participant actors a chance to drain before we push more data in and
        // cause lag (packet loss inside the ring buffer).
        const HIGH_WATER: f64 = 0.5;

        // After a pressure-triggered yield, give participants up to this many
        // additional scheduler turns to drain before resuming socket reads.
        // Each extra yield is only taken while pressure remains above HIGH_WATER.
        const MAX_PRESSURE_YIELDS: usize = 3;

        let mut spent_budget: usize = 0;

        loop {
            self.recv_batches.clear();
            self.demuxer.reset_pressure();

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
                        if !self.demuxer.demux(&mut self.socket, batch) {
                            // In case there's a malicious actor, close immediately as there's no
                            // associated participant.
                            self.socket.close_peer(&src);
                        }

                        spent_budget += cost;
                    }

                    // Check downstream health after dispatching the whole batch.
                    // Yield outside the drain loop so we don't split a batch mid-way,
                    // and so we can make a calmer, informed decision.
                    let pressure = self.demuxer.pressure();
                    let should_yield = pressure > HIGH_WATER || spent_budget >= COOP_BUDGET;

                    if should_yield {
                        spent_budget = 0;
                        tokio::task::yield_now().await;

                        // If participants are still congested after the first yield,
                        // give them more scheduler turns before pulling more packets.
                        // We re-read live fill ratios (not the cached max_pressure) so
                        // we accurately reflect how much they've drained since we yielded.
                        for _ in 0..MAX_PRESSURE_YIELDS {
                            if self.demuxer.current_pressure() <= HIGH_WATER {
                                break;
                            }
                            tokio::task::yield_now().await;
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
