//! Generated-language boundary for the shared native actor and owned media.
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use agent_core::ffi as core_ffi;
#[cfg(any(test, feature = "sim"))]
use tokio::sync::Mutex;

mod media;
pub use media::*;
mod collection;
mod frames;
pub use collection::*;
pub use frames::*;
mod resource_state;
pub use resource_state::*;

#[cfg(any(test, feature = "sim"))]
use crate::AgentEvent as RuntimeEvent;

use crate::{
    Agent as RuntimeAgent, Config, EncodingLimits, EncodingState, Error as RuntimeError, Host,
    InputFrame, MediaConfig, MediaStreamUpdate, NativeStatistics, ObservationStream,
    ProducerFeedback, Publication as RuntimePublication, ReceiveMode, RemoteMedia, SourceIdentity,
    StreamUpdate, Subscription as RuntimeSubscription,
};

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct NativeAgentConfig {
    pub session: core_ffi::AgentConfig,
    pub media: MediaConfig,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NativeError {
    #[error("SDK operation failed: {info:?}")]
    Operation { info: core_ffi::ErrorInfo },
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
            RuntimeError::Operation(info) => Self::Operation { info },
            RuntimeError::DataSend(reason) => Self::DataSend { reason },
            RuntimeError::Core(error) => Self::Core {
                error: error.into(),
            },
            RuntimeError::Rtc(message) => Self::Native { message },
            RuntimeError::Closed => Self::Closed,
            RuntimeError::NotReady => Self::NotReady,
            RuntimeError::Backpressure => Self::Backpressure,
            RuntimeError::CapacityExceeded => Self::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::CapacityExceeded,
                    "Frame-source capacity exceeded",
                ),
            },
            RuntimeError::UnknownSlot(slot) => Self::InvalidInput {
                message: format!("unknown media slot: {slot:?}"),
            },
            RuntimeError::InvalidInput(message) => Self::InvalidInput { message },
        }
    }
}

#[cfg(any(test, feature = "sim"))]
#[derive(Clone, Debug, PartialEq)]
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

#[cfg(any(test, feature = "sim"))]
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "Simulation diagnostic outcomes carry snapshots by value"
)]
pub enum SnapshotUpdate {
    Snapshot { snapshot: core_ffi::Snapshot },
    Lagged { skipped: u64 },
    Closed,
}

#[cfg(any(test, feature = "sim"))]
#[derive(Clone, Debug, PartialEq)]
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

fn startup_error() -> NativeError {
    NativeError::Operation {
        info: core_ffi::ErrorInfo::new(
            core_ffi::ErrorCode::Internal,
            "Native Agent startup failed",
        ),
    }
}

fn invalid_agent_config_error() -> NativeError {
    NativeError::Operation {
        info: core_ffi::ErrorInfo::new(
            core_ffi::ErrorCode::InvalidArgument,
            "Invalid Agent configuration",
        ),
    }
}

fn native_host_runtime() -> Result<Arc<tokio::runtime::Runtime>, ()> {
    static RUNTIME: OnceLock<StdMutex<Option<Arc<tokio::runtime::Runtime>>>> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| StdMutex::new(None));
    let mut runtime = runtime.lock().map_err(|_| ())?;
    if let Some(runtime) = runtime.as_ref() {
        return Ok(Arc::clone(runtime));
    }
    let created = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| ())?;
    let created = Arc::new(created);
    *runtime = Some(Arc::clone(&created));
    Ok(created)
}

fn construct_agent(config: NativeAgentConfig) -> Result<Arc<Agent>, NativeError> {
    let session = config
        .session
        .into_core()
        .map_err(|_| invalid_agent_config_error())?;
    let native_runtime = native_host_runtime().map_err(|()| startup_error())?;
    let _entered = native_runtime.handle().enter();
    let client = reqwest::Client::builder()
        .build()
        .map_err(|_| startup_error())?;
    let runtime = RuntimeAgent::spawn_on_handle(
        Config {
            session,
            media: config.media,
            decoded_video_observer: None,
            encoded_video_observer: None,
        },
        Host::new(Box::new(client)),
        native_runtime.handle(),
    )
    .map_err(operation_error)?;
    Ok(Arc::new(Agent { runtime }))
}

