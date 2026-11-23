use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::native_state::NativeState;
use serde::{Deserialize, Serialize};

use crate::{
    AgentConfig, ChannelId, ConnectionState, DataChannelEffect, DataChannelEvent,
    DataChannelReliability, DataChannelSpec, DesiredState, Effect, Failure, FailureClass,
    Generation, HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod, HttpRequest,
    HttpResponse, MediaSlot, Notification, OfferResources, OperationId, RtcEffect, RtcEvent,
    SlotBinding, Snapshot, TimerEffect, TimerEvent, TimerId, TopicDropReason, TopicError,
    TopicSend, ValidationError,
    id::IdGenerator,
    signaling::{self, SignalingError},
    topic::Topics,
};

const CONTENT_TYPE: &str = "Content-Type";
const JSON_CONTENT_TYPE: &str = "application/json";
const SIGNAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_EARLY_SIGNALING_MESSAGES: usize = 64;
pub(crate) const MAX_ORPHANED_CREATES: usize = 32;

macro_rules! agent_log {
    ($agent:expr, $level:ident, $($message:tt)*) => {
        if $agent.config.log_level.allows(log::Level::$level) {
            $agent.logs.write(log::Level::$level, module_path!(), format_args!($($message)*));
        }
    };
}

mod admission;
mod credentials;
mod native;
mod publication;
mod subscription;

use admission::CreateRequest;

#[derive(Clone, PartialEq, Eq)]
pub enum AgentCommand {
    ReplaceDesired(DesiredState),
    RenewAuthorization(String),
    Join(String),
    LeaveRoom,
    Connect,
    Disconnect,
    Reconnect,
    SendTopic(TopicSend),
}

