use crate::entity::ParticipantId;
use crate::gateway::demux::Demuxer;
use crate::participant::Participant;
use pulsebeam_runtime::actor::{ActorKind, ActorStatus, RunnerConfig};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::{actor, net};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{Hash, Hasher};
use std::io;
use std::pin::Pin;
use std::time::Duration;
use tokio::task::JoinSet;
use tachyonix::{Receiver as TachyonixReceiver, Sender as TachyonixSender};

#[derive(Debug)]
pub enum GatewayControlMessage {
    AddParticipant(ParticipantId, Participant),
    RemoveParticipant(ParticipantId),
    ParticipantControl(ParticipantId, crate::participant::ParticipantControlMessage),
    DownstreamRtp(ParticipantId, str0m::media::Mid, crate::rtp::RtpPacket),
}

pub struct GatewayMessageSet;

impl actor::MessageSet for GatewayMessageSet {
    type Msg = GatewayControlMessage;
    type Meta = String;
    type ObservableState = ();
}

pub enum MeshEvent {
    RemotePacket(ParticipantId, net::RecvPacketBatch),
    DownstreamRtp(ParticipantId, str0m::media::Mid, crate::rtp::RtpPacket),
}

pub struct GatewayActor {
    sockets: Vec<net::UnifiedSocketReader>,
    workers: Vec<GatewayWorkerHandle>,
    mesh_senders: Vec<TachyonixSender<MeshEvent>>,

    worker_tasks: JoinSet<(String, ActorStatus)>,
}

pub struct GatewayWorkerActor {
    id: usize,
    socket: net::UnifiedSocketReader,
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, Participant>,
    participant_ufrags: HashMap<ParticipantId, String>,
    recv_batches: Vec<net::RecvPacketBatch>,
    mesh_rx: TachyonixReceiver<MeshEvent>,
    deadline_heap: BinaryHeap<Reverse<(tokio::time::Instant, ParticipantId)>>,
    arq_deadlines: HashMap<ParticipantId, tokio::time::Instant>,
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
        let mut mesh_receivers = Vec::with_capacity(self.sockets.len());

        for _ in 0..self.sockets.len() {
            let (tx, rx) = tachyonix::channel(1024);
            self.mesh_senders.push(tx);
            mesh_receivers.push(rx);
        }

        for ((id, socket), mesh_rx) in self
            .sockets
            .drain(..)
            .enumerate()
            .zip(mesh_receivers.into_iter())
        {
            let (worker_handle, worker_task) =
                actor::prepare(GatewayWorkerActor::new(id, socket, mesh_rx), RunnerConfig::default());
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
        let mesh_senders = Vec::with_capacity(sockets.len());
        Self {
            sockets,
            workers,
            mesh_senders,
            worker_tasks: JoinSet::new(),
        }
    }

    pub async fn send_mesh_packet(&self, participant_id: ParticipantId, batch: net::RecvPacketBatch) {
        if self.mesh_senders.is_empty() {
            return;
        }

        let mut hasher = ahash::AHasher::default();
        participant_id.hash(&mut hasher);
        let worker_idx = (hasher.finish() as usize) % self.mesh_senders.len();

        let _ = self.mesh_senders[worker_idx]
            .send(MeshEvent::RemotePacket(participant_id, batch))
            .await;
    }
}

