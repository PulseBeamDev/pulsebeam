use std::{
    collections::{BTreeSet, VecDeque},
    future::{Future, poll_fn},
    io,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod catalog;
mod native_service;
pub(crate) mod ownership;
pub mod peer_ownership;

use super::api::creation;
use super::native::handler::StateProtocolHandler;
use super::{MAX_NATIVE_SDP_BYTES, egress::CommandBacklog, placement};
use crate::control::steering::Steering;
#[cfg(test)]
use crate::track::{SelectionPolicy, TrackSelector};
use crate::{
    control::{
        core::{ControllerCore, RoomPartitionPlacement},
        lifecycle::{TrackLifecycle, TrackLifecycleOperation, TrackLifecycleOutcome},
        negotiator::{Negotiator, NegotiatorError},
        ufrag::IceUfrag,
    },
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectRoomKey, RoomId},
    id::ShardId,
    log::{self, ConnectionEnd},
    shard::{
        ShardContext,
        worker::{ShardCommand, ShardEvent, ShardEventMessage},
    },
};
use pulsebeam_core::auth::{
    AuthorizationExpiry, KeyRegistry, MAX_COMPACT_TOKEN_LEN, TokenError, VerifiedAuthorization,
    VerifiedPrincipal, authenticate_participant_token,
};
#[cfg(test)]
use pulsebeam_proto::state::CloseReason;
use pulsebeam_queue as mailbox;
use pulsebeam_rtc::LocalCandidate;
use pulsebeam_runtime::rt::{Migration, MigrationHandle};
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::sync::oneshot;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub(crate) selection: super::sdp::Selection,
    pub room_id: Option<RoomId>,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
    pub authorization: Option<AuthorizationLease>,
    pub profile: SignalingProtocol,
    pub rtp_capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalingProtocol {
    /// The desired/confirmed state protocol bootstrapped by the native endpoint.
    State,
    Whip,
    Whep,
}

pub(super) const fn whep_reference(kind: crate::entity::TrackKind) -> Option<u64> {
    match kind {
        crate::entity::TrackKind::Audio => Some(1),
        crate::entity::TrackKind::Video => Some(2),
        crate::entity::TrackKind::Data => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OwnershipMode {
    Unavailable,
    Standalone,
}

const AUTHORIZATION_TIMER_HORIZON: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationLease {
    authority: Box<VerifiedAuthorization>,
    permissions: super::permissions::ResourcePermissions,
    deadline: pulsebeam_runtime::time::Instant,
    revision: u64,
}

impl AuthorizationLease {
    pub fn from_verified(
        authorization: VerifiedAuthorization,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, TokenError> {
        let permissions = super::permissions::ResourcePermissions::Grants {
            joined_room: authorization.room_id,
            grants: authorization.grants.clone(),
        };
        Self::from_authority(Box::new(authorization), permissions, wall_now, runtime_now)
    }

    fn from_authority(
        authority: Box<VerifiedAuthorization>,
        permissions: super::permissions::ResourcePermissions,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, TokenError> {
        let expiry = authority.expiry;
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
            authority,
            permissions,
            deadline: runtime_now.checked_add(delay).unwrap_or(runtime_now),
            revision: 1,
        })
    }

    fn is_expired_at(&self, wall_now: SystemTime) -> bool {
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        self.expiry().is_expired_at(unix_now.as_secs())
    }

    fn refresh_at(
        &self,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, TokenError> {
        let refreshed = Self::from_authority(
            self.authority.clone(),
            self.permissions.clone(),
            wall_now,
            runtime_now,
        )?;
        Ok(Self {
            revision: self.revision,
            ..refreshed
        })
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn permissions(&self) -> &super::permissions::ResourcePermissions {
        &self.permissions
    }

    fn allows_current_room(&self) -> bool {
        self.authority
            .room_id
            .as_ref()
            .is_none_or(|room| self.permissions.allows_join(room))
    }

    fn replacing(
        &self,
        authorization: VerifiedAuthorization,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
    ) -> Result<Self, ControllerError> {
        if !self.matches_identity(&authorization) {
            return Err(ControllerError::AuthorizationRejected);
        }
        let mut next = Self::from_verified(authorization, wall_now, runtime_now)
            .map_err(|_| ControllerError::AuthorizationExpired)?;
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or(ControllerError::ServiceUnavailable)?;
        Ok(next)
    }

    pub(crate) fn deadline(&self) -> pulsebeam_runtime::time::Instant {
        self.deadline
    }

    pub(crate) fn native_expiry(&self) -> Result<i64, TokenError> {
        i64::try_from(self.expiry().unix_seconds()).map_err(|_| TokenError::Invalid)
    }

    pub(crate) fn expiry(&self) -> AuthorizationExpiry {
        self.authority.expiry
    }

    fn matches_participant(&self, room: Option<RoomId>, participant: ParticipantId) -> bool {
        self.authority.room_id == room && self.authority.participant_id == participant
    }

    fn matches_endpoint(&self, room: ProjectRoomKey, participant: ParticipantId) -> bool {
        self.authority.project_id == room.project
            && self.matches_participant(Some(room.room), participant)
    }

    fn matches_scope(
        &self,
        scope: crate::entity::ConnectionScope,
        participant: ParticipantId,
    ) -> bool {
        self.authority.project_id == scope.project
            && self.authority.room_id == scope.room
            && self.authority.participant_id == participant
    }

    fn matches_identity(&self, authorization: &VerifiedAuthorization) -> bool {
        self.authority.project_id == authorization.project_id
            && self.authority.participant_id == authorization.participant_id
            && self.authority.room_id == authorization.room_id
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
    OpenLink(crate::link::OpenPeer),
    InspectLinks(oneshot::Sender<Result<crate::link::HostHealth, pulsebeam_link::LinkError>>),
    LinkDestinations(crate::link::Destinations),
    CloseLink(pulsebeam_link::NetworkId),
    BindLinkReceive(crate::link::BindReceive),
    BindLinkReverse(crate::link::BindReverse),
    UnbindLinkSend(crate::link::UnbindSend),
    UnbindLinkReceive(pulsebeam_link::BindingId),
    Admission(AdmissionCommand),
    PeerOwnership(peer_ownership::Command),
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    DeleteSdpResource(DeleteSdpResource),
    DeleteNative(
        NativeResourceRequest,
        oneshot::Sender<Result<(), ControllerError>>,
    ),
    ReplaceAuthorization(
        NativeResourceRequest,
        u64,
        oneshot::Sender<Result<u64, ControllerError>>,
    ),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

pub struct AdmissionCommand(AdmissionAction);

enum AdmissionAction {
    Issue(Box<IssueOwnershipCommand>),
    Create(Box<OwnedCreateCommand>),
}

struct IssueOwnershipCommand {
    principal: VerifiedPrincipal,
    room: Option<RoomId>,
    reply: oneshot::Sender<Result<ownership::Proof, ControllerError>>,
}

struct OwnedCreateCommand {
    message: CreateParticipant,
    attempt: OwnershipAttempt,
    reply: OwnedReply,
}

impl std::fmt::Debug for AdmissionCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AdmissionCommand([redacted])")
    }
}

impl AdmissionCommand {
    pub(crate) fn issue(
        principal: VerifiedPrincipal,
        room: Option<RoomId>,
        reply: oneshot::Sender<Result<ownership::Proof, ControllerError>>,
    ) -> Self {
        Self(AdmissionAction::Issue(Box::new(IssueOwnershipCommand {
            principal,
            room,
            reply,
        })))
    }

    pub(crate) fn create(
        message: CreateParticipant,
        proof: ownership::Proof,
        sequence: std::num::NonZeroU64,
        location: axum::http::HeaderValue,
        binding: ownership::RequestBinding,
        successor_receipt: Option<Vec<u8>>,
        reply: OwnedReply,
    ) -> Self {
        Self(AdmissionAction::Create(Box::new(OwnedCreateCommand {
            message,
            attempt: OwnershipAttempt {
                proof,
                sequence,
                binding,
                location,
                successor_receipt,
            },
            reply,
        })))
    }
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
    pub profile: SignalingProtocol,
}

pub struct DeleteSdpResource {
    pub principal: VerifiedPrincipal,
    pub connection_id: ConnectionId,
    pub profile: SignalingProtocol,
}

impl std::fmt::Debug for DeleteSdpResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeleteSdpResource([redacted])")
    }
}

pub struct NativeResourceRequest {
    pub principal: VerifiedPrincipal,
    pub connection_id: ConnectionId,
}

impl std::fmt::Debug for NativeResourceRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeResourceRequest([redacted])")
    }
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
    #[error("joined recovery is no longer available")]
    RecoveryUnavailable,
    #[error("authorization cannot replace this connection grant")]
    AuthorizationRejected,
    #[error("authorization precondition does not match")]
    AuthorizationPrecondition { revision: u64 },
    #[error("requested receive source is unavailable")]
    SourceUnavailable,
    #[error("requested receive source is incompatible with the offer")]
    SourceIncompatible,
    #[error("IO error: {0}")]
    IOError(#[from] io::Error),
    #[error("unknown error: {0}")]
    Unknown(String),
}

const SHARD_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHARD_EVENT_BUDGET: usize = 1_024;
const API_COMMAND_BUDGET: usize = 16;
const MAX_PENDING_MATERIALIZATIONS: usize = API_COMMAND_BUDGET;
const SHARD_COMMAND_EGRESS_BUDGET: usize = 64;
const SHARD_UPDATE_EGRESS_BUDGET: usize = 64;
const PENDING_NATIVE_DELETE_BYTES: usize = 64 * 1024;
const NATIVE_DELETE_TIMEOUT: Duration = Duration::from_secs(30);
type MaterializationCutoffs = arrayvec::ArrayVec<oneshot::Sender<()>, 4>;

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
    ownership: Option<OwnershipCutoff>,
}

#[derive(Debug)]
struct OwnershipCutoff {
    ticket: ownership::Ticket,
    incarnation: ownership::Incarnation,
    success: bool,
}

struct PendingMaterialization {
    native: Option<StateProtocolHandler>,
    shard: ShardId,
    partition_index: Option<usize>,
    command: Option<ShardCommand>,
    ack: Option<oneshot::Receiver<bool>>,
    participant: ParticipantId,
    claim: ownership::ClaimRank,
    authorization: Option<AuthorizationLease>,
    profile: SignalingProtocol,
    transport: crate::route::NodeTransportAddress,
    scope: crate::entity::ConnectionScope,
    answer: String,
    whep_bindings: super::sdp::MediaTracks<(ProjectRoomKey, crate::entity::PublicationKey)>,
    owned: Option<OwnedMaterialization>,
}

struct OwnedMaterialization {
    ticket: ownership::Ticket,
    image: Vec<u8>,
    successor_receipt: Option<Vec<u8>>,
}

enum MaterializationSuccess {
    Answer(String),
    Owned(ownership::ResultBytes),
}

impl PendingMaterialization {
    fn connection_id(&self) -> ConnectionId {
        self.claim.connection()
    }

    fn endpoint(&self) -> Option<EndpointKey> {
        self.scope.endpoint(self.participant)
    }

    fn release_partition(&self, registry: &mut super::registry::RoomRegistry) {
        if let Some(room) = self.scope.joined_room() {
            registry.release_partition(room, self.participant, self.connection_id());
        }
    }

    fn incarnation(&self) -> ownership::Incarnation {
        ownership::Incarnation {
            participant: self.participant,
            connection: self.connection_id(),
            transport: self.transport,
        }
    }
}

struct OwnershipAttempt {
    proof: ownership::Proof,
    sequence: std::num::NonZeroU64,
    binding: ownership::RequestBinding,
    location: axum::http::HeaderValue,
    successor_receipt: Option<Vec<u8>>,
}

enum OwnedPreparation {
    Start(ownership::Ticket, Box<PendingMaterialization>),
    Pending(ownership::Ticket),
    Replay(ownership::Ticket),
}

#[derive(Debug)]
pub(crate) enum OwnedAdmissionError {
    Request(ControllerError),
    Authority(ownership::Error),
    Image(creation::Error),
}

impl From<ControllerError> for OwnedAdmissionError {
    fn from(error: ControllerError) -> Self {
        Self::Request(error)
    }
}

impl From<ownership::Error> for OwnedAdmissionError {
    fn from(error: ownership::Error) -> Self {
        Self::Authority(error)
    }
}

enum RequiredAction {
    Materialize {
        pending: PendingMaterialization,
        reply: MaterializationReply,
    },
}

struct MaterializationWork {
    pending: PendingMaterialization,
    reply: MaterializationReply,
    ack: oneshot::Receiver<bool>,
    ready: Option<bool>,
    awaiting_predecessor_cutoff: bool,
    superseded: bool,
    cutoffs: MaterializationCutoffs,
    started: pulsebeam_runtime::time::Instant,
}

impl MaterializationWork {
    fn matches_native(
        &self,
        authorization: &VerifiedAuthorization,
        connection: ConnectionId,
    ) -> bool {
        let pending = &self.pending;
        pending.endpoint() == endpoint_for_authorization(authorization)
            && pending.connection_id() == connection
            && pending.profile == SignalingProtocol::State
            && pending
                .authorization
                .as_ref()
                .is_some_and(|lease| lease.matches_identity(authorization))
    }
}

fn endpoint_for_authorization(authorization: &VerifiedAuthorization) -> Option<EndpointKey> {
    Some(EndpointKey {
        room: ProjectRoomKey {
            project: authorization.project_id,
            room: authorization.room_id?,
        },
        participant: authorization.participant_id,
    })
}

const MAX_PENDING_RENEWALS: usize = 32;

struct RenewalResult {
    participant: ParticipantId,
    connection_id: ConnectionId,
    expected_revision: u64,
    verified: Option<VerifiedAuthorization>,
}

type OwnedReply = oneshot::Sender<Result<creation::Delivery, super::api::ProblemResponse>>;

struct OwnedWaiter {
    reply: OwnedReply,
    permit: pulsebeam_runtime::sync::OwnedSemaphorePermit,
}

type OwnedReplies = arrayvec::ArrayVec<OwnedWaiter, { ownership::MAX_WAITERS }>;

enum MaterializationReply {
    Create(oneshot::Sender<Result<CreateParticipantReply, ControllerError>>),
    Patch(oneshot::Sender<Result<PatchParticipantReply, ControllerError>>),
    Owned(Box<OwnedReplies>),
}

impl MaterializationReply {
    fn owned(reply: OwnedWaiter) -> Self {
        let mut replies = OwnedReplies::new();
        replies.push(reply);
        Self::Owned(Box::new(replies))
    }
}

struct NativeService {
    handler: StateProtocolHandler,
    deadline: Option<pulsebeam_runtime::time::Instant>,
}

pub struct ControllerActor {
    links: Option<super::link::Links>,
    router: crate::control::router::ShardRouter,
    core: ControllerCore,
    placement: placement::Placement,
    execution: Option<MigrationHandle>,
    migration: Option<(placement::ShardMove, Migration)>,
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
    command_backlog: CommandBacklog,
    authorization_expiries: BTreeSet<AuthorizationExpiryWork>,
    key_registry: Option<KeyRegistry>,
    renewal_tx: pulsebeam_runtime::sync::mpsc::Sender<RenewalResult>,
    renewal_rx: pulsebeam_runtime::sync::mpsc::Receiver<RenewalResult>,
    renewals_inflight: usize,
    deletion_tx: pulsebeam_runtime::sync::mpsc::Sender<NativeDeletionCompletion>,
    deletion_rx: pulsebeam_runtime::sync::mpsc::Receiver<NativeDeletionCompletion>,
    deletion_bytes: usize,
    ownership: ownership::Authority,
    peer_ownership: ownership::observations::Observations,
    provisioned_peers: Vec<peer_ownership::provisioned::Peer>,
    ownership_entropy: pulsebeam_runtime::rand::Rng,
    receipt_trust: Option<ownership::receipt::Authority>,
    delivery_budget: creation::DeliveryBudget,
    steering: Option<Box<dyn Steering>>,
    materializations: VecDeque<MaterializationWork>,
    native_connections: std::collections::HashMap<ConnectionId, NativeService>,
    retiring_native: std::collections::HashMap<ConnectionId, native_service::RetiringNative>,
    source_lookup_index: native_service::SourceLookupIndex,
    data_branches: super::native::DataBranchIds,
    catalog: catalog::CatalogRuntime,
    native_ready_tx: mailbox::Sender<ConnectionId>,
    native_ready_rx: mailbox::Receiver<ConnectionId>,
    native_deadlines: BTreeSet<(pulsebeam_runtime::time::Instant, ConnectionId)>,
}

impl ControllerActor {
    pub(crate) fn set_links(
        &mut self,
        links: pulsebeam_link::LinkControl<crate::shard::worker::ShardFrame>,
        ports: Vec<pulsebeam_link::PortId>,
        origin: pulsebeam_runtime::time::Instant,
    ) {
        self.links = Some(super::link::Links::new(links, ports, origin));
    }

    pub(crate) fn set_link_node(&mut self, node: log::RelayNode) {
        if let Some(links) = &mut self.links {
            links.set_node(node);
        }
    }

    pub(crate) fn set_link_host(
        &mut self,
        host: pulsebeam_queue::Sender<crate::link::host::Command>,
        node: log::RelayNode,
    ) {
        if let Some(links) = &mut self.links {
            links.set_node(node);
            links.set_host(host);
        }
    }

    pub(crate) fn with_placement(
        ownership_entropy: pulsebeam_runtime::rand::Rng,
        shard_contexts: Vec<ShardContext>,
        candidates: Vec<LocalCandidate>,
        room_partition_size: usize,
        placement: RoomPartitionPlacement,
        updates: Vec<crate::shard_update::ShardUpdateWriter>,
    ) -> Self {
        let shard_count = shard_contexts.len();
        debug_assert_eq!(updates.len(), shard_count);
        let (native_ready_tx, native_ready_rx) =
            mailbox::bounded(const { std::num::NonZeroUsize::new(16_384).unwrap() });
        let (renewal_tx, renewal_rx) = pulsebeam_runtime::sync::mpsc::channel(MAX_PENDING_RENEWALS);
        let (deletion_tx, deletion_rx) = pulsebeam_runtime::sync::mpsc::channel(API_COMMAND_BUDGET);
        Self {
            links: None,
            router: crate::control::router::ShardRouter::new(shard_contexts),
            core: ControllerCore::with_shards(shard_count, room_partition_size, placement),
            placement: placement::Placement::default(),
            execution: None,
            migration: None,
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
            command_backlog: CommandBacklog::new(shard_count),
            authorization_expiries: BTreeSet::new(),
            key_registry: None,
            renewal_tx,
            renewal_rx,
            renewals_inflight: 0,
            deletion_tx,
            deletion_rx,
            deletion_bytes: 0,
            ownership: ownership::Authority::default(),
            peer_ownership: peer_ownership::new_observations(),
            provisioned_peers: Vec::new(),
            ownership_entropy,
            receipt_trust: None,
            delivery_budget: creation::DeliveryBudget::new(),
            steering: None,
            materializations: VecDeque::new(),
            native_connections: std::collections::HashMap::new(),
            retiring_native: std::collections::HashMap::new(),
            source_lookup_index: native_service::SourceLookupIndex::default(),
            data_branches: super::native::DataBranchIds::default(),
            catalog: catalog::CatalogRuntime::default(),
            native_ready_tx,
            native_ready_rx,
            native_deadlines: BTreeSet::new(),
        }
    }

    #[cfg(feature = "sim")]
    pub(crate) fn set_sim_ownership_peer(&mut self, peer: ownership::sim::SimOwnershipPeer) {
        self.receipt_trust = Some(ownership::receipt::Authority::Local(peer.trust()));
    }

    pub(crate) fn set_ownership_mode(&mut self, mode: OwnershipMode) {
        self.receipt_trust = match mode {
            OwnershipMode::Unavailable => None,
            OwnershipMode::Standalone => Some(ownership::receipt::Authority::Local(Box::new(
                ownership::receipt::StandaloneTrust::new(&mut self.ownership_entropy),
            ))),
        };
    }

    pub(crate) fn set_execution(&mut self, execution: Option<MigrationHandle>) {
        self.execution = execution;
    }

    fn maintain_placement(&mut self, now: pulsebeam_runtime::time::Instant) {
        if let Some((_, migration)) = self.migration.as_mut() {
            match migration.try_complete() {
                Ok(false) => return,
                result => {
                    if let Some((movement, _)) = self.migration.take() {
                        self.placement.attempted(now);
                        if let Err(error) = result {
                            tracing::warn!(?movement, %error, "shard move failed");
                        }
                    }
                }
            }
        }
        let Some(execution) = self.execution.as_ref() else {
            return;
        };
        let decision = execution.with_snapshot(|snapshot| {
            snapshot.and_then(|snapshot| {
                self.placement.rebalance(
                    pulsebeam_runtime::time::Instant::now(),
                    snapshot,
                    self.router.shard_count(),
                )
            })
        });
        if let Some(movement) = decision {
            match execution.try_move_service(movement.shard, movement.destination) {
                Ok(migration) => self.migration = Some((movement, migration)),
                Err(error) => {
                    self.placement.attempted(now);
                    tracing::warn!(?movement, %error, "shard move unavailable");
                }
            }
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
                .map(|work| work.deadline)
                .into_iter()
                .chain(self.native_deadlines.first().map(|(deadline, _)| *deadline))
                .chain(self.peer_ownership.deadline())
                .chain(
                    self.provisioned_peers
                        .iter()
                        .map(peer_ownership::provisioned::Peer::deadline),
                )
                .min();
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
                    true = self.native_ready_rx.readable() => false,
                    () = poll_fn(|cx| Self::poll_materializations(&mut self.materializations, cx)) => false,
                    () = poll_fn(|cx| self.links.as_mut().map_or(Poll::Pending, |links| links.poll_ready(cx))) => false,
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

            self.tick(&mut command_rx, &mut shard_event_rx, maintenance_due);
        }
    }

    fn tick(
        &mut self,
        command_rx: &mut mailbox::Receiver<ControllerCommand>,
        shard_event_rx: &mut mailbox::Receiver<ShardEventMessage>,
        maintenance_due: bool,
    ) {
        let started = pulsebeam_runtime::time::Instant::now();
        for _ in 0..API_COMMAND_BUDGET {
            let Ok(bytes) = self.deletion_rx.try_recv() else {
                break;
            };
            self.release_deletion(bytes);
        }
        self.expire_authorizations(started, SystemTime::now());
        self.expire_peer_sources(started);
        self.drive_provisioned_peers(started);
        self.advance_materializations();
        if let Some(links) = &mut self.links {
            links.reap();
        }
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
            self.ownership.expire(started, API_COMMAND_BUDGET);
            self.router.poll_loads();
            self.maintain_placement(pulsebeam_runtime::time::Instant::now());
        }

        let mut commands: usize = 0;
        for _ in 0..API_COMMAND_BUDGET {
            let Ok(command) = command_rx.try_recv() else {
                break;
            };
            commands = commands.saturating_add(1);
            if let Some(required) = self.process_command(command) {
                self.start_materialization(required);
            }
        }
        self.record_budget_hit("api_commands", commands, API_COMMAND_BUDGET);