impl core::fmt::Debug for AgentCommand {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReplaceDesired(desired) => formatter
                .debug_tuple("ReplaceDesired")
                .field(desired)
                .finish(),
            Self::RenewAuthorization(_) => formatter.write_str("RenewAuthorization([REDACTED])"),
            Self::Join(room) => formatter.debug_tuple("Join").field(room).finish(),
            Self::LeaveRoom => formatter.write_str("LeaveRoom"),
            Self::Connect => formatter.write_str("Connect"),
            Self::Disconnect => formatter.write_str("Disconnect"),
            Self::Reconnect => formatter.write_str("Reconnect"),
            Self::SendTopic(send) => formatter.debug_tuple("SendTopic").field(send).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("operation failed: {0:?}")]
    OperationFailed(crate::ffi::ErrorInfo),
    #[error(transparent)]
    InvalidConfiguration(#[from] ValidationError),
    #[error("desired revision {received} is older than accepted revision {accepted}")]
    StaleDesiredRevision { received: u64, accepted: u64 },
    #[error("desired revision {0} was reused with different state")]
    ConflictingDesiredRevision(u64),
    #[error("authorization token is invalid")]
    InvalidAuthorizationToken,
    #[error("participant ownership was replaced")]
    Replaced,
    #[error("credential revision space is exhausted")]
    CredentialRevisionExhausted,
    #[error("authorization renewal is already in flight")]
    RenewalBusy,
    #[error("signaling is not ready for authorization renewal")]
    RenewalNotReady,
    #[error("host offer resources are invalid: {0}")]
    InvalidOffer(&'static str),
    #[error(transparent)]
    InvalidSignaling(#[from] SignalingError),
    #[error(transparent)]
    InvalidTopic(#[from] TopicError),
    #[error(transparent)]
    InvalidResource(#[from] crate::TrackResourceError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceReadiness {
    Ready,
    Preparing,
    Failed,
}

/// Producer acquisition waits only for physical changes to its own local slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePreparation<'a> {
    pub target: Option<&'a str>,
    pub readiness: SourceReadiness,
}

#[derive(Clone)]
struct Credential {
    token: String,
    revision: u64,
}

struct Session {
    credential: Credential,
    admission: admission::AcceptedReceipts,
    generation: Generation,
    connection_id: crate::ConnectionId,
    resource_uri: String,
    participant_id: String,
    room_id: Option<String>,
    coordinates: BTreeMap<MediaSlot, Option<SlotBinding>>,
    state: NativeState,
    payloads: BTreeMap<ChannelId, native::PayloadChannel>,
    payload_resources: BTreeMap<crate::native_state::Resource, ChannelId>,
    next_stream: u32,
    reusable_streams: BTreeSet<u16>,
    media_leases: BTreeMap<MediaSlot, native::MediaLease>,
    media_retirement: Option<native::MediaRetirement>,
    media_allocation: Option<native::MediaAllocation>,
    media_wanted: BTreeMap<MediaSlot, native::MediaWant>,
    media_blocked: BTreeMap<MediaSlot, native::MediaWant>,

    authorization_expires_at: Option<i64>,
    pending_renewal: Option<native::Renewal>,
    authorization_revision: u64,
    signaling_terminal: bool,
    signaling_channel: ChannelId,
    data_preparations: BTreeMap<ChannelId, Result<crate::PreparedDataChannel, String>>,
    capabilities: crate::BootstrapCapabilities,
    media_preparation:
        Option<Result<crate::rtc_preparation::PreparedMedia, crate::client_sdp::Error>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptMode {
    Fresh,
    Replace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptStage {
    Offering,
    Acquiring,
    Requesting,
    ApplyingAnswer,
}

struct Candidate {
    credential: Credential,
    connection_id: crate::ConnectionId,
    capabilities: crate::BootstrapCapabilities,
    resource_uri: String,
    room_id: Option<String>,
    participant_id: String,
}

#[derive(Serialize)]
struct NativeRequest<'a> {
    profile: &'static str,
    offer: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    room: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    successor_receipt: Option<&'a [u8]>,
}

#[derive(Deserialize)]
struct NativeResponse {
    profile: String,
    connection_id: String,
    rtp_capabilities: pulsebeam_proto::state::RtpCapabilities,
    data_delivery: Vec<serde_json::Value>,
    room_id: Option<String>,
    participant_id: String,
    answer: String,
}

struct CapabilityRequest {
    operation: Option<OperationId>,
    credential: Credential,
    request: HttpRequest,
    attempts: u8,
}

struct Attempt {
    generation: Generation,
    join_revision: u64,
    room: Option<String>,
    mode: AttemptMode,
    stage: AttemptStage,
    resources: Option<OfferResources>,
    capability: Option<CapabilityRequest>,
    request: Option<CreateRequest>,
    candidate: Option<Candidate>,
    answer_applied: bool,
    transport_connected: bool,
    signaling_open: bool,
    early_signaling: VecDeque<Vec<u8>>,
    topic_registrations: crate::TopicRegistrations,
    open_channels: BTreeSet<ChannelId>,
    media_preparation:
        Option<Result<crate::rtc_preparation::PreparedMedia, crate::client_sdp::Error>>,
    data_preparations: BTreeMap<ChannelId, Result<crate::PreparedDataChannel, String>>,
}

#[derive(Clone, Copy, Debug)]
enum RetryTarget {
    Connection(AttemptMode),
    Capability,
    Creation,
}

struct Retry {
    timer: TimerId,
    target: RetryTarget,
}

struct PendingSignal {
    operation: OperationId,
    generation: Generation,
    channel: ChannelId,
    payload: Vec<u8>,
    retrying: bool,
}

#[derive(Default)]
struct Closing {
    generations: BTreeSet<Generation>,
    deletes: BTreeSet<OperationId>,
}

pub struct Agent {
    config: AgentConfig,
    config_credential_revision: u64,
    admission_denial: Option<credentials::AdmissionDenial>,
    logs: crate::logging::LogBuffer,
    desired: DesiredState,
    declared: DesiredState,
    resources: crate::TrackResources,
    catalogs: crate::catalog::Catalogs,
    local_bindings: BTreeMap<MediaSlot, String>,
    snapshot: Snapshot,
    ids: IdGenerator,
    effects: VecDeque<Effect>,
    notifications: VecDeque<Notification>,
    published_notifications: VecDeque<Notification>,
    active: Option<Session>,
    ownership: admission::Ownership,
    joined_successor: Option<pulsebeam_proto::state::SuccessorReceipt>,
    attempt: Option<Attempt>,
    retry: Option<Retry>,
    retry_attempts: u8,
    pending_signal: Option<PendingSignal>,
    signal_retry: Option<TimerId>,
    topic_probe: Option<TimerId>,
    reconciliation_wake: Option<TimerId>,
    intent_dirty: bool,
    closing: Option<Closing>,
    orphaned_creates: BTreeMap<OperationId, Credential>,
    topics: Topics,
}

impl Agent {
    pub fn new(mut config: AgentConfig) -> Result<Self, AgentError> {
        config.validate()?;
        Ok(Self {
            config,
            config_credential_revision: 0,
            admission_denial: None,
            logs: Default::default(),
            desired: DesiredState::default(),
            declared: DesiredState::default(),
            resources: crate::TrackResources::default(),
            catalogs: crate::catalog::Catalogs::default(),
            local_bindings: BTreeMap::new(),
            snapshot: Snapshot::default(),
            ids: IdGenerator::new(),
            effects: VecDeque::new(),
            notifications: VecDeque::new(),
            published_notifications: VecDeque::new(),
            active: None,
            ownership: admission::Ownership::default(),
            joined_successor: None,
            attempt: None,
            retry: None,
            retry_attempts: 0,
            pending_signal: None,
            signal_retry: None,
            topic_probe: None,
            reconciliation_wake: None,
            intent_dirty: false,
            closing: None,
            orphaned_creates: BTreeMap::new(),
            topics: Topics::default(),
        })
    }

    pub fn defer_logs(&mut self) {
        self.logs.defer();
    }

    pub fn drain_logs(&mut self) -> Vec<crate::DeferredLog> {
        self.logs.drain()
    }

    pub fn set_log_level(&mut self, level: crate::LogLevel) {
        if self.config.log_level == level {
            return;
        }
        self.config.log_level = level;
    }

    pub fn command(&mut self, command: AgentCommand) -> Result<(), AgentError> {
        let result = match command {
            AgentCommand::ReplaceDesired(desired) => self.replace_desired(desired),
            AgentCommand::RenewAuthorization(token) => self.renew_authorization(token),
            AgentCommand::Join(room) => self.set_presence(Some(room)),
            AgentCommand::LeaveRoom => self.set_presence(None),
            AgentCommand::Connect => self.connect(),
            AgentCommand::Disconnect => self.disconnect(),
            AgentCommand::Reconnect => {
                self.reconnect();
                Ok(())
            }
            AgentCommand::SendTopic(send) => self
                .topics
                .send(
                    send,
                    &mut self.ids,
                    &mut self.effects,
                    &mut self.snapshot,
                    &mut self.notifications,
                )
                .map_err(AgentError::from),
        };
        let flushed = self.flush_notifications();
        let result = result.and(flushed);
        if let Err(error) = &result {
            agent_log!(
                self,
                Warn,
                "agent command rejected: code={:?}",
                crate::ffi::ErrorInfo::from(error).code
            );
        }
        result
    }

    pub fn handle(&mut self, event: HostEvent) -> Result<(), AgentError> {
        let result = match event {
            HostEvent::Rtc(event) => self.handle_rtc(event),
            HostEvent::Http(event) => self.handle_http(event),
            HostEvent::Timer(event) => self.handle_timer(event),
            HostEvent::DataChannel(event) => self.handle_data_channel(event),
        };
        if self.intent_dirty {
            self.request_reconciliation();
        }
        let flushed = self.flush_notifications();
        let result = result.and(flushed);
        if let Err(error) = &result {
            agent_log!(
                self,
                Warn,
                "host event rejected: code={:?}",
                crate::ffi::ErrorInfo::from(error).code
            );
        }
        result
    }

    pub fn host_failed(&mut self, failure: Failure) -> Result<(), AgentError> {
        self.fail_terminal(failure);
        self.flush_notifications()
    }

    pub fn send_data(
        &mut self,
        spec: crate::TrackSpec,
        payload: Vec<u8>,
    ) -> Result<OperationId, AgentError> {
        self.send_data_at(spec, payload, None)
    }

    pub fn send_data_at(
        &mut self,
        spec: crate::TrackSpec,
        payload: Vec<u8>,
        captured_at: Option<crate::ffi::CaptureTimestamp>,
    ) -> Result<OperationId, AgentError> {
        if let Some(failure) = &self.snapshot.terminal_failure {
            return Err(AgentError::OperationFailed(failure.into()));
        }
        if !self.desired.connected {
            return Err(crate::TrackResourceError::NotConnected.into());
        }
        let send = crate::TopicSend {
            publisher: spec.publisher()?,
            payload,
        };
        self.topics.validate_data_send(&send)?;
        self.resource_command(crate::ResourceCommand::RetainProducer { spec })?;
        let ticket =
            self.topics
                .begin_data_send(send, captured_at, &mut self.ids, &mut self.effects);
        self.flush_notifications()?;
        Ok(ticket)
    }

    pub fn resource(
        &mut self,
        command: crate::ResourceCommand,
    ) -> Result<crate::ResourceResult, AgentError> {
        let result = self.resource_command(command);
        let flushed = self.flush_notifications();
        result.and_then(|value| flushed.map(|()| value))
    }

    fn resource_command(
        &mut self,
        command: crate::ResourceCommand,
    ) -> Result<crate::ResourceResult, AgentError> {
        use crate::{ResourceCommand, ResourceResult, TrackOwner, TrackResourceError};
        let releasing = matches!(&command, ResourceCommand::Release { .. });
        let result = match command {
            ResourceCommand::ClaimPublication { spec } => {
                spec.validate()?;
                let kind = spec.media_kind().ok_or(TrackResourceError::WrongKind)?;
                if self.resources.has_publication(&spec) {
                    return Err(TrackResourceError::AlreadyOwned.into());
                }
                if self
                    .local_bindings
                    .iter()
                    .any(|(slot, track_id)| slot.kind() == kind && track_id == spec.track_id())
                {
                    return Err(TrackResourceError::AlreadyOwned.into());
                }
                if self.resources.owned_publication_capacity_exceeded() {
                    return Err(TrackResourceError::PublicationCapacityExceeded.into());
                }
                let occupied = self
                    .local_bindings
                    .keys()
                    .filter_map(|slot| match slot {
                        MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name) => {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect();
                let owner = self.resources.new_owner()?;
                let slot = self.resources.claim_publication(owner, spec, &occupied)?;
                let after_generation = None;
                ResourceResult::Publication {
                    owner: owner.id(),
                    slot,
                    after_generation,
                }
            }
            ResourceCommand::DeclareDataPublication { spec } => {
                self.resources.validate_data_publication_claim(&spec)?;
                let mut owned = self.resources.registrations();
                owned.publishers.push(spec.publisher()?);
                self.merged_topics(owned)?;
                let owner = self.resources.new_owner()?;
                self.resources.declare_data_publication(owner, &spec)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::UpdatePublication { owner, present } => {
                let owner = TrackOwner(owner);
                let spec = self
                    .resources
                    .publication_spec(owner)
                    .ok_or(TrackResourceError::Released)?;
                self.resources.update_publication(owner, &spec, present)?;
                ResourceResult::Updated
            }
            ResourceCommand::AttachPublicationInput { owner } => {
                let (operation, state) =
                    self.resources.attach_publication_input(TrackOwner(owner))?;
                ResourceResult::PublicationInput { operation, state }
            }
            ResourceCommand::DetachPublicationInput { input } => {
                match self.resources.detach_publication_input(input) {
                    Some((operation, state)) => {
                        ResourceResult::PublicationInput { operation, state }
                    }
                    None => return Ok(ResourceResult::Superseded),
                }
            }
            ResourceCommand::BeginPublicationInput {
                owner,
                input,
                present,
                muted,
            } => {
                let (operation, state) = self.resources.begin_publication_input(
                    TrackOwner(owner),
                    input,
                    present,
                    muted.unwrap_or(self.resources.publication_muted(TrackOwner(owner))?),
                )?;
                ResourceResult::PublicationInput { operation, state }
            }
            ResourceCommand::AuthorizePublicationInput { operation } => {
                return Ok(if self.resources.publication_input_current(&operation) {
                    ResourceResult::Updated
                } else {
                    ResourceResult::Superseded
                });
            }
            ResourceCommand::CompletePublicationInput { operation, result } => {
                match self
                    .resources
                    .complete_publication_input(&operation, result)
                {
                    Some(state) => ResourceResult::PublicationInputState(state),
                    None => return Ok(ResourceResult::Superseded),
                }
            }
            ResourceCommand::CompletePublicationInputTarget { control, result } => {
                if control.target.is_none()
                    || control.operation.input().is_none()
                    || !self.publication_input_control_current(&control)
                    || !self.resources.complete_input_control(&control)
                {
                    return Ok(ResourceResult::Superseded);
                }
                match result {
                    Ok(()) => ResourceResult::Updated,
                    Err(code) => match self
                        .resources
                        .fail_publication_input(&control.operation, code)
                    {
                        Some(state) => ResourceResult::PublicationInputState(state),
                        None => return Ok(ResourceResult::Superseded),
                    },
                }
            }
            ResourceCommand::SetPublicationMuted { owner, muted } => {
                ResourceResult::PublicationInputState(
                    self.resources
                        .set_publication_muted(TrackOwner(owner), muted)?,
                )
            }
            ResourceCommand::ClaimVideo { selector } => {
                if !self.declared.video.is_empty() {
                    return Err(TrackResourceError::AlreadyOwned.into());
                }
                let owner = self.resources.new_owner()?;
                self.resources.claim_video(owner, selector)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ObserveVideo { owner, observation } => {
                self.resources
                    .observe_video(TrackOwner(owner), observation)?;
                ResourceResult::Updated
            }
            ResourceCommand::ClaimAudio => {
                let owner = self.resources.new_owner()?;
                self.resources.claim_audio(owner)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ClaimAudioSubscription { selector } => {
                if self.declared.audio.has_demand() {
                    return Err(TrackResourceError::AlreadyOwned.into());
                }
                let owner = self.resources.new_owner()?;
                self.resources.claim_audio_subscription(owner, selector)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ConfigureAudio {
                owner,
                playout_delay,
            } => {
                self.resources
                    .configure_audio(TrackOwner(owner), playout_delay)?;
                ResourceResult::Updated
            }
            ResourceCommand::ClaimAudioPolicy { policy } => {
                if self.declared.audio.has_demand() {
                    return Err(TrackResourceError::AlreadyOwned.into());
                }
                let owner = self.resources.new_owner()?;
                self.resources.claim_audio_policy(owner, policy)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ConfigureAudioPolicy { owner, policy } => {
                self.resources
                    .configure_audio_policy(TrackOwner(owner), policy)?;
                ResourceResult::Updated
            }
            ResourceCommand::BeginMediaObservation { owner } => ResourceResult::MediaObservation(
                self.resources.begin_media_observation(TrackOwner(owner))?,
            ),
            ResourceCommand::BeginAudioSourceObservation { owner, source } => {
                ResourceResult::MediaObservation(
                    self.resources
                        .begin_audio_source_observation(TrackOwner(owner), source)?,
                )
            }
            ResourceCommand::AuthorizeMediaObservation { control } => {
                return Ok(if self.resources.observation_control_current(&control) {
                    ResourceResult::Updated
                } else {
                    ResourceResult::Superseded
                });
            }
            ResourceCommand::CompleteMediaObservation { control, succeeded } => {
                if !self
                    .resources
                    .complete_observation_control(&control, succeeded)
                {
                    return Ok(ResourceResult::Superseded);
                }
                ResourceResult::Updated
            }
            ResourceCommand::EndMediaObservation { control } => {
                if !self.resources.end_observation_control(&control) {
                    return Ok(ResourceResult::Superseded);
                }
                ResourceResult::Updated
            }
            ResourceCommand::RetainProducer { spec } => {
                if !self.desired.connected {
                    return Err(TrackResourceError::NotConnected.into());
                }
                let mut owned = self.resources.registrations();
                owned.publishers.push(spec.publisher()?);
                self.merged_topics(owned)?;
                self.resources.retain_producer(&spec)?;
                ResourceResult::Updated
            }
            ResourceCommand::ReceiveData { spec, source } => {
                let mut owned = self.resources.registrations();
                owned.subscribers.push(spec.subscriber(&source)?);
                self.merged_topics(owned)?;
                let owner = self.resources.new_owner()?;
                self.resources.receive(owner, &spec, source)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ReadData { owner } => {
                return Ok(ResourceResult::Event {
                    event: self.resources.read(TrackOwner(owner))?,
                });
            }
            ResourceCommand::Release { owner } => {
                if let Some(mut control) = self.resources.release_input_control(TrackOwner(owner)) {
                    control.effect = Some(self.ids.operation());
                    self.effects.push_back(Effect::PublicationInput(control));
                }
                for mut control in self
                    .resources
                    .release_observation_controls(TrackOwner(owner))
                {
                    control.effect = Some(self.ids.operation());
                    self.effects.push_back(Effect::MediaObservation(control));
                }
                self.resources.release(TrackOwner(owner));
                ResourceResult::Updated
            }
        };
        self.refresh_resources()?;
        if releasing
            && let Some(active) = self
                .active
                .as_mut()
                .filter(|active| !active.signaling_terminal)
        {
            native::fence_unwanted_media(active, &self.desired, &mut self.ids, &mut self.effects)?;
            if native::project_retired_receivers(active, &mut self.snapshot) {
                self.snapshot.version = self.snapshot.version.saturating_add(1);
                self.notifications.push_back(Notification::SnapshotChanged);
            }
        }
        Ok(result)
    }

    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effects.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn test_native_owned(
        &self,
        reference: u64,
    ) -> Option<&pulsebeam_proto::state::ConfirmedState> {
        let state = &self.active.as_ref()?.state;
        state
            .resources()
            .find(|resource| state.reference(resource) == Some(reference))
            .and_then(|resource| state.confirmed(resource))
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Whether accepted desired state publishes this local sender slot.
    /// Media silence/blanking is a host operation, not publication withdrawal.
    pub fn is_publishing(&self, slot: &MediaSlot) -> bool {
        let name = match slot {
            MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name) => name,
            MediaSlot::RemoteVideo(_) | MediaSlot::RemoteAudio(_) => return false,
        };
        self.desired.publications.iter().any(|publication| {
            publication.slot == *name && publication.kind == slot.kind() && publication.active
        })
    }

    pub fn next_notification(&mut self) -> Option<Notification> {
        self.published_notifications.pop_front()
    }

    /// Query attachment authority across registration work and connection incarnations.
    pub fn publication_input_state(
        &self,
        input: crate::PublicationInputId,
    ) -> Option<crate::PublicationInputState> {
        self.resources.publication_input_state(input)
    }

    pub fn publication_input_control_current(
        &self,
        control: &crate::PublicationInputControl,
    ) -> bool {
        control.effect.is_some() && self.resources.input_control_current(control)
    }

    fn input_control(
        &self,
        mut control: crate::PublicationInputControl,
    ) -> crate::PublicationInputControl {
        if !self.desired.connected || control.operation.input().is_none() {
            return control;
        }
        let slot = match self
            .resources
            .publication_spec(crate::TrackOwner(control.operation.owner()))
            .and_then(|spec| spec.media_kind())
        {
            Some(crate::MediaKind::Video) => MediaSlot::LocalVideo(control.operation.slot().into()),
            Some(crate::MediaKind::Audio) => MediaSlot::LocalAudio(control.operation.slot().into()),
            None => return control,
        };
        // A replacement attempt owns a new graph, even while the old session drains.
        let target = if let Some(attempt) = &self.attempt {
            attempt
                .resources
                .as_ref()
                .and_then(|resources| resources.slots.iter().find(|binding| binding.slot == slot))
                .map(|binding| crate::PublicationInputTarget {
                    generation: attempt.generation,
                    binding: binding.clone(),
                })
        } else {
            self.active
                .as_ref()
                .filter(|_| self.source_preparation(&slot).readiness == SourceReadiness::Ready)
                .and_then(|active| {
                    active
                        .coordinates
                        .get(&slot)
                        .and_then(Option::as_ref)
                        .map(|binding| crate::PublicationInputTarget {
                            generation: active.generation,
                            binding: binding.clone(),
                        })
                })
        };
        control.target = target;
        control
    }

    fn flush_notifications(&mut self) -> Result<(), AgentError> {
        let result = self.flush_topic_notifications();
        // Failed notification processing must not retain stale input grants.
        self.reconcile_input_controls();
        self.reconcile_observation_controls();
        result
    }

    fn flush_topic_notifications(&mut self) -> Result<(), AgentError> {
        self.topics.progress_data_sends(
            self.desired.connected,
            &mut self.ids,
            &mut self.effects,
            &mut self.snapshot,
            &mut self.notifications,
        );
        let mut ready = BTreeSet::new();
        while let Some(notification) = self.notifications.pop_front() {
            if let Notification::Topic(topic) = &notification {
                if self.config.log_level.allows(log::Level::Warn) {
                    match topic {
                        crate::TopicNotification::SendDropped { publisher, reason } => {
                            self.logs.write(
                                log::Level::Warn,
                                crate::topic::LOG_TARGET,
                                format_args!(
                                    "topic send dropped mode={:?} reason={reason:?}",
                                    publisher.mode
                                ),
                            );
                        }
                        crate::TopicNotification::ChannelFailed { channel, .. } => {
                            let direction = match channel {
                                crate::TopicChannel::Publisher(_) => "publish",
                                crate::TopicChannel::Subscriber(_) => "subscribe",
                            };
                            self.logs.write(
                                log::Level::Warn,
                                crate::topic::LOG_TARGET,
                                format_args!("topic channel failed direction={direction}"),
                            );
                        }
                        _ => {}
                    }
                }
                let update = self.resources.notify(topic);
                ready.extend(update.ready.into_iter().map(crate::TrackOwner::id));
                if update.registrations_changed {
                    self.refresh_resources()?;
                }
            }
            if matches!(notification, Notification::SnapshotChanged)
                && self
                    .published_notifications
                    .contains(&Notification::SnapshotChanged)
            {
                continue;
            }
            self.published_notifications.push_back(notification);
        }
        if !ready.is_empty() {
            self.published_notifications
                .push_back(Notification::DataReady {
                    owners: ready.into_iter().collect(),
                });
        }
        Ok(())
    }

    fn reconcile_input_controls(&mut self) {
        let controls: Vec<_> = self
            .resources
            .publication_inputs()
            .map(|control| self.input_control(control))
            .collect();
        for mut control in controls {
            if self.resources.input_control_changed(&control) {
                control.effect = Some(self.ids.operation());
                self.resources.update_input_control(&control);
                self.effects.push_back(Effect::PublicationInput(control));
            }
        }
    }

    fn reconcile_observation_controls(&mut self) {
        for (id, slot) in self.resources.observation_requests() {
            let source = self.resources.audio_observation_source(id);
            let authorized =
                source.is_none_or(|source| self.audio_source_authorized(id.owner(), source));
            let target = if !authorized {
                None
            } else if let Some(source) = source {
                self.audio_source_observation_target(source)
            } else {
                slot.as_ref()
                    .and_then(|slot| self.media_observation_target(slot))
                    .or_else(|| {
                        self.resources
                            .audio_observation_selector(id)
                            .and_then(|selector| self.audio_observation_target(selector))
                    })
            };
            let Some(mut control) = self.resources.observation_control(id, target) else {
                continue;
            };
            if !authorized {
                control.closed = true;
                control.target = None;
            }
            if self.resources.observation_control_changed(&control) {
                control.effect = Some(self.ids.operation());
                self.resources.update_observation_control(&control);
                self.effects.push_back(Effect::MediaObservation(control));
            }
        }
    }

    fn advance_credential_revision(&mut self) -> Result<u64, AgentError> {
        let revision = self
            .snapshot
            .credential_revision
            .checked_add(1)
            .ok_or(AgentError::CredentialRevisionExhausted)?;
        self.snapshot.credential_revision = revision;
        self.bump_snapshot();
        self.notifications.push_back(Notification::SnapshotChanged);
        Ok(revision)
    }

    fn replace_desired(&mut self, mut desired: DesiredState) -> Result<(), AgentError> {
        desired.normalize();
        desired.validate()?;
        if desired.revision < self.declared.revision {
            return Err(AgentError::StaleDesiredRevision {
                received: desired.revision,
                accepted: self.declared.revision,
            });
        }
        if desired.revision == self.declared.revision {
            return if desired == self.declared {
                Ok(())
            } else {
                Err(AgentError::ConflictingDesiredRevision(desired.revision))
            };
        }
        let owned_slots = self.resources.publication_slots();
        for publication in &desired.publications {
            let spec = match publication.kind {
                crate::MediaKind::Video => crate::TrackSpec::Video {
                    track_id: publication.track_id.clone(),
                },
                crate::MediaKind::Audio => crate::TrackSpec::Audio {
                    track_id: publication.track_id.clone(),
                },
            };
            if owned_slots.contains(&publication.slot) || self.resources.has_publication(&spec) {
                return Err(crate::TrackResourceError::AlreadyOwned.into());
            }
        }
        if !desired.video.is_empty() && self.resources.has_video()
            || desired.audio.has_demand() && self.resources.has_audio_subscriptions()
        {
            return Err(crate::TrackResourceError::AlreadyOwned.into());
        }
        let effective = self.effective_desired(&desired)?;
        self.local_bindings.clear();
        for publication in &desired.publications {
            self.local_bindings
                .insert(publication.media_slot(), publication.track_id.clone());
        }
        self.declared = desired;
        self.apply_desired(effective)
    }

    fn merged_topics(
        &self,
        mut owned: crate::TopicRegistrations,
    ) -> Result<crate::TopicRegistrations, AgentError> {
        owned
            .publishers
            .extend(self.declared.topics.publishers.iter().cloned());
        owned
            .subscribers
            .extend(self.declared.topics.subscribers.iter().cloned());
        owned.normalize();
        owned.publishers.dedup();
        owned.subscribers.dedup();
        owned.validate()?;
        Ok(owned)
    }

    fn effective_desired(&mut self, declared: &DesiredState) -> Result<DesiredState, AgentError> {
        let mut desired = declared.clone();
        desired.publications.extend(self.resources.publications());
        let owned = self.resources.registrations();
        desired.topics.publishers.extend(owned.publishers);
        desired.topics.subscribers.extend(owned.subscribers);
        desired.normalize();
        desired.topics.publishers.dedup();
        desired.topics.subscribers.dedup();
        if self.resources.has_video() {
            desired.video = self.resources.video_projection()?;
        }
        if self.resources.has_audio_subscriptions() {
            desired.audio = self.resources.audio_projection();
        }
        desired.validate()?;
        Ok(desired)
    }

    fn refresh_resources(&mut self) -> Result<(), AgentError> {
        if self.resources.is_empty() && self.desired == self.declared {
            return Ok(());
        }
        let effective = self.effective_desired(&self.declared.clone())?;
        self.apply_desired(effective)
    }

    fn apply_desired(&mut self, desired: DesiredState) -> Result<(), AgentError> {
        if self.desired == desired {
            return Ok(());
        }
        let video_changed = self.desired.video != desired.video;
        let intent_changed = self.desired.publications != desired.publications
            || video_changed
            || self.desired.audio != desired.audio;
        let topics_changed = self.desired.topics != desired.topics;
        let previous_video_count = self.desired.video.len();
        self.desired = desired;
        self.snapshot.desired_revision = self.desired.revision;
        self.bump_snapshot();
        if video_changed {
            agent_log!(
                self,
                Debug,
                "desired video subscriptions changed revision={} previous_count={} current_count={}",
                self.desired.revision,
                previous_video_count,
                self.desired.video.len(),
            );
        }
        agent_log!(
            self,
            Debug,
            "accepted desired state revision={} connected={} publications={} video_subscriptions={} pinned_audio={}",
            self.desired.revision,
            self.desired.connected,
            self.desired.publications.len(),
            self.desired.video.len(),
            self.desired.audio.pinned.len(),
        );
        if topics_changed {
            self.topics
                .accept_registrations(&self.desired.topics, &mut self.snapshot);
        }
        if intent_changed || topics_changed {
            self.intent_dirty = true;
        }

        if self.desired.connected {
            if self.closing.is_none() && self.attempt.is_none() && self.retry.is_none() {
                if self.snapshot.terminal_failure.is_some() {
                    return Ok(());
                } else if self.active.is_none() {
                    if self.snapshot.desired_room.is_some() {
                        self.start_attempt(AttemptMode::Fresh);
                    }
                } else if self.intent_dirty {
                    self.request_reconciliation();
                }
            } else if self.intent_dirty {
                self.request_reconciliation();
            }
        } else {
            self.begin_close();
        }
        Ok(())
    }

    fn set_presence(&mut self, room: Option<String>) -> Result<(), AgentError> {
        if self.participant_superseded() {
            return Err(AgentError::Replaced);
        }
        if let Some(room) = room.as_deref() {
            crate::validate_room_name(room)?;
        }
        let unchanged = self.snapshot.desired_room == room;
        if unchanged && self.snapshot.terminal_failure.is_none() {
            if room.is_some() && !self.desired.connected {
                self.connect()?;
            }
            return Ok(());
        }
        if !unchanged {
            self.ownership = admission::Ownership::default();
            self.joined_successor = None;
        }
        self.snapshot.desired_room = room;
        self.snapshot.join_revision = self.snapshot.join_revision.saturating_add(1);
        self.snapshot.room_id = None;
        self.snapshot.accepted_join_revision = None;
        self.clear_observed_state();
        self.notifications.push_back(Notification::SnapshotChanged);
        if !self.desired.connected {
            if self.snapshot.desired_room.is_some() {
                self.connect()?;
            }
        } else {
            self.snapshot.terminal_failure = None;
            self.begin_close();
        }
        Ok(())
    }

    fn connect(&mut self) -> Result<(), AgentError> {
        if self.participant_superseded() {
            return Err(AgentError::Replaced);
        }
        if !self.desired.connected {
            let mut desired = self.declared.clone();
            desired.revision = desired.revision.saturating_add(1);
            desired.connected = true;
            self.replace_desired(desired)?;
        }
        if self.snapshot.terminal_failure.is_some() {
            self.reconnect();
        } else if self.active.is_none()
            && self.attempt.is_none()
            && self.closing.is_none()
            && self.retry.is_none()
        {
            self.start_attempt(AttemptMode::Fresh);
        }
        Ok(())
    }

    fn participant_superseded(&self) -> bool {
        self.snapshot
            .terminal_failure
            .as_ref()
            .is_some_and(|failure| failure.class == FailureClass::ParticipantSuperseded)
    }

    fn disconnect(&mut self) -> Result<(), AgentError> {
        if self.desired.connected {
            let mut desired = self.declared.clone();
            desired.revision = desired.revision.saturating_add(1);
            desired.connected = false;
            self.replace_desired(desired)?;
        }
        Ok(())
    }

    fn reconnect(&mut self) {
        if self.participant_superseded() {
            return;
        }
        if !self.desired.connected || self.closing.is_some() || self.attempt.is_some() {
            return;
        }
        if let Some(retry) = self.retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
        }
        self.snapshot.terminal_failure = None;
        self.retry_attempts = 0;
        let mode = if let Some(active) = self.active.as_ref() {
            self.fence_signaling(active.generation);
            AttemptMode::Replace
        } else {
            AttemptMode::Fresh
        };
        self.start_attempt(mode);
    }

    fn start_attempt(&mut self, mode: AttemptMode) {
        self.admission_denial = None;
        if self.snapshot.terminal_failure.is_some() {
            return;
        }
        if self.replacing_joined(mode) && self.joined_successor.is_none() {
            self.fail_terminal(Failure {
                class: FailureClass::ResourceExpired,
                message: "joined recovery receipt unavailable; rejoin explicitly".into(),
            });
            return;
        }
        // Retain late-response cleanup fences rather than evicting unknown admissions.
        if self.orphaned_creates.len() >= MAX_ORPHANED_CREATES {
            self.fail_terminal(Failure {
                class: FailureClass::InvalidConfiguration,
                message: "pending admission cleanup capacity exhausted".into(),
            });
            return;
        }
        debug_assert!(self.attempt.is_none());
        let generation = self.ids.generation();
        let topic_registrations = self.desired.topics.clone();
        agent_log!(
            self,
            Info,
            "starting connection attempt mode={mode:?} generation={}",
            generation.get()
        );
        self.attempt = Some(Attempt {
            generation,
            join_revision: self.snapshot.join_revision,
            room: self.snapshot.desired_room.clone(),
            mode,
            stage: AttemptStage::Offering,
            resources: None,
            capability: None,
            request: None,
            candidate: None,
            answer_applied: false,
            transport_connected: false,
            signaling_open: false,
            early_signaling: VecDeque::new(),
            topic_registrations,
            open_channels: BTreeSet::new(),
            media_preparation: None,
            data_preparations: BTreeMap::new(),
        });
        let data_channels = vec![DataChannelSpec {
            label: signaling::SIGNALING_LABEL.to_string(),
            ordered: true,
            reliability: DataChannelReliability::Reliable,
            negotiated_id: Some(0),
        }];
        self.effects.push_back(Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
        }));
        if mode == AttemptMode::Fresh && self.active.is_none() {
            self.snapshot.generation = Some(generation);
            self.set_connection_state(ConnectionState::CreatingOffer);
        } else {
            self.set_connection_state(ConnectionState::Reconnecting);
        }
    }

    fn handle_rtc(&mut self, event: RtcEvent) -> Result<(), AgentError> {
        match event {
            RtcEvent::OfferCreated {
                generation,
                offer,
                resources,
            } => self.offer_created(generation, offer, resources),
            RtcEvent::BootstrapMediaPrepared {
                generation,
                preparation,
            } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                    && attempt.stage == AttemptStage::ApplyingAnswer
                    && attempt.media_preparation.is_none()
                {
                    attempt.media_preparation = Some(preparation.and_then(|bindings| {
                        let resources = attempt
                            .resources
                            .as_ref()
                            .ok_or(crate::client_sdp::Error::Observation)?;
                        let candidate = attempt
                            .candidate
                            .as_ref()
                            .ok_or(crate::client_sdp::Error::Observation)?;
                        crate::rtc_preparation::prepare_bootstrap_bindings(
                            bindings,
                            &resources.slots,
                            &candidate.capabilities.rtp,
                        )
                    }));
                }
                Ok(())
            }
            RtcEvent::MediaPrepared {
                generation,
                operation,
                slots,
                outcome,
                preparation,
            } => {
                if self
                    .active
                    .as_ref()
                    .and_then(|active| active.media_allocation.as_ref())
                    .is_some_and(|pending| pending.operation == operation)
                {
                    self.media_allocated(generation, operation, slots, outcome, preparation);
                } else {
                    self.media_retired(generation, operation, outcome, preparation);
                }
                Ok(())
            }
            RtcEvent::MediaObserved {
                generation,
                bindings,
            } => {
                self.media_observed(generation, bindings);
                Ok(())
            }
            RtcEvent::AnswerApplied { generation } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                {
                    attempt.answer_applied = true;
                    self.update_attempt_state();
                    self.try_activate()?;
                }
                Ok(())
            }
            RtcEvent::BootstrapRejected {
                generation,
                message,
            } => {
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.generation == generation)
                {
                    self.fail_attempt(Failure::protocol(message));
                }
                Ok(())
            }
            RtcEvent::Connected { generation } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                {
                    attempt.transport_connected = true;
                    self.update_attempt_state();
                    self.try_activate()?;
                }
                Ok(())
            }
            RtcEvent::Disconnected { generation } => {
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.generation == generation)
                {
                    self.fail_attempt(Failure::transient("candidate transport disconnected"));
                } else if self
                    .active
                    .as_ref()
                    .is_some_and(|session| session.generation == generation)
                {
                    self.fence_signaling(generation);
                    if self.desired.connected
                        && self.snapshot.terminal_failure.is_none()
                        && self.attempt.is_none()
                        && self.retry.is_none()
                    {
                        self.start_attempt(AttemptMode::Replace);
                    }
                }
                Ok(())
            }
            RtcEvent::Failed {
                generation,
                message,
            } => {
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.generation == generation)
                {
                    self.fail_attempt(Failure::transient(message));
                } else if self
                    .active
                    .as_ref()
                    .is_some_and(|session| session.generation == generation)
                {
                    self.fence_signaling(generation);
                    if self.desired.connected
                        && self.snapshot.terminal_failure.is_none()
                        && self.attempt.is_none()
                        && self.retry.is_none()
                    {
                        self.notify_failure(Failure::transient(message));
                        self.start_attempt(AttemptMode::Replace);
                    }
                }
                Ok(())
            }
            RtcEvent::Closed { generation } => {
                if let Some(closing) = self.closing.as_mut() {
                    let _ = closing.generations.remove(&generation);
                    self.finish_close_if_ready();
                }
                Ok(())
            }
        }
    }

    fn cancel_renewal(&mut self, generation: Generation) {
        if let Some(renewal) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
            .and_then(|active| active.pending_renewal.take())
        {
            self.cancel_credential_work(renewal);
        }
    }

    fn cancel_media(&mut self, generation: Generation) {
        if let Some(pending) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
            .and_then(|active| active.media_allocation.take())
        {
            self.effects
                .push_back(Effect::Rtc(RtcEffect::CancelMediaPreparation {
                    generation,
                    operation: pending.operation,
                }));
        }
        if let Some(pending) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
            .and_then(|active| active.media_retirement.take())
        {
            self.effects
                .push_back(Effect::Rtc(RtcEffect::CancelMediaPreparation {
                    generation,
                    operation: pending.operation,
                }));
        }
    }

    fn fence_signaling(&mut self, generation: Generation) {
        self.cancel_renewal(generation);
        self.cancel_media(generation);
        if let Some(active) = self.active.as_mut() {
            active.signaling_terminal = true;
        }
        self.pending_signal = None;
        self.cancel_signal_retry();
        self.cancel_reconciliation();
        self.topics.unbind_generation(
            generation,
            TopicDropReason::TransportReplaced,
            &mut self.snapshot,
            &mut self.notifications,
        );
    }

    fn offer_created(
        &mut self,
        generation: Generation,
        offer: String,
        resources: OfferResources,
    ) -> Result<(), AgentError> {
        let Some(attempt) = self.attempt.as_ref() else {
            return Ok(());
        };
        if attempt.generation != generation || attempt.stage != AttemptStage::Offering {
            return Ok(());
        }
        validate_offer(&offer, &resources, &attempt.topic_registrations)?;
        let prepared = CapabilityRequest {
            operation: None,
            credential: Credential {
                token: self.config.token.clone(),
                revision: self.config_credential_revision,
            },
            request: self.create_request(&offer)?,
            attempts: 0,
        };
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.stage = AttemptStage::Acquiring;
            attempt.resources = Some(resources);
        }
        if self.ownership.available() {
            self.claim_prepared(prepared);
        } else {
            if let Some(attempt) = self.attempt.as_mut() {
                attempt.capability = Some(prepared);
            }
            self.acquire_capability();
        }
        self.update_attempt_state();
        Ok(())
    }

    fn acquire_capability(&mut self) {
        let Some(attempt) = self.attempt.as_mut() else {
            return;
        };
        let Some(prepared) = attempt.capability.as_mut() else {
            return;
        };
        if prepared.operation.is_some() {
            return;
        }
        let operation = self.ids.operation();
        prepared.operation = Some(operation);
        prepared.attempts = prepared.attempts.saturating_add(1);
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: Some(attempt.generation),
            request: HttpRequest {
                method: HttpMethod::Post,
                uri: format!("{}/api/v1/native/admissions", self.config.endpoint),
                headers: admission_headers(&prepared.credential.token),
                body: match attempt.room.as_deref() {
                    Some(room) => serde_json::json!({"room":room}).to_string().into_bytes(),
                    None => b"{}".to_vec(),
                },
            },
        }));
    }

    fn capability_failed(&mut self, operation: OperationId, failure: Failure) -> bool {
        let Some(prepared) = self
            .attempt
            .as_mut()
            .and_then(|attempt| attempt.capability.as_mut())
            .filter(|prepared| prepared.operation == Some(operation))
        else {
            return false;
        };
        prepared.operation = None;
        let revision = prepared.credential.revision;
        let attempts = prepared.attempts;
        if failure.class == FailureClass::Transient && attempts < self.config.retry.maximum_attempts
        {
            self.notify_failure(failure);
            self.schedule_retry_target(RetryTarget::Capability);
        } else {
            let mode = self
                .attempt
                .as_ref()
                .map_or(AttemptMode::Fresh, |attempt| attempt.mode);
            if failure.class == FailureClass::Authorization {
                self.reject_admission_credential(revision, mode, failure);
            } else {
                self.fail_uncertain_admission(
                    revision,
                    Failure {
                        class: if failure.class == FailureClass::Transient {
                            FailureClass::RetryExhausted
                        } else {
                            failure.class
                        },
                        message: failure.message,
                    },
                );
            }
        }
        true
    }

    fn capability_response(&mut self, operation: OperationId, response: &HttpResponse) -> bool {
        if !self
            .attempt
            .as_ref()
            .and_then(|attempt| attempt.capability.as_ref())
            .is_some_and(|prepared| prepared.operation == Some(operation))
        {
            return false;
        }
        if response.status != 200 {
            return self.capability_failed(
                operation,
                classify_http_failure(response.status)
                    .unwrap_or_else(|| Failure::protocol("invalid admission capability response")),
            );
        }
        let Some(prepared) = self
            .attempt
            .as_mut()
            .and_then(|attempt| attempt.capability.take())
        else {
            return true;
        };
        if response.status != 200 || self.ownership.install(&response.body).is_err() {
            self.fail_uncertain_admission(
                prepared.credential.revision,
                Failure::protocol("invalid admission capability response"),
            );
            return true;
        }
        self.claim_prepared(prepared);
        true
    }

    fn claim_prepared(&mut self, prepared: CapabilityRequest) {
        let Some(attempt) = self.attempt.as_ref() else {
            return;
        };
        let generation = attempt.generation;
        let operation = self.ids.operation();
        let maximum_attempts = self.config.retry.maximum_attempts.min(
            u8::try_from(MAX_ORPHANED_CREATES.saturating_sub(self.orphaned_creates.len()))
                .unwrap_or(u8::MAX),
        );
        let result = self.ownership.create(
            operation,
            prepared.credential,
            generation,
            prepared.request,
            maximum_attempts,
        );
        match result {
            Ok((request, effect)) => {
                if let Some(attempt) = self.attempt.as_mut() {
                    attempt.stage = AttemptStage::Requesting;
                    attempt.request = Some(request);
                }
                self.effects.push_back(Effect::Http(effect));
            }
            Err(error) => self.fail_attempt(Failure {
                class: FailureClass::RetryExhausted,
                message: match error {
                    admission::Error::SequenceExhausted => "ownership sequence space exhausted",
                    _ => "native admission request cannot be claimed",
                }
                .into(),
            }),
        }
    }

    fn retry_creation(&mut self) {
        let operation = self.ids.operation();
        let result = self
            .attempt
            .as_mut()
            .filter(|attempt| attempt.stage == AttemptStage::Requesting)
            .and_then(|attempt| attempt.request.as_mut())
            .map(|request| request.retry(operation));
        match result {
            Some(Ok(effect)) => {
                self.effects.push_back(Effect::Http(effect));
                self.update_attempt_state();
            }
            Some(Err(_)) => self.fail_attempt(Failure {
                class: FailureClass::RetryExhausted,
                message: "native admission retry budget exhausted".into(),
            }),
            None => {}
        }
    }

    fn retry_uncertain_creation(&mut self, revision: u64, failure: Failure) {
        self.credential_outcome(
            revision,
            crate::CredentialStatus::Uncertain,
            Some(failure.clone()),
        );
        self.notify_failure(failure);
        if self
            .attempt
            .as_ref()
            .and_then(|attempt| attempt.request.as_ref())
            .is_some_and(CreateRequest::can_retry)
        {
            self.schedule_retry_target(RetryTarget::Creation);
        } else {
            self.fail_attempt(Failure {
                class: FailureClass::RetryExhausted,
                message: "native admission retry budget exhausted".into(),
            });
        }
    }

    fn handle_http(&mut self, event: HttpEvent) -> Result<(), AgentError> {
        match event {
            HttpEvent::Response {
                operation,
                response,
            } => self.http_response(operation, response),
            HttpEvent::Failed { operation, .. } => {
                if self.credential_http_failed(operation) {
                    return Ok(());
                }
                if self.remove_delete(operation) {
                    return Ok(());
                }
                if self.capability_failed(
                    operation,
                    Failure::transient("admission capability request failed"),
                ) {
                    return Ok(());
                }
                if let Some(revision) = self
                    .attempt
                    .as_mut()
                    .filter(|attempt| attempt.stage == AttemptStage::Requesting)
                    .and_then(|attempt| attempt.request.as_mut())
                    .and_then(|request| {
                        request
                            .failed(operation)
                            .then_some(request.credential.revision)
                    })
                {
                    self.retry_uncertain_creation(
                        revision,
                        Failure::transient("native admission outcome is uncertain"),
                    );
                }
                Ok(())
            }
        }
    }

    fn handle_unavailable_attempt(&mut self, operation: OperationId) -> bool {
        if let Some(attempt) = self.attempt.as_mut()
            && let Some(request) = attempt.request.as_mut()
            && request.is_latest_receipt(operation, &self.ownership)
        {
            if attempt.mode != AttemptMode::Replace
                || attempt.room.is_none()
                || self.joined_successor.is_none()
            {
                return false;
            }
            let answered = request.responded(operation, true).is_some();
            if attempt.stage == AttemptStage::Requesting
                && request.operation == operation
                && answered
            {
                let revision = request.credential.revision;
                if let Some(retry) = self.retry.take() {
                    self.effects
                        .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
                }
                self.ownership = admission::Ownership::default();
                self.fail_uncertain_admission(
                    revision,
                    Failure::transient("pinned native attempt is unavailable"),
                );
            }
            return true;
        }
        self.active.as_ref().is_some_and(|active| {
            active
                .admission
                .is_latest_receipt(operation, &self.ownership)
        })
    }

    fn http_response(
        &mut self,
        operation: OperationId,
        response: HttpResponse,
    ) -> Result<(), AgentError> {
        if self.credential_http_response(operation, &response) {
            return Ok(());
        }
        if self.remove_delete(operation) {
            return Ok(());
        }
        if self.capability_response(operation, &response) {
            return Ok(());
        }
        if admission::attempt_unavailable(&response) && self.handle_unavailable_attempt(operation) {
            return Ok(());
        }
        if matches!(response.status, 409 | 410)
            && (self
                .attempt
                .as_ref()
                .and_then(|attempt| attempt.request.as_ref())
                .is_some_and(|request| request.is_latest_receipt(operation, &self.ownership))
                || self.active.as_ref().is_some_and(|active| {
                    active
                        .admission
                        .is_latest_receipt(operation, &self.ownership)
                }))
        {
            if let Some(failure) = classify_http_failure(response.status) {
                if let Some(request) = self
                    .attempt
                    .as_mut()
                    .and_then(|attempt| attempt.request.as_mut())
                {
                    let _ = request.responded(operation, true);
                }
                let revision = self
                    .attempt
                    .as_ref()
                    .and_then(|attempt| attempt.request.as_ref())
                    .map(|request| request.credential.revision);
                if let Some(revision) = revision {
                    self.fail_uncertain_admission(revision, failure);
                } else {
                    self.fail_terminal(failure);
                }
            }
            return Ok(());
        }
        if let Some(credential) = self.orphaned_creates.remove(&operation) {
            if response.status / 100 == 2
                && let Ok((candidate, _)) = parse_create_response(&response, credential)
            {
                self.emit_untracked_delete(candidate.resource_uri, candidate.credential.token);
            }
            return Ok(());
        }
        let failure = classify_http_failure(response.status);
        let (mode, generation, credential, uncertain, current) = match self.attempt.as_mut() {
            Some(attempt) if attempt.stage == AttemptStage::Requesting => {
                let Some(request) = attempt.request.as_mut() else {
                    return Ok(());
                };
                let Some(uncertain) = request.responded(
                    operation,
                    failure
                        .as_ref()
                        .is_some_and(|failure| failure.class != FailureClass::Authorization),
                ) else {
                    return Ok(());
                };
                (
                    attempt.mode,
                    attempt.generation,
                    request.credential.clone(),
                    uncertain,
                    request.operation == operation,
                )
            }
            Some(_) | None => return Ok(()),
        };
        agent_log!(
            self,
            Debug,
            "received native response mode={mode:?} generation={} operation={} status={}",
            generation.get(),
            operation.get(),
            response.status,
        );
        let credential_revision = credential.revision;
        if let Some(failure) = failure {
            if !current
                && !matches!(
                    failure.class,
                    FailureClass::ParticipantSuperseded | FailureClass::ResourceExpired
                )
            {
                return Ok(());
            }
            if failure.class == FailureClass::Authorization && !uncertain {
                self.reject_admission_credential(credential_revision, mode, failure);
            } else if failure.class == FailureClass::Transient {
                self.retry_uncertain_creation(credential_revision, failure);
            } else {
                self.fail_uncertain_admission(credential_revision, failure);
            }
            return Ok(());
        }
        let (candidate, answer) = match parse_create_response(&response, credential) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.fail_uncertain_admission(credential_revision, Failure::protocol(message));
                return Ok(());
            }
        };
        if self
            .attempt
            .as_ref()
            .is_none_or(|attempt| attempt.room != candidate.room_id)
        {
            self.emit_untracked_delete(candidate.resource_uri, candidate.credential.token);
            self.fail_uncertain_admission(
                credential_revision,
                Failure::protocol("native response room does not match requested room"),
            );
            return Ok(());
        }
        let capabilities = candidate.capabilities.clone();
        if let Some(retry) = self.retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
        }
        if let Some(attempt) = self.attempt.as_mut() {
            if let Some(request) = attempt.request.as_mut() {
                for operation in request.accept() {
                    self.effects
                        .push_back(Effect::Http(HttpEffect::Cancel { operation }));
                }
            }
            attempt.stage = AttemptStage::ApplyingAnswer;
            attempt.candidate = Some(candidate);
        }
        self.effects.push_back(Effect::Rtc(RtcEffect::ApplyAnswer {
            generation,
            answer,
            capabilities,
        }));
        self.update_attempt_state();
        Ok(())
    }

    fn handle_timer(&mut self, event: TimerEvent) -> Result<(), AgentError> {
        match event {
            TimerEvent::Fired { timer } => {
                if self.credential_timer(timer) {
                    return Ok(());
                }
                if self
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.timer == timer)
                {
                    if let Some(retry) = self.retry.take()
                        && self.desired.connected
                    {
                        match retry.target {
                            RetryTarget::Connection(mode) => self.start_attempt(mode),
                            RetryTarget::Capability => self.acquire_capability(),
                            RetryTarget::Creation => self.retry_creation(),
                        }
                    }
                } else if self.reconciliation_wake == Some(timer) {
                    self.reconciliation_wake = None;
                    self.reconcile_current();
                } else if self.signal_retry == Some(timer) {
                    self.signal_retry = None;
                    self.request_reconciliation();
                } else if self.topic_probe == Some(timer) {
                    self.topic_probe = None;
                    if self.active.is_some() && self.desired.connected {
                        self.topics
                            .probe_remote_publishers(&mut self.ids, &mut self.effects);
                    }
                    self.schedule_topic_probe();
                } else {
                    self.topics.expire_data_send(
                        timer,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                }
            }
        }
        Ok(())
    }

    fn handle_data_channel(&mut self, event: DataChannelEvent) -> Result<(), AgentError> {
        match event {
            DataChannelEvent::Prepared {
                generation,
                channel,
                preparation,
            } => {
                let preparation = preparation.and_then(|properties| {
                    properties.validate().map_err(str::to_string)?;
                    Ok(properties)
                });
                let preparations = if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                    && attempt.resources.as_ref().is_some_and(|resources| {
                        resources.signaling_channel == channel
                            || resources
                                .data_channels
                                .iter()
                                .any(|binding| binding.channel == channel)
                    }) {
                    Some(&mut attempt.data_preparations)
                } else if let Some(active) = self.active.as_mut()
                    && active.generation == generation
                    && !active.signaling_terminal
                    && (active.signaling_channel == channel
                        || active.payloads.contains_key(&channel))
                {
                    Some(&mut active.data_preparations)
                } else {
                    None
                };
                if let Some(preparations) = preparations {
                    let preparation = match (preparations.get(&channel), preparation) {
                        (Some(Err(error)), _) => Err(error.clone()),
                        (Some(Ok(old)), Ok(mut next)) => {
                            if old.stream_id != next.stream_id
                                || old.negotiated != next.negotiated
                                || old.ordered != next.ordered
                                || old.reliability != next.reliability
                                || matches!((old.association_max_channels, next.association_max_channels), (Some(old), Some(next)) if next < old)
                            {
                                Err("prepared channel identity changed".to_string())
                            } else {
                                next.association_max_channels = next
                                    .association_max_channels
                                    .or(old.association_max_channels);
                                Ok(next)
                            }
                        }
                        (_, preparation) => preparation,
                    };
                    let changed = preparations.get(&channel) != Some(&preparation);
                    preparations.insert(channel, preparation);
                    if changed
                        && let Some(active) = self.active.as_mut().filter(|active| {
                            active.generation == generation && !active.signaling_terminal
                        })
                    {
                        self.intent_dirty = true;
                        if active.signaling_channel == channel {
                            self.intent_dirty = true;
                        } else if let Some(error) = active
                            .data_preparations
                            .get(&channel)
                            .and_then(|value| value.as_ref().err())
                            .cloned()
                        {
                            if let Some(physical) = active
                                .payloads
                                .get_mut(&channel)
                                .filter(|physical| physical.state == native::PayloadState::Active)
                            {
                                physical.state = native::PayloadState::Closing;
                                if let Some(offer) = physical.offer {
                                    active.state.report_logical_receiver(&physical.resource, offer,
                                        pulsebeam_proto::state::logical_receiver_report::Outcome::Failure(
                                            pulsebeam_proto::state::Failure { code: pulsebeam_proto::state::FailureCode::InternalFailure as i32, ..Default::default() }
                                        ))?;
                                } else {
                                    active.state.stage(physical.resource.clone(), None)?;
                                }
                                self.intent_dirty = true;
                                self.topics.suspend_source(generation, channel);
                                self.effects
                                    .push_back(Effect::Rtc(RtcEffect::CloseDataChannel {
                                        generation,
                                        channel,
                                    }));
                                self.notifications
                                    .push_back(Notification::ServerError(error));
                            }
                        }
                    }
                }
            }
            DataChannelEvent::Opened {
                generation,
                channel,
            } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                {
                    let recognized = if attempt
                        .resources
                        .as_ref()
                        .is_some_and(|resources| resources.signaling_channel == channel)
                    {
                        attempt.signaling_open = true;
                        true
                    } else if attempt.resources.as_ref().is_some_and(|resources| {
                        resources
                            .data_channels
                            .iter()
                            .any(|binding| binding.channel == channel)
                    }) {
                        let _ = attempt.open_channels.insert(channel);
                        true
                    } else {
                        false
                    };
                    if !recognized {
                        return Err(TopicError::UnknownChannel.into());
                    }
                    self.update_attempt_state();
                    self.try_activate()?;
                } else if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
                    && !self.active.as_ref().is_some_and(|active| {
                        active.signaling_channel == channel
                            || active.payloads.contains_key(&channel)
                    })
                {
                    return Ok(());
                }
            }
            DataChannelEvent::Closed {
                generation,
                channel,
                reset_complete,
            } => {
                let native_payload = self.active.as_ref().is_some_and(|active| {
                    active.generation == generation && active.payloads.contains_key(&channel)
                });
                let logical_payload = self.active.as_ref().is_some_and(|active| {
                    active.generation == generation
                        && active
                            .payloads
                            .get(&channel)
                            .is_some_and(|physical| physical.offer.is_some())
                });
                let signaling_closed = self.active.as_ref().is_some_and(|active| {
                    active.generation == generation && active.signaling_channel == channel
                });
                if let Some(attempt) = self
                    .attempt
                    .as_mut()
                    .filter(|attempt| attempt.generation == generation)
                {
                    attempt.data_preparations.remove(&channel);
                }
                if let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|active| active.generation == generation)
                {
                    active.data_preparations.remove(&channel);
                    if let Some(physical) = active.payloads.get_mut(&channel) {
                        if physical.state == native::PayloadState::Active {
                            physical.restart = !active.state.retaining(&physical.resource);
                        }
                        let previous_reset = matches!(
                            physical.state,
                            native::PayloadState::Closed {
                                reset_complete: true
                            }
                        );
                        let closed = native::PayloadState::Closed {
                            reset_complete: previous_reset || reset_complete,
                        };
                        if physical.state != closed {
                            physical.state = closed;
                            if let Some(offer) = physical.offer {
                                active.state.report_logical_receiver(&physical.resource, offer,
                                    pulsebeam_proto::state::logical_receiver_report::Outcome::Failure(
                                        pulsebeam_proto::state::Failure { code: pulsebeam_proto::state::FailureCode::ResourceGone as i32, ..Default::default() }
                                    ))?;
                            } else {
                                active.state.stage(physical.resource.clone(), None)?;
                            }
                            self.intent_dirty = true;
                        }
                    }
                    if active.signaling_channel == channel {
                        active.signaling_terminal = true;
                    }
                }
                if signaling_closed {
                    self.cancel_renewal(generation);
                    self.cancel_media(generation);
                    self.cancel_reconciliation();
                }
                let candidate_channel = self.attempt.as_ref().is_some_and(|attempt| {
                    attempt.generation == generation
                        && attempt.resources.as_ref().is_some_and(|resources| {
                            resources.signaling_channel == channel
                                || resources
                                    .data_channels
                                    .iter()
                                    .any(|binding| binding.channel == channel)
                        })
                });
                if candidate_channel {
                    self.fail_attempt(Failure::transient("candidate data channel closed"));
                    return Ok(());
                }
                if signaling_closed
                    && self.desired.connected
                    && self.attempt.is_none()
                    && self.retry.is_none()
                {
                    self.topics.unbind_generation(
                        generation,
                        TopicDropReason::ChannelClosed,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                    self.start_attempt(AttemptMode::Replace);
                } else if logical_payload {
                    self.topics.detach_source(
                        generation,
                        channel,
                        false,
                        &mut self.snapshot,
                        &mut self.notifications,
                        &mut self.effects,
                    );
                } else {
                    let topic_closed = self.topics.channel_closed(
                        generation,
                        channel,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                    if topic_closed
                        && !native_payload
                        && self.desired.connected
                        && self.attempt.is_none()
                        && self.retry.is_none()
                    {
                        self.topics.unbind_generation(
                            generation,
                            TopicDropReason::TransportReplaced,
                            &mut self.snapshot,
                            &mut self.notifications,
                        );
                        self.start_attempt(AttemptMode::Replace);
                    }
                }
            }
            DataChannelEvent::Message {
                generation,
                channel,
                payload,
            } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                    && attempt
                        .resources
                        .as_ref()
                        .is_some_and(|resources| resources.signaling_channel == channel)
                {
                    if attempt.early_signaling.len() == MAX_EARLY_SIGNALING_MESSAGES {
                        self.fail_attempt(Failure::transient("early signaling buffer exhausted"));
                        return Ok(());
                    }
                    attempt.early_signaling.push_back(payload);
                    return Ok(());
                }
                let Some(active) = self.active.as_mut() else {
                    return Ok(());
                };
                if active.generation != generation {
                    return Ok(());
                }
                if active.signaling_channel != channel {
                    if !self.topics.has_channel(generation, channel) {
                        return Ok(());
                    }
                    let _ = self.topics.handle_message(
                        generation,
                        channel,
                        payload,
                        &mut self.ids,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    )?;
                    return Ok(());
                }
                if active.signaling_terminal {
                    return Ok(());
                }
                if let Err(error) = self.apply_native(&payload) {
                    self.fail_terminal(Failure::protocol(error.to_string()));
                    return Err(error);
                }
            }
            DataChannelEvent::Sent {
                operation,
                generation,
                channel,
            } => {
                if self.pending_signal.as_ref().is_some_and(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                }) {
                    agent_log!(
                        self,
                        Debug,
                        "native control sent generation={} operation={} channel={}",
                        generation.get(),
                        operation.get(),
                        channel.get(),
                    );
                    self.pending_signal = None;
                    self.request_reconciliation();
                } else {
                    let _ = self.topics.handle_sent(
                        operation,
                        generation,
                        channel,
                        &mut self.ids,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                }
            }
            DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                message,
            } => {
                if let Some(pending) = self.pending_signal.as_mut().filter(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                }) {
                    pending.retrying = true;
                    self.notify_failure(Failure::transient(message));
                    self.schedule_signal_retry();
                } else {
                    let _ = self.topics.handle_send_failed(
                        operation,
                        generation,
                        channel,
                        message,
                        &mut self.ids,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                }
            }
        }
        Ok(())
    }

    fn try_activate(&mut self) -> Result<(), AgentError> {
        let ready = self.attempt.as_ref().is_some_and(|attempt| {
            attempt.candidate.is_some()
                && attempt.resources.is_some()
                && attempt.answer_applied
                && attempt.transport_connected
                && attempt.signaling_open
                && attempt.resources.as_ref().is_some_and(|resources| {
                    resources
                        .data_channels
                        .iter()
                        .all(|binding| attempt.open_channels.contains(&binding.channel))
                })
        });
        if !ready {
            return Ok(());
        }
        let Some(mut attempt) = self.attempt.take() else {
            return Ok(());
        };
        let Some(candidate) = attempt.candidate.take() else {
            debug_assert!(false, "ready attempt must have candidate metadata");
            return Ok(());
        };
        let Some(resources) = attempt.resources.take() else {
            debug_assert!(false, "ready attempt must have offer resources");
            return Ok(());
        };
        let Some(admission) = attempt.request.take() else {
            debug_assert!(false, "ready attempt must have admission receipts");
            return Ok(());
        };
        let coordinates = resources
            .slots
            .into_iter()
            .map(|binding| (binding.slot.clone(), Some(binding)))
            .collect();
        let state = NativeState::new(
            native::STATE_MEMORY_BYTES,
            candidate
                .room_id
                .as_ref()
                .map(|room_id| pulsebeam_proto::state::Participant {
                    participant_id: candidate.participant_id.clone(),
                    room_id: room_id.clone(),
                }),
        )?;
        self.retry_attempts = 0;
        self.snapshot.evaluated_join_revision = attempt.join_revision;
        self.snapshot.accepted_join_revision = Some(attempt.join_revision);
        let previous_generation = self.active.as_ref().map(|session| session.generation);
        self.joined_successor = None;
        if let Some(previous) = self.active.replace(Session {
            credential: candidate.credential,
            admission: admission.into_accepted_receipts(),
            generation: attempt.generation,
            connection_id: candidate.connection_id,
            resource_uri: candidate.resource_uri,
            participant_id: candidate.participant_id,
            room_id: candidate.room_id,
            coordinates,
            state,
            payloads: BTreeMap::new(),
            payload_resources: BTreeMap::new(),
            next_stream: 1,
            reusable_streams: BTreeSet::new(),
            media_leases: BTreeMap::new(),
            media_retirement: None,
            media_allocation: None,
            media_wanted: BTreeMap::new(),
            media_blocked: BTreeMap::new(),

            authorization_expires_at: None,
            pending_renewal: None,
            authorization_revision: 0,
            signaling_terminal: false,
            signaling_channel: resources.signaling_channel,
            data_preparations: attempt.data_preparations,
            capabilities: candidate.capabilities,
            media_preparation: attempt.media_preparation,
        }) && previous.generation != attempt.generation
        {
            if let Some(renewal) = previous.pending_renewal {
                self.cancel_credential_work(renewal);
            }
            self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                generation: previous.generation,
            }));
        }
        agent_log!(
            self,
            Info,
            "activated connection generation={} replaced_generation={:?}",
            attempt.generation.get(),
            previous_generation.map(Generation::get),
        );
        self.pending_signal = None;
        self.cancel_signal_retry();
        self.clear_observed_state();
        self.intent_dirty = true;
        if let Some(active) = &self.active {
            self.snapshot.generation = Some(active.generation);
            self.snapshot.connection_id = Some(active.connection_id.clone());
            self.snapshot.participant_id = Some(active.participant_id.clone());
            self.snapshot.room_id = active.room_id.clone();
            self.snapshot.authorization_expires_at = None;
            self.snapshot.accepted_credential_revision = Some(active.credential.revision);
            self.topics.bind_native(
                active.generation,
                active.participant_id.clone(),
                &mut self.snapshot,
                &mut self.notifications,
            );
            self.topics
                .probe_remote_publishers(&mut self.ids, &mut self.effects);
        }
        if let Some(revision) = self.snapshot.accepted_credential_revision {
            self.credential_accepted(revision);
        }
        self.set_connection_state(ConnectionState::Connected);
        for payload in attempt.early_signaling {
            self.handle_data_channel(DataChannelEvent::Message {
                generation: attempt.generation,
                channel: resources.signaling_channel,
                payload,
            })?;
        }
        self.refresh_resources()?;
        self.request_reconciliation();
        self.schedule_topic_probe();
        Ok(())
    }

    fn update_attempt_state(&mut self) {
        if self.active.is_some() {
            self.set_connection_state(ConnectionState::Reconnecting);
            return;
        }
        let Some(attempt) = self.attempt.as_ref() else {
            return;
        };
        let state = match attempt.stage {
            AttemptStage::Offering => ConnectionState::CreatingOffer,
            AttemptStage::Acquiring | AttemptStage::Requesting => ConnectionState::Joining,
            AttemptStage::ApplyingAnswer if !attempt.answer_applied => {
                ConnectionState::ApplyingAnswer
            }
            AttemptStage::ApplyingAnswer if !attempt.transport_connected => {
                ConnectionState::WaitingForTransport
            }
            AttemptStage::ApplyingAnswer => ConnectionState::WaitingForSignaling,
        };
        self.set_connection_state(state);
    }

    fn fail_attempt(&mut self, failure: Failure) {
        let Some(mut attempt) = self.attempt.take() else {
            return;
        };
        self.snapshot.evaluated_join_revision = attempt.join_revision;
        if let Some(retry) = self.retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
        }
        agent_log!(
            self,
            Warn,
            "connection attempt failed mode={:?} generation={} class={:?}",
            attempt.mode,
            attempt.generation.get(),
            failure.class,
        );
        self.effects.push_back(Effect::Rtc(RtcEffect::Close {
            generation: attempt.generation,
        }));
        if let Some(capability) = attempt.capability.take()
            && let Some(operation) = capability.operation
        {
            self.effects
                .push_back(Effect::Http(HttpEffect::Cancel { operation }));
        }
        if let Some(request) = attempt.request.take() {
            self.retire_creation(request);
        }
        if let Some(candidate) = attempt.candidate.take() {
            self.emit_untracked_delete(candidate.resource_uri, candidate.credential.token);
        }
        self.pending_signal = None;
        self.intent_dirty = true;
        if failure.class == FailureClass::Authorization && self.retry_denied_admission() {
            return;
        }
        match failure.class {
            FailureClass::Transient => {
                self.notify_failure(failure);
                self.schedule_retry(attempt.mode);
            }
            FailureClass::InvalidConfiguration
            | FailureClass::Authorization
            | FailureClass::ParticipantSuperseded
            | FailureClass::Protocol
            | FailureClass::ResourceExpired
            | FailureClass::RetryExhausted => self.fail_terminal(failure),
        }
    }

    fn fail_terminal(&mut self, failure: Failure) {
        if failure.class != FailureClass::Authorization {
            self.admission_denial = None;
        }
        if self.snapshot.terminal_failure.is_some() {
            return;
        }
        if matches!(
            failure.class,
            FailureClass::ParticipantSuperseded | FailureClass::ResourceExpired
        ) {
            self.joined_successor = None;
        }
        if self.attempt.is_some() {
            self.fail_attempt(failure);
            return;
        }
        if let Some(retry) = self.retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
        }
        self.pending_signal = None;
        self.cancel_signal_retry();
        if let Some(timer) = self.topic_probe.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
        if let Some(active) = self.active.as_ref() {
            self.fence_signaling(active.generation);
        }
        if matches!(
            failure.class,
            FailureClass::ParticipantSuperseded | FailureClass::ResourceExpired
        ) {
            if let Some(active) = self.active.as_ref() {
                self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                    generation: active.generation,
                }));
            }
            self.snapshot.connection_id = None;
            self.snapshot.room_id = None;
            self.snapshot.accepted_join_revision = None;
            self.clear_observed_state();
        }
        self.notify_failure(failure.clone());
        self.snapshot.terminal_failure = Some(failure);
        self.set_connection_state(ConnectionState::TerminalFailure);
    }

    fn schedule_retry(&mut self, mode: AttemptMode) {
        self.schedule_retry_target(RetryTarget::Connection(mode));
    }

    fn schedule_retry_target(&mut self, target: RetryTarget) {
        if self.retry.is_some() {
            return;
        }
        self.retry_attempts = self.retry_attempts.saturating_add(1);
        if self.retry_attempts >= self.config.retry.maximum_attempts {
            self.fail_terminal(Failure {
                class: FailureClass::RetryExhausted,
                message: "connection retry budget exhausted".into(),
            });
            return;
        }
        let shift = u32::from(self.retry_attempts.saturating_sub(1).min(10));
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let ceiling = self.config.retry.maximum_delay.min(Duration::from_secs(10));
        let base = self
            .config
            .retry
            .initial_delay
            .checked_mul(multiplier)
            .unwrap_or(ceiling)
            .min(ceiling);
        let timer = self.ids.timer();
        let mut entropy = timer.get().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        entropy ^= entropy >> 30;
        entropy = entropy.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        entropy ^= entropy >> 27;
        let jitter = u64::try_from(
            u128::from(entropy & u64::from(u32::MAX))
                .saturating_mul(base.as_millis().saturating_div(2).saturating_add(1))
                .checked_div(u128::from(u32::MAX).saturating_add(1))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        let delay = base
            .checked_div(2)
            .unwrap_or_default()
            .saturating_add(Duration::from_millis(jitter));
        agent_log!(
            self,
            Info,
            "scheduled connection retry target={target:?} attempt={} timer={} delay_ms={}",
            self.retry_attempts,
            timer.get(),
            delay.as_millis(),
        );
        self.retry = Some(Retry { timer, target });
        self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: delay,
        }));
        self.set_connection_state(ConnectionState::RetryWaiting {
            attempt: self.retry_attempts,
            after: delay,
        });
    }

    fn request_reconciliation(&mut self) {
        if self.reconciliation_wake.is_some()
            || self.snapshot.terminal_failure.is_some()
            || self.closing.is_some()
            || self
                .active
                .as_ref()
                .is_none_or(|active| active.signaling_terminal)
        {
            return;
        }
        let timer = self.ids.timer();
        self.reconciliation_wake = Some(timer);
        self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: Duration::ZERO,
        }));
    }

    fn cancel_reconciliation(&mut self) {
        if let Some(timer) = self.reconciliation_wake.take() {
            let queued = self.effects.iter().position(|effect| {
                matches!(effect, Effect::Timer(TimerEffect::Schedule { timer: pending, .. }) if *pending == timer)
            });
            if let Some(index) = queued {
                self.effects.remove(index);
            } else {
                self.effects
                    .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
            }
        }
    }

    pub fn catalog_watch(
        &mut self,
        room: String,
        config: crate::ffi::WatchConfig,
    ) -> Result<crate::TrackOwner, crate::ffi::ErrorInfo> {
        let owner = self.resources.new_owner().map_err(|_| {
            crate::ffi::ErrorInfo::new(
                crate::ffi::ErrorCode::CapacityExceeded,
                "catalog owner identity exhausted",
            )
        })?;
        self.catalogs.open(owner, room, config)?;
        self.request_reconciliation();
        Ok(owner)
    }

    pub fn catalog_watch_state(&self, owner: crate::TrackOwner) -> Option<crate::ffi::WatchState> {
        self.catalogs.state(owner).or_else(|| {
            self.notifications
                .iter()
                .chain(self.published_notifications.iter())
                .find_map(|notification| match notification {
                    Notification::CatalogWatchTerminated {
                        owner: ended,
                        state,
                    } if *ended == owner.id() => Some(state.clone()),
                    _ => None,
                })
        })
    }

    pub fn close_catalog_watch(
        &mut self,
        owner: crate::TrackOwner,
    ) -> Option<crate::ffi::WatchState> {
        let state = self.catalogs.close(owner)?;
        self.request_reconciliation();
        Some(state)
    }

    fn reconcile_current(&mut self) {
        if self.snapshot.terminal_failure.is_some() || self.closing.is_some() {
            return;
        }
        self.reconcile_credentials();
        self.topics.reconcile(
            &self.desired.topics,
            &mut self.snapshot,
            &mut self.notifications,
        );
        self.schedule_topic_probe();
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| !active.signaling_terminal)
        else {
            return;
        };
        if let Err(error) = self
            .catalogs
            .reconcile(active.generation, &mut active.state)
        {
            self.fail_terminal(Failure::protocol(error.to_string()));
            return;
        }
        if self.intent_dirty {
            self.intent_dirty = false;
            if let Err(error) = native::reconcile(
                active,
                &mut self.logs,
                &self.desired,
                &mut self.ids,
                &mut self.effects,
            ) {
                self.notifications
                    .push_back(Notification::ServerError(error.to_string()));
            }
            if native::project_retired_receivers(active, &mut self.snapshot) {
                self.snapshot.version = self.snapshot.version.saturating_add(1);
                self.notifications.push_back(Notification::SnapshotChanged);
            }
        }
        if self.signal_retry.is_some() {
            return;
        }
        if let Some(pending) = self.pending_signal.as_mut() {
            if !pending.retrying {
                return;
            }
            pending.operation = self.ids.operation();
            pending.retrying = false;
            self.effects
                .push_back(Effect::DataChannel(DataChannelEffect::Send {
                    operation: pending.operation,
                    generation: pending.generation,
                    channel: pending.channel,
                    binary: true,
                    payload: pending.payload.clone(),
                }));
            return;
        }
        let payload = match active.state.next_frame() {
            Ok(Some(payload)) => payload,
            Ok(None) => return,
            Err(error) => {
                self.fail_terminal(Failure::protocol(error.to_string()));
                return;
            }
        };
        let operation = self.ids.operation();
        self.effects
            .push_back(Effect::DataChannel(DataChannelEffect::Send {
                operation,
                generation: active.generation,
                channel: active.signaling_channel,
                binary: true,
                payload: payload.clone(),
            }));
        self.pending_signal = Some(PendingSignal {
            operation,
            generation: active.generation,
            channel: active.signaling_channel,
            payload,
            retrying: false,
        });
    }

    fn schedule_signal_retry(&mut self) {
        if self.signal_retry.is_some() || self.active.is_none() {
            return;
        }
        let timer = self.ids.timer();
        agent_log!(
            self,
            Info,
            "scheduled signaling retry timer={} delay_ms={}",
            timer.get(),
            SIGNAL_RETRY_DELAY.as_millis(),
        );
        self.signal_retry = Some(timer);
        self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: SIGNAL_RETRY_DELAY,
        }));
    }

    fn schedule_topic_probe(&mut self) {
        if self.topic_probe.is_some()
            || self.snapshot.terminal_failure.is_some()
            || !self.desired.connected
            || !self.topics.has_ordered_subscriptions()
            || self.active.is_none()
        {
            return;
        }
        let timer = self.ids.timer();
        self.topic_probe = Some(timer);
        self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: Duration::from_secs(2),
        }));
    }

    fn cancel_signal_retry(&mut self) {
        if let Some(timer) = self.signal_retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }

    fn begin_close(&mut self) {
        self.catalogs.disconnect();
        self.admission_denial = None;
        if self.closing.is_some() {
            return;
        }
        agent_log!(
            self,
            Info,
            "closing agent session active={} candidate={} retry_pending={}",
            self.active.is_some(),
            self.attempt.is_some(),
            self.retry.is_some(),
        );
        if let Some(retry) = self.retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer: retry.timer }));
        }
        self.cancel_signal_retry();
        self.cancel_reconciliation();
        if let Some(timer) = self.topic_probe.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
        self.pending_signal = None;
        if let Some(active) = self.active.as_ref() {
            self.topics.unbind_generation(
                active.generation,
                TopicDropReason::ChannelClosed,
                &mut self.snapshot,
                &mut self.notifications,
            );
        }
        if !self.desired.connected {
            self.topics
                .end_session(&mut self.snapshot, &mut self.notifications);
        }
        let mut closing = Closing::default();
        if let Some(mut attempt) = self.attempt.take() {
            let _ = closing.generations.insert(attempt.generation);
            self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                generation: attempt.generation,
            }));
            if let Some(capability) = attempt.capability.take()
                && let Some(operation) = capability.operation
            {
                self.effects
                    .push_back(Effect::Http(HttpEffect::Cancel { operation }));
            }
            if let Some(request) = attempt.request.take() {
                self.retire_creation(request);
            }
            if let Some(candidate) = attempt.candidate.take() {
                self.track_delete(
                    candidate.resource_uri,
                    candidate.credential.token,
                    &mut closing,
                );
            }
        }
        if let Some(active) = self.active.take() {
            if let Some(renewal) = active.pending_renewal {
                self.cancel_credential_work(renewal);
            }
            if closing.generations.insert(active.generation) {
                self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                    generation: active.generation,
                }));
            }
            self.track_delete(active.resource_uri, active.credential.token, &mut closing);
        }
        self.retry_attempts = 0;
        if closing.generations.is_empty() && closing.deletes.is_empty() {
            self.finish_disconnected();
        } else {
            self.closing = Some(closing);
            if !self.participant_superseded() {
                self.set_connection_state(ConnectionState::Closing);
            }
        }
    }

    fn retire_creation(&mut self, request: CreateRequest) {
        for (operation, pending) in request.unsettled() {
            if pending {
                self.effects
                    .push_back(Effect::Http(HttpEffect::Cancel { operation }));
            }
            let _ = self
                .orphaned_creates
                .insert(operation, request.credential.clone());
        }
    }

    fn track_delete(&mut self, resource_uri: String, token: String, closing: &mut Closing) {
        let operation = self.ids.operation();
        let _ = closing.deletes.insert(operation);
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request: delete_request(resource_uri, &token),
        }));
    }

    fn emit_untracked_delete(&mut self, resource_uri: String, token: String) {
        let operation = self.ids.operation();
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request: delete_request(resource_uri, &token),
        }));
    }

    fn remove_delete(&mut self, operation: OperationId) -> bool {
        let Some(closing) = self.closing.as_mut() else {
            return false;
        };
        if !closing.deletes.remove(&operation) {
            return false;
        }
        self.finish_close_if_ready();
        true
    }

    fn finish_close_if_ready(&mut self) {
        let ready = self
            .closing
            .as_ref()
            .is_some_and(|closing| closing.generations.is_empty() && closing.deletes.is_empty());
        if ready {
            self.closing = None;
            self.finish_disconnected();
        }
    }

    fn finish_disconnected(&mut self) {
        agent_log!(self, Info, "agent session disconnected");
        self.joined_successor = None;
        self.catalogs.disconnect();
        self.active = None;
        self.attempt = None;
        self.retry = None;
        self.pending_signal = None;
        self.snapshot.generation = None;
        self.snapshot.connection_id = None;
        self.snapshot.participant_id = None;
        self.snapshot.room_id = None;
        self.snapshot.accepted_join_revision = None;
        self.snapshot.authorization_expires_at = None;
        self.snapshot.accepted_credential_revision = None;
        self.clear_observed_state();
        if self.participant_superseded() {
            self.set_connection_state(ConnectionState::TerminalFailure);
            return;
        }
        self.snapshot.terminal_failure = None;
        self.set_connection_state(ConnectionState::Disconnected);
        if self.desired.connected {
            self.start_attempt(AttemptMode::Fresh);
        }
    }

    fn clear_observed_state(&mut self) {
        self.notifications.push_back(Notification::SnapshotChanged);
        self.snapshot.catalog_revision = 0;
        self.snapshot.accepted_intent_revision = 0;
        self.snapshot.video_mapping.clear();
        self.snapshot.audio_mapping.clear();
        self.snapshot.participants.clear();
        self.snapshot.publications.clear();
        self.snapshot.remote_tracks.clear();
        self.bump_snapshot();
    }

    fn replacing_joined(&self, mode: AttemptMode) -> bool {
        mode == AttemptMode::Replace && self.snapshot.desired_room.is_some()
    }

    fn create_request(&self, offer: &str) -> Result<HttpRequest, AgentError> {
        let body = serde_json::to_vec(&NativeRequest {
            profile: pulsebeam_proto::state::PROFILE,
            offer,
            room: self
                .attempt
                .as_ref()
                .and_then(|attempt| attempt.room.as_deref()),
            successor_receipt: self
                .attempt
                .as_ref()
                .filter(|attempt| self.replacing_joined(attempt.mode))
                .and(self.joined_successor.as_ref())
                .map(|receipt| receipt.proof.as_slice()),
        })
        .map_err(|_| AgentError::InvalidOffer("SDP offer could not be encoded"))?;
        if body.len() > admission::MAX_CREATE_BODY_BYTES {
            return Err(AgentError::InvalidOffer("native JSON request is too large"));
        }
        Ok(HttpRequest {
            method: HttpMethod::Post,
            uri: format!("{}/api/v1/native", self.config.endpoint),
            headers: admission_headers(&self.config.token),
            body,
        })
    }

    fn notify_failure(&mut self, failure: Failure) {
        self.notifications.push_back(Notification::Failure(failure));
    }

    fn set_connection_state(&mut self, state: ConnectionState) {
        if self.snapshot.connection == state {
            return;
        }
        let from = self.snapshot.connection.clone();
        agent_log!(
            self,
            Debug,
            "connection state changed from={from:?} to={state:?}"
        );
        self.snapshot.connection = state.clone();
        self.snapshot.version = self.snapshot.version.saturating_add(1);
        self.notifications
            .push_back(Notification::ConnectionStateChanged { from, to: state });
    }

    fn bump_snapshot(&mut self) {
        self.snapshot.version = self.snapshot.version.saturating_add(1);
    }
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Transient,
            message: message.into(),
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Protocol,
            message: message.into(),
        }
    }
}