#[uniffi::export(async_runtime = "tokio")]
impl Agent {
    #[uniffi::constructor]
    pub fn new(config: core_ffi::AgentConfig) -> Result<Arc<Self>, NativeError> {
        construct_agent(NativeAgentConfig {
            session: config,
            media: MediaConfig::default(),
        })
    }

    #[uniffi::constructor]
    pub fn create_with_media(config: NativeAgentConfig) -> Result<Arc<Self>, NativeError> {
        construct_agent(config)
    }

    pub fn renew(&self, token: String) -> Result<(), NativeError> {
        self.runtime.renew(token).map_err(operation_error)
    }
    pub fn state(&self) -> core_ffi::AgentState {
        self.runtime.state()
    }
    pub fn support_report(&self) -> core_ffi::SupportReport {
        core_ffi::SupportReport {
            connection_id: self.runtime.state().connection_id,
        }
    }

    pub fn updates(&self) -> Result<Arc<AgentUpdates>, NativeError> {
        self.runtime
            .updates()
            .map(|updates| Arc::new(AgentUpdates { updates }))
            .map_err(|_| NativeError::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::CapacityExceeded,
                    "Observer capacity exceeded",
                ),
            })
    }
    pub fn data(&self, track_id: String) -> Result<Arc<LocalDataTrack>, NativeError> {
        Ok(Arc::new(LocalDataTrack {
            runtime: self
                .runtime
                .data(track_id, agent_core::TopicMode::Ordered)?,
        }))
    }
    pub fn room(&self, name: String) -> Result<Arc<RoomRef>, NativeError> {
        Ok(Arc::new(RoomRef {
            runtime: self.runtime.room(name)?,
        }))
    }
    pub fn statistics(&self) -> Option<NativeStatistics> {
        self.runtime.statistics().borrow().clone()
    }
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub fn join(&self, room: String) -> Result<(), NativeError> {
        self.runtime.join(room).map_err(operation_error)
    }

    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

#[cfg(any(test, feature = "sim"))]
impl Agent {
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
}

impl Agent {
    pub fn from_runtime(runtime: RuntimeAgent) -> Arc<Self> {
        Arc::new(Self { runtime })
    }
}

fn operation_error(error: RuntimeError) -> NativeError {
    NativeError::Operation {
        info: error.error_info(),
    }
}

#[derive(uniffi::Object)]
pub struct AgentUpdates {
    updates: agent_core::observer::AgentUpdates,
}

#[uniffi::export(async_runtime = "tokio")]
impl AgentUpdates {
    pub async fn next(&self) -> Result<Option<core_ffi::AgentState>, NativeError> {
        let next = self.updates.next().map_err(|_| NativeError::Operation {
            info: core_ffi::ErrorInfo::new(
                core_ffi::ErrorCode::Conflict,
                "An observation read is already pending",
            ),
        })?;
        Ok(next.await)
    }

    pub fn close(&self) {
        self.updates.close();
    }
}

#[derive(uniffi::Object)]
pub struct LocalDataTrack {
    runtime: crate::LocalDataTrack,
}

