use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;
use pulsebeam_proto::signaling_v1 as wire;
use serde::{Deserialize, Serialize};

use crate::{
    AgentConfig, ChannelId, ConnectionState, DataChannelEffect, DataChannelEvent,
    DataChannelReliability, DataChannelSpec, DesiredState, Effect, Failure, FailureClass,
    Generation, HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod, HttpRequest,
    HttpResponse, MediaSlot, Notification, OfferResources, OperationId, ParticipantId, RoomId,
    RtcEffect, RtcEvent, SlotBinding, Snapshot, TimerEffect, TimerEvent, TimerId, TopicDropReason,
    TopicError, TopicSend, ValidationError,
    id::IdGenerator,
    signaling::{self, SignalingError},
    signaling_v1::{self, CatalogState, ServerOutput},
    topic::Topics,
};

const CONTENT_TYPE: &str = "Content-Type";
const JSON_CONTENT_TYPE: &str = "application/json";
const SIGNAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_EARLY_SIGNALING_MESSAGES: usize = 64;

macro_rules! agent_log {
    ($agent:expr, $level:ident, $($message:tt)*) => {
        if $agent.config.log_level.allows(log::Level::$level) {
            log::log!(log::Level::$level, $($message)*);
        }
    };
}

#[derive(Clone, PartialEq, Eq)]
pub enum AgentCommand {
    ReplaceDesired(DesiredState),
    RenewAuthorization(String),
    Connect,
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
            Self::Connect => formatter.write_str("Connect"),
            Self::Reconnect => formatter.write_str("Reconnect"),
            Self::SendTopic(send) => formatter.debug_tuple("SendTopic").field(send).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    InvalidConfiguration(#[from] ValidationError),
    #[error("desired revision {received} is older than accepted revision {accepted}")]
    StaleDesiredRevision { received: u64, accepted: u64 },
    #[error("desired revision {0} was reused with different state")]
    ConflictingDesiredRevision(u64),
    #[error("authorization token is invalid")]
    InvalidAuthorizationToken,
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

#[derive(Clone)]
struct Session {
    generation: Generation,
    resource_uri: String,
    participant_id: String,
    room_external_id: String,
    _room_id: RoomId,
    participant_external_id: String,
    _opaque_participant_id: ParticipantId,
    coordinates: BTreeMap<MediaSlot, SlotBinding>,
    observed: CatalogState,
    intent_revision: u64,
    publication_bindings: BTreeMap<String, String>,
    authorization_expires_at: Option<i64>,
    pending_renewal: Option<String>,
    renewal_send: Option<OperationId>,
    signaling_terminal: bool,
    signaling_channel: ChannelId,
    seed_requests: VecDeque<wire::SubscriptionSeed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptMode {
    Fresh,
    Replace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptStage {
    Offering,
    Requesting,
    ApplyingAnswer,
}

struct Candidate {
    resource_uri: String,
    room_external_id: String,
    room_id: RoomId,
    participant_external_id: String,
    participant_id: String,
    opaque_participant_id: ParticipantId,
}

#[derive(Serialize)]
struct NativeRequest<'a> {
    offer: &'a str,
    manual: bool,
}

#[derive(Deserialize)]
struct NativeResponse {
    room_external_id: String,
    room_id: String,
    participant_external_id: String,
    participant_id: String,
    #[serde(rename = "connection_id")]
    _canonical_connection_id: String,
    answer: String,
}

struct Attempt {
    generation: Generation,
    mode: AttemptMode,
    stage: AttemptStage,
    resources: Option<OfferResources>,
    request: Option<OperationId>,
    candidate: Option<Candidate>,
    answer_applied: bool,
    transport_connected: bool,
    signaling_open: bool,
    early_signaling: VecDeque<Vec<u8>>,
    topic_registrations: crate::TopicRegistrations,
    open_channels: BTreeSet<ChannelId>,
}

struct Retry {
    timer: TimerId,
    mode: AttemptMode,
}

struct PendingSignal {
    seed: bool,
    operation: OperationId,
    generation: Generation,
    channel: ChannelId,
}

#[derive(Default)]
struct Closing {
    generations: BTreeSet<Generation>,
    deletes: BTreeSet<OperationId>,
}

pub struct Agent {
    config: AgentConfig,
    desired: DesiredState,
    declared: DesiredState,
    resources: crate::TrackResources,
    unmapped_video: BTreeSet<crate::TrackSelector>,
    local_bindings: BTreeMap<MediaSlot, String>,
    retired_tracks: BTreeSet<String>,
    snapshot: Snapshot,
    ids: IdGenerator,
    effects: VecDeque<Effect>,
    notifications: VecDeque<Notification>,
    published_notifications: VecDeque<Notification>,
    active: Option<Session>,
    attempt: Option<Attempt>,
    retry: Option<Retry>,
    retry_attempts: u8,
    pending_signal: Option<PendingSignal>,
    signal_retry: Option<TimerId>,
    topic_probe: Option<TimerId>,
    seed_timeout: Option<(TimerId, Generation)>,
    intent_dirty: bool,
    closing: Option<Closing>,
    orphaned_creates: BTreeSet<OperationId>,
    topics: Topics,
}

impl Agent {
    pub fn new(mut config: AgentConfig) -> Result<Self, AgentError> {
        config.validate()?;
        let log_level = config.log_level;
        Ok(Self {
            config,
            desired: DesiredState::default(),
            declared: DesiredState::default(),
            resources: crate::TrackResources::default(),
            unmapped_video: BTreeSet::new(),
            local_bindings: BTreeMap::new(),
            retired_tracks: BTreeSet::new(),
            snapshot: Snapshot::default(),
            ids: IdGenerator::new(),
            effects: VecDeque::new(),
            notifications: VecDeque::new(),
            published_notifications: VecDeque::new(),
            active: None,
            attempt: None,
            retry: None,
            retry_attempts: 0,
            pending_signal: None,
            signal_retry: None,
            topic_probe: None,
            seed_timeout: None,
            intent_dirty: false,
            closing: None,
            orphaned_creates: BTreeSet::new(),
            topics: Topics::new(log_level),
        })
    }

    pub fn set_log_level(&mut self, level: crate::LogLevel) {
        if self.config.log_level == level {
            return;
        }
        self.config.log_level = level;
        self.topics.set_log_level(level);
    }

    pub fn command(&mut self, command: AgentCommand) -> Result<(), AgentError> {
        let result = match command {
            AgentCommand::ReplaceDesired(desired) => self.replace_desired(desired),
            AgentCommand::RenewAuthorization(token) => self.renew_authorization(token),
            AgentCommand::Connect => self.connect(),
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
            agent_log!(self, Warn, "agent command rejected: {error}");
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
        let refreshed = self.refresh_resources();
        self.send_intent_if_ready();
        let flushed = self.flush_notifications();
        let result = result.and(refreshed).and(flushed);
        if let Err(error) = &result {
            agent_log!(self, Warn, "host event rejected: {error}");
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
        if !self.desired.connected {
            return Err(crate::TrackResourceError::NotConnected.into());
        }
        let subscriber = spec.subscriber()?;
        let send = crate::TopicSend {
            publisher: crate::TopicPublisher {
                topic: subscriber.topic,
                mode: subscriber.mode,
            },
            payload,
        };
        if self.snapshot.terminal_failure.is_some() {
            return Err(crate::TopicError::PublisherUnavailable(send.publisher.topic).into());
        }
        self.topics.validate_data_send(&send)?;
        self.resource_command(crate::ResourceCommand::RetainProducer { spec })?;
        let ticket = self
            .topics
            .begin_data_send(send, &mut self.ids, &mut self.effects);
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
        let result = match command {
            ResourceCommand::ClaimPublication { spec } => {
                spec.validate()?;
                let kind = spec.media_kind().ok_or(TrackResourceError::WrongKind)?;
                if self
                    .local_bindings
                    .iter()
                    .any(|(slot, label)| slot.kind() == kind && label == spec.label())
                {
                    return Err(TrackResourceError::AlreadyOwned.into());
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
                let label = spec.label().to_owned();
                let owner = self.resources.new_owner()?;
                let slot = self.resources.claim_publication(
                    owner,
                    spec,
                    &self.config.topology,
                    &occupied,
                )?;
                let after_generation = self
                    .active
                    .as_ref()
                    .filter(|session| {
                        session
                            .publication_bindings
                            .get(&slot)
                            .is_some_and(|bound| bound != &label)
                    })
                    .map(|session| session.generation);
                ResourceResult::Publication {
                    owner: owner.id(),
                    slot,
                    after_generation,
                }
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
            ResourceCommand::RetainProducer { spec } => {
                if !self.desired.connected {
                    return Err(TrackResourceError::NotConnected.into());
                }
                let subscriber = spec.subscriber()?;
                let mut owned = self.resources.registrations();
                owned.publishers.push(crate::TopicPublisher {
                    topic: subscriber.topic,
                    mode: subscriber.mode,
                });
                self.merged_topics(owned)?;
                self.resources.retain_producer(&spec)?;
                ResourceResult::Updated
            }
            ResourceCommand::ReceiveData {
                spec,
                participant_id,
            } => {
                let mut owned = self.resources.registrations();
                owned.subscribers.push(spec.subscriber()?);
                self.merged_topics(owned)?;
                let owner = self.resources.new_owner()?;
                self.resources.receive(owner, &spec, participant_id)?;
                ResourceResult::Owned { owner: owner.id() }
            }
            ResourceCommand::ReadData { owner } => {
                return Ok(ResourceResult::Event {
                    event: self.resources.read(TrackOwner(owner))?,
                });
            }
            ResourceCommand::Release { owner } => {
                self.resources.release(TrackOwner(owner));
                ResourceResult::Updated
            }
        };
        self.refresh_resources()?;
        Ok(result)
    }

    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effects.pop_front()
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
        self.config.topology.local_slot_kind(name) == Some(slot.kind())
            && self
                .desired
                .publications
                .iter()
                .any(|publication| publication.slot == *name && publication.active)
    }

    pub fn next_notification(&mut self) -> Option<Notification> {
        self.published_notifications.pop_front()
    }

    fn flush_notifications(&mut self) -> Result<(), AgentError> {
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
                let update = self.resources.notify(topic);
                ready.extend(update.ready.into_iter().map(crate::TrackOwner::id));
                if update.registrations_changed {
                    self.refresh_resources()?;
                }
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

    fn renew_authorization(&mut self, token: String) -> Result<(), AgentError> {
        if token.is_empty()
            || token.len() > 16_384
            || token.bytes().any(|byte| !(b'!'..=b'~').contains(&byte))
        {
            return Err(AgentError::InvalidAuthorizationToken);
        }
        if !self.desired.connected || self.snapshot.terminal_failure.is_some() {
            self.config.token = token;
            return Ok(());
        }
        let active = self
            .active
            .as_mut()
            .filter(|active| {
                !active.signaling_terminal && active.authorization_expires_at.is_some()
            })
            .ok_or(AgentError::RenewalNotReady)?;
        if active.pending_renewal.is_some() {
            return Err(AgentError::RenewalBusy);
        }
        let payload = pulsebeam_proto::codec::encode_client(&wire::ClientMessage {
            payload: Some(wire::client_message::Payload::RenewAuthorization(
                wire::RenewAuthorization {
                    token: token.clone(),
                },
            )),
        })
        .map_err(|_| SignalingError::Encode)?;
        let operation = self.ids.operation();
        active.pending_renewal = Some(token);
        active.renewal_send = Some(operation);
        self.effects
            .push_back(Effect::DataChannel(DataChannelEffect::Send {
                operation,
                generation: active.generation,
                channel: active.signaling_channel,
                binary: true,
                payload,
            }));
        Ok(())
    }

    fn replace_desired(&mut self, mut desired: DesiredState) -> Result<(), AgentError> {
        desired.normalize();
        desired.validate(&self.config.topology)?;
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
            let spec = match self.config.topology.local_slot_kind(&publication.slot) {
                Some(crate::MediaKind::Video) => crate::TrackSpec::Video {
                    label: publication.label.clone(),
                },
                Some(crate::MediaKind::Audio) => crate::TrackSpec::Audio {
                    label: publication.label.clone(),
                },
                None => {
                    return Err(
                        ValidationError::UnknownPublicationSlot(publication.slot.clone()).into(),
                    );
                }
            };
            if owned_slots.contains(&publication.slot) || self.resources.has_publication(&spec) {
                return Err(crate::TrackResourceError::AlreadyOwned.into());
            }
        }
        if !desired.video.is_empty() && self.resources.has_video() {
            return Err(crate::TrackResourceError::AlreadyOwned.into());
        }
        let effective = self.effective_desired(&desired)?;
        let local_slot = |name: &str| match self.config.topology.local_slot_kind(name) {
            Some(crate::MediaKind::Video) => Ok(MediaSlot::LocalVideo(name.to_string())),
            Some(crate::MediaKind::Audio) => Ok(MediaSlot::LocalAudio(name.to_string())),
            None => Err(ValidationError::UnknownPublicationSlot(name.to_string())),
        };
        for publication in &desired.publications {
            let slot = local_slot(&publication.slot)?;
            if self
                .local_bindings
                .get(&slot)
                .is_some_and(|label| label != &publication.label)
                || self.local_bindings.iter().any(|(reserved_slot, label)| {
                    reserved_slot.kind() == slot.kind()
                        && label == &publication.label
                        && reserved_slot != &slot
                })
            {
                return Err(ValidationError::PublicationBinding(publication.label.clone()).into());
            }
        }
        for publication in &desired.publications {
            self.local_bindings
                .insert(local_slot(&publication.slot)?, publication.label.clone());
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
            let projection = self.resources.video_projection(
                &self.snapshot.remote_tracks,
                self.config.topology.remote_video,
            );
            let unmapped: BTreeSet<_> = projection.unmapped.into_iter().collect();
            for selector in unmapped.difference(&self.unmapped_video) {
                agent_log!(
                    self,
                    Warn,
                    "remote video demand exceeds receiver capacity participant={} label={} capacity={}",
                    selector.participant_external_id,
                    selector.label,
                    self.config.topology.remote_video
                );
            }
            self.unmapped_video = unmapped;
            desired.video = projection.subscriptions;
        } else {
            self.unmapped_video.clear();
        }
        desired.validate(&self.config.topology)?;
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
        if let Some(active) = self.active.as_ref() {
            for video in &desired.video {
                if video.selector.is_none()
                    && active.observed.tracks.contains_key(&video.track_id)
                    && !self.desired.video.iter().any(|previous| {
                        previous.selector.is_none() && previous.track_id == video.track_id
                    })
                {
                    self.retired_tracks.remove(&video.track_id);
                }
            }
            for id in &desired.audio.pinned {
                if active.observed.tracks.contains_key(id)
                    && !self.desired.audio.pinned.contains(id)
                {
                    self.retired_tracks.remove(id);
                }
            }
        }
        let video_changed = self.desired.video != desired.video;
        let intent_changed = self.desired.publications != desired.publications
            || video_changed
            || self.desired.audio != desired.audio;
        let topics_changed = self.desired.topics != desired.topics;
        let previous_desired = core::mem::replace(&mut self.desired, desired);
        self.snapshot.desired_revision = self.desired.revision;
        self.bump_snapshot();
        if video_changed {
            agent_log!(
                self,
                Debug,
                "desired video subscriptions changed revision={} previous={:?} current={:?}",
                self.desired.revision,
                previous_desired.video,
                self.desired.video,
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
            self.topics.reconcile(
                &self.desired.topics,
                &mut self.snapshot,
                &mut self.notifications,
            );
            self.schedule_topic_probe();
            if !self.topics.seeding() {
                self.cancel_seed_timeout();
            }
        }
        if intent_changed {
            self.intent_dirty = true;
        }

        if self.desired.connected {
            if self.closing.is_none() && self.attempt.is_none() && self.retry.is_none() {
                if self.snapshot.terminal_failure.is_some() {
                    return Ok(());
                } else if self.active.is_none() {
                    self.start_attempt(AttemptMode::Fresh);
                } else if topics_changed {
                    self.start_attempt(AttemptMode::Replace);
                } else {
                    self.send_intent_if_ready();
                }
            } else {
                self.send_intent_if_ready();
            }
        } else {
            self.begin_close();
        }
        Ok(())
    }

    fn connect(&mut self) -> Result<(), AgentError> {
        if !self.desired.connected {
            let mut desired = self.declared.clone();
            desired.revision = desired.revision.saturating_add(1);
            desired.connected = true;
            self.replace_desired(desired)?;
        }
        if self.snapshot.terminal_failure.is_some() {
            self.reconnect();
        }
        Ok(())
    }

    fn reconnect(&mut self) {
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
        if self.snapshot.terminal_failure.is_some() {
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
            mode,
            stage: AttemptStage::Offering,
            resources: None,
            request: None,
            candidate: None,
            answer_applied: false,
            transport_connected: false,
            signaling_open: false,
            early_signaling: VecDeque::new(),
            topic_registrations: topic_registrations.clone(),
            open_channels: BTreeSet::new(),
        });
        let mut data_channels = vec![DataChannelSpec {
            label: signaling::SIGNALING_LABEL.to_string(),
            ordered: true,
            reliability: DataChannelReliability::Reliable,
        }];
        data_channels.extend(Topics::channel_specs(&topic_registrations));
        self.effects.push_back(Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            topology: self.config.topology.clone(),
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

    fn fence_signaling(&mut self, generation: Generation) {
        self.cancel_seed_timeout();
        if let Some(active) = self.active.as_mut() {
            active.signaling_terminal = true;
        }
        self.pending_signal = None;
        self.cancel_signal_retry();
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
        validate_offer(
            &offer,
            &resources,
            &self.config,
            &attempt.topic_registrations,
        )?;
        let mode = attempt.mode;
        let operation = self.ids.operation();
        let request = self.create_request(&offer)?;
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.stage = AttemptStage::Requesting;
            attempt.resources = Some(resources);
            attempt.request = Some(operation);
        }
        agent_log!(
            self,
            Debug,
            "requesting native session mode={mode:?} generation={} operation={}",
            generation.get(),
            operation.get(),
        );
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: Some(generation),
            request,
        }));
        if mode == AttemptMode::Fresh && self.active.is_none() {
            self.set_connection_state(ConnectionState::Joining);
        }
        Ok(())
    }

    fn handle_http(&mut self, event: HttpEvent) -> Result<(), AgentError> {
        match event {
            HttpEvent::Response {
                operation,
                response,
            } => self.http_response(operation, response),
            HttpEvent::Failed { operation, message } => {
                if self.remove_delete(operation) {
                    return Ok(());
                }
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.request == Some(operation))
                {
                    if let Some(attempt) = self.attempt.as_mut() {
                        attempt.request = None;
                    }
                    self.fail_attempt(Failure::transient(message));
                }
                Ok(())
            }
        }
    }

    fn http_response(
        &mut self,
        operation: OperationId,
        response: HttpResponse,
    ) -> Result<(), AgentError> {
        if self.remove_delete(operation) {
            return Ok(());
        }
        if self.orphaned_creates.remove(&operation) {
            if response.status / 100 == 2
                && let Ok((candidate, _)) = parse_create_response(&response)
            {
                self.emit_untracked_delete(candidate.resource_uri);
            }
            return Ok(());
        }
        let (mode, generation) = match self.attempt.as_ref() {
            Some(attempt)
                if attempt.request == Some(operation)
                    && attempt.stage == AttemptStage::Requesting =>
            {
                (attempt.mode, attempt.generation)
            }
            Some(_) | None => return Ok(()),
        };
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.request = None;
        }
        agent_log!(
            self,
            Debug,
            "received native response mode={mode:?} generation={} operation={} status={}",
            generation.get(),
            operation.get(),
            response.status,
        );
        let failure = classify_http_failure(response.status);
        if let Some(failure) = failure {
            self.fail_attempt(failure);
            return Ok(());
        }
        let (candidate, answer) = match parse_create_response(&response) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.fail_attempt(Failure::protocol(message));
                return Ok(());
            }
        };
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.stage = AttemptStage::ApplyingAnswer;
            attempt.request = None;
            attempt.candidate = Some(candidate);
        }
        self.effects
            .push_back(Effect::Rtc(RtcEffect::ApplyAnswer { generation, answer }));
        self.update_attempt_state();
        Ok(())
    }

    fn handle_timer(&mut self, event: TimerEvent) -> Result<(), AgentError> {
        match event {
            TimerEvent::Fired { timer } => {
                if self
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.timer == timer)
                {
                    if let Some(retry) = self.retry.take()
                        && self.desired.connected
                    {
                        self.start_attempt(retry.mode);
                    }
                } else if self.signal_retry == Some(timer) {
                    self.signal_retry = None;
                    self.send_intent_if_ready();
                } else if let Some((pending, generation)) = self.seed_timeout
                    && pending == timer
                {
                    self.seed_timeout = None;
                    if self.topics.seeding() {
                        self.handle_rtc(RtcEvent::Failed {
                            generation,
                            message: "subscription seed did not complete within 30 seconds".into(),
                        })?;
                    }
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
                            || self.topics.has_channel(generation, channel)
                    })
                {
                    return Err(TopicError::UnknownChannel.into());
                }
            }
            DataChannelEvent::Closed {
                generation,
                channel,
            } => {
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
                if self.active.as_ref().is_some_and(|session| {
                    session.generation == generation && session.signaling_channel == channel
                }) && self.desired.connected
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
                } else {
                    let topic_closed = self.topics.channel_closed(
                        generation,
                        channel,
                        &mut self.effects,
                        &mut self.snapshot,
                        &mut self.notifications,
                    );
                    if topic_closed
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
                let resolved_before =
                    signaling::resolved_selectors(&self.desired, &active.observed);
                let tracks_before: BTreeSet<_> = active.observed.tracks.keys().cloned().collect();
                let output = signaling_v1::decode_and_apply(
                    &payload,
                    &mut active.observed,
                    &mut self.snapshot,
                    &mut self.notifications,
                    &active.coordinates,
                    &active.participant_id,
                    &active.participant_external_id,
                );
                let output = match output {
                    Ok(output) => output,
                    Err(error) => {
                        self.fail_terminal(Failure::protocol(error.to_string()));
                        return Err(error.into());
                    }
                };
                match output {
                    ServerOutput::StateChanged => {
                        if active.intent_revision != 0
                            && active.observed.intent_revision == active.intent_revision
                        {
                            self.retry_attempts = 0;
                        }
                        self.topics.sync_catalog(
                            generation,
                            &active.observed,
                            &mut self.snapshot,
                            &mut self.notifications,
                            &mut self.ids,
                            &mut self.effects,
                        );
                        self.topics
                            .probe_remote_publishers(&mut self.ids, &mut self.effects);
                        agent_log!(
                            self,
                            Debug,
                            "applied signaling state generation={} participants={} publications={} video_bindings={} audio_bindings={}",
                            generation.get(),
                            self.snapshot.participants.len(),
                            self.snapshot.publications.len(),
                            self.snapshot.video_mapping.len(),
                            self.snapshot.audio_mapping.len(),
                        );
                        let removed: Vec<_> = tracks_before
                            .into_iter()
                            .filter(|id| !active.observed.tracks.contains_key(id))
                            .collect();
                        let direct_removed = removed.iter().any(|id| {
                            self.desired
                                .video
                                .iter()
                                .any(|video| video.selector.is_none() && video.track_id == *id)
                                || self.desired.audio.pinned.contains(id)
                        });
                        self.retired_tracks.extend(removed);
                        if direct_removed
                            || resolved_before
                                != signaling::resolved_selectors(&self.desired, &active.observed)
                        {
                            self.intent_dirty = true;
                        }
                    }
                    ServerOutput::SeedComplete(completion) => {
                        if self
                            .topics
                            .complete_seed(generation, &completion, active.observed.revision)
                            .is_err()
                        {
                            self.fail_terminal(Failure::protocol(
                                "invalid subscription seed completion",
                            ));
                            return Err(
                                SignalingError::Invalid("subscription seed completion").into()
                            );
                        }
                        self.topics.sync_catalog(
                            generation,
                            &active.observed,
                            &mut self.snapshot,
                            &mut self.notifications,
                            &mut self.ids,
                            &mut self.effects,
                        );
                        self.topics
                            .probe_remote_publishers(&mut self.ids, &mut self.effects);
                        if !self.topics.seeding() {
                            self.cancel_seed_timeout();
                        }
                    }
                    ServerOutput::Authorization(expires_at) => {
                        if active.pending_renewal.is_some()
                            && active
                                .authorization_expires_at
                                .is_some_and(|previous| expires_at <= previous)
                        {
                            self.fail_terminal(Failure::protocol(
                                "authorization renewal did not extend the lease",
                            ));
                            return Err(SignalingError::Invalid("renewal expiry").into());
                        }
                        active.authorization_expires_at = Some(expires_at);
                        if let Some(token) = active.pending_renewal.take() {
                            self.config.token = token;
                            active.renewal_send = None;
                        }
                        self.snapshot.authorization_expires_at = Some(expires_at);
                        self.snapshot.version = self.snapshot.version.saturating_add(1);
                    }
                    ServerOutput::ServerError(error) => {
                        agent_log!(
                            self,
                            Warn,
                            "server reported signaling error generation={} code={} fatal={}",
                            generation.get(),
                            error.code,
                            error.fatal
                        );
                        if error.fatal {
                            let class = if error.code
                                == pulsebeam_proto::signaling_v1::ErrorCode::AuthorizationExpired
                                    as i32
                            {
                                FailureClass::Authorization
                            } else {
                                FailureClass::Protocol
                            };
                            let failure = Failure {
                                class,
                                message: error.message,
                            };
                            self.fail_terminal(failure);
                        } else {
                            if error.code == wire::ErrorCode::AuthorizationRejected as i32 {
                                active.pending_renewal = None;
                                active.renewal_send = None;
                            }
                            self.notifications
                                .push_back(Notification::ServerError(error.message));
                        }
                    }
                    ServerOutput::Reconnect => {
                        active.signaling_terminal = true;
                        self.pending_signal = None;
                        self.cancel_signal_retry();
                        self.cancel_seed_timeout();
                        if self.desired.connected && self.attempt.is_none() {
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
            }
            DataChannelEvent::Sent {
                operation,
                generation,
                channel,
            } => {
                if self.active.as_ref().is_some_and(|active| {
                    active.generation == generation
                        && active.signaling_channel == channel
                        && active.renewal_send == Some(operation)
                }) {
                    if let Some(active) = self.active.as_mut() {
                        active.renewal_send = None;
                    }
                } else if self.pending_signal.as_ref().is_some_and(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                }) {
                    agent_log!(
                        self,
                        Debug,
                        "signaling intent sent generation={} operation={} channel={}",
                        generation.get(),
                        operation.get(),
                        channel.get(),
                    );
                    let seed = self
                        .pending_signal
                        .take()
                        .is_some_and(|pending| pending.seed);
                    if let Some(active) = self.active.as_mut()
                        && seed
                    {
                        active.seed_requests.pop_front();
                    }
                    self.send_intent_if_ready();
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
                if self.active.as_ref().is_some_and(|active| {
                    active.generation == generation
                        && active.signaling_channel == channel
                        && active.renewal_send == Some(operation)
                }) {
                    if let Some(active) = self.active.as_mut() {
                        active.renewal_send = None;
                        active.pending_renewal = None;
                    }
                    self.notify_failure(Failure::transient(
                        "authorization renewal send failed".to_string(),
                    ));
                } else if self.pending_signal.as_ref().is_some_and(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                }) {
                    agent_log!(
                        self,
                        Warn,
                        "signaling send failed generation={} operation={} channel={}",
                        generation.get(),
                        operation.get(),
                        channel.get(),
                    );
                    self.pending_signal = None;
                    self.intent_dirty = true;
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
        let topic_bindings = resources.data_channels.clone();
        let coordinates: BTreeMap<_, _> = resources
            .slots
            .into_iter()
            .map(|binding| (binding.slot.clone(), binding))
            .collect();
        let previous_generation = self.active.as_ref().map(|session| session.generation);
        if let Some(previous) = self.active.replace(Session {
            generation: attempt.generation,
            resource_uri: candidate.resource_uri,
            participant_id: candidate.participant_id,
            room_external_id: candidate.room_external_id,
            _room_id: candidate.room_id,
            participant_external_id: candidate.participant_external_id,
            _opaque_participant_id: candidate.opaque_participant_id,
            coordinates,
            observed: CatalogState::default(),
            intent_revision: 0,
            publication_bindings: BTreeMap::new(),
            authorization_expires_at: None,
            pending_renewal: None,
            renewal_send: None,
            signaling_terminal: false,
            signaling_channel: resources.signaling_channel,
            seed_requests: VecDeque::new(),
        }) && previous.generation != attempt.generation
        {
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
        self.intent_dirty = true;
        if let Some(active) = &self.active {
            self.snapshot.generation = Some(active.generation);
            self.snapshot.participant_id = Some(active.participant_id.clone());
            self.snapshot.participant_external_id = Some(active.participant_external_id.clone());
            self.snapshot.room_external_id = Some(active.room_external_id.clone());
            self.snapshot.authorization_expires_at = None;
            self.topics.bind(
                active.generation,
                active.participant_id.clone(),
                &attempt.topic_registrations,
                topic_bindings,
                &mut self.ids,
                &mut self.effects,
                &mut self.snapshot,
                &mut self.notifications,
            );
            self.topics
                .probe_remote_publishers(&mut self.ids, &mut self.effects);
        }
        if let Some(active) = self.active.as_mut() {
            active.seed_requests = self.topics.seed_requests().into();
        }
        self.cancel_seed_timeout();
        if self.topics.seeding() {
            let timer = self.ids.timer();
            self.seed_timeout = Some((timer, attempt.generation));
            self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
                timer,
                after: Duration::from_secs(30),
            }));
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
        self.send_intent_if_ready();
        self.schedule_topic_probe();
        if attempt.topic_registrations != self.desired.topics {
            self.start_attempt(AttemptMode::Replace);
        }
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
            AttemptStage::Requesting => ConnectionState::Joining,
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
        if let Some(operation) = attempt.request.take() {
            self.effects
                .push_back(Effect::Http(HttpEffect::Cancel { operation }));
            let _ = self.orphaned_creates.insert(operation);
        }
        if let Some(candidate) = attempt.candidate.take() {
            self.emit_untracked_delete(candidate.resource_uri);
        }
        self.pending_signal = None;
        self.intent_dirty = true;
        match failure.class {
            FailureClass::Transient => {
                self.notify_failure(failure);
                self.schedule_retry(attempt.mode);
            }
            FailureClass::InvalidConfiguration
            | FailureClass::Authorization
            | FailureClass::Protocol
            | FailureClass::ResourceExpired
            | FailureClass::RetryExhausted => self.fail_terminal(failure),
        }
    }

    fn fail_terminal(&mut self, failure: Failure) {
        if self.snapshot.terminal_failure.is_some() {
            return;
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
        self.notify_failure(failure.clone());
        self.snapshot.terminal_failure = Some(failure);
        self.set_connection_state(ConnectionState::TerminalFailure);
    }

    fn schedule_retry(&mut self, mode: AttemptMode) {
        self.retry_attempts = self.retry_attempts.saturating_add(1);
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
            "scheduled connection retry mode={mode:?} attempt={} timer={} delay_ms={}",
            self.retry_attempts,
            timer.get(),
            delay.as_millis(),
        );
        self.retry = Some(Retry { timer, mode });
        self.effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: delay,
        }));
        self.set_connection_state(ConnectionState::RetryWaiting {
            attempt: self.retry_attempts,
            after: delay,
        });
    }

    fn publication_handoff_required(&self) -> bool {
        self.active.as_ref().is_some_and(|session| {
            self.desired.publications.iter().any(|publication| {
                session
                    .publication_bindings
                    .get(&publication.slot)
                    .is_some_and(|label| label != &publication.label)
            })
        })
    }

    fn send_intent_if_ready(&mut self) {
        if self.publication_handoff_required() {
            if self.desired.connected
                && self.closing.is_none()
                && self.attempt.is_none()
                && self.retry.is_none()
            {
                self.start_attempt(AttemptMode::Replace);
            }
            return;
        }
        if self.snapshot.terminal_failure.is_some()
            || !self.intent_dirty
            || self.pending_signal.is_some()
            || self.signal_retry.is_some()
        {
            return;
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.signaling_terminal {
            return;
        }
        if let Some(seed) = active.seed_requests.front() {
            let message = wire::ClientMessage {
                payload: Some(wire::client_message::Payload::SubscriptionSeed(
                    seed.clone(),
                )),
            };
            let Ok(payload) = pulsebeam_proto::codec::encode_client(&message) else {
                self.fail_terminal(Failure::protocol(
                    "subscription seed exceeds signaling limit",
                ));
                return;
            };
            let operation = self.ids.operation();
            let generation = active.generation;
            let channel = active.signaling_channel;
            self.effects
                .push_back(Effect::DataChannel(DataChannelEffect::Send {
                    operation,
                    generation,
                    channel,
                    binary: true,
                    payload,
                }));
            self.pending_signal = Some(PendingSignal {
                seed: true,
                operation,
                generation,
                channel,
            });
            return;
        }
        let Some(revision) = active.intent_revision.checked_add(1) else {
            return;
        };
        let mut desired = self.desired.clone();
        desired.video.retain(|video| {
            video.selector.is_some() || !self.retired_tracks.contains(&video.track_id)
        });
        desired
            .audio
            .pinned
            .retain(|id| !self.retired_tracks.contains(id));
        let payload = match signaling::encode_v1_intent(
            &desired,
            &self.config.topology,
            &active.coordinates,
            &active.observed,
            revision,
        ) {
            Ok(payload) => payload,
            Err(error) => {
                let failure = Failure::protocol(error.to_string());
                agent_log!(
                    self,
                    Error,
                    "failed to encode desired signaling intent: {error}"
                );
                self.fail_terminal(failure);
                return;
            }
        };
        // The SFU retains sender-index labels even when a later intent omits them.
        active.publication_bindings.extend(
            desired
                .publications
                .iter()
                .map(|publication| (publication.slot.clone(), publication.label.clone())),
        );
        active.intent_revision = revision;
        let operation = self.ids.operation();
        let generation = active.generation;
        let channel = active.signaling_channel;
        agent_log!(
            self,
            Debug,
            "sending desired signaling intent revision={} generation={} operation={} channel={} bytes={}",
            revision,
            generation.get(),
            operation.get(),
            channel.get(),
            payload.len(),
        );
        self.effects
            .push_back(Effect::DataChannel(DataChannelEffect::Send {
                operation,
                generation,
                channel,
                binary: true,
                payload,
            }));
        self.pending_signal = Some(PendingSignal {
            seed: false,
            operation,
            generation,
            channel,
        });
        self.intent_dirty = false;
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

    fn cancel_seed_timeout(&mut self) {
        if let Some((timer, _)) = self.seed_timeout.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }

    fn cancel_signal_retry(&mut self) {
        if let Some(timer) = self.signal_retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }

    fn begin_close(&mut self) {
        self.cancel_seed_timeout();
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
            if let Some(operation) = attempt.request.take() {
                self.effects
                    .push_back(Effect::Http(HttpEffect::Cancel { operation }));
                let _ = self.orphaned_creates.insert(operation);
            }
            if let Some(candidate) = attempt.candidate.take() {
                self.track_delete(candidate.resource_uri, &mut closing);
            }
        }
        if let Some(active) = self.active.take() {
            if closing.generations.insert(active.generation) {
                self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                    generation: active.generation,
                }));
            }
            self.track_delete(active.resource_uri, &mut closing);
        }
        self.retry_attempts = 0;
        if closing.generations.is_empty() && closing.deletes.is_empty() {
            self.finish_disconnected();
        } else {
            self.closing = Some(closing);
            self.set_connection_state(ConnectionState::Closing);
        }
    }

    fn track_delete(&mut self, resource_uri: String, closing: &mut Closing) {
        let operation = self.ids.operation();
        let _ = closing.deletes.insert(operation);
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request: delete_request(resource_uri, &self.config.token),
        }));
    }

    fn emit_untracked_delete(&mut self, resource_uri: String) {
        let operation = self.ids.operation();
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request: delete_request(resource_uri, &self.config.token),
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
        self.active = None;
        self.attempt = None;
        self.retry = None;
        self.pending_signal = None;
        self.snapshot.generation = None;
        self.snapshot.participant_id = None;
        self.snapshot.participant_external_id = None;
        self.snapshot.room_external_id = None;
        self.snapshot.authorization_expires_at = None;
        self.clear_observed_state();
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

    fn create_request(&self, offer: &str) -> Result<HttpRequest, AgentError> {
        let body = serde_json::to_vec(&NativeRequest {
            offer,
            manual: true,
        })
        .map_err(|_| AgentError::InvalidOffer("SDP offer could not be encoded"))?;
        Ok(HttpRequest {
            method: HttpMethod::Post,
            uri: format!("{}/api/v1/native", self.config.endpoint),
            headers: request_headers(&self.config.token, true),
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
    config: &AgentConfig,
    registrations: &crate::TopicRegistrations,
) -> Result<(), AgentError> {
    const MAX_SDP_BYTES: usize = 1_048_576;
    if offer.is_empty() || offer.len() > MAX_SDP_BYTES {
        return Err(AgentError::InvalidOffer("SDP offer is empty or too large"));
    }
    let expected: BTreeSet<MediaSlot> = config.topology.slots().into_iter().collect();
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
    if actual != expected {
        return Err(AgentError::InvalidOffer(
            "slot mapping does not match configured topology",
        ));
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
    let expected_labels = Topics::expected_labels(registrations);
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
        408 | 425 | 429 | 500..=599 => (FailureClass::Transient, "transient HTTP failure"),
        400 | 404 | 409 | 410 | 412 | 422 => (
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

fn parse_create_response(response: &HttpResponse) -> Result<(Candidate, String), String> {
    let resource_uri = unique_header(response, "Location")?
        .ok_or_else(|| "create response is missing Location".to_string())?;
    validate_resource_uri(resource_uri)?;
    const MAX_RESPONSE_BYTES: usize = 1_064_960;
    if response.body.is_empty() || response.body.len() > MAX_RESPONSE_BYTES {
        return Err("create response contains invalid JSON".to_string());
    }
    let body: NativeResponse = serde_json::from_slice(&response.body)
        .map_err(|_| "create response contains invalid JSON".to_string())?;
    validate_answer(&body.answer)?;
    Ok((
        Candidate {
            resource_uri: resource_uri.to_string(),
            room_external_id: body.room_external_id,
            room_id: RoomId::from_server(body.room_id),
            participant_external_id: body.participant_external_id,
            participant_id: body.participant_id.clone(),
            opaque_participant_id: ParticipantId::from_server(body.participant_id),
        },
        body.answer,
    ))
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
    fn native_response_requires_fields_and_types_but_keeps_values_opaque() {
        let complete = serde_json::json!({
            "room_external_id": "room",
            "room_id": "not canonical / room",
            "participant_external_id": "participant",
            "participant_id": "not canonical / participant",
            "connection_id": "not canonical / connection",
            "answer": "v=0\r\n",
            "future": {"ignored": true},
        });
        let (candidate, answer) = parse_create_response(&response(complete.clone())).unwrap();
        assert_eq!(
            candidate.resource_uri,
            "https://elsewhere.test/exact%2Fopaque?x=a%2Fb"
        );
        assert_eq!(candidate.room_id.as_str(), "not canonical / room");
        assert_eq!(candidate.participant_id, "not canonical / participant");
        assert_eq!(
            candidate.opaque_participant_id.as_str(),
            "not canonical / participant"
        );
        assert_eq!(answer, "v=0\r\n");

        for field in [
            "room_external_id",
            "room_id",
            "participant_external_id",
            "participant_id",
            "connection_id",
            "answer",
        ] {
            let mut missing = complete.as_object().unwrap().clone();
            let _ = missing.remove(field);
            assert!(parse_create_response(&response(missing.into())).is_err());
        }

        let mut wrong_type = complete.as_object().unwrap().clone();
        wrong_type.insert("connection_id".to_string(), serde_json::Value::Bool(true));
        assert!(parse_create_response(&response(wrong_type.into())).is_err());

        let mut missing_location = response(complete.clone());
        missing_location.headers.clear();
        assert!(parse_create_response(&missing_location).is_err());

        let mut relative_location = response(complete);
        relative_location.headers[0].value = "/api/v1/native/relative".to_string();
        assert!(parse_create_response(&relative_location).is_err());
    }
}