fn validate_offer(
    offer: &str,
    resources: &OfferResources,
    _registrations: &crate::TopicRegistrations,
) -> Result<(), AgentError> {
    const MAX_SDP_BYTES: usize = 131_072;
    if offer.is_empty() || offer.len() > MAX_SDP_BYTES {
        return Err(AgentError::InvalidOffer("SDP offer is empty or too large"));
    }
    let mut actual = BTreeSet::new();
    let mut mids = BTreeSet::new();
    for SlotBinding { slot, mid, .. } in &resources.slots {
        if mid.is_empty()
            || mid.len() > crate::MAX_MID_BYTES
            || mid.chars().any(char::is_control)
            || mid.contains('/')
        {
            return Err(AgentError::InvalidOffer("negotiated mid is invalid"));
        }
        if !actual.insert(slot.clone()) {
            return Err(AgentError::InvalidOffer("slot is mapped more than once"));
        }
        if !mids.insert(mid.as_str()) {
            return Err(AgentError::InvalidOffer("mid is mapped more than once"));
        }
    }
    let actual_bindings = crate::negotiated_slot_bindings(
        offer,
        resources
            .slots
            .iter()
            .map(|binding| (binding.slot.clone(), binding.mid.clone())),
    )
    .map_err(AgentError::InvalidOffer)?;
    let expected_bindings: BTreeMap<_, _> = actual_bindings
        .into_iter()
        .map(|binding| (binding.slot.clone(), binding))
        .collect();
    if resources
        .slots
        .iter()
        .any(|binding| expected_bindings.get(&binding.slot) != Some(binding))
    {
        return Err(AgentError::InvalidOffer(
            "negotiated media coordinates do not match the SDP offer",
        ));
    }
    let expected_labels = BTreeSet::new();
    let mut actual_labels = BTreeSet::new();
    let mut channels = BTreeSet::new();
    let _ = channels.insert(resources.signaling_channel);
    for binding in &resources.data_channels {
        if !actual_labels.insert(binding.label.clone()) {
            return Err(AgentError::InvalidOffer(
                "data channel label is mapped more than once",
            ));
        }
        if !channels.insert(binding.channel) {
            return Err(AgentError::InvalidOffer(
                "data channel is mapped more than once",
            ));
        }
    }
    if actual_labels != expected_labels {
        return Err(AgentError::InvalidOffer(
            "data channel mapping does not match topic registrations",
        ));
    }
    Ok(())
}

