use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec,
};
use core::{str, time::Duration};

use crate::{
    AgentConfig, ChannelId, ConnectionState, DataChannelEffect, DataChannelEvent,
    DataChannelReliability, DataChannelSpec, DesiredState, Effect, Failure, FailureClass,
    Generation, HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod, HttpRequest,
    HttpResponse, MediaSlot, Notification, OfferResources, OperationId, PlayoutDelay, RtcEffect,
    RtcEvent, SlotBinding, Snapshot, TimerEffect, TimerEvent, TimerId, TopicDropReason, TopicError,
    TopicSend, ValidationError,
    id::IdGenerator,
    signaling::{self, ServerOutput, SignalingError},
    topic::Topics,
};

const CONTENT_TYPE: &str = "Content-Type";
const SDP_CONTENT_TYPE: &str = "application/sdp";
const SIGNAL_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentCommand {
    ReplaceDesired(DesiredState),
    SendTopic(TopicSend),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    InvalidConfiguration(#[from] ValidationError),
    #[error("desired revision {received} is older than accepted revision {accepted}")]
    StaleDesiredRevision { received: u64, accepted: u64 },
    #[error("desired revision {0} was reused with different state")]
    ConflictingDesiredRevision(u64),
    #[error("fixed playout delay cannot return to adaptive within one session")]
    AdaptiveAfterFixed,
    #[error("host offer resources are invalid: {0}")]
    InvalidOffer(&'static str),
    #[error(transparent)]
    InvalidSignaling(#[from] SignalingError),
    #[error(transparent)]
    InvalidTopic(#[from] TopicError),
}

#[derive(Clone)]
struct Session {
    generation: Generation,
    resource_uri: String,
    participant_id: String,
    etag: String,
    mids: BTreeMap<MediaSlot, String>,
    signaling_channel: ChannelId,
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
    participant_id: String,
    etag: String,
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
    topic_registrations: crate::TopicRegistrations,
    open_channels: BTreeSet<ChannelId>,
}

struct Retry {
    timer: TimerId,
    mode: AttemptMode,
}

struct PendingSignal {
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
    snapshot: Snapshot,
    ids: IdGenerator,
    effects: VecDeque<Effect>,
    notifications: VecDeque<Notification>,
    active: Option<Session>,
    attempt: Option<Attempt>,
    retry: Option<Retry>,
    retry_attempts: u8,
    pending_signal: Option<PendingSignal>,
    signal_retry: Option<TimerId>,
    intent_dirty: bool,
    closing: Option<Closing>,
    orphaned_creates: BTreeSet<OperationId>,
    topics: Topics,
}

impl Agent {
    pub fn new(mut config: AgentConfig) -> Result<Self, AgentError> {
        config.validate()?;
        Ok(Self {
            config,
            desired: DesiredState::default(),
            snapshot: Snapshot::default(),
            ids: IdGenerator::new(),
            effects: VecDeque::new(),
            notifications: VecDeque::new(),
            active: None,
            attempt: None,
            retry: None,
            retry_attempts: 0,
            pending_signal: None,
            signal_retry: None,
            intent_dirty: false,
            closing: None,
            orphaned_creates: BTreeSet::new(),
            topics: Topics::default(),
        })
    }

    pub fn command(&mut self, command: AgentCommand) -> Result<(), AgentError> {
        let result = match command {
            AgentCommand::ReplaceDesired(desired) => self.replace_desired(desired),
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
        if let Err(error) = &result {
            log::warn!("agent command rejected: {error}");
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
        if let Err(error) = &result {
            log::warn!("host event rejected: {error}");
        }
        result
    }

    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effects.pop_front()
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn next_notification(&mut self) -> Option<Notification> {
        self.notifications.pop_front()
    }

    fn replace_desired(&mut self, mut desired: DesiredState) -> Result<(), AgentError> {
        desired.normalize();
        desired.validate(&self.config.topology)?;
        if desired.revision < self.desired.revision {
            return Err(AgentError::StaleDesiredRevision {
                received: desired.revision,
                accepted: self.desired.revision,
            });
        }
        if desired.revision == self.desired.revision {
            return if desired == self.desired {
                Ok(())
            } else {
                Err(AgentError::ConflictingDesiredRevision(desired.revision))
            };
        }
        if self.desired.connected
            && desired.connected
            && matches!(self.desired.playout_delay, PlayoutDelay::Fixed { .. })
            && desired.playout_delay == PlayoutDelay::Adaptive
        {
            return Err(AgentError::AdaptiveAfterFixed);
        }

        let intent_changed = self.desired.publications != desired.publications
            || self.desired.video != desired.video
            || self.desired.audio != desired.audio
            || self.desired.playout_delay != desired.playout_delay;
        let topics_changed = self.desired.topics != desired.topics;
        self.desired = desired;
        self.snapshot.desired_revision = self.desired.revision;
        self.bump_snapshot();
        log::debug!(
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
        }
        if intent_changed {
            self.intent_dirty = true;
        }

        if self.desired.connected {
            if self.closing.is_none() && self.attempt.is_none() && self.retry.is_none() {
                if self.snapshot.connection == ConnectionState::TerminalFailure {
                    self.snapshot.terminal_failure = None;
                    let mode = if self.active.is_some() {
                        AttemptMode::Replace
                    } else {
                        AttemptMode::Fresh
                    };
                    self.start_attempt(mode);
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

    fn start_attempt(&mut self, mode: AttemptMode) {
        debug_assert!(self.attempt.is_none());
        let generation = self.ids.generation();
        let topic_registrations = self.desired.topics.clone();
        log::info!(
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
                    self.try_activate();
                }
                Ok(())
            }
            RtcEvent::Connected { generation } => {
                if let Some(attempt) = self.attempt.as_mut()
                    && attempt.generation == generation
                {
                    attempt.transport_connected = true;
                    self.update_attempt_state();
                    self.try_activate();
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
        let request = match mode {
            AttemptMode::Fresh => self.create_request(&offer),
            AttemptMode::Replace => {
                let Some(active) = self.active.as_ref() else {
                    debug_assert!(false, "replacement requires an active session");
                    return Ok(());
                };
                update_request(active, &offer)
            }
        };
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.stage = AttemptStage::Requesting;
            attempt.resources = Some(resources);
            attempt.request = Some(operation);
        }
        log::debug!(
            "requesting participant session mode={mode:?} generation={} operation={}",
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
                && let Ok(candidate) = parse_create_response(&response)
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
        log::debug!(
            "received participant response mode={mode:?} generation={} operation={} status={}",
            generation.get(),
            operation.get(),
            response.status,
        );
        let failure = classify_http_failure(response.status, mode);
        if let Some(failure) = failure {
            self.fail_attempt(failure);
            return Ok(());
        }
        let candidate = match mode {
            AttemptMode::Fresh => parse_create_response(&response),
            AttemptMode::Replace => {
                let Some(active) = self.active.as_ref() else {
                    debug_assert!(false, "replacement response requires active session");
                    return Ok(());
                };
                parse_update_response(&response, active)
            }
        };
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(message) => {
                self.fail_attempt(Failure::protocol(message));
                return Ok(());
            }
        };
        let Some(answer) = response_body(&response) else {
            self.fail_attempt(Failure::protocol("missing SDP answer"));
            return Ok(());
        };
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.stage = AttemptStage::ApplyingAnswer;
            attempt.request = None;
            attempt.candidate = Some(candidate);
        }
        self.effects.push_back(Effect::Rtc(RtcEffect::ApplyAnswer {
            generation,
            answer: answer.to_string(),
        }));
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
                    self.try_activate();
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
                let Some(active) = self.active.as_ref() else {
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
                match signaling::apply_server_message(
                    &payload,
                    &mut self.snapshot,
                    &mut self.notifications,
                    &active.mids,
                )? {
                    ServerOutput::StateChanged => {
                        log::debug!(
                            "applied signaling state generation={} participants={} publications={} video_bindings={} audio_bindings={}",
                            generation.get(),
                            self.snapshot.participants.len(),
                            self.snapshot.publications.len(),
                            self.snapshot.video.len(),
                            self.snapshot.audio.len(),
                        );
                    }
                    ServerOutput::ServerError(message) => {
                        log::warn!(
                            "server reported signaling error generation={}",
                            generation.get()
                        );
                        self.notifications
                            .push_back(Notification::ServerError(message));
                    }
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
                    log::debug!(
                        "signaling intent sent generation={} operation={} channel={}",
                        generation.get(),
                        operation.get(),
                        channel.get(),
                    );
                    self.pending_signal = None;
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
                if self.pending_signal.as_ref().is_some_and(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                }) {
                    log::warn!(
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

    fn try_activate(&mut self) {
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
            return;
        }
        let Some(mut attempt) = self.attempt.take() else {
            return;
        };
        let Some(candidate) = attempt.candidate.take() else {
            debug_assert!(false, "ready attempt must have candidate metadata");
            return;
        };
        let Some(resources) = attempt.resources.take() else {
            debug_assert!(false, "ready attempt must have offer resources");
            return;
        };
        let topic_bindings = resources.data_channels.clone();
        let mids = resources
            .slots
            .into_iter()
            .map(|binding| (binding.slot, binding.mid))
            .collect();
        let previous_generation = self.active.as_ref().map(|session| session.generation);
        if let Some(previous) = self.active.replace(Session {
            generation: attempt.generation,
            resource_uri: candidate.resource_uri,
            participant_id: candidate.participant_id,
            etag: candidate.etag,
            mids,
            signaling_channel: resources.signaling_channel,
        }) && previous.generation != attempt.generation
        {
            self.effects.push_back(Effect::Rtc(RtcEffect::Close {
                generation: previous.generation,
            }));
        }
        log::info!(
            "activated connection generation={} replaced_generation={:?}",
            attempt.generation.get(),
            previous_generation.map(Generation::get),
        );
        self.retry_attempts = 0;
        self.pending_signal = None;
        self.cancel_signal_retry();
        self.intent_dirty = true;
        if let Some(active) = &self.active {
            self.snapshot.generation = Some(active.generation);
            self.snapshot.participant_id = Some(active.participant_id.clone());
            self.topics.bind(
                active.generation,
                active.participant_id.clone(),
                &attempt.topic_registrations,
                topic_bindings,
                &mut self.snapshot,
                &mut self.notifications,
            );
        }
        self.snapshot.terminal_failure = None;
        self.set_connection_state(ConnectionState::Connected);
        self.send_intent_if_ready();
        if attempt.topic_registrations != self.desired.topics {
            self.start_attempt(AttemptMode::Replace);
        }
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
        log::warn!(
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
            if attempt.mode == AttemptMode::Fresh {
                let _ = self.orphaned_creates.insert(operation);
            }
        }
        if attempt.mode == AttemptMode::Fresh
            && let Some(candidate) = attempt.candidate.take()
        {
            self.emit_untracked_delete(candidate.resource_uri);
        }
        self.pending_signal = None;
        self.intent_dirty = true;
        self.notify_failure(failure.clone());
        match failure.class {
            FailureClass::ResourceExpired if attempt.mode == AttemptMode::Replace => {
                self.start_attempt(AttemptMode::Fresh);
            }
            FailureClass::Transient => self.schedule_retry(attempt.mode),
            FailureClass::InvalidConfiguration
            | FailureClass::Authorization
            | FailureClass::Protocol
            | FailureClass::ResourceExpired
            | FailureClass::RetryExhausted => {
                self.snapshot.terminal_failure = Some(failure);
                self.set_connection_state(ConnectionState::TerminalFailure);
            }
        }
    }

    fn schedule_retry(&mut self, mode: AttemptMode) {
        self.retry_attempts = self.retry_attempts.saturating_add(1);
        if self.retry_attempts > self.config.retry.maximum_attempts {
            let failure = Failure {
                class: FailureClass::RetryExhausted,
                message: "connection retry budget exhausted".to_string(),
            };
            log::error!(
                "connection retry budget exhausted attempts={}",
                self.retry_attempts.saturating_sub(1)
            );
            self.notify_failure(failure.clone());
            self.snapshot.terminal_failure = Some(failure);
            self.set_connection_state(ConnectionState::TerminalFailure);
            return;
        }
        let shift = u32::from(self.retry_attempts.saturating_sub(1).min(10));
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = self
            .config
            .retry
            .initial_delay
            .checked_mul(multiplier)
            .unwrap_or(self.config.retry.maximum_delay)
            .min(self.config.retry.maximum_delay);
        let timer = self.ids.timer();
        log::info!(
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

    fn send_intent_if_ready(&mut self) {
        if !self.intent_dirty || self.pending_signal.is_some() || self.signal_retry.is_some() {
            return;
        }
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let payload =
            match signaling::encode_intent(&self.desired, &self.config.topology, &active.mids) {
                Ok(payload) => payload,
                Err(error) => {
                    let failure = Failure::protocol(error.to_string());
                    log::error!("failed to encode desired signaling intent: {error}");
                    self.notify_failure(failure.clone());
                    self.snapshot.terminal_failure = Some(failure);
                    self.set_connection_state(ConnectionState::TerminalFailure);
                    return;
                }
            };
        let operation = self.ids.operation();
        let generation = active.generation;
        let channel = active.signaling_channel;
        log::debug!(
            "sending desired signaling intent revision={} generation={} operation={} channel={} bytes={}",
            self.desired.revision,
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
        log::info!(
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

    fn cancel_signal_retry(&mut self) {
        if let Some(timer) = self.signal_retry.take() {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }

    fn begin_close(&mut self) {
        if self.closing.is_some() {
            return;
        }
        log::info!(
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
        self.pending_signal = None;
        if let Some(active) = self.active.as_ref() {
            self.topics.unbind_generation(
                active.generation,
                TopicDropReason::ChannelClosed,
                &mut self.snapshot,
                &mut self.notifications,
            );
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
                if attempt.mode == AttemptMode::Fresh {
                    let _ = self.orphaned_creates.insert(operation);
                }
            }
            if attempt.mode == AttemptMode::Fresh
                && let Some(candidate) = attempt.candidate.take()
            {
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
            request: delete_request(resource_uri),
        }));
    }

    fn emit_untracked_delete(&mut self, resource_uri: String) {
        let operation = self.ids.operation();
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request: delete_request(resource_uri),
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
        log::info!("agent session disconnected");
        self.active = None;
        self.attempt = None;
        self.retry = None;
        self.pending_signal = None;
        self.snapshot.generation = None;
        self.snapshot.participant_id = None;
        self.clear_observed_state();
        self.snapshot.terminal_failure = None;
        self.set_connection_state(ConnectionState::Disconnected);
        if self.desired.connected {
            self.start_attempt(AttemptMode::Fresh);
        }
    }

    fn clear_observed_state(&mut self) {
        for id in self.snapshot.participants.keys() {
            self.notifications
                .push_back(Notification::ParticipantRemoved(id.clone()));
        }
        for id in self.snapshot.publications.keys() {
            self.notifications
                .push_back(Notification::PublicationRemoved(id.clone()));
        }
        for mid in self.snapshot.video.keys() {
            self.notifications
                .push_back(Notification::VideoBindingChanged {
                    mid: mid.clone(),
                    binding: None,
                });
        }
        if !self.snapshot.audio.is_empty() {
            self.notifications
                .push_back(Notification::AudioBindingsChanged(vec![]));
        }
        self.snapshot.participants.clear();
        self.snapshot.publications.clear();
        self.snapshot.video.clear();
        self.snapshot.audio.clear();
        self.bump_snapshot();
    }

    fn create_request(&self, offer: &str) -> HttpRequest {
        let mut uri = format!(
            "{}/api/v1/rooms/{}/participants",
            self.config.endpoint, self.config.room_id
        );
        if self.config.manual_subscriptions {
            uri.push_str("?manual_sub=true");
        }
        HttpRequest {
            method: HttpMethod::Post,
            uri,
            headers: vec![HttpHeader {
                name: CONTENT_TYPE.to_string(),
                value: SDP_CONTENT_TYPE.to_string(),
            }],
            body: offer.as_bytes().to_vec(),
        }
    }

    fn notify_failure(&mut self, failure: Failure) {
        self.notifications.push_back(Notification::Failure(failure));
    }

    fn set_connection_state(&mut self, state: ConnectionState) {
        if self.snapshot.connection == state {
            return;
        }
        let from = self.snapshot.connection.clone();
        log::debug!("connection state changed from={from:?} to={state:?}");
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
    for SlotBinding { slot, mid } in &resources.slots {
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

fn update_request(active: &Session, offer: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Patch,
        uri: active.resource_uri.clone(),
        headers: vec![
            HttpHeader {
                name: CONTENT_TYPE.to_string(),
                value: SDP_CONTENT_TYPE.to_string(),
            },
            HttpHeader {
                name: "If-Match".to_string(),
                value: active.etag.clone(),
            },
        ],
        body: offer.as_bytes().to_vec(),
    }
}

fn delete_request(resource_uri: String) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Delete,
        uri: resource_uri,
        headers: vec![],
        body: vec![],
    }
}

fn classify_http_failure(status: u16, mode: AttemptMode) -> Option<Failure> {
    if status / 100 == 2 {
        return None;
    }
    let (class, message) = match status {
        401 | 403 => (FailureClass::Authorization, "server rejected authorization"),
        404 | 410 | 412 if mode == AttemptMode::Replace => (
            FailureClass::ResourceExpired,
            "participant resource expired",
        ),
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

fn parse_create_response(response: &HttpResponse) -> Result<Candidate, String> {
    let resource_uri = unique_header(response, "Location")?
        .ok_or_else(|| "create response is missing Location".to_string())?;
    validate_resource_uri(resource_uri)?;
    let etag = parse_etag(response)?;
    let participant_id = unique_header(response, "pb-participant-id")?
        .map(ToString::to_string)
        .or_else(|| participant_id_from_uri(resource_uri))
        .ok_or_else(|| "create response is missing participant ID".to_string())?;
    crate::validate_identifier("participant_id", &participant_id, 256, true)
        .map_err(|_| "create response participant ID is invalid".to_string())?;
    if response_body(response).is_none() {
        return Err("create response contains invalid SDP".to_string());
    }
    Ok(Candidate {
        resource_uri: resource_uri.to_string(),
        participant_id,
        etag,
    })
}

fn parse_update_response(response: &HttpResponse, active: &Session) -> Result<Candidate, String> {
    let etag = parse_etag(response)?;
    if response_body(response).is_none() {
        return Err("update response contains invalid SDP".to_string());
    }
    Ok(Candidate {
        resource_uri: active.resource_uri.clone(),
        participant_id: active.participant_id.clone(),
        etag,
    })
}

fn response_body(response: &HttpResponse) -> Option<&str> {
    const MAX_SDP_BYTES: usize = 1_048_576;
    if response.body.is_empty() || response.body.len() > MAX_SDP_BYTES {
        return None;
    }
    str::from_utf8(&response.body)
        .ok()
        .filter(|body| !body.is_empty())
}

fn parse_etag(response: &HttpResponse) -> Result<String, String> {
    let etag = unique_header(response, "ETag")?
        .ok_or_else(|| "response is missing ETag".to_string())?
        .trim_matches('"');
    crate::validate_identifier("etag", etag, 256, true)
        .map_err(|_| "response ETag is invalid".to_string())?;
    Ok(etag.to_string())
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
    if !(uri.starts_with("http://") || uri.starts_with("https://"))
        || uri.len() > 2048
        || uri.chars().any(char::is_control)
    {
        return Err("response Location is invalid".to_string());
    }
    Ok(())
}

fn participant_id_from_uri(uri: &str) -> Option<String> {
    uri.split('?')
        .next()
        .and_then(|path| path.rsplit('/').next())
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
}
