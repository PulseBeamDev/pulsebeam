use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};

use pulsebeam_proto::{
    prelude::Message,
    reliable::{RelControl, RelDelivery, RelMsg, RelNack, rel_control},
};

use crate::{
    ChannelId, DataChannelBinding, DataChannelEffect, DataChannelReliability, DataChannelSpec,
    Effect, Generation, Notification, OperationId, Snapshot, ValidationError, id::IdGenerator,
};

pub const MAX_TOPIC_CHANNELS: usize = 64;
pub const MAX_TOPIC_PAYLOAD_BYTES: usize = 65_536;
pub const TOPIC_HISTORY_CAPACITY: usize = 256;
pub const TOPIC_REORDER_CAPACITY: usize = 256;
pub const TOPIC_SEND_QUEUE_CAPACITY: usize = 256;

const MAX_TOPIC_LABEL_BYTES: usize = 96;
const MAX_TOPIC_FRAME_BYTES: usize = MAX_TOPIC_PAYLOAD_BYTES.saturating_add(512);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TopicMode {
    Latest,
    Ordered,
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
    pub publisher_id: Option<String>,
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
pub enum TopicMessage {
    Latest {
        topic: String,
        publisher_id: Option<String>,
        payload: Vec<u8>,
    },
    Ordered {
        topic: String,
        publisher_id: String,
        stream_id: u64,
        sequence: u64,
        payload: Vec<u8>,
    },
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
        stream_id: Option<u64>,
        sequence: Option<u64>,
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
    Resynchronized {
        subscriber: TopicSubscriber,
        publisher_id: String,
        stream_id: u64,
        next_sequence: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopicSnapshot {
    pub publishers: Vec<TopicPublisherStatus>,
    pub subscribers: Vec<TopicSubscriberStatus>,
    pub accepted_sends: u64,
    pub dropped_sends: u64,
    pub delivered_messages: u64,
    pub resynchronizations: u64,
    pub channel_failures: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicPublisherStatus {
    pub registration: TopicPublisher,
    pub channel: Option<ChannelId>,
    pub stream_id: Option<u64>,
    pub next_sequence: Option<u64>,
    pub accepted_history: usize,
    pub replay_messages: usize,
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
    #[error("topic publisher channel is unavailable: {0}")]
    PublisherUnavailable(String),
    #[error("topic publisher queue is full: {0}")]
    SendQueueFull(String),
    #[error("ordered topic sequence space is exhausted: {0}")]
    SequenceExhausted(String),
    #[error("topic message is malformed")]
    MalformedMessage,
    #[error("topic message has an invalid publisher identity")]
    InvalidPublisher,
    #[error("topic message has an invalid stream identity or sequence")]
    InvalidSequence,
    #[error("topic message belongs to a retired stream")]
    StaleStream,
    #[error("topic control is invalid")]
    InvalidControl,
    #[error("topic channel is not active")]
    UnknownChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopicDirection {
    Publish,
    Subscribe,
}

#[derive(Clone)]
enum ChannelRegistration {
    Publisher(TopicPublisher),
    Subscriber(TopicSubscriber),
}

struct ActiveTopics {
    generation: Generation,
    participant_id: String,
    channels: BTreeMap<ChannelId, ChannelRegistration>,
}

struct PendingSend {
    operation: OperationId,
    generation: Generation,
    channel: ChannelId,
    stream_id: Option<u64>,
    sequence: Option<u64>,
    payload: Vec<u8>,
}

struct PublisherState {
    stream_id: Option<u64>,
    next_sequence: u64,
    bound_once: bool,
    history: VecDeque<RelMsg>,
    queue: VecDeque<Vec<u8>>,
    pending: Option<PendingSend>,
}

struct SubscriberState {
    publishers: BTreeMap<String, PublisherDelivery>,
}

struct PublisherDelivery {
    stream_id: u64,
    next_sequence: u64,
    pending: BTreeMap<u64, RelMsg>,
    retired_streams: VecDeque<u64>,
}

struct AuxiliarySend {
    generation: Generation,
    channel: ChannelId,
    registration: TopicChannel,
}

#[derive(Default)]
pub(crate) struct Topics {
    publishers: BTreeMap<TopicPublisher, PublisherState>,
    subscribers: BTreeMap<TopicSubscriber, SubscriberState>,
    active: Option<ActiveTopics>,
    auxiliary_sends: BTreeMap<OperationId, AuxiliarySend>,
    next_stream_id: u64,
    accepted_sends: u64,
    dropped_sends: u64,
    delivered_messages: u64,
    resynchronizations: u64,
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
            if !publishers.insert(publisher.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "topic publisher",
                    value: label(
                        publisher.mode,
                        TopicDirection::Publish,
                        &publisher.topic,
                        None,
                    ),
                });
            }
        }

        let mut subscribers = BTreeSet::new();
        let mut latest_scopes: BTreeMap<&str, Vec<Option<&str>>> = BTreeMap::new();
        for subscriber in &self.subscribers {
            validate_topic(&subscriber.topic)?;
            if subscriber.mode == TopicMode::Ordered && subscriber.publisher_id.is_some() {
                return Err(ValidationError::TopicScope(subscriber.topic.clone()));
            }
            if let Some(publisher_id) = subscriber.publisher_id.as_deref() {
                validate_publisher_id(publisher_id)
                    .map_err(|()| ValidationError::TopicScope(publisher_id.to_string()))?;
            }
            let channel_label = subscriber_label(subscriber);
            if channel_label.len() > MAX_TOPIC_LABEL_BYTES {
                return Err(ValidationError::Topic(subscriber.topic.clone()));
            }
            if !subscribers.insert(subscriber.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "topic subscriber",
                    value: channel_label,
                });
            }
            if subscriber.mode == TopicMode::Latest {
                latest_scopes
                    .entry(subscriber.topic.as_str())
                    .or_default()
                    .push(subscriber.publisher_id.as_deref());
            }
        }
        for (topic, scopes) in latest_scopes {
            if scopes.len() > 1 && scopes.iter().any(Option::is_none) {
                return Err(ValidationError::Duplicate {
                    field: "overlapping latest subscriber",
                    value: topic.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl Topics {
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
        let desired_publishers: BTreeSet<_> = registrations.publishers.iter().cloned().collect();
        let removed_publishers: Vec<_> = self
            .publishers
            .keys()
            .filter(|publisher| !desired_publishers.contains(*publisher))
            .cloned()
            .collect();
        for publisher in removed_publishers {
            if let Some(mut state) = self.publishers.remove(&publisher) {
                self.drop_publisher_sends(
                    &publisher,
                    &mut state,
                    TopicDropReason::NotRegistered,
                    notifications,
                );
            }
        }
        for publisher in &registrations.publishers {
            if !self.publishers.contains_key(publisher) {
                let stream_id = if publisher.mode == TopicMode::Ordered {
                    self.stream_id()
                } else {
                    None
                };
                self.publishers.insert(
                    publisher.clone(),
                    PublisherState {
                        stream_id,
                        next_sequence: 0,
                        bound_once: false,
                        history: VecDeque::with_capacity(TOPIC_HISTORY_CAPACITY),
                        queue: VecDeque::with_capacity(TOPIC_SEND_QUEUE_CAPACITY),
                        pending: None,
                    },
                );
            }
        }

        let desired_subscribers: BTreeSet<_> = registrations.subscribers.iter().cloned().collect();
        self.subscribers
            .retain(|subscriber, _| desired_subscribers.contains(subscriber));
        for subscriber in &registrations.subscribers {
            self.subscribers
                .entry(subscriber.clone())
                .or_insert_with(|| SubscriberState {
                    publishers: BTreeMap::new(),
                });
        }

        if let Some(active) = self.active.as_mut() {
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
        log::info!(
            "reconciled topic registrations publishers={} subscribers={}",
            self.publishers.len(),
            self.subscribers.len(),
        );
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn channel_specs(registrations: &TopicRegistrations) -> Vec<DataChannelSpec> {
        registrations
            .publishers
            .iter()
            .map(publisher_spec)
            .chain(registrations.subscribers.iter().map(subscriber_spec))
            .collect()
    }

    pub(crate) fn expected_labels(registrations: &TopicRegistrations) -> BTreeSet<String> {
        Self::channel_specs(registrations)
            .into_iter()
            .map(|spec| spec.label)
            .collect()
    }

    pub(crate) fn bind(
        &mut self,
        generation: Generation,
        participant_id: String,
        registrations: &TopicRegistrations,
        bindings: Vec<DataChannelBinding>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        if self.active.is_some() {
            self.unbind(TopicDropReason::TransportReplaced, snapshot, notifications);
        }

        let by_label: BTreeMap<_, _> = bindings
            .into_iter()
            .map(|binding| (binding.label, binding.channel))
            .collect();
        let mut channels = BTreeMap::new();
        for publisher in &registrations.publishers {
            if let Some(channel) = by_label.get(&publisher_label(publisher)).copied()
                && self.publishers.contains_key(publisher)
            {
                channels.insert(channel, ChannelRegistration::Publisher(publisher.clone()));
                let rotate = self
                    .publishers
                    .get(publisher)
                    .is_some_and(|state| state.bound_once && publisher.mode == TopicMode::Ordered);
                let stream_id = if rotate { self.stream_id() } else { None };
                if let Some(state) = self.publishers.get_mut(publisher) {
                    if rotate {
                        state.stream_id = stream_id;
                        state.next_sequence = 0;
                    }
                    state.bound_once = true;
                }
            }
        }
        for subscriber in &registrations.subscribers {
            if let Some(channel) = by_label.get(&subscriber_label(subscriber)).copied()
                && self.subscribers.contains_key(subscriber)
            {
                channels.insert(channel, ChannelRegistration::Subscriber(subscriber.clone()));
                if let Some(state) = self.subscribers.get_mut(subscriber) {
                    state.publishers.clear();
                }
            }
        }
        self.active = Some(ActiveTopics {
            generation,
            participant_id,
            channels,
        });
        log::info!(
            "bound topic channels generation={} publishers={} subscribers={}",
            generation.get(),
            registrations.publishers.len(),
            registrations.subscribers.len(),
        );
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
            let actual = send.payload.len();
            self.record_drop(
                &send.publisher,
                TopicDropReason::InvalidPayload,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PayloadTooLarge {
                actual,
                maximum: MAX_TOPIC_PAYLOAD_BYTES,
            });
        }
        let Some(state) = self.publishers.get_mut(&send.publisher) else {
            self.record_drop(
                &send.publisher,
                TopicDropReason::NotRegistered,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherNotRegistered(send.publisher.topic));
        };
        let available = self.active.as_ref().is_some_and(|active| {
            active.channels.values().any(|registration| {
                matches!(registration, ChannelRegistration::Publisher(publisher) if publisher == &send.publisher)
            })
        });
        if !available {
            self.record_drop(
                &send.publisher,
                TopicDropReason::ChannelUnavailable,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::PublisherUnavailable(send.publisher.topic));
        }
        if send.publisher.mode == TopicMode::Ordered
            && (state.stream_id.is_none() || state.next_sequence == u64::MAX)
        {
            self.record_drop(
                &send.publisher,
                TopicDropReason::SequenceExhausted,
                notifications,
            );
            self.refresh_snapshot(snapshot);
            return Err(TopicError::SequenceExhausted(send.publisher.topic));
        }
        if state.queue.len() >= TOPIC_SEND_QUEUE_CAPACITY {
            self.record_drop(&send.publisher, TopicDropReason::QueueFull, notifications);
            self.refresh_snapshot(snapshot);
            return Err(TopicError::SendQueueFull(send.publisher.topic));
        }
        let superseded =
            send.publisher.mode == TopicMode::Latest && state.queue.pop_back().is_some();
        state.queue.push_back(send.payload);
        if superseded {
            self.record_drop(&send.publisher, TopicDropReason::Superseded, notifications);
        }
        self.dispatch_publisher(&send.publisher, ids, effects);
        self.refresh_snapshot(snapshot);
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one deterministic transition updates all four owned core outputs"
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
            let _ = self.auxiliary_sends.remove(&operation);
            return true;
        }

        let publisher = self.publishers.iter().find_map(|(publisher, state)| {
            state.pending.as_ref().and_then(|pending| {
                (pending.operation == operation
                    && pending.generation == generation
                    && pending.channel == channel)
                    .then(|| publisher.clone())
            })
        });
        let Some(publisher) = publisher else {
            return false;
        };
        let Some(state) = self.publishers.get_mut(&publisher) else {
            debug_assert!(false, "pending topic send must have a publisher");
            return true;
        };
        let Some(pending) = state.pending.take() else {
            debug_assert!(false, "matched topic send must be pending");
            return true;
        };
        if let (Some(stream_id), Some(sequence)) = (pending.stream_id, pending.sequence) {
            debug_assert_eq!(state.stream_id, Some(stream_id));
            debug_assert_eq!(state.next_sequence, sequence);
            if state.history.len() == TOPIC_HISTORY_CAPACITY {
                let _ = state.history.pop_front();
            }
            state.history.push_back(RelMsg {
                stream_id,
                seq: sequence,
                payload: pending.payload,
                resync_required: false,
            });
            state.next_sequence = state.next_sequence.saturating_add(1);
        }
        self.accepted_sends = self.accepted_sends.saturating_add(1);
        log::debug!(
            "topic send admitted mode={:?} topic={} generation={} operation={} stream={:?} sequence={:?}",
            publisher.mode,
            publisher.topic,
            generation.get(),
            operation.get(),
            pending.stream_id,
            pending.sequence,
        );
        notifications.push_back(Notification::Topic(TopicNotification::SendAdmitted {
            publisher: publisher.clone(),
            operation,
            stream_id: pending.stream_id,
            sequence: pending.sequence,
        }));
        self.dispatch_publisher(&publisher, ids, effects);
        self.refresh_snapshot(snapshot);
        true
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one deterministic transition updates all four owned core outputs"
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
                self.refresh_snapshot(snapshot);
            }
            return true;
        }

        let publisher = self.publishers.iter().find_map(|(publisher, state)| {
            state.pending.as_ref().and_then(|pending| {
                (pending.operation == operation
                    && pending.generation == generation
                    && pending.channel == channel)
                    .then(|| publisher.clone())
            })
        });
        let Some(publisher) = publisher else {
            return false;
        };
        if let Some(state) = self.publishers.get_mut(&publisher) {
            let _ = state.pending.take();
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
        reason = "one deterministic transition updates all four owned core outputs"
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
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if active.generation != generation {
            return Ok(false);
        }
        let Some(registration) = active.channels.get(&channel).cloned() else {
            return Err(TopicError::UnknownChannel);
        };
        let participant_id = active.participant_id.clone();

        match registration {
            ChannelRegistration::Publisher(publisher) => {
                if publisher.mode != TopicMode::Ordered {
                    return Err(TopicError::InvalidControl);
                }
                self.handle_control(
                    generation,
                    channel,
                    &participant_id,
                    &publisher,
                    &payload,
                    ids,
                    effects,
                )?;
            }
            ChannelRegistration::Subscriber(subscriber) => match subscriber.mode {
                TopicMode::Latest => {
                    if payload.len() > MAX_TOPIC_PAYLOAD_BYTES {
                        return Err(TopicError::PayloadTooLarge {
                            actual: payload.len(),
                            maximum: MAX_TOPIC_PAYLOAD_BYTES,
                        });
                    }
                    notifications.push_back(Notification::Topic(TopicNotification::Message(
                        TopicMessage::Latest {
                            topic: subscriber.topic.clone(),
                            publisher_id: subscriber.publisher_id.clone(),
                            payload,
                        },
                    )));
                    self.delivered_messages = self.delivered_messages.saturating_add(1);
                }
                TopicMode::Ordered => {
                    self.handle_ordered_message(
                        generation,
                        channel,
                        &subscriber,
                        &payload,
                        ids,
                        effects,
                        notifications,
                    )?;
                }
            },
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
        let Some(registration) = self.active.as_mut().and_then(|active| {
            (active.generation == generation)
                .then(|| active.channels.remove(&channel))
                .flatten()
        }) else {
            return false;
        };
        match registration {
            ChannelRegistration::Publisher(publisher) => {
                if let Some(mut state) = self.publishers.remove(&publisher) {
                    self.drop_publisher_sends(
                        &publisher,
                        &mut state,
                        TopicDropReason::ChannelClosed,
                        notifications,
                    );
                    self.publishers.insert(publisher.clone(), state);
                }
                self.record_channel_failure(
                    TopicChannel::Publisher(publisher),
                    "data channel closed".to_string(),
                    notifications,
                );
            }
            ChannelRegistration::Subscriber(subscriber) => {
                if let Some(state) = self.subscribers.get_mut(&subscriber) {
                    state.publishers.clear();
                }
                self.record_channel_failure(
                    TopicChannel::Subscriber(subscriber),
                    "data channel closed".to_string(),
                    notifications,
                );
            }
        }
        self.refresh_snapshot(snapshot);
        true
    }

    fn unbind(
        &mut self,
        reason: TopicDropReason,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        let _ = self.active.take();
        self.auxiliary_sends.clear();
        let publishers: Vec<_> = self.publishers.keys().cloned().collect();
        for publisher in publishers {
            if let Some(mut state) = self.publishers.remove(&publisher) {
                self.drop_publisher_sends(&publisher, &mut state, reason, notifications);
                self.publishers.insert(publisher, state);
            }
        }
        for state in self.subscribers.values_mut() {
            state.publishers.clear();
        }
        self.refresh_snapshot(snapshot);
    }

    fn dispatch_publisher(
        &mut self,
        publisher: &TopicPublisher,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let channel = active.channels.iter().find_map(|(channel, registration)| {
            matches!(registration, ChannelRegistration::Publisher(candidate) if candidate == publisher)
                .then_some(*channel)
        });
        let Some(channel) = channel else {
            return;
        };
        let Some(state) = self.publishers.get_mut(publisher) else {
            return;
        };
        if state.pending.is_some() {
            return;
        }
        let Some(payload) = state.queue.pop_front() else {
            return;
        };
        let operation = ids.operation();
        let (wire_payload, stream_id, sequence) = match publisher.mode {
            TopicMode::Latest => (payload.clone(), None, None),
            TopicMode::Ordered => {
                let Some(stream_id) = state.stream_id else {
                    debug_assert!(false, "ordered publisher must have a stream identity");
                    return;
                };
                let sequence = state.next_sequence;
                let message = RelMsg {
                    stream_id,
                    seq: sequence,
                    payload: payload.clone(),
                    resync_required: false,
                };
                (
                    encode_delivery(&active.participant_id, &message),
                    Some(stream_id),
                    Some(sequence),
                )
            }
        };
        state.pending = Some(PendingSend {
            operation,
            generation: active.generation,
            channel,
            stream_id,
            sequence,
            payload,
        });
        log::debug!(
            "dispatching topic send mode={:?} topic={} generation={} operation={} channel={} bytes={}",
            publisher.mode,
            publisher.topic,
            active.generation.get(),
            operation.get(),
            channel.get(),
            wire_payload.len(),
        );
        effects.push_back(Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation: active.generation,
            channel,
            binary: true,
            payload: wire_payload,
        }));
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "control recovery needs correlated I/O and active stream identity"
    )]
    fn handle_control(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        participant_id: &str,
        publisher: &TopicPublisher,
        payload: &[u8],
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) -> Result<(), TopicError> {
        if payload.len() > MAX_TOPIC_FRAME_BYTES {
            return Err(TopicError::MalformedMessage);
        }
        let control = RelControl::decode(payload).map_err(|_| TopicError::MalformedMessage)?;
        let Some(rel_control::Msg::Nack(nack)) = control.msg else {
            return Err(TopicError::InvalidControl);
        };
        if nack.publisher_id != participant_id {
            return Err(TopicError::InvalidControl);
        }
        let Some(state) = self.publishers.get(publisher) else {
            return Err(TopicError::InvalidControl);
        };
        let Some(stream_id) = state.stream_id else {
            return Err(TopicError::InvalidControl);
        };
        if nack.stream_id != stream_id {
            return Err(TopicError::StaleStream);
        }
        if nack.from_seq >= state.next_sequence {
            return Err(TopicError::InvalidControl);
        }
        let replay: Vec<_> = state
            .history
            .iter()
            .filter(|message| message.stream_id == stream_id)
            .cloned()
            .collect();
        let Some(earliest) = replay.first() else {
            return Err(TopicError::InvalidControl);
        };
        if earliest.seq > nack.from_seq {
            let reset = RelMsg {
                stream_id,
                seq: earliest.seq,
                payload: Vec::new(),
                resync_required: true,
            };
            self.emit_auxiliary(
                generation,
                channel,
                TopicChannel::Publisher(publisher.clone()),
                encode_delivery(participant_id, &reset),
                ids,
                effects,
            );
        }
        for message in replay
            .into_iter()
            .filter(|message| message.seq >= nack.from_seq)
        {
            self.emit_auxiliary(
                generation,
                channel,
                TopicChannel::Publisher(publisher.clone()),
                encode_delivery(participant_id, &message),
                ids,
                effects,
            );
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "ordered recovery atomically updates protocol state and owned outputs"
    )]
    fn handle_ordered_message(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        subscriber: &TopicSubscriber,
        payload: &[u8],
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        notifications: &mut VecDeque<Notification>,
    ) -> Result<(), TopicError> {
        if payload.len() > MAX_TOPIC_FRAME_BYTES {
            return Err(TopicError::MalformedMessage);
        }
        let delivery = RelDelivery::decode(payload).map_err(|_| TopicError::MalformedMessage)?;
        validate_publisher_id(&delivery.publisher_id).map_err(|()| TopicError::InvalidPublisher)?;
        let message =
            RelMsg::decode(delivery.frame.as_slice()).map_err(|_| TopicError::MalformedMessage)?;
        if message.stream_id == 0 || message.seq == u64::MAX {
            return Err(TopicError::InvalidSequence);
        }
        if message.resync_required && !message.payload.is_empty() {
            return Err(TopicError::InvalidControl);
        }
        if message.payload.len() > MAX_TOPIC_PAYLOAD_BYTES {
            return Err(TopicError::PayloadTooLarge {
                actual: message.payload.len(),
                maximum: MAX_TOPIC_PAYLOAD_BYTES,
            });
        }
        let publisher_id = delivery.publisher_id;
        let stream_id = message.stream_id;
        let Some(subscriber_state) = self.subscribers.get_mut(subscriber) else {
            return Ok(());
        };
        let state = subscriber_state
            .publishers
            .entry(publisher_id.clone())
            .or_insert_with(|| PublisherDelivery::new(&message));
        let result = state.accept(message)?;
        if result.resynchronized {
            self.resynchronizations = self.resynchronizations.saturating_add(1);
            log::warn!(
                "ordered topic resynchronized topic={} generation={} stream={} next_sequence={}",
                subscriber.topic,
                generation.get(),
                stream_id,
                result.next_sequence,
            );
            notifications.push_back(Notification::Topic(TopicNotification::Resynchronized {
                subscriber: subscriber.clone(),
                publisher_id: publisher_id.clone(),
                stream_id,
                next_sequence: result.next_sequence,
            }));
        }
        for message in result.messages {
            self.delivered_messages = self.delivered_messages.saturating_add(1);
            notifications.push_back(Notification::Topic(TopicNotification::Message(
                TopicMessage::Ordered {
                    topic: subscriber.topic.clone(),
                    publisher_id: publisher_id.clone(),
                    stream_id: message.stream_id,
                    sequence: message.seq,
                    payload: message.payload,
                },
            )));
        }
        if let Some(from_seq) = result.nack_from {
            let nack = RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id,
                    from_seq,
                    publisher_id,
                })),
            };
            self.emit_auxiliary(
                generation,
                channel,
                TopicChannel::Subscriber(subscriber.clone()),
                nack.encode_to_vec(),
                ids,
                effects,
            );
        }
        Ok(())
    }

    fn emit_auxiliary(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        registration: TopicChannel,
        payload: Vec<u8>,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) {
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
            payload,
        }));
    }

    fn drop_publisher_sends(
        &mut self,
        publisher: &TopicPublisher,
        state: &mut PublisherState,
        reason: TopicDropReason,
        notifications: &mut VecDeque<Notification>,
    ) {
        let mut dropped = state.queue.len();
        if state.pending.take().is_some() {
            dropped = dropped.saturating_add(1);
        }
        state.queue.clear();
        for _ in 0..dropped {
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
        log::warn!(
            "topic send dropped mode={:?} topic={} reason={reason:?}",
            publisher.mode,
            publisher.topic,
        );
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
        log::warn!("topic channel failed channel={channel:?}");
        notifications.push_back(Notification::Topic(TopicNotification::ChannelFailed {
            channel,
            message,
        }));
    }

    fn stream_id(&mut self) -> Option<u64> {
        let next = self.next_stream_id.checked_add(1)?;
        self.next_stream_id = next;
        Some(next)
    }

    fn refresh_snapshot(&self, snapshot: &mut Snapshot) {
        let next = TopicSnapshot {
            publishers: self
                .publishers
                .iter()
                .map(|(registration, state)| {
                    let channel = self.channel_for_publisher(registration);
                    let replay_messages = state.stream_id.map_or(0, |stream_id| {
                        state
                            .history
                            .iter()
                            .filter(|message| message.stream_id == stream_id)
                            .count()
                    });
                    TopicPublisherStatus {
                        registration: registration.clone(),
                        channel,
                        stream_id: state.stream_id,
                        next_sequence: (registration.mode == TopicMode::Ordered)
                            .then_some(state.next_sequence),
                        accepted_history: state.history.len(),
                        replay_messages,
                        queued_messages: state.queue.len(),
                        send_pending: state.pending.is_some(),
                    }
                })
                .collect(),
            subscribers: self
                .subscribers
                .iter()
                .map(|(registration, state)| TopicSubscriberStatus {
                    registration: registration.clone(),
                    channel: self.channel_for_subscriber(registration),
                    publishers: state.publishers.len(),
                    buffered_messages: state
                        .publishers
                        .values()
                        .fold(0usize, |total, publisher| {
                            total.saturating_add(publisher.pending.len())
                        }),
                })
                .collect(),
            accepted_sends: self.accepted_sends,
            dropped_sends: self.dropped_sends,
            delivered_messages: self.delivered_messages,
            resynchronizations: self.resynchronizations,
            channel_failures: self.channel_failures,
        };
        if snapshot.topics != next {
            snapshot.topics = next;
            snapshot.version = snapshot.version.saturating_add(1);
        }
    }

    fn channel_for_publisher(&self, publisher: &TopicPublisher) -> Option<ChannelId> {
        self.active.as_ref().and_then(|active| {
            active.channels.iter().find_map(|(channel, registration)| {
                matches!(registration, ChannelRegistration::Publisher(candidate) if candidate == publisher)
                    .then_some(*channel)
            })
        })
    }

    fn channel_for_subscriber(&self, subscriber: &TopicSubscriber) -> Option<ChannelId> {
        self.active.as_ref().and_then(|active| {
            active.channels.iter().find_map(|(channel, registration)| {
                matches!(registration, ChannelRegistration::Subscriber(candidate) if candidate == subscriber)
                    .then_some(*channel)
            })
        })
    }
}