fn delete_request(resource_uri: String, token: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Delete,
        uri: resource_uri,
        headers: request_headers(token, false),
        body: vec![],
    }
}

fn admission_headers(token: &str) -> Vec<HttpHeader> {
    let mut headers = request_headers(token, true);
    headers.push(HttpHeader {
        name: "Accept-Encoding".into(),
        value: "identity".into(),
    });
    headers
}

fn request_headers(token: &str, json: bool) -> Vec<HttpHeader> {
    let mut headers = vec![HttpHeader {
        name: "Authorization".to_string(),
        value: format!("Bearer {token}"),
    }];
    if json {
        headers.push(HttpHeader {
            name: CONTENT_TYPE.to_string(),
            value: JSON_CONTENT_TYPE.to_string(),
        });
    }
    headers
}

fn classify_http_failure(status: u16) -> Option<Failure> {
    if status == 201 {
        return None;
    }
    let (class, message) = match status {
        401 | 403 => (FailureClass::Authorization, "server rejected authorization"),
        409 => (
            FailureClass::ParticipantSuperseded,
            "participant session was superseded",
        ),
        408 | 425 | 429 | 500..=599 => (FailureClass::Transient, "transient HTTP failure"),
        410 => (
            FailureClass::ResourceExpired,
            "admission recovery is unavailable",
        ),
        400 | 404 | 412 | 422 => (
            FailureClass::InvalidConfiguration,
            "server rejected session configuration",
        ),
        _ => (FailureClass::Protocol, "unexpected HTTP status"),
    };
    Some(Failure {
        class,
        message: format!("{message}: {status}"),
    })
}

