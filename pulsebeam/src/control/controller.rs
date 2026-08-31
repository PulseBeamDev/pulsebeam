use std::{collections::VecDeque, io, time::Duration};

use crate::control::steering::Steering;
use crate::track::{SelectionPolicy, TrackSelector};
use crate::{
    control::{
        core::{ControllerCore, RoomPlacement},
        lifecycle::{TrackLifecycle, TrackLifecycleOperation, TrackLifecycleOutcome},
        negotiator::{DirectNegotiation, Negotiator, NegotiatorError},
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    route::TransportHandle,
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_runtime::mailbox;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

#[derive(Debug, derive_more::From)]
pub enum ControllerCommand {
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

#[derive(Debug)]
pub struct CreateParticipant {
    pub state: ParticipantState,
    pub offer: String,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: String,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: String,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: String,
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] NegotiatorError),
    #[error("server is busy, please try again later.")]
    ServiceUnavailable,
    #[error("participant connection generation is stale")]
    StaleConnection,
    #[error("IO error: {0}")]
    IOError(#[from] io::Error),
    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHARD_EVENT_BUDGET: usize = 1_024;
const TCP_HANDOFF_BUDGET: usize = 64;
const API_COMMAND_BUDGET: usize = 16;
const SHARD_COMMAND_EGRESS_BUDGET: usize = 64;
const SHARD_UPDATE_EGRESS_BUDGET: usize = 64;

struct PendingMaterialization {
    shard: ShardId,
    command: Option<ShardCommand>,
    ack: Option<oneshot::Receiver<bool>>,
    participant: ParticipantId,
    room_id: RoomId,
    answer: String,
}

enum RequiredAction {
    Materialize {
        pending: PendingMaterialization,
        reply: MaterializationReply,
    },
}

enum MaterializationReply {
    Create(oneshot::Sender<Result<CreateParticipantReply, ControllerError>>),
    Patch(oneshot::Sender<Result<PatchParticipantReply, ControllerError>>),
}

pub struct ControllerActor {
    router: crate::control::router::ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    tcp_listener: Option<pulsebeam_core::net::TcpListener>,
    cluster_id: u16,
    node_id: u16,
    updates: Vec<crate::shard_update::ShardUpdateWriter>,
    update_touched: Vec<bool>,
    pending_updates: VecDeque<ShardId>,
    update_queued: Vec<bool>,
    egress_ready: bool,
    lifecycle: TrackLifecycle,
    command_backlog: VecDeque<(ShardId, ShardCommand)>,
    steering: Option<Box<dyn Steering>>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Box<[pulsebeam_rtc::IceCandidate]>,
        tcp_listener: pulsebeam_core::net::TcpListener,
        room_shard_slot: usize,
        placement: RoomPlacement,
        updates: Vec<crate::shard_update::ShardUpdateWriter>,
    ) -> Self {
        let shard_count = shard_contexts.len();
        debug_assert_eq!(updates.len(), shard_count);
        Self {
            router: crate::control::router::ShardRouter::new(shard_contexts),
            core: ControllerCore::with_shards(shard_count, room_shard_slot, placement),
            negotiator: Negotiator::new(candidates),
            tcp_listener: Some(tcp_listener),
            cluster_id: 0,
            node_id: 0,
            update_touched: vec![false; shard_count],
            pending_updates: VecDeque::new(),
            update_queued: vec![false; shard_count],
            egress_ready: false,
            updates,
            lifecycle: TrackLifecycle::new(shard_count),
            command_backlog: VecDeque::new(),
            steering: None,
        }
    }

    pub(crate) fn set_steering(&mut self, steering: Option<Box<dyn Steering>>) {
        self.steering = steering;
    }

    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        let Some(steering) = self.steering.as_mut() else {
            return;
        };
        steering.pin_flow_to_owner(source, destination, shard);
    }

    pub(crate) async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEventMessage>,
        shutdown: CancellationToken,
    ) {
        let Some(listener) = self.tcp_listener.take() else {
            pulsebeam_runtime::fatal!("ControllerActor::run called twice")
        };
        let acceptor = TcpAcceptorHandle::spawn(
            listener,
            crate::control::tcp_acceptor::TcpAcceptorConfig {
                cluster_id: self.cluster_id,
                node_id: self.node_id,
                shard_count: self.router.shard_count(),
            },
            shutdown.child_token(),
        );
        let mut pending_rx = acceptor.event_rx;
        let mut poll_interval = tokio::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        describe_controller_metrics();

        loop {
            let maintenance_due = if self.has_ready_egress() {
                false
            } else {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    Some(_) = shard_event_rx.readable() => false,
                    _ = poll_interval.tick() => true,
                    Some(_) = pending_rx.readable() => false,
                    Some(_) = command_rx.readable() => false,
                }
            };

            let action = self.tick(
                &mut command_rx,
                &mut shard_event_rx,
                &mut pending_rx,
                maintenance_due,
            );
            if let Some(action) = action {
                self.run_required_action(action).await;
            }
        }
    }

    fn tick(
        &mut self,
        command_rx: &mut mailbox::Receiver<ControllerCommand>,
        shard_event_rx: &mut mailbox::Receiver<ShardEventMessage>,
        pending_rx: &mut mailbox::Receiver<crate::control::tcp_acceptor::TcpAcceptorEvent>,
        maintenance_due: bool,
    ) -> Option<RequiredAction> {
        let started = tokio::time::Instant::now();
        let mut shard_events: usize = 0;
        for _ in 0..SHARD_EVENT_BUDGET {
            let Ok(event) = shard_event_rx.try_recv() else {
                break;
            };
            shard_events = shard_events.saturating_add(1);
            self.handle_shard_event(event);
        }
        self.record_budget_hit("shard_events", shard_events, SHARD_EVENT_BUDGET);

        if maintenance_due {
            self.router.poll_loads();
        }

        let mut tcp_handoffs: usize = 0;
        for _ in 0..TCP_HANDOFF_BUDGET {
            let Ok(event) = pending_rx.try_recv() else {
                break;
            };
            tcp_handoffs = tcp_handoffs.saturating_add(1);
            if let Some(connection) = event.result {
                self.route_tcp_connection(connection);
            }
        }
        self.record_budget_hit("tcp_handoffs", tcp_handoffs, TCP_HANDOFF_BUDGET);

        let mut action = None;
        let mut commands: usize = 0;
        for _ in 0..API_COMMAND_BUDGET {
            let Ok(command) = command_rx.try_recv() else {
                break;
            };
            commands = commands.saturating_add(1);
            if let Some(required) = self.process_command(command) {
                action = Some(required);
                break;
            }
        }
        self.record_budget_hit("api_commands", commands, API_COMMAND_BUDGET);

        let commands_ready = self.flush_command_backlog();
        let updates_ready = self.flush_update_backlogs();
        self.egress_ready = commands_ready || updates_ready;
        metrics::histogram!("control_tick_us").record(started.elapsed().as_micros() as f64);
        action
    }

    fn process_command(&mut self, command: ControllerCommand) -> Option<RequiredAction> {
        match command {
            ControllerCommand::CreateParticipant(message, reply) => {
                match self.begin_create_participant(message.state, message.offer) {
                    Ok(pending) => Some(RequiredAction::Materialize {
                        pending,
                        reply: MaterializationReply::Create(reply),
                    }),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        None
                    }
                }
            }
            ControllerCommand::DeleteParticipant(message) => {
                self.remove_participant(message.participant_id);
                None
            }
            ControllerCommand::PatchParticipant(message, reply) => {
                match self.begin_patch_participant(message.state, message.offer) {
                    Ok(pending) => Some(RequiredAction::Materialize {
                        pending,
                        reply: MaterializationReply::Patch(reply),
                    }),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        None
                    }
                }
            }
        }
    }

    async fn run_required_action(&mut self, action: RequiredAction) {
        match action {
            RequiredAction::Materialize { pending, reply } => {
                let started = tokio::time::Instant::now();
                let mut pending = pending;
                let Some(ack) = pending.ack.take() else {
                    pulsebeam_runtime::fatal!(
                        "materialization must retain its acknowledgement receiver"
                    );
                };
                let Some(command) = pending.command.take() else {
                    pulsebeam_runtime::fatal!("materialization must retain its shard command");
                };
                let materialized = self.router.send(pending.shard, command).await.is_ok()
                    && ack.await.unwrap_or(false);
                metrics::histogram!("control_materialization_wait_us")
                    .record(started.elapsed().as_micros() as f64);
                let result = if materialized {
                    Ok(self.complete_materialization(pending))
                } else {
                    self.abort_materialization(pending.participant);
                    Err(ControllerError::ServiceUnavailable)
                };
                let commands_ready = self.flush_command_backlog();
                let updates_ready = self.flush_update_backlogs();
                self.egress_ready = commands_ready || updates_ready;
                match reply {
                    MaterializationReply::Create(reply) => {
                        let _ = reply.send(result.map(|answer| CreateParticipantReply { answer }));
                    }
                    MaterializationReply::Patch(reply) => {
                        let _ = reply.send(result.map(|answer| PatchParticipantReply { answer }));
                    }
                }
            }
        }
    }

    fn handle_shard_event(&mut self, (_shard, event): ShardEventMessage) {
        match event {
            ShardEvent::TrackPublished { track } => {
                if let Some(outcome) =
                    self.lifecycle
                        .publish(track, &self.core.registry, tokio::time::Instant::now())
                {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                if let Some(outcome) = self.lifecycle.unpublish(
                    origin,
                    track_id,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                ) {
                    self.apply_track_lifecycle(outcome);
                    self.publish_staged();
                }
            }
            ShardEvent::TrackSubscribed {
                subscriber, track, ..
            } => {
                let outcome = self.lifecycle.activate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackUnsubscribed {
                subscriber, track, ..
            } => {
                let outcome = self.lifecycle.deactivate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackSubscriptionAdded {
                room_id,
                subscriber,
                selector,
                selection,
            } => {
                let outcomes = self.lifecycle.subscribe(
                    room_id,
                    subscriber,
                    selector,
                    selection,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TrackSubscriptionRemoved {
                room_id,
                subscriber,
                selector,
            } => {
                let outcomes = self.lifecycle.unsubscribe(
                    room_id,
                    subscriber,
                    selector,
                    &self.core.registry,
                    tokio::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TransportAuthenticated {
                source,
                destination,
                source_shard,
                handle,
                shard: owner_shard,
                ..
            } => {
                let Some(owner) = u16::try_from(owner_shard.index()).ok() else {
                    debug_assert!(false, "shard index must fit in u16");
                    return;
                };
                self.pin_flow_to_owner(source, destination, owner);
                self.command_backlog.push_back((
                    source_shard,
                    ShardCommand::AuthenticateTransport { source, handle },
                ));
                self.emit_placeholder(owner_shard);
            }
            ShardEvent::ParticipantClosed { participant, .. } => {
                self.remove_participant_with_mode(participant, true);
            }
        }
    }
}

impl ControllerActor {
    fn apply_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        for operation in outcome.operations {
            match operation {
                TrackLifecycleOperation::Update { shard, op } => {
                    self.stage_update_at(shard, outcome.generation, op);
                }
                TrackLifecycleOperation::Plans { shard, plans } => {
                    self.stage_plans_at(shard, outcome.generation, plans);
                }
                TrackLifecycleOperation::ParticipantEffect {
                    shard,
                    participant,
                    effect,
                } => self.stage_participant_at(shard, outcome.generation, participant, effect),
            }
        }
    }

    fn publish_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        self.apply_track_lifecycle(outcome);
        self.publish_staged();
    }

    fn stage_update_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        op: crate::shard_update::ShardUpdateOp,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "an update must target a live shard");
            return;
        };
        update.stage(generation, op);
        self.mark_update_touched(shard);
    }

    fn stage_plans_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        plans: Vec<crate::shard_update::TrackPlanUpdate>,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "plans must target a live shard");
            return;
        };
        update.stage_plans(generation, plans);
        self.mark_update_touched(shard);
    }

    fn stage_participant_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        participant: crate::keys::ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "participant effects must target a live shard");
            return;
        };
        update.stage_participant_effect(generation, participant, effect);
        self.mark_update_touched(shard);
    }

    fn stage_participant_change_at(
        &mut self,
        room_id: crate::entity::RoomId,
        generation: u64,
        added: Option<ParticipantId>,
        removed: Option<ParticipantId>,
    ) {
        let participants: Vec<_> = self
            .core
            .registry
            .participant_ids_in_room(&room_id)
            .into_iter()
            .filter(|participant| Some(*participant) != added && Some(*participant) != removed)
            .collect();
        for participant in participants {
            let Some(meta) = self.core.registry.get_participant(&participant) else {
                continue;
            };
            let Some(key) = meta.binding else {
                continue;
            };
            self.stage_participant_at(
                meta.shard_id,
                generation,
                key,
                crate::participant::ParticipantEffect::ParticipantsChanged {
                    added: added.into_iter().collect(),
                    removed: removed.into_iter().collect(),
                },
            );
        }
    }

    fn publish_staged(&mut self) {
        for index in 0..self.updates.len() {
            let Some(touched) = self.update_touched.get_mut(index) else {
                debug_assert!(false, "every update writer must have a touched flag");
                continue;
            };
            if !std::mem::take(touched) {
                continue;
            }
            let Some(update) = self.updates.get_mut(index) else {
                debug_assert!(false, "every touched update writer must exist");
                continue;
            };
            let _ = update.publish();
            if update.has_backlog() {
                self.schedule_update(ShardId::new(index));
            }
        }
    }

    fn mark_update_touched(&mut self, shard: ShardId) {
        let Some(touched) = self.update_touched.get_mut(shard.index()) else {
            debug_assert!(false, "an update must target a configured shard");
            return;
        };
        *touched = true;
    }

    fn schedule_update(&mut self, shard: ShardId) {
        let Some(queued) = self.update_queued.get_mut(shard.index()) else {
            debug_assert!(false, "a scheduled update must target a configured shard");
            return;
        };
        if !std::mem::replace(queued, true) {
            self.pending_updates.push_back(shard);
        }
    }

    fn emit_placeholder(&mut self, shard: ShardId) {
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "shard update targeted an unknown shard");
            return;
        };
        update.stage(generation, crate::shard_update::ShardUpdateOp::Placeholder);
        self.mark_update_touched(shard);
        self.publish_staged();
    }

    fn flush_command_backlog(&mut self) -> bool {
        let mut sent: usize = 0;
        for _ in 0..SHARD_COMMAND_EGRESS_BUDGET {
            let Some((shard, command)) = self.command_backlog.pop_front() else {
                break;
            };
            sent = sent.saturating_add(1);
            match self.router.try_send(shard, command) {
                Ok(()) => {}
                Err(error) => match *error {
                    mailbox::TrySendError::Full(command) => {
                        self.command_backlog.push_front((shard, command));
                        break;
                    }
                    mailbox::TrySendError::Closed(_) => {
                        tracing::warn!(%shard, "shard command mailbox closed");
                    }
                },
            }
        }
        self.record_budget_hit("shard_commands", sent, SHARD_COMMAND_EGRESS_BUDGET);
        sent > 0
    }

    fn flush_update_backlogs(&mut self) -> bool {
        let mut attempts: usize = 0;
        let mut sent = false;
        let mut blocked = Vec::new();
        for _ in 0..SHARD_UPDATE_EGRESS_BUDGET {
            let Some(shard) = self.pending_updates.pop_front() else {
                break;
            };
            attempts = attempts.saturating_add(1);
            let Some(queued) = self.update_queued.get_mut(shard.index()) else {
                debug_assert!(false, "scheduled update must have a queue flag");
                continue;
            };
            debug_assert!(*queued, "scheduled update must set its queue flag");
            *queued = false;
            let Some(update) = self.updates.get_mut(shard.index()) else {
                debug_assert!(false, "scheduled update must target a configured shard");
                continue;
            };
            let flushed = update.flush_one();
            sent |= flushed;
            if update.has_backlog() {
                if flushed {
                    self.schedule_update(shard);
                } else {
                    blocked.push(shard);
                }
            }
        }
        for shard in blocked {
            self.schedule_update(shard);
        }
        self.record_budget_hit("shard_updates", attempts, SHARD_UPDATE_EGRESS_BUDGET);
        sent
    }

    fn has_ready_egress(&self) -> bool {
        self.egress_ready && (!self.command_backlog.is_empty() || !self.pending_updates.is_empty())
    }

    fn record_budget_hit(&self, phase: &'static str, used: usize, budget: usize) {
        debug_assert!(budget > 0);
        if used == budget {
            metrics::counter!("control_tick_budget_hit", "phase" => phase).increment(1);
        }
    }

    fn route_tcp_connection(&mut self, connection: PendingTcpConn) {
        debug_assert!(connection.handle.shard().index() < self.router.shard_count());
        self.command_backlog.push_back((
            connection.handle.shard(),
            ShardCommand::AdoptTcpConnection {
                stream: connection.stream,
                peer_addr: connection.peer_addr,
            },
        ));
    }

    fn publish_transport(
        &mut self,
        shard: ShardId,
        handle: TransportHandle,
        key: crate::keys::ParticipantKey,
    ) -> bool {
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            return false;
        };
        update.stage(
            generation,
            crate::shard_update::ShardUpdateOp::InsertParticipant,
        );
        update.stage(
            generation,
            crate::shard_update::ShardUpdateOp::InstallTransport {
                binding: crate::shard_update::TransportBinding {
                    handle,
                    participant: key,
                },
            },
        );
        self.mark_update_touched(shard);
        self.publish_staged();
        true
    }

    fn begin_create_participant(
        &mut self,
        state: ParticipantState,
        offer: String,
    ) -> Result<PendingMaterialization, ControllerError> {
        let participant_id = state.participant_id;
        let (slot, placement) = self.core.room_slot(&state.room_id);
        let shard = match placement {
            RoomPlacement::Hashed => self
                .router
                .stable_route(&state.room_id)
                .ok_or(ControllerError::ServiceUnavailable)?,
            RoomPlacement::RoundRobin => {
                let shard_count = self.router.shard_count();
                debug_assert_ne!(shard_count, 0);
                ShardId::new(
                    slot.checked_rem(shard_count)
                        .ok_or(ControllerError::ServiceUnavailable)?,
                )
            }
        };
        let now = tokio::time::Instant::now();
        let handle = self.core.reserve_transport(shard, now);
        let key = self
            .core
            .mint_participant(shard, state.participant_id)
            .ok_or(ControllerError::ServiceUnavailable)?;
        let creds = IceUfrag::new(self.cluster_id, self.node_id, handle.route, handle.epoch)
            .into_ice_creds();
        let direct_id = (u64::from(handle.route.get()) << 16) | u64::from(handle.epoch);
        let DirectNegotiation {
            answer,
            peer,
            media,
        } = match self
            .negotiator
            .create_answer(&offer, direct_id, creds.0, creds.1)
        {
            Ok(value) => value,
            Err(error) => {
                self.core.remove_participant_key(shard, key);
                self.core.release_transport(handle, now);
                return Err(error.into());
            }
        };
        if !self.publish_transport(shard, handle, key) {
            self.core.remove_participant_key(shard, key);
            self.core.release_transport(handle, now);
            return Err(ControllerError::ServiceUnavailable);
        }
        let config = self
            .core
            .create_participant(peer, media, state, shard, handle, key);
        let room_id = config.room_id;
        let (ack_tx, ack_rx) = oneshot::channel();
        Ok(PendingMaterialization {
            shard,
            command: Some(ShardCommand::MaterializeParticipant {
                key,
                transport: handle,
                config: Box::new(config),
                ack: ack_tx,
            }),
            ack: Some(ack_rx),
            participant: participant_id,
            room_id,
            answer,
        })
    }

    fn complete_materialization(&mut self, pending: PendingMaterialization) -> String {
        let participant_id = pending.participant;
        let room_id = pending.room_id;
        let generation = self.lifecycle.next_generation();
        self.stage_participant_change_at(room_id, generation, Some(participant_id), None);
        if let Some(meta) = self.core.registry.get_participant(&participant_id)
            && let Some(key) = meta.binding
        {
            let participants = self
                .core
                .registry
                .participant_ids_in_room(&room_id)
                .into_iter()
                .filter(|participant| *participant != participant_id)
                .collect();
            self.stage_participant_at(
                meta.shard_id,
                generation,
                key,
                crate::participant::ParticipantEffect::ParticipantsChanged {
                    added: participants,
                    removed: Vec::new(),
                },
            );
        }
        self.publish_staged();
        let outcomes = self.lifecycle.subscribe_defaults(
            room_id,
            participant_id,
            [
                (TrackSelector::audio(), SelectionPolicy::All),
                (TrackSelector::video(), SelectionPolicy::Allocated),
            ],
            &self.core.registry,
            tokio::time::Instant::now(),
        );
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        pending.answer
    }

    fn abort_materialization(&mut self, participant: ParticipantId) {
        self.remove_participant(participant);
    }

    fn begin_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: String,
    ) -> Result<PendingMaterialization, ControllerError> {
        let current = self
            .core
            .registry
            .get_participant(&state.participant_id)
            .and_then(|meta| meta.connection_id);
        if current != state.old_connection_id {
            return Err(ControllerError::StaleConnection);
        }
        self.remove_participant_with_mode(state.participant_id, true);
        self.begin_create_participant(state, offer)
    }

    fn remove_participant(&mut self, participant: ParticipantId) {
        self.remove_participant_with_mode(participant, false);
    }

    fn remove_participant_with_mode(&mut self, participant: ParticipantId, retain_identity: bool) {
        let Some(meta) = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| crate::control::core::ParticipantMeta {
                shard: meta.shard_id,
                binding: meta.binding,
                transport: meta.transport,
            })
        else {
            return;
        };
        let mut outcomes = self.lifecycle.remove_participant(
            participant,
            &self.core.registry,
            tokio::time::Instant::now(),
        );
        let generation = outcomes
            .first()
            .map(|outcome| outcome.generation)
            .unwrap_or_else(|| self.lifecycle.next_generation());
        if let Some(outcome) = outcomes.first()
            && outcome.generation == generation
        {
            let outcome = outcomes.remove(0);
            self.apply_track_lifecycle(outcome);
        }
        let room_id = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| meta.room_id);
        if let Some(room_id) = room_id {
            self.stage_participant_change_at(room_id, generation, None, Some(participant));
        }
        if retain_identity {
            let _ = self.core.disconnect_participant(&participant);
        } else {
            let _ = self.core.delete_participant(&participant);
        }
        self.publish_staged();
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        let Some(handle) = meta.transport else { return };
        let generation = self.lifecycle.next_generation();
        if let Some(update) = self.updates.get_mut(meta.shard.index()) {
            update.stage(
                generation,
                crate::shard_update::ShardUpdateOp::RetireTransport { handle },
            );
            if let Some(key) = meta.binding {
                update.stage(
                    generation,
                    crate::shard_update::ShardUpdateOp::RemoveParticipant { key },
                );
            }
            self.mark_update_touched(meta.shard);
            self.publish_staged();
        }
        if let Some(key) = meta.binding {
            self.core.remove_participant_key(meta.shard, key);
        }
        self.core
            .release_transport(handle, tokio::time::Instant::now());
    }
}

fn describe_controller_metrics() {
    metrics::describe_histogram!(
        "control_tick_us",
        metrics::Unit::Microseconds,
        "how long one synchronous controller tick took"
    );
    metrics::describe_counter!(
        "control_tick_budget_hit",
        "controller ticks that exhausted a bounded work phase"
    );
    metrics::describe_histogram!(
        "control_materialization_wait_us",
        metrics::Unit::Microseconds,
        "how long required shard materialization admission and acknowledgement took"
    );
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;
