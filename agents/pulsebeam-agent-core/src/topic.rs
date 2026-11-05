use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};

use pulsebeam_proto::{
    prelude::Message,
    topic::{
        TopicFloor, TopicFrame, TopicFrontier, TopicMessage as WireMessage, TopicNack, TopicProbe,
        topic_frame::Body,
    },
};

use crate::{
    ChannelId, DataChannelEffect, DataChannelReliability, DataChannelSpec, Effect, Generation,
    LogLevel, Notification, OperationId, Snapshot, ValidationError, id::IdGenerator,
};

mod admission;
pub use admission::DataSendFailure;

pub const MAX_TOPIC_CHANNELS: usize = 64;
pub const MAX_TOPIC_PAYLOAD_BYTES: usize = pulsebeam_proto::topic::MAX_PAYLOAD_BYTES;
pub const TOPIC_HISTORY_CAPACITY: usize = 256;
pub const TOPIC_REORDER_CAPACITY: usize = 256;
const TOPIC_DEDUPLICATION_CAPACITY: usize = 256;
pub const TOPIC_SEND_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TopicMode {
    Latest,
    Lossy,
    Ordered,
}

impl TopicMode {
    pub(crate) fn from_delivery(value: i32) -> Option<Self> {
        use pulsebeam_proto::state::DataDelivery;
        Some(match DataDelivery::try_from(value).ok()? {
            DataDelivery::Latest => Self::Latest,
            DataDelivery::Lossy => Self::Lossy,
            DataDelivery::Ordered => Self::Ordered,
            DataDelivery::Unspecified => return None,
        })
    }