        self.service_native_connections();
        let mut catalog = std::mem::take(&mut self.catalog);
        catalog.service(self);
        self.catalog = catalog;
        self.advance_incarnation_cutoffs();
        let commands_ready = self.flush_command_backlog();
        let updates_ready = self.flush_update_backlogs();
        self.egress_ready = commands_ready || updates_ready;
        metrics::histogram!("control_tick_us").record(started.elapsed().as_micros() as f64);
    }

    fn process_command(&mut self, command: ControllerCommand) -> Option<RequiredAction> {
        match command {
            ControllerCommand::LinkDestinations(command) => {
                let ingress = self.lifecycle.ingress_peer(command.room, command.track);
                let result = if ingress.is_some()
                    || self
                        .lifecycle
                        .network_source(command.room, command.track, &self.core.registry)
                        .is_some()
                {
                    self.links
                        .as_mut()
                        .ok_or(pulsebeam_link::BindError::Closed)
                        .map(|links| links.destinations(command.track, ingress))
                } else {
                    Err(pulsebeam_link::BindError::Closed)
                };
                let _ = command.reply.send(result);
                None
            }
            ControllerCommand::InspectLinks(reply) => {
                if let Some(links) = &self.links {
                    links.inspect(reply);
                } else {
                    let _ = reply.send(Err(pulsebeam_link::LinkError::Closed));
                }
                None
            }
            ControllerCommand::OpenLink(command) => {
                let result = self
                    .links
                    .as_mut()
                    .ok_or(pulsebeam_link::LinkError::Closed)
                    .and_then(|links| links.open_peer(command.peer));
                if let Err(Ok(peer)) = command.reply.send(result) {
                    if let Some(links) = &mut self.links {
                        let _ = links.close_peer(peer);
                    }
                }
                None
            }
            ControllerCommand::BindLinkReverse(command) => {
                let allowed = self.lifecycle.allows_network_reverse(
                    command.room,
                    command.track,
                    command.shard,
                    &self.core.registry,
                ) || self.lifecycle.allows_ingress_reverse(
                    command.room,
                    command.track,
                    command.shard,
                    command.peer,
                );
                let result = allowed
                    .then_some(())
                    .ok_or(pulsebeam_link::BindError::Closed)
                    .and_then(|()| {
                        let (binding, plan) = self
                            .links
                            .as_mut()
                            .ok_or(pulsebeam_link::BindError::Closed)?
                            .bind_reverse(
                                command.shard,
                                command.track,
                                command.peer,
                                command.address,
                            )?;
                        let generation = self.lifecycle.next_generation();
                        self.stage_plans_at(command.shard, generation, vec![plan]);
                        self.publish_staged();
                        Ok(binding)
                    });
                if let Err(Ok(binding)) = command.reply.send(result) {
                    self.retire_network_send(binding);
                }
                None
            }
            ControllerCommand::UnbindLinkSend(command) => {
                self.retire_network_send(command.0);
                None
            }
            ControllerCommand::BindLinkReceive(command) => {
                let result = self
                    .links
                    .as_mut()
                    .ok_or(pulsebeam_link::BindError::Closed)
                    .and_then(|links| links.bind_receive(command.peer, command.destinations));
                if let Err(Ok(route)) = command.reply.send(result) {
                    if let Some(links) = &mut self.links {
                        links.unbind_receive(route.binding);
                    }
                }
                None
            }
            ControllerCommand::UnbindLinkReceive(binding) => {
                if let Some(links) = &mut self.links {
                    links.unbind_receive(binding);
                }
                None
            }
            ControllerCommand::CloseLink(peer) => {
                self.close_link_peer(peer);
                None
            }
            ControllerCommand::Admission(command) => {
                self.process_admission(command, SystemTime::now());
                None
            }
            ControllerCommand::PeerOwnership(command) => {
                self.process_peer_ownership(command);
                None
            }
            ControllerCommand::CreateParticipant(message, reply) => {
                if self.materializations.len() >= MAX_PENDING_MATERIALIZATIONS {
                    let _ = reply.send(Err(ControllerError::ServiceUnavailable));
                    return None;
                }
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
                    .get_incarnation(message.participant_id, message.connection_id)
                    .is_some_and(|meta| {
                        meta.scope.room == Some(message.room_id) && meta.profile == message.profile
                    })
                {
                    self.remove_incarnation(message.participant_id, message.connection_id);
                }
                None
            }
            ControllerCommand::DeleteSdpResource(message) => {
                let wall_now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if message.profile != SignalingProtocol::State
                    && !message.principal.expiry().is_expired_at(wall_now)
                {
                    let authorization = self
                        .sdp_resource_lease(message.connection_id, message.profile)
                        .and_then(|accepted| {
                            message
                                .principal
                                .authorize_existing(&accepted.authority)
                                .ok()
                        });
                    if let Some(authorization) = authorization {
                        for work in &mut self.materializations {
                            if work.pending.connection_id() == message.connection_id
                                && work.pending.endpoint()
                                    == endpoint_for_authorization(&authorization)
                                && work.pending.profile == message.profile
                            {
                                work.superseded = true;
                            }
                        }
                        self.remove_incarnation(
                            authorization.participant_id,
                            message.connection_id,
                        );
                    }
                }
                None
            }
            ControllerCommand::DeleteNative(message, reply) => {
                let wall_now = SystemTime::now();
                let connection_id = message.connection_id;
                let result =
                    self.authorize_native_resource(message, wall_now)
                        .and_then(|authorization| {
                            self.begin_native_delete(
                                DeleteNative {
                                    authorization,
                                    connection_id,
                                },
                                wall_now,
                            )
                        });
                match result {
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
            ControllerCommand::ReplaceAuthorization(message, expected_revision, reply) => {
                let wall_now = SystemTime::now();
                let connection_id = message.connection_id;
                let result =
                    self.authorize_native_resource(message, wall_now)
                        .and_then(|authorization| {
                            self.replace_authorization(
                                ReplaceAuthorization {
                                    authorization,
                                    connection_id,
                                    expected_revision,
                                },
                                wall_now,
                                pulsebeam_runtime::time::Instant::now(),
                            )
                        });
                let _ = reply.send(result);
                None
            }
            ControllerCommand::PatchParticipant(message, reply) => {
                if self.materializations.len() >= MAX_PENDING_MATERIALIZATIONS {
                    let _ = reply.send(Err(ControllerError::ServiceUnavailable));
                    return None;
                }
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

    fn process_admission(&mut self, command: AdmissionCommand, wall_now: SystemTime) {
        match command.0 {
            AdmissionAction::Issue(command) => {
                let IssueOwnershipCommand {
                    principal,
                    room,
                    reply,
                } = *command;
                if !reply.is_closed() {
                    let result = self.issue_ownership(&principal, room, wall_now);
                    let _ = reply.send(result);
                }
            }
            AdmissionAction::Create(command) => {
                let OwnedCreateCommand {
                    message,
                    attempt,
                    reply,
                } = *command;
                if !reply.is_closed()
                    && let Err((reply, error)) =
                        self.submit_owned_create(message, attempt, reply, wall_now)
                {
                    let _ = reply.send(Err(error.into()));
                }
            }
        }
    }

    fn issue_ownership(
        &mut self,
        principal: &VerifiedPrincipal,
        room: Option<RoomId>,
        wall_now: SystemTime,
    ) -> Result<ownership::Proof, ControllerError> {
        if !self
            .receipt_trust
            .as_ref()
            .is_some_and(ownership::receipt::Authority::ready)
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let unix_now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        if principal.expiry().is_expired_at(unix_now.as_secs()) {
            return Err(ControllerError::AuthorizationExpired);
        }
        let key = ownership::PrincipalKey::from_principal(principal, room);
        let now = pulsebeam_runtime::time::Instant::now();
        if room.is_none() {
            self.ownership
                .issue_roomless(key, now, &mut self.ownership_entropy)
        } else {
            self.ownership.issue(key, now, &mut self.ownership_entropy)
        }
        .map_err(|_| ControllerError::ServiceUnavailable)
    }

    fn submit_owned_create(
        &mut self,
        message: CreateParticipant,
        attempt: OwnershipAttempt,
        reply: OwnedReply,
        wall_now: SystemTime,
    ) -> Result<ownership::Ticket, (OwnedReply, OwnedAdmissionError)> {
        self.prune_owned_waiters();
        let Some(permit) = self.delivery_budget.reserve() else {
            return Err((reply, ControllerError::ServiceUnavailable.into()));
        };
        let waiter = OwnedWaiter { reply, permit };
        let prepared = match self.begin_owned_create(message, attempt, wall_now) {
            Ok(prepared) => prepared,
            Err(error) => return Err((waiter.reply, error)),
        };
        match prepared {
            OwnedPreparation::Start(ticket, pending) => {
                self.start_materialization(RequiredAction::Materialize {
                    pending: *pending,
                    reply: MaterializationReply::owned(waiter),
                });
                Ok(ticket)
            }
            OwnedPreparation::Pending(ticket) => {
                self.attach_owned_waiter(&ticket, waiter)
                    .map_err(|(waiter, error)| (waiter.reply, error.into()))?;
                Ok(ticket)
            }
            OwnedPreparation::Replay(ticket) => {
                let bytes = match self
                    .ownership
                    .result(&ticket, pulsebeam_runtime::time::Instant::now())
                {
                    Ok(bytes) => bytes,
                    Err(error) => return Err((waiter.reply, error.into())),
                };
                let _ = waiter
                    .reply
                    .send(Ok(creation::Delivery::new(bytes, waiter.permit)));
                Ok(ticket)
            }
        }
    }

    fn begin_owned_create(
        &mut self,
        message: CreateParticipant,
        attempt: OwnershipAttempt,
        wall_now: SystemTime,
    ) -> Result<OwnedPreparation, OwnedAdmissionError> {
        if !self
            .receipt_trust
            .as_ref()
            .is_some_and(ownership::receipt::Authority::ready)
        {
            return Err(ControllerError::ServiceUnavailable.into());
        }
        let state = &message.state;
        let lease = state
            .authorization
            .as_ref()
            .ok_or(ControllerError::AuthorizationRejected)?;
        if !lease.matches_participant(state.room_id, state.participant_id)
            || !lease.allows_current_room()
        {
            return Err(ControllerError::AuthorizationRejected.into());
        }
        if lease.is_expired_at(wall_now) {
            return Err(ControllerError::AuthorizationExpired.into());
        }
        let principal = ownership::PrincipalKey::from_verified(&lease.authority);
        let principal = if state.room_id.is_none() {
            principal.roomless(attempt.proof)
        } else {
            principal
        };
        let now = pulsebeam_runtime::time::Instant::now();
        let OwnershipAttempt {
            proof,
            sequence,
            binding,
            location,
            successor_receipt,
        } = attempt;
        let preview = self
            .ownership
            .preview(&principal, proof, sequence, binding, now)?;
        if !matches!(preview, ownership::Admission::Start(_)) {
            return match self
                .ownership
                .begin(&principal, proof, sequence, binding, now)?
            {
                ownership::Admission::Pending(ticket) => Ok(OwnedPreparation::Pending(ticket)),
                ownership::Admission::Replay(ticket) => Ok(OwnedPreparation::Replay(ticket)),
                ownership::Admission::Start(_) => {
                    pulsebeam_runtime::fatal!("ownership preview changed without a transition")
                }
            };
        }
        let endpoint = lease.authority.room_id.map(|room| EndpointKey {
            room: ProjectRoomKey {
                project: lease.authority.project_id,
                room,
            },
            participant: lease.authority.participant_id,
        });
        let claim = if let Some(proof) = successor_receipt.as_deref() {
            self.successor_rank(proof, endpoint, state.connection_id, wall_now)?
        } else {
            ownership::ClaimRank::root(state.connection_id)
        };
        if endpoint.is_some_and(|endpoint| !self.candidate_is_newer(endpoint, claim)) {
            return Err(ControllerError::Superseded.into());
        }
        if endpoint.is_some_and(|endpoint| {
            self.materializations.iter().any(|work| {
                work.awaiting_predecessor_cutoff && work.pending.endpoint() == Some(endpoint)
            })
        }) {
            return Err(ownership::Error::QuiescencePending.into());
        }
        if self.materializations.len() >= MAX_PENDING_MATERIALIZATIONS
            || self.materializations.try_reserve(1).is_err()
        {
            return Err(ControllerError::ServiceUnavailable.into());
        }
        self.ownership.reserve_binding()?;
        let capabilities = message.state.rtp_capabilities.clone();
        let mut pending = self.begin_create_participant(message.state, message.offer)?;
        pending.claim = claim;
        let response = if pending.profile == SignalingProtocol::State {
            let Some(capabilities) = capabilities.as_ref() else {
                pulsebeam_runtime::fatal!("prepared native creation lost protocol capabilities");
            };
            let Some(lease) = pending.authorization.as_ref() else {
                pulsebeam_runtime::fatal!("owned creation lost verified authorization");
            };
            creation::CreationResponse::native(
                &location,
                &lease.authority,
                pending.connection_id(),
                capabilities,
                &pending.answer,
            )
        } else {
            creation::CreationResponse::sdp(&location, &pending.answer)
        };
        let prepared = match response.validate() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.discard_materialization(pending, MaterializationCutoffs::new());
                return Err(OwnedAdmissionError::Image(error));
            }
        };
        let claim_now = pulsebeam_runtime::time::Instant::now();
        let claim_wall = wall_now.checked_add(claim_now.saturating_duration_since(now));
        if claim_wall.is_none_or(|wall| {
            pending
                .authorization
                .as_ref()
                .is_none_or(|lease| lease.is_expired_at(wall))
        }) {
            self.discard_materialization(pending, MaterializationCutoffs::new());
            return Err(ControllerError::AuthorizationExpired.into());
        }
        let ticket = match self
            .ownership
            .begin(&principal, proof, sequence, binding, claim_now)
        {
            Ok(ownership::Admission::Start(ticket)) => ticket,
            Err(error) => {
                self.discard_materialization(pending, MaterializationCutoffs::new());
                return Err(error.into());
            }
            Ok(_) => {
                pulsebeam_runtime::fatal!("prepared ownership claim changed without a transition")
            }
        };
        let image = prepared.encode();
        let live = self.ownership.live_predecessor(&ticket);
        self.fence_predecessors(&ticket, claim, live);
        self.advance_materializations();
        let Some(lease) = pending.authorization.as_ref() else {
            pulsebeam_runtime::fatal!("owned admission lost verified authorization");
        };
        if let Err(error) = self
            .ownership
            .bind(&ticket, &lease.authority, pending.incarnation())
        {
            drop(image);
            self.discard_materialization(pending, MaterializationCutoffs::new());
            self.ownership.retire(&ticket, claim_now);
            return Err(error.into());
        }
        pending.owned = Some(OwnedMaterialization {
            ticket: ticket.clone(),
            image,
            successor_receipt,
        });
        Ok(OwnedPreparation::Start(ticket, Box::new(pending)))
    }

    fn prune_owned_waiters(&mut self) {
        for work in &mut self.materializations {
            let Some(owned) = work.pending.owned.as_ref() else {
                continue;
            };
            let MaterializationReply::Owned(replies) = &mut work.reply else {
                pulsebeam_runtime::fatal!("owned materialization must retain charged reply sinks");
            };
            replies.retain(|waiter| {
                if waiter.reply.is_closed() {
                    self.ownership.detach_waiter(&owned.ticket);
                    false
                } else {
                    true
                }
            });
        }
    }

    fn attach_owned_waiter(
        &mut self,
        ticket: &ownership::Ticket,
        reply: OwnedWaiter,
    ) -> Result<(), (OwnedWaiter, ownership::Error)> {
        let work = self.materializations.iter_mut().find(|work| {
            work.pending
                .owned
                .as_ref()
                .is_some_and(|owned| &owned.ticket == ticket)
        });
        let Some(work) = work else {
            self.ownership.detach_waiter(ticket);
            return Err((reply, ownership::Error::ReplayUnavailable));
        };
        let MaterializationReply::Owned(replies) = &mut work.reply else {
            pulsebeam_runtime::fatal!("owned materialization must retain charged reply sinks");
        };
        if replies.is_full() {
            self.ownership.detach_waiter(ticket);
            return Err((reply, ownership::Error::Capacity));
        }
        replies.push(reply);
        Ok(())
    }

    fn successor_rank(
        &self,
        proof: &[u8],
        endpoint: Option<EndpointKey>,
        connection: ConnectionId,
        wall_now: SystemTime,
    ) -> Result<ownership::ClaimRank, OwnedAdmissionError> {
        use ownership::receipt::Error;
        let endpoint = endpoint.ok_or(ownership::Error::InvalidProof)?;
        let trust = self
            .receipt_trust
            .as_deref()
            .ok_or(ControllerError::ServiceUnavailable)?;
        let now = wall_now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        ownership::receipt::validate_successor(trust, proof, endpoint, connection, now).map_err(
            |error| {
                match error {
                    Error::Invalid | Error::Scope => ownership::Error::InvalidProof,
                    Error::Unfresh => return ControllerError::ServiceUnavailable.into(),
                    Error::Expired | Error::NoAuthority => {
                        return ControllerError::RecoveryUnavailable.into();
                    }
                    Error::Retired => ownership::Error::Replaced,
                }
                .into()
            },
        )
    }

    fn validate_pending_successor(
        &self,
        pending: &PendingMaterialization,
        owned: Option<&OwnedMaterialization>,
        wall_now: SystemTime,
    ) -> Result<(), ControllerError> {
        if owned.is_some()
            && !self
                .receipt_trust
                .as_ref()
                .is_some_and(ownership::receipt::Authority::ready)
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let Some(proof) = owned.and_then(|owned| owned.successor_receipt.as_deref()) else {
            return Ok(());
        };
        match self.successor_rank(proof, pending.endpoint(), pending.connection_id(), wall_now) {
            Ok(rank) if rank == pending.claim => Ok(()),
            Err(OwnedAdmissionError::Authority(ownership::Error::Replaced)) => {
                Err(ControllerError::Superseded)
            }
            Err(OwnedAdmissionError::Request(error)) => Err(error),
            _ => Err(ControllerError::RecoveryUnavailable),
        }
    }

    fn retire_defeated_lineage(
        &mut self,
        previous: &crate::control::registry::ParticipantMeta,
        winner: ownership::ClaimRank,
    ) {
        if let (Some(endpoint), Some(previous), Some(trust)) = (
            previous.endpoint(),
            previous.claim,
            self.receipt_trust.as_deref_mut(),
        ) && previous.lineage() != winner.lineage()
        {
            trust.retire(endpoint, previous);
        }
    }

    fn fence_predecessors(
        &mut self,
        ticket: &ownership::Ticket,
        winner: ownership::ClaimRank,
        keep: Option<ownership::Incarnation>,
    ) {
        for incarnation in self.ownership.predecessors(ticket) {
            if keep == Some(incarnation) {
                continue;
            }
            for work in &mut self.materializations {
                if work.pending.incarnation() == incarnation {
                    work.superseded = true;
                }
            }
            let previous = self
                .core
                .registry
                .get_incarnation(incarnation.participant, incarnation.connection)
                .filter(|meta| meta.transport == Some(incarnation.transport))
                .cloned();
            if let Some(meta) = previous {
                self.retire_defeated_lineage(&meta, winner);
                self.terminate_incarnation(
                    incarnation.participant,
                    meta,
                    true,
                    ConnectionEnd::Replaced(Some(winner.connection())),
                );
            }
            self.ownership.retry_retirement(incarnation);
        }
    }

    fn submit_materialization(
        &mut self,
        pending: &PendingMaterialization,
    ) -> Result<bool, ControllerError> {
        if self.ownership.has_incarnation(pending.incarnation())
            && pending
                .authorization
                .as_ref()
                .is_none_or(|lease| lease.is_expired_at(SystemTime::now()))
        {
            return Err(ControllerError::AuthorizationExpired);
        }
        if pending
            .endpoint()
            .is_some_and(|endpoint| !self.candidate_is_newer(endpoint, pending.claim))
        {
            return Err(ControllerError::Superseded);
        }
        match self.ownership.submit(pending.incarnation()) {
            Ok(()) => Ok(true),
            Err(ownership::Error::QuiescencePending) => Ok(false),
            Err(ownership::Error::Replaced) => Err(ControllerError::Superseded),
            Err(_) => Err(ControllerError::ServiceUnavailable),
        }
    }

    fn discard_materialization(
        &mut self,
        mut pending: PendingMaterialization,
        cutoffs: MaterializationCutoffs,
    ) {
        let Some(command) = pending.command.take() else {
            pulsebeam_runtime::fatal!("cannot locally discard an issued materialization");
        };
        drop(command);
        drop(pending.owned.take());
        self.ownership.cancel_unsubmitted(
            pending.incarnation(),
            pulsebeam_runtime::time::Instant::now(),
        );
        pending.release_partition(&mut self.core.registry);
        self.core
            .release_transport(pending.transport, pulsebeam_runtime::time::Instant::now());
        for cutoff in cutoffs {
            let _ = cutoff.send(());
        }
    }

    fn issue_materialization(&mut self, pending: &mut PendingMaterialization) {
        let Some(command) = pending.command.take() else {
            pulsebeam_runtime::fatal!("materialization must retain its shard command");
        };
        self.command_backlog.push_back((pending.shard, command));
    }

    fn start_materialization(&mut self, action: RequiredAction) {
        let RequiredAction::Materialize { mut pending, reply } = action;
        let submitted = match self.submit_materialization(&pending) {
            Ok(submitted) => submitted,
            Err(error) => {
                self.discard_materialization(pending, MaterializationCutoffs::new());
                Self::reply_materialization(reply, Err(error));
                return;
            }
        };
        let Some(ack) = pending.ack.take() else {
            pulsebeam_runtime::fatal!("materialization must retain its acknowledgement receiver");
        };
        if submitted {
            self.issue_materialization(&mut pending);
        }
        self.materializations.push_back(MaterializationWork {
            pending,
            reply,
            ack,
            ready: None,
            awaiting_predecessor_cutoff: false,
            superseded: false,
            cutoffs: MaterializationCutoffs::new(),
            started: pulsebeam_runtime::time::Instant::now(),
        });
    }

    fn poll_materializations(
        work: &mut VecDeque<MaterializationWork>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        for pending in work {
            if pending.awaiting_predecessor_cutoff {
                continue;
            }
            if pending.ready.is_some() {
                return Poll::Ready(());
            }
            if let Poll::Ready(result) = Pin::new(&mut pending.ack).poll(cx) {
                pending.ready = Some(result.unwrap_or(false));
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }

    fn advance_materializations(&mut self) {
        for _ in 0..self.materializations.len().min(API_COMMAND_BUDGET) {
            let Some(mut work) = self.materializations.pop_front() else {
                break;
            };
            if work.awaiting_predecessor_cutoff {
                if work.superseded
                    || !work.cutoffs.is_empty()
                    || work
                        .pending
                        .owned
                        .as_ref()
                        .is_some_and(|owned| self.ownership.predecessors_retired(&owned.ticket))
                {
                    self.finish_materialization(work, true);
                } else {
                    self.materializations.push_back(work);
                }
                continue;
            }
            if work.pending.command.is_some() {
                let submitted = if work.superseded {
                    Err(ControllerError::Superseded)
                } else {
                    self.submit_materialization(&work.pending)
                };
                match submitted {
                    Ok(false) => {
                        self.materializations.push_back(work);
                        continue;
                    }
                    Err(error) => {
                        self.discard_materialization(work.pending, work.cutoffs);
                        Self::reply_materialization(work.reply, Err(error));
                        continue;
                    }
                    Ok(true) => self.issue_materialization(&mut work.pending),
                }
            }
            let ready = work.ready.or_else(|| match work.ack.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Closed) => Some(false),
                Err(oneshot::error::TryRecvError::Empty) => None,
            });
            if let Some(materialized) = ready {
                if materialized && self.begin_predecessor_cutoff(&mut work) {
                    self.materializations.push_back(work);
                } else {
                    self.finish_materialization(work, materialized);
                }
            } else {
                self.materializations.push_back(work);
            }
        }
    }

    fn begin_predecessor_cutoff(&mut self, work: &mut MaterializationWork) -> bool {
        let pending = &work.pending;
        let (Some(owned), Some(endpoint), Some(slot), Some(lease)) = (
            pending.owned.as_ref(),
            pending.endpoint(),
            pending.partition_index,
            pending.authorization.as_ref(),
        ) else {
            return false;
        };
        let wall_now = SystemTime::now();
        if work.superseded
            || !work.cutoffs.is_empty()
            || !self.ownership.selected(&owned.ticket)
            || self.ownership.live_predecessor(&owned.ticket).is_none()
            || self
                .validate_pending_successor(pending, Some(owned), wall_now)
                .is_err()
            || lease.is_expired_at(wall_now)
            || !lease.matches_endpoint(endpoint.room, endpoint.participant)
            || !lease.allows_current_room()
            || (pending.profile == SignalingProtocol::State && lease.native_expiry().is_err())
            || !self.candidate_is_newer(endpoint, pending.claim)
            || !self.core.registry.partition_reserved(
                endpoint.room,
                endpoint.participant,
                pending.connection_id(),
                pending.shard,
                slot,
            )
        {
            return false;
        }
        self.fence_predecessors(&owned.ticket, pending.claim, None);
        work.awaiting_predecessor_cutoff = true;
        true
    }

    fn finish_materialization(&mut self, work: MaterializationWork, materialized: bool) {
        let MaterializationWork {
            mut pending,
            reply,
            started,
            superseded,
            cutoffs,
            ..
        } = work;
        let incarnation = pending.incarnation();
        let (shard, transport) = (pending.shard, pending.transport);
        let owned = pending.owned.take();
        let wall_now = SystemTime::now();
        let validation = if superseded
            || !cutoffs.is_empty()
            || pending
                .endpoint()
                .is_some_and(|endpoint| !self.candidate_is_newer(endpoint, pending.claim))
        {
            Err(ControllerError::Superseded)
        } else {
            self.validate_pending_successor(&pending, owned.as_ref(), wall_now)
        };
        metrics::histogram!("control_materialization_wait_us")
            .record(started.elapsed().as_micros() as f64);
        let result = match (validation, materialized) {
            (Ok(()), true) => self.complete_materialization(pending, wall_now),
            (validation, _) => {
                pending.release_partition(&mut self.core.registry);
                validation.and_then(|()| {
                    if let Some(owned) = &owned {
                        self.ownership.retry_failed_materialization(&owned.ticket);
                    }
                    Err(ControllerError::ServiceUnavailable)
                })
            }
        };
        let committed = result.is_ok();
        let result = result.and_then(|answer| {
            let Some(OwnedMaterialization { ticket, image, .. }) = owned else {
                return Ok(MaterializationSuccess::Answer(answer));
            };
            let now = pulsebeam_runtime::time::Instant::now();
            self.ownership
                .commit(&ticket, image, now)
                .map_err(|_| ControllerError::ServiceUnavailable)?;
            let bytes = self.ownership.result(&ticket, now).unwrap_or_else(|_| {
                pulsebeam_runtime::fatal!("committed ownership image must remain available");
            });
            Ok(MaterializationSuccess::Owned(bytes))
        });
        if result.is_err() {
            if committed {
                let meta = self
                    .core
                    .registry
                    .get_incarnation(incarnation.participant, incarnation.connection)
                    .cloned()
                    .unwrap_or_else(|| {
                        pulsebeam_runtime::fatal!("committed materialization lost its incarnation");
                    });
                self.terminate_incarnation(
                    incarnation.participant,
                    meta,
                    true,
                    ConnectionEnd::Closed,
                );
            } else if materialized
                || !cutoffs.is_empty()
                || self.ownership.has_incarnation(incarnation)
            {
                self.cleanup_materialization(incarnation, shard, cutoffs);
            } else {
                self.core
                    .release_transport(transport, pulsebeam_runtime::time::Instant::now());
            }
        }
        Self::reply_materialization(reply, result);
    }

    fn cleanup_materialization(
        &mut self,
        incarnation: ownership::Incarnation,
        shard: ShardId,
        cutoffs: MaterializationCutoffs,
    ) {
        let ownership::Incarnation {
            connection,
            transport,
            ..
        } = incarnation;
        self.publish_staged();
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            pulsebeam_runtime::fatal!("materialization cleanup has no owning shard");
        };
        update.stage(
            generation,
            crate::shard_update::ShardUpdateOp::RemoveParticipant {
                connection,
                address: transport,
            },
        );
        self.mark_update_touched(shard);
        self.publish_staged();
        self.core
            .release_transport(transport, pulsebeam_runtime::time::Instant::now());
        self.ownership.queue_retirement(incarnation);
        if cutoffs.is_empty() {
            return;
        }
        // The command is finished. Fence its final cleanup, not an earlier empty shard.
        let generation = self.lifecycle.next_generation();
        let Some(update) = self.updates.get_mut(shard.index()) else {
            pulsebeam_runtime::fatal!("materialization cutoff has no owning shard");
        };
        let ack = update.stage_cutoff(generation);
        self.mark_update_touched(shard);
        self.publish_staged();
        pulsebeam_runtime::task::spawn(async move {
            if ack.await.is_ok() {
                for cutoff in cutoffs {
                    let _ = cutoff.send(());
                }
            }
        });
    }

    fn reply_materialization(
        reply: MaterializationReply,
        result: Result<MaterializationSuccess, ControllerError>,
    ) {
        match reply {
            MaterializationReply::Create(reply) => {
                let _ = reply.send(result.map(|success| {
                    let MaterializationSuccess::Answer(answer) = success else {
                        pulsebeam_runtime::fatal!(
                            "owned creation cannot return an uncharged answer"
                        );
                    };
                    CreateParticipantReply { answer }
                }));
            }
            MaterializationReply::Patch(reply) => {
                let _ = reply.send(result.map(|success| {
                    let MaterializationSuccess::Answer(answer) = success else {
                        pulsebeam_runtime::fatal!("owned creation cannot return a patch answer");
                    };
                    PatchParticipantReply { answer }
                }));
            }
            MaterializationReply::Owned(replies) => {
                let result = result
                    .map(|success| {
                        let MaterializationSuccess::Owned(bytes) = success else {
                            pulsebeam_runtime::fatal!("owned creation must return charged bytes");
                        };
                        bytes
                    })
                    .map_err(|error| super::api::ApiError::JoinError(error).into_problem());
                for waiter in *replies {
                    let result = result
                        .clone()
                        .map(|image| creation::Delivery::new(image, waiter.permit));
                    let _ = waiter.reply.send(result);
                }
            }
        }
    }

    fn current_incarnation(&self, participant: ParticipantId, connection_id: ConnectionId) -> bool {
        self.core
            .registry
            .get_incarnation(participant, connection_id)
            .is_some()
    }

    fn handle_shard_event(&mut self, (shard, event): ShardEventMessage) {
        match event {
            ShardEvent::NetworkMediaRepair(interest) => {
                if interest.parent.destination.shard() == shard
                    && let Some(peer) = self
                        .provisioned_peers
                        .iter_mut()
                        .find(|peer| peer.matches_network(interest.parent.ingress.association))
                {
                    peer.request_media_repair(*interest, pulsebeam_runtime::time::Instant::now());
                }
            }
            ShardEvent::NetworkDataRepair(event) => {
                use crate::route::fabric::network_repair::InterestEvent;
                let network = match &*event {
                    InterestEvent::Request(interest) | InterestEvent::Cancel(interest) => {
                        interest.parent.ingress.association
                    }
                };
                let Some(peer) = self
                    .provisioned_peers
                    .iter_mut()
                    .find(|peer| peer.matches_network(network))
                else {
                    return;
                };
                let now = pulsebeam_runtime::time::Instant::now();
                match *event {
                    InterestEvent::Request(interest)
                        if interest.parent.destination.shard() == shard
                            && self.links.as_ref().is_some_and(|links| {
                                links.allows_wan_repair_interest(interest, now)
                            }) =>
                    {
                        peer.request_repair(interest, now);
                    }
                    InterestEvent::Cancel(interest)
                        if interest.parent.destination.shard() == shard =>
                    {
                        peer.cancel_repair(interest, now);
                    }
                    _ => {}
                }
            }
            ShardEvent::PublisherDataExpectation(expectation) => {
                let now = pulsebeam_runtime::time::Instant::now();
                if let Some(peer) = self
                    .provisioned_peers
                    .iter()
                    .find(|peer| peer.matches_network(expectation.route.next.peer))
                    && peer.allows_wan_data_expectation(&expectation, now)
                {
                    peer.send_publisher_data_expectation(expectation, now);
                }
            }
            ShardEvent::LocalFabricRtpSourceReady {
                meta,
                encoding,
                incarnation,
                emission,
            } => {
                if self
                    .core
                    .registry
                    .get_incarnation(meta.origin, meta.connection_id)
                    .is_some_and(|owner| owner.shard_id == shard)
                    && let Some(outcome) = self.lifecycle.set_local_rtp_source(
                        &meta,
                        encoding,
                        incarnation,
                        emission,
                        &self.core.registry,
                        pulsebeam_runtime::time::Instant::now(),
                    )
                {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::ShardDataReceiverReady(ready) => {
                if ready.reference == 0
                    || ready.binding.destination.shard() != shard
                    || ready.binding.origin == shard
                    || !self
                        .core
                        .registry
                        .get_connection(ready.receiver.connection)
                        .is_some_and(|(_, owner)| owner.shard_id == shard && owner.materialized)
                    || !self
                        .native_connections
                        .contains_key(&ready.receiver.connection)
                {
                    return;
                }
                let previous = self.lifecycle.record_ordered_receiver(ready);
                if previous == Some(ready) {
                    return;
                }
                let generation = self.lifecycle.next_generation();
                if let Some(previous) = previous {
                    self.stage_ordered_receiver(generation, previous, false);
                }
                self.stage_ordered_receiver(generation, ready, true);
                self.publish_staged();
            }
            ShardEvent::TrackPublished(track) => {
                if !self.current_incarnation(track.meta().origin, track.meta().connection_id) {
                    return;
                }
                if let Some(outcome) = self.lifecycle.publish(
                    track.clone(),
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                ) {
                    self.observe_native_source(track.meta(), true);
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
                    EndpointKey {
                        room: meta.room,
                        participant: meta.origin,
                    },
                    meta.id,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                ) {
                    self.observe_native_source(&meta, false);
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
                        .get_incarnation(request.subscriber, connection_id)
                        .is_some_and(|_| self.native_connections.contains_key(&connection_id))
                {
                    return;
                }
                if !request.enabled
                    && let Some(ready) = self
                        .lifecycle
                        .withdraw_ordered_receiver(connection_id, request.reference)
                {
                    let generation = self.lifecycle.next_generation();
                    self.stage_ordered_receiver(generation, ready, false);
                    self.publish_staged();
                }
                let outcomes = self.lifecycle.set_owned_subscription(
                    connection_id,
                    request,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                for outcome in outcomes {
                    self.publish_track_lifecycle(outcome);
                }
            }
            ShardEvent::OwnedSubscriptionRemoved {
                source_room,
                subscriber,
                connection_id,
                reference,
            } => self.begin_owned_cutoff(subscriber, connection_id, reference, source_room, None),
            ShardEvent::TrackSubscribed {
                subscriber,
                connection_id,
                track,
                ..
            } => {
                let Some(endpoint) = self
                    .core
                    .registry
                    .get_incarnation(subscriber, connection_id)
                    .and_then(super::registry::ParticipantMeta::endpoint)
                else {
                    return;
                };
                let outcome = self.lifecycle.activate(
                    track.room,
                    track.origin,
                    track.id,
                    endpoint,
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
                let Some(endpoint) = self
                    .core
                    .registry
                    .get_incarnation(subscriber, connection_id)
                    .and_then(super::registry::ParticipantMeta::endpoint)
                else {
                    return;
                };
                let outcome = self.lifecycle.deactivate(
                    track.room,
                    track.origin,
                    track.id,
                    endpoint,
                    &self.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                );
                self.publish_track_lifecycle(outcome);
            }
            #[cfg(test)]
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
            } => self.begin_renewal(shard, participant, connection_id, token.0),
        }
    }

    fn advance_incarnation_cutoffs(&mut self) {
        for _ in 0..API_COMMAND_BUDGET {
            let Some((ticket, incarnation)) = self.ownership.next_retirement() else {
                break;
            };
            let bytes = self
                .updates
                .len()
                .saturating_mul(
                    std::mem::size_of::<crate::shard_update::ShardUpdate>()
                        .saturating_add(std::mem::size_of::<oneshot::Receiver<()>>()),
                )
                .saturating_add(std::mem::size_of::<NativeDeletionCompletion>().saturating_mul(2))
                .saturating_add(ticket.retained_bytes());
            let mut acks = Vec::new();
            if self.deletion_bytes.saturating_add(bytes) > PENDING_NATIVE_DELETE_BYTES
                || self
                    .updates
                    .get(incarnation.transport.shard().index())
                    .is_none()
                || self
                    .updates
                    .iter()
                    .any(crate::shard_update::ShardUpdateWriter::is_closed)
                || acks.try_reserve_exact(self.updates.len()).is_err()
            {
                break;
            }
            self.ownership.start_retirement(&ticket);
            self.deletion_bytes = self.deletion_bytes.saturating_add(bytes);
            self.publish_staged();
            let generation = self.lifecycle.next_generation();
            for (index, writer) in self.updates.iter_mut().enumerate() {
                acks.push(writer.stage_cutoff(generation));
                let Some(touched) = self.update_touched.get_mut(index) else {
                    pulsebeam_runtime::fatal!("ownership cutoff targeted an unconfigured shard");
                };
                *touched = true;
            }
            self.publish_staged();
            let completed = self.deletion_tx.clone();
            pulsebeam_runtime::task::spawn(async move {
                let success = drain_cutoffs(&mut acks).await;
                let _ = completed
                    .send(NativeDeletionCompletion {
                        bytes,
                        publication: None,
                        ownership: Some(OwnershipCutoff {
                            ticket,
                            incarnation,
                            success,
                        }),
                    })
                    .await;
            });
        }
    }

    fn release_deletion(&mut self, completed: NativeDeletionCompletion) {
        self.deletion_bytes = self
            .deletion_bytes
            .checked_sub(completed.bytes)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("native deletion credit released twice"));
        if let Some(result) = completed.ownership {
            self.ownership.finish_retirement(
                &result.ticket,
                result.incarnation,
                result.success,
                pulsebeam_runtime::time::Instant::now(),
            );
        }
        if let Some(result) = completed.publication
            && let Some(meta) = self
                .core
                .registry
                .get_incarnation(result.participant, result.connection_id)
            && self.native_connections.contains_key(&result.connection_id)
        {
            self.command_backlog.push_back((
                meta.shard_id,
                ShardCommand::PublicationCutoffResult {
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
            meta.room,
            Some(meta),
        );
    }

    fn begin_owned_cutoff(
        &mut self,
        participant: ParticipantId,
        connection_id: ConnectionId,
        reference: u64,
        source_room: ProjectRoomKey,
        publication: Option<crate::track::TrackMeta>,
    ) {
        let Some(shard) = self
            .core
            .registry
            .get_incarnation(participant, connection_id)
            .and_then(|current| {
                (current.scope.project == source_room.project
                    && publication
                        .as_ref()
                        .is_none_or(|track| current.scope.joined_room() == Some(track.room))
                    && self.native_connections.contains_key(&connection_id))
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
        let ordered = publication
            .is_none()
            .then(|| {
                self.lifecycle
                    .withdraw_ordered_receiver(connection_id, reference)
            })
            .flatten();
        let outcomes = if let Some(meta) = publication {
            let outcome = self.lifecycle.unpublish(
                EndpointKey {
                    room: meta.room,
                    participant: meta.origin,
                },
                meta.id,
                &self.core.registry,
                pulsebeam_runtime::time::Instant::now(),
            );
            if outcome.is_some() {
                self.observe_native_source(&meta, false);
            }
            outcome.into_iter().collect()
        } else {
            self.lifecycle.unsubscribe_owned(
                source_room,
                super::topology::Subscriber::Connection(connection_id),
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
        if let Some(ready) = ordered {
            if let Some(touched) = affected.get_mut(ready.binding.origin.index()) {
                *touched = true;
            }
            self.stage_ordered_receiver(generation, ready, false);
        }
        if !admitted {
            // Local execution is fenced; withdraw topology even if receipt credit is unavailable.
            self.publish_staged();
            self.release_deletion(NativeDeletionCompletion {
                bytes: 0,
                ownership: None,
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

    fn sdp_resource_lease(
        &self,
        connection: ConnectionId,
        profile: SignalingProtocol,
    ) -> Option<&AuthorizationLease> {
        if let Some((participant, meta)) = self.core.registry.get_connection(connection)
            && meta.profile == profile
            && let Some(lease) = &meta.authorization
            && lease.matches_scope(meta.scope, participant)
        {
            return Some(lease);
        }
        self.materializations.iter().find_map(|work| {
            let pending = &work.pending;
            let lease = pending.authorization.as_ref()?;
            (pending.connection_id() == connection
                && pending.profile == profile
                && lease.matches_scope(pending.scope, pending.participant))
            .then_some(lease)
        })
    }

    fn authorize_native_resource(
        &self,
        request: NativeResourceRequest,
        wall_now: SystemTime,
    ) -> Result<VerifiedAuthorization, ControllerError> {
        if request.principal.expiry().is_expired_at(
            wall_now
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ) {
            return Err(ControllerError::AuthorizationExpired);
        }
        let accepted = self
            .native_resource_lease(request.connection_id)
            .ok_or(ControllerError::GrantGone)?;
        if request.principal.project_id() != accepted.authority.project_id
            || request.principal.participant_id() != &accepted.authority.participant_id
        {
            return Err(ControllerError::AuthorizationRejected);
        }
        let authorization = request
            .principal
            .authorize_existing(&accepted.authority)
            .map_err(|_| ControllerError::AuthorizationRejected)?;
        if !accepted.matches_identity(&authorization) {
            return Err(ControllerError::AuthorizationRejected);
        }
        Ok(authorization)
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
        let pending = self
            .materializations
            .iter()
            .filter(|work| work.matches_native(&verified, request.connection_id))
            .count();
        let bytes = self
            .updates
            .len()
            .saturating_mul(
                std::mem::size_of::<crate::shard_update::ShardUpdate>()
                    .saturating_add(std::mem::size_of::<oneshot::Receiver<()>>()),
            )
            .saturating_add(std::mem::size_of::<ControllerCommand>())
            .saturating_add(
                pending.saturating_mul(
                    std::mem::size_of::<crate::shard_update::ShardUpdate>()
                        .saturating_add(
                            std::mem::size_of::<oneshot::Receiver<()>>().saturating_mul(2),
                        )
                        .saturating_add(std::mem::size_of::<MaterializationCutoffs>()),
                ),
            );
        if self.deletion_bytes.saturating_add(bytes) > PENDING_NATIVE_DELETE_BYTES
            || self.materializations.iter().any(|work| {
                work.matches_native(&verified, request.connection_id) && work.cutoffs.is_full()
            })
            || self
                .updates
                .iter()
                .any(crate::shard_update::ShardUpdateWriter::is_closed)
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let mut acks = Vec::new();
        acks.try_reserve_exact(self.updates.len().saturating_add(pending))
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        for work in &mut self.materializations {
            if work.matches_native(&verified, request.connection_id) {
                let (done, receipt) = oneshot::channel();
                work.cutoffs.push(done);
                work.superseded = true;
                acks.push(receipt);
            }
        }
        if self
            .core
            .registry
            .get_incarnation(verified.participant_id, request.connection_id)
            .is_some_and(|meta| {
                meta.scope.room == verified.room_id
                    && meta
                        .authorization
                        .as_ref()
                        .is_some_and(|lease| lease.matches_identity(&verified))
                    && meta.profile == SignalingProtocol::State
            })
        {
            self.remove_incarnation(verified.participant_id, request.connection_id);
        }
        // Fence every writer: an old Location may already have teardown queued on a former shard.
        self.publish_staged();
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
            .get_incarnation(verified.participant_id, request.connection_id)
            .ok_or(ControllerError::GrantGone)?;
        if meta.profile != SignalingProtocol::State {
            return Err(ControllerError::GrantGone);
        }
        if meta.scope.room != verified.room_id {
            return Err(ControllerError::AuthorizationRejected);
        }
        let previous = meta
            .authorization
            .clone()
            .ok_or(ControllerError::GrantGone)?;
        if !previous.matches_identity(&verified) {
            return Err(ControllerError::AuthorizationRejected);
        }
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
        let participant = verified.participant_id;
        let lease = previous.replacing(verified, wall_now, runtime_now)?;
        let revision = lease.revision();
        self.install_authorization(participant, request.connection_id, &previous, lease)?;
        self.send_renewal_result(shard, request.connection_id, revision, Some(expiry));
        Ok(revision)
    }

    fn install_authorization(
        &mut self,
        participant: ParticipantId,
        connection_id: ConnectionId,
        previous: &AuthorizationLease,
        lease: AuthorizationLease,
    ) -> Result<(), ControllerError> {
        let meta = self
            .core
            .registry
            .get_incarnation(participant, connection_id)
            .filter(|meta| meta.authorization.as_ref() == Some(previous))
            .ok_or(ControllerError::GrantGone)?;
        if !previous.matches_identity(&lease.authority) {
            return Err(ControllerError::AuthorizationRejected);
        }
        if !lease.allows_current_room() {
            self.terminate_incarnation(
                participant,
                meta.clone(),
                true,
                ConnectionEnd::AuthorizationRevoked,
            );
            return Err(ControllerError::AuthorizationRejected);
        }
        self.authorization_expiries
            .remove(&AuthorizationExpiryWork {
                deadline: previous.deadline(),
                participant_id: participant,
                connection_id,
            });
        self.core
            .registry
            .update_authorization(&participant, connection_id, lease.clone());
        self.authorization_expiries.insert(AuthorizationExpiryWork {
            deadline: lease.deadline(),
            participant_id: participant,
            connection_id,
        });
        if lease.authority != previous.authority {
            log::credential_event(
                false,
                lease.authority.project_id,
                lease.authority.token_id,
                connection_id,
            );
        }
        self.observe_native_authorization(connection_id, &lease);
        Ok(())
    }

    fn begin_renewal(
        &mut self,
        shard: ShardId,
        participant: ParticipantId,
        connection_id: ConnectionId,
        token: String,
    ) {
        let Some(meta) = self
            .core
            .registry
            .get_incarnation(participant, connection_id)
        else {
            return;
        };
        if meta.shard_id != shard {
            return;
        }
        let Some(accepted) = meta.authorization.as_ref() else {
            return;
        };
        let expected_revision = accepted.revision();
        let accepted = accepted.authority.clone();
        let Some(registry) = self.key_registry.clone() else {
            self.send_renewal_result(shard, connection_id, expected_revision, None);
            return;
        };
        if token.len() > MAX_COMPACT_TOKEN_LEN || self.renewals_inflight >= MAX_PENDING_RENEWALS {
            self.send_renewal_result(shard, connection_id, expected_revision, None);
            return;
        }
        self.renewals_inflight = self.renewals_inflight.saturating_add(1);
        let tx = self.renewal_tx.clone();
        pulsebeam_runtime::task::spawn_blocking(move || {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            let verified = authenticate_participant_token(&registry, &token, now)
                .ok()
                .and_then(|principal| principal.authorize_existing(&accepted).ok());
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
        let Some(meta) = self
            .core
            .registry
            .get_incarnation(result.participant, result.connection_id)
        else {
            return;
        };
        if meta.profile != SignalingProtocol::State {
            return;
        }
        let shard = meta.shard_id;
        let previous = meta.authorization.clone();
        let wall_now = SystemTime::now();
        let expiry = result.verified.and_then(|verified| {
            if verified.participant_id != result.participant
                || verified.room_id != meta.scope.room
                || previous.as_ref().is_none_or(|lease| {
                    !lease.matches_identity(&verified)
                        || lease.revision() != result.expected_revision
                        || lease.is_expired_at(wall_now)
                        || verified.expiry.unix_seconds() <= lease.expiry().unix_seconds()
                })
            {
                return None;
            }
            i64::try_from(verified.expiry.unix_seconds()).ok()?;
            previous
                .as_ref()?
                .replacing(verified, wall_now, pulsebeam_runtime::time::Instant::now())
                .ok()
        });
        let revision = expiry
            .as_ref()
            .map_or(result.expected_revision, AuthorizationLease::revision);
        let mut unix_expiry = expiry
            .as_ref()
            .and_then(|lease| i64::try_from(lease.expiry().unix_seconds()).ok());
        if let (Some(lease), Some(previous)) = (expiry, previous)
            && self
                .install_authorization(result.participant, result.connection_id, &previous, lease)
                .is_err()
        {
            unix_expiry = None;
        }
        self.send_renewal_result(shard, result.connection_id, revision, unix_expiry);
    }

    fn send_renewal_result(
        &mut self,
        shard: ShardId,
        connection_id: ConnectionId,
        revision: u64,
        expiry: Option<i64>,
    ) {
        self.command_backlog.push_back((
            shard,
            ShardCommand::AuthorizationResult {
                connection_id,
                revision,
                expiry,
            },
        ));
    }
}

impl ControllerActor {
    fn apply_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        for operation in outcome.operations {
            match operation {
                TrackLifecycleOperation::Update {
                    shard,
                    op,
                    route_scope,
                } => {
                    self.stage_update_at(shard, outcome.generation, op, route_scope);
                }
                TrackLifecycleOperation::Plans { shard, plans } => {
                    self.stage_plans_at(shard, outcome.generation, plans);
                }
                TrackLifecycleOperation::ParticipantEffect {
                    shard,
                    connection,
                    effect,
                    ..
                } => self.stage_participant_at(shard, outcome.generation, connection, effect),
            }
        }
    }

    fn publish_track_lifecycle(&mut self, outcome: TrackLifecycleOutcome) {
        self.apply_track_lifecycle(outcome);
        self.publish_staged();
    }

    fn stage_ordered_receiver(
        &mut self,
        generation: u64,
        ready: crate::route::fabric::ShardDataReady,
        enabled: bool,
    ) {
        self.stage_update_at(
            ready.binding.origin,
            generation,
            crate::shard_update::ShardUpdateOp::SetShardDataReceiver { ready, enabled },
            None,
        );
    }

    fn stage_update_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        op: crate::shard_update::ShardUpdateOp,
        route_scope: Option<super::link::RouteScope>,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "an update must target a live shard");
            return;
        };
        let retired = match &op {
            crate::shard_update::ShardUpdateOp::RetireRoute { address } => Some(*address),
            _ => None,
        };
        let installed = match &op {
            crate::shard_update::ShardUpdateOp::InstallRoute { address, action } => {
                Some((*address, *action))
            }
            _ => None,
        };
        update.stage(generation, op);
        if let (Some((address, action)), Some(links)) = (installed, self.links.as_mut())
            && links.network_enabled()
        {
            links.install_after(
                address,
                action,
                route_scope,
                update.stage_cutoff(generation),
            );
        }
        if let (Some(address), Some(links)) = (retired, self.links.as_mut())
            && links.contains(address)
        {
            links.retire_after(address, |bindings| {
                update.stage_link_cutoff(generation, bindings)
            });
        }
        self.mark_update_touched(shard);
    }

    fn stage_plans_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        mut plans: Vec<crate::shard_update::TrackPlanUpdate>,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "plans must target a live shard");
            return;
        };
        let mut retired = Vec::new();
        if let Some(links) = &mut self.links {
            for update in &mut plans {
                retired.extend(links.prepare_plan(shard, update));
            }
        }
        update.stage_plans(generation, plans);
        if !retired.is_empty() {
            if let Some(links) = &mut self.links {
                links.retire_bindings_after(retired, update.stage_cutoff(generation));
            }
        }
        self.mark_update_touched(shard);
    }

    fn retire_network_send(&mut self, binding: pulsebeam_link::BindingId) {
        let Some((shard, plan)) = self
            .links
            .as_mut()
            .and_then(|links| links.remove_send(binding))
        else {
            return;
        };
        let generation = self.lifecycle.next_generation();
        self.stage_plans_at(shard, generation, vec![plan]);
        if let (Some(update), Some(links)) =
            (self.updates.get_mut(shard.index()), self.links.as_mut())
        {
            links.retire_bindings_after(vec![binding], update.stage_cutoff(generation));
        }
        self.publish_staged();
    }

    fn stage_publisher_data_observations(
        &mut self,
        publisher_observations: Vec<crate::route::fabric::PublisherObservation>,
    ) {
        if publisher_observations.is_empty() {
            return;
        }
        let generation = self.lifecycle.next_generation();
        for observation in publisher_observations {
            let shard = observation.route.shard;
            if let Some(writer) = self
                .updates
                .get_mut(shard.index())
                .filter(|writer| !writer.is_closed())
            {
                writer.stage_publisher_data_observation(generation, observation);
                self.mark_update_touched(shard);
            }
        }
        self.publish_staged();
    }

    fn stage_peer_wan_live_receive(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        spec: super::link::wan::WanLiveReceiveSpec,
    ) -> Option<pulsebeam_core::fabric::wan::Window> {
        self.stage_live_children(|links, stage| links.stage_wan_live_receive(now, spec, stage).ok())
    }

    fn stage_live_children<T>(
        &mut self,
        stage: impl FnOnce(
            &mut super::link::Links,
            &mut dyn FnMut(
                crate::route::fabric::LiveNetworkChild,
                pulsebeam_link::BindingCutoff,
            )
                -> oneshot::Receiver<Option<crate::route::fabric::LiveNetworkChild>>,
        ) -> Option<T>,
    ) -> Option<T> {
        let links = self.links.as_mut()?;
        let updates = &mut self.updates;
        let generation = self.lifecycle.next_generation();
        let mut staged = Vec::new();
        let receive = stage(links, &mut |child, lifetime| {
            let shard = child.destination.shard();
            if let Some(writer) = updates
                .get_mut(shard.index())
                .filter(|writer| !writer.is_closed())
            {
                staged.push(shard);
                writer.stage_live_network_child(generation, child, lifetime)
            } else {
                let (tx, receipt) = oneshot::channel();
                let _ = tx.send(None);
                receipt
            }
        });
        for shard in staged {
            self.mark_update_touched(shard);
        }
        self.publish_staged();
        receive
    }

    fn sample_wan_publisher_anchors(
        &mut self,
        snapshot: &ownership::observations::Snapshot,
    ) -> Vec<(
        (ProjectRoomKey, crate::shard_update::PublisherAnchor),
        oneshot::Receiver<Option<crate::shard_update::PublisherPosition>>,
    )> {
        let candidates = snapshot
            .iter()
            .flat_map(|(endpoint, fact)| {
                fact.publications.iter().filter_map(|publication| {
                    let connection = fact.claim.connection();
                    let key = publication
                        .data_publisher(connection)
                        .map(|publisher| publisher.publication)
                        .or_else(|| publication.opus_publication())?;
                    Some((endpoint.room, key, connection))
                })
            })
            .take(peer_ownership::relay::LIMIT)
            .filter_map(|(room, publication, connection)| {
                let (shard, publisher) = self.lifecycle.network_publisher_anchor(
                    room,
                    publication,
                    &self.core.registry,
                )?;
                (publisher.connection == connection).then_some((
                    shard,
                    publisher,
                    (room, publisher),
                ))
            })
            .collect::<Vec<_>>();
        self.stage_anchor_samples(candidates)
    }

    fn stage_anchor_samples<T>(
        &mut self,
        candidates: Vec<(ShardId, crate::shard_update::PublisherAnchor, T)>,
    ) -> Vec<(
        T,
        oneshot::Receiver<Option<crate::shard_update::PublisherPosition>>,
    )> {
        let generation = self.lifecycle.next_generation();
        let mut receipts = Vec::new();
        for (shard, anchor, publisher) in candidates {
            let Some(writer) = self
                .updates
                .get_mut(shard.index())
                .filter(|writer| !writer.is_closed())
            else {
                continue;
            };
            receipts.push((publisher, writer.stage_publisher_anchor(generation, anchor)));
            self.mark_update_touched(shard);
        }
        self.publish_staged();
        receipts
    }

    fn stage_unavailable_network_repair(
        &mut self,
        interest: crate::route::fabric::network_repair::Interest,
        now: pulsebeam_runtime::time::Instant,
    ) {
        if !self
            .links
            .as_ref()
            .is_some_and(|links| links.allows_wan_repair_interest(interest, now))
        {
            return;
        }
        let shard = interest.parent.destination.shard();
        let Some(writer) = self
            .updates
            .get_mut(shard.index())
            .filter(|writer| !writer.is_closed())
        else {
            return;
        };
        let generation = self.lifecycle.next_generation();
        writer.stage_unavailable_network_repair(generation, interest);
        self.mark_update_touched(shard);
        self.publish_staged();
    }

    fn stage_wan_repair_receive(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        interest: crate::route::fabric::network_repair::Interest,
        sequence: u64,
    ) -> Option<crate::route::fabric::network_repair::RepairChild> {
        let shard = interest.parent.destination.shard();
        let links = self.links.as_mut()?;
        let writer = self
            .updates
            .get_mut(shard.index())
            .filter(|writer| !writer.is_closed())?;
        let generation = self.lifecycle.next_generation();
        let receipt = links.stage_wan_repair_receive(now, interest, sequence, |child, lifetime| {
            writer.stage_repair_network_child(generation, child, lifetime)
        });
        self.mark_update_touched(shard);
        self.publish_staged();
        receipt.ok()
    }

    fn stage_wan_media_repair_receive(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        parent: crate::route::fabric::LiveNetworkChild,
        position: u64,
        expires: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::network_repair::MediaRepairChild> {
        let shard = parent.destination.shard();
        let links = self.links.as_mut()?;
        let writer = self
            .updates
            .get_mut(shard.index())
            .filter(|writer| !writer.is_closed())?;
        let generation = self.lifecycle.next_generation();
        let receipt = links.stage_wan_media_repair_receive(
            now,
            parent,
            position,
            expires,
            |child, lifetime| writer.stage_media_repair_child(generation, child, lifetime),
        );
        self.mark_update_touched(shard);
        self.publish_staged();
        receipt.ok()
    }

    fn cancel_peer_repair_receive(
        &mut self,
        interest: crate::route::fabric::network_repair::Interest,
    ) {
        if let Some(links) = &mut self.links {
            links.cancel_wan_repair(interest);
        }
    }

    fn stage_wan_repair_handoff(
        &mut self,
        handoff: crate::route::fabric::relay::PeerRepairHandoff,
    ) -> bool {
        let shard = handoff.located.lookup.shard();
        let Some(writer) = self
            .updates
            .get_mut(shard.index())
            .filter(|writer| !writer.is_closed())
        else {
            return false;
        };
        let generation = self.lifecycle.next_generation();
        writer.stage_relay_repair_handoff(generation, handoff);
        self.mark_update_touched(shard);
        self.publish_staged();
        true
    }

    fn stage_repair_lookups(
        &mut self,
        lookups: Vec<crate::route::fabric::relay::PeerRepairLookup>,
    ) -> Vec<oneshot::Receiver<Option<crate::route::fabric::relay::LocatedRepair>>> {
        if lookups.is_empty() {
            return Vec::new();
        }
        let generation = self.lifecycle.next_generation();
        let mut receipts = Vec::with_capacity(lookups.len());
        for lookup in lookups {
            let shard = lookup.shard();
            if let Some(writer) = self
                .updates
                .get_mut(shard.index())
                .filter(|writer| !writer.is_closed())
            {
                receipts.push(writer.stage_relay_repair_lookup(generation, lookup));
                self.mark_update_touched(shard);
            }
        }
        self.publish_staged();
        receipts
    }

    fn stage_peer_data_expectation(
        &mut self,
        peer: pulsebeam_link::NetworkId,
        frame: peer_ownership::fabric_data::Frame,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<Vec<pulsebeam_runtime::sync::oneshot::Receiver<bool>>> {
        let notices = self
            .links
            .as_ref()?
            .wan_data_expectations(peer, frame, now)?;
        if notices.is_empty()
            || notices.iter().any(|notice| {
                self.updates
                    .get(notice.parent.destination.shard().index())
                    .is_none_or(crate::shard_update::ShardUpdateWriter::is_closed)
            })
        {
            return None;
        }
        let generation = self.lifecycle.next_generation();
        let mut receipts = Vec::with_capacity(notices.len());
        for notice in notices {
            let shard = notice.parent.destination.shard();
            let writer = self.updates.get_mut(shard.index())?;
            receipts.push(writer.stage_network_data_expectation(generation, notice));
            self.mark_update_touched(shard);
        }
        self.publish_staged();
        Some(receipts)
    }

    fn stage_publisher_send(
        &mut self,
        change: crate::route::fabric::PublisherSendChange,
    ) -> Option<oneshot::Receiver<bool>> {
        let shard = change.route().shard;
        let writer = self
            .updates
            .get_mut(shard.index())
            .filter(|writer| !writer.is_closed())?;
        let generation = self.lifecycle.next_generation();
        let receipt = writer.stage_publisher_send(generation, change);
        self.mark_update_touched(shard);
        self.publish_staged();
        Some(receipt)
    }

    fn stage_fabric_relay_change(
        &mut self,
        change: crate::route::fabric::LiveRelayChange,
    ) -> Option<pulsebeam_runtime::sync::oneshot::Receiver<bool>> {
        let shard = change.route().input.destination.shard();
        let update = self.updates.get_mut(shard.index())?;
        if update.is_closed() {
            return None;
        }
        let receipt = update.stage_live_relay(self.lifecycle.next_generation(), change);
        self.mark_update_touched(shard);
        self.publish_staged();
        Some(receipt)
    }

    fn stage_participant_at(
        &mut self,
        shard: ShardId,
        generation: u64,
        connection: ConnectionId,
        effect: crate::participant::ParticipantEffect,
    ) {
        let Some(update) = self.updates.get_mut(shard.index()) else {
            debug_assert!(false, "participant effects must target a live shard");
            return;
        };
        update.stage_participant_effect(generation, connection, effect);
        self.mark_update_touched(shard);
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
        let (attempts, progressed) = self
            .command_backlog
            .flush(&mut self.router, SHARD_COMMAND_EGRESS_BUDGET);
        self.record_budget_hit("shard_commands", attempts, SHARD_COMMAND_EGRESS_BUDGET);
        progressed
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
        self.catalog.has_work()
            || self.egress_ready
                && (!self.command_backlog.is_empty() || !self.pending_updates.is_empty())
    }

    fn record_budget_hit(&self, phase: &'static str, used: usize, budget: usize) {
        debug_assert!(budget > 0);
        if used == budget {
            metrics::counter!("control_tick_budget_hit", "phase" => phase).increment(1);
        }
    }

    fn admission_room(
        room: RoomId,
        authorization: Option<&AuthorizationLease>,
    ) -> Result<ProjectRoomKey, ControllerError> {
        if let Some(lease) = authorization {
            return Ok(ProjectRoomKey {
                project: lease.authority.project_id,
                room,
            });
        }
        #[cfg(test)]
        return Ok(ProjectRoomKey::fixture(room));
        #[cfg(not(test))]
        Err(ControllerError::AuthorizationRejected)
    }

    fn resolve_whep_sources(
        &self,
        sources: &super::sdp::MediaTracks<super::sdp::SourceAddress>,
        project: crate::entity::ProjectId,
        lease: &AuthorizationLease,
    ) -> Result<
        super::sdp::MediaTracks<(ProjectRoomKey, crate::entity::PublicationKey)>,
        ControllerError,
    > {
        use pulsebeam_core::auth::grants::{Action, TrackKind};
        use pulsebeam_rtc::MediaKind;
        let kind_for = |kind| match kind {
            MediaKind::Audio => TrackKind::Audio,
            MediaKind::Video => TrackKind::Video,
        };
        if !sources.iter().all(|(kind, source)| {
            lease.permissions().allows_at_room(
                Action::Subscribe,
                source.room.as_str(),
                Some(source.participant.as_str()),
                Some(kind_for(kind)),
                Some(source.track.as_str()),
            )
        }) {
            return Err(ControllerError::AuthorizationRejected);
        }
        let mut bindings = super::sdp::MediaTracks::default();
        for (kind, source) in sources.iter() {
            let room = ProjectRoomKey {
                project,
                room: source.room,
            };
            let track_kind = match kind {
                MediaKind::Audio => crate::entity::TrackKind::Audio,
                MediaKind::Video => crate::entity::TrackKind::Video,
            };
            let publication = self
                .lifecycle
                .current_named_source(
                    EndpointKey {
                        room,
                        participant: source.participant,
                    },
                    track_kind,
                    source.track,
                    &self.core.registry,
                )
                .ok_or(ControllerError::SourceUnavailable)?;
            bindings
                .insert(kind, (room, publication.id()))
                .map_err(|_| NegotiatorError::InvalidResources)?;
        }
        Ok(bindings)
    }

    fn validate_whep_codecs(
        &self,
        session: &pulsebeam_rtc::SessionInfo,
        bindings: &super::sdp::MediaTracks<(ProjectRoomKey, crate::entity::PublicationKey)>,
    ) -> Result<(), ControllerError> {
        for (kind, &(room, publication)) in bindings.iter() {
            let receiver = session
                .senders
                .iter()
                .find(|sender| sender.kind == kind)
                .ok_or(NegotiatorError::InvalidResources)?;
            let track = self
                .lifecycle
                .source_exact(room, publication)
                .ok_or(ControllerError::SourceUnavailable)?;
            let format = match track {
                crate::track::Track::Audio(audio) => audio.format.as_ref(),
                crate::track::Track::Video(video) => video.format.as_ref(),
                crate::track::Track::Data(_) => None,
            };
            if !format.is_some_and(|format| super::sdp::codec_compatible(format, receiver.codec)) {
                return Err(ControllerError::SourceIncompatible);
            }
        }
        Ok(())
    }

    fn begin_create_participant(
        &mut self,
        mut state: ParticipantState,
        offer: String,
    ) -> Result<PendingMaterialization, ControllerError> {
        if state.authorization.as_ref().is_some_and(|lease| {
            !lease.matches_participant(state.room_id, state.participant_id)
                || !lease.allows_current_room()
        }) {
            return Err(ControllerError::AuthorizationRejected);
        }
        let control_capabilities = if state.profile == SignalingProtocol::State {
            if offer.len() > MAX_NATIVE_SDP_BYTES {
                return Err(NegotiatorError::InvalidResources.into());
            }
            if let Some(lease) = state.authorization.as_ref() {
                lease
                    .native_expiry()
                    .map_err(|_| ControllerError::AuthorizationRejected)?;
            }
            Some(
                state
                    .rtp_capabilities
                    .take()
                    .ok_or(NegotiatorError::InvalidResources)?,
            )
        } else {
            None
        };
        let scope = match state.room_id {
            Some(room) => Self::admission_room(room, state.authorization.as_ref())?.into(),
            None if state.profile != SignalingProtocol::Whip => crate::entity::ConnectionScope {
                project: state
                    .authorization
                    .as_ref()
                    .ok_or(ControllerError::AuthorizationRejected)?
                    .authority
                    .project_id,
                room: None,
            },
            None => return Err(ControllerError::AuthorizationRejected),
        };
        let participant_id = state.participant_id;
        let whep_bindings = match (&state.selection, state.profile) {
            (super::sdp::Selection::Native, SignalingProtocol::State)
            | (super::sdp::Selection::Whip(_), SignalingProtocol::Whip) => Default::default(),
            (super::sdp::Selection::Whep(sources), SignalingProtocol::Whep)
                if scope.room.is_none() =>
            {
                self.resolve_whep_sources(
                    sources,
                    scope.project,
                    state
                        .authorization
                        .as_ref()
                        .ok_or(ControllerError::AuthorizationRejected)?,
                )?
            }
            _ => return Err(NegotiatorError::InvalidResources.into()),
        };
        let choose = |snapshot: Option<&pulsebeam_runtime::rt::ExecutionSnapshot>| {
            let now = pulsebeam_runtime::time::Instant::now();
            let shards = self.router.shard_count();
            if let Some(room) = scope.joined_room() {
                self.placement
                    .admit(&self.core, &room, &participant_id, shards, snapshot, now)
                    .map(|(slot, shard)| (Some(slot), shard))
            } else {
                self.placement
                    .admit_without_partition(&self.core, shards, snapshot, now)
                    .map(|shard| (None, shard))
            }
        };
        let selected = if let Some(execution) = self.execution.as_ref() {
            execution.with_snapshot(choose)
        } else {
            choose(None)
        };
        let (slot, shard) = selected.ok_or(ControllerError::ServiceUnavailable)?;
        let at = self
            .clock
            .observe_execution(pulsebeam_runtime::net::ReceiveTime::now());
        let now = at.monotonic;
        let rtp_capabilities = control_capabilities
            .as_ref()
            .map(crate::control::rtp::transport_capabilities)
            .transpose()
            .map_err(|_| NegotiatorError::InvalidResources)?;
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
        let selected_media = match &state.selection {
            super::sdp::Selection::Native => Ok(()),
            super::sdp::Selection::Whip(overrides) => resources
                .name_whip_tracks(&rtc.session, overrides)
                .map_err(Into::into),
            super::sdp::Selection::Whep(_) => resources
                .bind_whep(&whep_bindings)
                .map_err(ControllerError::from)
                .and_then(|()| self.validate_whep_codecs(&rtc.session, &whep_bindings)),
        };
        if let Err(error) = selected_media {
            self.core.release_transport(address, now);
            return Err(error);
        }
        if !resources.publications_allowed(
            state.profile,
            state.authorization.as_ref().map_or(
                &super::permissions::ResourcePermissions::Denied,
                AuthorizationLease::permissions,
            ),
        ) {
            self.core.release_transport(address, now);
            return Err(ControllerError::AuthorizationRejected);
        }
        if control_capabilities.is_some() && answer.len() > MAX_NATIVE_SDP_BYTES {
            self.core.release_transport(address, now);
            return Err(NegotiatorError::InvalidResources.into());
        }
        if let Some(capabilities) = control_capabilities {
            if let Err(error) = rtc
                .connection
                .update_rtp(pulsebeam_rtc::RtpConfigurationPatch::default())
            {
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
            resources.native_rtp_capabilities = Some(capabilities);
            let signaling = crate::participant::signaling::native_signaling_config();
            resources.signaling_channel = Some(
                crate::participant::signaling::SignalingChannel::compile(&signaling),
            );
            if let Err((reason, _)) = rtc.connection.open_data_channel(now.into(), signaling) {
                self.core.release_transport(address, now);
                return Err(match reason {
                    pulsebeam_rtc::WriteError::InvalidState => {
                        NegotiatorError::InvalidResources.into()
                    }
                    _ => ControllerError::ServiceUnavailable,
                });
            }
        }
        let connection_id = state.connection_id;
        let authorization = state.authorization.clone();
        let profile = state.profile;
        let native = if let Some(capabilities) = resources.native_rtp_capabilities.take() {
            let (control, endpoint) =
                super::native::handoff::pair(connection_id, self.native_ready_tx.clone());
            resources.native_handoff = Some(endpoint);
            let Ok(mut native) = StateProtocolHandler::new(
                participant_id,
                state.room_id,
                Some(capabilities),
                control,
            ) else {
                self.core.release_transport(address, now);
                return Err(ControllerError::ServiceUnavailable);
            };
            if let Some(lease) = authorization.as_ref() {
                let initialized = lease.native_expiry().ok().and_then(|expiry| {
                    native
                        .model
                        .authorization(expiry, lease.revision, lease.permissions().clone())
                        .ok()
                });
                if initialized.is_none() {
                    self.core.release_transport(address, now);
                    return Err(ControllerError::AuthorizationRejected);
                }
            }
            Some(native)
        } else {
            None
        };
        let config = self.core.prepare_participant(rtc, resources, state, scope);
        let (ack_tx, ack_rx) = oneshot::channel();
        if let (Some(room), Some(slot)) = (scope.joined_room(), slot) {
            self.core
                .registry
                .reserve_partition(room, participant_id, connection_id, shard, slot);
        }
        Ok(PendingMaterialization {
            native,
            shard,
            partition_index: slot,
            command: Some(ShardCommand::MaterializeParticipant {
                transport: address,
                config: Box::new(config),
                ack: ack_tx,
            }),
            ack: Some(ack_rx),
            participant: participant_id,
            claim: ownership::ClaimRank::root(connection_id),
            authorization,
            profile,
            transport: address,
            scope,
            answer,
            whep_bindings,
            owned: None,
        })
    }

    fn complete_materialization(
        &mut self,
        pending: PendingMaterialization,
        wall_now: SystemTime,
    ) -> Result<String, ControllerError> {
        let endpoint = pending.endpoint();
        let connection = pending.connection_id();
        let result = self.commit_materialization(pending, wall_now);
        if let Some(endpoint) = endpoint {
            self.core
                .registry
                .release_partition(endpoint.room, endpoint.participant, connection);
        }
        result
    }

    fn commit_materialization(
        &mut self,
        mut pending: PendingMaterialization,
        wall_now: SystemTime,
    ) -> Result<String, ControllerError> {
        let participant_id = pending.participant;
        for (_, &(room, publication)) in pending.whep_bindings.iter() {
            if self
                .lifecycle
                .network_source(room, publication, &self.core.registry)
                .is_none()
            {
                return Err(ControllerError::SourceUnavailable);
            }
        }
        let previous = if let Some(room) = pending.scope.joined_room() {
            self.commit_candidate(
                participant_id,
                room,
                pending.shard,
                pending.partition_index.unwrap_or_else(|| {
                    pulsebeam_runtime::fatal!("joined placement reserves a partition")
                }),
                pending.transport,
                pending.claim,
                pending.authorization.take(),
                pending.profile,
                wall_now,
            )?
        } else {
            let lease = pending
                .authorization
                .take()
                .ok_or(ControllerError::AuthorizationRejected)?;
            if !lease.matches_scope(pending.scope, participant_id) {
                return Err(ControllerError::AuthorizationRejected);
            }
            if lease.is_expired_at(wall_now) {
                return Err(ControllerError::AuthorizationExpired);
            }
            self.core
                .registry
                .commit_unjoined(
                    pending.scope.project,
                    participant_id,
                    pending.shard,
                    pending.transport,
                    pending.connection_id(),
                    pending.profile,
                )
                .map_err(|_| ControllerError::Superseded)?;
            self.record_admitted_authorization(participant_id, pending.connection_id(), lease);
            None
        };
        if let Some(previous) = previous {
            self.retire_defeated_lineage(&previous, pending.claim);
            self.terminate_incarnation(
                participant_id,
                previous,
                false,
                ConnectionEnd::Replaced(Some(pending.connection_id())),
            );
        }
        if let Some(mut native) = pending.native.take() {
            native
                .model
                .identify(pending.scope.project, pending.connection_id());
            let _ = native.handoff.rearm();
            self.native_connections.insert(
                pending.connection_id(),
                NativeService {
                    handler: native,
                    deadline: None,
                },
            );
        }
        self.publish_staged();
        let now = pulsebeam_runtime::time::Instant::now();
        for (_, &(source_room, track)) in pending.whep_bindings.iter() {
            let Some(reference) = whep_reference(track.kind()) else {
                continue;
            };
            let outcomes = self.lifecycle.set_owned_subscription(
                pending.connection_id(),
                super::topology::OwnedSubscription {
                    source_room,
                    subscriber: participant_id,
                    reference,
                    track,
                    enabled: true,
                },
                &self.core.registry,
                now,
            );
            for outcome in outcomes {
                self.publish_track_lifecycle(outcome);
            }
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
        room: ProjectRoomKey,
        shard: ShardId,
        partition_index: usize,
        transport: crate::route::NodeTransportAddress,
        claim: ownership::ClaimRank,
        authorization: Option<AuthorizationLease>,
        profile: SignalingProtocol,
        wall_now: SystemTime,
    ) -> Result<Option<crate::control::registry::ParticipantMeta>, ControllerError> {
        let connection_id = claim.connection();
        if authorization.as_ref().is_some_and(|lease| {
            lease.authority.project_id != room.project
                || !lease.matches_endpoint(room, participant_id)
                || !lease.allows_current_room()
        }) {
            return Err(ControllerError::AuthorizationRejected);
        }
        if authorization
            .as_ref()
            .is_some_and(|lease| lease.is_expired_at(wall_now))
        {
            return Err(ControllerError::AuthorizationExpired);
        }
        if profile == SignalingProtocol::State
            && let Some(lease) = authorization.as_ref()
        {
            lease
                .native_expiry()
                .map_err(|_| ControllerError::AuthorizationRejected)?;
        }
        let endpoint = EndpointKey {
            room,
            participant: participant_id,
        };
        if !self.candidate_is_newer(endpoint, claim) {
            return Err(ControllerError::Superseded);
        }
        // The exact claim holds the room slot and its shard through concurrent
        // joins and departures. A fresh placement choice is not a reservation.
        if !self.core.registry.partition_reserved(
            room,
            participant_id,
            connection_id,
            shard,
            partition_index,
        ) {
            return Err(ControllerError::ServiceUnavailable);
        }
        let previous = self
            .core
            .registry
            .commit_ranked_candidate(
                participant_id,
                room,
                shard,
                partition_index,
                transport,
                claim,
                profile,
            )
            .map_err(|_| ControllerError::Superseded)?;
        if let Some(lease) = authorization {
            self.record_admitted_authorization(participant_id, connection_id, lease);
        }
        self.reconcile_peer_sources(endpoint, pulsebeam_runtime::time::Instant::now());
        // Retain the fence with bounded in-flight work, even if the replacement
        // is removed before an older materialization acknowledges installation.
        for work in &mut self.materializations {
            if work.pending.endpoint() == Some(endpoint) && work.pending.claim < claim {
                work.superseded = true;
            }
        }
        Ok(previous)
    }

    fn record_admitted_authorization(
        &mut self,
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        lease: AuthorizationLease,
    ) {
        let deadline = lease.deadline();
        let project = lease.authority.project_id;
        let jti = lease.authority.token_id;
        self.core
            .registry
            .update_authorization(&participant_id, connection_id, lease);
        self.authorization_expiries.insert(AuthorizationExpiryWork {
            deadline,
            participant_id,
            connection_id,
        });
        log::credential_event(true, project, jti, connection_id);
    }

    fn candidate_is_newer(&self, endpoint: EndpointKey, claim: ownership::ClaimRank) -> bool {
        !self.peer_lineage_retired(endpoint, claim)
            && self
                .peer_ownership
                .highest(endpoint, pulsebeam_runtime::time::Instant::now(), |rank| {
                    !self.peer_lineage_retired(endpoint, rank)
                })
                .is_none_or(|remote| claim > remote)
            && self
                .core
                .registry
                .get_endpoint(endpoint)
                .and_then(|current| current.claim)
                .is_none_or(|current| claim > current)
    }

    fn begin_patch_participant(
        &mut self,
        state: ParticipantState,
        offer: String,
    ) -> Result<PendingMaterialization, ControllerError> {
        let current = self
            .core
            .registry
            .get_endpoint(EndpointKey {
                room: Self::admission_room(
                    state
                        .room_id
                        .ok_or(ControllerError::AuthorizationRejected)?,
                    state.authorization.as_ref(),
                )?,
                participant: state.participant_id,
            })
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
            .get_incarnation(participant, connection_id)
            .cloned()
        else {
            return;
        };
        self.terminate_incarnation(participant, meta, true, ConnectionEnd::Closed);
    }

    fn terminate_incarnation(
        &mut self,
        participant: ParticipantId,
        meta: crate::control::registry::ParticipantMeta,
        remove_current: bool,
        end: ConnectionEnd,
    ) {
        if let Some(transport) = meta.transport {
            self.ownership.withdraw_incarnation(ownership::Incarnation {
                participant,
                connection: meta.connection_id,
                transport,
            });
        }
        let ordered = self
            .lifecycle
            .withdraw_ordered_connection(meta.connection_id);
        if !ordered.is_empty() {
            self.publish_staged();
            let generation = self.lifecycle.next_generation();
            for ready in ordered {
                self.stage_ordered_receiver(generation, ready, false);
            }
            self.publish_staged();
        }
        let native = self.retire_native_connection(meta.connection_id);
        if let Some(lease) = meta.authorization.as_ref() {
            self.authorization_expiries
                .remove(&AuthorizationExpiryWork {
                    deadline: lease.deadline(),
                    participant_id: participant,
                    connection_id: meta.connection_id,
                });
        }
        let sources: Vec<_> = meta
            .scope
            .joined_room()
            .into_iter()
            .flat_map(|room| self.lifecycle.sources_in_room(room))
            .filter(|track| {
                track.meta().origin == participant
                    && track.meta().connection_id == meta.connection_id
            })
            .map(|track| track.meta().clone())
            .collect();
        let mut outcomes = self.lifecycle.remove_connection(
            meta.endpoint(),
            meta.connection_id,
            &self.core.registry,
            pulsebeam_runtime::time::Instant::now(),
        );
        for source in sources {
            self.observe_native_source(&source, false);
        }
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
        if remove_current {
            let _ = self
                .core
                .remove_incarnation(&participant, meta.connection_id);
        }
        self.publish_staged();
        for outcome in outcomes {
            self.publish_track_lifecycle(outcome);
        }
        if remove_current && let Some(endpoint) = meta.endpoint() {
            self.reconcile_peer_sources(endpoint, pulsebeam_runtime::time::Instant::now());
        }
        if meta.authorization.is_some() {
            log::connection_ended(meta.scope.project, meta.connection_id, end);
        }
        if let (Some(native), Some(reason)) = (native, end.native_close())
            && self.begin_native_retirement(participant, meta.clone(), native.handler, reason)
        {
            return;
        }
        self.retire_participant_transport(participant, &meta);
    }

    fn retire_participant_transport(
        &mut self,
        participant: ParticipantId,
        meta: &crate::control::registry::ParticipantMeta,
    ) {
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
                    connection: meta.connection_id,
                    address,
                },
            );
            self.mark_update_touched(meta.shard_id);
            self.publish_staged();
        }
        self.core
            .release_transport(address, pulsebeam_runtime::time::Instant::now());
        self.ownership.queue_retirement(ownership::Incarnation {
            participant,
            connection: meta.connection_id,
            transport: address,
        });
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
                .get_incarnation(work.participant_id, work.connection_id)
                .cloned()
            else {
                continue;
            };
            let Some(lease) = meta.authorization.as_ref() else {
                continue;
            };
            if lease.is_expired_at(wall_now) {
                self.terminate_incarnation(
                    work.participant_id,
                    meta,
                    true,
                    ConnectionEnd::AuthorizationExpired,
                );
            } else if let Ok(lease) = lease.refresh_at(wall_now, runtime_now) {
                let deadline = lease.deadline();
                self.core.registry.update_authorization(
                    &work.participant_id,
                    work.connection_id,
                    lease,
                );
                self.authorization_expiries
                    .insert(AuthorizationExpiryWork { deadline, ..work });
            }
        }
    }
}

fn describe_controller_metrics() {
    crate::control::native::handoff::describe_metrics();
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
                    ownership: None,
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
            ownership: None,
        })
        .await;
}

pub type ControllerSender = mailbox::Sender<ControllerCommand>;

#[cfg(test)]
mod replacement_tests {
    use super::*;
    use crate::{
        control::registry::CommitCandidateError, entity::RoomId, shard::metrics::ShardMetrics,
        shard_update::new_shard_update,
    };

    mod catalog_tests;
    mod permissions;

    impl ControllerActor {
        #[allow(
            clippy::too_many_arguments,
            reason = "test setup supplies the same complete incarnation as the production commit fence"
        )]
        fn commit_reserved_candidate(
            &mut self,
            participant_id: ParticipantId,
            room_id: RoomId,
            shard: ShardId,
            partition_index: usize,
            transport: crate::route::NodeTransportAddress,
            connection_id: ConnectionId,
            authorization: Option<AuthorizationLease>,
            profile: SignalingProtocol,
            wall_now: SystemTime,
        ) -> Result<Option<crate::control::registry::ParticipantMeta>, ControllerError> {
            let room = Self::admission_room(room_id, authorization.as_ref())?;
            self.core.registry.reserve_partition(
                room,
                participant_id,
                connection_id,
                shard,
                partition_index,
            );
            let result = self.commit_candidate(
                participant_id,
                room,
                shard,
                partition_index,
                transport,
                ownership::ClaimRank::root(connection_id),
                authorization,
                profile,
                wall_now,
            );
            self.core
                .registry
                .release_partition(room, participant_id, connection_id);
            result
        }
    }

    fn actor() -> ControllerActor {
        actor_with_shards(1)
    }

    fn actor_with_shards(shard_count: usize) -> ControllerActor {
        actor_with_receivers(shard_count).0
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the production ShardContext owns the sanctioned shared occupancy counters"
    )]
    fn actor_with_receivers(
        shard_count: usize,
    ) -> (ControllerActor, Vec<mailbox::Receiver<ShardCommand>>) {
        let (native_ready_tx, native_ready_rx) =
            mailbox::bounded(const { std::num::NonZeroUsize::new(16_384).unwrap() });
        let (renewal_tx, renewal_rx) = pulsebeam_runtime::sync::mpsc::channel(MAX_PENDING_RENEWALS);
        let (deletion_tx, deletion_rx) = pulsebeam_runtime::sync::mpsc::channel(API_COMMAND_BUDGET);
        let mut contexts = Vec::new();
        let mut updates = Vec::new();
        let mut receivers = Vec::new();
        for index in 0..shard_count {
            let (command_tx, command_rx) =
                mailbox::bounded(const { core::num::NonZeroUsize::new(4).unwrap() });
            receivers.push(command_rx);
            contexts.push(ShardContext {
                command_tx,
                metrics: std::sync::Arc::new(ShardMetrics::new()),
            });
            let (update, _update_rx) = new_shard_update(ShardId::new(index));
            updates.push(update);
        }
        (
            ControllerActor {
                links: None,
                router: crate::control::router::ShardRouter::new(contexts),
                core: ControllerCore::with_shards(shard_count, 1, RoomPartitionPlacement::Balanced),
                placement: placement::Placement::default(),
                execution: None,
                migration: None,
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
                command_backlog: CommandBacklog::new(shard_count),
                authorization_expiries: BTreeSet::new(),
                key_registry: None,
                renewal_tx,
                renewal_rx,
                renewals_inflight: 0,
                deletion_tx,
                deletion_rx,
                deletion_bytes: 0,
                ownership: ownership::Authority::default(),
                peer_ownership: peer_ownership::new_observations(),
                provisioned_peers: Vec::new(),
                ownership_entropy: pulsebeam_runtime::rand::seeded_rng(19),
                receipt_trust: None,
                delivery_budget: creation::DeliveryBudget::new(),
                steering: None,
                materializations: VecDeque::new(),
                native_connections: std::collections::HashMap::new(),
                retiring_native: std::collections::HashMap::new(),
                source_lookup_index: native_service::SourceLookupIndex::default(),
                data_branches: crate::control::native::DataBranchIds::default(),
                catalog: catalog::CatalogRuntime::default(),
                native_ready_tx,
                native_ready_rx,
                native_deadlines: BTreeSet::new(),
            },
            receivers,
        )
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn native_exact_reception_and_discovery_scopes_use_authoritative_source_facts() {
        use crate::control::native::{
            ExecutionAction, ExecutionCompletion, PreparedResource, ReceiverPreparation,
            ResourceExecution, handoff,
        };
        use crate::entity::{ParticipantId, TrackKind};
        use crate::participant::policy::ReceiverExecution;
        use crate::participant::signaling::SignalingProgress;
        use pulsebeam_proto::state::{self as wire, codec};

        for (source_published, same_room) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let (mut actor, _receivers) = actor_with_receivers(2);
            let room = RoomId::new(if same_room {
                "source-authority"
            } else {
                "subscriber-room"
            })
            .unwrap();
            let source_room = RoomId::new("source-authority").unwrap();
            let publisher = ParticipantId::new("server-publisher").unwrap();
            let subscriber = ParticipantId::new("subscriber").unwrap();
            for (participant, connection, shard, partition, participant_room) in [
                (publisher, connection_id(1), 0, 0, source_room),
                (
                    subscriber,
                    connection_id(3),
                    1,
                    usize::from(same_room),
                    room,
                ),
            ] {
                let address = actor.core.reserve_transport(
                    ShardId::new(shard),
                    pulsebeam_runtime::time::Instant::now(),
                );
                actor
                    .core
                    .registry
                    .commit_candidate(
                        participant,
                        ProjectRoomKey::fixture(participant_room),
                        ShardId::new(shard),
                        partition,
                        address,
                        connection,
                        SignalingProtocol::State,
                    )
                    .unwrap();
                actor.core.registry.mark_materialized(&participant);
            }
            let track = crate::control::native::fixture::audio_track(crate::track::TrackMeta {
                room: crate::entity::ProjectRoomKey::fixture(source_room),
                id: connection_id(1).fixture_publication_at(
                    TrackKind::Audio,
                    "private-topic",
                    std::num::NonZeroU64::new(1).unwrap(),
                ),
                origin: publisher,
                connection_id: connection_id(1),
                track_id: Some(crate::entity::TrackId::new("private-topic").unwrap()),
            });
            let publisher_lease = authorization_lease(
                u64::MAX,
                UNIX_EPOCH,
                pulsebeam_runtime::time::Instant::now(),
                "source-authority",
                source_room,
                publisher,
            );
            actor
                .core
                .registry
                .update_authorization(&publisher, connection_id(1), publisher_lease);
            if source_published {
                actor.handle_shard_event((
                    ShardId::new(0),
                    ShardEvent::TrackPublished(track.clone()),
                ));
            }
            let (controller, mut endpoint) =
                handoff::pair(connection_id(3), actor.native_ready_tx.clone());
            let mut native = StateProtocolHandler::new(
                subscriber,
                Some(room),
                Some(crate::control::rtp::capabilities()),
                controller,
            )
            .unwrap();
            native
                .model
                .authorization(
                    i64::MAX,
                    1,
                    crate::control::native::ResourcePermissions::RoomBound,
                )
                .unwrap();
            actor.native_connections.insert(
                connection_id(3),
                NativeService {
                    handler: native,
                    deadline: None,
                },
            );
            assert!(endpoint.opened(handoff::Opened {
                channel: crate::media::ChannelId::from_value(0),
                at: pulsebeam_runtime::time::Instant::now()
            }));
            let request = wire::ClientStateBatch {
                records: vec![wire::ClientStateRecord {
                    owned: 1,
                    execution: None,
                    desired: Some(wire::DesiredState {
                        revision: 1,
                        value: Some(wire::desired_state::Value::Present(Box::new(
                            crate::control::native::fixture::audio_receiver(
                                track.id(),
                                "source-authority",
                            ),
                        ))),
                    }),
                }],
                ..Default::default()
            };
            assert!(endpoint.input(codec::encode_client(&request).unwrap()));
            actor.service_native_connections();
            let output = endpoint.output().unwrap();
            let mut records = codec::decode_server(&output.bytes).unwrap().records;
            if source_published {
                let ExecutionAction::Prepare {
                    work,
                    resource: PreparedResource::Receiver(ReceiverPreparation::Ready(plan)),
                } = endpoint.command().unwrap().into_action()
                else {
                    panic!("authoritative receiver preparation")
                };
                assert_eq!(plan.source.id(), track.id());
                assert_eq!(plan.source.meta().origin, publisher);
                assert_eq!(plan.source.meta().track_id, track.meta().track_id);
                assert!(endpoint.progress(SignalingProgress {
                    sequence: output.sequence,
                    cid: output.cid,
                    at: pulsebeam_runtime::time::Instant::now()
                }));
                assert!(
                    endpoint.fact(handoff::Fact::Completion(ExecutionCompletion {
                        work,
                        result: Ok(ResourceExecution::Receiver(ReceiverExecution {
                            source: track.id(),
                            enabled: true,
                            ready: false,
                            failed: false
                        })),
                        released: false,
                        unix_now: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    }))
                );
                actor.service_native_connections();
                let confirmed = endpoint.output().unwrap();
                let batch = codec::decode_server(&confirmed.bytes).unwrap();
                let live = batch
                    .records
                    .iter()
                    .find_map(|record| match record.confirmed.as_ref()?.value.as_ref()? {
                        wire::confirmed_state::Value::Owned(live) => Some(live),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    live.participant.as_ref().unwrap().participant_id,
                    publisher.as_str()
                );
                assert_eq!(
                    live.participant.as_ref().unwrap().room_id,
                    track.meta().room.room.as_str()
                );
                assert_eq!(live.track, crate::control::native::track_descriptor(&track));
                records.extend(batch.records);
                assert!(endpoint.progress(SignalingProgress {
                    sequence: confirmed.sequence,
                    cid: confirmed.cid,
                    at: pulsebeam_runtime::time::Instant::now(),
                }));
                let native = &mut actor
                    .native_connections
                    .get_mut(&connection_id(3))
                    .unwrap()
                    .handler;
                native
                    .model
                    .authorization(
                        i64::MAX,
                        2,
                        crate::control::native::ResourcePermissions::Denied,
                    )
                    .unwrap();
                assert!(native.handoff.schedule());
                actor.service_native_connections();
                let ExecutionAction::Retire(scope) = endpoint.command().unwrap().into_action()
                else {
                    panic!("permission reduction must dispatch retirement")
                };
                assert_eq!(scope.binding, 1);
                assert_eq!(scope.authorization, 2);
                let authorization = endpoint.output().unwrap();
                let batch = codec::decode_server(&authorization.bytes).unwrap();
                assert!(batch.records.iter().all(|record| !matches!(
                    record.key.as_ref().and_then(|key| key.key.as_ref()),
                    Some(wire::resource_key::Key::Owned(1))
                )));
                records.extend(batch.records);
                assert!(endpoint.progress(SignalingProgress {
                    sequence: authorization.sequence,
                    cid: authorization.cid,
                    at: pulsebeam_runtime::time::Instant::now()
                }));
                assert!(endpoint.fact(handoff::Fact::Resource(
                    crate::participant::policy::ResourceObservation::Released { binding: 1 }
                )));
                actor.service_native_connections();
                let revoked = endpoint.output().unwrap();
                let batch = codec::decode_server(&revoked.bytes).unwrap();
                assert!(batch.records.iter().any(|record| record.confirmed.as_ref().is_some_and(|value| matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::PermissionRevoked as i32))));
                records.extend(batch.records);
                assert!(endpoint.progress(SignalingProgress {
                    sequence: revoked.sequence,
                    cid: revoked.cid,
                    at: pulsebeam_runtime::time::Instant::now()
                }));
                if same_room {
                    let discovery_scope = |discover| {
                        use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind};
                        let mut grants = vec![
                            Grant::new(
                                vec!["source-authority".into()],
                                vec![Action::Join],
                                None,
                                None,
                                None,
                            )
                            .unwrap(),
                        ];
                        if discover {
                            grants.push(
                                Grant::new(
                                    vec!["source-authority".into()],
                                    vec![Action::Discover],
                                    None,
                                    Some(TrackKind::Audio),
                                    Some("private-topic".into()),
                                )
                                .unwrap(),
                            );
                        }
                        crate::control::native::ResourcePermissions::Grants {
                            joined_room: Some(RoomId::new("source-authority").unwrap()),
                            grants: GrantSet::new(grants).unwrap(),
                        }
                    };
                    assert!(
                        actor
                            .native_connections
                            .get_mut(&connection_id(3))
                            .unwrap()
                            .handler
                            .model
                            .authorization(i64::MAX, 3, discovery_scope(true))
                            .unwrap()
                    );
                    assert!(
                        actor.native_connections[&connection_id(3)]
                            .handler
                            .handoff
                            .schedule()
                    );
                    actor.service_native_connections();
                    let output = endpoint.output().unwrap();
                    let state = codec::decode_server(&output.bytes).unwrap();
                    assert!(state.records.iter().all(|record| !matches!(
                        record.key.as_ref().and_then(|key| key.key.as_ref()),
                        Some(wire::resource_key::Key::Publication(_))
                    )));
                    assert!(state.records.iter().all(|record| !matches!(
                        record.key.as_ref().and_then(|key| key.key.as_ref()),
                        Some(wire::resource_key::Key::Participant(id)) if id.participant_id == publisher.as_str()
                    )));
                    assert!(endpoint.command().is_none());
                }
            } else {
                assert!(endpoint.command().is_none());
            }
            assert!(records.iter().all(|record| !matches!(
                record.key.as_ref().and_then(|key| key.key.as_ref()),
                Some(wire::resource_key::Key::Publication(_))
            )));
            assert!(records.iter().all(|record| {
                match record.key.as_ref().and_then(|key| key.key.as_ref()) {
                    Some(wire::resource_key::Key::Participant(id)) => {
                        id.participant_id != publisher.as_str()
                    }
                    _ => true,
                }
            }));
        }
    }

    #[test]
    fn native_handler_rearms_bounded_source_lookup_refresh_until_drained() {
        use crate::control::native::handoff;
        use pulsebeam_proto::state::{self as wire, codec};

        let connection = connection_id(37);
        let (ready_tx, mut ready_rx) = mailbox::bounded(std::num::NonZeroUsize::new(4).unwrap());
        let (handoff, mut endpoint) = handoff::pair(connection, ready_tx);
        let participant = crate::entity::fixture_participant_id();
        let room = RoomId::new("handler-room").unwrap();
        let external = crate::entity::ParticipantId::new("subscriber").unwrap();
        let mut handler =
            StateProtocolHandler::new(participant, Some(room), None, handoff).unwrap();
        handler
            .model
            .authorization(
                10_000,
                1,
                crate::control::native::ResourcePermissions::RoomBound,
            )
            .unwrap();

        for start in (1_u64..=48).step_by(16) {
            let records = (start..start + 16)
                .map(|reference| wire::ClientStateRecord {
                    owned: reference,
                    execution: None,
                    desired: Some(wire::DesiredState {
                        revision: reference,
                        value: Some(wire::desired_state::Value::Present(Box::new(
                            wire::ResourceSpec {
                                settings: Some(wire::ResourceSettings {
                                    enabled: Some(true),
                                    receive: None,
                                }),
                                declaration: Some(wire::resource_spec::Declaration::SourceLookup(
                                    wire::SourceLookupSpec {
                                        source: Some(wire::LogicalSourceAddress {
                                            room: "handler-room".into(),
                                            participant_id: "publisher".into(),
                                            kind: wire::TrackKind::Data as i32,
                                            track_id: "label".into(),
                                        }),
                                    },
                                )),
                            },
                        ))),
                    }),
                })
                .collect();
            let bytes = codec::encode_client(&wire::ClientStateBatch {
                records,
                ..Default::default()
            })
            .unwrap();
            assert!(bytes.len() <= codec::MAX_FRAME_SIZE);
            assert!(handler.model.receive_input(&bytes, 1).unwrap().is_empty());
        }

        assert!(handler.handoff.schedule());
        assert_eq!(ready_rx.try_recv().unwrap(), connection);
        let branches = crate::control::native::DataBranchIds::default();
        handler
            .service(1, &branches, |_| None, |_, _| None, |_| None)
            .unwrap();
        assert!(handler.model.lookup_refresh_pending());
        assert_eq!(ready_rx.try_recv().unwrap(), connection);
        handler
            .service(1, &branches, |_| None, |_, _| None, |_| None)
            .unwrap();
        assert!(!handler.model.lookup_refresh_pending());
        assert!(!handler.model.preparation_pending(1));
        assert!(endpoint.command().is_none());
        assert!(ready_rx.try_recv().is_err());
    }

    #[test]
    fn native_readiness_overflow_retires_without_a_queued_wake_or_stale_deadline() {
        let mut actor = actor();
        let (sender, receiver) = mailbox::bounded(std::num::NonZeroUsize::MIN);
        actor.native_ready_tx = sender;
        actor.native_ready_rx = receiver;
        actor.native_ready_tx.try_send(connection_id(99)).unwrap();
        let participant = crate::entity::fixture_participant_id();
        let connection = connection_id(1);
        let room = RoomId::new("native-overflow").unwrap();
        let external = crate::entity::ParticipantId::new("alice").unwrap();
        let shard = ShardId::new(0);
        let transport = actor
            .core
            .reserve_transport(shard, pulsebeam_runtime::time::Instant::now());
        actor
            .commit_reserved_candidate(
                participant,
                room,
                shard,
                0,
                transport,
                connection,
                None,
                SignalingProtocol::State,
                SystemTime::now(),
            )
            .unwrap();
        let (handoff, endpoint) =
            super::super::native::handoff::pair(connection, actor.native_ready_tx.clone());
        let native = StateProtocolHandler::new(participant, Some(room), None, handoff).unwrap();
        assert!(native.model.deadline().is_none());
        let previous = pulsebeam_runtime::time::Instant::now() + Duration::from_secs(60);
        actor.native_connections.insert(
            connection,
            NativeService {
                handler: native,
                deadline: Some(previous),
            },
        );
        actor.native_deadlines.insert((previous, connection));

        actor.finish_native_observation(connection, Ok(()));
        assert!(endpoint.failed());
        assert_eq!(actor.native_deadlines.len(), 1);
        assert!(!actor.native_deadlines.contains(&(previous, connection)));
        actor.service_native_connections();
        assert!(!actor.native_connections.contains_key(&connection));
        assert!(actor.core.registry.get_participant(&participant).is_none());
        assert!(actor.native_deadlines.is_empty());
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn replacement_and_join_loss_preserve_terminal_output_until_bounded_teardown() {
        use crate::participant::signaling::SignalingProgress;
        use pulsebeam_proto::state::{self, codec};

        for (acknowledge, revoke_join) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            let mut actor = actor();
            let participant = crate::entity::fixture_participant_id();
            let room = RoomId::new("terminal-replacement").unwrap();
            let wall = SystemTime::now();
            let now = pulsebeam_runtime::time::Instant::now();
            let lease = authorization_lease(
                i64::MAX as u64,
                wall,
                now,
                "terminal-replacement",
                room,
                participant,
            );
            let external = crate::entity::ParticipantId::new("alice").unwrap();
            let shard = ShardId::new(0);
            let old = connection_id(1);
            let winner = connection_id(2);
            let transport = actor
                .core
                .reserve_transport(shard, pulsebeam_runtime::time::Instant::now());
            actor
                .commit_reserved_candidate(
                    participant,
                    room,
                    shard,
                    0,
                    transport,
                    old,
                    Some(lease.clone()),
                    SignalingProtocol::State,
                    wall,
                )
                .unwrap();
            let (handoff, mut endpoint) =
                super::super::native::handoff::pair(old, actor.native_ready_tx.clone());
            let mut native =
                StateProtocolHandler::new(participant, Some(room), None, handoff).unwrap();
            native
                .model
                .authorization(
                    lease.native_expiry().unwrap(),
                    lease.revision(),
                    lease.permissions().clone(),
                )
                .unwrap();
            native.model.set_cid(
                crate::media::ChannelId::from_value(0),
                pulsebeam_runtime::time::Instant::now(),
            );
            native
                .service(
                    0,
                    &crate::control::native::DataBranchIds::default(),
                    |_| None,
                    |_, _| None,
                    |_| None,
                )
                .unwrap();
            let initial = endpoint.output().expect("initial session output in flight");
            endpoint.acknowledge_wakeup();
            actor.native_connections.insert(
                old,
                NativeService {
                    handler: native,
                    deadline: None,
                },
            );

            let source = permissions::source(
                room,
                participant,
                old,
                crate::entity::TrackKind::Data,
                "cleanup",
            );
            let outcome = actor
                .lifecycle
                .publish(source, &actor.core.registry, now)
                .unwrap();
            actor.publish_track_lifecycle(outcome);
            assert_eq!(
                actor
                    .lifecycle
                    .sources_in_room(ProjectRoomKey::fixture(room))
                    .count(),
                1
            );
            let reason = if revoke_join {
                use pulsebeam_core::auth::grants::{Action, Grant, GrantSet};
                let mut joined = lease
                    .replacing((*lease.authority).clone(), wall, now)
                    .unwrap();
                joined.permissions = crate::control::permissions::ResourcePermissions::Grants {
                    joined_room: Some(RoomId::new("terminal-replacement").unwrap()),
                    grants: GrantSet::new(vec![
                        Grant::new(
                            vec!["terminal-replacement".into()],
                            vec![
                                Action::Join,
                                Action::Publish,
                                Action::Discover,
                                Action::Subscribe,
                            ],
                            None,
                            None,
                            None,
                        )
                        .unwrap(),
                    ])
                    .unwrap(),
                };
                actor
                    .install_authorization(participant, old, &lease, joined.clone())
                    .unwrap();
                assert_eq!(
                    actor
                        .core
                        .registry
                        .get_participant(&participant)
                        .unwrap()
                        .authorization
                        .as_ref(),
                    Some(&joined)
                );
                assert!(matches!(
                    actor.install_authorization(participant, old, &lease, lease.clone()),
                    Err(ControllerError::GrantGone)
                ));
                assert!(actor.native_connections.contains_key(&old));
                assert_eq!(actor.authorization_expiries.len(), 1);
                let mut next = joined
                    .replacing((*lease.authority).clone(), wall, now)
                    .unwrap();
                next.permissions = crate::control::permissions::ResourcePermissions::Grants {
                    joined_room: Some(RoomId::new("terminal-replacement").unwrap()),
                    grants: pulsebeam_core::auth::grants::GrantSet::default(),
                };
                assert!(matches!(
                    actor.install_authorization(participant, old, &joined, next),
                    Err(ControllerError::AuthorizationRejected)
                ));
                assert!(actor.core.registry.get_participant(&participant).is_none());
                actor.finish_renewal(RenewalResult {
                    participant,
                    connection_id: old,
                    expected_revision: lease.revision(),
                    verified: Some((*lease.authority).clone()),
                });
                assert!(actor.core.registry.get_participant(&participant).is_none());
                assert!(!actor.command_backlog.iter().any(|(_, command)| matches!(
                    command,
                    ShardCommand::AuthorizationResult { .. }
                )));
                CloseReason::AuthorizationRevoked
            } else {
                let replacement = actor.core.reserve_transport(shard, now);
                let previous = actor
                    .commit_reserved_candidate(
                        participant,
                        room,
                        shard,
                        0,
                        replacement,
                        winner,
                        None,
                        SignalingProtocol::State,
                        wall,
                    )
                    .unwrap()
                    .unwrap();
                actor.terminate_incarnation(
                    participant,
                    previous,
                    false,
                    ConnectionEnd::Replaced(None),
                );
                CloseReason::ParticipantReplaced
            };
            assert!(actor.authorization_expiries.is_empty());
            assert_eq!(
                actor
                    .lifecycle
                    .sources_in_room(ProjectRoomKey::fixture(room))
                    .count(),
                0
            );
            assert!(!actor.native_connections.contains_key(&old));
            assert!(actor.retiring_native.contains_key(&old));
            let cleanup_context = actor.native_resource_lease(old).unwrap();
            assert_eq!(cleanup_context.authority.room_id, Some(room));
            assert_eq!(cleanup_context.authority.participant_id, participant);
            assert!(
                !endpoint.failed(),
                "keep the old endpoint alive while handing off terminal output"
            );

            assert!(endpoint.progress(SignalingProgress {
                sequence: initial.sequence,
                cid: initial.cid,
                at: pulsebeam_runtime::time::Instant::now()
            }));
            actor.service_native_connections();
            assert!(
                actor.retiring_native.contains_key(&old),
                "an earlier send is not terminal progress"
            );
            let terminal = endpoint.output().expect("terminal session output");
            let batch = codec::decode_server(&terminal.bytes).unwrap();
            assert!(batch.records.iter().any(|record| matches!(
                record.confirmed.as_ref().and_then(|value| value.value.as_ref()),
                Some(state::confirmed_state::Value::Session(session))
                    if session.closed == Some(reason as i32)
            )));
            if acknowledge {
                assert!(endpoint.progress(SignalingProgress {
                    sequence: terminal.sequence,
                    cid: terminal.cid,
                    at: pulsebeam_runtime::time::Instant::now()
                }));
            }
            actor.service_native_connections();
            assert!(
                !endpoint.failed(),
                "RTC admission must not abort buffered network output"
            );
            assert!(actor.retiring_native.contains_key(&old));
            pulsebeam_runtime::time::advance(Duration::from_millis(100)).await;
            actor.service_native_connections();
            assert!(actor.retiring_native.is_empty());
            assert!(actor.native_resource_lease(old).is_none());
            assert!(actor.native_deadlines.is_empty());
            assert!(endpoint.failed());
            if revoke_join {
                assert!(actor.core.registry.get_participant(&participant).is_none());
            } else {
                assert_eq!(
                    actor
                        .core
                        .registry
                        .get_participant(&participant)
                        .unwrap()
                        .connection_id,
                    winner
                );
                assert!(matches!(
                    actor.install_authorization(participant, old, &lease, lease.clone()),
                    Err(ControllerError::GrantGone)
                ));
                actor.remove_incarnation(participant, old);
                assert_eq!(
                    actor
                        .core
                        .registry
                        .get_participant(&participant)
                        .unwrap()
                        .connection_id,
                    winner
                );
            }
            assert!(actor.authorization_expiries.is_empty());
        }
    }

    #[test]
    fn command_egress_services_ready_destinations_without_reordering_a_blocked_one() {
        let (mut actor, mut receivers) = actor_with_receivers(2);
        let blocked = ShardId::new(0);
        let ready = ShardId::new(1);
        let command = |reference| ShardCommand::PublicationCutoffResult {
            connection_id: connection_id(1),
            reference,
            success: true,
        };
        for reference in 1..=4 {
            actor.router.try_send(blocked, command(reference)).unwrap();
        }
        for (shard, reference) in [(blocked, 5), (ready, 7), (blocked, 6), (ready, 8)] {
            actor.command_backlog.push_back((shard, command(reference)));
        }
        assert!(actor.flush_command_backlog());
        for expected in [7, 8] {
            assert!(
                matches!(receivers[1].try_recv(), Ok(ShardCommand::PublicationCutoffResult { reference, .. }) if reference == expected)
            );
        }
        assert!(receivers[1].try_recv().is_err());
        assert_eq!(actor.command_backlog.len(), 2);
        assert!(
            !actor.flush_command_backlog(),
            "a full destination is not runnable progress"
        );
        for expected in 1..=4 {
            assert!(
                matches!(receivers[0].try_recv(), Ok(ShardCommand::PublicationCutoffResult { reference, .. }) if reference == expected)
            );
        }
        assert!(actor.flush_command_backlog());
        for expected in [5, 6] {
            assert!(
                matches!(receivers[0].try_recv(), Ok(ShardCommand::PublicationCutoffResult { reference, .. }) if reference == expected)
            );
        }
        assert!(actor.command_backlog.is_empty());
        assert_eq!(actor.command_backlog.len(), 0);
    }

    #[test]
    fn pending_materialization_does_not_block_other_completion_or_teardown() {
        let (mut actor, _receivers) = actor_with_receivers(2);
        let room_id = RoomId::new("materializations").unwrap();
        let participants = [
            crate::entity::fixture_participant_id(),
            crate::entity::fixture_participant_id(),
        ];
        let mut acknowledgements = VecDeque::new();
        let mut replies = Vec::new();
        for (index, participant) in participants.into_iter().enumerate() {
            let shard = ShardId::new(index);
            let transport = actor
                .core
                .reserve_transport(shard, pulsebeam_runtime::time::Instant::now());
            let (ack, receiver) = oneshot::channel();
            let (reply, response) = oneshot::channel();
            acknowledgements.push_back(ack);
            replies.push(response);
            let partition_index = actor
                .core
                .next_room_partition(&ProjectRoomKey::fixture(room_id), &participant)
                .0;
            actor.core.registry.reserve_partition(
                ProjectRoomKey::fixture(room_id),
                participant,
                connection_id(1),
                shard,
                partition_index,
            );
            actor.materializations.push_back(MaterializationWork {
                pending: PendingMaterialization {
                    native: None,
                    shard,
                    partition_index: Some(partition_index),
                    command: None,
                    ack: None,
                    participant,

                    claim: ownership::ClaimRank::root(connection_id(1)),
                    authorization: None,
                    profile: SignalingProtocol::Whip,
                    transport,
                    scope: ProjectRoomKey::fixture(room_id).into(),
                    answer: "answer".to_owned(),
                    whep_bindings: Default::default(),
                    owned: None,
                },
                reply: MaterializationReply::Create(reply),
                ack: receiver,
                ready: None,
                awaiting_predecessor_cutoff: false,
                superseded: false,
                cutoffs: MaterializationCutoffs::new(),
                started: pulsebeam_runtime::time::Instant::now(),
            });
        }
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(
            ControllerActor::poll_materializations(&mut actor.materializations, &mut cx)
                .is_pending()
        );
        acknowledgements.pop_back().unwrap().send(true).unwrap();
        assert!(
            ControllerActor::poll_materializations(&mut actor.materializations, &mut cx).is_ready()
        );
        let (command_tx, mut commands) =
            mailbox::bounded(const { core::num::NonZeroUsize::new(4).unwrap() });
        let (_event_tx, mut events) =
            mailbox::bounded(const { core::num::NonZeroUsize::new(4).unwrap() });
        actor.tick(&mut commands, &mut events, false);
        assert_eq!(replies[1].try_recv().unwrap().unwrap().answer, "answer");
        assert!(matches!(
            replies[0].try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(actor.materializations.len(), 1);
        assert!(
            actor
                .core
                .registry
                .get_participant(&participants[0])
                .is_none()
        );
        assert!(
            actor
                .core
                .registry
                .get_participant(&participants[1])
                .is_some()
        );
        command_tx
            .try_send(
                DeleteParticipant {
                    room_id,
                    participant_id: participants[1],
                    connection_id: connection_id(1),
                    profile: SignalingProtocol::Whip,
                }
                .into(),
            )
            .unwrap();
        actor.tick(&mut commands, &mut events, false);
        assert!(
            actor
                .core
                .registry
                .get_participant(&participants[1])
                .is_none()
        );
        drop(acknowledgements);
        actor.tick(&mut commands, &mut events, false);
        assert!(matches!(
            replies[0].try_recv().unwrap(),
            Err(ControllerError::ServiceUnavailable)
        ));
        assert!(actor.materializations.is_empty());
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

    fn full_grants(room_pattern: &str) -> pulsebeam_core::auth::grants::GrantSet {
        serde_json::from_value(serde_json::json!([{"rooms":[room_pattern],"allow":["join","publish","discover","subscribe"]}])).unwrap()
    }

    fn authorization_lease(
        expiry: u64,
        wall_now: SystemTime,
        runtime_now: pulsebeam_runtime::time::Instant,
        _room: &str,
        room_id: RoomId,
        participant_id: ParticipantId,
    ) -> AuthorizationLease {
        AuthorizationLease::from_verified(
            VerifiedAuthorization {
                token_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                grants: full_grants("*"),
                project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
                room_id: Some(room_id),
                participant_id,
                expiry: AuthorizationExpiry::from_unix_seconds(expiry),
            },
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
        let room_id = RoomId::new("delete").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_reserved_candidate(
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
                    "delete",
                    room_id,
                    participant_id,
                )),
                SignalingProtocol::State,
                UNIX_EPOCH,
            )
            .unwrap();
        let authorization = VerifiedAuthorization {
            token_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
            grants: full_grants("*"),
            project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room_id: Some(room_id),
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
        for ack in current.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
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
        for ack in former.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
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
    async fn native_delete_waits_for_late_materialization_and_applied_cleanup() {
        struct NoFrames;
        impl crate::shard::core::IngressTransport for NoFrames {
            fn send_frame(&self, _: ShardId, _: crate::shard::worker::ShardFrame) {
                panic!("admission and cleanup must not forward application frames");
            }
        }

        let now = pulsebeam_runtime::time::Instant::now();
        let (mut actor, _commands) = actor_with_receivers(2);
        actor.core = ControllerCore::with_shards(2, 4, RoomPartitionPlacement::Balanced);
        actor.negotiator = Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
            "127.0.0.1:6000".parse().unwrap(),
        )]);
        actor.updates.clear();
        let mut shards = Vec::new();
        for index in 0..2 {
            let shard = ShardId::new(index);
            let (writer, updates) = new_shard_update(shard);
            actor.updates.push(writer);
            shards.push(crate::shard::core::ShardCore::new(
                shard,
                4,
                2,
                crate::clock::WallAnchor::new(UNIX_EPOCH, now),
                updates,
            ));
        }
        let config = |participant_id, connection_id, room_id, external: &str| {
            let mut resources = crate::control::NegotiatedResources::empty_for_test();
            let handoff =
                crate::control::native::fixture::execution_handoff(&mut resources, connection_id);
            let (rtc, offer) =
                crate::control::negotiator::application_only_test_connection_and_offer();
            (
                Box::new(crate::participant::ConnectionConfig {
                    scope: crate::entity::ProjectRoomKey::fixture(room_id).into(),
                    participant_id,

                    connection_id,
                    profile: SignalingProtocol::State,
                    authorization_expiry: None,
                    rtc,
                    resources,
                }),
                handoff,
                offer,
            )
        };
        let participant = ParticipantId::new("alice").unwrap();
        let room = RoomId::new("late-cutoff").unwrap();
        let connection = connection_id(1);
        let expiry = u64::try_from(i64::MAX).unwrap();
        let lease = authorization_lease(expiry, UNIX_EPOCH, now, "late-cutoff", room, participant);
        let authorization = lease.authority.as_ref().clone();
        let credential = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &full_grants("capability-only"),
            "alice",
            2_000,
        )
        .unwrap()
        .token;
        let registry = KeyRegistry::new(
            *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            vec![pulsebeam_core::auth::KeyRegistryEntry {
                key_id: *pulsebeam_core::auth::DEVELOPMENT_API_KEY_ID,
                public_key: *pulsebeam_core::auth::DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )
        .unwrap();
        let authenticated =
            pulsebeam_core::auth::authenticate_participant_token(&registry, &credential, 1_000)
                .unwrap();
        let wall_now = UNIX_EPOCH + Duration::from_secs(1_000);
        let issue = |actor: &mut ControllerActor, wall_now| {
            let (reply, mut received) = oneshot::channel();
            let command = AdmissionCommand::issue(authenticated.clone(), Some(room), reply);
            assert_eq!(format!("{command:?}"), "AdmissionCommand([redacted])");
            actor.process_admission(command, wall_now);
            received.try_recv().unwrap()
        };
        assert!(matches!(
            issue(&mut actor, wall_now),
            Err(ControllerError::ServiceUnavailable)
        ));
        actor.set_ownership_mode(OwnershipMode::Standalone);
        let (reply, canceled) = oneshot::channel();
        drop(canceled);
        actor.process_admission(
            AdmissionCommand::issue(authenticated.clone(), Some(room), reply),
            wall_now,
        );
        assert!(matches!(
            issue(&mut actor, UNIX_EPOCH + Duration::from_secs(2_000)),
            Err(ControllerError::AuthorizationExpired)
        ));
        let proofs: [ownership::Proof; 4] =
            std::array::from_fn(|_| issue(&mut actor, wall_now).unwrap());
        assert!(matches!(
            issue(&mut actor, wall_now),
            Err(ControllerError::ServiceUnavailable)
        ));
        assert!(actor.core.registry.get_participant(&participant).is_none());
        assert!(actor.materializations.is_empty());
        assert!(actor.command_backlog.is_empty());
        let principal = ownership::PrincipalKey::from_principal(&authenticated, Some(room));
        assert_eq!(
            principal,
            ownership::PrincipalKey::from_verified(&authorization)
        );
        let original = proofs[0];
        let transport = actor.core.reserve_transport(ShardId::new(0), now);
        actor.core.registry.reserve_partition(
            ProjectRoomKey {
                project: authorization.project_id,
                room,
            },
            participant,
            connection,
            ShardId::new(0),
            0,
        );
        let binding = ownership::RequestBinding::for_request(
            SignalingProtocol::State,
            authorization.room_id.as_ref().unwrap(),
            "original offer",
            "original credential",
        )
        .unwrap();
        let one = std::num::NonZeroU64::new(1).unwrap();
        let ownership::Admission::Start(old_ticket) = actor
            .ownership
            .begin(&principal, original, one, binding, now)
            .unwrap()
        else {
            panic!("expected original attempt");
        };
        let incarnation = ownership::Incarnation {
            participant,
            connection,
            transport,
        };
        let mut wrong_scope = authorization.clone();
        wrong_scope.project_id = crate::entity::ProjectId::new();
        assert_eq!(
            actor.ownership.bind(&old_ticket, &wrong_scope, incarnation),
            Err(ownership::Error::InvalidTransition)
        );
        actor
            .ownership
            .bind(&old_ticket, &authorization, incarnation)
            .unwrap();
        assert_eq!(
            actor
                .ownership
                .bind(&old_ticket, &authorization, incarnation),
            Err(ownership::Error::InvalidTransition)
        );
        assert_eq!(
            actor.ownership.commit(&old_ticket, Vec::new(), now),
            Err(ownership::Error::InvalidTransition)
        );
        let (old_config, _old_handoff, native_offer) =
            config(participant, connection, room, "alice");
        let offer = permissions::publishing_offer();
        let (ack, received) = oneshot::channel();
        let (reply, mut old_response) = oneshot::channel();
        actor.start_materialization(RequiredAction::Materialize {
            pending: PendingMaterialization {
                native: None,
                shard: ShardId::new(0),
                partition_index: Some(0),
                command: Some(ShardCommand::MaterializeParticipant {
                    transport,
                    config: old_config,
                    ack,
                }),
                ack: Some(received),
                participant,

                claim: ownership::ClaimRank::root(connection),
                authorization: Some(lease),
                profile: SignalingProtocol::State,
                transport,
                scope: ProjectRoomKey {
                    project: authorization.project_id,
                    room,
                }
                .into(),
                answer: "late answer".to_owned(),
                whep_bindings: Default::default(),
                owned: None,
            },
            reply: MaterializationReply::Create(reply),
        });
        let (owner, old_command) = actor.command_backlog.pop_front().unwrap();
        assert_eq!(owner, ShardId::new(0));

        let other = crate::entity::fixture_participant_id();
        let other_room = RoomId::new("unblocked").unwrap();
        let other_transport = actor.core.reserve_transport(ShardId::new(1), now);
        let (other_config, _other_handoff, _) = config(other, connection_id(10), other_room, "bob");
        let (ack, other_ready) = oneshot::channel();
        shards[1].on_command(
            ShardCommand::MaterializeParticipant {
                transport: other_transport,
                config: other_config,
                ack,
            },
            &NoFrames,
        );
        assert!(other_ready.await.unwrap());
        actor.core.registry.add_participant(
            other,
            other_room,
            ShardId::new(1),
            Some(other_transport),
        );
        assert_eq!(shards[1].participant_count(), 1);

        let next_participant = participant;
        let next_lease = authorization_lease(
            expiry,
            UNIX_EPOCH,
            now,
            room.as_str(),
            room,
            next_participant,
        );
        assert_eq!(
            ownership::PrincipalKey::from_verified(&next_lease.authority),
            principal
        );
        let message = |connection_id, offer: &str| CreateParticipant {
            state: ParticipantState {
                room_id: Some(room),
                participant_id: next_participant,

                connection_id,
                old_connection_id: None,
                authorization: Some(next_lease.clone()),
                selection: super::super::sdp::Selection::Whip(Default::default()),
                profile: SignalingProtocol::Whip,
                rtp_capabilities: None,
            },
            offer: offer.to_owned(),
        };
        let proof = issue(&mut actor, wall_now).unwrap();
        assert_eq!(
            actor
                .ownership
                .preview(&principal, original, one, binding, now),
            Ok(ownership::Admission::Pending(old_ticket.clone()))
        );
        let attempt = |proof, sequence, offer: &str| OwnershipAttempt {
            proof,
            sequence,
            location: axum::http::HeaderValue::from_static("https://edge.test/whip/first"),
            successor_receipt: None,
            binding: ownership::RequestBinding::for_request(
                SignalingProtocol::Whip,
                next_lease.authority.room_id.as_ref().unwrap(),
                offer,
                "next credential",
            )
            .unwrap(),
        };
        let create = |message: CreateParticipant, sequence, reply| {
            let attempt = attempt(proof, sequence, &message.offer);
            AdmissionCommand::create(
                message,
                attempt.proof,
                attempt.sequence,
                attempt.location,
                attempt.binding,
                attempt.successor_receipt,
                reply,
            )
        };
        actor.set_ownership_mode(OwnershipMode::Unavailable);
        assert!(matches!(
            actor.begin_owned_create(
                message(connection_id(3), &offer),
                attempt(proof, one, &offer),
                UNIX_EPOCH
            ),
            Err(OwnedAdmissionError::Request(
                ControllerError::ServiceUnavailable
            ))
        ));
        actor.set_ownership_mode(OwnershipMode::Standalone);
        let unknown = ownership::Proof::parse(&"0".repeat(52)).unwrap();
        assert!(matches!(
            actor.begin_owned_create(
                message(connection_id(3), &offer),
                attempt(unknown, one, &offer),
                UNIX_EPOCH
            ),
            Err(OwnedAdmissionError::Authority(_))
        ));
        assert!(matches!(
            actor.begin_owned_create(
                message(connection_id(3), "invalid SDP"),
                attempt(proof, one, "invalid SDP"),
                UNIX_EPOCH
            ),
            Err(OwnedAdmissionError::Request(_))
        ));
        assert!(matches!(
            actor.begin_owned_create(
                message(connection_id(3), &offer),
                OwnershipAttempt {
                    location: axum::http::HeaderValue::from_bytes(&vec![
                        b'x';
                        ownership::MAX_RESULT_BYTES
                    ],)
                    .unwrap(),
                    ..attempt(proof, one, &offer)
                },
                UNIX_EPOCH,
            ),
            Err(OwnedAdmissionError::Image(creation::Error::TooLarge))
        ));
        assert!(!actor.materializations.front().unwrap().superseded);
        assert_eq!(
            actor
                .ownership
                .preview(&principal, original, one, binding, now),
            Ok(ownership::Admission::Pending(old_ticket.clone()))
        );
        assert!(!actor.core.registry.partition_reserved(
            ProjectRoomKey::fixture(room),
            next_participant,
            connection_id(3),
            ShardId::new(0),
            0
        ));

        let (reply, canceled) = oneshot::channel();
        drop(canceled);
        actor.process_admission(
            create(message(connection_id(3), &offer), one, reply),
            UNIX_EPOCH,
        );
        assert!(!actor.materializations.front().unwrap().superseded);
        assert_eq!(
            actor
                .ownership
                .preview(&principal, original, one, binding, now),
            Ok(ownership::Admission::Pending(old_ticket.clone()))
        );

        let saturated: Vec<_> = (0..creation::MAX_DELIVERIES)
            .map(|_| actor.delivery_budget.reserve().unwrap())
            .collect();
        let (reply, mut busy) = oneshot::channel();
        let command = create(message(connection_id(3), &offer), one, reply);
        assert_eq!(format!("{command:?}"), "AdmissionCommand([redacted])");
        actor.process_admission(command, UNIX_EPOCH);
        assert_eq!(
            busy.try_recv().unwrap().err().unwrap().status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(!actor.materializations.front().unwrap().superseded);
        assert_eq!(
            actor
                .ownership
                .preview(&principal, original, one, binding, now),
            Ok(ownership::Admission::Pending(old_ticket.clone()))
        );
        drop(saturated);
        let (reply, mut unsent_response) = oneshot::channel();
        actor.process_admission(
            create(message(connection_id(3), &offer), one, reply),
            UNIX_EPOCH,
        );
        let first_next = actor
            .materializations
            .back()
            .unwrap()
            .pending
            .owned
            .as_ref()
            .unwrap()
            .ticket
            .clone();
        assert_eq!(
            actor.materializations.back().unwrap().pending.shard,
            ShardId::new(0)
        );
        assert!(
            actor.command_backlog.is_empty(),
            "old physical work blocks new shard commands"
        );
        assert!(
            actor.materializations.front().unwrap().superseded,
            "claim fences pending work before a client close"
        );
        let (reply, mut duplicate_response) = oneshot::channel();
        let duplicate = actor
            .submit_owned_create(
                message(connection_id(99), &offer),
                attempt(proof, one, &offer),
                reply,
                UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(duplicate, first_next);
        let two = std::num::NonZeroU64::new(2).unwrap();
        let (reply, mut next_response) = oneshot::channel();
        let next_ticket = actor
            .submit_owned_create(
                message(connection_id(2), &offer),
                attempt(proof, two, &offer),
                reply,
                UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(
            unsent_response.try_recv().unwrap().unwrap_err().status(),
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(
            duplicate_response.try_recv().unwrap().unwrap_err().status(),
            axum::http::StatusCode::CONFLICT
        );
        let expected_image = actor
            .materializations
            .back()
            .unwrap()
            .pending
            .owned
            .as_ref()
            .unwrap()
            .image
            .clone();
        let mut duplicates = Vec::new();
        for _ in 1..ownership::MAX_WAITERS {
            let (reply, response) = oneshot::channel();
            let ticket = actor
                .submit_owned_create(
                    message(connection_id(99), &offer),
                    attempt(proof, two, &offer),
                    reply,
                    UNIX_EPOCH,
                )
                .unwrap();
            assert_eq!(ticket, next_ticket);
            duplicates.push(response);
        }
        let (reply, _rejected) = oneshot::channel();
        assert!(matches!(
            actor.submit_owned_create(
                message(connection_id(99), &offer),
                attempt(proof, two, &offer),
                reply,
                UNIX_EPOCH,
            ),
            Err((
                _,
                OwnedAdmissionError::Authority(ownership::Error::Capacity)
            ))
        ));
        drop(duplicates.pop().unwrap());
        let (reply, response) = oneshot::channel();
        let ticket = actor
            .submit_owned_create(
                message(connection_id(99), &offer),
                attempt(proof, two, &offer),
                reply,
                UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(ticket, next_ticket);
        duplicates.push(response);
        assert!(actor.command_backlog.is_empty());
        assert_eq!(actor.materializations.len(), 2);
        actor.ownership.retire(&old_ticket, now);
        assert!(actor.ownership.has_incarnation(incarnation));
        assert_eq!(
            actor.ownership.commit(&next_ticket, Vec::new(), now),
            Err(ownership::Error::InvalidTransition)
        );

        let (bytes, acks) = actor
            .begin_native_delete(
                DeleteNative {
                    authorization,
                    connection_id: connection,
                },
                UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(
            acks.len(),
            3,
            "two prior shard fences plus delayed incarnation"
        );
        let (reply, mut response) = oneshot::channel();
        let task = pulsebeam_runtime::task::spawn(complete_native_delete(
            acks,
            NativeDeleteReply::Http(reply),
            actor.deletion_tx.clone(),
            bytes,
        ));
        for shard in &mut shards {
            assert_eq!(shard.apply_updates(1), 1);
        }
        pulsebeam_runtime::task::yield_now().await;
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        actor.advance_materializations();
        assert!(actor.command_backlog.is_empty());

        shards[0].on_command(old_command, &NoFrames);
        assert_eq!(
            shards[0].participant_count(),
            1,
            "early fence did not prevent delayed install"
        );
        actor.advance_materializations();
        assert!(matches!(
            old_response.try_recv().unwrap(),
            Err(ControllerError::Superseded)
        ));
        assert!(actor.command_backlog.is_empty());
        assert_eq!(shards[1].apply_updates(1), 0);
        assert_eq!(shards[0].apply_updates(1), 1);
        assert_eq!(shards[0].participant_count(), 0);
        pulsebeam_runtime::task::yield_now().await;
        assert!(
            matches!(
                response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "removal alone is not its applied receipt"
        );
        assert_eq!(shards[0].apply_updates(1), 1);
        assert!(response.await.unwrap().is_ok());
        task.await.unwrap();
        let completed = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(completed);
        assert_eq!(actor.deletion_bytes, 0);
        assert_eq!(actor.materializations.len(), 1);
        assert!(actor.ownership.has_incarnation(incarnation));
        actor.advance_materializations();
        assert!(
            actor.command_backlog.is_empty(),
            "native close alone does not release authority"
        );

        actor.ownership.start_retirement(&old_ticket);
        actor
            .ownership
            .finish_retirement(&old_ticket, incarnation, false, now);
        actor
            .ownership
            .finish_retirement(&old_ticket, incarnation, true, now);
        actor.ownership.retire(&old_ticket, now);
        assert!(actor.ownership.has_incarnation(incarnation));
        actor.ownership.queue_retirement(incarnation);
        actor.deletion_bytes = PENDING_NATIVE_DELETE_BYTES;
        actor.advance_incarnation_cutoffs();
        assert_eq!(
            actor.ownership.next_retirement(),
            Some((old_ticket.clone(), incarnation))
        );
        actor.deletion_bytes = 0;
        actor.advance_incarnation_cutoffs();
        assert!(actor.deletion_bytes > 0);
        assert!(actor.ownership.next_retirement().is_none());
        assert_eq!(shards[0].apply_updates(1), 1);
        pulsebeam_runtime::task::yield_now().await;
        assert!(matches!(
            actor.deletion_rx.try_recv(),
            Err(pulsebeam_runtime::sync::mpsc::error::TryRecvError::Empty)
        ));
        actor.advance_materializations();
        assert!(
            actor.command_backlog.is_empty(),
            "partial applied proof cannot issue replacement"
        );
        assert_eq!(shards[1].participant_count(), 1);
        assert!(matches!(
            next_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(shards[1].apply_updates(1), 1);
        let completed = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(completed);
        assert!(!actor.ownership.has_incarnation(incarnation));
        assert_eq!(actor.deletion_bytes, 0);
        actor
            .ownership
            .finish_retirement(&old_ticket, incarnation, true, now);
        actor.advance_materializations();
        let (destination, next_command) = actor.command_backlog.pop_front().unwrap();
        assert_eq!(destination, ShardId::new(0));
        assert_eq!(shards[0].participant_count(), 0);
        assert!(matches!(
            next_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            actor.ownership.result(&next_ticket, now),
            Err(ownership::Error::ReplayUnavailable)
        ));
        assert!(!actor.current_incarnation(next_participant, connection_id(2)));
        shards[0].on_command(next_command, &NoFrames);
        assert_eq!(shards[0].participant_count(), 1);
        assert_eq!(shards[1].participant_count(), 1);
        actor.advance_materializations();
        let delivered = next_response.try_recv().unwrap().unwrap();
        assert_eq!(delivered.as_ref(), expected_image);
        for response in &mut duplicates {
            assert_eq!(
                response.try_recv().unwrap().unwrap().as_ref(),
                delivered.as_ref()
            );
        }
        assert_eq!(
            actor.ownership.result(&next_ticket, now).unwrap().as_ref(),
            delivered.as_ref(),
            "the complete image is cached before the first success is visible"
        );
        assert!(actor.materializations.is_empty());
        assert!(actor.current_incarnation(next_participant, connection_id(2)));
        assert!(!actor.current_incarnation(participant, connection));
        let (reply, mut replayed) = oneshot::channel();
        let recovered = actor
            .submit_owned_create(
                message(connection_id(100), &offer),
                attempt(proof, two, &offer),
                reply,
                UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(recovered, next_ticket);
        assert_eq!(
            replayed.try_recv().unwrap().unwrap().as_ref(),
            delivered.as_ref(),
            "cached replay replies in the same actor turn without new physical work"
        );
        let (reply, _expired) = oneshot::channel();
        assert!(matches!(
            actor.submit_owned_create(
                message(connection_id(101), &offer),
                attempt(proof, two, &offer),
                reply,
                UNIX_EPOCH.checked_add(Duration::from_secs(expiry)).unwrap(),
            ),
            Err((
                _,
                OwnedAdmissionError::Request(ControllerError::AuthorizationExpired)
            ))
        ));
        assert!(actor.materializations.is_empty());
        assert!(actor.command_backlog.is_empty());
        assert_eq!(
            actor.ownership.result(&next_ticket, now).unwrap().as_ref(),
            delivered.as_ref()
        );
        assert_eq!(shards[0].participant_count(), 1);
        assert_eq!(shards[1].participant_count(), 1);

        {
            use axum::{
                body::{Body, to_bytes},
                http::{Request, StatusCode},
            };
            use tower::ServiceExt;

            async fn mutate(
                app: axum::Router,
                request: Request<Body>,
                commands: &mut mailbox::Receiver<ControllerCommand>,
                actor: &mut ControllerActor,
            ) -> axum::response::Response {
                let mut response = Box::pin(app.oneshot(request));
                let command = pulsebeam_runtime::select! {
                    command = commands.recv() => command.unwrap(),
                    _ = &mut response => panic!("mutation HTTP response preceded controller processing"),
                };
                assert!(actor.process_command(command).is_none());
                response.await.unwrap()
            }

            let expiry = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 60;
            let key = pulsebeam_server::insecure_dev_api_key().unwrap();
            let issue_token = pulsebeam_server::sign_participant_token(
                &key,
                &full_grants("unselected"),
                "http-native",
                expiry,
            )
            .unwrap()
            .token;
            let claim_token = pulsebeam_server::sign_participant_token(
                &key,
                &full_grants("http-target"),
                "http-native",
                expiry,
            )
            .unwrap()
            .token;
            let (controller, mut commands) =
                mailbox::bounded(const { std::num::NonZeroUsize::new(4).unwrap() });
            let app = super::super::api::owned_native_router(
                controller,
                super::super::api::ApiConfig {
                    base_path: "/api/v1".to_owned(),
                    default_host: "media.example".to_owned(),
                },
                registry.clone(),
            )
            .layer(tower_http::compression::CompressionLayer::new());
            let request = |path: &str, token: &str, body: Vec<u8>| {
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/{path}"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .header("accept-encoding", "zstd")
                    .header("x-forwarded-proto", "https")
                    .body(Body::from(body))
                    .unwrap()
            };
            let mut scoped_request = br#"{"room":"http-target"}"#.to_vec();
            scoped_request.resize(1_024, b' ');
            let mut issuing = Box::pin(app.clone().oneshot(request(
                "native/admissions",
                &issue_token,
                scoped_request,
            )));
            let command = pulsebeam_runtime::select! {
                command = commands.recv() => command.unwrap(),
                _ = &mut issuing => panic!("capability HTTP response preceded controller processing"),
            };
            let ControllerCommand::Admission(command) = command else {
                panic!("expected issuance")
            };
            actor.process_admission(command, SystemTime::now());
            let issued = issuing.await.unwrap();
            assert_eq!(issued.status(), StatusCode::OK);
            assert_eq!(issued.headers()["cache-control"], "no-store");
            assert_eq!(issued.headers()["content-encoding"], "identity");
            assert!(actor.materializations.is_empty());
            assert!(actor.command_backlog.is_empty());
            let issued = to_bytes(issued.into_body(), 1_024).await.unwrap();
            let issued: serde_json::Value = serde_json::from_slice(&issued).unwrap();
            let owner = issued["owner"].as_str().unwrap();
            let body = serde_json::to_vec(&serde_json::json!({
                "profile": pulsebeam_proto::state::PROFILE, "room": "http-target", "offer": native_offer,
            }))
            .unwrap();
            let create = |token: &str| {
                let mut request = request("native", token, body.clone());
                request.headers_mut().insert(
                    "pulsebeam-owner",
                    axum::http::HeaderValue::from_str(owner).unwrap(),
                );
                request.headers_mut().insert(
                    "pulsebeam-attempt",
                    axum::http::HeaderValue::from_static("2"),
                );
                request
            };
            let mut malformed = create(&claim_token);
            malformed.headers_mut().insert(
                "pulsebeam-attempt",
                axum::http::HeaderValue::from_static("1"),
            );
            *malformed.body_mut() = Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "profile": pulsebeam_proto::state::PROFILE,
                    "room": "http-target",
                    "offer": "not SDP",
                }))
                .unwrap(),
            );
            let mut malformed = Box::pin(app.clone().oneshot(malformed));
            let command = pulsebeam_runtime::select! {
                command = commands.recv() => command.unwrap(),
                _ = &mut malformed => panic!("malformed offer response preceded owned admission"),
            };
            let ControllerCommand::Admission(command) = command else {
                panic!("expected owned admission for malformed offer")
            };
            actor.process_admission(command, SystemTime::now());
            let rejected = malformed.await.unwrap();
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            assert!(actor.materializations.is_empty());
            assert!(actor.command_backlog.is_empty());

            let mut creating = Box::pin(app.clone().oneshot(create(&claim_token)));
            let command = pulsebeam_runtime::select! {
                command = commands.recv() => command.unwrap(),
                _ = &mut creating => panic!("creation HTTP response preceded controller processing"),
            };
            let ControllerCommand::Admission(command) = command else {
                panic!("expected owned creation")
            };
            actor.process_admission(command, SystemTime::now());
            let pending = &actor.materializations.back().unwrap().pending;
            let http_ticket = pending.owned.as_ref().unwrap().ticket.clone();
            let http_participant = pending.participant;
            let http_connection = pending.connection_id();
            let expected_answer = pending.answer.clone();
            assert_eq!(pending.profile, SignalingProtocol::State);
            assert!(pending.authorization.is_some());
            assert!(pending.native.is_some());
            assert!(!actor.native_connections.contains_key(&http_connection));
            let principal =
                pulsebeam_core::auth::authenticate_participant_token(&registry, &claim_token, 0)
                    .unwrap();
            let pending_authority = actor
                .authorize_native_resource(
                    NativeResourceRequest {
                        principal,
                        connection_id: http_connection,
                    },
                    SystemTime::now(),
                )
                .unwrap();
            assert_eq!(pending_authority.participant_id, http_participant);
            assert_eq!(
                pending_authority.room_id.as_ref().unwrap().as_str(),
                "http-target"
            );
            actor.advance_materializations();
            assert!(
                futures_lite::future::poll_once(&mut creating)
                    .await
                    .is_none()
            );
            assert!(!actor.current_incarnation(http_participant, http_connection));
            let (destination, command) = actor.command_backlog.pop_front().unwrap();
            shards[destination.index()].on_command(command, &NoFrames);
            actor.advance_materializations();
            let lost_response = creating.await.unwrap();
            assert_eq!(lost_response.status(), StatusCode::CREATED);
            let expected_headers = lost_response.headers().clone();
            drop(lost_response);
            assert!(actor.current_incarnation(http_participant, http_connection));
            let image = actor
                .ownership
                .result(&http_ticket, pulsebeam_runtime::time::Instant::now())
                .unwrap();
            let expected = crate::control::api::creation::into_response(creation::Delivery::new(
                image,
                actor.delivery_budget.reserve().unwrap(),
            ))
            .unwrap();
            let expected_body = to_bytes(expected.into_body(), ownership::MAX_RESULT_BYTES)
                .await
                .unwrap();
            let state: serde_json::Value = serde_json::from_slice(&expected_body).unwrap();
            assert!(state.get("room_external_id").is_none());
            assert_eq!(state["room_id"], "http-target");
            assert!(state.get("participant_external_id").is_none());
            assert_eq!(state["connection_id"], http_connection.to_string());
            assert_eq!(state["participant_id"], http_participant.to_string());
            assert_eq!(state["profile"], pulsebeam_proto::state::PROFILE);
            assert_eq!(
                state["data_delivery"],
                serde_json::json!(["LATEST", "LOSSY", "ORDERED"])
            );
            assert_eq!(
                state["rtp_capabilities"],
                serde_json::to_value(crate::control::rtp::capabilities()).unwrap()
            );
            assert_eq!(state["answer"], expected_answer);
            assert!(
                str0m::change::SdpAnswer::from_sdp_string(state["answer"].as_str().unwrap())
                    .is_ok()
            );
            assert_eq!(expected_headers["content-type"], "application/json");
            assert_eq!(
                expected_headers["location"],
                format!("https://media.example/api/v1/native/{http_connection}")
            );
            let mut rejected_candidate = create(&claim_token);
            rejected_candidate.headers_mut().insert(
                "pulsebeam-attempt",
                axum::http::HeaderValue::from_static("3"),
            );
            let mut replacing = Box::pin(app.clone().oneshot(rejected_candidate));
            let command = pulsebeam_runtime::select! {
                command = commands.recv() => command.unwrap(),
                _ = &mut replacing => panic!("replacement bypassed materialization"),
            };
            let ControllerCommand::Admission(command) = command else {
                panic!("expected owned creation");
            };
            actor.process_admission(command, SystemTime::now());
            let (_, ShardCommand::MaterializeParticipant { ack, .. }) =
                actor.command_backlog.pop_front().unwrap()
            else {
                panic!("expected replacement preparation");
            };
            assert!(actor.current_incarnation(http_participant, http_connection));
            ack.send(false).unwrap();
            actor.advance_materializations();
            assert_eq!(
                replacing.await.unwrap().status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert!(actor.current_incarnation(http_participant, http_connection));
            actor.advance_incarnation_cutoffs();
            for shard in &mut shards {
                shard.apply_updates(usize::MAX);
            }
            let completed = actor.deletion_rx.recv().await.unwrap();
            actor.release_deletion(completed);
            let changed_token = pulsebeam_server::sign_participant_token(
                &key,
                &full_grants("http-target"),
                "http-native",
                expiry + 1,
            )
            .unwrap()
            .token;
            let mut changed_image = create(&claim_token);
            let mut reserialized = body.clone();
            reserialized.push(b' ');
            *changed_image.body_mut() = Body::from(reserialized);
            for (request, status) in [
                (create(&changed_token), StatusCode::BAD_REQUEST),
                (changed_image, StatusCode::BAD_REQUEST),
                (create(&claim_token), StatusCode::CREATED),
                (create(&claim_token), StatusCode::CREATED),
            ] {
                let mut retrying = Box::pin(app.clone().oneshot(request));
                let command = pulsebeam_runtime::select! {
                    command = commands.recv() => command.unwrap(),
                    _ = &mut retrying => panic!("replay HTTP response preceded controller processing"),
                };
                let ControllerCommand::Admission(command) = command else {
                    panic!("expected replay")
                };
                actor.process_admission(command, SystemTime::now());
                assert!(actor.materializations.is_empty());
                assert!(actor.command_backlog.is_empty());
                let replayed = retrying.await.unwrap();
                assert_eq!(replayed.status(), status);
                assert!(actor.current_incarnation(http_participant, http_connection));
                if status != StatusCode::CREATED {
                    continue;
                }
                assert_eq!(replayed.headers(), &expected_headers);
                assert_eq!(
                    to_bytes(replayed.into_body(), ownership::MAX_RESULT_BYTES)
                        .await
                        .unwrap(),
                    expected_body
                );
                assert!(actor.current_incarnation(http_participant, http_connection));
            }

            let mutation = |method: &str, token: &str, connection, tag: Option<&str>| {
                let mut request = Request::builder()
                    .method(method)
                    .uri(format!("/api/v1/native/{connection}"))
                    .header("authorization", format!("Bearer {token}"));
                if let Some(tag) = tag {
                    request = request.header("if-match", tag);
                }
                request.body(Body::empty()).unwrap()
            };
            let other_participant = pulsebeam_server::sign_participant_token(
                &key,
                &full_grants("http-target"),
                "http-other",
                expiry,
            )
            .unwrap()
            .token;
            let credit = actor.deletion_bytes;
            for (method, token, connection, tag, status) in [
                (
                    "PATCH",
                    &other_participant,
                    http_connection,
                    Some("\"auth-1\""),
                    StatusCode::FORBIDDEN,
                ),
                (
                    "DELETE",
                    &other_participant,
                    http_connection,
                    None,
                    StatusCode::FORBIDDEN,
                ),
                (
                    "DELETE",
                    &claim_token,
                    ConnectionId::new(),
                    None,
                    StatusCode::GONE,
                ),
                (
                    "PATCH",
                    &changed_token,
                    http_connection,
                    Some("\"auth-9\""),
                    StatusCode::PRECONDITION_FAILED,
                ),
            ] {
                let response = mutate(
                    app.clone(),
                    mutation(method, token, connection, tag),
                    &mut commands,
                    &mut actor,
                )
                .await;
                assert_eq!(response.status(), status);
                assert!(
                    actor.current_incarnation(http_participant, http_connection),
                    "rejected {method} {tag:?} for {connection} removed the accepted incarnation"
                );
                assert_eq!(actor.deletion_bytes, credit);
                assert!(actor.command_backlog.is_empty());
            }
            let accepted = mutate(
                app.clone(),
                mutation("PATCH", &changed_token, http_connection, Some("\"auth-1\"")),
                &mut commands,
                &mut actor,
            )
            .await;
            assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
            assert_eq!(accepted.headers()["etag"], "\"auth-2\"");
            assert_eq!(
                actor
                    .native_resource_lease(http_connection)
                    .unwrap()
                    .revision(),
                2
            );
            while let Some((destination, command)) = actor.command_backlog.pop_front() {
                shards[destination.index()].on_command(command, &NoFrames);
            }
            let stale = mutate(
                app.clone(),
                mutation("PATCH", &changed_token, http_connection, Some("\"auth-1\"")),
                &mut commands,
                &mut actor,
            )
            .await;
            assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
            assert_eq!(stale.headers()["etag"], "\"auth-2\"");

            let catalog_version = actor.core.registry.catalog_version();
            let mut unjoined = Vec::new();
            for _ in 0..2 {
                let mut issuing = Box::pin(app.clone().oneshot(request(
                    "native/admissions",
                    &issue_token,
                    b"{}".to_vec(),
                )));
                let command = pulsebeam_runtime::select! {
                    command = commands.recv() => command.unwrap(),
                    _ = &mut issuing => panic!("roomless ticket bypassed the controller"),
                };
                let ControllerCommand::Admission(command) = command else {
                    panic!("expected issuance")
                };
                actor.process_admission(command, SystemTime::now());
                let issued = issuing.await.unwrap();
                assert_eq!(issued.status(), StatusCode::OK);
                let issued: serde_json::Value =
                    serde_json::from_slice(&to_bytes(issued.into_body(), 1_024).await.unwrap())
                        .unwrap();
                let mut create = request(
                    "native",
                    &issue_token,
                    serde_json::to_vec(&serde_json::json!({
                        "profile": pulsebeam_proto::state::PROFILE, "offer": native_offer,
                    }))
                    .unwrap(),
                );
                create.headers_mut().insert(
                    "pulsebeam-owner",
                    issued["owner"].as_str().unwrap().parse().unwrap(),
                );
                create.headers_mut().insert(
                    "pulsebeam-attempt",
                    axum::http::HeaderValue::from_static("1"),
                );
                let mut creating = Box::pin(app.clone().oneshot(create));
                let command = pulsebeam_runtime::select! {
                    command = commands.recv() => command.unwrap(),
                    response = &mut creating => panic!("roomless bootstrap returned {} before controller placement", response.unwrap().status()),
                };
                let ControllerCommand::Admission(command) = command else {
                    panic!("expected admission")
                };
                actor.process_admission(command, SystemTime::now());
                actor.advance_materializations();
                let (destination, command) = actor.command_backlog.pop_front().unwrap();
                shards[destination.index()].on_command(command, &NoFrames);
                actor.advance_materializations();
                let created = creating.await.unwrap();
                assert_eq!(created.status(), StatusCode::CREATED);
                let body = to_bytes(created.into_body(), ownership::MAX_RESULT_BYTES)
                    .await
                    .unwrap();
                let state: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(state.get("room_id").is_none());
                assert_eq!(state["participant_id"], http_participant.to_string());
                let connection = state["connection_id"]
                    .as_str()
                    .unwrap()
                    .parse::<ConnectionId>()
                    .unwrap();
                let (_, meta) = actor.core.registry.get_connection(connection).unwrap();
                assert_eq!(meta.endpoint(), None);
                assert!(actor.native_resource_lease(connection).is_some());
                unjoined.push(connection);
            }
            assert_ne!(unjoined[0], unjoined[1]);
            assert!(
                unjoined
                    .iter()
                    .all(|connection| actor.current_incarnation(http_participant, *connection))
            );
            assert!(actor.current_incarnation(http_participant, http_connection));
            assert_eq!(actor.core.registry.catalog_version(), catalog_version);

            let mut deleting =
                Box::pin(app.oneshot(mutation("DELETE", &claim_token, http_connection, None)));
            let command = pulsebeam_runtime::select! {
                command = commands.recv() => command.unwrap(),
                _ = &mut deleting => panic!("delete HTTP response preceded controller processing"),
            };
            assert!(actor.process_command(command).is_none());
            assert!(!actor.current_incarnation(http_participant, http_connection));
            assert!(
                futures_lite::future::poll_once(&mut deleting)
                    .await
                    .is_none()
            );
            while shards[0].apply_updates(64) != 0 {}
            pulsebeam_runtime::task::yield_now().await;
            assert!(
                futures_lite::future::poll_once(&mut deleting)
                    .await
                    .is_none()
            );
            while shards[1].apply_updates(64) != 0 {}
            assert_eq!(deleting.await.unwrap().status(), StatusCode::NO_CONTENT);
            let completed = actor.deletion_rx.recv().await.unwrap();
            actor.release_deletion(completed);
            assert_eq!(actor.deletion_bytes, credit);
        }
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
        let mut actor = actor_with_shards(3);
        actor.updates.clear();
        let mut receivers = Vec::new();
        for index in 0..3 {
            let (writer, receiver) = new_shard_update(ShardId::new(index));
            actor.updates.push(writer);
            receivers.push(receiver);
        }
        let room_id = RoomId::new("owned-cutoff").unwrap();
        let publisher = crate::entity::fixture_participant_id();
        let subscriber = crate::entity::fixture_participant_id();
        let mut endpoints = Vec::new();
        for (participant, shard) in [(publisher, 0), (subscriber, 1)] {
            let connection = if shard == 0 {
                connection_id(1)
            } else {
                connection_id(3)
            };
            let address = actor
                .core
                .reserve_transport(ShardId::new(shard), pulsebeam_runtime::time::Instant::now());
            actor
                .core
                .registry
                .commit_candidate(
                    participant,
                    ProjectRoomKey::fixture(room_id),
                    ShardId::new(shard),
                    shard,
                    address,
                    connection,
                    SignalingProtocol::State,
                )
                .unwrap();
            actor.core.registry.mark_materialized(&participant);
            let (handoff, endpoint) =
                super::super::native::handoff::pair(connection, actor.native_ready_tx.clone());
            let external = crate::entity::ParticipantId::new(&format!("cutoff-{shard}")).unwrap();
            let handler =
                StateProtocolHandler::new(participant, Some(room_id), None, handoff).unwrap();
            actor.native_connections.insert(
                connection,
                NativeService {
                    handler,
                    deadline: None,
                },
            );
            endpoints.push(endpoint);
        }
        let make_track = |reference| {
            crate::track::Track::data(
                crate::track::TrackMeta {
                    room: crate::entity::ProjectRoomKey::fixture(room_id),
                    id: connection_id(1).fixture_publication_at(
                        crate::entity::TrackKind::Data,
                        "topic",
                        std::num::NonZeroU64::new(reference).unwrap(),
                    ),
                    origin: publisher,
                    connection_id: connection_id(1),
                    track_id: Some(crate::entity::TrackId::new("topic").unwrap()),
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
                room_id: ProjectRoomKey::fixture(room_id),
                subscriber,
                connection_id: connection_id(3),
                selector: TrackSelector::track(first.id()),
                selection: SelectionPolicy::All,
            },
        ));
        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(first.clone())));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        let control = actor.native_connections.remove(&connection_id(1)).unwrap();
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::OwnedTrackUnpublished {
                meta: first.meta().clone(),
                reference: 1,
            },
        ));
        assert_eq!(actor.deletion_bytes, 0);
        actor.native_connections.insert(connection_id(1), control);
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
        for ack in origin.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
        pulsebeam_runtime::task::yield_now().await;
        assert!(actor.deletion_rx.try_recv().is_err());
        for ack in destination.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
        let completed = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(completed);
        assert!(
            matches!(actor.command_backlog.pop_front(), Some((shard, ShardCommand::PublicationCutoffResult { connection_id: connection, reference: 1, success: true })) if shard == ShardId::new(0) && connection == connection_id(1))
        );
        let released = actor.deletion_rx.recv().await.unwrap();
        actor.release_deletion(released);
        assert_eq!(actor.deletion_bytes, 0);

        let received = make_track(2);
        actor.handle_shard_event((
            ShardId::new(0),
            ShardEvent::TrackPublished(received.clone()),
        ));
        actor.handle_shard_event((
            ShardId::new(1),
            ShardEvent::OwnedSubscriptionSet {
                connection_id: connection_id(3),
                request: super::super::topology::OwnedSubscription {
                    source_room: ProjectRoomKey::fixture(room_id),
                    subscriber,
                    reference: 2,
                    track: received.id(),
                    enabled: true,
                },
            },
        ));
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        actor.handle_shard_event((
            ShardId::new(1),
            ShardEvent::OwnedSubscriptionRemoved {
                source_room: ProjectRoomKey::fixture(room_id),
                subscriber,
                connection_id: connection_id(3),
                reference: 2,
            },
        ));
        let mut origin = receivers[0].try_recv().unwrap();
        let mut destination = receivers[1].try_recv().unwrap();
        assert!(receivers[2].try_recv().is_err());
        for ack in origin.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
        pulsebeam_runtime::task::yield_now().await;
        assert!(actor.deletion_rx.try_recv().is_err());
        for ack in destination.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
        for _ in 0..2 {
            let completed = actor.deletion_rx.recv().await.unwrap();
            actor.release_deletion(completed);
        }
        assert!(
            matches!(actor.command_backlog.pop_front(), Some((shard, ShardCommand::PublicationCutoffResult { connection_id: connection, reference: 2, success: true })) if connection == connection_id(3) && shard == ShardId::new(1))
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
                    EndpointKey {
                        room: rejected.meta().room,
                        participant: publisher
                    },
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
                ProjectRoomKey::fixture(room_id),
                ShardId::new(0),
                0,
                address,
                connection_id(2),
                SignalingProtocol::State,
            )
            .unwrap();
        for receiver in &mut receivers {
            while let Ok(mut update) = receiver.try_recv() {
                for ack in update.cutoff_acks.drain(..) {
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
        let participant = crate::entity::fixture_participant_id();
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
        let room_id = RoomId::new("expired-prepare").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(
            10,
            UNIX_EPOCH,
            runtime_now,
            "expiry",
            room_id,
            participant_id,
        );

        let result = actor.commit_reserved_candidate(
            participant_id,
            room_id,
            ShardId::new(0),
            0,
            transport,
            connection_id(1),
            Some(lease),
            SignalingProtocol::State,
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
        let room_id = RoomId::new("expiry").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let lease = authorization_lease(
            10,
            UNIX_EPOCH,
            runtime_now,
            "expiry",
            room_id,
            participant_id,
        );
        actor
            .commit_reserved_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection_id(1),
                Some(lease),
                SignalingProtocol::State,
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
    fn correlation_follows_committed_connections_and_changed_credentials() {
        use std::io::{Read, Seek};

        let key = pulsebeam_server::insecure_dev_api_key().unwrap();
        let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
        let registry = KeyRegistry::new(
            project,
            vec![pulsebeam_core::auth::KeyRegistryEntry {
                key_id: *pulsebeam_core::auth::DEVELOPMENT_API_KEY_ID,
                public_key: *pulsebeam_core::auth::DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )
        .unwrap();
        let now = SystemTime::now();
        let unix_now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let issue = |participant, expiry| {
            pulsebeam_server::sign_participant_token(&key, &full_grants("*"), participant, expiry)
                .unwrap()
        };
        let first = issue("correlation-subject", unix_now + 60);
        let renewed = issue("correlation-subject", unix_now + 120);
        let rejected = issue("not-the-subject", unix_now + 120);
        let authenticate = |token: &str| {
            pulsebeam_core::auth::authenticate_participant_token(&registry, token, unix_now)
                .unwrap()
        };
        let principal = authenticate(&first.token);
        let participant = *principal.participant_id();
        let room = RoomId::new("correlation-room").unwrap();
        let connection = connection_id(1);
        let other_connection = connection_id(2);
        let mut actor = actor();
        let mut trace = tempfile::tempfile().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(trace.try_clone().unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for (room, conn) in [
                (room, connection),
                (RoomId::new("other-room").unwrap(), other_connection),
            ] {
                let authorized = principal.clone().authorize_room(room).unwrap();
                let runtime_now = pulsebeam_runtime::time::Instant::now();
                let lease =
                    AuthorizationLease::from_verified(authorized, now, runtime_now).unwrap();
                let shard = ShardId::new(0);
                let transport = actor.core.reserve_transport(shard, runtime_now);
                actor
                    .commit_reserved_candidate(
                        participant,
                        room,
                        shard,
                        0,
                        transport,
                        conn,
                        Some(lease.clone()),
                        SignalingProtocol::State,
                        now,
                    )
                    .unwrap();
                assert!(
                    actor
                        .commit_reserved_candidate(
                            participant,
                            room,
                            shard,
                            0,
                            transport,
                            conn,
                            Some(lease),
                            SignalingProtocol::State,
                            now
                        )
                        .is_err()
                );
            }
            let mut finish = |token: &str| {
                let verified = authenticate(token).authorize_room(room).unwrap();
                actor.finish_renewal(RenewalResult {
                    participant,
                    connection_id: connection,
                    expected_revision: 1,
                    verified: Some(verified),
                });
            };
            finish(&rejected.token);
            finish(&renewed.token);
            finish(&renewed.token);
            let verified = authenticate(&renewed.token).authorize_room(room).unwrap();
            actor
                .replace_authorization(
                    ReplaceAuthorization {
                        authorization: verified,
                        connection_id: connection,
                        expected_revision: 2,
                    },
                    now,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .unwrap();
        });
        trace.rewind().unwrap();
        let mut output = String::new();
        trace.read_to_string(&mut output).unwrap();
        let events: Vec<_> = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|value| {
                matches!(
                    value["fields"]["event"].as_str(),
                    Some("connection_admitted" | "credential_renewed")
                )
            })
            .collect();
        assert_eq!(events.len(), 3);
        for (event, (name, conn, id)) in events.iter().zip([
            ("connection_admitted", connection, first.jti),
            ("connection_admitted", other_connection, first.jti),
            ("credential_renewed", connection, renewed.jti),
        ]) {
            assert!(event["timestamp"].is_string());
            let fields = event["fields"].as_object().unwrap();
            assert_eq!(fields.len(), 5);
            assert_eq!(fields["event"], name);
            assert_eq!(fields["project"], project.as_str());
            assert_eq!(fields["conn"], conn.to_string());
            assert_eq!(fields["jti"], id.to_string());
        }
    }

    #[test]
    fn renewal_rejects_identity_changes_but_removing_join_revokes_participation() {
        let mut actor = actor();
        let room_id = RoomId::new("renewal-room").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let connection = connection_id(1);
        let now = SystemTime::now();
        let unix_now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let old_expiry = unix_now + 60;
        let old_lease = authorization_lease(
            old_expiry,
            now,
            pulsebeam_runtime::time::Instant::now(),
            "renewal-room",
            room_id,
            participant_id,
        );
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_reserved_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection,
                Some(old_lease.clone()),
                SignalingProtocol::State,
                now,
            )
            .unwrap();
        let verified = VerifiedAuthorization {
            token_id: "00000000-0000-4000-8000-000000000002".parse().unwrap(),
            grants: full_grants("*"),
            project_id: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room_id: Some(room_id),
            participant_id,
            expiry: AuthorizationExpiry::from_unix_seconds(unix_now + 120),
        };
        let mut wrong_identity = verified.clone();
        wrong_identity.participant_id = crate::entity::ParticipantId::new("mallory").unwrap();
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 1,
            verified: Some(wrong_identity),
        });
        let mut wrong_project = verified.clone();
        wrong_project.project_id = pulsebeam_core::identity::ProjectId::new();
        let mut wrong_room_name = verified.clone();
        wrong_room_name.room_id = Some(RoomId::new("other-room").unwrap());
        for invalid in [wrong_project, wrong_room_name] {
            actor.finish_renewal(RenewalResult {
                participant: participant_id,
                connection_id: connection,
                expected_revision: 1,
                verified: Some(invalid),
            });
        }
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
        let accepted = actor
            .core
            .registry
            .get_incarnation(participant_id, connection)
            .unwrap()
            .authorization
            .as_ref()
            .unwrap();
        assert_eq!(accepted.expiry().unix_seconds(), unix_now + 120);
        assert_eq!(accepted.authority.token_id, verified.token_id);
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
            .clone()
            .unwrap();
        assert_eq!(current.revision(), 3);
        assert_eq!(current.expiry(), narrowed.expiry);
        assert_eq!(current.authority.as_ref(), &narrowed);
        let refreshed = current
            .refresh_at(now, pulsebeam_runtime::time::Instant::now())
            .unwrap();
        assert_eq!(refreshed.authority.as_ref(), &narrowed);
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
            verified: Some(verified.clone()),
        });
        assert_eq!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .unwrap()
                .authorization,
            Some(current.clone())
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
            Some(current.clone())
        );
        assert_eq!(actor.command_backlog.len(), queued);
        for invalid in [
            {
                let mut value = request();
                value.authorization.project_id = pulsebeam_core::identity::ProjectId::new();
                value
            },
            {
                let mut value = request();
                value.authorization.room_id = Some(RoomId::new("other-room").unwrap());
                value
            },
        ] {
            assert!(matches!(
                actor.replace_authorization(invalid, now, pulsebeam_runtime::time::Instant::now()),
                Err(ControllerError::AuthorizationRejected)
            ));
        }
        let mut wrong = request();
        wrong.expected_revision = 3;
        wrong.authorization.room_id = Some(RoomId::new("other-room").unwrap());
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
        let mut exhausted = current.clone();
        exhausted.revision = u64::MAX;
        assert!(matches!(
            exhausted.replacing(
                narrowed.clone(),
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

        let mut no_join = verified;
        no_join.grants = full_grants("elsewhere");
        no_join.expiry = AuthorizationExpiry::from_unix_seconds(unix_now + 180);
        actor.finish_renewal(RenewalResult {
            participant: participant_id,
            connection_id: connection,
            expected_revision: 3,
            verified: Some(no_join),
        });
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none()
        );
        assert!(matches!(
            &actor.command_backlog.back().unwrap().1,
            ShardCommand::AuthorizationResult { expiry: None, .. }
        ));
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
        let room = RoomId::new("renewal-room").unwrap();
        let external = crate::entity::ParticipantId::new("alice").unwrap();
        let room_id = room;
        let participant = external;
        let connection = connection_id(1);
        let now = SystemTime::now();
        let unix_now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let previous = authorization_lease(
            unix_now + 60,
            now,
            pulsebeam_runtime::time::Instant::now(),
            "renewal-room",
            room_id,
            participant,
        );
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .commit_reserved_candidate(
                participant,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection,
                Some(previous),
                SignalingProtocol::State,
                now,
            )
            .unwrap();
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &full_grants((room).as_str()),
            (external).as_str(),
            unix_now + 120,
        )
        .unwrap()
        .token;
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
                .as_ref()
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
    fn remote_receipt_needs_a_local_attempt_and_fresh_authority() {
        use ownership::{Error, RequestBinding, receipt::tests::offline_issuer_fixture};
        use std::num::NonZeroU64;

        let now = pulsebeam_runtime::time::Instant::now();
        let wall_now = UNIX_EPOCH + Duration::from_secs(1000);
        let room = RoomId::new("offline-issuer").unwrap();
        let participant = ParticipantId::new("alice").unwrap();
        let lease = authorization_lease(2000, wall_now, now, room.as_str(), room, participant);
        let endpoint = EndpointKey {
            room: ProjectRoomKey {
                project: lease.authority.project_id,
                room,
            },
            participant,
        };
        let predecessor = ownership::ClaimRank::root(connection_id(10));
        let candidate = connection_id(1);
        let (receipt, receiving_trust) = offline_issuer_fixture(endpoint, predecessor, 1000, 1030);
        let mut issuer = actor();
        let mut receiver = actor();
        receiver.ownership_entropy = pulsebeam_runtime::rand::seeded_rng(43);
        receiver.receipt_trust = Some(ownership::receipt::Authority::Local(receiving_trust));
        receiver.negotiator = Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
            "127.0.0.1:6000".parse().unwrap(),
        )]);
        let principal = ownership::PrincipalKey::from_verified(&lease.authority);
        let issuer_attempt = issuer
            .ownership
            .issue(principal.clone(), now, &mut issuer.ownership_entropy)
            .unwrap();
        let receiver_attempt = receiver
            .ownership
            .issue(principal, now, &mut receiver.ownership_entropy)
            .unwrap();
        let (_, offer) = crate::control::negotiator::application_only_test_connection_and_offer();
        let message = || CreateParticipant {
            state: ParticipantState {
                room_id: Some(room),
                participant_id: participant,
                connection_id: candidate,
                old_connection_id: None,
                authorization: Some(lease.clone()),
                selection: super::super::sdp::Selection::Native,
                profile: SignalingProtocol::State,
                rtp_capabilities: Some(crate::control::rtp::capabilities()),
            },
            offer: offer.clone(),
        };
        let attempt = |proof, body: &[u8]| OwnershipAttempt {
            proof,
            sequence: NonZeroU64::MIN,
            binding: RequestBinding::for_native(body, "credential").unwrap(),
            location: axum::http::HeaderValue::from_static("https://receiver.test/native/resource"),
            successor_receipt: Some(receipt.proof.clone()),
        };
        drop(issuer);
        assert!(matches!(
            receiver.begin_owned_create(message(), attempt(issuer_attempt, b"request"), wall_now),
            Err(OwnedAdmissionError::Authority(Error::UnknownProof))
        ));
        let OwnedPreparation::Start(_, pending) = receiver
            .begin_owned_create(message(), attempt(receiver_attempt, b"request"), wall_now)
            .unwrap()
        else {
            panic!("the receiving node must reserve a distinct attempt");
        };
        assert_eq!(pending.claim, predecessor.successor(candidate).unwrap());
        assert!(matches!(
            receiver.begin_owned_create(
                message(),
                attempt(receiver_attempt, b"changed request"),
                wall_now
            ),
            Err(OwnedAdmissionError::Authority(Error::ChangedAttempt))
        ));
        assert!(matches!(
            receiver.validate_pending_successor(
                &pending,
                pending.owned.as_ref(),
                wall_now + Duration::from_secs(31)
            ),
            Err(ControllerError::RecoveryUnavailable)
        ));
        assert!(receiver.core.registry.get_endpoint(endpoint).is_none());
        receiver.discard_materialization(*pending, MaterializationCutoffs::new());
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn current_peer_snapshot_retires_only_the_local_loser() {
        use ownership::observations::Error;
        use std::num::{NonZeroU64, NonZeroUsize};

        let wall_now = SystemTime::now();
        let now = pulsebeam_runtime::time::Instant::now();
        let expiry = wall_now.duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
        let room = RoomId::new("peer-reconciliation").unwrap();
        let participant = crate::entity::fixture_participant_id();
        let admit = |actor: &mut ControllerActor, conn| {
            let lease = authorization_lease(
                expiry,
                wall_now,
                now,
                "peer-reconciliation",
                room,
                participant,
            );
            let shard = ShardId::new(0);
            let address = actor.core.reserve_transport(shard, now);
            actor
                .commit_reserved_candidate(
                    participant,
                    room,
                    shard,
                    0,
                    address,
                    conn,
                    Some(lease),
                    SignalingProtocol::State,
                    wall_now,
                )
                .unwrap();
        };
        let mut left = actor_with_shards(2);
        let mut right = actor();
        let mut link =
            pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0)).unwrap();
        let (source_port, _source_frames) = link.create_port().unwrap();
        let (subscriber_port, _subscriber_frames) = link.create_port().unwrap();
        left.links = Some(crate::control::link::Links::new(
            link,
            vec![source_port, subscriber_port],
            now,
        ));
        admit(&mut left, connection_id(1));
        admit(&mut right, connection_id(2));
        let subscriber = ParticipantId::new("receiver").unwrap();
        let subscriber_connection = connection_id(3);
        let subscriber_shard = ShardId::new(1);
        let address = left.core.reserve_transport(subscriber_shard, now);
        left.commit_reserved_candidate(
            subscriber,
            room,
            subscriber_shard,
            1,
            address,
            subscriber_connection,
            None,
            SignalingProtocol::State,
            wall_now,
        )
        .unwrap();
        let (writer, reader) = new_shard_update(subscriber_shard);
        left.updates[1] = writer;
        let mut route_shard = crate::shard::core::ShardCore::new(
            subscriber_shard,
            4,
            2,
            crate::clock::WallAnchor::new(wall_now, now),
            reader,
        );
        struct NoFrames;
        impl crate::shard::router::IngressTransport for NoFrames {
            fn send_frame(&self, _: ShardId, _: crate::shard::worker::ShardFrame) {
                panic!("materialization must not send frames");
            }
        }
        let mut resources = crate::control::NegotiatedResources::empty_for_test();
        let _handoff = crate::control::native::fixture::execution_handoff(
            &mut resources,
            subscriber_connection,
        );
        let (ack, received) = pulsebeam_runtime::sync::oneshot::channel();
        route_shard.on_command(
            ShardCommand::MaterializeParticipant {
                transport: address,
                config: Box::new(crate::participant::ConnectionConfig {
                    scope: ProjectRoomKey::fixture(room).into(),
                    participant_id: subscriber,
                    connection_id: subscriber_connection,
                    profile: SignalingProtocol::State,
                    authorization_expiry: None,
                    rtc: crate::control::application_only_test_connection(),
                    resources,
                }),
                ack,
            },
            &NoFrames,
        );
        assert!(received.await.unwrap());
        let shutdown = CancellationToken::new();
        let shard = ShardId::new(0);
        let start = |mut actor: ControllerActor| {
            let (update_writer, updates) = new_shard_update(shard);
            actor.updates[0] = update_writer;
            let (command_tx, command_rx) = mailbox::bounded(NonZeroUsize::MIN);
            let (event_tx, event_rx) = mailbox::bounded(NonZeroUsize::new(4).unwrap());
            let task =
                pulsebeam_runtime::task::spawn(actor.run(command_rx, event_rx, shutdown.clone()));
            (
                peer_ownership::Control::new(command_tx),
                event_tx,
                updates,
                task,
            )
        };
        let (left, left_events, mut left_updates, left_task) = start(left);
        let (right, right_events, mut right_updates, right_task) = start(right);
        let left_view = left.read().await.unwrap();
        let right_view = right.read().await.unwrap();
        let [(endpoint, low)] = left_view.as_ref() else {
            panic!("one live left claim");
        };
        let [(same_endpoint, high)] = right_view.as_ref() else {
            panic!("one live right claim");
        };
        assert_eq!(endpoint, same_endpoint);
        assert!(low.claim < high.claim);
        let endpoint = *endpoint;
        let publish = |events: &mailbox::Sender<ShardEventMessage>, connection: ConnectionId| {
            let publication = connection.fixture_publication_at(
                crate::entity::TrackKind::Data,
                "feed",
                NonZeroU64::MIN,
            );
            let track = crate::track::Track::data_publication(
                crate::track::TrackMeta {
                    room: endpoint.room,
                    id: publication,
                    origin: participant,
                    connection_id: connection,
                    track_id: Some(crate::entity::TrackId::new("feed").unwrap()),
                },
                crate::track::DataLane::Ordered,
            );
            assert!(
                events
                    .try_send((shard, ShardEvent::TrackPublished(track)))
                    .is_ok()
            );
            publication
        };
        let low_publication = publish(&left_events, connection_id(1));
        let high_publication = publish(&right_events, connection_id(2));
        assert!(
            left_events
                .try_send((
                    subscriber_shard,
                    ShardEvent::TrackSubscriptionAdded {
                        room_id: endpoint.room,
                        subscriber,
                        connection_id: subscriber_connection,
                        selector: TrackSelector::track(low_publication),
                        selection: SelectionPolicy::All,
                    }
                ))
                .is_ok()
        );
        let left_node = pulsebeam_core::identity::NodeId::new();
        let right_node = pulsebeam_core::identity::NodeId::new();
        let one = NonZeroU64::MIN;
        let two = NonZeroU64::new(2).unwrap();
        let fresh_until = now + Duration::from_secs(10);
        let from_left = right.open(left_node, one).await.unwrap();
        let from_right = left.open(right_node, one).await.unwrap();
        route_shard.apply_updates(16);
        assert_eq!(
            route_shard.installed_route_count(),
            1,
            "forwarding route must exist before defeat"
        );
        use crate::shard_update::ShardUpdateOp;
        for (updates, publication) in [
            (&mut left_updates, low_publication),
            (&mut right_updates, high_publication),
        ] {
            let mut installed = false;
            while let Ok(update) = updates.try_recv() {
                installed |= update.lifecycle.iter().any(|op| matches!(op, ShardUpdateOp::InsertTrackRuntime { publication_id, .. } if *publication_id == publication));
            }
            assert!(
                installed,
                "both isolated publications must be installed before reconciliation"
            );
        }

        let left_view = left.read().await.unwrap();
        let right_view = right.read().await.unwrap();
        for (snapshot, publication) in [
            (&left_view, low_publication),
            (&right_view, high_publication),
        ] {
            assert_eq!(snapshot.len(), 1);
            let (published_endpoint, fact) = &snapshot[0];
            assert_eq!(*published_endpoint, endpoint);
            assert!(matches!(fact.publications.as_ref(), [descriptor]
                if descriptor.descriptor.publication_id == publication.as_str() && descriptor.descriptor.track_id == "feed"));
        }
        assert_eq!(
            right
                .delta(
                    from_left,
                    one,
                    two,
                    (endpoint, Some(left_view[0].1.clone())),
                    fresh_until
                )
                .await,
            Err(Error::NeedsSnapshot)
        );
        right
            .snapshot(from_left, two, left_view, fresh_until)
            .await
            .unwrap();
        assert_eq!(right.read().await.unwrap(), right_view);
        left.snapshot(from_right, one, right_view.clone(), fresh_until)
            .await
            .unwrap();
        assert!(left.read().await.unwrap().is_empty());
        route_shard.apply_updates(16);
        assert_eq!(
            route_shard.installed_route_count(),
            0,
            "local forwarding route survived defeat"
        );
        let mut removed_track = false;
        let mut retired_transport = false;
        while let Ok(update) = left_updates.try_recv() {
            removed_track |= update.lifecycle.iter().any(|op| matches!(op, ShardUpdateOp::RemoveTrackRuntime { publication_id } if *publication_id == low_publication));
            retired_transport |= update
                .lifecycle
                .iter()
                .any(|op| matches!(op, ShardUpdateOp::RetireTransport { .. }));
        }
        assert!(
            removed_track && retired_transport,
            "defeat publishes a cutoff for the local publication and transport"
        );
        while let Ok(update) = right_updates.try_recv() {
            assert!(!update.lifecycle.iter().any(|op| matches!(
                op,
                ShardUpdateOp::RemoveTrackRuntime { .. } | ShardUpdateOp::RetireTransport { .. }
            )));
        }

        left.close(from_right).await.unwrap();
        let reconnected = left.open(right_node, two).await.unwrap();
        assert_eq!(
            left.snapshot(from_right, two, right_view, fresh_until)
                .await,
            Err(Error::Stale)
        );
        left.snapshot(reconnected, one, Box::new([]), fresh_until)
            .await
            .unwrap();
        publish(&left_events, connection_id(1));
        assert!(left.read().await.unwrap().is_empty());
        assert!(
            left_updates.try_recv().is_err(),
            "late publication cannot recreate a defeated transport's state"
        );
        shutdown.cancel();
        left_task.await.unwrap();
        right_task.await.unwrap();
    }

    #[test]
    fn old_expiry_work_cannot_close_replacement() {
        let mut actor = actor();
        let room_id = RoomId::new("expiry-fence").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let runtime_now = pulsebeam_runtime::time::Instant::now();
        let old_id = connection_id(1);
        let current_id = connection_id(2);
        let old_lease = authorization_lease(
            10,
            UNIX_EPOCH,
            runtime_now,
            "expiry-fence",
            room_id,
            participant_id,
        );
        let old_transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        actor
            .commit_reserved_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                old_transport,
                old_id,
                Some(old_lease),
                SignalingProtocol::State,
                UNIX_EPOCH,
            )
            .unwrap();
        let old_work = *actor.authorization_expiries.first().unwrap();
        let current_lease = authorization_lease(
            20,
            UNIX_EPOCH,
            runtime_now,
            "expiry-fence",
            room_id,
            participant_id,
        );
        let current_transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        let previous = actor
            .commit_reserved_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                current_transport,
                current_id,
                Some(current_lease),
                SignalingProtocol::State,
                UNIX_EPOCH,
            )
            .unwrap()
            .unwrap();
        actor.terminate_incarnation(
            participant_id,
            previous,
            false,
            ConnectionEnd::Replaced(None),
        );

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
        let room_id = RoomId::new("bounded-expiry").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let runtime_now = pulsebeam_runtime::time::Instant::now();

        for sequence in 1..=100 {
            let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
            let lease = authorization_lease(
                i64::MAX.unsigned_abs(),
                UNIX_EPOCH,
                runtime_now,
                "bounded-expiry",
                room_id,
                participant_id,
            );
            let previous = actor
                .commit_reserved_candidate(
                    participant_id,
                    room_id,
                    ShardId::new(0),
                    0,
                    transport,
                    connection_id(sequence),
                    Some(lease),
                    SignalingProtocol::State,
                    UNIX_EPOCH,
                )
                .unwrap();
            if let Some(previous) = previous {
                actor.terminate_incarnation(
                    participant_id,
                    previous,
                    false,
                    ConnectionEnd::Replaced(None),
                );
            }
        }

        let current = actor
            .core
            .registry
            .get_participant(&participant_id)
            .unwrap();
        assert_eq!(current.connection_id, connection_id(100));
        let current_lease = current.authorization.clone().unwrap();
        assert_eq!(current_lease.native_expiry(), Ok(i64::MAX));
        assert_eq!(actor.authorization_expiries.len(), 1);

        for (room, participant) in [
            (RoomId::new("other-room").unwrap(), participant_id),
            (room_id, ParticipantId::new("mallory").unwrap()),
        ] {
            assert!(matches!(
                actor.begin_create_participant(
                    ParticipantState {
                        room_id: Some(room),
                        participant_id: participant,

                        connection_id: connection_id(101),
                        old_connection_id: None,
                        authorization: Some(current_lease.clone()),
                        selection: super::super::sdp::Selection::Native,
                        profile: SignalingProtocol::State,
                        rtp_capabilities: Some(crate::control::rtp::capabilities()),
                    },
                    String::new(),
                ),
                Err(ControllerError::AuthorizationRejected)
            ));
        }

        let unrepresentable = authorization_lease(
            u64::MAX,
            UNIX_EPOCH,
            runtime_now,
            "bounded-expiry",
            room_id,
            participant_id,
        );
        assert_eq!(unrepresentable.expiry().unix_seconds(), u64::MAX);
        assert!(matches!(
            unrepresentable.native_expiry(),
            Err(TokenError::Invalid)
        ));
        assert!(matches!(
            actor.begin_create_participant(
                ParticipantState {
                    room_id: Some(room_id),
                    participant_id,

                    connection_id: connection_id(101),
                    old_connection_id: None,
                    authorization: Some(unrepresentable.clone()),
                    selection: super::super::sdp::Selection::Native,
                    profile: SignalingProtocol::State,
                    rtp_capabilities: Some(crate::control::rtp::capabilities()),
                },
                String::new(),
            ),
            Err(ControllerError::AuthorizationRejected)
        ));
        let transport = actor.core.reserve_transport(ShardId::new(0), runtime_now);
        assert!(matches!(
            actor.commit_reserved_candidate(
                participant_id,
                room_id,
                ShardId::new(0),
                0,
                transport,
                connection_id(101),
                Some(unrepresentable),
                SignalingProtocol::State,
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
        let room_id = RoomId::new("replacement").unwrap();
        let participant_id = crate::entity::fixture_participant_id();
        let old = connection_id(1);
        let current = connection_id(3);
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
                ProjectRoomKey::fixture(room_id),
                ShardId::new(0),
                0,
                old_transport,
                old,
                SignalingProtocol::State,
            )
            .unwrap();
        let prepared = |connection_id, transport| PendingMaterialization {
            native: None,
            shard: ShardId::new(0),
            partition_index: Some(0),
            command: None,
            ack: None,
            participant: participant_id,

            claim: ownership::ClaimRank::root(connection_id),
            authorization: None,
            profile: SignalingProtocol::State,
            transport,
            scope: ProjectRoomKey::fixture(room_id).into(),
            answer: "native-answer".to_owned(),
            whep_bindings: Default::default(),
            owned: None,
        };
        let pending_transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        let (ack, receiver) = oneshot::channel();
        let (reply, mut response) = oneshot::channel();
        actor.core.registry.reserve_partition(
            ProjectRoomKey::fixture(room_id),
            participant_id,
            connection_id(2),
            ShardId::new(0),
            0,
        );
        actor.core.registry.reserve_partition(
            ProjectRoomKey::fixture(room_id),
            participant_id,
            current,
            ShardId::new(0),
            0,
        );
        actor.materializations.push_back(MaterializationWork {
            pending: prepared(connection_id(2), pending_transport),
            reply: MaterializationReply::Create(reply),
            ack: receiver,
            ready: None,
            awaiting_predecessor_cutoff: false,
            superseded: false,
            cutoffs: MaterializationCutoffs::new(),
            started: pulsebeam_runtime::time::Instant::now(),
        });
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
                ProjectRoomKey::fixture(room_id),
                ShardId::new(0),
                0,
                old_transport,
                old,
                SignalingProtocol::State,
            ),
            Err(CommitCandidateError::Superseded)
        );

        actor.process_command(
            DeleteParticipant {
                room_id,
                participant_id,
                connection_id: old,
                profile: SignalingProtocol::State,
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
                profile: SignalingProtocol::Whip,
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
                profile: SignalingProtocol::State,
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
        ack.send(true).unwrap();
        actor.advance_materializations();
        assert!(matches!(
            response.try_recv().unwrap(),
            Err(ControllerError::Superseded)
        ));
        assert!(actor.materializations.is_empty());
        assert!(
            actor
                .core
                .registry
                .get_participant(&participant_id)
                .is_none(),
            "late installation cannot resurrect a superseded connection after its replacement was deleted"
        );
    }

    #[test]
    fn stale_media_events_cannot_republish_or_unpublish_the_replacement() {
        let mut actor = actor();
        let room_id = RoomId::new("media-fence").unwrap();
        let participant = crate::entity::fixture_participant_id();
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
                    ProjectRoomKey::fixture(room_id),
                    ShardId::new(0),
                    0,
                    address,
                    connection_id,
                    SignalingProtocol::State,
                )
                .unwrap();
        }
        let make_track = |connection_id: ConnectionId| {
            crate::track::Track::data(
                crate::track::TrackMeta {
                    room: crate::entity::ProjectRoomKey::fixture(room_id),
                    id: connection_id
                        .fixture_publication(crate::entity::TrackKind::Data, "rt/chat"),
                    origin: participant,
                    connection_id,
                    track_id: Some(crate::entity::TrackId::new("chat").unwrap()),
                },
                crate::track::Topic::for_test("chat"),
                crate::track::DataLane::Latest,
                None,
            )
        };
        let stale_track = make_track(stale);
        let stale_meta = stale_track.meta().clone();
        let endpoint = EndpointKey {
            room: stale_meta.room,
            participant,
        };
        let publication_id = current.fixture_publication(crate::entity::TrackKind::Data, "rt/chat");

        actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(stale_track)));
        assert!(
            actor
                .lifecycle
                .unpublish(
                    endpoint,
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
                    endpoint,
                    publication_id,
                    &actor.core.registry,
                    pulsebeam_runtime::time::Instant::now(),
                )
                .is_some(),
            "the replacement's publication must survive stale teardown"
        );
    }
}
