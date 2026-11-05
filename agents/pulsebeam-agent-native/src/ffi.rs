//! Generated-language boundary for the shared native actor and owned media.
use std::sync::Arc;

use agent_core::ffi as core_ffi;
use tokio::sync::Mutex;

mod media;
pub use media::*;

use crate::{
    Agent as RuntimeAgent, AgentEvent as RuntimeEvent, Config, EncodingLimits, EncodingState,
    Error as RuntimeError, Host, InputFrame, MediaConfig, MediaStreamUpdate, NativeStatistics,
    ObservationStream, ProducerFeedback, Publication as RuntimePublication, ReceiveMode,
    RemoteMedia, SourceIdentity, StreamUpdate, Subscription as RuntimeSubscription,
};

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct NativeAgentConfig {
    pub session: core_ffi::AgentConfig,
    pub media: MediaConfig,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NativeError {
    #[error("core operation failed: {error:?}")]
    Core { error: core_ffi::AgentError },
    #[error("resource operation failed: {reason:?}")]
    Resource { reason: core_ffi::ResourceFailure },
    #[error("topic operation failed: {reason:?}")]
    Topic { reason: core_ffi::TopicFailure },
    #[error("data send failed: {reason}")]
    DataSend { reason: agent_core::DataSendFailure },
    #[error("native SDK operation failed: {message}")]
    Native { message: String },
    #[error("agent or media resource is closed")]
    Closed,
    #[error("native media resource is not available yet")]
    NotReady,
    #[error("bounded admission queue is full")]
    Backpressure,
    #[error("invalid media input: {message}")]
    InvalidInput { message: String },
}

impl From<RuntimeError> for NativeError {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::Core(agent_core::AgentError::InvalidResource(reason)) => Self::Resource {
                reason: reason.into(),
            },
            RuntimeError::Core(agent_core::AgentError::InvalidTopic(reason)) => Self::Topic {
                reason: reason.into(),
            },
            RuntimeError::DataSend(reason) => Self::DataSend { reason },
            RuntimeError::Core(error) => Self::Core {
                error: error.into(),
            },
            RuntimeError::Rtc(message) => Self::Native { message },
            RuntimeError::Closed => Self::Closed,
            RuntimeError::NotReady => Self::NotReady,
            RuntimeError::Backpressure => Self::Backpressure,
            RuntimeError::UnknownSlot(slot) => Self::InvalidInput {
                message: format!("unknown media slot: {slot:?}"),
            },
            RuntimeError::InvalidInput(message) => Self::InvalidInput { message },
        }
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum NativeEvent {
    Core {
        notification: core_ffi::Notification,
    },
    ProducerFeedback {
        slot: String,
        source: SourceIdentity,
        feedback: ProducerFeedback,
    },
    ChannelMessagesDropped {
        generation: u64,
        channel: u64,
        messages: u64,
        bytes: u64,
    },
    RuntimeFailed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
#[allow(
    clippy::large_enum_variant,
    reason = "UniFFI outcomes carry portable snapshots by value"
)]
pub enum SnapshotUpdate {
    Snapshot { snapshot: core_ffi::Snapshot },
    Lagged { skipped: u64 },
    Closed,
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum EventUpdate {
    Event { event: NativeEvent },
    Lagged { skipped: u64 },
    Closed,
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum FeedbackUpdate {
    Feedback { feedback: ProducerFeedback },
    Lagged { skipped: u64 },
    Closed,
}

#[derive(uniffi::Object)]
pub struct Agent {
    runtime: RuntimeAgent,
}

#[uniffi::export(async_runtime = "tokio")]
impl Agent {
    // UniFFI 0.31 Python supports named async constructors, not async primary ones.
    #[uniffi::constructor]
    pub async fn create(config: core_ffi::AgentConfig) -> Result<Arc<Self>, NativeError> {
        Self::create_with_media(NativeAgentConfig {
            session: config,
            media: MediaConfig::default(),
        })
        .await
    }

    #[uniffi::constructor]
    pub async fn create_with_media(config: NativeAgentConfig) -> Result<Arc<Self>, NativeError> {
        let session = config
            .session
            .into_core()
            .map_err(|error| NativeError::Core { error })?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| NativeError::Native {
                message: format!("failed to initialize HTTP client: {error}"),
            })?;
        let runtime = RuntimeAgent::spawn(
            Config {
                session,
                media: config.media,
                decoded_video_observer: None,
                encoded_video_observer: None,
            },
            Host::new(Box::new(client)),
        )
        .await?;
        Ok(Self::from_runtime(runtime))
    }

    pub async fn renew_authorization(&self, token: String) -> Result<(), NativeError> {
        self.runtime
            .renew_authorization(token)
            .await
            .map_err(Into::into)
    }
    pub fn data(
        &self,
        label: String,
        delivery: core_ffi::TopicMode,
    ) -> Result<Arc<LocalDataTrack>, NativeError> {
        Ok(Arc::new(LocalDataTrack {
            runtime: self.runtime.data(label, delivery.into())?,
        }))
    }
    pub fn participant(&self, id: String) -> Result<Arc<Participant>, NativeError> {
        Ok(Arc::new(Participant {
            runtime: self.runtime.participant(id)?,
        }))
    }
    pub fn snapshot(&self) -> core_ffi::Snapshot {
        (&self.runtime.snapshot()).into()
    }
    pub fn snapshots(&self) -> Arc<SnapshotStream> {
        let (initial, updates) = self.runtime.snapshot_events();
        Arc::new(SnapshotStream {
            initial: Mutex::new(Some(initial)),
            updates,
        })
    }
    pub fn events(&self) -> Arc<EventStream> {
        Arc::new(EventStream {
            updates: self.runtime.events(),
        })
    }
    pub fn statistics(&self) -> Option<NativeStatistics> {
        self.runtime.statistics().borrow().clone()
    }
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn connect(&self) -> Result<(), NativeError> {
        self.runtime.connect().await.map_err(Into::into)
    }
    pub async fn disconnect(&self) -> Result<(), NativeError> {
        self.runtime.disconnect().await.map_err(Into::into)
    }
    pub async fn reconnect(&self) -> Result<(), NativeError> {
        self.runtime.reconnect().await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

impl Agent {
    pub fn from_runtime(runtime: RuntimeAgent) -> Arc<Self> {
        Arc::new(Self { runtime })
    }
}

#[derive(uniffi::Object)]
pub struct LocalDataTrack {
    runtime: crate::LocalDataTrack,
}

#[uniffi::export(async_runtime = "tokio")]
impl LocalDataTrack {
    pub async fn send(&self, data: Vec<u8>) -> Result<(), NativeError> {
        self.runtime.send(data).await.map_err(Into::into)
    }
    pub async fn receive(&self) -> Result<Arc<DataReceiver>, NativeError> {
        Ok(Arc::new(DataReceiver {
            runtime: self.runtime.receive().await?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct Participant {
    runtime: crate::Participant,
}

#[uniffi::export]
impl Participant {
    pub fn id(&self) -> String {
        self.runtime.id().into()
    }
    pub fn data(
        &self,
        label: String,
        delivery: core_ffi::TopicMode,
    ) -> Result<Arc<RemoteDataTrack>, NativeError> {
        Ok(Arc::new(RemoteDataTrack {
            runtime: self.runtime.data(label, delivery.into())?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteDataTrack {
    runtime: crate::RemoteDataTrack,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteDataTrack {
    pub async fn receive(&self) -> Result<Arc<DataReceiver>, NativeError> {
        Ok(Arc::new(DataReceiver {
            runtime: self.runtime.receive().await?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct DataReceiver {
    runtime: crate::DataReceiver,
}

#[uniffi::export(async_runtime = "tokio")]
impl DataReceiver {
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn next(&self) -> Result<agent_core::DataUpdate, NativeError> {
        self.runtime.next().await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct RemoteMediaReceiver {
    runtime: RemoteMedia,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteMediaReceiver {
    pub fn source_track_id(&self) -> Option<String> {
        self.runtime.source_track_id().map(str::to_owned)
    }
    pub fn generation(&self) -> u64 {
        self.runtime.generation()
    }
    pub fn receiver(&self) -> u64 {
        self.runtime.receiver()
    }
    pub async fn next(&self) -> MediaStreamUpdate {
        self.runtime.next().await
    }
    pub async fn close(&self) {
        self.runtime.close().await;
    }
    pub async fn request_keyframe(&self) -> Result<(), NativeError> {
        self.runtime.request_keyframe().await.map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct SnapshotStream {
    initial: Mutex<Option<agent_core::Snapshot>>,
    updates: ObservationStream<agent_core::Snapshot>,
}
#[uniffi::export(async_runtime = "tokio")]
impl SnapshotStream {
    pub fn is_closed(&self) -> bool {
        self.updates.is_closed()
    }
    pub async fn close(&self) {
        self.updates.close().await;
        self.initial.lock().await.take();
    }
    pub async fn next(&self) -> SnapshotUpdate {
        let initial = self.initial.lock().await.take();
        if self.updates.is_closed() {
            return SnapshotUpdate::Closed;
        }
        if let Some(snapshot) = initial {
            return SnapshotUpdate::Snapshot {
                snapshot: (&snapshot).into(),
            };
        }
        match self.updates.next().await {
            StreamUpdate::Value(snapshot) => SnapshotUpdate::Snapshot {
                snapshot: (&snapshot).into(),
            },
            StreamUpdate::Lagged { skipped } => SnapshotUpdate::Lagged { skipped },
            StreamUpdate::Closed => SnapshotUpdate::Closed,
        }
    }
}

#[derive(uniffi::Object)]
pub struct EventStream {
    updates: ObservationStream<RuntimeEvent>,
}
#[uniffi::export(async_runtime = "tokio")]
impl EventStream {
    pub fn is_closed(&self) -> bool {
        self.updates.is_closed()
    }
    pub async fn close(&self) {
        self.updates.close().await;
    }
    pub async fn next(&self) -> EventUpdate {
        match self.updates.next().await {
            StreamUpdate::Value(event) => EventUpdate::Event {
                event: match event {
                    RuntimeEvent::Core(notification) => NativeEvent::Core {
                        notification: notification.into(),
                    },
                    RuntimeEvent::ProducerFeedback {
                        slot,
                        source,
                        feedback,
                    } => NativeEvent::ProducerFeedback {
                        slot: match slot {
                            agent_core::MediaSlot::LocalVideo(slot) => slot,
                            slot => format!("{slot:?}"),
                        },
                        source,
                        feedback,
                    },
                    RuntimeEvent::ChannelMessagesDropped {
                        generation,
                        channel,
                        messages,
                        bytes,
                    } => NativeEvent::ChannelMessagesDropped {
                        generation,
                        channel,
                        messages,
                        bytes,
                    },
                    RuntimeEvent::RuntimeFailed(message) => NativeEvent::RuntimeFailed { message },
                },
            },
            StreamUpdate::Lagged { skipped } => EventUpdate::Lagged { skipped },
            StreamUpdate::Closed => EventUpdate::Closed,
        }
    }
}

#[derive(uniffi::Object)]
pub struct FeedbackStream {
    updates: ObservationStream<ProducerFeedback>,
}
#[uniffi::export(async_runtime = "tokio")]
impl FeedbackStream {
    pub fn is_closed(&self) -> bool {
        self.updates.is_closed()
    }
    pub async fn close(&self) {
        self.updates.close().await;
    }
    pub async fn next(&self) -> FeedbackUpdate {
        match self.updates.next().await {
            StreamUpdate::Value(feedback) => FeedbackUpdate::Feedback { feedback },
            StreamUpdate::Lagged { skipped } => FeedbackUpdate::Lagged { skipped },
            StreamUpdate::Closed => FeedbackUpdate::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn foreign_control_uses_actor_order_and_failed_desired_does_not_consume_a_revision() {
        let session = agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "token".into(),
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
        };
        // No connect intent: this is a boundary/policy test, not native media proof.
        let agent = Agent::create(session.into()).await.unwrap();
        let mut desired = core_ffi::DesiredState::from(agent_core::DesiredState::default());
        desired.publications.push(core_ffi::PublicationIntent {
            slot: "publication".into(),
            kind: core_ffi::MediaKind::Video,
            active: true,
            label: String::new(),
        });
        assert!(matches!(
            agent.runtime.replace_foreign_desired(desired).await,
            Err(_)
        ));
        assert_eq!(agent.snapshot().desired_revision, 0);
        let disconnected = || core_ffi::DesiredState::from(agent_core::DesiredState::default());
        agent
            .runtime
            .replace_foreign_desired(disconnected())
            .await
            .unwrap();
        assert_eq!(agent.snapshot().desired_revision, 1);
        agent.disconnect().await.unwrap();
        agent
            .runtime
            .replace_foreign_desired(disconnected())
            .await
            .unwrap();
        assert_eq!(agent.snapshot().desired_revision, 2);
        let events = agent.events();
        let snapshots = agent.snapshots();
        agent.close().await.unwrap();
        agent.close().await.unwrap();
        assert!(agent.is_closed());
        assert!(matches!(agent.connect().await, Err(NativeError::Closed)));
        assert!(
            agent
                .runtime
                .replace_foreign_desired(disconnected())
                .await
                .is_err()
        );
        assert!(matches!(events.next().await, EventUpdate::Closed));
        assert!(matches!(snapshots.next().await, SnapshotUpdate::Closed));
        assert!(matches!(
            agent.audio("microphone".into()),
            Err(NativeError::Closed)
        ));
    }
}
