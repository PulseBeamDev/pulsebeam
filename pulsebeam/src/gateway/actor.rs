use crate::entity::ParticipantId;
use crate::gateway::demux::Demuxer;
use crate::participant::Participant;
use pulsebeam_runtime::actor::{ActorKind, ActorStatus, RunnerConfig};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::{actor, net};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use tokio::task::JoinSet;

#[derive(Debug)]
pub enum GatewayControlMessage {
    AddParticipant(ParticipantId, Participant),
    RemoveParticipant(ParticipantId),
    ParticipantControl(ParticipantId, crate::participant::ParticipantControlMessage),
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
        if self.workers.is_empty() {
            return;
        }

        let target_participant_id = match &msg {
            GatewayControlMessage::AddParticipant(pid, _) => pid,
            GatewayControlMessage::RemoveParticipant(pid) => pid,
            GatewayControlMessage::ParticipantControl(pid, _) => pid,
        };

        let mut hasher = ahash::AHasher::default();
        target_participant_id.hash(&mut hasher);
        let worker_idx = (hasher.finish() as usize) % self.workers.len();

        let _ = self.workers[worker_idx]
            .tx
            .send(msg)
            .await
            .expect("worker is alive until the controller has drained");
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
    participants: HashMap<ParticipantId, Participant>,
    participant_ufrags: HashMap<ParticipantId, String>,
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
            GatewayControlMessage::AddParticipant(participant_id, mut participant) => {
                let ufrag = participant
                    .core
                    .rtc
                    .direct_api()
                    .local_ice_credentials()
                    .ufrag
                    .clone();

                self.demuxer
                    .register_ice_ufrag(ufrag.as_bytes(), participant_id);
                self.participant_ufrags.insert(participant_id, ufrag);
                self.participants.insert(participant_id, participant);
            }
            GatewayControlMessage::RemoveParticipant(participant_id) => {
                if let Some(ufrag) = self.participant_ufrags.remove(&participant_id) {
                    self.demuxer.unregister(&mut self.socket, ufrag.as_bytes());
                }
                self.participants.remove(&participant_id);
            }
            GatewayControlMessage::ParticipantControl(participant_id, msg) => {
                if let Some(p) = self.participants.get_mut(&participant_id) {
                    p.on_control_message(msg);
                }
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
            demuxer: Demuxer::new(),
            participants: HashMap::new(),
            participant_ufrags: HashMap::new(),
            recv_batches: recv_batch,
        }
    }

    async fn read_socket(&mut self) -> io::Result<()> {
        // Maximum logical packets to dispatch before yielding for fairness.
        // A yield every ~64 packets keeps latency low for other Tokio tasks.
        const COOP_BUDGET: usize = 64;


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
                        if let Some(participant_id) = self.demuxer.demux(&mut self.socket, &batch) {
                            if let Some(participant) = self.participants.get_mut(&participant_id) {
                                participant.on_udp_batch(batch);
                                participant.poll();
                            } else {
                                self.socket.close_peer(&src);
                            }
                        } else {
                            // In case there's a malicious actor, close immediately as there's no
                            // associated participant.
                            self.socket.close_peer(&src);
                        }

                        spent_budget += cost;
                    }

                    // Check downstream health after dispatching the whole batch.
                    // Yield outside the drain loop so we don't split a batch mid-way.
                    let should_yield = spent_budget >= COOP_BUDGET;

                    if should_yield {
                        spent_budget = 0;
                        tokio::task::yield_now().await;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
