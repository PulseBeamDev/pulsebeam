use alloc::{collections::BTreeSet, format, string::String, vec::Vec};

use crate::{ChannelId, OperationId, Snapshot, ValidationError};

mod admission;
pub use admission::DataSendFailure;

pub const MAX_TOPIC_CHANNELS: usize = 64;
pub const MAX_TOPIC_PAYLOAD_BYTES: usize = pulsebeam_proto::data_record::MAX_UNIT_BYTES;
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
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopicPublisher {
    pub topic: String,
    pub mode: TopicMode,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TopicSubscriber {
    pub room: String,
    pub participant_id: String,
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
    pub captured_at: Option<crate::ffi::CaptureTimestamp>,
    pub room: String,
    pub topic: String,
    pub mode: TopicMode,
    pub publication_id: String,
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
        publication_id: String,
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
        publication_id: String,
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
    pub publication_id: Option<String>,
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
            crate::validate_room_name(&subscriber.room)?;
            crate::validate_participant_id(&subscriber.participant_id)?;
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

impl TopicSnapshot {
    pub(crate) fn publish(registrations: &TopicRegistrations, snapshot: &mut Snapshot) {
        let next = Self {
            publishers: registrations
                .publishers
                .iter()
                .cloned()
                .map(|registration| TopicPublisherStatus {
                    registration,
                    channel: None,
                    publication_id: None,
                    next_sequence: 0,
                    accepted_history: 0,
                    queued_messages: 0,
                    send_pending: false,
                })
                .collect(),
            subscribers: registrations
                .subscribers
                .iter()
                .cloned()
                .map(|registration| TopicSubscriberStatus {
                    registration,
                    channel: None,
                    publishers: 0,
                    buffered_messages: 0,
                })
                .collect(),
            ..Self::default()
        };
        if snapshot.topics != next {
            snapshot.topics = next;
            snapshot.version = snapshot.version.saturating_add(1);
        }
    }
}

fn validate_topic(topic: &str) -> Result<(), ValidationError> {
    crate::model::validate_track_id(topic)
}

fn publisher_label(publisher: &TopicPublisher) -> String {
    format!("{:?}/{}", publisher.mode, publisher.topic)
}

fn subscriber_label(subscriber: &TopicSubscriber) -> String {
    format!(
        "{}/{}/{:?}/{}",
        subscriber.room, subscriber.participant_id, subscriber.mode, subscriber.topic
    )
}