struct DeliveryResult {
    messages: Vec<RelMsg>,
    nack_from: Option<u64>,
    resynchronized: bool,
    next_sequence: u64,
}

impl PublisherDelivery {
    fn new(message: &RelMsg) -> Self {
        Self {
            stream_id: message.stream_id,
            next_sequence: message.seq,
            pending: BTreeMap::new(),
            retired_streams: VecDeque::with_capacity(TOPIC_HISTORY_CAPACITY),
        }
    }

    fn accept(&mut self, message: RelMsg) -> Result<DeliveryResult, TopicError> {
        let mut resynchronized = false;
        if self.stream_id != message.stream_id {
            if self.retired_streams.contains(&message.stream_id) {
                return Err(TopicError::StaleStream);
            }
            if self.retired_streams.len() == TOPIC_HISTORY_CAPACITY {
                let _ = self.retired_streams.pop_front();
            }
            self.retired_streams.push_back(self.stream_id);
            self.stream_id = message.stream_id;
            self.next_sequence = message.seq;
            self.pending.clear();
            resynchronized = true;
        }
        if message.resync_required {
            self.next_sequence = message.seq;
            self.pending.clear();
            return Ok(DeliveryResult {
                messages: Vec::new(),
                nack_from: None,
                resynchronized: true,
                next_sequence: self.next_sequence,
            });
        }
        if message.seq >= self.next_sequence {
            if message.seq.saturating_sub(self.next_sequence) >= TOPIC_REORDER_CAPACITY as u64 {
                self.next_sequence = message.seq;
                self.pending.clear();
                resynchronized = true;
            }
            self.pending.entry(message.seq).or_insert(message);
        }
        let mut messages = Vec::new();
        while let Some(current) = self.pending.remove(&self.next_sequence) {
            debug_assert_eq!(current.stream_id, self.stream_id);
            debug_assert_eq!(current.seq, self.next_sequence);
            messages.push(current);
            self.next_sequence = self.next_sequence.saturating_add(1);
        }
        let nack_from = (!self.pending.is_empty()).then_some(self.next_sequence);
        Ok(DeliveryResult {
            messages,
            nack_from,
            resynchronized,
            next_sequence: self.next_sequence,
        })
    }
}

