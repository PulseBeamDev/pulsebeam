//! Generated-language boundary for the shared native actor and owned media.
use std::sync::Arc;

use agent_core::ffi as core_ffi;
use tokio::sync::Mutex;

use crate::{
    Agent as RuntimeAgent, AgentEvent as RuntimeEvent, Config, EncodingLimits, EncodingState,
    Error as RuntimeError, Host, InputFrame, LocalSource as RuntimeSource, MediaConfig,
    MediaStreamUpdate, NativeStatistics, ObservationStream, ProducerFeedback, ReceiveMode,
    RemoteMedia, SourceIdentity, StreamUpdate,
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
    pub async fn create(config: NativeAgentConfig) -> Result<Arc<Self>, NativeError> {
        let session = config
            .session
            .into_core()
            .map_err(|error| NativeError::Core { error })?;
        let runtime = RuntimeAgent::spawn(
            Config {
                session,
                media: config.media,
                decoded_video_observer: None,
                encoded_video_observer: None,
            },
            Host::new(Box::new(reqwest::Client::new())),
        )
        .await?;
        Ok(Self::from_runtime(runtime))
    }

    pub async fn replace_desired(
        &self,
        desired: core_ffi::DesiredState,
    ) -> Result<(), NativeError> {
        self.runtime
            .replace_foreign_desired(desired)
            .await
            .map_err(|error| NativeError::Core { error })
    }
    pub async fn renew_authorization(&self, token: String) -> Result<(), NativeError> {
        self.runtime
            .renew_authorization(token)
            .await
            .map_err(Into::into)
    }
    pub async fn send_topic(
        &self,
        publisher: core_ffi::TopicPublisher,
        payload: Vec<u8>,
    ) -> Result<(), NativeError> {
        self.runtime
            .send_topic(agent_core::TopicSend {
                publisher: publisher.into(),
                payload,
            })
            .await
            .map_err(Into::into)
    }
    /// Acquires a new owned source, replacing/detaching any previous source in
    /// the slot. Profile selection remains fixed in NativeAgentConfig.
    pub async fn local_video(&self, slot: String) -> Result<Arc<LocalSource>, NativeError> {
        self.source(agent_core::MediaSlot::LocalVideo(slot)).await
    }
    pub async fn local_audio(&self, slot: String) -> Result<Arc<LocalSource>, NativeError> {
        self.source(agent_core::MediaSlot::LocalAudio(slot)).await
    }
    pub async fn remote_video(
        &self,
        slot: u8,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        self.remote(agent_core::MediaSlot::RemoteVideo(slot), mode)
            .await
    }
    pub async fn remote_audio(
        &self,
        slot: u8,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        self.remote(agent_core::MediaSlot::RemoteAudio(slot), mode)
            .await
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
    pub async fn reconnect(&self) -> Result<(), NativeError> {
        self.runtime.reconnect().await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
    pub async fn abort(&self) -> Result<(), NativeError> {
        self.runtime.abort().await.map_err(Into::into)
    }
}

impl Agent {
    pub fn from_runtime(runtime: RuntimeAgent) -> Arc<Self> {
        Arc::new(Self { runtime })
    }
    async fn source(&self, slot: agent_core::MediaSlot) -> Result<Arc<LocalSource>, NativeError> {
        Ok(Arc::new(LocalSource {
            runtime: self.runtime.source(slot).await?,
        }))
    }
    async fn remote(
        &self,
        slot: agent_core::MediaSlot,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        Ok(Arc::new(RemoteMediaReceiver {
            runtime: self.runtime.remote(slot, mode).await?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct LocalSource {
    runtime: RuntimeSource,
}

#[uniffi::export(async_runtime = "tokio")]
impl LocalSource {
    pub fn identity(&self) -> SourceIdentity {
        self.runtime.identity()
    }
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    /// Ok acknowledges native input admission, not encode or remote delivery.
    pub async fn submit(&self, frame: InputFrame) -> Result<(), NativeError> {
        self.runtime.submit(frame).await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
    pub async fn set_enabled(&self, enabled: bool) -> Result<bool, NativeError> {
        self.runtime.set_enabled(enabled).await.map_err(Into::into)
    }
    /// Actual native capture clock for this live source generation, in microseconds.
    pub async fn capture_time_us(&self) -> Result<i64, NativeError> {
        self.runtime.capture_time_us().await.map_err(Into::into)
    }
    pub async fn encoding_state(&self) -> Result<Vec<EncodingState>, NativeError> {
        self.runtime.encoding_state().await.map_err(Into::into)
    }
    pub async fn set_limits(
        &self,
        limits: Vec<EncodingLimits>,
    ) -> Result<Vec<EncodingState>, NativeError> {
        self.runtime.set_limits(limits).await.map_err(Into::into)
    }
    pub fn feedback(&self) -> Arc<FeedbackStream> {
        Arc::new(FeedbackStream {
            updates: self.runtime.feedback(),
        })
    }
}

#[derive(uniffi::Object)]
pub struct RemoteMediaReceiver {
    runtime: RemoteMedia,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteMediaReceiver {
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
            topology: agent_core::MediaTopology {
                local_video: 1,
                local_audio: 1,
                remote_video: 1,
                remote_audio: 1,
            },
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
        };
        // No connect intent: this is a boundary/policy test, not native media proof.
        let agent = Agent::create(NativeAgentConfig {
            session: session.into(),
            media: MediaConfig::default(),
        })
        .await
        .unwrap();
        let mut desired = core_ffi::DesiredState::from(agent_core::DesiredState::default());
        desired.publications.push(core_ffi::PublicationIntent {
            slot: "unknown".into(),
            active: true,
            label: "test".into(),
        });
        assert!(matches!(
            agent.replace_desired(desired).await,
            Err(NativeError::Core { .. })
        ));
        assert_eq!(agent.snapshot().desired_revision, 0);
        let disconnected = || core_ffi::DesiredState::from(agent_core::DesiredState::default());
        agent.replace_desired(disconnected()).await.unwrap();
        assert_eq!(agent.snapshot().desired_revision, 1);
        agent.close().await.unwrap();
        agent.replace_desired(disconnected()).await.unwrap();
        assert_eq!(agent.snapshot().desired_revision, 3);
        let events = agent.events();
        let snapshots = agent.snapshots();
        agent.abort().await.unwrap();
        assert!(matches!(events.next().await, EventUpdate::Closed));
        assert!(matches!(snapshots.next().await, SnapshotUpdate::Closed));
        assert!(matches!(
            agent.local_audio("a0".into()).await,
            Err(NativeError::Closed)
        ));
    }
}
