//! Generated-language boundary for the shared native actor and owned media.
use std::sync::Arc;

use agent_core::ffi as core_ffi;
use tokio::sync::Mutex;

use crate::{
    Agent as RuntimeAgent, AgentEvent as RuntimeEvent, Config, EncodingLimits, EncodingState,
    Error as RuntimeError, Host, InputFrame, MediaConfig, MediaStreamUpdate, NativeStatistics,
    ObservationStream, ProducerFeedback, Publication as RuntimePublication,
    PublicationInput as RuntimePublicationInput, ReceiveMode, RemoteMedia, SourceIdentity,
    StreamUpdate, Subscription as RuntimeSubscription,
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
    pub async fn publication(
        &self,
        spec: core_ffi::TrackSpec,
    ) -> Result<Arc<Publication>, NativeError> {
        Ok(Arc::new(Publication {
            runtime: self.runtime.publication(spec.into()).await?,
        }))
    }
    pub async fn video_subscription(
        &self,
        selector: core_ffi::TrackSelector,
        observation: core_ffi::VideoObservation,
    ) -> Result<Arc<Subscription>, NativeError> {
        Ok(Arc::new(Subscription {
            runtime: self
                .runtime
                .video_subscription(selector.into(), observation.into())
                .await?,
        }))
    }

    pub async fn audio_subscription(
        &self,
        selector: core_ffi::TrackSelector,
    ) -> Result<Arc<Subscription>, NativeError> {
        Ok(Arc::new(Subscription {
            runtime: self.runtime.audio_subscription(selector.into()).await?,
        }))
    }

    pub async fn audio_policy(
        &self,
        policy: core_ffi::AudioDemand,
    ) -> Result<Arc<Subscription>, NativeError> {
        Ok(Arc::new(Subscription {
            runtime: self.runtime.audio_policy(policy.into()).await?,
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
}

#[derive(uniffi::Object)]
pub struct Publication {
    runtime: RuntimePublication,
}

#[uniffi::export(async_runtime = "tokio")]
impl Publication {
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn input(&self) -> Result<Arc<PublicationInput>, NativeError> {
        Ok(Arc::new(PublicationInput {
            runtime: self.runtime.input().await?,
        }))
    }
    pub async fn set_muted(&self, muted: bool) -> Result<(), NativeError> {
        self.runtime.set_muted(muted).await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct PublicationInput {
    runtime: RuntimePublicationInput,
}

#[uniffi::export(async_runtime = "tokio")]
impl PublicationInput {
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn ready(&self) -> Result<(), NativeError> {
        self.runtime.ready().await.map_err(Into::into)
    }
    /// Ok acknowledges native input admission, not encode or remote delivery.
    pub async fn submit(&self, frame: InputFrame) -> Result<(), NativeError> {
        self.runtime.submit(frame).await.map_err(Into::into)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
    pub async fn capture_time_us(&self) -> Result<i64, NativeError> {
        self.runtime.capture_time_us().await.map_err(Into::into)
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
pub struct Subscription {
    runtime: RuntimeSubscription,
}

#[uniffi::export(async_runtime = "tokio")]
impl Subscription {
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn observe(
        &self,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        Ok(Arc::new(RemoteMediaReceiver {
            runtime: self.runtime.observe(mode).await?,
        }))
    }
    pub async fn observe_source(
        &self,
        source: String,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        Ok(Arc::new(RemoteMediaReceiver {
            runtime: self.runtime.observe_source(source, mode).await?,
        }))
    }
    pub async fn configure_audio_policy(
        &self,
        policy: core_ffi::AudioDemand,
    ) -> Result<(), NativeError> {
        self.runtime
            .configure_audio_policy(policy.into())
            .await
            .map_err(Into::into)
    }
    pub async fn configure_video(
        &self,
        observation: core_ffi::VideoObservation,
    ) -> Result<(), NativeError> {
        self.runtime
            .configure_video(observation.into())
            .await
            .map_err(Into::into)
    }
    pub async fn configure_audio(
        &self,
        playout_delay: core_ffi::PlayoutDelay,
    ) -> Result<(), NativeError> {
        self.runtime
            .configure_audio(playout_delay.into())
            .await
            .map_err(Into::into)
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
            slot: "publication".into(),
            kind: core_ffi::MediaKind::Video,
            active: true,
            label: String::new(),
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
            agent
                .publication(core_ffi::TrackSpec::Audio {
                    label: "microphone".into()
                })
                .await,
            Err(NativeError::Closed)
        ));
    }
}