pub type GatewayHandle = actor::ActorHandle<GatewayMessageSet>;

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
        let mut sleep = Box::pin(tokio::time::sleep(Duration::from_secs(3600)));
        sleep.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));

        loop {
            if let Some(next_deadline) = self.next_deadline() {
                sleep.as_mut().reset(next_deadline);
            } else {
                sleep.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
            }

            tokio::select! {
                biased;
                sys_msg = ctx.sys_rx.recv() => {
                    match sys_msg {
                        Some(actor::SystemMsg::GetState(responder)) => {
                            let _ = responder.send(self.get_observable_state());
                        }
                        Some(actor::SystemMsg::Terminate) | None => break,
                    }
                }
                msg = ctx.rx.recv() => {
                    match msg {
                        Some(msg) => self.on_msg(ctx, msg).await,
                        None => break,
                    }
                }
                _ = self.socket.readable() => {
                    let _ = self.read_socket().await;
                }
                    mesh_msg = self.mesh_rx.recv() => {
                    match mesh_msg {
                        Ok(MeshEvent::RemotePacket(participant_id, batch)) => {
                            if let Some(participant) = self.participants.get_mut(&participant_id) {
                                participant.on_udp_batch(batch);
                                self.poll_participant(participant_id);
                            }
                        }
                        Ok(MeshEvent::DownstreamRtp(participant_id, mid, pkt)) => {
                            if let Some(participant) = self.participants.get_mut(&participant_id) {
                                participant.on_downstream_rtp(mid, pkt);
                            }
                        }
                        Err(_) => break,
                    }
                }
                _ = &mut sleep => {
                    self.handle_timer_tick();
                }
            }
        }

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
                self.poll_participant(participant_id);
            }
            GatewayControlMessage::RemoveParticipant(participant_id) => {
                if let Some(ufrag) = self.participant_ufrags.remove(&participant_id) {
                    self.demuxer.unregister(&mut self.socket, ufrag.as_bytes());
                }
                self.participants.remove(&participant_id);
                self.arq_deadlines.remove(&participant_id);
            }
            GatewayControlMessage::ParticipantControl(participant_id, msg) => {
                if let Some(p) = self.participants.get_mut(&participant_id) {
                    p.on_control_message(msg);
                    self.poll_participant(participant_id);
                }
            }
            GatewayControlMessage::DownstreamRtp(participant_id, mid, pkt) => {
                if let Some(p) = self.participants.get_mut(&participant_id) {
                    p.on_downstream_rtp(mid, pkt);
                }
            }
        };
    }
}

impl GatewayWorkerActor {
    pub fn new(id: usize, socket: net::UnifiedSocketReader, mesh_rx: TachyonixReceiver<MeshEvent>) -> Self {
        let recv_batch = Vec::with_capacity(net::BATCH_SIZE);

        Self {
            id,
            socket,
            demuxer: Demuxer::new(),
            participants: HashMap::new(),
            participant_ufrags: HashMap::new(),
            recv_batches: recv_batch,
            mesh_rx,
            deadline_heap: BinaryHeap::new(),
            arq_deadlines: HashMap::new(),
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
                    let mut deadline_updates = Vec::new();
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
                                if let Some(deadline) = participant.poll() {
                                    deadline_updates.push((participant_id, deadline));
                                }
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

                    // Apply collected deadline updates for polled participants.
                    for (id, deadline) in deadline_updates.drain(..) {
                        self.schedule_deadline(id, deadline);
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

    fn poll_participant(&mut self, participant_id: ParticipantId) {
        if let Some(participant) = self.participants.get_mut(&participant_id) {
            if let Some(deadline) = participant.poll() {
                self.schedule_deadline(participant_id, deadline);
            }
        }
    }

    fn schedule_deadline(&mut self, participant_id: ParticipantId, deadline: tokio::time::Instant) {
        let existing = self.arq_deadlines.get(&participant_id);
        if existing.map_or(true, |existing| deadline < *existing) {
            self.arq_deadlines.insert(participant_id, deadline);
            self.deadline_heap.push(Reverse((deadline, participant_id)));
        }
    }

    fn next_deadline(&mut self) -> Option<tokio::time::Instant> {
        while let Some(Reverse((deadline, pid))) = self.deadline_heap.peek() {
            if let Some(&current) = self.arq_deadlines.get(pid) {
                if current != *deadline {
                    self.deadline_heap.pop();
                    continue;
                }
            } else {
                self.deadline_heap.pop();
                continue;
            }
            return Some(*deadline);
        }
        None
    }

    fn handle_timer_tick(&mut self) {
        let now = tokio::time::Instant::now();
        while let Some(Reverse((deadline, participant_id))) = self.deadline_heap.peek().cloned() {
            if deadline > now {
                break;
            }

            self.deadline_heap.pop();
            if let Some(&current) = self.arq_deadlines.get(&participant_id) {
                if current == deadline {
                    self.arq_deadlines.remove(&participant_id);
                    self.poll_participant(participant_id);
                }
            }
        }
    }

    fn handle_mesh_event(&mut self, event: MeshEvent) {
        match event {
            MeshEvent::RemotePacket(participant_id, batch) => {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_udp_batch(batch);
                    self.poll_participant(participant_id);
                }
            }
        }
    }
}

pub type GatewayWorkerHandle = actor::ActorHandle<GatewayMessageSet>;
