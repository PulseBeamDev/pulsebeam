use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::control::steering::Steering;
use crate::track::{SelectionPolicy, TrackSelector};
use crate::{
    control::{
        core::{ControllerCore, RoomPlacement},
        lifecycle::{TrackLifecycle, TrackLifecycleOperation, TrackLifecycleOutcome},
        negotiator::{Negotiator, NegotiatorError},
        tcp_acceptor::{PendingTcpConn, TcpAcceptorHandle},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_core::auth::{AuthorizationExpiry, TokenError};
use pulsebeam_runtime::mailbox;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
    pub authorization: Option<AuthorizationLease>,
    pub profile: ConnectionProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionProfile {
    Native,
    Whip,
    Whep,
}

const AUTHORIZATION_TIMER_HORIZON: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizationLease {
    expiry: AuthorizationExpiry,
    deadline: tokio::time::Instant,
}

impl AuthorizationLease {
    pub fn from_expiry(
        expiry: AuthorizationExpiry,
        wall_now: SystemTime,
        runtime_now: tokio::time::Instant,
    ) -> Result<Self, TokenError> {
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        if expiry.is_expired_at(unix_now.as_secs()) {
            return Err(TokenError::Expired);
        }
        let remaining =
            Duration::from_secs(expiry.unix_seconds().saturating_sub(unix_now.as_secs()))
                .saturating_sub(Duration::from_nanos(u64::from(unix_now.subsec_nanos())));
        let delay = remaining.min(AUTHORIZATION_TIMER_HORIZON);
        Ok(Self {
            expiry,
            deadline: runtime_now.checked_add(delay).unwrap_or(runtime_now),
        })
    }

    fn is_expired_at(self, wall_now: SystemTime) -> bool {
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        self.expiry.is_expired_at(unix_now.as_secs())
    }

    fn refresh_at(
        self,
        wall_now: SystemTime,
        runtime_now: tokio::time::Instant,
    ) -> Result<Self, TokenError> {
        Self::from_expiry(self.expiry, wall_now, runtime_now)
    }

    pub(crate) fn deadline(self) -> tokio::time::Instant {
        self.deadline
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AuthorizationExpiryWork {
    deadline: tokio::time::Instant,
    participant_id: ParticipantId,
    connection_id: ConnectionId,
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
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub profile: ConnectionProfile,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] NegotiatorError),
    #[error("server is busy, please try again later.")]
    ServiceUnavailable,
    #[error("participant connection was superseded")]
    Superseded,
    #[error("participant authorization expired")]
    AuthorizationExpired,
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
    connection_id: ConnectionId,
    authorization: Option<AuthorizationLease>,
    profile: ConnectionProfile,
    transport: crate::route::NodeTransportAddress,
    room_id: RoomId,
    answer: SdpAnswer,
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
    authorization_expiries: BTreeSet<AuthorizationExpiryWork>,
    steering: Option<Box<dyn Steering>>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<Candidate>,
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
            authorization_expiries: BTreeSet::new(),
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
            let authorization_deadline = self
                .authorization_expiries
                .first()
                .map(|work| work.deadline);
            let maintenance_due = if self.has_ready_egress() {
                false
            } else {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep_until(
                        authorization_deadline.unwrap_or_else(tokio::time::Instant::now)
                    ), if authorization_deadline.is_some() => false,
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
        self.expire_authorizations(started, SystemTime::now());
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
                if self
                    .core
                    .registry
                    .get_participant(&message.participant_id)
                    .is_some_and(|meta| {
                        meta.room_id == message.room_id && meta.profile == message.profile
                    })
                {
                    self.remove_incarnation(message.participant_id, message.connection_id);
                }
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
                if !self.candidate_is_newer(pending.participant, pending.connection_id) {
                    self.core
                        .release_transport(pending.transport, tokio::time::Instant::now());
                    match reply {
                        MaterializationReply::Create(reply) => {
                            let _ = reply.send(Err(ControllerError::Superseded));
                        }
                        MaterializationReply::Patch(reply) => {
                            let _ = reply.send(Err(ControllerError::Superseded));
                        }
                    }
                    return;
                }
                let shard = self.router.sender(pending.shard);
                let sent = self
                    .wait_with_authorization_expiries(shard.send(command))
                    .await
                    .is_ok();
                let materialized = sent
                    && self
                        .wait_with_authorization_expiries(ack)
                        .await
                        .unwrap_or(false);
                metrics::histogram!("control_materialization_wait_us")
                    .record(started.elapsed().as_micros() as f64);
                let result = if materialized {
                    let transport = pending.transport;
                    let result = self.complete_materialization(pending, SystemTime::now());
                    if result.is_err() {
                        self.core
                            .release_transport(transport, tokio::time::Instant::now());
                    }
                    result
                } else {
                    self.core
                        .release_transport(pending.transport, tokio::time::Instant::now());
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
                address,
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
                    ShardCommand::AuthenticateTransport { source, address },
                ));
                self.emit_placeholder(owner_shard);
            }
            ShardEvent::ParticipantClosed {
                participant,
                connection_id,
            } => {
                self.remove_incarnation(participant, connection_id);
            }
        }
    }

    async fn wait_with_authorization_expiries<F>(&mut self, future: F) -> F::Output
    where
        F: Future,
    {
        tokio::pin!(future);
        loop {
            let Some(deadline) = self
                .authorization_expiries
                .first()
                .map(|work| work.deadline)
            else {
                return future.await;
            };
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    self.expire_authorizations(tokio::time::Instant::now(), SystemTime::now());
                }
                result = &mut future => return result,
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
        participant: ParticipantId,
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
            if !meta.materialized {
                continue;
            }
            self.stage_participant_at(
                meta.shard_id,
                generation,
                participant,
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
        debug_assert!(connection.address.shard().index() < self.router.shard_count());
        self.command_backlog.push_back((
            connection.address.shard(),
            ShardCommand::AdoptTcpConnection {
                stream: connection.stream,
                peer_addr: connection.peer_addr,
            },
        ));
    }

    fn begin_create_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
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
        let address = self.core.reserve_transport(shard, now);
        let creds = IceUfrag::new(self.cluster_id, self.node_id, address.route, address.epoch)
            .into_ice_creds();
        let (rtc, answer) = match self.negotiator.create_answer(offer, creds) {
            Ok(value) => value,
            Err(error) => {
                self.core.release_transport(address, now);
                return Err(error.into());
            }
        };
        let connection_id = state.connection_id;
        let authorization = state.authorization;
        let profile = state.profile;
        let config = self.core.prepare_participant(rtc, state);
        let room_id = config.room_id;
        let (ack_tx, ack_rx) = oneshot::channel();
        Ok(PendingMaterialization {
            shard,
            command: Some(ShardCommand::MaterializeParticipant {
                transport: address,
                config: Box::new(config),
                ack: ack_tx,
            }),
            ack: Some(ack_rx),
            participant: participant_id,
            connection_id,
            authorization,
            profile,
            transport: address,
            room_id,
            answer,
        })
    }

    fn complete_materialization(
        &mut self,
        pending: PendingMaterialization,
        wall_now: SystemTime,
    ) -> Result<SdpAnswer, ControllerError> {
        let participant_id = pending.participant;
        let room_id = pending.room_id;
        let previous = self.commit_candidate(
            participant_id,
            room_id,
            pending.shard,
            pending.transport,
            pending.connection_id,
            pending.authorization,
            pending.profile,
            wall_now,
        )?;
        if let Some(previous) = previous {
            self.terminate_incarnation(participant_id, previous, false);
        }
        let generation = self.lifecycle.next_generation();
        self.stage_participant_change_at(room_id, generation, Some(participant_id), None);
        if let Some(meta) = self.core.registry.get_participant(&participant_id) {
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
                participant_id,
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
        Ok(pending.answer)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the commit fence checks one complete prepared connection incarnation"
    )]
    fn commit_candidate(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard: ShardId,
        transport: crate::route::NodeTransportAddress,
        connection_id: ConnectionId,
        authorization: Option<AuthorizationLease>,
        profile: ConnectionProfile,
        wall_now: SystemTime,
    ) -> Result<Option<crate::control::registry::ParticipantMeta>, ControllerError> {
        if authorization.is_some_and(|lease| lease.is_expired_at(wall_now)) {
            return Err(ControllerError::AuthorizationExpired);
        }
        let previous = self
            .core
            .registry
            .commit_candidate(
                participant_id,
                room_id,
                shard,
                transport,
                connection_id,
                profile,
            )
            .map_err(|_| ControllerError::Superseded)?;
        if let Some(lease) = authorization {
            self.core
                .registry
                .update_authorization(&participant_id, connection_id, lease);
            self.authorization_expiries.insert(AuthorizationExpiryWork {
                deadline: lease.deadline(),
                participant_id,
                connection_id,
            });
        }
        Ok(previous)
    }

    fn candidate_is_newer(&self, participant: ParticipantId, connection_id: ConnectionId) -> bool {
        self.core
            .registry
            .get_participant(&participant)
            .map(|meta| meta.connection_id)
            .is_none_or(|current| connection_id > current)
    }

    fn begin_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: SdpOffer,
    ) -> Result<PendingMaterialization, ControllerError> {
        let current = self
            .core
            .registry
            .get_participant(&state.participant_id)
            .map(|meta| meta.connection_id);
        if current != state.old_connection_id {
            return Err(ControllerError::Superseded);
        }
        self.begin_create_participant(state, offer)
    }

    fn remove_incarnation(&mut self, participant: ParticipantId, connection_id: ConnectionId) {
        let Some(meta) = self
            .core
            .registry
            .get_participant(&participant)
            .copied()
            .filter(|meta| meta.connection_id == connection_id)
        else {
            return;
        };
        self.terminate_incarnation(participant, meta, true);
    }

    fn terminate_incarnation(
        &mut self,
        participant: ParticipantId,
        meta: crate::control::registry::ParticipantMeta,
        remove_current: bool,
    ) {
        if let Some(lease) = meta.authorization {
            self.authorization_expiries
                .remove(&AuthorizationExpiryWork {
                    deadline: lease.deadline(),
                    participant_id: participant,
                    connection_id: meta.connection_id,
                });
        }
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
        self.stage_participant_change_at(meta.room_id, generation, None, Some(participant));
        if remove_current {
            let _ = self
                .core
                .remove_incarnation(&participant, meta.connection_id);
        }
        self.publish_staged();
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        let Some(address) = meta.transport else {
            return;
        };
        let generation = self.lifecycle.next_generation();
        if let Some(update) = self.updates.get_mut(meta.shard_id.index()) {
            update.stage(
                generation,
                crate::shard_update::ShardUpdateOp::RetireTransport { address },
            );
            update.stage(
                generation,
                crate::shard_update::ShardUpdateOp::RemoveParticipant {
                    participant,
                    address,
                },
            );
            self.mark_update_touched(meta.shard_id);
            self.publish_staged();
        }
        self.core
            .release_transport(address, tokio::time::Instant::now());
    }

    fn expire_authorizations(&mut self, runtime_now: tokio::time::Instant, wall_now: SystemTime) {
        while self
            .authorization_expiries
            .first()
            .is_some_and(|work| work.deadline <= runtime_now)
        {
            let Some(work) = self.authorization_expiries.pop_first() else {
                break;
            };
            let Some(meta) = self
                .core
                .registry
                .get_participant(&work.participant_id)
                .copied()
                .filter(|meta| meta.connection_id == work.connection_id)
            else {
                continue;
            };
            let Some(lease) = meta.authorization else {
                continue;
            };
            if lease.is_expired_at(wall_now) {
                self.remove_incarnation(work.participant_id, work.connection_id);
            } else if let Ok(lease) = lease.refresh_at(wall_now, runtime_now) {
                self.core.registry.update_authorization(
                    &work.participant_id,
                    work.connection_id,
                    lease,
                );
                self.authorization_expiries.insert(AuthorizationExpiryWork {
                    deadline: lease.deadline(),
                    ..work
                });
            }
        }
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

#[cfg(test)]
mod replacement_tests {
    use super::*;
    use crate::{
        control::registry::CommitCandidateError, entity::RoomExternalId,
        shard::metrics::ShardMetrics, shard_update::new_shard_update,
    };

    #[allow(
        clippy::disallowed_types,
        reason = "the production ShardContext owns the sanctioned shared occupancy counters"
    )]
    fn actor() -> ControllerActor {
        let (command_tx, _command_rx) = mailbox::new(4);
        let context = ShardContext {
            command_tx,
            metrics: std::sync::Arc::new(ShardMetrics::new()),
        };
        let (update, _update_rx) = new_shard_update(ShardId::new(0));
        ControllerActor {
            router: crate::control::router::ShardRouter::new(vec![context]),
            core: ControllerCore::with_shards(1, 1, RoomPlacement::Hashed),
            negotiator: Negotiator::new(Vec::new()),
            tcp_listener: None,
            cluster_id: 0,
            node_id: 0,
            updates: vec![update],
            update_touched: vec![false],
            pending_updates: VecDeque::new(),
            update_queued: vec![false],
            egress_ready: false,
            lifecycle: TrackLifecycle::new(1),
            command_backlog: VecDeque::new(),
            authorization_expiries: BTreeSet::new(),
            steering: None,
        }
    }

    fn connection_id(sequence: u8) -> ConnectionId {
        ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, sequence])
    }

    fn authorization_lease(
        expiry: u64,
        wall_now: SystemTime,
        runtime_now: tokio::time::Instant,
    ) -> AuthorizationLease {
        AuthorizationLease::from_expiry(
            AuthorizationExpiry::from_unix_seconds(expiry),
            wall_now,
            runtime_now,
        )
        .unwrap()
    }

    #[test]
    fn preparation_crossing_authorization_expiry_cannot_commit() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expired-prepare").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = tokio::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(10, UNIX_EPOCH, runtime_now);

        let result = actor.commit_candidate(
            participant_id,
            room_id,
            ShardId::new(0),
            transport,
            connection_id(1),
            Some(lease),
            ConnectionProfile::Native,
            UNIX_EPOCH + Duration::from_secs(10),
        );

        assert!(matches!(result, Err(ControllerError::AuthorizationExpired)));
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none()
        );
        assert!(actor.authorization_expiries.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn current_connection_closes_at_authorization_deadline_without_grace() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expiry").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = tokio::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(10, UNIX_EPOCH, runtime_now);
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                transport,
                connection_id(1),
                Some(lease),
                ConnectionProfile::Native,
                UNIX_EPOCH,
            )
            .unwrap();

        tokio::time::advance(Duration::from_secs(10)).await;
        actor.expire_authorizations(
            tokio::time::Instant::now(),
            UNIX_EPOCH + Duration::from_secs(10),
        );

        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none()
        );
        assert!(actor.authorization_expiries.is_empty());
    }

    #[test]
    fn old_expiry_work_cannot_close_replacement() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expiry-fence").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = tokio::time::Instant::now();
        let old_id = connection_id(1);
        let current_id = connection_id(2);
        let old_lease = authorization_lease(10, UNIX_EPOCH, runtime_now);
        let old_transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                old_transport,
                old_id,
                Some(old_lease),
                ConnectionProfile::Native,
                UNIX_EPOCH,
            )
            .unwrap();
        let old_work = *actor.authorization_expiries.first().unwrap();
        let current_lease = authorization_lease(20, UNIX_EPOCH, runtime_now);
        let current_transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let previous = actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                current_transport,
                current_id,
                Some(current_lease),
                ConnectionProfile::Native,
                UNIX_EPOCH,
            )
            .unwrap()
            .unwrap();
        actor.terminate_incarnation(participant_id, previous, false);

        actor.authorization_expiries.insert(old_work);
        actor.expire_authorizations(
            runtime_now + Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_secs(10),
        );

        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .connection_id,
            current_id
        );
        assert_eq!(actor.authorization_expiries.len(), 1);
    }

    #[test]
    fn distant_expiry_and_replacement_keep_bounded_live_state() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("bounded-expiry").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = tokio::time::Instant::now();

        for sequence in 1..=100 {
            let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
            let lease = authorization_lease(u64::MAX, UNIX_EPOCH, runtime_now);
            let previous = actor
                .commit_candidate(
                    participant_id,
                    room_id,
                    ShardId::new(0),
                    transport,
                    connection_id(sequence),
                    Some(lease),
                    ConnectionProfile::Native,
                    UNIX_EPOCH,
                )
                .unwrap();
            if let Some(previous) = previous {
                actor.terminate_incarnation(participant_id, previous, false);
            }
        }

        let current = actor
            .core
            .registry
            .get_participant(&participant_id)
            .unwrap();
        assert_eq!(current.connection_id, connection_id(100));
        assert!(current.authorization.is_some());
        assert_eq!(actor.authorization_expiries.len(), 1);
    }

    #[test]
    fn controller_orders_commits_and_fences_every_stale_teardown_source() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("replacement").unwrap());
        let participant_id = ParticipantId::new();
        let old = connection_id(1);
        let current = connection_id(2);
        let old_transport = actor
            .core
            .reserve_transport(ShardId::new(0), tokio::time::Instant::now());
        let current_transport = actor
            .core
            .reserve_transport(ShardId::new(0), tokio::time::Instant::now());
        actor
            .core
            .registry
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                old_transport,
                old,
                ConnectionProfile::Native,
            )
            .unwrap();
        actor
            .core
            .registry
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                current_transport,
                current,
                ConnectionProfile::Native,
            )
            .unwrap();

        assert_eq!(
            actor.core.registry.commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                old_transport,
                old,
                ConnectionProfile::Native,
            ),
            Err(CommitCandidateError::Superseded)
        );

        actor.process_command(
            DeleteParticipant {
                room_id,
                participant_id,
                connection_id: old,
                profile: ConnectionProfile::Native,
            }
            .into(),
        );
        for _ in 0..3 {
            actor.handle_shard_event((
                ShardId::new(0),
                ShardEvent::ParticipantClosed {
                    participant: participant_id,
                    connection_id: old,
                },
            ));
        }
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .connection_id,
            current
        );

        actor.process_command(
            DeleteParticipant {
                room_id,
                participant_id,
                connection_id: current,
                profile: ConnectionProfile::Whip,
            }
            .into(),
        );
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_some()
        );

        actor.process_command(
            DeleteParticipant {
                room_id,
                participant_id,
                connection_id: current,
                profile: ConnectionProfile::Native,
            }
            .into(),
        );
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none()
        );
    }
}
