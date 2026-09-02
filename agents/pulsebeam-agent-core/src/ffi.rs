use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate as model;

uniffi::setup_scaffolding!();

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RetryPolicy {
    pub initial_delay_ms: u64,
    pub maximum_delay_ms: u64,
    pub maximum_attempts: u8,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay_ms: 500,
            maximum_delay_ms: 5_000,
            maximum_attempts: 10,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct MediaTopology {
    pub local_video: Vec<String>,
    pub local_audio: Vec<String>,
    pub remote_video: u8,
    pub remote_audio: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentConfig {
    pub endpoint: String,
    pub room_id: String,
    pub request_headers: Vec<HttpHeader>,
    pub topology: MediaTopology,
    pub manual_subscriptions: bool,
    pub retry: RetryPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PublicationIntent {
    pub slot: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VideoDemand {
    pub slot: u8,
    pub publication_id: String,
    pub height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct AudioDemand {
    pub pinned: Vec<String>,
    pub automatic: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum PlayoutDelay {
    #[default]
    Adaptive,
    Fixed {
        min_ms: u32,
        max_ms: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TopicMode {
    Latest,
    Ordered,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TopicPublisher {
    pub name: String,
    pub mode: TopicMode,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TopicSubscriber {
    pub name: String,
    pub mode: TopicMode,
    pub publisher_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct DesiredState {
    pub connected: bool,
    pub publications: Vec<PublicationIntent>,
    pub video: Vec<VideoDemand>,
    pub audio: AudioDemand,
    pub playout_delay: PlayoutDelay,
    pub topic_publishers: Vec<TopicPublisher>,
    pub topic_subscribers: Vec<TopicSubscriber>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Participant {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Publication {
    pub id: String,
    pub participant_id: String,
    pub kind: MediaKind,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VideoBinding {
    pub publication_id: String,
    pub mid: String,
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AudioBinding {
    pub publication_id: String,
    pub mid: String,
    pub level_dbov: i8,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionState {
    Disconnected,
    CreatingOffer,
    Joining,
    ApplyingAnswer,
    WaitingForTransport,
    WaitingForSignaling,
    Connected,
    Reconnecting,
    RetryWaiting { attempt: u8, after_ms: u64 },
    Closing,
    TerminalFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FailureClass {
    InvalidConfiguration,
    Authorization,
    Protocol,
    Transient,
    ResourceExpired,
    RetryExhausted,
    Browser,
    Native,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Failure {
    pub class: FailureClass,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TopicPublisherState {
    pub publisher: TopicPublisher,
    pub connected: bool,
    pub stream_id: Option<u64>,
    pub next_sequence: Option<u64>,
    pub accepted_history: u64,
    pub replay_messages: u64,
    pub queued_messages: u64,
    pub send_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TopicSubscriberState {
    pub subscriber: TopicSubscriber,
    pub connected: bool,
    pub publishers: u64,
    pub buffered_messages: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct TopicState {
    pub publishers: Vec<TopicPublisherState>,
    pub subscribers: Vec<TopicSubscriberState>,
    pub accepted_sends: u64,
    pub dropped_sends: u64,
    pub delivered_messages: u64,
    pub resynchronizations: u64,
    pub channel_failures: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Snapshot {
    pub version: u64,
    pub desired_revision: u64,
    pub connection: ConnectionState,
    pub generation: Option<u64>,
    pub participant_id: Option<String>,
    pub participants: Vec<Participant>,
    pub publications: Vec<Publication>,
    pub video: Vec<VideoBinding>,
    pub audio: Vec<AudioBinding>,
    pub topics: TopicState,
    pub failure: Option<Failure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
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

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TopicMessage {
    Latest {
        name: String,
        publisher_id: Option<String>,
        payload: Vec<u8>,
    },
    Ordered {
        name: String,
        publisher_id: String,
        stream_id: u64,
        sequence: u64,
        payload: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TopicNotification {
    SendAdmitted {
        publisher: TopicPublisher,
        operation_id: u64,
        stream_id: Option<u64>,
        sequence: Option<u64>,
    },
    SendDropped {
        publisher: TopicPublisher,
        reason: TopicDropReason,
    },
    Message {
        message: TopicMessage,
    },
    ChannelFailed {
        name: String,
        mode: TopicMode,
        publishing: bool,
        publisher_id: Option<String>,
        message: String,
    },
    Resynchronized {
        subscriber: TopicSubscriber,
        publisher_id: String,
        stream_id: u64,
        next_sequence: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Notification {
    ConnectionChanged {
        from: ConnectionState,
        to: ConnectionState,
    },
    ParticipantAdded {
        participant: Participant,
    },
    ParticipantRemoved {
        participant_id: String,
    },
    PublicationAdded {
        publication: Publication,
    },
    PublicationRemoved {
        publication_id: String,
    },
    VideoBindingChanged {
        mid: String,
        binding: Option<VideoBinding>,
    },
    AudioBindingsChanged {
        bindings: Vec<AudioBinding>,
    },
    Topic {
        notification: TopicNotification,
    },
    Failure {
        failure: Failure,
    },
    ServerError {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MediaFrame {
    pub timestamp: u64,
    pub clock_rate: u32,
    pub data: Vec<u8>,
    pub absolute_capture_time_unix_us: Option<u64>,
    pub contiguous: bool,
    pub keyframe: bool,
    pub audio_level_dbov: Option<i8>,
    pub voice_activity: Option<bool>,
    pub target_bitrate_bps: Option<u64>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub frames_per_second: Option<u8>,
    pub dependency_descriptor: Option<Vec<u8>>,
    pub temporal_layers: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, uniffi::Record)]
pub struct TransportStatistics {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub round_trip_time_ms: Option<f64>,
    pub receive_loss: Option<f32>,
    pub keyframe_requests: u64,
    pub received_packets: u64,
    pub sent_packets: u64,
    pub unroutable_media_dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ErrorCode {
    InvalidConfiguration,
    InvalidDesiredState,
    InvalidTopic,
    Closed,
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentError {
    pub code: ErrorCode,
    pub message: String,
}

impl AgentConfig {
    pub fn into_core(self) -> Result<model::AgentConfig, AgentError> {
        let mut config = model::AgentConfig {
            endpoint: self.endpoint,
            room_id: self.room_id,
            request_headers: self.request_headers.into_iter().map(Into::into).collect(),
            topology: self.topology.into(),
            manual_subscriptions: self.manual_subscriptions,
            retry: self.retry.into(),
        };
        config.validate().map_err(|error| AgentError {
            code: ErrorCode::InvalidConfiguration,
            message: error.to_string(),
        })?;
        Ok(config)
    }
}

impl DesiredState {
    pub fn into_core(
        self,
        revision: u64,
        topology: &model::MediaTopology,
    ) -> Result<model::DesiredState, AgentError> {
        let mut desired = model::DesiredState {
            revision,
            connected: self.connected,
            publications: self.publications.into_iter().map(Into::into).collect(),
            video: self.video.into_iter().map(Into::into).collect(),
            audio: self.audio.into(),
            playout_delay: self.playout_delay.into(),
            topics: model::TopicRegistrations {
                publishers: self.topic_publishers.into_iter().map(Into::into).collect(),
                subscribers: self.topic_subscribers.into_iter().map(Into::into).collect(),
            },
        };
        desired.normalize();
        desired.validate(topology).map_err(|error| AgentError {
            code: ErrorCode::InvalidDesiredState,
            message: error.to_string(),
        })?;
        Ok(desired)
    }
}

impl From<model::DesiredState> for DesiredState {
    fn from(value: model::DesiredState) -> Self {
        Self {
            connected: value.connected,
            publications: value.publications.into_iter().map(Into::into).collect(),
            video: value.video.into_iter().map(Into::into).collect(),
            audio: value.audio.into(),
            playout_delay: value.playout_delay.into(),
            topic_publishers: value
                .topics
                .publishers
                .into_iter()
                .map(Into::into)
                .collect(),
            topic_subscribers: value
                .topics
                .subscribers
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<HttpHeader> for model::HttpHeader {
    fn from(value: HttpHeader) -> Self {
        Self {
            name: value.name,
            value: value.value,
        }
    }
}

impl From<model::HttpHeader> for HttpHeader {
    fn from(value: model::HttpHeader) -> Self {
        Self {
            name: value.name,
            value: value.value,
        }
    }
}

impl From<RetryPolicy> for model::RetryPolicy {
    fn from(value: RetryPolicy) -> Self {
        Self {
            initial_delay: Duration::from_millis(value.initial_delay_ms),
            maximum_delay: Duration::from_millis(value.maximum_delay_ms),
            maximum_attempts: value.maximum_attempts,
        }
    }
}

impl From<model::RetryPolicy> for RetryPolicy {
    fn from(value: model::RetryPolicy) -> Self {
        Self {
            initial_delay_ms: duration_ms(value.initial_delay),
            maximum_delay_ms: duration_ms(value.maximum_delay),
            maximum_attempts: value.maximum_attempts,
        }
    }
}

impl From<MediaTopology> for model::MediaTopology {
    fn from(value: MediaTopology) -> Self {
        Self {
            local_video: value.local_video,
            local_audio: value.local_audio,
            remote_video: value.remote_video,
            remote_audio: value.remote_audio,
        }
    }
}

impl From<model::MediaTopology> for MediaTopology {
    fn from(value: model::MediaTopology) -> Self {
        Self {
            local_video: value.local_video,
            local_audio: value.local_audio,
            remote_video: value.remote_video,
            remote_audio: value.remote_audio,
        }
    }
}

impl From<model::AgentConfig> for AgentConfig {
    fn from(value: model::AgentConfig) -> Self {
        Self {
            endpoint: value.endpoint,
            room_id: value.room_id,
            request_headers: value.request_headers.into_iter().map(Into::into).collect(),
            topology: value.topology.into(),
            manual_subscriptions: value.manual_subscriptions,
            retry: value.retry.into(),
        }
    }
}

impl From<PublicationIntent> for model::PublicationIntent {
    fn from(value: PublicationIntent) -> Self {
        Self {
            slot: value.slot,
            active: value.active,
        }
    }
}

impl From<model::PublicationIntent> for PublicationIntent {
    fn from(value: model::PublicationIntent) -> Self {
        Self {
            slot: value.slot,
            active: value.active,
        }
    }
}

impl From<VideoDemand> for model::VideoSubscription {
    fn from(value: VideoDemand) -> Self {
        Self {
            slot: value.slot,
            track_id: value.publication_id,
            height: value.height,
            min_height: value.min_height,
            min_fps: value.min_fps,
            priority: value.priority,
        }
    }
}

impl From<model::VideoSubscription> for VideoDemand {
    fn from(value: model::VideoSubscription) -> Self {
        Self {
            slot: value.slot,
            publication_id: value.track_id,
            height: value.height,
            min_height: value.min_height,
            min_fps: value.min_fps,
            priority: value.priority,
        }
    }
}

impl From<AudioDemand> for model::AudioSubscription {
    fn from(value: AudioDemand) -> Self {
        Self {
            pinned: value.pinned,
            automatic: value.automatic,
        }
    }
}

impl From<model::AudioSubscription> for AudioDemand {
    fn from(value: model::AudioSubscription) -> Self {
        Self {
            pinned: value.pinned,
            automatic: value.automatic,
        }
    }
}

impl From<PlayoutDelay> for model::PlayoutDelay {
    fn from(value: PlayoutDelay) -> Self {
        match value {
            PlayoutDelay::Adaptive => Self::Adaptive,
            PlayoutDelay::Fixed { min_ms, max_ms } => Self::Fixed { min_ms, max_ms },
        }
    }
}

impl From<model::PlayoutDelay> for PlayoutDelay {
    fn from(value: model::PlayoutDelay) -> Self {
        match value {
            model::PlayoutDelay::Adaptive => Self::Adaptive,
            model::PlayoutDelay::Fixed { min_ms, max_ms } => Self::Fixed { min_ms, max_ms },
        }
    }
}

impl From<TopicMode> for model::TopicMode {
    fn from(value: TopicMode) -> Self {
        match value {
            TopicMode::Latest => Self::Latest,
            TopicMode::Ordered => Self::Ordered,
        }
    }
}

impl From<model::TopicMode> for TopicMode {
    fn from(value: model::TopicMode) -> Self {
        match value {
            model::TopicMode::Latest => Self::Latest,
            model::TopicMode::Ordered => Self::Ordered,
        }
    }
}

impl From<TopicPublisher> for model::TopicPublisher {
    fn from(value: TopicPublisher) -> Self {
        Self {
            topic: value.name,
            mode: value.mode.into(),
        }
    }
}

impl From<model::TopicPublisher> for TopicPublisher {
    fn from(value: model::TopicPublisher) -> Self {
        Self {
            name: value.topic,
            mode: value.mode.into(),
        }
    }
}

impl From<TopicSubscriber> for model::TopicSubscriber {
    fn from(value: TopicSubscriber) -> Self {
        Self {
            topic: value.name,
            mode: value.mode.into(),
            publisher_id: value.publisher_id,
        }
    }
}

impl From<model::TopicSubscriber> for TopicSubscriber {
    fn from(value: model::TopicSubscriber) -> Self {
        Self {
            name: value.topic,
            mode: value.mode.into(),
            publisher_id: value.publisher_id,
        }
    }
}

impl From<model::MediaKind> for MediaKind {
    fn from(value: model::MediaKind) -> Self {
        match value {
            model::MediaKind::Video => Self::Video,
            model::MediaKind::Audio => Self::Audio,
        }
    }
}

impl From<model::Participant> for Participant {
    fn from(value: model::Participant) -> Self {
        Self { id: value.id }
    }
}

impl From<model::Publication> for Publication {
    fn from(value: model::Publication) -> Self {
        Self {
            id: value.id,
            participant_id: value.participant_id,
            kind: value.kind.into(),
        }
    }
}

impl From<model::VideoBinding> for VideoBinding {
    fn from(value: model::VideoBinding) -> Self {
        Self {
            publication_id: value.track_id,
            mid: value.mid,
            paused: value.paused,
        }
    }
}

impl From<model::AudioBinding> for AudioBinding {
    fn from(value: model::AudioBinding) -> Self {
        Self {
            publication_id: value.track_id,
            mid: value.mid,
            level_dbov: value.level_dbov,
        }
    }
}

impl From<model::ConnectionState> for ConnectionState {
    fn from(value: model::ConnectionState) -> Self {
        match value {
            model::ConnectionState::Disconnected => Self::Disconnected,
            model::ConnectionState::CreatingOffer => Self::CreatingOffer,
            model::ConnectionState::Joining => Self::Joining,
            model::ConnectionState::ApplyingAnswer => Self::ApplyingAnswer,
            model::ConnectionState::WaitingForTransport => Self::WaitingForTransport,
            model::ConnectionState::WaitingForSignaling => Self::WaitingForSignaling,
            model::ConnectionState::Connected => Self::Connected,
            model::ConnectionState::Reconnecting => Self::Reconnecting,
            model::ConnectionState::RetryWaiting { attempt, after } => Self::RetryWaiting {
                attempt,
                after_ms: duration_ms(after),
            },
            model::ConnectionState::Closing => Self::Closing,
            model::ConnectionState::TerminalFailure => Self::TerminalFailure,
        }
    }
}

impl From<model::FailureClass> for FailureClass {
    fn from(value: model::FailureClass) -> Self {
        match value {
            model::FailureClass::InvalidConfiguration => Self::InvalidConfiguration,
            model::FailureClass::Authorization => Self::Authorization,
            model::FailureClass::Protocol => Self::Protocol,
            model::FailureClass::Transient => Self::Transient,
            model::FailureClass::ResourceExpired => Self::ResourceExpired,
            model::FailureClass::RetryExhausted => Self::RetryExhausted,
        }
    }
}

impl From<model::Failure> for Failure {
    fn from(value: model::Failure) -> Self {
        Self {
            class: value.class.into(),
            message: value.message,
        }
    }
}

impl From<&model::TopicSnapshot> for TopicState {
    fn from(value: &model::TopicSnapshot) -> Self {
        Self {
            publishers: value
                .publishers
                .iter()
                .cloned()
                .map(|status| TopicPublisherState {
                    publisher: status.registration.into(),
                    connected: status.channel.is_some(),
                    stream_id: status.stream_id,
                    next_sequence: status.next_sequence,
                    accepted_history: count(status.accepted_history),
                    replay_messages: count(status.replay_messages),
                    queued_messages: count(status.queued_messages),
                    send_pending: status.send_pending,
                })
                .collect(),
            subscribers: value
                .subscribers
                .iter()
                .cloned()
                .map(|status| TopicSubscriberState {
                    subscriber: status.registration.into(),
                    connected: status.channel.is_some(),
                    publishers: count(status.publishers),
                    buffered_messages: count(status.buffered_messages),
                })
                .collect(),
            accepted_sends: value.accepted_sends,
            dropped_sends: value.dropped_sends,
            delivered_messages: value.delivered_messages,
            resynchronizations: value.resynchronizations,
            channel_failures: value.channel_failures,
        }
    }
}

impl From<&model::Snapshot> for Snapshot {
    fn from(value: &model::Snapshot) -> Self {
        Self {
            version: value.version,
            desired_revision: value.desired_revision,
            connection: value.connection.clone().into(),
            generation: value.generation.map(model::Generation::get),
            participant_id: value.participant_id.clone(),
            participants: value
                .participants
                .values()
                .cloned()
                .map(Into::into)
                .collect(),
            publications: value
                .publications
                .values()
                .cloned()
                .map(Into::into)
                .collect(),
            video: value.video.values().cloned().map(Into::into).collect(),
            audio: value.audio.iter().cloned().map(Into::into).collect(),
            topics: (&value.topics).into(),
            failure: value.terminal_failure.clone().map(Into::into),
        }
    }
}

impl From<model::TopicDropReason> for TopicDropReason {
    fn from(value: model::TopicDropReason) -> Self {
        match value {
            model::TopicDropReason::InvalidPayload => Self::InvalidPayload,
            model::TopicDropReason::NotRegistered => Self::NotRegistered,
            model::TopicDropReason::ChannelUnavailable => Self::ChannelUnavailable,
            model::TopicDropReason::QueueFull => Self::QueueFull,
            model::TopicDropReason::Superseded => Self::Superseded,
            model::TopicDropReason::HostRejected => Self::HostRejected,
            model::TopicDropReason::ChannelClosed => Self::ChannelClosed,
            model::TopicDropReason::TransportReplaced => Self::TransportReplaced,
            model::TopicDropReason::SequenceExhausted => Self::SequenceExhausted,
        }
    }
}

impl From<model::TopicMessage> for TopicMessage {
    fn from(value: model::TopicMessage) -> Self {
        match value {
            model::TopicMessage::Latest {
                topic,
                publisher_id,
                payload,
            } => Self::Latest {
                name: topic,
                publisher_id,
                payload,
            },
            model::TopicMessage::Ordered {
                topic,
                publisher_id,
                stream_id,
                sequence,
                payload,
            } => Self::Ordered {
                name: topic,
                publisher_id,
                stream_id,
                sequence,
                payload,
            },
        }
    }
}

impl From<model::TopicNotification> for TopicNotification {
    fn from(value: model::TopicNotification) -> Self {
        match value {
            model::TopicNotification::SendAdmitted {
                publisher,
                operation,
                stream_id,
                sequence,
            } => Self::SendAdmitted {
                publisher: publisher.into(),
                operation_id: operation.get(),
                stream_id,
                sequence,
            },
            model::TopicNotification::SendDropped { publisher, reason } => Self::SendDropped {
                publisher: publisher.into(),
                reason: reason.into(),
            },
            model::TopicNotification::Message(message) => Self::Message {
                message: message.into(),
            },
            model::TopicNotification::ChannelFailed { channel, message } => {
                let (name, mode, publishing, publisher_id) = match channel {
                    model::TopicChannel::Publisher(value) => {
                        (value.topic, value.mode.into(), true, None)
                    }
                    model::TopicChannel::Subscriber(value) => {
                        (value.topic, value.mode.into(), false, value.publisher_id)
                    }
                };
                Self::ChannelFailed {
                    name,
                    mode,
                    publishing,
                    publisher_id,
                    message,
                }
            }
            model::TopicNotification::Resynchronized {
                subscriber,
                publisher_id,
                stream_id,
                next_sequence,
            } => Self::Resynchronized {
                subscriber: subscriber.into(),
                publisher_id,
                stream_id,
                next_sequence,
            },
        }
    }
}

impl From<model::Notification> for Notification {
    fn from(value: model::Notification) -> Self {
        match value {
            model::Notification::ConnectionStateChanged { from, to } => Self::ConnectionChanged {
                from: from.into(),
                to: to.into(),
            },
            model::Notification::ParticipantAdded(participant) => Self::ParticipantAdded {
                participant: participant.into(),
            },
            model::Notification::ParticipantRemoved(participant_id) => {
                Self::ParticipantRemoved { participant_id }
            }
            model::Notification::PublicationAdded(publication) => Self::PublicationAdded {
                publication: publication.into(),
            },
            model::Notification::PublicationRemoved(publication_id) => {
                Self::PublicationRemoved { publication_id }
            }
            model::Notification::VideoBindingChanged { mid, binding } => {
                Self::VideoBindingChanged {
                    mid,
                    binding: binding.map(Into::into),
                }
            }
            model::Notification::AudioBindingsChanged(bindings) => Self::AudioBindingsChanged {
                bindings: bindings.into_iter().map(Into::into).collect(),
            },
            model::Notification::Topic(notification) => Self::Topic {
                notification: notification.into(),
            },
            model::Notification::Failure(failure) => Self::Failure {
                failure: failure.into(),
            },
            model::Notification::ServerError(message) => Self::ServerError { message },
        }
    }
}

impl From<model::AgentError> for AgentError {
    fn from(value: model::AgentError) -> Self {
        let code = match &value {
            model::AgentError::InvalidConfiguration(_) => ErrorCode::InvalidConfiguration,
            model::AgentError::StaleDesiredRevision { .. }
            | model::AgentError::ConflictingDesiredRevision(_)
            | model::AgentError::AdaptiveAfterFixed => ErrorCode::InvalidDesiredState,
            model::AgentError::InvalidTopic(_) => ErrorCode::InvalidTopic,
            model::AgentError::InvalidOffer(_) | model::AgentError::InvalidSignaling(_) => {
                ErrorCode::Runtime
            }
        };
        Self {
            code,
            message: value.to_string(),
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};
    use core::fmt::Debug;

    use super::*;

    fn config() -> AgentConfig {
        AgentConfig {
            endpoint: "https://sfu.example.com/".to_string(),
            room_id: "room".to_string(),
            request_headers: vec![HttpHeader {
                name: "Authorization".to_string(),
                value: "Bearer private".to_string(),
            }],
            topology: MediaTopology {
                local_video: vec!["camera".to_string()],
                local_audio: vec!["microphone".to_string()],
                remote_video: 2,
                remote_audio: 1,
            },
            manual_subscriptions: true,
            retry: RetryPolicy::default(),
        }
    }

    #[test]
    fn config_round_trip_preserves_the_portable_contract() {
        let original = config();
        let core = original.clone().into_core().expect("valid boundary config");
        assert_eq!(core.endpoint, "https://sfu.example.com");
        let mut normalized = original;
        normalized.endpoint.pop();
        assert_eq!(AgentConfig::from(core), normalized);
    }

    #[test]
    fn invalid_boundary_values_are_rejected_before_the_runtime() {
        let mut invalid = config();
        invalid.endpoint = "relative".to_string();
        let error = invalid
            .into_core()
            .expect_err("relative endpoint must fail");
        assert_eq!(error.code, ErrorCode::InvalidConfiguration);

        let topology = config().into_core().expect("valid config").topology;
        let desired = DesiredState {
            video: vec![VideoDemand {
                slot: 3,
                publication_id: "participant/video".to_string(),
                height: 720,
                min_height: 180,
                min_fps: 15,
                priority: 1,
            }],
            ..DesiredState::default()
        };
        let error = desired
            .into_core(1, &topology)
            .expect_err("out-of-range receive slot must fail");
        assert_eq!(error.code, ErrorCode::InvalidDesiredState);
    }

    #[test]
    fn desired_conversion_normalizes_complete_state() {
        let topology = config().into_core().expect("valid config").topology;
        let desired = DesiredState {
            connected: true,
            publications: vec![PublicationIntent {
                slot: "camera".to_string(),
                active: true,
            }],
            topic_publishers: vec![
                TopicPublisher {
                    name: "z".to_string(),
                    mode: TopicMode::Latest,
                },
                TopicPublisher {
                    name: "a".to_string(),
                    mode: TopicMode::Ordered,
                },
            ],
            ..DesiredState::default()
        };
        let converted = desired
            .into_core(7, &topology)
            .expect("valid desired state");
        assert_eq!(converted.revision, 7);
        assert_eq!(converted.topics.publishers[0].topic, "a");
        assert_eq!(converted.topics.publishers[1].topic, "z");
        let projected = DesiredState::from(converted);
        assert_eq!(projected.publications.len(), 1);
        assert_eq!(projected.topic_publishers[0].name, "a");
        assert_eq!(projected.topic_publishers[1].name, "z");
    }

    #[test]
    fn snapshots_are_deterministic_owned_values() {
        let mut snapshot = model::Snapshot::default();
        snapshot.participants.insert(
            "z".to_string(),
            model::Participant {
                id: "z".to_string(),
            },
        );
        snapshot.participants.insert(
            "a".to_string(),
            model::Participant {
                id: "a".to_string(),
            },
        );
        let projected = Snapshot::from(&snapshot);
        assert_eq!(
            projected
                .participants
                .into_iter()
                .map(|value| value.id)
                .collect::<Vec<_>>(),
            vec!["a".to_string(), "z".to_string()],
        );
    }

    #[test]
    fn notification_conversion_keeps_topic_payload_and_identity() {
        let notification = model::Notification::Topic(model::TopicNotification::Message(
            model::TopicMessage::Ordered {
                topic: "chat".to_string(),
                publisher_id: "participant".to_string(),
                stream_id: 4,
                sequence: 9,
                payload: vec![1, 2, 3],
            },
        ));
        assert_eq!(
            Notification::from(notification),
            Notification::Topic {
                notification: TopicNotification::Message {
                    message: TopicMessage::Ordered {
                        name: "chat".to_string(),
                        publisher_id: "participant".to_string(),
                        stream_id: 4,
                        sequence: 9,
                        payload: vec![1, 2, 3],
                    },
                },
            },
        );
    }

    #[test]
    fn every_portable_record_survives_uniffi_serialization() {
        let publisher = TopicPublisher {
            name: "chat".to_string(),
            mode: TopicMode::Ordered,
        };
        let subscriber = TopicSubscriber {
            name: "chat".to_string(),
            mode: TopicMode::Ordered,
            publisher_id: Some("participant".to_string()),
        };
        let publication = Publication {
            id: "participant/video".to_string(),
            participant_id: "participant".to_string(),
            kind: MediaKind::Video,
        };
        let video = VideoBinding {
            publication_id: publication.id.clone(),
            mid: "0".to_string(),
            paused: false,
        };
        let audio = AudioBinding {
            publication_id: "participant/audio".to_string(),
            mid: "1".to_string(),
            level_dbov: 42,
        };
        let failure = Failure {
            class: FailureClass::Protocol,
            message: "invalid response".to_string(),
        };
        let publisher_state = TopicPublisherState {
            publisher: publisher.clone(),
            connected: true,
            stream_id: Some(4),
            next_sequence: Some(9),
            accepted_history: 3,
            replay_messages: 2,
            queued_messages: 1,
            send_pending: true,
        };
        let subscriber_state = TopicSubscriberState {
            subscriber: subscriber.clone(),
            connected: true,
            publishers: 1,
            buffered_messages: 2,
        };
        let topics = TopicState {
            publishers: vec![publisher_state.clone()],
            subscribers: vec![subscriber_state.clone()],
            accepted_sends: 8,
            dropped_sends: 1,
            delivered_messages: 7,
            resynchronizations: 2,
            channel_failures: 1,
        };

        assert_ffi_round_trip(HttpHeader {
            name: "X-Test".to_string(),
            value: "value".to_string(),
        });
        assert_ffi_round_trip(RetryPolicy::default());
        assert_ffi_round_trip(config().topology);
        assert_ffi_round_trip(config());
        assert_ffi_round_trip(PublicationIntent {
            slot: "camera".to_string(),
            active: true,
        });
        assert_ffi_round_trip(VideoDemand {
            slot: 1,
            publication_id: publication.id.clone(),
            height: 720,
            min_height: 180,
            min_fps: 15,
            priority: 2,
        });
        assert_ffi_round_trip(AudioDemand {
            pinned: vec![audio.publication_id.clone()],
            automatic: false,
        });
        assert_ffi_round_trip(publisher.clone());
        assert_ffi_round_trip(subscriber.clone());
        assert_ffi_round_trip(DesiredState {
            connected: true,
            publications: vec![PublicationIntent {
                slot: "camera".to_string(),
                active: true,
            }],
            video: vec![],
            audio: AudioDemand::default(),
            playout_delay: PlayoutDelay::Fixed {
                min_ms: 80,
                max_ms: 160,
            },
            topic_publishers: vec![publisher],
            topic_subscribers: vec![subscriber],
        });
        assert_ffi_round_trip(Participant {
            id: "participant".to_string(),
        });
        assert_ffi_round_trip(publication.clone());
        assert_ffi_round_trip(video.clone());
        assert_ffi_round_trip(audio.clone());
        assert_ffi_round_trip(failure.clone());
        assert_ffi_round_trip(publisher_state);
        assert_ffi_round_trip(subscriber_state);
        assert_ffi_round_trip(topics.clone());
        assert_ffi_round_trip(Snapshot {
            version: 12,
            desired_revision: 7,
            connection: ConnectionState::RetryWaiting {
                attempt: 2,
                after_ms: 500,
            },
            generation: Some(3),
            participant_id: Some("participant".to_string()),
            participants: vec![Participant {
                id: "participant".to_string(),
            }],
            publications: vec![publication],
            video: vec![video],
            audio: vec![audio],
            topics,
            failure: Some(failure),
        });
        assert_ffi_round_trip(MediaFrame {
            timestamp: 90_000,
            clock_rate: 90_000,
            data: vec![1, 2, 3],
            absolute_capture_time_unix_us: Some(123),
            contiguous: true,
            keyframe: true,
            audio_level_dbov: Some(42),
            voice_activity: Some(true),
            target_bitrate_bps: Some(1_000_000),
            width: Some(1280),
            height: Some(720),
            frames_per_second: Some(30),
            dependency_descriptor: Some(vec![4, 5]),
            temporal_layers: Some(3),
        });
        assert_ffi_round_trip(TransportStatistics {
            bytes_sent: 10,
            bytes_received: 20,
            round_trip_time_ms: Some(12.5),
            receive_loss: Some(0.02),
            keyframe_requests: 2,
            received_packets: 3,
            sent_packets: 4,
            unroutable_media_dropped: 1,
        });
        assert_ffi_round_trip(AgentError {
            code: ErrorCode::Runtime,
            message: "runtime unavailable".to_string(),
        });
    }

    #[test]
    fn every_portable_enum_variant_survives_uniffi_serialization() {
        for value in [
            PlayoutDelay::Adaptive,
            PlayoutDelay::Fixed {
                min_ms: 1,
                max_ms: 2,
            },
        ] {
            assert_ffi_round_trip(value);
        }
        for value in [TopicMode::Latest, TopicMode::Ordered] {
            assert_ffi_round_trip(value);
        }
        for value in [MediaKind::Video, MediaKind::Audio] {
            assert_ffi_round_trip(value);
        }
        for value in [
            ConnectionState::Disconnected,
            ConnectionState::CreatingOffer,
            ConnectionState::Joining,
            ConnectionState::ApplyingAnswer,
            ConnectionState::WaitingForTransport,
            ConnectionState::WaitingForSignaling,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
            ConnectionState::RetryWaiting {
                attempt: 2,
                after_ms: 500,
            },
            ConnectionState::Closing,
            ConnectionState::TerminalFailure,
        ] {
            assert_ffi_round_trip(value);
        }
        for value in [
            FailureClass::InvalidConfiguration,
            FailureClass::Authorization,
            FailureClass::Protocol,
            FailureClass::Transient,
            FailureClass::ResourceExpired,
            FailureClass::RetryExhausted,
            FailureClass::Browser,
            FailureClass::Native,
        ] {
            assert_ffi_round_trip(value);
        }
        for value in [
            TopicDropReason::InvalidPayload,
            TopicDropReason::NotRegistered,
            TopicDropReason::ChannelUnavailable,
            TopicDropReason::QueueFull,
            TopicDropReason::Superseded,
            TopicDropReason::HostRejected,
            TopicDropReason::ChannelClosed,
            TopicDropReason::TransportReplaced,
            TopicDropReason::SequenceExhausted,
        ] {
            assert_ffi_round_trip(value);
        }
        for value in [
            ErrorCode::InvalidConfiguration,
            ErrorCode::InvalidDesiredState,
            ErrorCode::InvalidTopic,
            ErrorCode::Closed,
            ErrorCode::Runtime,
        ] {
            assert_ffi_round_trip(value);
        }
        assert_ffi_round_trip(TopicMessage::Latest {
            name: "cursor".to_string(),
            publisher_id: None,
            payload: vec![1],
        });
        assert_ffi_round_trip(TopicMessage::Ordered {
            name: "chat".to_string(),
            publisher_id: "participant".to_string(),
            stream_id: 3,
            sequence: 4,
            payload: vec![2],
        });
        let publisher = TopicPublisher {
            name: "chat".to_string(),
            mode: TopicMode::Ordered,
        };
        let subscriber = TopicSubscriber {
            name: "chat".to_string(),
            mode: TopicMode::Ordered,
            publisher_id: Some("participant".to_string()),
        };
        for value in [
            TopicNotification::SendAdmitted {
                publisher: publisher.clone(),
                operation_id: 1,
                stream_id: Some(2),
                sequence: Some(3),
            },
            TopicNotification::SendDropped {
                publisher: publisher.clone(),
                reason: TopicDropReason::QueueFull,
            },
            TopicNotification::Message {
                message: TopicMessage::Latest {
                    name: "chat".to_string(),
                    publisher_id: Some("participant".to_string()),
                    payload: vec![1, 2],
                },
            },
            TopicNotification::ChannelFailed {
                name: "chat".to_string(),
                mode: TopicMode::Ordered,
                publishing: false,
                publisher_id: Some("participant".to_string()),
                message: "closed".to_string(),
            },
            TopicNotification::Resynchronized {
                subscriber,
                publisher_id: "participant".to_string(),
                stream_id: 2,
                next_sequence: 3,
            },
        ] {
            assert_ffi_round_trip(value);
        }
        let participant = Participant {
            id: "participant".to_string(),
        };
        let publication = Publication {
            id: "participant/video".to_string(),
            participant_id: participant.id.clone(),
            kind: MediaKind::Video,
        };
        let video = VideoBinding {
            publication_id: publication.id.clone(),
            mid: "0".to_string(),
            paused: false,
        };
        let audio = AudioBinding {
            publication_id: "participant/audio".to_string(),
            mid: "1".to_string(),
            level_dbov: 42,
        };
        for value in [
            Notification::ConnectionChanged {
                from: ConnectionState::Joining,
                to: ConnectionState::Connected,
            },
            Notification::ParticipantAdded {
                participant: participant.clone(),
            },
            Notification::ParticipantRemoved {
                participant_id: participant.id,
            },
            Notification::PublicationAdded {
                publication: publication.clone(),
            },
            Notification::PublicationRemoved {
                publication_id: publication.id,
            },
            Notification::VideoBindingChanged {
                mid: "0".to_string(),
                binding: Some(video),
            },
            Notification::AudioBindingsChanged {
                bindings: vec![audio],
            },
            Notification::Topic {
                notification: TopicNotification::SendDropped {
                    publisher,
                    reason: TopicDropReason::QueueFull,
                },
            },
            Notification::Failure {
                failure: Failure {
                    class: FailureClass::Protocol,
                    message: "invalid response".to_string(),
                },
            },
            Notification::ServerError {
                message: "unavailable".to_string(),
            },
        ] {
            assert_ffi_round_trip(value);
        }
    }

    fn assert_ffi_round_trip<T>(value: T)
    where
        T: Clone + Debug + PartialEq + uniffi::Lift<UniFfiTag> + uniffi::Lower<UniFfiTag>,
    {
        let buffer = <T as uniffi::Lower<UniFfiTag>>::lower_into_rust_buffer(value.clone());
        let lifted = <T as uniffi::Lift<UniFfiTag>>::try_lift_from_rust_buffer(buffer)
            .expect("valid UniFFI serialization");
        assert_eq!(lifted, value);
    }
}