fn parse_create_response(
    response: &HttpResponse,
    credential: Credential,
) -> Result<(Candidate, String), String> {
    let resource_uri = unique_header(response, "Location")?
        .ok_or_else(|| "create response is missing Location".to_string())?;
    validate_resource_uri(resource_uri)?;
    const MAX_RESPONSE_BYTES: usize = 1_064_960;
    if response.body.is_empty() || response.body.len() > MAX_RESPONSE_BYTES {
        return Err("create response contains invalid JSON".to_string());
    }
    let body: NativeResponse = serde_json::from_slice(&response.body)
        .map_err(|_| "create response contains invalid JSON".to_string())?;
    if body.profile != pulsebeam_proto::state::PROFILE {
        return Err("unsupported native profile".to_string());
    }
    let connection_id = crate::ConnectionId::from_native_response(body.connection_id)
        .ok_or_else(|| "create response contains invalid connection ID".to_string())?;
    pulsebeam_proto::state::validation::validate_capabilities(&body.rtp_capabilities)
        .map_err(|_| "create response contains invalid RTP capabilities".to_string())?;
    let data_delivery = supported_data_delivery(&body.data_delivery)?;
    validate_answer(&body.answer)?;
    if let Some(room) = body.room_id.as_deref() {
        crate::validate_room_name(room)
            .map_err(|_| "create response contains invalid room ID".to_string())?;
    }
    crate::validate_participant_id(&body.participant_id)
        .map_err(|_| "create response contains invalid participant ID".to_string())?;
    Ok((
        Candidate {
            credential,
            connection_id,
            capabilities: crate::BootstrapCapabilities {
                rtp: body.rtp_capabilities,
                data_delivery,
            },
            resource_uri: resource_uri.to_string(),
            room_id: body.room_id,
            participant_id: body.participant_id,
        },
        body.answer,
    ))
}

