use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::MAX_NATIVE_SDP_BYTES;
use crate::control::steering::Steering;
use crate::track::{DataTrackIntent, SelectionPolicy, TrackSelector};
use crate::{
    control::{
        core::{ControllerCore, RoomPartitionPlacement},
        lifecycle::{TrackLifecycle, TrackLifecycleOperation, TrackLifecycleOutcome},
        negotiator::{Negotiator, NegotiatorError},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_core::auth::{
    AuthorizationExpiry, KeyRegistry, MAX_COMPACT_TOKEN_LEN, TokenError, VerifiedAuthorization,
    verify_participant_token,
};
use pulsebeam_queue as mailbox;
use pulsebeam_rtc::LocalCandidate;
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::sync::oneshot;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub participant_external_id: crate::entity::ParticipantExternalId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
    pub authorization: Option<AuthorizationLease>,
    pub profile: ConnectionProfile,
    pub rtp_capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
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
    deadline: pulsebeam_runtime::time::Instant,
    revision: u64,
}

impl AuthorizationLease {
    pub fn from_expiry(
        expiry: AuthorizationExpiry,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
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
            revision: 1,
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
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, TokenError> {
        let refreshed = Self::from_expiry(self.expiry, wall_now, runtime_now)?;
        Ok(Self {
            revision: self.revision,
            ..refreshed
        })
    }

    pub(crate) fn revision(self) -> u64 {
        self.revision
    }

    fn replacing(
        self,
        expiry: AuthorizationExpiry,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, ControllerError> {
        let mut next = Self::from_expiry(expiry, wall_now, runtime_now)
            .map_err(|_| ControllerError::AuthorizationExpired)?;
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or(ControllerError::ServiceUnavailable)?;
        Ok(next)
    }

    pub(crate) fn deadline(self) -> pulsebeam_runtime::time::Instant {
        self.deadline
    }

    pub(crate) fn native_expiry(self) -> Result<i64, TokenError> {
        i64::try_from(self.expiry.unix_seconds()).map_err(|_| TokenError::Invalid)
    }

    pub(crate) fn expiry(self) -> AuthorizationExpiry {
        self.expiry
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AuthorizationExpiryWork {
    deadline: pulsebeam_runtime::time::Instant,
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
    DeleteNative(DeleteNative, oneshot::Sender<Result<(), ControllerError>>),
    ReplaceAuthorization(
        ReplaceAuthorization,
        oneshot::Sender<Result<u64, ControllerError>>,
    ),
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
    pub connection_id: ConnectionId,
    pub profile: ConnectionProfile,
}

#[derive(Debug)]
pub struct DeleteNative {
    pub authorization: VerifiedAuthorization,
    pub connection_id: ConnectionId,
}

#[derive(Debug)]
pub struct ReplaceAuthorization {
    pub authorization: VerifiedAuthorization,
    pub connection_id: ConnectionId,
    pub expected_revision: u64,
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
    #[error("participant connection was superseded")]
    Superseded,
    #[error("participant authorization expired")]
    AuthorizationExpired,
    #[error("native connection grant is gone")]
    GrantGone,
    #[error("authorization cannot replace this connection grant")]
    AuthorizationRejected,
    #[error("authorization precondition does not match")]
    AuthorizationPrecondition { revision: u64 },
    #[error("IO error: {0}")]
    IOError(#[from] io::Error),
    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHARD_EVENT_BUDGET: usize = 1_024;
const API_COMMAND_BUDGET: usize = 16;
const SHARD_COMMAND_EGRESS_BUDGET: usize = 64;
const SHARD_UPDATE_EGRESS_BUDGET: usize = 64;
const PENDING_NATIVE_DELETE_BYTES: usize = 64 * 1024;
const NATIVE_DELETE_TIMEOUT: Duration = Duration::from_secs(30);

enum NativeDeleteReply {
    Http(oneshot::Sender<Result<(), ControllerError>>),
    Publication {
        participant: ParticipantId,
        connection_id: ConnectionId,
        reference: u64,
    },
}

#[derive(Debug)]
struct PublicationCutoff {
    participant: ParticipantId,
    connection_id: ConnectionId,
    reference: u64,
    success: bool,
}

#[derive(Debug)]
struct NativeDeletionCompletion {
    bytes: usize,
    publication: Option<PublicationCutoff>,
}

struct PendingMaterialization {
    shard: ShardId,
    partition_index: usize,
    command: Option<ShardCommand>,
    ack: Option<oneshot::Receiver<bool>>,
    participant: ParticipantId,
    participant_external_id: crate::entity::ParticipantExternalId,
    connection_id: ConnectionId,
    authorization: Option<AuthorizationLease>,
    profile: ConnectionProfile,
    transport: crate::route::NodeTransportAddress,
    room_id: RoomId,
    answer: String,
}

enum RequiredAction {
    Materialize {
        pending: PendingMaterialization,
        reply: MaterializationReply,
    },
}

const MAX_PENDING_RENEWALS: usize = 32;

struct RenewalResult {
    participant: ParticipantId,
    connection_id: ConnectionId,
    expected_revision: u64,
    verified: Option<VerifiedAuthorization>,
}

enum MaterializationReply {
    Create(oneshot::Sender<Result<CreateParticipantReply, ControllerError>>),
    Patch(oneshot::Sender<Result<PatchParticipantReply, ControllerError>>),
}

pub struct ControllerActor {
    router: crate::control::router::ShardRouter,
    core: ControllerCore,
    negotiator: Negotiator,
    clock: crate::clock::WallAnchor,
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
    key_registry: Option<KeyRegistry>,
    renewal_tx: pulsebeam_runtime::sync::mpsc::Sender<RenewalResult>,
    renewal_rx: pulsebeam_runtime::sync::mpsc::Receiver<RenewalResult>,
    renewals_inflight: usize,
    deletion_tx: pulsebeam_runtime::sync::mpsc::Sender<NativeDeletionCompletion>,
    deletion_rx: pulsebeam_runtime::sync::mpsc::Receiver<NativeDeletionCompletion>,
    deletion_bytes: usize,
    steering: Option<Box<dyn Steering>>,
}

impl ControllerActor {
    pub(crate) fn with_placement(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<LocalCandidate>,
        room_partition_size: usize,
        placement: RoomPartitionPlacement,
        updates: Vec<crate::shard_update::ShardUpdateWriter>,
    ) -> Self {
        let shard_count = shard_contexts.len();
        debug_assert_eq!(updates.len(), shard_count);
        let (renewal_tx, renewal_rx) = pulsebeam_runtime::sync::mpsc::channel(MAX_PENDING_RENEWALS);
        let (deletion_tx, deletion_rx) = pulsebeam_runtime::sync::mpsc::channel(API_COMMAND_BUDGET);
        Self {
            router: crate::control::router::ShardRouter::new(shard_contexts),
            core: ControllerCore::with_shards(shard_count, room_partition_size, placement),
            negotiator: Negotiator::new(candidates),
            clock: crate::clock::WallAnchor::from(pulsebeam_runtime::net::ReceiveTime::now()),
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
            key_registry: None,
            renewal_tx,
            renewal_rx,
            renewals_inflight: 0,
            deletion_tx,
            deletion_rx,
            deletion_bytes: 0,
            steering: None,
        }
    }

    pub(crate) fn set_key_registry(&mut self, registry: Option<KeyRegistry>) {
        self.key_registry = registry;
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
        let mut poll_interval = pulsebeam_runtime::time::interval(SHARD_LOAD_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(pulsebeam_runtime::time::MissedTickBehavior::Skip);

        describe_controller_metrics();

        loop {
            let authorization_deadline = self
                .authorization_expiries
                .first()
                .map(|work| work.deadline);
            let maintenance_due = if self.has_ready_egress() {
                false
            } else {
                pulsebeam_runtime::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = pulsebeam_runtime::time::sleep_until(
                        authorization_deadline.unwrap_or_else(pulsebeam_runtime::time::Instant::now)
                    ), if authorization_deadline.is_some() => false,
                    true = shard_event_rx.readable() => false,
                    Some(bytes) = self.deletion_rx.recv() => {
                        self.release_deletion(bytes);
                        false
                    },
                    Some(result) = self.renewal_rx.recv() => {
                        self.finish_renewal(result);
                        false
                    },
                    _ = poll_interval.tick() => true,
                    true = command_rx.readable() => false,
                }
            };

            #[cfg(feature = "sim")]
            crate::sim_metrics::wait_controller_stall().await;

            let action = self.tick(&mut command_rx, &mut shard_event_rx, maintenance_due);
            if let Some(action) = action {
                self.run_required_action(action).await;
            }
        }
    }

    fn tick(
        &mut self,
        command_rx: &mut mailbox::Receiver<ControllerCommand>,
        shard_event_rx: &mut mailbox::Receiver<ShardEventMessage>,
        maintenance_due: bool,
    ) -> Option<RequiredAction> {
        let started = pulsebeam_runtime::time::Instant::now();
        for _ in 0..API_COMMAND_BUDGET {
            let Ok(bytes) = self.deletion_rx.try_recv() else {
                break;
            };
            self.release_deletion(bytes);
        }
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
            ControllerCommand::DeleteNative(message, reply) => {
                match self.begin_native_delete(message, SystemTime::now()) {
                    Ok((bytes, acks)) => {
                        let completed = self.deletion_tx.clone();
                        pulsebeam_runtime::task::spawn(async move {
                            complete_native_delete(
                                acks,
                                NativeDeleteReply::Http(reply),
                                completed,
                                bytes,
                            )
                            .await;
                        });
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
                None
            }
            ControllerCommand::ReplaceAuthorization(message, reply) => {
                let result = self.replace_authorization(
                    message,
                    SystemTime::now(),
                    pulsebeam_runtime::time::Instant::now(),
                );
                let _ = reply.send(result);
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
                let started = pulsebeam_runtime::time::Instant::now();
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
                    self.core.release_transport(
                        pending.transport,
                        pulsebeam_runtime::time::Instant::now(),
                    );
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
                    let participant = pending.participant;
                    let shard = pending.shard;
                    let result = self.complete_materialization(pending, SystemTime::now());
                    if result.is_err() {
                        let generation = self.lifecycle.next_generation();
                        if let Some(update) = self.updates.get_mut(shard.index()) {
                            update.stage(
                                generation,
                                crate::shard_update::ShardUpdateOp::RemoveParticipant {
                                    participant,
                                    address: transport,
                                },
                            );
                            self.mark_update_touched(shard);
                            self.publish_staged();
                        }
                        self.core
                            .release_transport(transport, pulsebeam_runtime::time::Instant::now());
                    }
                    result
                } else {
                    self.core.release_transport(
                        pending.transport,
                        pulsebeam_runtime::time::Instant::now(),
                    );
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

    fn current_incarnation(&self, participant: ParticipantId, connection_id: ConnectionId) -> bool {
        self.core
            .registry
            .get_participant(&participant)
            .is_some_and(|meta| meta.connection_id == connection_id)
    }

    fn handle_shard_event(&mut self, (_shard, event): ShardEventMessage) {
        match event {
            ShardEvent::TrackPublished(track) => {
                if !self.current_incarnation(track.meta().origin, track.meta().connection_id) {
                    return;
                }
                if let Some(outcome) = self.lifecycle.publish(
                    track,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                ) {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::OwnedTrackUnpublished { meta, reference } => {
                self.begin_owned_unpublish(meta, reference);
            }
            ShardEvent::TrackUnpublished(meta) => {
                if !self.current_incarnation(meta.origin, meta.connection_id) {
                    return;
                }
                if let Some(outcome) = self.lifecycle.unpublish(
                    meta.origin,
                    meta.id,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                ) {
                    self.apply_track_lifecycle(outcome);
                    self.publish_staged();
                }
            }
            ShardEvent::OwnedSubscriptionSet {
                connection_id,
                request,
            } => {
                if request.reference == 0
                    || !self
                        .core
                        .registry
                        .get_participant(&request.subscriber)
                        .is_some_and(|meta| {
                            meta.connection_id == connection_id
                                && meta.room_id == request.room_id
                                && meta.profile == ConnectionProfile::Native
                        })
                {
                    return;
                }
                let outcomes = self.lifecycle.set_owned_subscription(
                    request,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::OwnedSubscriptionRemoved {
                room_id,
                subscriber,
                connection_id,
                reference,
            } => {
                self.begin_owned_cutoff(subscriber, connection_id, reference, room_id, None);
            }
            ShardEvent::TrackSubscribed {
                subscriber,
                connection_id,
                track,
                ..
            } => {
                if !self.current_incarnation(subscriber, connection_id) {
                    return;
                }
                let outcome = self.lifecycle.activate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackUnsubscribed {
                subscriber,
                connection_id,
                track,
                ..
            } => {
                if !self.current_incarnation(subscriber, connection_id) {
                    return;
                }
                let outcome = self.lifecycle.deactivate(
                    track.room_id,
                    track.origin,
                    track.id,
                    subscriber,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            ShardEvent::TrackSubscriptionAdded {
                room_id,
                subscriber,
                connection_id,
                selector,
                selection,
            } => {
                if !self.current_incarnation(subscriber, connection_id) {
                    return;
                }
                let outcomes = self.lifecycle.subscribe(
                    room_id,
                    subscriber,
                    selector,
                    selection,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::TrackSubscriptionRemoved {
                room_id,
                subscriber,
                connection_id,
                selector,
            } => {
                if !self.current_incarnation(subscriber, connection_id) {
                    return;
                }
                let outcomes = self.lifecycle.unsubscribe(
                    room_id,
                    subscriber,
                    selector,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
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
            ShardEvent::RenewAuthorization {
                participant,
                connection_id,
                token,
            } => self.begin_renewal(_shard, participant, connection_id, token.0),
        }
    }

    fn release_deletion(&mut self, completed: NativeDeletionCompletion) {
        self.deletion_bytes = self
            .deletion_bytes
            .checked_sub(completed.bytes)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("native deletion credit released twice"));
        if let Some(result) = completed.publication
            && let Some(meta) = self.core.registry.get_participant(&result.participant)
            && meta.connection_id == result.connection_id
            && meta.profile == ConnectionProfile::Native
        {
            self.command_backlog.push_back((
                meta.shard_id,
                ShardCommand::PublicationCutoffResult {
                    participant: result.participant,
                    connection_id: result.connection_id,
                    reference: result.reference,
                    success: result.success,
                },
            ));
        }
    }

    fn begin_owned_unpublish(&mut self, meta: crate::track::TrackMeta, reference: u64) {
        self.begin_owned_cutoff(
            meta.origin,
            meta.connection_id,
            reference,
            meta.room_id,
            Some(meta),
        );
    }

    fn begin_owned_cutoff(
        &mut self,
        participant: ParticipantId,
        connection_id: ConnectionId,
        reference: u64,
        room_id: RoomId,
        publication: Option<crate::track::TrackMeta>,
    ) {
        let Some(shard) = self
            .core
            .registry
            .get_participant(&participant)
            .and_then(|current| {
                (current.connection_id == connection_id
                    && current.room_id == room_id
                    && current.profile == ConnectionProfile::Native)
                    .then_some(current.shard_id)
            })
        else {
            return;
        };
        let bytes = self
            .updates
            .len()
            .saturating_mul(
                std::mem::size_of::<crate::shard_update::ShardUpdate>()
                    .saturating_add(std::mem::size_of::<oneshot::Receiver<()>>())
                    .saturating_add(std::mem::size_of::<bool>()),
            )
            .saturating_add(std::mem::size_of::<NativeDeletionCompletion>().saturating_mul(2))
            .saturating_add(std::mem::size_of::<NativeDeleteReply>());
        let mut acks = Vec::new();
        let mut affected = Vec::new();
        let admitted = reference != 0
            && self.deletion_bytes.saturating_add(bytes) <= PENDING_NATIVE_DELETE_BYTES
            && !self
                .updates
                .iter()
                .any(crate::shard_update::ShardUpdateWriter::is_closed)
            && acks.try_reserve_exact(self.updates.len()).is_ok()
            && affected.try_reserve_exact(self.updates.len()).is_ok();
        if admitted {
            affected.resize(self.updates.len(), false);
        }
        self.publish_staged();
        let outcomes = if let Some(meta) = publication {
            self.lifecycle
                .unpublish(
                    meta.origin,
                    meta.id,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .into_iter()
                .collect()
        } else {
            self.lifecycle.unsubscribe_owned(
                room_id,
                participant,
                reference,
                &self.core.registry,
                pulsebeam_runtime::time::Instant::now(),
            )
        };
        let generation = outcomes
            .last()
            .map(|outcome| outcome.generation)
            .unwrap_or_else(|| self.lifecycle.next_generation());
        for outcome in outcomes {
            if admitted {
                for op in &outcome.operations {
                    let target = match op {
                        TrackLifecycleOperation::Update { shard, .. }
                        | TrackLifecycleOperation::Plans { shard, .. }
                        | TrackLifecycleOperation::ParticipantEffect { shard, .. } => *shard,
                    };
                    if let Some(touched) = affected.get_mut(target.index()) {
                        *touched = true;
                    }
                }
            }
            self.apply_track_lifecycle(outcome);
        }
        if !admitted {
            // Local execution is fenced; withdraw topology even if receipt credit is unavailable.
            self.publish_staged();
            self.release_deletion(NativeDeletionCompletion {
                bytes: 0,
                publication: Some(PublicationCutoff {
                    participant,
                    connection_id,
                    reference,
                    success: false,
                }),
            });
            return;
        }
        // Cover previously staged local work even when this withdrawal is already satisfied.
        if let Some(touched) = affected.get_mut(shard.index()) {
            *touched = true;
        }
        for (index, touched) in affected.into_iter().enumerate() {
            if touched && let Some(writer) = self.updates.get_mut(index) {
                acks.push(writer.stage_cutoff(generation));
                self.mark_update_touched(ShardId::new(index));
            }
        }
        self.deletion_bytes = self.deletion_bytes.saturating_add(bytes);
        self.publish_staged();
        let completed = self.deletion_tx.clone();
        pulsebeam_runtime::task::spawn(complete_native_delete(
            acks,
            NativeDeleteReply::Publication {
                participant,
                connection_id,
                reference,
            },
            completed,
            bytes,
        ));
    }

    fn begin_native_delete(
        &mut self,
        request: DeleteNative,
        wall_now: SystemTime,
    ) -> Result<(usize, Vec<oneshot::Receiver<()>>), ControllerError> {
        let verified = request.authorization;
        if verified.expiry.is_expired_at(
            wall_now
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ) {
            return Err(ControllerError::AuthorizationExpired);
        }
        let bytes = self
            .updates
            .len()
            .saturating_mul(
                std::mem::size_of::<crate::shard_update::ShardUpdate>()
                    .saturating_add(std::mem::size_of::<oneshot::Receiver<()>>()),
            )
            .saturating_add(std::mem::size_of::<ControllerCommand>());
        if self.deletion_bytes.saturating_add(bytes) > PENDING_NATIVE_DELETE_BYTES
            || self
                .updates
                .iter()
                .any(crate::shard_update::ShardUpdateWriter::is_closed)
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let mut acks = Vec::new();
        acks.try_reserve_exact(self.updates.len())
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        if self
            .core
            .registry
            .get_participant(&verified.participant_id)
            .is_some_and(|meta| {
                meta.room_id == verified.room_id
                    && meta.profile == ConnectionProfile::Native
                    && meta.participant_external_id.as_ref()
                        == Some(&verified.participant_external_id)
            })
        {
            self.remove_incarnation(verified.participant_id, request.connection_id);
        }
        // Fence every writer: an old Location may already have teardown queued on a former shard.
        let generation = self.lifecycle.next_generation();
        for (index, writer) in self.updates.iter_mut().enumerate() {
            acks.push(writer.stage_cutoff(generation));
            if let Some(touched) = self.update_touched.get_mut(index) {
                *touched = true;
            }
        }
        self.deletion_bytes = self.deletion_bytes.saturating_add(bytes);
        self.publish_staged();
        Ok((bytes, acks))
    }

    fn replace_authorization(
        &mut self,
        request: ReplaceAuthorization,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<u64, ControllerError> {
        let verified = request.authorization;
        let meta = self
            .core
            .registry
            .get_participant(&verified.participant_id)
            .ok_or(ControllerError::GrantGone)?;
        if meta.connection_id != request.connection_id || meta.profile != ConnectionProfile::Native
        {
            return Err(ControllerError::GrantGone);
        }
        if meta.room_id != verified.room_id
            || meta.participant_external_id.as_ref() != Some(&verified.participant_external_id)
        {
            return Err(ControllerError::AuthorizationRejected);
        }
        let previous = meta.authorization.ok_or(ControllerError::GrantGone)?;
        if previous.is_expired_at(wall_now) {
            self.remove_incarnation(verified.participant_id, request.connection_id);
            return Err(ControllerError::GrantGone);
        }
        if previous.revision() != request.expected_revision {
            return Err(ControllerError::AuthorizationPrecondition {
                revision: previous.revision(),
            });
        }
        let expiry = i64::try_from(verified.expiry.unix_seconds())
            .map_err(|_| ControllerError::AuthorizationRejected)?;
        // Bound queued imperative work, not live connections or grant lifetimes.
        const GRANT_COMMAND_BACKLOG_BYTES: usize = 64 * 1024;
        if self
            .command_backlog
            .len()
            .saturating_add(1)
            .saturating_mul(std::mem::size_of::<(ShardId, ShardCommand)>())
            > GRANT_COMMAND_BACKLOG_BYTES
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let shard = meta.shard_id;
        let lease = previous.replacing(verified.expiry, wall_now, runtime_now)?;
        self.install_authorization(
            verified.participant_id,
            request.connection_id,
            previous,
            lease,
        );
        self.send_renewal_result(
            shard,
            verified.participant_id,
            request.connection_id,
            lease.revision(),
            Some(expiry),
        );
        Ok(lease.revision())
    }

    fn install_authorization(
        &mut self,
        participant: ParticipantId,
        connection_id: ConnectionId,
        previous: AuthorizationLease,
        lease: AuthorizationLease,
    ) {
        self.authorization_expiries
            .remove(&AuthorizationExpiryWork {
                deadline: previous.deadline(),
                participant_id: participant,
                connection_id,
            });
        self.core
            .registry
            .update_authorization(&participant, connection_id, lease);
        self.authorization_expiries.insert(AuthorizationExpiryWork {
            deadline: lease.deadline(),
            participant_id: participant,
            connection_id,
        });
    }

    fn begin_renewal(
        &mut self,
        shard: ShardId,
        participant: ParticipantId,
        connection_id: ConnectionId,
        token: String,
    ) {
        let Some(meta) = self.core.registry.get_participant(&participant) else {
            return;
        };
        if meta.connection_id != connection_id
            || meta.shard_id != shard
            || meta.authorization.is_none()
        {
            return;
        }
        let expected_revision = meta.authorization.map_or(0, AuthorizationLease::revision);
        let Some(registry) = self.key_registry.clone() else {
            self.send_renewal_result(shard, participant, connection_id, expected_revision, None);
            return;
        };
        if token.len() > MAX_COMPACT_TOKEN_LEN || self.renewals_inflight >= MAX_PENDING_RENEWALS {
            self.send_renewal_result(shard, participant, connection_id, expected_revision, None);
            return;
        }
        self.renewals_inflight = self.renewals_inflight.saturating_add(1);
        let tx = self.renewal_tx.clone();
        pulsebeam_runtime::task::spawn_blocking(move || {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            let verified = verify_participant_token(&registry, &token, now).ok();
            let _ = tx.blocking_send(RenewalResult {
                participant,
                connection_id,
                expected_revision,
                verified,
            });
        });
    }

    fn finish_renewal(&mut self, result: RenewalResult) {
        self.renewals_inflight = self.renewals_inflight.saturating_sub(1);
        let Some(meta) = self.core.registry.get_participant(&result.participant) else {
            return;
        };
        if meta.connection_id != result.connection_id || meta.profile != ConnectionProfile::Native {
            return;
        }
        let shard = meta.shard_id;
        let previous = meta.authorization;
        let wall_now = SystemTime::now();
        let expiry = result.verified.and_then(|verified| {
            if verified.participant_id != result.participant
                || verified.room_id != meta.room_id
                || Some(verified.participant_external_id) != meta.participant_external_id
                || previous.is_none_or(|lease| {
                    lease.revision() != result.expected_revision
                        || lease.is_expired_at(wall_now)
                        || verified.expiry.unix_seconds() <= lease.expiry.unix_seconds()
                })
            {
                return None;
            }
            i64::try_from(verified.expiry.unix_seconds()).ok()?;
            previous?
                .replacing(
                    verified.expiry,
                    wall_now,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .ok()
        });
        if let (Some(lease), Some(previous)) = (expiry, previous) {
            self.install_authorization(result.participant, result.connection_id, previous, lease);
        }
        self.send_renewal_result(
            shard,
            result.participant,
            result.connection_id,
            expiry.map_or(result.expected_revision, AuthorizationLease::revision),
            expiry.and_then(|lease| i64::try_from(lease.expiry.unix_seconds()).ok()),
        );
    }

    fn send_renewal_result(
        &mut self,
        shard: ShardId,
        participant: ParticipantId,
        connection_id: ConnectionId,
        revision: u64,
        expiry: Option<i64>,
    ) {
        self.command_backlog.push_back((
            shard,
            ShardCommand::AuthorizationResult {
                participant,
                connection_id,
                revision,
                expiry,
            },
        ));
    }

    async fn wait_with_authorization_expiries<F>(&mut self, future: F) -> F::Output
    where
        F: Future,
    {
        pulsebeam_runtime::pin!(future);
        loop {
            let Some(deadline) = self
                .authorization_expiries
                .first()
                .map(|work| work.deadline)
            else {
                return future.await;
            };
            pulsebeam_runtime::select! {
                biased;
                _ = pulsebeam_runtime::time::sleep_until(deadline) => {
                    self.expire_authorizations(pulsebeam_runtime::time::Instant::now(), SystemTime::now());
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

    fn room_peer(&self, id: ParticipantId) -> crate::participant::effect::RoomPeer {
        crate::participant::effect::RoomPeer {
            id,
            external_id: self
                .core
                .registry
                .get_participant(&id)
                .and_then(|meta| meta.participant_external_id.clone()),
        }
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
            .local_participant_ids_in_room(&room_id)
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
                    added: added.into_iter().map(|id| self.room_peer(id)).collect(),
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

    fn begin_create_participant(
        &mut self,
        mut state: ParticipantState,
        offer: String,
    ) -> Result<PendingMaterialization, ControllerError> {
        if state.profile == ConnectionProfile::Native {
            if offer.len() > MAX_NATIVE_SDP_BYTES {
                return Err(NegotiatorError::InvalidResources.into());
            }
            if let Some(lease) = state.authorization {
                lease
                    .native_expiry()
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
            }
        }
        let participant_id = state.participant_id;
        let (slot, placement) = self
            .core
            .next_room_partition(&state.room_id, &state.participant_id);
        // A replacement (native POST or fenced PATCH) changes the transport,
        // not placement. Counting an already-live participant as a new admission
        // would collapse room reconfigurations onto one shard.
        let current_owner = self
            .core
            .registry
            .get_participant(&participant_id)
            .map(|meta| meta.shard_id);
        let shard = match current_owner {
            Some(shard) => shard,
            None => match placement {
                RoomPartitionPlacement::Hashed => self
                    .router
                    .stable_room_partition(&state.room_id, slot)
                    .ok_or(ControllerError::ServiceUnavailable)?,
                RoomPartitionPlacement::RoundRobin => {
                    let shard_count = self.router.shard_count();
                    debug_assert_ne!(shard_count, 0);
                    ShardId::new(
                        slot.checked_rem(shard_count)
                            .ok_or(ControllerError::ServiceUnavailable)?,
                    )
                }
            },
        };
        let at = self
            .clock
            .observe_execution(pulsebeam_runtime::net::ReceiveTime::now());
        let now = at.monotonic;
        let rtp_capabilities = if state.profile == ConnectionProfile::Native {
            let capabilities = state
                .rtp_capabilities
                .as_ref()
                .ok_or(NegotiatorError::InvalidResources)?;
            Some(
                crate::participant::rtp::transport_capabilities(capabilities)
                    .map_err(|_| NegotiatorError::InvalidResources)?,
            )
        } else {
            None
        };
        let address = self.core.reserve_transport(shard, now);
        let creds = IceUfrag::new(self.cluster_id, self.node_id, address.route, address.epoch)
            .into_ice_creds();
        let (mut rtc, answer, mut resources) =
            match self
                .negotiator
                .create_answer(offer, creds, at, rtp_capabilities)
            {
                Ok(value) => value,
                Err(error) => {
                    self.core.release_transport(address, now);
                    return Err(error.into());
                }
            };
        if state.profile == ConnectionProfile::Native && answer.len() > MAX_NATIVE_SDP_BYTES {
            self.core.release_transport(address, now);
            return Err(NegotiatorError::InvalidResources.into());
        }
        if state.profile == ConnectionProfile::Native {
            let initialized = state
                .rtp_capabilities
                .take()
                .ok_or(pulsebeam_rtc::RtpConfigurationError::Invalid)
                .and_then(|capabilities| {
                    rtc.connection
                        .update_rtp(pulsebeam_rtc::RtpConfigurationPatch::default())?;
                    resources.native_rtp_capabilities = Some(capabilities);
                    Ok(())
                });
            if let Err(error) = initialized {
                self.core.release_transport(address, now);
                return Err(match error {
                    pulsebeam_rtc::RtpConfigurationError::Capacity => {
                        ControllerError::ServiceUnavailable
                    }
                    pulsebeam_rtc::RtpConfigurationError::CapabilityState
                    | pulsebeam_rtc::RtpConfigurationError::ConnectionState => {
                        ControllerError::Unknown("native RTP initialization failed".to_owned())
                    }
                    _ => NegotiatorError::InvalidResources.into(),
                });
            }
        }
        if state.profile == ConnectionProfile::Native
            && let Err((reason, _)) = rtc
                .connection
                .open_data_channel(now.into(), DataTrackIntent::native_signaling_config())
        {
            self.core.release_transport(address, now);
            return Err(match reason {
                pulsebeam_rtc::WriteError::InvalidState => NegotiatorError::InvalidResources.into(),
                _ => ControllerError::ServiceUnavailable,
            });
        }
        let connection_id = state.connection_id;
        let participant_external_id = state.participant_external_id.clone();
        let authorization = state.authorization;
        let profile = state.profile;
        let config = self.core.prepare_participant(rtc, resources, state);
        let room_id = config.room_id;
        let (ack_tx, ack_rx) = oneshot::channel();
        Ok(PendingMaterialization {
            shard,
            partition_index: slot,
            command: Some(ShardCommand::MaterializeParticipant {
                transport: address,
                config: Box::new(config),
                ack: ack_tx,
            }),
            ack: Some(ack_rx),
            participant: participant_id,
            participant_external_id,
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
    ) -> Result<String, ControllerError> {
        let participant_id = pending.participant;
        let room_id = pending.room_id;
        let previous = self.commit_candidate(
            participant_id,
            room_id,
            pending.shard,
            pending.partition_index,
            pending.transport,
            pending.connection_id,
            pending.authorization,
            pending.profile,
            pending.participant_external_id,
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
                .local_participant_ids_in_room(&room_id)
                .into_iter()
                .filter(|participant| *participant != participant_id)
                .map(|id| self.room_peer(id))
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
        let defaults = if pending.profile == ConnectionProfile::Native {
            vec![
                (TrackSelector::audio(), SelectionPolicy::Allocated),
                (TrackSelector::video(), SelectionPolicy::Allocated),
                (TrackSelector::data(), SelectionPolicy::Allocated),
            ]
        } else {
            vec![
                (TrackSelector::audio(), SelectionPolicy::All),
                (TrackSelector::video(), SelectionPolicy::Allocated),
            ]
        };
        let outcomes = self.lifecycle.subscribe_defaults(
            room_id,
            participant_id,
            defaults,
            &self.core.registry,
            pulsebeam_runtime::time::Instant::now(),
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
        partition_index: usize,
        transport: crate::route::NodeTransportAddress,
        connection_id: ConnectionId,
        authorization: Option<AuthorizationLease>,
        profile: ConnectionProfile,
        participant_external_id: crate::entity::ParticipantExternalId,
        wall_now: SystemTime,
    ) -> Result<Option<crate::control::registry::ParticipantMeta>, ControllerError> {
        if authorization.is_some_and(|lease| lease.is_expired_at(wall_now)) {
            return Err(ControllerError::AuthorizationExpired);
        }
        if profile == ConnectionProfile::Native
            && let Some(lease) = authorization
        {
            lease
                .native_expiry()
                .map_err(|_| ControllerError::AuthorizationRejected)?;
        }
        // A timeout can remove the last participant while materialization is
        // in flight. Never commit a prepared connection to a retired slot.
        if self.core.next_room_partition(&room_id, &participant_id).0 != partition_index {
            return Err(ControllerError::ServiceUnavailable);
        }
        let previous = self
            .core
            .registry
            .commit_candidate(
                participant_id,
                room_id,
                shard,
                partition_index,
                transport,
                connection_id,
                profile,
                Some(participant_external_id),
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
        offer: String,
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
            .cloned()
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
            pulsebeam_runtime::time::Instant::now(),
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
            .release_transport(address, pulsebeam_runtime::time::Instant::now());
    }

    fn expire_authorizations(
        &mut self,
        runtime_now: pulsebeam_runtime::time::Instant,
        wall_now: SystemTime,
    ) {
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
                .cloned()
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

async fn drain_cutoffs(acks: &mut Vec<oneshot::Receiver<()>>) -> bool {
    let mut success = true;
    while let Some(ack) = acks.last_mut() {
        success &= ack.await.is_ok();
        acks.pop();
    }
    success
}

async fn complete_native_delete(
    mut acks: Vec<oneshot::Receiver<()>>,
    reply: NativeDeleteReply,
    completed: pulsebeam_runtime::sync::mpsc::Sender<NativeDeletionCompletion>,
    bytes: usize,
) {
    let success =
        pulsebeam_runtime::time::timeout(NATIVE_DELETE_TIMEOUT, drain_cutoffs(&mut acks)).await;
    match reply {
        NativeDeleteReply::Http(reply) => {
            let _ = reply.send(match success {
                Ok(true) => Ok(()),
                _ => Err(ControllerError::ServiceUnavailable),
            });
        }
        NativeDeleteReply::Publication {
            participant,
            connection_id,
            reference,
        } => {
            let _ = completed
                .send(NativeDeletionCompletion {
                    bytes: 0,
                    publication: Some(PublicationCutoff {
                        participant,
                        connection_id,
                        reference,
                        success: matches!(success, Ok(true)),
                    }),
                })
                .await;
        }
    }
    // A timed-out caller does not free the credit for fences still retained in shard lanes.
    drain_cutoffs(&mut acks).await;
    let _ = completed
        .send(NativeDeletionCompletion {
            bytes,
            publication: None,
        })
        .await;
}

pub type ControllerSender = mailbox::Sender<ControllerCommand>;

#[cfg(test)]
mod replacement_tests {
    use super::*;
    use crate::{
        control::registry::CommitCandidateError, entity::RoomExternalId,
        shard::metrics::ShardMetrics, shard_update::new_shard_update,
    };

    fn actor() -> ControllerActor {
        actor_with_shards(1)
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the production ShardContext owns the sanctioned shared occupancy counters"
    )]
    fn actor_with_shards(shard_count: usize) -> ControllerActor {
        let (renewal_tx, renewal_rx) = pulsebeam_runtime::sync::mpsc::channel(MAX_PENDING_RENEWALS);
        let (deletion_tx, deletion_rx) = pulsebeam_runtime::sync::mpsc::channel(API_COMMAND_BUDGET);
        let mut contexts = Vec::new();
        let mut updates = Vec::new();
        for index in 0..shard_count {
            let (command_tx, _command_rx) =
                mailbox::bounded(const { core::num::NonZeroUsize::new(4).unwrap() });
            contexts.push(ShardContext {
                command_tx,
                metrics: std::sync::Arc::new(ShardMetrics::new()),
            });
            let (update, _update_rx) = new_shard_update(ShardId::new(index));
            updates.push(update);
        }
        ControllerActor {
            router: crate::control::router::ShardRouter::new(contexts),
            core: ControllerCore::with_shards(shard_count, 1, RoomPartitionPlacement::Hashed),
            negotiator: Negotiator::new(Vec::new()),
            clock: crate::clock::WallAnchor::from(pulsebeam_runtime::net::ReceiveTime::now()),
            cluster_id: 0,
            node_id: 0,
            updates,
            update_touched: vec![false; shard_count],
            pending_updates: VecDeque::new(),
            update_queued: vec![false; shard_count],
            egress_ready: false,
            lifecycle: TrackLifecycle::new(shard_count),
            command_backlog: VecDeque::new(),
            authorization_expiries: BTreeSet::new(),
            key_registry: None,
            renewal_tx,
            renewal_rx,
            renewals_inflight: 0,
            deletion_tx,
            deletion_rx,
            deletion_bytes: 0,
            steering: None,
        }
    }

    struct RecordingSteering(
        pulsebeam_runtime::sync::mpsc::Sender<(std::net::SocketAddr, std::net::SocketAddr, u16)>,
    );

    impl Steering for RecordingSteering {
        fn pin_flow_to_owner(
            &mut self,
            source: std::net::SocketAddr,
            destination: std::net::SocketAddr,
            shard: u16,
        ) {
            self.0.try_send((source, destination, shard)).unwrap();
        }
    }

    #[test]
    fn authenticated_transport_pins_the_exact_flow_and_queues_source_authentication() {
        let source = "203.0.113.7:40000".parse().unwrap();
        let destination = "198.51.100.1:3478".parse().unwrap();
        for (owner, ingress) in [(0, 1), (1, 0)] {
            let mut actor = actor_with_shards(2);
            let owner_shard = ShardId::new(owner);
            let source_shard = ShardId::new(ingress);
            let address = crate::route::NodeTransportAddress::new(
                crate::route::TransportRoute::new(owner_shard, 51),
                7,
            );
            let (pins, mut received) = pulsebeam_runtime::sync::mpsc::channel(2);
            actor.set_steering(Some(Box::new(RecordingSteering(pins))));
            actor.handle_shard_event((
                owner_shard,
                ShardEvent::TransportAuthenticated {
                    source,
                    destination,
                    source_shard,
                    address,
                    shard: owner_shard,
                },
            ));

            assert_eq!(
                received.try_recv().unwrap(),
                (source, destination, u16::try_from(owner).unwrap())
            );
            assert!(received.try_recv().is_err(), "one flow must be pinned once");
            assert!(matches!(
                actor.command_backlog.pop_front(),
                Some((target, ShardCommand::AuthenticateTransport { source: actual_source, address: actual_address }))
                    if target == source_shard && actual_source == source && actual_address == address
            ));
            assert!(actor.command_backlog.is_empty());
        }
    }

    fn connection_id(sequence: u8) -> ConnectionId {
        ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, sequence])
    }

    fn authorization_lease(
        expiry: u64,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> AuthorizationLease {
        AuthorizationLease::from_expiry(
            AuthorizationExpiry::from_unix_seconds(expiry),
            wall_now,
            runtime_now,
        )
        .unwrap()
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn native_delete_fences_former_owners_without_touching_the_replacement() {
        let mut actor = actor();
        let (writer, mut current_rx) = new_shard_update(ShardId::new(0));
        let (former_writer, mut former_rx) = new_shard_update(ShardId::new(1));
        actor.updates = vec![writer, former_writer];
        actor.update_touched = vec![false; 2];
        actor.update_queued = vec![false; 2];
        let room_external_id = RoomExternalId::new("delete").unwrap();
        let room_id = RoomId::from_external(&room_external_id);
        let participant_id = ParticipantId::new();
        let participant_external_id = crate::entity::ParticipantExternalId::new("alice").unwrap();
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection_id(2),
                Some(authorization_lease(
                    100,
                    UNIX_EPOCH,
                    pulsebeam_runtime::time::Instant::now(),
                )),
                ConnectionProfile::Native,
                participant_external_id.clone(),
                UNIX_EPOCH,
            )
            .unwrap();
        let authorization = VerifiedAuthorization {
            project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room_external_id,
            room_id,
            participant_external_id,
            participant_id,
            expiry: AuthorizationExpiry::from_unix_seconds(100),
        };
        let request = || DeleteNative {
            authorization: authorization.clone(),
            connection_id: connection_id(1),
        };
        assert!(matches!(
            actor.begin_native_delete(request(), UNIX_EPOCH + Duration::from_secs(100)),
            Err(ControllerError::AuthorizationExpired)
        ));
        actor.deletion_bytes = PENDING_NATIVE_DELETE_BYTES;
        assert!(matches!(
            actor.begin_native_delete(request(), UNIX_EPOCH),
            Err(ControllerError::ServiceUnavailable)
        ));
        actor.deletion_bytes = 0;
        let old_generation = actor.lifecycle.next_generation();
        let old_address = crate::route::NodeTransportAddress::new(
            crate::route::TransportRoute::new(ShardId::new(1), 8),
            1,
        );
        actor.updates[1].stage(
            old_generation,
            crate::shard_update::ShardUpdateOp::RetireTransport {
                address: old_address,
            },
        );
        actor.mark_update_touched(ShardId::new(1));
        actor.publish_staged();
        let (bytes, acks) = actor.begin_native_delete(request(), UNIX_EPOCH).unwrap();
        assert_eq!(acks.len(), 2);
        let mut current = current_rx.try_recv().unwrap();
        let old = former_rx.try_recv().unwrap();
        assert!(
            matches!(old.lifecycle.as_slice(), [crate::shard_update::ShardUpdateOp::RetireTransport { address }] if *address == old_address)
        );
        let mut former = former_rx.try_recv().unwrap();
        assert!(former.generation > old.generation);
        let (reply, mut response) = oneshot::channel();
        let task = pulsebeam_runtime::task::spawn(complete_native_delete(
            acks,
            NativeDeleteReply::Http(reply),
            actor.deletion_tx.clone(),
            bytes,
        ));
        current.cutoff_ack.take().unwrap().send(()).unwrap();
        pulsebeam_runtime::task::yield_now().await;
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            actor
                .replace_authorization(
                    ReplaceAuthorization {
                        authorization: authorization.clone(),
                        connection_id: connection_id(2),
                        expected_revision: 1,
                    },
                    UNIX_EPOCH,
                    pulsebeam_runtime::time::Instant::now()
                )
                .unwrap(),
            2
        );
        former.cutoff_ack.take().unwrap().send(()).unwrap();
        assert!(response.await.unwrap().is_ok());
        task.await.unwrap();
        let released = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(released);
        assert_eq!(actor.deletion_bytes, 0);
        assert!(actor.current_incarnation(participant_id, connection_id(2)));

        let (_, acks) = actor
            .begin_native_delete(
                DeleteNative {
                    authorization,
                    connection_id: connection_id(2),
                },
                UNIX_EPOCH,
            )
            .unwrap();
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none()
        );
        assert_eq!(acks.len(), 2);
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn native_delete_failure_never_acknowledges_cutoff_or_releases_retained_work() {
        let (done, mut completed) = pulsebeam_runtime::sync::mpsc::channel(1);
        let (ack, pending) = oneshot::channel();
        let (reply, response) = oneshot::channel();
        drop(ack);
        complete_native_delete(
            vec![pending],
            NativeDeleteReply::Http(reply),
            done.clone(),
            7,
        )
        .await;
        assert!(matches!(
            response.await.unwrap(),
            Err(ControllerError::ServiceUnavailable)
        ));
        assert_eq!(completed.recv().await.unwrap().bytes, 7);

        let (ack, pending) = oneshot::channel();
        let (reply, response) = oneshot::channel();
        let task = pulsebeam_runtime::task::spawn(complete_native_delete(
            vec![pending],
            NativeDeleteReply::Http(reply),
            done,
            11,
        ));
        pulsebeam_runtime::task::yield_now().await;
        pulsebeam_runtime::time::advance(NATIVE_DELETE_TIMEOUT).await;
        assert!(matches!(
            response.await.unwrap(),
            Err(ControllerError::ServiceUnavailable)
        ));
        assert!(matches!(
            completed.try_recv(),
            Err(pulsebeam_runtime::sync::mpsc::error::TryRecvError::Empty)
        ));
        ack.send(()).unwrap();
        assert_eq!(completed.recv().await.unwrap().bytes, 11);
        task.await.unwrap();
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn owned_publication_cutoff_waits_for_only_affected_shards_and_fences_replacements() {
        let mut actor = actor();
        actor.core = ControllerCore::with_shards(3, 1, RoomPartitionPlacement::Hashed);
        actor.lifecycle = TrackLifecycle::new(3);
        actor.updates.clear();
        actor.update_touched = vec![false; 3];
        actor.update_queued = vec![false; 3];
        let mut receivers = Vec::new();
        for index in 0..3 {
            let (writer, receiver) = new_shard_update(ShardId::new(index));
            actor.updates.push(writer);
            receivers.push(receiver);
        }
        let room_id = RoomId::from_external(&RoomExternalId::new("owned-cutoff").unwrap());
        let publisher = ParticipantId::new();
        let subscriber = ParticipantId::new();
        for (participant, shard) in [(publisher, 0), (subscriber, 1)] {
            let address = actor
                .core
                .reserve_transport(ShardId::new(shard), pulsebeam_runtime::time::Instant::now());
            actor
                .core
                .registry
                .commit_candidate(
                    participant,
                    room_id,
                    ShardId::new(shard),
                    shard,
                    address,
                    connection_id(1),
                    ConnectionProfile::Native,
                    None,
                )
                .unwrap();
            actor.core.registry.mark_materialized(&participant);
        }
        let make_track = |reference| {
            crate::track::Track::data(
                crate::track::TrackMeta {
                    room_id,
                    shard_id: ShardId::new(0),
                    id: connection_id(1).derive_publication_track_id(
                        crate::entity::TrackKind::Data,
                        "topic",
                        std::num::NonZeroU64::new(reference).unwrap(),
                    ),
                    origin: publisher,
                    connection_id: connection_id(1),
                    label: Some("topic".into()),
                },
                crate::track::Topic::for_test("topic"),
                crate::track::DataLane::Ordered,
                None,
            )
        };
        let first = make_track(1);
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackSubscriptionAdded {
                room_id,
                subscriber,
                connection_id: connection_id(1),
                selector: TrackSelector::track(first.id()),
                selection: SelectionPolicy::All,
            },
        ));
        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(first.clone())));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::OwnedTrackUnpublished {
                meta: first.meta().clone(),
                reference: 1,
            },
        ));
        assert!(actor.deletion_bytes > 0);
        let mut origin = receivers[0].try_recv().unwrap();
        let mut destination = receivers[1].try_recv().unwrap();
        assert!(receivers[2].try_recv().is_err());
        assert_eq!(origin.generation, destination.generation);
        pulsebeam_runtime::task::yield_now().await;
        assert!(actor.deletion_rx.try_recv().is_err());
        origin.cutoff_ack.take().unwrap().send(()).unwrap();
        pulsebeam_runtime::task::yield_now().await;
        assert!(actor.deletion_rx.try_recv().is_err());
        destination.cutoff_ack.take().unwrap().send(()).unwrap();
        let completed = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(completed);
        assert!(
            matches!(actor.command_backlog.pop_front(), Some((shard, ShardCommand::PublicationCutoffResult { participant, connection_id: connection, reference: 1, success: true })) if shard == ShardId::new(0) && participant == publisher && connection == connection_id(1))
        );
        let released = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(released);
        assert_eq!(actor.deletion_bytes, 0);

        let received = make_track(2);
        actor.handle_shard_event((
            ShardId::new(1),
            ShardEvent::OwnedSubscriptionSet {
                connection_id: connection_id(1),
                request: super::super::topology::OwnedSubscription {
                    room_id,
                    subscriber,
                    reference: 2,
                    track: received.id(),
                    enabled: true,
                },
            },
        ));
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackPublished(received.clone()),
        ));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        actor.handle_shard_event((
            ShardId::new(1),
            ShardEvent::OwnedSubscriptionRemoved {
                room_id,
                subscriber,
                connection_id: connection_id(1),
                reference: 2,
            },
        ));
        let mut origin = receivers[0].try_recv().unwrap();
        let mut destination = receivers[1].try_recv().unwrap();
        assert!(receivers[2].try_recv().is_err());
        origin.cutoff_ack.take().unwrap().send(()).unwrap();
        pulsebeam_runtime::task::yield_now().await;
        assert!(actor.deletion_rx.try_recv().is_err());
        destination.cutoff_ack.take().unwrap().send(()).unwrap();
        for _ in 0..2 {
            let completed = actor.deletion_rx.recv().await.unwrap();
            actor.release_deletion(completed);
        }
        assert!(
            matches!(actor.command_backlog.pop_front(), Some((shard, ShardCommand::PublicationCutoffResult { participant, reference: 2, success: true, .. })) if participant == subscriber && shard == ShardId::new(1))
        );
        assert_eq!(actor.deletion_bytes, 0);
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackUnpublished(received.meta().clone()),
        ));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }

        let rejected = make_track(3);
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackPublished(rejected.clone()),
        ));
        actor.deletion_bytes = PENDING_NATIVE_DELETE_BYTES;
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::OwnedTrackUnpublished {
                meta: rejected.meta().clone(),
                reference: 3,
            },
        ));
        assert_eq!(actor.deletion_bytes, PENDING_NATIVE_DELETE_BYTES);
        assert!(matches!(
            actor.command_backlog.pop_front(),
            Some((
                _,
                ShardCommand::PublicationCutoffResult {
                    reference: 3,
                    success: false,
                    ..
                }
            ))
        ));
        assert!(
            actor
                .lifecycle
                .unpublish(
                    publisher,
                    rejected.id(),
                    &actor.core.registry,
                    pulsebeam_runtime::time::Instant::now()
                )
                .is_none()
        );
        actor.deletion_bytes = 0;
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }

        let second = make_track(5);
        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(second.clone())));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::OwnedTrackUnpublished {
                meta: second.meta().clone(),
                reference: 5,
            },
        ));
        let address = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .core
            .registry
            .commit_candidate(
                publisher,
                room_id,
                ShardId::new(0),
                0,
                address,
                connection_id(2),
                ConnectionProfile::Native,
                None,
            )
            .unwrap();
        for receiver in &mut receivers {
            while let Ok(mut update) = receiver.try_recv() {
                if let Some(ack) = update.cutoff_ack.take() {
                    ack.send(()).unwrap();
                }
            }
        }
        for _ in 0..2 {
            let completed = actor.deletion_rx.recv().await.unwrap();
            actor.release_deletion(completed);
        }
        assert!(actor.command_backlog.is_empty());
        assert_eq!(actor.deletion_bytes, 0);
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::OwnedTrackUnpublished {
                meta: second.meta().clone(),
                reference: 5,
            },
        ));
        assert_eq!(actor.deletion_bytes, 0);
        assert!(actor.command_backlog.is_empty());
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn owned_publication_timeout_reports_failure_but_retains_fence_credit() {
        let (ack, pending) = oneshot::channel();
        let (completed, mut results) = pulsebeam_runtime::sync::mpsc::channel(2);
        let participant = ParticipantId::new();
        let task = pulsebeam_runtime::task::spawn(complete_native_delete(
            vec![pending],
            NativeDeleteReply::Publication {
                participant,
                connection_id: connection_id(1),
                reference: 9,
            },
            completed,
            123,
        ));
        pulsebeam_runtime::task::yield_now().await;
        pulsebeam_runtime::time::advance(NATIVE_DELETE_TIMEOUT).await;
        let failure = results.recv().await.unwrap();
        assert_eq!(failure.bytes, 0);
        assert!(
            matches!(failure.publication, Some(PublicationCutoff { participant: current, reference: 9, success: false, .. }) if current == participant)
        );
        assert!(results.try_recv().is_err());
        ack.send(()).unwrap();
        let released = results.recv().await.unwrap();
        assert_eq!(released.bytes, 123);
        assert!(released.publication.is_none());
        task.await.unwrap();
    }

    #[test]
    fn preparation_crossing_authorization_expiry_cannot_commit() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expired-prepare").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(10, UNIX_EPOCH, runtime_now);

        let result = actor.commit_candidate(
            participant_id,
            room_id,
            ShardId::new(0),
            0,
            transport,
            connection_id(1),
            Some(lease),
            ConnectionProfile::Native,
            crate::entity::ParticipantExternalId::new("alice").unwrap(),
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

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn current_connection_closes_at_authorization_deadline_without_grace() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expiry").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(10, UNIX_EPOCH, runtime_now);
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection_id(1),
                Some(lease),
                ConnectionProfile::Native,
                crate::entity::ParticipantExternalId::new("alice").unwrap(),
                UNIX_EPOCH,
            )
            .unwrap();

        pulsebeam_runtime::time::advance(Duration::from_secs(10)).await;
        actor.expire_authorizations(
            pulsebeam_runtime::time::Instant::now(),
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
    fn renewal_rejects_identity_changes_and_shorter_expiry_without_losing_lease() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("renewal-room").unwrap());
        let participant_id = ParticipantId::new();
        let connection = connection_id(1);
        let now = SystemTime::now();
        let unix_now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let old_expiry = unix_now + 60;
        let old_lease = AuthorizationLease::from_expiry(
            AuthorizationExpiry::from_unix_seconds(old_expiry),
            now,
            pulsebeam_runtime::time::Instant::now(),
        )
        .unwrap();
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection,
                Some(old_lease),
                ConnectionProfile::Native,
                crate::entity::ParticipantExternalId::new("alice").unwrap(),
                now,
            )
            .unwrap();
        let verified = VerifiedAuthorization {
            project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room_external_id: RoomExternalId::new("renewal-room").unwrap(),
            room_id,
            participant_external_id: crate::entity::ParticipantExternalId::new("alice").unwrap(),
            participant_id,
            expiry: AuthorizationExpiry::from_unix_seconds(unix_now + 120),
        };
        let mut wrong_identity = verified.clone();
        wrong_identity.participant_external_id =
            crate::entity::ParticipantExternalId::new("mallory").unwrap();
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 1,
            verified: Some(wrong_identity),
        });
        let mut shorter = verified.clone();
        shorter.expiry = AuthorizationExpiry::from_unix_seconds(old_expiry);
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 1,
            verified: Some(shorter),
        });
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization,
            Some(old_lease)
        );
        assert_eq!(actor.authorization_expiries.len(), 1);
        assert!(
            actor.command_backlog.iter().all(|(_, cmd)| matches!(
                cmd,
                ShardCommand::AuthorizationResult { expiry: None, .. }
            ))
        );

        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 1,
            verified: Some(verified.clone()),
        });
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization
                .unwrap()
                .expiry()
                .unix_seconds(),
            unix_now + 120
        );
        assert_eq!(actor.authorization_expiries.len(), 1);
        assert!(matches!(
            &actor.command_backlog.back().unwrap().1,
            ShardCommand::AuthorizationResult {
                expiry: Some(_),
                ..
            }
        ));
        let mut narrowed = verified.clone();
        narrowed.expiry = AuthorizationExpiry::from_unix_seconds(unix_now + 30);
        let request = || ReplaceAuthorization {
            authorization: narrowed.clone(),
            connection_id: connection,
            expected_revision: 2,
        };
        assert_eq!(
            actor
                .replace_authorization(request(), now, pulsebeam_runtime::time::Instant::now())
                .unwrap(),
            3
        );
        let current = actor
            .core
            .registry
            .get_participant(&participant_id)
            .unwrap()
            .authorization
            .unwrap();
        assert_eq!(current.revision(), 3);
        assert_eq!(current.expiry(), narrowed.expiry);
        assert_eq!(
            current
                .refresh_at(now, pulsebeam_runtime::time::Instant::now())
                .unwrap()
                .revision(),
            3
        );
        assert_eq!(actor.authorization_expiries.len(), 1);
        assert!(matches!(&actor.command_backlog.back().unwrap().1,
            ShardCommand::AuthorizationResult { revision: 3, expiry: Some(expiry), .. }
                if *expiry == i64::try_from(unix_now + 30).unwrap()));
        let delivered = actor.command_backlog.len();
        assert!(matches!(
            actor.replace_authorization(request(), now, pulsebeam_runtime::time::Instant::now()),
            Err(ControllerError::AuthorizationPrecondition { revision: 3 })
        ));
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 1,
            verified: Some(verified),
        });
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization,
            Some(current)
        );
        assert!(matches!(
            &actor.command_backlog.back().unwrap().1,
            ShardCommand::AuthorizationResult {
                revision: 1,
                expiry: None,
                ..
            }
        ));
        assert_eq!(actor.command_backlog.len(), delivered + 1);
        let mut unrepresentable = request();
        unrepresentable.expected_revision = 3;
        unrepresentable.authorization.expiry = AuthorizationExpiry::from_unix_seconds(u64::MAX);
        let queued = actor.command_backlog.len();
        assert!(matches!(
            actor.replace_authorization(
                unrepresentable,
                now,
                pulsebeam_runtime::time::Instant::now()
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization,
            Some(current)
        );
        assert_eq!(actor.command_backlog.len(), queued);
        let mut wrong = request();
        wrong.expected_revision = 3;
        wrong.authorization.room_id =
            RoomId::from_external(&RoomExternalId::new("other-room").unwrap());
        assert!(matches!(
            actor.replace_authorization(wrong, now, pulsebeam_runtime::time::Instant::now()),
            Err(ControllerError::AuthorizationRejected)
        ));
        let mut expired = request();
        expired.expected_revision = 3;
        expired.authorization.expiry = AuthorizationExpiry::from_unix_seconds(unix_now);
        assert!(matches!(
            actor.replace_authorization(expired, now, pulsebeam_runtime::time::Instant::now()),
            Err(ControllerError::AuthorizationExpired)
        ));
        let mut exhausted = current;
        exhausted.revision = u64::MAX;
        assert!(matches!(
            exhausted.replacing(
                narrowed.expiry,
                now,
                pulsebeam_runtime::time::Instant::now()
            ),
            Err(ControllerError::ServiceUnavailable)
        ));
        let mut stale = request();
        stale.connection_id = connection_id(2);
        assert!(matches!(
            actor.replace_authorization(stale, now, pulsebeam_runtime::time::Instant::now()),
            Err(ControllerError::GrantGone)
        ));
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection_id(2),
            expected_revision: 1,
            verified: None,
        });
        assert_eq!(actor.command_backlog.len(), delivered + 1);
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization,
            Some(current)
        );
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn renewal_verifies_jwt_off_control_loop_and_commits_only_current_identity() {
        use pulsebeam_core::auth::{
            DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
            KeyRegistryEntry,
        };
        let mut actor = actor();
        actor.set_key_registry(Some(
            KeyRegistry::new(
                *DEVELOPMENT_PROJECT_ID,
                vec![KeyRegistryEntry {
                    key_id: *DEVELOPMENT_API_KEY_ID,
                    public_key: *DEVELOPMENT_API_PUBLIC_KEY,
                }],
            )
            .unwrap(),
        ));
        let room = RoomExternalId::new("renewal-room").unwrap();
        let external = crate::entity::ParticipantExternalId::new("alice").unwrap();
        let room_id = RoomId::derive(&DEVELOPMENT_PROJECT_ID, &room);
        let participant = ParticipantId::derive(&room_id, &external);
        let connection = connection_id(1);
        let now = SystemTime::now();
        let unix_now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let previous = AuthorizationLease::from_expiry(
            AuthorizationExpiry::from_unix_seconds(unix_now + 60),
            now,
            pulsebeam_runtime::time::Instant::now(),
        )
        .unwrap();
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_candidate(
                participant,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection,
                Some(previous),
                ConnectionProfile::Native,
                external.clone(),
                now,
            )
            .unwrap();
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            (room).as_str(),
            (external).as_str(),
            unix_now + 120,
        )
        .unwrap();
        assert!(
            !format!(
                "{:?}",
                ShardEvent::RenewAuthorization {
                    participant,
                    connection_id: connection,
                    token: crate::shard::worker::RenewalToken(token.clone()),
                }
            )
            .contains(&token)
        );
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::RenewAuthorization {
                participant,
                connection_id: connection,
                token: crate::shard::worker::RenewalToken(token),
            },
        ));
        let result =
            pulsebeam_runtime::time::timeout(Duration::from_secs(2), actor.renewal_rx.recv())
                .await
                .unwrap()
                .unwrap();
        actor.finish_renewal(result);
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant)
                .unwrap()
                .authorization
                .unwrap()
                .expiry()
                .unix_seconds(),
            unix_now + 120
        );
        assert_eq!(actor.authorization_expiries.len(), 1);
        assert!(matches!(
            &actor.command_backlog.back().unwrap().1,
            ShardCommand::AuthorizationResult {
                expiry: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn old_expiry_work_cannot_close_replacement() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("expiry-fence").unwrap());
        let participant_id = ParticipantId::new();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let old_id = connection_id(1);
        let current_id = connection_id(2);
        let old_lease = authorization_lease(10, UNIX_EPOCH, runtime_now);
        let old_transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        actor
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                old_transport,
                old_id,
                Some(old_lease),
                ConnectionProfile::Native,
                crate::entity::ParticipantExternalId::new("alice").unwrap(),
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
                0,
                current_transport,
                current_id,
                Some(current_lease),
                ConnectionProfile::Native,
                crate::entity::ParticipantExternalId::new("alice").unwrap(),
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
        let runtime_now = pulsebeam_runtime::time::Instant::now();

        for sequence in 1..=100 {
            let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
            let lease = authorization_lease(i64::MAX.unsigned_abs(), UNIX_EPOCH, runtime_now);
            let previous = actor
                .commit_candidate(
                    participant_id,
                    room_id,
                    ShardId::new(0),
                    0,
                    transport,
                    connection_id(sequence),
                    Some(lease),
                    ConnectionProfile::Native,
                    crate::entity::ParticipantExternalId::new("alice").unwrap(),
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
        let current_lease = current.authorization.unwrap();
        assert_eq!(current_lease.native_expiry(), Ok(i64::MAX));
        assert_eq!(actor.authorization_expiries.len(), 1);

        let unrepresentable = authorization_lease(u64::MAX, UNIX_EPOCH, runtime_now);
        assert_eq!(unrepresentable.expiry().unix_seconds(), u64::MAX);
        assert!(matches!(
            unrepresentable.native_expiry(),
            Err(TokenError::Invalid)
        ));
        let participant_external_id = crate::entity::ParticipantExternalId::new("alice").unwrap();
        assert!(matches!(
            actor.begin_create_participant(
                ParticipantState {
                    manual_sub: true,
                    room_id,
                    participant_id,
                    participant_external_id: participant_external_id.clone(),
                    connection_id: connection_id(101),
                    old_connection_id: None,
                    authorization: Some(unrepresentable),
                    profile: ConnectionProfile::Native,
                    rtp_capabilities: Some(crate::participant::rtp::capabilities()),
                },
                String::new(),
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        assert!(matches!(
            actor.commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection_id(101),
                Some(unrepresentable),
                ConnectionProfile::Native,
                participant_external_id,
                UNIX_EPOCH,
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        actor.core.release_transport(transport, runtime_now);
        let current = actor
            .core
            .registry
            .get_participant(&participant_id)
            .unwrap();
        assert_eq!(current.connection_id, connection_id(100));
        assert_eq!(current.authorization, Some(current_lease));
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
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        let current_transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .core
            .registry
            .commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                old_transport,
                old,
                ConnectionProfile::Native,
                None,
            )
            .unwrap();
        let prepared = |connection_id, transport| PendingMaterialization {
            shard: ShardId::new(0),
            partition_index: 0,
            command: None,
            ack: None,
            participant: participant_id,
            participant_external_id: crate::entity::ParticipantExternalId::new("alice").unwrap(),
            connection_id,
            authorization: None,
            profile: ConnectionProfile::Native,
            transport,
            room_id,
            answer: "native-answer".to_owned(),
        };
        assert_eq!(
            actor
                .complete_materialization(prepared(current, current_transport), UNIX_EPOCH)
                .unwrap(),
            "native-answer"
        );
        assert!(matches!(
            actor.complete_materialization(prepared(old, old_transport), UNIX_EPOCH),
            Err(ControllerError::Superseded)
        ));

        assert_eq!(
            actor.core.registry.commit_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                old_transport,
                old,
                ConnectionProfile::Native,
                None,
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

    #[test]
    fn stale_media_events_cannot_republish_or_unpublish_the_replacement() {
        let mut actor = actor();
        let room_id = RoomId::from_external(&RoomExternalId::new("media-fence").unwrap());
        let participant = ParticipantId::new();
        let stale = connection_id(1);
        let current = connection_id(2);
        for connection_id in [stale, current] {
            let address = actor
                .core
                .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
            actor
                .core
                .registry
                .commit_candidate(
                    participant,
                    room_id,
                    ShardId::new(0),
                    0,
                    address,
                    connection_id,
                    ConnectionProfile::Native,
                    None,
                )
                .unwrap();
        }
        let make_track = |connection_id: ConnectionId| {
            crate::track::Track::data(
                crate::track::TrackMeta {
                    room_id,
                    shard_id: ShardId::new(0),
                    id: connection_id.derive_track_id(crate::entity::TrackKind::Data, "rt/chat"),
                    origin: participant,
                    connection_id,
                    label: Some("rt/chat".to_owned()),
                },
                crate::track::Topic::for_test("chat"),
                crate::track::DataLane::Latest,
                None,
            )
        };
        let stale_track = make_track(stale);
        let stale_meta = stale_track.meta().clone();
        let track_id = current.derive_track_id(crate::entity::TrackKind::Data, "rt/chat");

        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(stale_track)));
        assert!(
            actor
                .lifecycle
                .unpublish(
                    participant,
                    stale_meta.id,
                    &actor.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .is_none(),
            "a retired connection cannot add a publication"
        );

        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackPublished(make_track(current)),
        ));
        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackUnpublished(stale_meta)));
        assert!(
            actor
                .lifecycle
                .unpublish(
                    participant,
                    track_id,
                    &actor.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .is_some(),
            "the replacement's publication must survive stale teardown"
        );
    }
}