#[uniffi::export]
impl LocalDataTrack {
    #[uniffi::method(default(options = None))]
    pub fn publish(
        &self,
        options: Option<core_ffi::DataPublishOptions>,
    ) -> Result<Arc<DataPublication>, NativeError> {
        let options = options.unwrap_or_default();
        Ok(Arc::new(DataPublication {
            runtime: self.runtime.publish_with_options(
                options.delivery.unwrap_or_default().into(),
                options.capture_clock,
                options.record_dd.unwrap_or(false),
            )?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct DataPublication {
    runtime: crate::DataPublication,
}

#[uniffi::export(async_runtime = "tokio")]
impl DataPublication {
    pub fn state(&self) -> core_ffi::DataPublicationState {
        self.runtime.state()
    }
    pub fn updates(&self) -> Result<Arc<DataPublicationUpdates>, NativeError> {
        Ok(Arc::new(DataPublicationUpdates {
            runtime: self
                .runtime
                .updates()
                .map_err(resource_state::observer_limit)?,
        }))
    }
    #[uniffi::method(default(options = None))]
    pub async fn send(
        &self,
        data: Vec<u8>,
        options: Option<core_ffi::DataSendOptions>,
    ) -> Result<(), NativeError> {
        self.runtime
            .send_at(data, options)
            .await
            .map_err(operation_error)
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().map_err(operation_error)
    }
}

#[derive(uniffi::Object)]
pub struct RoomRef {
    runtime: crate::RoomRef,
}

#[uniffi::export]
impl RoomRef {
    #[uniffi::method(default(config = None))]
    pub fn watch(&self, config: Option<core_ffi::WatchConfig>) -> Result<Arc<Watch>, NativeError> {
        Ok(Arc::new(Watch {
            room: self.runtime.clone(),
            runtime: self
                .runtime
                .watch(config.unwrap_or_default())
                .map_err(operation_error)?,
        }))
    }

    pub fn name(&self) -> String {
        self.runtime.name().into()
    }

    pub fn participant(&self, id: String) -> Result<Arc<Participant>, NativeError> {
        Ok(Arc::new(Participant {
            runtime: self.runtime.participant(id)?,
        }))
    }
}

#[uniffi::export]
impl RoomRef {
    #[uniffi::method(default(filter = None))]
    pub fn data(&self, filter: Option<agent_core::TrackFilter>) -> Arc<DataSetRef> {
        Arc::new(DataSetRef {
            room: self.runtime.clone(),
            filter: filter.unwrap_or_default(),
        })
    }
}

#[derive(uniffi::Object)]
pub struct DataSetRef {
    room: crate::RoomRef,
    filter: agent_core::TrackFilter,
}

#[uniffi::export]
impl DataSetRef {
    pub fn subscribe(&self) -> Result<Arc<DataSubscription>, NativeError> {
        Ok(Arc::new(DataSubscription {
            runtime: self
                .room
                .subscribe_data(self.filter.clone())
                .map_err(operation_error)?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct Watch {
    runtime: crate::Watch,
    room: crate::RoomRef,
}

#[uniffi::export(async_runtime = "tokio")]
impl Watch {
    pub fn state(&self) -> CatalogWatchState {
        CatalogWatchState::new(self.runtime.state(), &self.room)
    }
    pub fn updates(&self) -> Result<Arc<WatchUpdates>, NativeError> {
        Ok(Arc::new(WatchUpdates {
            room: self.room.clone(),
            runtime: self
                .runtime
                .updates()
                .map_err(resource_state::observer_limit)?,
        }))
    }
    pub async fn close(&self) {
        self.runtime.close();
    }
}

#[derive(uniffi::Record)]
pub struct CatalogWatchState {
    pub observation_revision: u64,
    pub room: String,
    pub limit: u32,
    pub cursor: Vec<u8>,
    pub status: core_ffi::WatchStatus,
    pub participants: Option<Vec<Arc<CatalogParticipant>>>,
    pub has_more: Option<bool>,
    pub next_cursor: Option<Vec<u8>>,
    pub cursor_expires_at_unix_seconds: Option<i64>,
    pub error: Option<core_ffi::ErrorInfo>,
}

impl CatalogWatchState {
    fn new(state: core_ffi::WatchState, room: &crate::RoomRef) -> Self {
        Self {
            observation_revision: state.observation_revision,
            room: state.room,
            limit: state.limit,
            cursor: state.cursor,
            status: state.status,
            participants: state.page.as_ref().map(|page| {
                page.participants()
                    .into_iter()
                    .map(|entry| {
                        Arc::new(CatalogParticipant {
                            entry,
                            room: room.clone(),
                        })
                    })
                    .collect()
            }),
            has_more: state.page.as_ref().map(|page| page.has_more()),
            next_cursor: state.page.as_ref().map(|page| page.next_cursor()),
            cursor_expires_at_unix_seconds: state
                .page
                .as_ref()
                .map(|page| page.cursor_expires_at_unix_seconds()),
            error: state.error,
        }
    }
}

#[derive(uniffi::Object)]
pub struct CatalogParticipant {
    entry: core_ffi::CatalogParticipantInfo,
    room: crate::RoomRef,
}

#[uniffi::export]
impl CatalogParticipant {
    pub fn id(&self) -> String {
        self.entry.id.clone()
    }

    pub fn tracks(&self) -> Vec<core_ffi::CatalogPublicationInfo> {
        self.entry.publications.clone()
    }

    pub fn video(&self, label: String) -> Result<Arc<media::RemoteVideoTrack>, NativeError> {
        Participant {
            runtime: self.room.participant(self.entry.id.clone())?,
        }
        .video(label)
    }

    pub fn audio(&self, label: String) -> Result<Arc<media::RemoteAudioTrack>, NativeError> {
        Participant {
            runtime: self.room.participant(self.entry.id.clone())?,
        }
        .audio(label)
    }

    pub fn data(&self, label: String) -> Result<Arc<RemoteDataTrack>, NativeError> {
        Participant {
            runtime: self.room.participant(self.entry.id.clone())?,
        }
        .data(label)
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
    pub fn data(&self, track_id: String) -> Result<Arc<RemoteDataTrack>, NativeError> {
        Ok(Arc::new(RemoteDataTrack {
            runtime: self.runtime.data(track_id)?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteDataTrack {
    runtime: crate::RemoteDataTrack,
}

#[uniffi::export]
impl RemoteDataTrack {
    pub fn subscribe(&self) -> Result<Arc<DataSubscription>, NativeError> {
        Ok(Arc::new(DataSubscription {
            runtime: self.runtime.subscribe()?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct DataSubscription {
    runtime: crate::DataReceiver,
}

#[uniffi::export(async_runtime = "tokio")]
impl DataSubscription {
    pub fn state(&self) -> core_ffi::DataSubscriptionState {
        self.runtime.state()
    }
    pub fn updates(&self) -> Result<Arc<DataSubscriptionUpdates>, NativeError> {
        Ok(Arc::new(DataSubscriptionUpdates {
            runtime: self
                .runtime
                .updates()
                .map_err(resource_state::observer_limit)?,
        }))
    }
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn next(&self) -> Result<Option<agent_core::DataEvent>, NativeError> {
        match self.runtime.next().await.map_err(operation_error)? {
            agent_core::DataUpdate::Event { event } => Ok(Some(event)),
            agent_core::DataUpdate::Closed => Ok(None),
        }
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

#[cfg(any(test, feature = "sim"))]
pub struct SnapshotStream {
    initial: Mutex<Option<agent_core::Snapshot>>,
    updates: ObservationStream<agent_core::Snapshot>,
}
#[cfg(any(test, feature = "sim"))]
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

#[cfg(any(test, feature = "sim"))]
pub struct EventStream {
    updates: ObservationStream<RuntimeEvent>,
}
#[cfg(any(test, feature = "sim"))]
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
        let agent = Agent::new(session.into()).unwrap();
        let mut desired = core_ffi::DesiredState::from(agent_core::DesiredState::default());
        desired.publications.push(core_ffi::PublicationIntent {
            slot: "publication".into(),
            kind: core_ffi::MediaKind::Video,
            active: true,
            track_id: String::new(),
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
        assert!(matches!(
            agent.join("room".into()),
            Err(NativeError::Operation { info }) if info.code == core_ffi::ErrorCode::Closed
        ));
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