fn validate_topic(topic: &str) -> Result<(), ValidationError> {
    if topic.is_empty()
        || !topic
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        || label(TopicMode::Latest, TopicDirection::Publish, topic, None).len()
            > MAX_TOPIC_LABEL_BYTES
    {
        return Err(ValidationError::Topic(topic.to_string()));
    }
    Ok(())
}

fn validate_publisher_id(publisher_id: &str) -> Result<(), ()> {
    let Some(encoded) = publisher_id.strip_prefix("pa_") else {
        return Err(());
    };
    if encoded.len() != 26
        || !encoded.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte.to_ascii_uppercase(),
                    b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
    {
        return Err(());
    }
    Ok(())
}

fn publisher_spec(publisher: &TopicPublisher) -> DataChannelSpec {
    spec(publisher.mode, publisher_label(publisher))
}

fn subscriber_spec(subscriber: &TopicSubscriber) -> DataChannelSpec {
    spec(subscriber.mode, subscriber_label(subscriber))
}

fn spec(mode: TopicMode, label: String) -> DataChannelSpec {
    match mode {
        TopicMode::Latest => DataChannelSpec {
            label,
            ordered: false,
            reliability: DataChannelReliability::MaxRetransmits(0),
        },
        TopicMode::Ordered => DataChannelSpec {
            label,
            ordered: true,
            reliability: DataChannelReliability::Reliable,
        },
    }
}