fn supported_data_delivery(
    advertised: &[serde_json::Value],
) -> Result<Vec<pulsebeam_proto::state::DataDelivery>, String> {
    use pulsebeam_proto::state::DataDelivery;
    let invalid = || "create response contains invalid data capabilities".to_string();
    if advertised.is_empty() || advertised.len() > 32 {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    let mut supported = Vec::new();
    for value in advertised {
        let mode = match value {
            serde_json::Value::String(name)
                if !name.is_empty()
                    && name.len() <= 64
                    && !name.chars().any(|c| c.is_whitespace() || c.is_control()) =>
            {
                DataDelivery::from_str_name(name)
            }
            serde_json::Value::Number(number) => {
                let number = number
                    .as_i64()
                    .and_then(|value| i32::try_from(value).ok())
                    .ok_or_else(invalid)?;
                DataDelivery::try_from(number).ok()
            }
            _ => return Err(invalid()),
        };
        if let Some(mode) = mode {
            if mode == DataDelivery::Unspecified || !seen.insert(mode as i32) {
                return Err(invalid());
            }
            supported.push(mode);
        }
    }
    Ok(supported)
}

fn validate_answer(answer: &str) -> Result<(), String> {
    const MAX_SDP_BYTES: usize = 1_048_576;
    if answer.is_empty() || answer.len() > MAX_SDP_BYTES {
        return Err("create response contains invalid SDP".to_string());
    }
    Ok(())
}

fn unique_header<'a>(response: &'a HttpResponse, name: &str) -> Result<Option<&'a str>, String> {
    let mut matching = response
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let first = matching.next();
    if matching.next().is_some() {
        return Err(format!("response contains duplicate {name} headers"));
    }
    if first.is_some_and(|header| header.value.chars().any(char::is_control)) {
        return Err(format!("response {name} header is invalid"));
    }
    Ok(first.map(|header| header.value.as_str()))
}