    pub(crate) fn delivery(self) -> pulsebeam_proto::state::DataDelivery {
        use pulsebeam_proto::state::DataDelivery;
        match self {
            Self::Latest => DataDelivery::Latest,
            Self::Lossy => DataDelivery::Lossy,
            Self::Ordered => DataDelivery::Ordered,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Lossy => "lossy",
            Self::Ordered => "ordered",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopicPublisher {
    pub topic: String,
    pub mode: TopicMode,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopicSubscriber {
    pub topic: String,
    pub mode: TopicMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopicRegistrations {
    pub publishers: Vec<TopicPublisher>,
    pub subscribers: Vec<TopicSubscriber>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicSend {
    pub publisher: TopicPublisher,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicMessage {
    pub topic: String,
    pub mode: TopicMode,
    pub data_track_id: String,
    pub sequence: u64,
    pub participant_id: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicDropReason {
    InvalidPayload,
    NotRegistered,
    ChannelUnavailable,
    QueueFull,
    Superseded,
    HostRejected,
    ChannelClosed,
    TransportReplaced,
    SequenceExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicChannel {
    Publisher(TopicPublisher),
    Subscriber(TopicSubscriber),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicNotification {
    SendAdmitted {
        publisher: TopicPublisher,
        operation: OperationId,
        data_track_id: String,
        sequence: u64,
    },
    SendDropped {
        publisher: TopicPublisher,
        reason: TopicDropReason,
    },
    Message(TopicMessage),
    ChannelFailed {
        channel: TopicChannel,
        message: String,
    },
    RecoveryGap {
        subscriber: TopicSubscriber,
        data_track_id: String,
        participant_id: String,
        expected_sequence: u64,
        available_sequence: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopicSnapshot {
    pub publishers: Vec<TopicPublisherStatus>,
    pub subscribers: Vec<TopicSubscriberStatus>,
    pub accepted_sends: u64,
    pub dropped_sends: u64,
    pub delivered_messages: u64,
    pub channel_failures: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicPublisherStatus {
    pub registration: TopicPublisher,
    pub channel: Option<ChannelId>,
    pub data_track_id: Option<String>,
    pub next_sequence: u64,
    pub accepted_history: usize,
    pub queued_messages: usize,
    pub send_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicSubscriberStatus {
    pub registration: TopicSubscriber,
    pub channel: Option<ChannelId>,
    pub publishers: usize,
    pub buffered_messages: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TopicError {
    #[error("topic name is invalid: {0}")]
    InvalidTopic(String),
    #[error("topic payload is {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge { actual: usize, maximum: usize },
    #[error("topic publisher is not registered: {0}")]
    PublisherNotRegistered(String),
    #[error("topic publisher channel or authoritative binding is unavailable: {0}")]
    PublisherUnavailable(String),
    #[error("topic publisher queue is full: {0}")]
    SendQueueFull(String),
    #[error("topic sequence space is exhausted: {0}")]
    SequenceExhausted(String),
    #[error("topic frame is malformed")]
    MalformedMessage,
    #[error("topic control is invalid")]
    InvalidControl,
    #[error("topic channel is not active")]
    UnknownChannel,
}

#[derive(Clone)]
enum ChannelRegistration {
    Publisher(TopicPublisher),
    Subscriber(TopicSubscriber),
}

pub(crate) struct TopicSource<'a> {
    pub channel: ChannelId,
    pub registration: TopicChannel,
    pub track: &'a pulsebeam_proto::state::Track,
    pub participant: &'a pulsebeam_proto::state::Participant,
}

struct ActiveTopics {
    generation: Generation,
    canonical_participant_id: String,
    channels: BTreeMap<ChannelId, ChannelRegistration>,
    channel_sources: BTreeMap<ChannelId, String>,
    suspended: BTreeSet<ChannelId>,
}

struct PendingSend {
    operation: OperationId,
    generation: Generation,
    channel: ChannelId,
    admission: Option<OperationId>,
}

struct QueuedMessage {
    message: WireMessage,
    admission: Option<OperationId>,
}

#[derive(Default)]
struct PublisherState {
    data_track_id: Option<String>,
    next_sequence: u64,
    history: VecDeque<WireMessage>,
    queue: VecDeque<QueuedMessage>,
    pending: Option<PendingSend>,
}

#[derive(Default)]
struct SubscriberState {
    latest: BTreeMap<String, u64>,
    lossy: BTreeMap<String, LossyDelivery>,
    ordered: BTreeMap<String, OrderedDelivery>,
}

#[derive(Clone)]
struct CatalogBinding {
    topic: String,
    mode: TopicMode,
    participant_id: String,
    local: bool,
}

impl CatalogBinding {
    fn matches(&self, subscriber: &TopicSubscriber) -> bool {
        !self.local && self.topic == subscriber.topic && self.mode == subscriber.mode
    }
}

struct AuxiliarySend {
    generation: Generation,
    channel: ChannelId,
    registration: TopicChannel,
}

#[derive(Default)]
pub(crate) struct Topics {
    log_level: LogLevel,
    publishers: BTreeMap<TopicPublisher, PublisherState>,
    subscribers: BTreeMap<TopicSubscriber, SubscriberState>,
    registrations: TopicRegistrations,
    active: Option<ActiveTopics>,
    catalog: BTreeMap<String, CatalogBinding>,
    auxiliary_sends: BTreeMap<OperationId, AuxiliarySend>,
    admissions: BTreeMap<OperationId, admission::Admission>,
    accepted_sends: u64,
    dropped_sends: u64,
    delivered_messages: u64,
    channel_failures: u64,
}

impl TopicRegistrations {
    pub(crate) fn normalize(&mut self) {
        self.publishers.sort();
        self.subscribers.sort();
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        let mut publishers = BTreeSet::new();
        for publisher in &self.publishers {
            validate_topic(&publisher.topic)?;
            if !publishers.insert(&publisher.topic) {
                return Err(ValidationError::Duplicate {
                    field: "topic publisher",
                    value: publisher_label(publisher),
                });
            }
        }
        let mut subscribers = BTreeSet::new();
        for subscriber in &self.subscribers {
            validate_topic(&subscriber.topic)?;
            if !subscribers.insert(subscriber) {
                return Err(ValidationError::Duplicate {
                    field: "topic subscriber",
                    value: subscriber_label(subscriber),
                });
            }
        }
        Ok(())
    }
}

impl Topics {
    pub(crate) fn new(log_level: LogLevel) -> Self {
        Self {
            log_level,
            ..Self::default()
        }
    }

    pub(crate) fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = level;
    }

    pub(crate) fn end_session(
        &mut self,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.unbind(TopicDropReason::ChannelClosed, snapshot, notifications);
    }

    pub(crate) fn recovery_sources(&self) -> impl Iterator<Item = (&TopicSubscriber, &str)> {
        self.subscribers
            .iter()
            .filter(|(subscriber, _)| subscriber.mode == TopicMode::Ordered)
            .flat_map(|(subscriber, state)| {
                state.ordered.iter().filter_map(move |(source, delivery)| {
                    delivery
                        .next_sequence
                        .map(|_| (subscriber, source.as_str()))
                })
            })
    }

    pub(crate) fn bind_native(
        &mut self,
        generation: Generation,
        canonical_participant_id: String,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.unbind(TopicDropReason::TransportReplaced, snapshot, notifications);
        // Native channels become routable only through an eligible owned confirmation.
        self.active = Some(ActiveTopics {
            generation,
            canonical_participant_id,
            channels: BTreeMap::new(),
            channel_sources: BTreeMap::new(),
            suspended: BTreeSet::new(),
        });
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn has_ordered_subscriptions(&self) -> bool {
        self.subscribers
            .keys()
            .any(|subscriber| subscriber.mode == TopicMode::Ordered)
    }

    pub(crate) fn install_source(
        &mut self,
        generation: Generation,
        source: TopicSource<'_>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        else {
            return;
        };
        let Some(pulsebeam_proto::state::track::Kind::Data(format)) = source.track.kind.as_ref()
        else {
            return;
        };
        let Some(mode) = TopicMode::from_delivery(format.delivery) else {
            return;
        };
        let registration = match &source.registration {
            TopicChannel::Publisher(value) => ChannelRegistration::Publisher(value.clone()),
            TopicChannel::Subscriber(value) => ChannelRegistration::Subscriber(value.clone()),
        };
        let newly_ready = active.suspended.remove(&source.channel)
            || !active.channels.contains_key(&source.channel);
        active.channels.insert(source.channel, registration);
        active
            .channel_sources
            .insert(source.channel, source.track.track_id.clone());
        self.catalog.insert(
            source.track.track_id.clone(),
            CatalogBinding {
                topic: source.track.label.clone(),
                mode,
                participant_id: source.participant.participant_external_id.clone(),
                local: source.track.participant_id == active.canonical_participant_id,
            },
        );
        match source.registration {
            TopicChannel::Publisher(publisher) => {
                let state = self.publishers.entry(publisher.clone()).or_default();
                if state.data_track_id.as_ref() != Some(&source.track.track_id) {
                    state.history.clear();
                    state.next_sequence = 0;
                    state.data_track_id = Some(source.track.track_id.clone());
                }
                self.dispatch_publisher(&publisher, ids, effects);
            }
            TopicChannel::Subscriber(subscriber) if newly_ready && mode == TopicMode::Ordered => {
                let cursor = self.subscribers.get_mut(&subscriber).and_then(|state| {
                    state
                        .ordered
                        .entry(source.track.track_id.clone())
                        .or_default()
                        .next_sequence
                });
                self.emit_auxiliary(
                    generation,
                    source.channel,
                    TopicChannel::Subscriber(subscriber),
                    Body::Probe(TopicProbe {
                        data_track_id: source.track.track_id.clone(),
                        from_sequence: cursor.unwrap_or(0),
                        live: cursor.is_none(),
                    }),
                    ids,
                    effects,
                );
            }
            TopicChannel::Subscriber(_) => {}
        }
        self.refresh_snapshot(snapshot);
        let _ = notifications;
    }

    pub(crate) fn end_source(&mut self, subscriber: &TopicSubscriber, source: &str) {
        if let Some(state) = self.subscribers.get_mut(subscriber) {
            state.latest.remove(source);
            state.lossy.remove(source);
            state.ordered.remove(source);
        }
    }

    pub(crate) fn suspend_source(&mut self, generation: Generation, channel: ChannelId) {
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        {
            active.suspended.insert(channel);
        }
    }

    pub(crate) fn detach_source(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        source_ended: bool,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        else {
            return;
        };
        active.suspended.remove(&channel);
        let registration = active.channels.remove(&channel);
        let source = active.channel_sources.remove(&channel);
        if let Some(source) = source {
            self.catalog.remove(&source);
            if let Some(ChannelRegistration::Subscriber(subscriber)) = &registration
                && let Some(state) = self.subscribers.get_mut(subscriber)
            {
                state.latest.remove(&source);
                state.lossy.remove(&source);
                if source_ended {
                    state.ordered.remove(&source);
                } else if let Some(delivery) = state.ordered.get_mut(&source) {
                    delivery.pending.clear();
                }
            }
        }
        if let Some(ChannelRegistration::Publisher(publisher)) = registration
            && let Some(state) = self.publishers.remove(&publisher)
        {
            self.fail_publisher_sends(
                &publisher,
                DataSendFailure::ChannelFailed,
                effects,
                notifications,
            );
            self.drop_state(
                &publisher,
                state,
                TopicDropReason::ChannelClosed,
                notifications,
            );
            self.publishers.insert(publisher, PublisherState::default());
        }
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn probe_remote_publishers(
        &mut self,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let generation = active.generation;
        let mut probes = Vec::new();
        for (channel, registration) in &active.channels {
            if active.suspended.contains(channel) {
                continue;
            }
            let ChannelRegistration::Subscriber(subscriber) = registration else {
                continue;
            };
            if subscriber.mode != TopicMode::Ordered {
                continue;
            }
            for (id, binding) in &self.catalog {
                if active
                    .channel_sources
                    .get(channel)
                    .is_some_and(|source| source != id)
                    || !binding.matches(subscriber)
                {
                    continue;
                }
                let cursor = self
                    .subscribers
                    .get(subscriber)
                    .and_then(|state| state.ordered.get(id))
                    .and_then(|state| state.next_sequence);
                probes.push((
                    *channel,
                    subscriber.clone(),
                    TopicProbe {
                        data_track_id: id.clone(),
                        from_sequence: cursor.unwrap_or(0),
                        live: cursor.is_none(),
                    },
                ));
            }
        }
        for (channel, subscriber, probe) in probes {
            self.emit_auxiliary(
                generation,
                channel,
                TopicChannel::Subscriber(subscriber),
                Body::Probe(probe),
                ids,
                effects,
            );
        }
    }

    pub(crate) fn has_channel(&self, generation: Generation, channel: ChannelId) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.generation == generation
                && active.channels.contains_key(&channel)
                && !active.suspended.contains(&channel)
        })
    }

    pub(crate) fn accept_registrations(
        &mut self,
        registrations: &TopicRegistrations,
        snapshot: &mut Snapshot,
    ) {
        self.registrations = registrations.clone();
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn reconcile(
        &mut self,
        registrations: &TopicRegistrations,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.registrations = registrations.clone();
        let desired_publishers = registrations
            .publishers
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let removed = self
            .publishers
            .keys()
            .filter(|publisher| !desired_publishers.contains(*publisher))
            .cloned()
            .collect::<Vec<_>>();
        for publisher in removed {
            if let Some(state) = self.publishers.remove(&publisher) {
                self.drop_state(
                    &publisher,
                    state,
                    TopicDropReason::NotRegistered,
                    notifications,
                );
            }
        }
        for publisher in &registrations.publishers {
            self.publishers.entry(publisher.clone()).or_default();
        }
        let desired_subscribers = registrations
            .subscribers
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        self.subscribers
            .retain(|subscriber, _| desired_subscribers.contains(subscriber));
        for subscriber in &registrations.subscribers {
            self.subscribers.entry(subscriber.clone()).or_default();
        }
        if let Some(active) = &mut self.active {
            // Retired tokens remain fenced within this generation, not their
            // publication frontiers. The initial channel limit bounds tombstones.
            active
                .channels
                .retain(|_, registration| match registration {
                    ChannelRegistration::Publisher(publisher) => {
                        desired_publishers.contains(publisher)
                    }
                    ChannelRegistration::Subscriber(subscriber) => {
                        desired_subscribers.contains(subscriber)
                    }
                });
        }
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn channel_specs(registrations: &TopicRegistrations) -> Vec<DataChannelSpec> {
        registrations
            .publishers
            .iter()
            .map(|publisher| datagram_spec(publisher_label(publisher)))
            .chain(
                registrations
                    .subscribers
                    .iter()
                    .map(|subscriber| datagram_spec(subscriber_label(subscriber))),
            )
            .collect()
    }

    pub(crate) fn expected_labels(registrations: &TopicRegistrations) -> BTreeSet<String> {
        Self::channel_specs(registrations)
            .into_iter()
            .map(|spec| spec.label)
            .collect()
    }

    pub(crate) fn unbind_generation(
        &mut self,
        generation: Generation,
        reason: TopicDropReason,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            self.unbind(reason, snapshot, notifications);
        }
    }

    pub(crate) fn send(
        &mut self,
        send: TopicSend,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> Result<(), TopicError> {
        self.send_owned(send, None, ids, effects, snapshot, notifications)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "owned sends reuse the native topic queue and correlation"
    )]
    fn send_owned(
        &mut self,
        send: TopicSend,
        admission: Option<OperationId>,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> Result<(), TopicError> {
        if validate_topic(&send.publisher.topic).is_err() {
            self.record_drop(
                &send.publisher,
                TopicDropReason::NotRegistered,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::InvalidTopic(send.publisher.topic));
        }
        if send.payload.len() > MAX_TOPIC_PAYLOAD_BYTES {
            self.record_drop(
                &send.publisher,
                TopicDropReason::InvalidPayload,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PayloadTooLarge {
                actual: send.payload.len(),
                maximum: MAX_TOPIC_PAYLOAD_BYTES,
            });
        }
        if !self.registrations.publishers.contains(&send.publisher) {
            self.record_drop(
                &send.publisher,
                TopicDropReason::NotRegistered,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherNotRegistered(send.publisher.topic));
        }
        let channel = self.channel_for_publisher(&send.publisher);
        let waiting = self.waiting_data_sends(&send.publisher);
        let Some(state) = self.publishers.get_mut(&send.publisher) else {
            self.record_drop(
                &send.publisher,
                TopicDropReason::ChannelUnavailable,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherUnavailable(send.publisher.topic));
        };
        let Some(data_track_id) = state
            .data_track_id
            .as_ref()
            .filter(|_| channel.is_some())
            .cloned()
        else {
            self.record_drop(
                &send.publisher,
                TopicDropReason::ChannelUnavailable,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherUnavailable(send.publisher.topic));
        };
        if state.next_sequence == u64::MAX {
            self.record_drop(
                &send.publisher,
                TopicDropReason::SequenceExhausted,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::SequenceExhausted(send.publisher.topic));
        }
        if state
            .queue
            .len()
            .saturating_add(usize::from(state.pending.is_some()))
            .saturating_add(waiting)
            >= TOPIC_SEND_QUEUE_CAPACITY
        {
            self.record_drop(&send.publisher, TopicDropReason::QueueFull, notifications);
            self.refresh_snapshot(snapshot);
            return Err(TopicError::SendQueueFull(send.publisher.topic));
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let message = WireMessage {
            data_track_id: data_track_id.clone(),
            sequence,
            payload: send.payload,
        };
        let superseded =
            send.publisher.mode == TopicMode::Latest && state.queue.pop_back().is_some();
        if send.publisher.mode == TopicMode::Ordered {
            if state.history.len() == TOPIC_HISTORY_CAPACITY {
                state.history.pop_front();
            }
            state.history.push_back(message.clone());
        }
        state.queue.push_back(QueuedMessage { message, admission });
        if superseded {
            self.record_drop(&send.publisher, TopicDropReason::Superseded, notifications);
        }
        self.accepted_sends = self.accepted_sends.saturating_add(1);
        notifications.push_back(Notification::Topic(TopicNotification::SendAdmitted {
            publisher: send.publisher.clone(),
            operation: ids.operation(),
            data_track_id,
            sequence,
        }));
        self.dispatch_publisher(&send.publisher, ids, effects);
        self.refresh_snapshot(snapshot);
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "host completion updates correlated protocol outputs"
    )]
    pub(crate) fn handle_sent(
        &mut self,
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> bool {
        if self
            .auxiliary_sends
            .get(&operation)
            .is_some_and(|pending| pending.generation == generation && pending.channel == channel)
        {
            self.auxiliary_sends.remove(&operation);
            return true;
        }
        let Some(publisher) = self.pending_publisher(operation, generation, channel) else {
            return false;
        };
        if let Some(ticket) = self
            .publishers
            .get_mut(&publisher)
            .and_then(|state| state.pending.take())
            .and_then(|pending| pending.admission)
        {
            self.finish_data_send(ticket, None, effects, notifications);
        }
        self.dispatch_publisher(&publisher, ids, effects);
        self.refresh_snapshot(snapshot);
        true
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "host failure updates correlated protocol outputs"
    )]
    pub(crate) fn handle_send_failed(
        &mut self,
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
        message: String,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> bool {
        if self
            .auxiliary_sends
            .get(&operation)
            .is_some_and(|pending| pending.generation == generation && pending.channel == channel)
        {
            if let Some(pending) = self.auxiliary_sends.remove(&operation) {
                self.record_channel_failure(pending.registration, message, effects, notifications);
            }
            self.refresh_snapshot(snapshot);
            return true;
        }
        let Some(publisher) = self.pending_publisher(operation, generation, channel) else {
            return false;
        };
        if let Some(state) = self.publishers.get_mut(&publisher) {
            state.pending = None;
        }
        self.fail_publisher_sends(
            &publisher,
            DataSendFailure::HostRejected,
            effects,
            notifications,
        );
        self.record_drop(&publisher, TopicDropReason::HostRejected, notifications);
        self.record_channel_failure(
            TopicChannel::Publisher(publisher.clone()),
            message,
            effects,
            notifications,
        );
        self.dispatch_publisher(&publisher, ids, effects);
        self.refresh_snapshot(snapshot);
        true
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one deterministic input updates all owned topic outputs"
    )]
    pub(crate) fn handle_message(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        payload: Vec<u8>,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> Result<bool, TopicError> {
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == generation)
        else {
            return Ok(false);
        };
        let registration = active
            .channels
            .get(&channel)
            .cloned()
            .ok_or(TopicError::UnknownChannel)?;
        let frame =
            TopicFrame::decode_bounded(&payload).map_err(|_| TopicError::MalformedMessage)?;
        let id = frame
            .data_track_id()
            .ok_or(TopicError::MalformedMessage)?
            .to_string();
        if active
            .channel_sources
            .get(&channel)
            .is_some_and(|source| source != &id)
        {
            return Err(TopicError::InvalidControl);
        }
        match registration {
            ChannelRegistration::Publisher(publisher) => {
                let Some(state) = self.publishers.get(&publisher) else {
                    return Err(TopicError::InvalidControl);
                };
                if publisher.mode != TopicMode::Ordered || state.data_track_id.as_ref() != Some(&id)
                {
                    return Err(TopicError::InvalidControl);
                }
                let response = match frame.body {
                    Some(Body::Probe(probe)) if probe.live => {
                        alloc::vec![Body::Frontier(TopicFrontier {
                            data_track_id: id,
                            next_sequence: state.next_sequence
                        })]
                    }
                    Some(Body::Probe(probe)) => Self::replay(state, probe.from_sequence)?,
                    Some(Body::Nack(nack)) => Self::replay(state, nack.from_sequence)?,
                    _ => return Err(TopicError::InvalidControl),
                };
                for body in response {
                    self.emit_auxiliary(
                        generation,
                        channel,
                        TopicChannel::Publisher(publisher.clone()),
                        body,
                        ids,
                        effects,
                    );
                }
            }
            ChannelRegistration::Subscriber(subscriber) => {
                // Unknown/retired identities are never guessed or buffered. Once
                // established, ordered cursors survive this loss and probes recover it.
                let Some(binding) = self
                    .catalog
                    .get(&id)
                    .filter(|binding| binding.matches(&subscriber))
                    .cloned()
                else {
                    return Ok(true);
                };
                let Some(state) = self.subscribers.get_mut(&subscriber) else {
                    return Ok(true);
                };
                match (subscriber.mode, frame.body) {
                    (TopicMode::Latest, Some(Body::Message(message))) => {
                        if state
                            .latest
                            .get(&id)
                            .is_none_or(|sequence| message.sequence > *sequence)
                        {
                            state.latest.insert(id, message.sequence);
                            self.deliver(
                                &subscriber,
                                &binding,
                                alloc::vec![message],
                                notifications,
                            );
                        }
                    }
                    (TopicMode::Lossy, Some(Body::Message(message))) => {
                        if state.lossy.entry(id).or_default().accept(message.sequence) {
                            self.deliver(
                                &subscriber,
                                &binding,
                                alloc::vec![message],
                                notifications,
                            );
                        }
                    }
                    (TopicMode::Ordered, body) => {
                        let delivery = state.ordered.entry(id.clone()).or_default();
                        let mut gap = None;
                        let result = match body {
                            Some(Body::Frontier(frontier)) => {
                                if delivery.next_sequence.is_none() {
                                    delivery.next_sequence = Some(frontier.next_sequence);
                                }
                                delivery.drain()
                            }
                            Some(Body::Floor(floor)) => {
                                if let Some(expected) = delivery.next_sequence
                                    && floor.available_sequence > expected
                                {
                                    gap = Some((expected, floor.available_sequence));
                                    delivery.next_sequence = Some(floor.available_sequence);
                                    delivery.pending.retain(|sequence, _| {
                                        *sequence >= floor.available_sequence
                                    });
                                }
                                delivery.drain()
                            }
                            Some(Body::Message(message)) => delivery.accept(message),
                            _ => return Err(TopicError::InvalidControl),
                        };
                        if let Some((expected_sequence, available_sequence)) = gap {
                            notifications.push_back(Notification::Topic(
                                TopicNotification::RecoveryGap {
                                    subscriber: subscriber.clone(),
                                    data_track_id: id.clone(),
                                    participant_id: binding.participant_id.clone(),
                                    expected_sequence,
                                    available_sequence,
                                },
                            ));
                        }
                        self.deliver(&subscriber, &binding, result.messages, notifications);
                        if let Some(from_sequence) = result.nack_from {
                            self.emit_auxiliary(
                                generation,
                                channel,
                                TopicChannel::Subscriber(subscriber),
                                Body::Nack(TopicNack {
                                    data_track_id: id,
                                    from_sequence,
                                }),
                                ids,
                                effects,
                            );
                        }
                    }
                    _ => return Err(TopicError::InvalidControl),
                }
            }
        }
        self.refresh_snapshot(snapshot);
        Ok(true)
    }

    pub(crate) fn channel_closed(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> bool {
        let Some(registration) = self
            .active
            .as_ref()
            .filter(|active| active.generation == generation)
            .and_then(|active| active.channels.get(&channel))
            .cloned()
        else {
            return false;
        };
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.channel_sources.contains_key(&channel))
        {
            self.detach_source(generation, channel, false, snapshot, notifications, effects);
            let failed = match registration {
                ChannelRegistration::Publisher(value) => TopicChannel::Publisher(value),
                ChannelRegistration::Subscriber(value) => TopicChannel::Subscriber(value),
            };
            self.record_channel_failure(
                failed,
                "data channel closed".to_string(),
                effects,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return true;
        }
        if let Some(active) = self.active.as_mut() {
            active.channels.remove(&channel);
        }
        let failed = match registration {
            ChannelRegistration::Publisher(publisher) => {
                if let Some(state) = self.publishers.remove(&publisher) {
                    self.drop_state(
                        &publisher,
                        state,
                        TopicDropReason::ChannelClosed,
                        notifications,
                    );
                    self.publishers
                        .insert(publisher.clone(), PublisherState::default());
                }
                TopicChannel::Publisher(publisher)
            }
            ChannelRegistration::Subscriber(subscriber) => {
                self.subscribers
                    .insert(subscriber.clone(), SubscriberState::default());
                TopicChannel::Subscriber(subscriber)
            }
        };
        self.record_channel_failure(
            failed,
            "data channel closed".to_string(),
            effects,
            notifications,
        );
        self.refresh_snapshot(snapshot);
        true
    }

    fn replay(state: &PublisherState, from: u64) -> Result<Vec<Body>, TopicError> {
        if from > state.next_sequence {
            return Err(TopicError::InvalidControl);
        }
        let floor = state
            .history
            .front()
            .map_or(state.next_sequence, |message| message.sequence);
        let mut result = Vec::new();
        if from < floor {
            result.push(Body::Floor(TopicFloor {
                data_track_id: state
                    .data_track_id
                    .clone()
                    .ok_or(TopicError::InvalidControl)?,
                available_sequence: floor,
            }));
        }
        result.extend(
            state
                .history
                .iter()
                .filter(|message| message.sequence >= from)
                .cloned()
                .map(Body::Message),
        );
        Ok(result)
    }

    fn deliver(
        &mut self,
        subscriber: &TopicSubscriber,
        binding: &CatalogBinding,
        messages: Vec<WireMessage>,
        notifications: &mut VecDeque<Notification>,
    ) {
        for message in messages {
            self.delivered_messages = self.delivered_messages.saturating_add(1);
            notifications.push_back(Notification::Topic(TopicNotification::Message(
                TopicMessage {
                    topic: subscriber.topic.clone(),
                    mode: subscriber.mode,
                    data_track_id: message.data_track_id,
                    sequence: message.sequence,
                    participant_id: binding.participant_id.clone(),
                    payload: message.payload,
                },
            )));
        }
    }

    fn unbind(
        &mut self,
        reason: TopicDropReason,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.active = None;
        self.catalog.clear();
        self.auxiliary_sends.clear();
        let states = core::mem::take(&mut self.publishers);
        for (publisher, state) in states {
            self.drop_state(&publisher, state, reason, notifications);
            self.publishers.insert(publisher, PublisherState::default());
        }
        for state in self.subscribers.values_mut() {
            if reason == TopicDropReason::TransportReplaced {
                // A transport fence is not an unsubscription. Keep the delivery
                // frontier, not payload buffers. Only a completed replacement
                // subscription seed can authoritatively prune that frontier.
                state.latest.clear();
                state.lossy.clear();
                for delivery in state.ordered.values_mut() {
                    delivery.pending.clear();
                }
            } else {
                *state = SubscriberState::default();
            }
        }
        self.refresh_snapshot(snapshot);
    }

    fn pending_publisher(
        &self,
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
    ) -> Option<TopicPublisher> {
        self.publishers.iter().find_map(|(publisher, state)| {
            state
                .pending
                .as_ref()
                .filter(|pending| {
                    pending.operation == operation
                        && pending.generation == generation
                        && pending.channel == channel
                })
                .map(|_| publisher.clone())
        })
    }

    fn dispatch_publisher(
        &mut self,
        publisher: &TopicPublisher,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(channel) = self.channel_for_publisher(publisher) else {
            return;
        };
        let Some(generation) = self.active.as_ref().map(|active| active.generation) else {
            return;
        };
        let Some(state) = self.publishers.get_mut(publisher) else {
            return;
        };
        if state.pending.is_some() || state.data_track_id.is_none() {
            return;
        }
        let Some(message) = state.queue.pop_front() else {
            return;
        };
        let operation = ids.operation();
        state.pending = Some(PendingSend {
            operation,
            generation,
            channel,
            admission: message.admission,
        });
        if let Some(ticket) = message.admission {
            self.dispatch_data_send(ticket, effects);
        }
        effects.push_back(Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation,
            channel,
            binary: true,
            payload: TopicFrame {
                body: Some(Body::Message(message.message)),
            }
            .encode_to_vec(),
        }));
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "bounded control output retains host completion correlation"
    )]
    fn emit_auxiliary(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        registration: TopicChannel,
        body: Body,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        if self.auxiliary_sends.len() >= TOPIC_SEND_QUEUE_CAPACITY {
            return;
        }
        let operation = ids.operation();
        self.auxiliary_sends.insert(
            operation,
            AuxiliarySend {
                generation,
                channel,
                registration,
            },
        );
        effects.push_back(Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation,
            channel,
            binary: true,
            payload: TopicFrame { body: Some(body) }.encode_to_vec(),
        }));
    }

    fn drop_state(
        &mut self,
        publisher: &TopicPublisher,
        state: PublisherState,
        reason: TopicDropReason,
        notifications: &mut VecDeque<Notification>,
    ) {
        for _ in 0..state
            .queue
            .len()
            .saturating_add(usize::from(state.pending.is_some()))
        {
            self.record_drop(publisher, reason, notifications);
        }
    }

    fn record_drop(
        &mut self,
        publisher: &TopicPublisher,
        reason: TopicDropReason,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.dropped_sends = self.dropped_sends.saturating_add(1);
        if self.log_level.allows(log::Level::Warn) {
            log::warn!(
                "topic send dropped topic={} mode={:?} reason={reason:?}",
                publisher.topic,
                publisher.mode
            );
        }
        notifications.push_back(Notification::Topic(TopicNotification::SendDropped {
            publisher: publisher.clone(),
            reason,
        }));
    }

    fn record_channel_failure(
        &mut self,
        channel: TopicChannel,
        message: String,
        effects: &mut VecDeque<Effect>,
        notifications: &mut VecDeque<Notification>,
    ) {
        if let TopicChannel::Publisher(publisher) = &channel {
            self.fail_publisher_sends(
                publisher,
                DataSendFailure::ChannelFailed,
                effects,
                notifications,
            );
        }
        self.channel_failures = self.channel_failures.saturating_add(1);
        if self.log_level.allows(log::Level::Warn) {
            log::warn!("topic channel failed channel={channel:?}: {message}");
        }
        notifications.push_back(Notification::Topic(TopicNotification::ChannelFailed {
            channel,
            message,
        }));
    }

    fn refresh_snapshot(&self, snapshot: &mut Snapshot) {
        let next = TopicSnapshot {
            publishers: self
                .registrations
                .publishers
                .iter()
                .map(|registration| {
                    let state = self.publishers.get(registration);
                    TopicPublisherStatus {
                        registration: registration.clone(),
                        channel: self.channel_for_publisher(registration),
                        data_track_id: state.and_then(|state| state.data_track_id.clone()),
                        next_sequence: state.map_or(0, |state| state.next_sequence),
                        accepted_history: state.map_or(0, |state| state.history.len()),
                        queued_messages: state.map_or(0, |state| state.queue.len()),
                        send_pending: state.is_some_and(|state| state.pending.is_some()),
                    }
                })
                .collect(),
            subscribers: self
                .registrations
                .subscribers
                .iter()
                .map(|registration| TopicSubscriberStatus {
                    registration: registration.clone(),
                    channel: self.channel_for_subscriber(registration),
                    publishers: self
                        .catalog
                        .values()
                        .filter(|binding| binding.matches(registration))
                        .count(),
                    buffered_messages: self.subscribers.get(registration).map_or(0, |state| {
                        state.ordered.values().fold(0usize, |total, state| {
                            total.saturating_add(state.pending.len())
                        })
                    }),
                })
                .collect(),
            accepted_sends: self.accepted_sends,
            dropped_sends: self.dropped_sends,
            delivered_messages: self.delivered_messages,
            channel_failures: self.channel_failures,
        };
        if snapshot.topics != next {
            snapshot.topics = next;
            snapshot.version = snapshot.version.saturating_add(1);
        }
    }

    fn channel_for_publisher(&self, publisher: &TopicPublisher) -> Option<ChannelId> {
        let active = self.active.as_ref()?;
        active.channels.iter().filter(|(channel, _)| !active.suspended.contains(channel)).find_map(|(channel, registration)| matches!(registration, ChannelRegistration::Publisher(candidate) if candidate == publisher).then_some(*channel))
    }

    fn channel_for_subscriber(&self, subscriber: &TopicSubscriber) -> Option<ChannelId> {
        let active = self.active.as_ref()?;
        active.channels.iter().filter(|(channel, _)| !active.suspended.contains(channel)).find_map(|(channel, registration)| matches!(registration, ChannelRegistration::Subscriber(candidate) if candidate == subscriber).then_some(*channel))
    }
}

#[derive(Default)]
struct LossyDelivery {
    floor: u64,
    seen: BTreeSet<u64>,
}

impl LossyDelivery {
    fn accept(&mut self, sequence: u64) -> bool {
        if sequence < self.floor || !self.seen.insert(sequence) {
            return false;
        }
        if self.seen.last().is_some_and(|highest| *highest == sequence) {
            self.floor = sequence.saturating_sub(
                u64::try_from(TOPIC_DEDUPLICATION_CAPACITY.saturating_sub(1)).unwrap_or(u64::MAX),
            );
            self.seen.retain(|seen| *seen >= self.floor);
        }
        true
    }
}

#[derive(Default)]
struct OrderedDelivery {
    next_sequence: Option<u64>,
    pending: BTreeMap<u64, WireMessage>,
}

struct DeliveryResult {
    messages: Vec<WireMessage>,
    nack_from: Option<u64>,
}

impl OrderedDelivery {
    fn accept(&mut self, message: WireMessage) -> DeliveryResult {
        let Some(next) = self.next_sequence else {
            return DeliveryResult {
                messages: Vec::new(),
                nack_from: None,
            };
        };
        if message.sequence == next {
            self.next_sequence = Some(next.saturating_add(1));
            let mut result = self.drain();
            result.messages.insert(0, message);
            return result;
        }
        let missing = message.sequence > next;
        if message.sequence > next
            && (self.pending.len() < TOPIC_REORDER_CAPACITY
                || self.pending.contains_key(&message.sequence))
        {
            self.pending.entry(message.sequence).or_insert(message);
        }
        let mut result = self.drain();
        if missing && result.messages.is_empty() {
            result.nack_from = self.next_sequence;
        }
        result
    }

    fn drain(&mut self) -> DeliveryResult {
        let mut messages = Vec::new();
        if let Some(mut next) = self.next_sequence {
            while let Some(message) = self.pending.remove(&next) {
                messages.push(message);
                next = next.saturating_add(1);
            }
            self.next_sequence = Some(next);
        }
        DeliveryResult {
            messages,
            nack_from: (!self.pending.is_empty())
                .then_some(self.next_sequence)
                .flatten(),
        }
    }
}

fn validate_topic(topic: &str) -> Result<(), ValidationError> {
    if topic.is_empty() || topic.len() > 256 {
        return Err(ValidationError::Topic(topic.to_string()));
    }
    Ok(())
}

fn datagram_spec(label: String) -> DataChannelSpec {
    DataChannelSpec {
        label,
        ordered: false,
        reliability: DataChannelReliability::MaxRetransmits(0),
        negotiated_id: None,
    }
}

fn publisher_label(publisher: &TopicPublisher) -> String {
    format!("v2/{}/pub/{}", publisher.mode.label(), publisher.topic)
}
fn subscriber_label(subscriber: &TopicSubscriber) -> String {
    format!("v2/{}/sub/{}", subscriber.mode.label(), subscriber.topic)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn message(sequence: u64) -> WireMessage {
        WireMessage {
            data_track_id: "authoritative".into(),
            sequence,
            payload: alloc::vec![42],
        }
    }

    fn ordered_topics(ids: &mut IdGenerator) -> (Topics, TopicSubscriber, Generation) {
        let subscriber = TopicSubscriber {
            topic: "state".into(),
            mode: TopicMode::Ordered,
        };
        let generation = ids.generation();
        let channel = ChannelId::new(1).unwrap();
        let mut topics = Topics {
            active: Some(ActiveTopics {
                generation,
                canonical_participant_id: "local".into(),
                channel_sources: BTreeMap::new(),
                suspended: BTreeSet::new(),
                channels: BTreeMap::from([(
                    channel,
                    ChannelRegistration::Subscriber(subscriber.clone()),
                )]),
            }),
            ..Topics::default()
        };
        topics.catalog.insert(
            "authoritative".into(),
            CatalogBinding {
                topic: subscriber.topic.clone(),
                mode: subscriber.mode,
                participant_id: "remote".into(),
                local: false,
            },
        );
        topics.subscribers.insert(
            subscriber.clone(),
            SubscriberState {
                ordered: BTreeMap::from([(
                    "authoritative".into(),
                    OrderedDelivery {
                        next_sequence: Some(4),
                        pending: BTreeMap::from([(5, message(5))]),
                    },
                )]),
                ..SubscriberState::default()
            },
        );
        (topics, subscriber, generation)
    }

    #[test]
    fn ordered_cursor_survives_transport_rebind_and_probes_for_recovery() {
        let mut ids = IdGenerator::new();
        let (mut topics, subscriber, generation) = ordered_topics(&mut ids);
        let mut snapshot = Snapshot::default();
        let mut notifications = VecDeque::new();
        let mut effects = VecDeque::new();
        let replacement = ids.generation();
        topics.bind_native(
            replacement,
            "local".into(),
            &mut snapshot,
            &mut notifications,
        );
        assert!(topics.catalog.is_empty());
        assert_eq!(
            topics.recovery_sources().collect::<Vec<_>>(),
            vec![(&subscriber, "authoritative")]
        );
        assert_eq!(
            topics.subscribers[&subscriber].ordered["authoritative"].next_sequence,
            Some(4)
        );
        assert!(
            topics.subscribers[&subscriber].ordered["authoritative"]
                .pending
                .is_empty()
        );
        topics.unbind_generation(
            generation,
            TopicDropReason::ChannelClosed,
            &mut snapshot,
            &mut notifications,
        );
        topics.probe_remote_publishers(&mut ids, &mut effects);
        assert!(effects.is_empty(), "retained desire is not a ready binding");

        let channel = ChannelId::new(2).unwrap();
        let track = pulsebeam_proto::state::Track {
            track_id: "authoritative".into(),
            participant_id: "remote-owner".into(),
            label: subscriber.topic.clone(),
            kind: Some(pulsebeam_proto::state::track::Kind::Data(
                pulsebeam_proto::state::DataTrack {
                    delivery: subscriber.mode.delivery() as i32,
                },
            )),
        };
        let participant = pulsebeam_proto::state::Participant {
            participant_id: track.participant_id.clone(),
            participant_external_id: "remote-app".into(),
        };
        for current in [generation, replacement] {
            topics.install_source(
                current,
                TopicSource {
                    channel,
                    registration: TopicChannel::Subscriber(subscriber.clone()),
                    track: &track,
                    participant: &participant,
                },
                &mut snapshot,
                &mut notifications,
                &mut ids,
                &mut effects,
            );
            if current == generation {
                assert!(
                    effects.is_empty(),
                    "old confirmation cannot reopen a channel"
                );
            }
        }
        let Some(Effect::DataChannel(DataChannelEffect::Send {
            generation: actual_generation,
            channel: actual_channel,
            payload,
            ..
        })) = effects.pop_front()
        else {
            panic!("current owned source must probe the retained ordered cursor");
        };
        assert_eq!(actual_generation, replacement);
        assert_eq!(actual_channel, channel);
        let frame = TopicFrame::decode_bounded(&payload).unwrap();
        let Some(Body::Probe(probe)) = frame.body else {
            panic!("ordered recovery probe");
        };
        assert_eq!(probe.data_track_id, "authoritative");
        assert_eq!(probe.from_sequence, 4);
        assert!(!probe.live, "reconnect is not a new live-only subscription");
        topics.detach_source(
            replacement,
            channel,
            false,
            &mut snapshot,
            &mut notifications,
            &mut effects,
        );
        assert_eq!(
            topics.recovery_sources().collect::<Vec<_>>(),
            vec![(&subscriber, "authoritative")]
        );
        topics.end_source(&subscriber, "authoritative");
        assert!(topics.recovery_sources().next().is_none());
    }

    #[test]
    fn ordered_cursor_is_fenced_by_close_explicit_source_end_or_unsubscribe() {
        for boundary in 0..3 {
            let mut ids = IdGenerator::new();
            let (mut topics, subscriber, _) = ordered_topics(&mut ids);
            let mut snapshot = Snapshot::default();
            let mut notifications = VecDeque::new();
            match boundary {
                0 => topics.end_session(&mut snapshot, &mut notifications),
                1 => topics.end_source(&subscriber, "authoritative"),
                _ => topics.reconcile(
                    &TopicRegistrations::default(),
                    &mut snapshot,
                    &mut notifications,
                ),
            }
            assert!(
                topics
                    .subscribers
                    .get(&subscriber)
                    .is_none_or(|state| state.ordered.is_empty()),
                "boundary {boundary} must discard the old publication cursor"
            );
        }
    }

    #[test]
    fn lossy_deduplicates_with_a_bounded_reordering_window() {
        let mut delivery = LossyDelivery::default();
        assert!(delivery.accept(2));
        assert!(!delivery.accept(2));
        assert!(delivery.accept(0));
        assert!(delivery.accept(1));
        for sequence in 3..300 {
            assert!(delivery.accept(sequence));
        }
        assert!(delivery.seen.len() <= TOPIC_DEDUPLICATION_CAPACITY);
        assert!(!delivery.accept(0));
        assert!(!delivery.accept(299));
    }

    #[test]
    fn ordered_frontier_reordering_deduplication_and_bounded_recovery() {
        let mut state = OrderedDelivery::default();
        assert!(state.accept(message(40)).messages.is_empty());
        assert_eq!(state.next_sequence, None);
        state.next_sequence = Some(41);
        assert_eq!(state.accept(message(42)).nack_from, Some(41));
        let recovered = state.accept(message(41));
        assert_eq!(
            recovered
                .messages
                .iter()
                .map(|message| message.sequence)
                .collect::<Vec<_>>(),
            alloc::vec![41, 42]
        );
        assert!(state.accept(message(41)).messages.is_empty());
        for sequence in 44..400 {
            state.accept(message(sequence));
        }
        assert_eq!(state.next_sequence, Some(43));
        assert_eq!(state.pending.len(), TOPIC_REORDER_CAPACITY);
        assert_eq!(state.accept(message(999)).nack_from, Some(43));
        assert_eq!(state.next_sequence, Some(43));
        assert_eq!(
            state.accept(message(43)).messages.len(),
            TOPIC_REORDER_CAPACITY.saturating_add(1)
        );
        assert!(state.pending.is_empty());
        assert_eq!(state.next_sequence, Some(300));
    }

    #[test]
    fn sequence_exhaustion_never_wraps_in_any_mode() {
        for mode in [TopicMode::Latest, TopicMode::Lossy, TopicMode::Ordered] {
            let publisher = TopicPublisher {
                topic: "events".into(),
                mode,
            };
            let mut ids = IdGenerator::new();
            let generation = ids.generation();
            let channel = ChannelId::new(1).unwrap();
            let mut topics = Topics {
                registrations: TopicRegistrations {
                    publishers: alloc::vec![publisher.clone()],
                    subscribers: Vec::new(),
                },
                active: Some(ActiveTopics {
                    generation,
                    canonical_participant_id: "local".into(),
                    channel_sources: BTreeMap::new(),
                    suspended: BTreeSet::new(),
                    channels: BTreeMap::from([(
                        channel,
                        ChannelRegistration::Publisher(publisher.clone()),
                    )]),
                }),
                ..Topics::default()
            };
            topics.publishers.insert(
                publisher.clone(),
                PublisherState {
                    data_track_id: Some("authoritative".into()),
                    next_sequence: u64::MAX.saturating_sub(1),
                    ..PublisherState::default()
                },
            );
            let mut snapshot = Snapshot::default();
            let mut effects = VecDeque::new();
            let mut notifications = VecDeque::new();
            topics
                .send(
                    TopicSend {
                        publisher: publisher.clone(),
                        payload: alloc::vec![1],
                    },
                    &mut ids,
                    &mut effects,
                    &mut snapshot,
                    &mut notifications,
                )
                .unwrap();
            let Some(Effect::DataChannel(DataChannelEffect::Send { payload, .. })) =
                effects.pop_front()
            else {
                panic!("last valid send");
            };
            let frame = TopicFrame::decode_bounded(&payload).unwrap();
            assert!(
                matches!(frame.body, Some(Body::Message(message)) if message.sequence == u64::MAX.saturating_sub(1))
            );
            assert_eq!(
                topics.send(
                    TopicSend {
                        publisher: publisher.clone(),
                        payload: alloc::vec![2]
                    },
                    &mut ids,
                    &mut effects,
                    &mut snapshot,
                    &mut notifications
                ),
                Err(TopicError::SequenceExhausted(publisher.topic))
            );
            assert!(effects.is_empty());
            assert_eq!(
                snapshot.topics.publishers.first().unwrap().next_sequence,
                u64::MAX
            );
        }
    }

    #[test]
    fn delivery_modes_are_explicit_and_publication_labels_are_unique() {
        let mut registrations = TopicRegistrations::default();
        for mode in [TopicMode::Latest, TopicMode::Lossy, TopicMode::Ordered] {
            assert_eq!(TopicMode::from_delivery(mode.delivery() as i32), Some(mode));
            registrations.subscribers.push(TopicSubscriber {
                topic: "literal/Δ label".into(),
                mode,
            });
        }
        registrations.publishers.push(TopicPublisher {
            topic: "literal/Δ label".into(),
            mode: TopicMode::Latest,
        });
        registrations.validate().unwrap();
        registrations.publishers.push(TopicPublisher {
            topic: "literal/Δ label".into(),
            mode: TopicMode::Ordered,
        });
        assert!(matches!(
            registrations.validate(),
            Err(ValidationError::Duplicate {
                field: "topic publisher",
                ..
            })
        ));
        assert_eq!(TopicMode::from_delivery(0), None);
        assert_eq!(TopicMode::from_delivery(i32::MAX), None);
    }
}