fn publisher_label(publisher: &TopicPublisher) -> String {
    label(
        publisher.mode,
        TopicDirection::Publish,
        &publisher.topic,
        None,
    )
}

fn subscriber_label(subscriber: &TopicSubscriber) -> String {
    label(
        subscriber.mode,
        TopicDirection::Subscribe,
        &subscriber.topic,
        subscriber.publisher_id.as_deref(),
    )
}

fn label(
    mode: TopicMode,
    direction: TopicDirection,
    topic: &str,
    publisher_id: Option<&str>,
) -> String {
    let lane = match mode {
        TopicMode::Latest => "rt",
        TopicMode::Ordered => "rel",
    };
    let direction = match direction {
        TopicDirection::Publish => "pub",
        TopicDirection::Subscribe => "sub",
    };
    match publisher_id {
        Some(publisher_id) => format!("v1/{lane}/{direction}/{topic}/{publisher_id}"),
        None => format!("v1/{lane}/{direction}/{topic}"),
    }
}

fn encode_delivery(publisher_id: &str, message: &RelMsg) -> Vec<u8> {
    RelDelivery {
        publisher_id: publisher_id.to_string(),
        frame: message.encode_to_vec(),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(stream_id: u64, seq: u64) -> RelMsg {
        RelMsg {
            stream_id,
            seq,
            payload: alloc::vec![u8::try_from(seq).unwrap_or(u8::MAX)],
            resync_required: false,
        }
    }

    #[test]
    fn labels_and_reliability_match_the_server_contract() {
        let latest = publisher_spec(&TopicPublisher {
            topic: "game-sync".to_string(),
            mode: TopicMode::Latest,
        });
        assert_eq!(latest.label, "v1/rt/pub/game-sync");
        assert!(!latest.ordered);
        assert_eq!(
            latest.reliability,
            DataChannelReliability::MaxRetransmits(0)
        );

        let ordered = subscriber_spec(&TopicSubscriber {
            topic: "chat".to_string(),
            mode: TopicMode::Ordered,
            publisher_id: None,
        });
        assert_eq!(ordered.label, "v1/rel/sub/chat");
        assert!(ordered.ordered);
        assert_eq!(ordered.reliability, DataChannelReliability::Reliable);
    }

    #[test]
    fn ordered_delivery_reorders_and_deduplicates() {
        let mut delivery = PublisherDelivery::new(&message(7, 0));
        let first = delivery.accept(message(7, 0)).unwrap();
        assert_eq!(
            first
                .messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            alloc::vec![0]
        );
        assert_eq!(first.nack_from, None);

        let gap = delivery.accept(message(7, 2)).unwrap();
        assert!(gap.messages.is_empty());
        assert_eq!(gap.nack_from, Some(1));

        let filled = delivery.accept(message(7, 1)).unwrap();
        assert_eq!(
            filled
                .messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            alloc::vec![1, 2]
        );
        assert_eq!(filled.nack_from, None);
        assert!(delivery.accept(message(7, 2)).unwrap().messages.is_empty());
    }

    #[test]
    fn stream_changes_resynchronize_and_retired_streams_are_rejected() {
        let mut delivery = PublisherDelivery::new(&message(3, 9));
        let _ = delivery.accept(message(3, 9)).unwrap();
        let changed = delivery.accept(message(4, 0)).unwrap();
        assert!(changed.resynchronized);
        assert_eq!(changed.messages[0].seq, 0);
        assert!(matches!(
            delivery.accept(message(3, 10)),
            Err(TopicError::StaleStream)
        ));
    }
}