fn validate_resource_uri(uri: &str) -> Result<(), String> {
    let authority = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next());
    if authority.is_none_or(str::is_empty)
        || uri.len() > 2048
        || uri.chars().any(char::is_whitespace)
    {
        return Err("response Location is invalid".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use crate::{DataChannelReliability, PreparedDataChannel, tests::RunReconciliation};

    fn facts() -> PreparedDataChannel {
        PreparedDataChannel {
            stream_id: 0,
            negotiated: true,
            ordered: true,
            reliability: DataChannelReliability::Reliable,
            association_max_channels: None,
        }
    }

    fn prepare(
        agent: &mut Agent,
        generation: Generation,
        channel: ChannelId,
        facts: PreparedDataChannel,
    ) {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
                generation,
                channel,
                preparation: Ok(facts),
            }))
            .unwrap();
    }

    #[test]
    fn preparation_is_generation_scoped_and_does_not_invent_capacity() {
        let (mut agent, generation, channel) =
            crate::tests::native_connection::connected(Default::default());
        let stale = agent.ids.generation();
        prepare(&mut agent, stale, channel, facts());
        prepare(
            &mut agent,
            generation,
            ChannelId::new(u64::MAX).unwrap(),
            facts(),
        );
        assert!(agent.active.as_ref().unwrap().data_preparations.is_empty());
        prepare(&mut agent, generation, channel, facts());
        assert_eq!(
            agent.active.as_ref().unwrap().data_preparations[&channel],
            Ok(facts())
        );
        let mut known = facts();
        known.association_max_channels = Some(128);
        prepare(&mut agent, generation, channel, known.clone());
        prepare(&mut agent, generation, channel, facts());
        assert_eq!(
            agent.active.as_ref().unwrap().data_preparations[&channel],
            Ok(known)
        );
        assert!(agent.attempt.is_none());
        assert!(agent.next_effect_after_turn().is_none());
    }

    #[test]
    fn changed_identity_is_latched_until_the_channel_is_retired() {
        let (mut agent, generation, channel) =
            crate::tests::native_connection::connected(Default::default());
        prepare(&mut agent, generation, channel, facts());
        let mut changed = facts();
        changed.stream_id = 1;
        prepare(&mut agent, generation, channel, changed);
        prepare(&mut agent, generation, channel, facts());
        assert!(agent.active.as_ref().unwrap().data_preparations[&channel].is_err());
        assert!(agent.attempt.is_none());
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                generation,
                channel,
                reset_complete: true,
            }))
            .unwrap();
        prepare(&mut agent, generation, channel, facts());
        assert!(agent.active.as_ref().unwrap().data_preparations.is_empty());
        assert!(agent.active.as_ref().unwrap().signaling_terminal);
    }

    #[test]
    fn bootstrap_rtp_callbacks_cannot_retarget_an_active_or_future_generation() {
        let (mut agent, generation, _) =
            crate::tests::native_connection::connected(Default::default());
        agent
            .handle(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
                generation,
                preparation: Ok(Default::default()),
            }))
            .unwrap();
        assert!(agent.active.as_ref().unwrap().media_preparation.is_none());
        agent.command(AgentCommand::Reconnect).unwrap();
        let candidate = agent.attempt.as_ref().unwrap().generation;
        for generation in [generation, candidate] {
            agent
                .handle(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
                    generation,
                    preparation: Err(crate::client_sdp::Error::Observation),
                }))
                .unwrap();
        }
        assert!(agent.attempt.as_ref().unwrap().media_preparation.is_none());
        assert!(agent.active.as_ref().unwrap().media_preparation.is_none());
    }

    #[test]
    fn prepared_streams_must_fit_actual_association_capacity() {
        let mut prepared = facts();
        for (stream_id, capacity, valid) in [
            (0, None, true),
            (0, Some(0), false),
            (127, Some(128), true),
            (128, Some(128), false),
            (65_534, Some(65_536), true),
            (65_535, Some(65_536), false),
            (0, Some(65_537), false),
        ] {
            prepared.stream_id = stream_id;
            prepared.association_max_channels = capacity;
            assert_eq!(prepared.validate().is_ok(), valid);
        }
    }
}

