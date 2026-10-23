use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};

use pulsebeam_proto::{
    prelude::Message,
    signaling_v1 as wire,
    topic::{
        TopicFloor, TopicFrame, TopicFrontier, TopicMessage as WireMessage, TopicNack, TopicProbe,
        topic_frame::Body,
    },
};

use crate::{
    ChannelId, DataChannelBinding, DataChannelEffect, DataChannelReliability, DataChannelSpec,
    Effect, Generation, LogLevel, Notification, OperationId, Snapshot, ValidationError,
    id::IdGenerator,
};

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
    fn from_wire(mode: i32) -> Option<Self> {
        match wire::TopicMode::try_from(mode).ok()? {
            wire::TopicMode::Latest => Some(Self::Latest),
            wire::TopicMode::Lossy => Some(Self::Lossy),
            wire::TopicMode::Ordered => Some(Self::Ordered),
            wire::TopicMode::Unspecified => None,
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

struct ActiveTopics {
    generation: Generation,
    canonical_participant_id: String,
    channels: BTreeMap<ChannelId, ChannelRegistration>,
}

struct PendingSend {
    operation: OperationId,
    generation: Generation,
    channel: ChannelId,
}

#[derive(Default)]
struct PublisherState {
    data_track_id: Option<String>,
    next_sequence: u64,
    history: VecDeque<WireMessage>,
    queue: VecDeque<WireMessage>,
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
    active: Option<ActiveTopics>,
    catalog: BTreeMap<String, CatalogBinding>,
    auxiliary_sends: BTreeMap<OperationId, AuxiliarySend>,
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
        let total = self.publishers.len().saturating_add(self.subscribers.len());
        if total > MAX_TOPIC_CHANNELS {
            return Err(ValidationError::TopicChannelLimit {
                actual: total,
                maximum: MAX_TOPIC_CHANNELS,
            });
        }
        let mut publishers = BTreeSet::new();
        for publisher in &self.publishers {
            validate_topic(&publisher.topic)?;
            if !publishers.insert(publisher) {
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

    #[cfg(feature = "sim")]
    pub(crate) fn simulation_ordered_cursors(&self) -> Vec<(TopicSubscriber, String, u64)> {
        self.subscribers
            .iter()
            .flat_map(|(subscriber, state)| {
                state.ordered.iter().filter_map(|(id, delivery)| {
                    delivery
                        .next_sequence
                        .map(|next| (subscriber.clone(), id.clone(), next))
                })
            })
            .collect()
    }

    pub(crate) fn has_ordered_subscriptions(&self) -> bool {
        self.subscribers
            .keys()
            .any(|subscriber| subscriber.mode == TopicMode::Ordered)
    }

    pub(crate) fn sync_catalog(
        &mut self,
        generation: Generation,
        catalog: &crate::signaling_v1::CatalogState,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == generation)
        else {
            return;
        };
        self.catalog = catalog
            .tracks
            .iter()
            .filter_map(|(id, track)| {
                if track.kind != wire::TrackKind::Data as i32 {
                    return None;
                }
                let owner = catalog.participants.get(&track.participant_id)?;
                Some((
                    id.clone(),
                    CatalogBinding {
                        topic: track.label.clone(),
                        mode: TopicMode::from_wire(track.mode)?,
                        participant_id: owner.participant_external_id.clone(),
                        local: track.participant_id == active.canonical_participant_id,
                    },
                ))
            })
            .collect();
        for (publisher, state) in &mut self.publishers {
            let binding = self.catalog.iter().find_map(|(id, binding)| {
                (binding.local
                    && binding.topic == publisher.topic
                    && binding.mode == publisher.mode)
                    .then(|| id.clone())
            });
            if state.data_track_id != binding {
                let dropped = state
                    .queue
                    .len()
                    .saturating_add(usize::from(state.pending.take().is_some()));
                state.queue.clear();
                state.history.clear();
                state.next_sequence = 0;
                state.data_track_id = binding;
                for _ in 0..dropped {
                    self.dropped_sends = self.dropped_sends.saturating_add(1);
                    notifications.push_back(Notification::Topic(TopicNotification::SendDropped {
                        publisher: publisher.clone(),
                        reason: TopicDropReason::TransportReplaced,
                    }));
                }
            }
        }
        for (subscriber, state) in &mut self.subscribers {
            state.latest.retain(|id, _| {
                self.catalog
                    .get(id)
                    .is_some_and(|binding| binding.matches(subscriber))
            });
            state.lossy.retain(|id, _| {
                self.catalog
                    .get(id)
                    .is_some_and(|binding| binding.matches(subscriber))
            });
            state.ordered.retain(|id, _| {
                self.catalog
                    .get(id)
                    .is_some_and(|binding| binding.matches(subscriber))
            });
        }
        let publishers = self.publishers.keys().cloned().collect::<Vec<_>>();
        for publisher in publishers {
            self.dispatch_publisher(&publisher, ids, effects);
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
            let ChannelRegistration::Subscriber(subscriber) = registration else {
                continue;
            };
            if subscriber.mode != TopicMode::Ordered {
                continue;
            }
            for (id, binding) in &self.catalog {
                if !binding.matches(subscriber) {
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
            active.generation == generation && active.channels.contains_key(&channel)
        })
    }

    pub(crate) fn reconcile(
        &mut self,
        registrations: &TopicRegistrations,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
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

    #[allow(
        clippy::too_many_arguments,
        reason = "session binding updates owned protocol outputs atomically"
    )]
    pub(crate) fn bind(
        &mut self,
        generation: Generation,
        canonical_participant_id: String,
        registrations: &TopicRegistrations,
        bindings: Vec<DataChannelBinding>,
        _ids: &mut IdGenerator,
        _effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        self.unbind(TopicDropReason::TransportReplaced, snapshot, notifications);
        let by_label = bindings
            .into_iter()
            .map(|binding| (binding.label, binding.channel))
            .collect::<BTreeMap<_, _>>();
        let mut channels = BTreeMap::new();
        for publisher in &registrations.publishers {
            if let Some(channel) = by_label.get(&publisher_label(publisher)) {
                channels.insert(*channel, ChannelRegistration::Publisher(publisher.clone()));
            }
        }
        for subscriber in &registrations.subscribers {
            if let Some(channel) = by_label.get(&subscriber_label(subscriber)) {
                channels.insert(
                    *channel,
                    ChannelRegistration::Subscriber(subscriber.clone()),
                );
            }
        }
        self.active = Some(ActiveTopics {
            generation,
            canonical_participant_id,
            channels,
        });
        self.refresh_snapshot(snapshot);
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
        let channel = self.channel_for_publisher(&send.publisher);
        let Some(state) = self.publishers.get_mut(&send.publisher) else {
            self.record_drop(
                &send.publisher,
                TopicDropReason::NotRegistered,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherNotRegistered(send.publisher.topic));
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
        state.queue.push_back(message);
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
        _notifications: &mut VecDeque<Notification>,
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
        if let Some(state) = self.publishers.get_mut(&publisher) {
            state.pending = None;
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
                self.record_channel_failure(pending.registration, message, notifications);
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
        self.record_drop(&publisher, TopicDropReason::HostRejected, notifications);
        self.record_channel_failure(
            TopicChannel::Publisher(publisher.clone()),
            message,
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
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) -> bool {
        let Some(registration) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
            .and_then(|active| active.channels.remove(&channel))
        else {
            return false;
        };
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
        self.record_channel_failure(failed, "data channel closed".to_string(), notifications);
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
                // frontier, not payload buffers. Catalog pruning is unchanged;
                // see ORDERED_RECOVERY_CHECKPOINT.md for the bootstrap blocker.
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
        let Some(active) = self.active.as_ref() else {
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
            generation: active.generation,
            channel,
        });
        effects.push_back(Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation: active.generation,
            channel,
            binary: true,
            payload: TopicFrame {
                body: Some(Body::Message(message)),
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
        notifications: &mut VecDeque<Notification>,
    ) {
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
                .publishers
                .iter()
                .map(|(registration, state)| TopicPublisherStatus {
                    registration: registration.clone(),
                    channel: self.channel_for_publisher(registration),
                    data_track_id: state.data_track_id.clone(),
                    next_sequence: state.next_sequence,
                    accepted_history: state.history.len(),
                    queued_messages: state.queue.len(),
                    send_pending: state.pending.is_some(),
                })
                .collect(),
            subscribers: self
                .subscribers
                .iter()
                .map(|(registration, state)| TopicSubscriberStatus {
                    registration: registration.clone(),
                    channel: self.channel_for_subscriber(registration),
                    publishers: self
                        .catalog
                        .values()
                        .filter(|binding| binding.matches(registration))
                        .count(),
                    buffered_messages: state.ordered.values().fold(0usize, |total, state| {
                        total.saturating_add(state.pending.len())
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
        self.active.as_ref()?.channels.iter().find_map(|(channel, registration)| matches!(registration, ChannelRegistration::Publisher(candidate) if candidate == publisher).then_some(*channel))
    }

    fn channel_for_subscriber(&self, subscriber: &TopicSubscriber) -> Option<ChannelId> {
        self.active.as_ref()?.channels.iter().find_map(|(channel, registration)| matches!(registration, ChannelRegistration::Subscriber(candidate) if candidate == subscriber).then_some(*channel))
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
    if topic.is_empty()
        || topic.len() > 85
        || !topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ValidationError::Topic(topic.to_string()));
    }
    Ok(())
}

fn datagram_spec(label: String) -> DataChannelSpec {
    DataChannelSpec {
        label,
        ordered: false,
        reliability: DataChannelReliability::MaxRetransmits(0),
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
        let catalog = topics.catalog.clone();
        let mut snapshot = Snapshot::default();
        let mut notifications = VecDeque::new();
        let mut effects = VecDeque::new();
        topics.unbind_generation(
            generation,
            TopicDropReason::TransportReplaced,
            &mut snapshot,
            &mut notifications,
        );
        assert!(topics.active.is_none());
        assert!(topics.catalog.is_empty());
        assert_eq!(
            topics.subscribers[&subscriber]
                .ordered
                .get("authoritative")
                .and_then(|delivery| delivery.next_sequence),
            Some(4)
        );
        assert!(
            topics.subscribers[&subscriber].ordered["authoritative"]
                .pending
                .is_empty()
        );
        let replacement = ids.generation();
        let channel = ChannelId::new(2).unwrap();
        topics.bind(
            replacement,
            "local".into(),
            &TopicRegistrations {
                subscribers: alloc::vec![subscriber.clone()],
                ..TopicRegistrations::default()
            },
            alloc::vec![DataChannelBinding {
                label: subscriber_label(&subscriber),
                channel,
            }],
            &mut ids,
            &mut effects,
            &mut snapshot,
            &mut notifications,
        );
        // A late close from the retired graph cannot fence the replacement.
        topics.unbind_generation(
            generation,
            TopicDropReason::ChannelClosed,
            &mut snapshot,
            &mut notifications,
        );
        // This unit restores the known catalog directly, not through signaling
        // bootstrap. The on-wire gap is recorded in ORDERED_RECOVERY_CHECKPOINT.md.
        // Catalog removal is separately checked below, not inferred from unbind.
        topics.catalog = catalog;
        topics.probe_remote_publishers(&mut ids, &mut effects);
        let Some(Effect::DataChannel(DataChannelEffect::Send {
            generation,
            channel: actual_channel,
            payload,
            ..
        })) = effects.pop_front()
        else {
            panic!("replacement must probe the retained ordered cursor");
        };
        assert_eq!(generation, replacement);
        assert_eq!(actual_channel, channel);
        let frame = TopicFrame::decode_bounded(&payload).unwrap();
        let Some(Body::Probe(probe)) = frame.body else {
            panic!("ordered recovery probe");
        };
        assert_eq!(probe.data_track_id, "authoritative");
        assert_eq!(probe.from_sequence, 4);
        assert!(!probe.live, "reconnect is not a new live-only subscription");
    }

    #[test]
    fn ordered_cursor_is_fenced_by_close_catalog_removal_or_unsubscribe() {
        for boundary in 0..3 {
            let mut ids = IdGenerator::new();
            let (mut topics, subscriber, generation) = ordered_topics(&mut ids);
            let mut snapshot = Snapshot::default();
            let mut notifications = VecDeque::new();
            match boundary {
                0 => topics.end_session(&mut snapshot, &mut notifications),
                1 => topics.sync_catalog(
                    generation,
                    &crate::signaling_v1::CatalogState::default(),
                    &mut snapshot,
                    &mut notifications,
                    &mut ids,
                    &mut VecDeque::new(),
                ),
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
                active: Some(ActiveTopics {
                    generation,
                    canonical_participant_id: "local".into(),
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
    fn all_modes_are_explicit_and_topic_limit_does_not_use_media_labels() {
        for mode in [TopicMode::Latest, TopicMode::Lossy, TopicMode::Ordered] {
            let publisher = TopicPublisher {
                topic: "x".repeat(85),
                mode,
            };
            let registrations = TopicRegistrations {
                subscribers: alloc::vec![TopicSubscriber {
                    topic: publisher.topic.clone(),
                    mode
                }],
                publishers: alloc::vec![publisher],
            };
            registrations.validate().unwrap();
            let specs = Topics::channel_specs(&registrations);
            for spec in specs {
                assert!(spec.label.starts_with("v2/"));
                assert!(!spec.ordered);
                assert_eq!(spec.reliability, DataChannelReliability::MaxRetransmits(0));
            }
        }
        assert!(validate_topic(&"x".repeat(86)).is_err());
    }
}