#[cfg(test)]
mod response_tests {
    use alloc::{string::ToString, vec};

    use super::*;

    fn response(body: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status: 201,
            headers: vec![HttpHeader {
                name: "Location".to_string(),
                value: "https://elsewhere.test/exact%2Fopaque?x=a%2Fb".to_string(),
            }],
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    #[test]
    fn native_response_requires_canonical_names_and_keeps_location_opaque() {
        let complete = serde_json::json!({
            "room_id": "Stage_9",
            "participant_id": "Stage_Participant",
            "connection_id": "conn_0065WZSB8G1XN31RDHP9883WVRC",
            "profile": "pulsebeam.media.v1",
            "rtp_capabilities": {},
            "data_delivery": ["LATEST", "LOSSY", "ORDERED"],
            "answer": "v=0\r\n",
            "future": {"ignored": true},
            "token": "not-a-request-credential",
        });
        let parse = |response: &HttpResponse| {
            parse_create_response(
                response,
                Credential {
                    token: "request-token".into(),
                    revision: 17,
                },
            )
        };
        let (candidate, answer) = parse(&response(complete.clone())).unwrap();
        assert_eq!(candidate.credential.token, "request-token");
        assert_eq!(candidate.credential.revision, 17);
        assert_eq!(
            candidate.resource_uri,
            "https://elsewhere.test/exact%2Fopaque?x=a%2Fb"
        );
        assert_eq!(candidate.room_id.as_deref(), Some("Stage_9"));
        assert_eq!(candidate.participant_id, "Stage_Participant");
        assert_eq!(
            candidate.connection_id.as_str(),
            "conn_0065WZSB8G1XN31RDHP9883WVRC"
        );
        assert_eq!(answer, "v=0\r\n");

        let mut roomless = complete.as_object().unwrap().clone();
        let _ = roomless.remove("room_id");
        assert!(
            parse(&response(roomless.into()))
                .unwrap()
                .0
                .room_id
                .is_none()
        );

        for field in [
            "participant_id",
            "connection_id",
            "profile",
            "rtp_capabilities",
            "data_delivery",
            "answer",
        ] {
            let mut missing = complete.as_object().unwrap().clone();
            let _ = missing.remove(field);
            assert!(parse(&response(missing.into())).is_err());
        }

        for (field, value) in [
            ("profile", serde_json::json!(true)),
            ("participant_id", serde_json::json!("not / a name")),
            (
                "connection_id",
                serde_json::json!("https://private.test/Sentinel"),
            ),
            ("profile", serde_json::json!("legacy")),
            ("rtp_capabilities", serde_json::json!({"codecs": [{}]})),
            ("rtp_capabilities", serde_json::json!({"rtcp_modes": [""]})),
            ("data_delivery", serde_json::json!([])),
            ("data_delivery", serde_json::json!(["LATEST", "LATEST"])),
            (
                "data_delivery",
                serde_json::json!(["DATA_DELIVERY_UNSPECIFIED"]),
            ),
            ("data_delivery", serde_json::json!([true])),
            ("data_delivery", serde_json::json!([""])),
            ("data_delivery", serde_json::json!([1.5])),
            ("data_delivery", serde_json::json!(["LATEST", 1])),
        ] {
            let mut invalid = complete.as_object().unwrap().clone();
            invalid.insert(field.to_string(), value);
            assert!(parse(&response(invalid.into())).is_err(), "{field}");
        }

        for (modes, supported) in [
            (
                serde_json::json!(["LATEST", "FUTURE_MODE", 99]),
                vec![pulsebeam_proto::state::DataDelivery::Latest],
            ),
            (serde_json::json!(["FUTURE_MODE"]), vec![]),
        ] {
            let mut future = complete.as_object().unwrap().clone();
            future.insert("data_delivery".to_string(), modes);
            let (candidate, _) = parse(&response(future.into())).unwrap();
            assert_eq!(candidate.capabilities.data_delivery, supported);
        }
        let mut missing_location = response(complete.clone());
        missing_location.headers.clear();
        assert!(parse(&missing_location).is_err());

        let mut relative_location = response(complete);
        relative_location.headers[0].value = "/api/v1/native/relative".to_string();
        assert!(parse(&relative_location).is_err());
    }
}
