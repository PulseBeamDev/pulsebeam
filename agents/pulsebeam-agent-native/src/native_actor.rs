//! Shared application actor. Runtime selection changes services, not RTC/media policy.
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use agent_core::{
    AgentCommand, DesiredState, Effect, HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod,
    MediaSlot, Notification, OperationId, RtcEffect, RtcEvent, Snapshot, SourceReadiness,
    TimerEffect, TimerEvent, TimerId, TopicSend,
};
use pulsebeam_core::net::AsyncHttpClient;
use pulsebeam_webrtc_sys as rtc;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::native_media::*;
use crate::native_peer::{NativePeer, Observation};

mod publication;
pub use publication::{Publication, PublicationInput};
use publication::{PublicationLease, RegisteredInput};
mod subscription;
pub use subscription::Subscription;
use subscription::{ObservationIo, SubscriptionLease};

const COMMANDS: usize = 64;
const MEDIA_COMMANDS: usize = 4;
const EVENTS: usize = 64;
const STREAM_FRAMES: usize = 4;
const OBSERVERS: usize = 32;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
// Never overwrite a live consumer solely within one actor turn. Reserve one
// channel entry for each possible SDK-drop observation, then yield before
// draining the next native batch. Combined mode has two independent counters.
// Queue memory limits remain unchanged.
const FRAME_BUDGET: usize = STREAM_FRAMES - 1;
const COMBINED_FRAME_BUDGET: usize = STREAM_FRAMES - 2;
const PLAYOUT: Duration = Duration::from_millis(10);
const STATS_INTERVAL: Duration = Duration::from_millis(200);
// Bound the host I/O future, including connect, headers and body. A timeout is
// an ordinary HTTP failure for core's existing cancellation/retry policy.
const SIGNALING_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub session: agent_core::AgentConfig,
    pub media: MediaConfig,
    /// Rust-only callback before polling retention. Defaults to no observer.
    pub decoded_video_observer: Option<crate::DecodedVideoObserver>,
    /// Rust-only combined-mode observation before encoded polling retention.
    /// Requires a decoded observer. Defaults to no encoded observer.
    pub encoded_video_observer: Option<crate::EncodedVideoObserver>,
}
impl Config {
    pub fn new(session: agent_core::AgentConfig) -> Self {
        Self {
            session,
            media: MediaConfig::default(),
            decoded_video_observer: None,
            encoded_video_observer: None,
        }
    }
}

/// HTTP services shared by production and managed actors.
/// Requests use a ten-second actor-clock deadline. Expiry drops the host I/O
/// future and reports an ordinary HTTP failure to core's existing retry policy.
pub struct Host {
    http: Arc<dyn AsyncHttpClient>,
}
impl Host {
    pub fn new(http: Box<dyn AsyncHttpClient>) -> Self {
        Self {
            http: Arc::from(http),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    Core(Notification),
    ProducerFeedback {
        slot: MediaSlot,
        source: SourceIdentity,
        feedback: ProducerFeedback,
    },
    ChannelMessagesDropped {
        generation: u64,
        channel: u64,
        messages: u64,
        bytes: u64,
    },
    RuntimeFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] agent_core::AgentError),
    #[error("native RTC error: {0}")]
    Rtc(String),
    #[error("agent or media resource is closed")]
    Closed,
    #[error("native media target is not available yet")]
    NotReady,
    #[error("bounded admission queue is full")]
    Backpressure,
    #[error("unknown media slot: {0:?}")]
    UnknownSlot(MediaSlot),
    #[error("invalid media input: {0}")]
    InvalidInput(String),
}
impl From<rtc::PeerError> for Error {
    fn from(error: rtc::PeerError) -> Self {
        if error.kind == rtc::PeerErrorKind::Closed {
            Self::Closed
        } else {
            Self::Rtc(format!("{:?}: {}", error.kind, error.message))
        }
    }
}

#[derive(Default)]
struct Signal {
    closed: AtomicBool,
    changed: Notify,
}
impl Signal {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_closed() {
                return;
            }
            changed.await;
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum StreamUpdate<T> {
    Value(T),
    Lagged { skipped: u64 },
    Closed,
}

/// Bounded observations with an explicit terminal signal even while a retained
/// application handle still owns a broadcast sender.
pub struct ObservationStream<T: Clone> {
    receiver: Mutex<broadcast::Receiver<T>>,
    closed: Arc<Signal>,
}
impl<T: Clone> ObservationStream<T> {
    pub fn is_closed(&self) -> bool {
        self.closed.is_closed()
    }
    /// Nonblocking observation for application loops. `None` means no update
    /// is ready (or another reader holds the stream), never terminal closure.
    pub fn try_next(&self) -> Option<StreamUpdate<T>> {
        let mut receiver = self.receiver.try_lock().ok()?;
        if self.closed.is_closed() {
            *receiver = receiver.resubscribe();
            return Some(StreamUpdate::Closed);
        }
        let result = match receiver.try_recv() {
            Ok(value) => Some(StreamUpdate::Value(value)),
            Err(broadcast::error::TryRecvError::Empty) => None,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                Some(StreamUpdate::Lagged { skipped })
            }
            Err(broadcast::error::TryRecvError::Closed) => Some(StreamUpdate::Closed),
        };
        if self.closed.is_closed() {
            *receiver = receiver.resubscribe();
            Some(StreamUpdate::Closed)
        } else {
            result
        }
    }

    pub async fn next(&self) -> StreamUpdate<T> {
        let mut receiver = self.receiver.lock().await;
        if self.closed.is_closed() {
            *receiver = receiver.resubscribe();
            return StreamUpdate::Closed;
        }
        let result = tokio::select! {
            _ = self.closed.wait() => { *receiver = receiver.resubscribe(); return StreamUpdate::Closed; },
            result = receiver.recv() => result,
        };
        if self.closed.is_closed() {
            *receiver = receiver.resubscribe();
            return StreamUpdate::Closed;
        }
        match result {
            Ok(value) => StreamUpdate::Value(value),
            Err(broadcast::error::RecvError::Lagged(skipped)) => StreamUpdate::Lagged { skipped },
            Err(broadcast::error::RecvError::Closed) => StreamUpdate::Closed,
        }
    }
}

struct AgentLease {
    abort: Arc<Signal>,
    finished: Arc<Signal>,
}
impl Drop for AgentLease {
    fn drop(&mut self) {
        self.abort.close();
    }
}

enum Command {
    ForeignDesired {
        desired: agent_core::ffi::DesiredState,
        response: oneshot::Sender<Result<(), agent_core::ffi::AgentError>>,
    },
    Core {
        command: AgentCommand,
        response: oneshot::Sender<Result<(), Error>>,
    },
    Limits {
        control: Arc<agent_core::PublicationInputControl>,
        slot: MediaSlot,
        identity: SourceIdentity,
        limits: Vec<EncodingLimits>,
        response: oneshot::Sender<Result<Vec<EncodingState>, Error>>,
    },
    CaptureTime {
        slot: MediaSlot,
        identity: SourceIdentity,
        response: oneshot::Sender<Result<i64, Error>>,
    },
    Keyframe {
        slot: MediaSlot,
        generation: u64,
        receiver: u64,
        response: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        response: oneshot::Sender<Result<(), Error>>,
    },
    Publication {
        spec: agent_core::TrackSpec,
        response: oneshot::Sender<Result<Publication, Error>>,
    },
    VideoSubscription {
        selector: agent_core::TrackSelector,
        observation: agent_core::VideoObservation,
        response: oneshot::Sender<Result<Subscription, Error>>,
    },
    AudioSubscription {
        selector: agent_core::TrackSelector,
        response: oneshot::Sender<Result<Subscription, Error>>,
    },
    AudioPolicy {
        policy: agent_core::AudioSubscription,
        response: oneshot::Sender<Result<Subscription, Error>>,
    },
    ObserveSubscription {
        owner: u64,
        source: Option<String>,
        mode: ReceiveMode,
        response: oneshot::Sender<Result<RemoteMedia, Error>>,
    },
    PublicationInput {
        owner: u64,
        response: oneshot::Sender<Result<PublicationInput, Error>>,
    },
    ClosePublicationInput {
        operation: agent_core::PublicationInputOperation,
        response: oneshot::Sender<Result<(), Error>>,
    },
    Resource {
        command: agent_core::ResourceCommand,
        response: oneshot::Sender<Result<agent_core::ResourceResult, Error>>,
    },
}
struct Submission {
    control: Arc<agent_core::PublicationInputControl>,
    slot: MediaSlot,
    identity: SourceIdentity,
    frame: InputFrame,
    response: oneshot::Sender<Result<(), Error>>,
}

#[derive(Clone)]
pub struct Agent {
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<Snapshot>,
    snapshot_events: broadcast::Sender<Snapshot>,
    statistics: watch::Receiver<Option<NativeStatistics>>,
    events: broadcast::Sender<AgentEvent>,
    lease: Arc<AgentLease>,
}

impl Agent {
    pub async fn spawn(config: Config, host: Host) -> Result<Self, Error> {
        let (agent, actor) = Self::parts(config, host, || Ok((rtc::ThreadedRuntime, ())))?;
        tokio::spawn(actor.run());
        Ok(agent)
    }

    /// The caller launches and drives this non-Send future on its controlled
    /// world's OS thread. Native pumping, packet delivery and virtual deadlines
    /// belong to that world's existing outer driver, not a second RTC backend.
    pub fn managed<F>(
        config: Config,
        host: Host,
        mut runtime: F,
    ) -> Result<(Self, impl Future<Output = ()> + 'static), Error>
    where
        F: FnMut() -> Result<rtc::ManagedRuntime, Error> + 'static,
    {
        Self::managed_with_services(config, host, move || runtime().map(|runtime| (runtime, ())))
    }

    /// Retain external runtime services until their native graph has been
    /// destroyed. The owner may be thread-affine, like the managed runtime.
    /// Neither a reconnect nor constructor failure retains a retired owner.
    pub fn managed_with_services<F, O>(
        config: Config,
        host: Host,
        runtime: F,
    ) -> Result<(Self, impl Future<Output = ()> + 'static), Error>
    where
        F: FnMut() -> Result<(rtc::ManagedRuntime, O), Error> + 'static,
        O: 'static,
    {
        let (agent, actor) = Self::parts(config, host, runtime)?;
        Ok((agent, actor.run()))
    }

    fn parts<R, F, O>(
        config: Config,
        host: Host,
        runtime: F,
    ) -> Result<(Self, Actor<R, F, O>), Error>
    where
        R: rtc::SessionRuntime,
        F: FnMut() -> Result<(R, O), Error>,
    {
        let core = agent_core::Agent::new(config.session.clone())?;
        let (commands, command_rx) = mpsc::channel(COMMANDS);
        let (media, media_rx) = mpsc::channel(MEDIA_COMMANDS);
        let (snapshot_tx, snapshot) = watch::channel(core.snapshot().clone());
        let (snapshot_events, _) = broadcast::channel(EVENTS);
        let (statistics_tx, statistics) = watch::channel(None);
        let (events, _) = broadcast::channel(EVENTS);
        let abort = Arc::new(Signal::default());
        let finished = Arc::new(Signal::default());
        let (http_tx, http_rx) = mpsc::channel(COMMANDS);
        let agent = Self {
            commands: commands.clone(),
            snapshot,
            snapshot_events: snapshot_events.clone(),
            statistics,
            events: events.clone(),
            lease: Arc::new(AgentLease {
                abort: abort.clone(),
                finished: finished.clone(),
            }),
        };
        let actor = Actor {
            config,
            core,
            peer: None,
            peer_owner: None,
            runtime,
            http: host.http,
            commands: command_rx,
            command_tx: commands,
            media: media_rx,
            media_tx: media,
            snapshot: snapshot_tx,
            snapshot_events,
            statistics: statistics_tx,
            events,
            abort,
            finished,
            leases_changed: Arc::new(Notify::new()),
            sources: BTreeMap::new(),
            publications: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            observations: BTreeMap::new(),
            inputs: BTreeMap::new(),
            streams: BTreeMap::new(),
            http_tasks: BTreeMap::new(),
            http_tx,
            http_rx,
            timers: BTreeMap::new(),
            close_waiters: Vec::new(),
            next_playout: None,
            next_stats: None,
        };
        Ok((agent, actor))
    }

    async fn command(&self, command: AgentCommand) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Core { command, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
    // Foreign callers submit complete states without revisions. Assign them in
    // the actor's control order, including close and Rust-originated commands.
    pub(crate) async fn replace_foreign_desired(
        &self,
        desired: agent_core::ffi::DesiredState,
    ) -> Result<(), agent_core::ffi::AgentError> {
        let closed = || agent_core::ffi::AgentError {
            code: agent_core::ffi::ErrorCode::Closed,
            message: "agent is closed".into(),
        };
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::ForeignDesired { desired, response })
            .await
            .map_err(|_| closed())?;
        result.await.map_err(|_| closed())?
    }

    pub async fn video_subscription(
        &self,
        selector: agent_core::TrackSelector,
        observation: agent_core::VideoObservation,
    ) -> Result<Subscription, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::VideoSubscription {
                selector,
                observation,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn audio_subscription(
        &self,
        selector: agent_core::TrackSelector,
    ) -> Result<Subscription, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::AudioSubscription { selector, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn audio_policy(
        &self,
        policy: agent_core::AudioSubscription,
    ) -> Result<Subscription, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::AudioPolicy { policy, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn publication(&self, spec: agent_core::TrackSpec) -> Result<Publication, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Publication { spec, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    #[cfg(feature = "sim")]
    pub async fn resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Resource { command, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn replace_desired(&self, desired: DesiredState) -> Result<(), Error> {
        self.command(AgentCommand::ReplaceDesired(desired)).await
    }
    pub async fn renew_authorization(&self, token: String) -> Result<(), Error> {
        self.command(AgentCommand::RenewAuthorization(token)).await
    }
    pub async fn send_topic(&self, send: TopicSend) -> Result<(), Error> {
        self.command(AgentCommand::SendTopic(send)).await
    }
    pub async fn reconnect(&self) -> Result<(), Error> {
        self.command(AgentCommand::Reconnect).await
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }
    pub fn snapshots(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }
    pub fn snapshot_events(&self) -> (Snapshot, ObservationStream<Snapshot>) {
        let receiver = self.snapshot_events.subscribe();
        (
            self.snapshot(),
            ObservationStream {
                receiver: Mutex::new(receiver),
                closed: self.lease.finished.clone(),
            },
        )
    }
    pub fn events(&self) -> ObservationStream<AgentEvent> {
        ObservationStream {
            receiver: Mutex::new(self.events.subscribe()),
            closed: self.lease.finished.clone(),
        }
    }
    pub fn statistics(&self) -> watch::Receiver<Option<NativeStatistics>> {
        self.statistics.clone()
    }
    pub async fn close(&self) -> Result<(), Error> {
        self.begin_close().await?.await.map_err(|_| Error::Closed)?
    }
    pub(crate) async fn begin_close(&self) -> Result<oneshot::Receiver<Result<(), Error>>, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Close { response })
            .await
            .map_err(|_| Error::Closed)?;
        Ok(result)
    }
    pub async fn abort(&self) -> Result<(), Error> {
        self.lease.abort.close();
        self.lease.finished.wait().await;
        Ok(())
    }
}

struct SourceLease {
    slot: MediaSlot,
    identity: SourceIdentity,
    closed: Arc<Signal>,
    leases_changed: Arc<Notify>,
    commands: mpsc::Sender<Command>,
    media: mpsc::Sender<Submission>,
}
impl Drop for SourceLease {
    fn drop(&mut self) {
        self.closed.close();
        self.leases_changed.notify_one();
    }
}
#[derive(Clone)]
struct PhysicalSource {
    inner: Arc<SourceLease>,
}
impl PhysicalSource {
    fn identity(&self) -> SourceIdentity {
        self.inner.identity.clone()
    }
    fn slot(&self) -> &MediaSlot {
        &self.inner.slot
    }
    fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
    }
    async fn submit(
        &self,
        frame: InputFrame,
        control: Arc<agent_core::PublicationInputControl>,
    ) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if matches!(&frame, InputFrame::H264Simulcast { frames } if frames.len() != 3) {
            return Err(Error::InvalidInput(
                "simulcast requires three genuine encoding-specific units".into(),
            ));
        }
        let bytes = match &frame {
            InputFrame::Pcm { frame } => frame.samples.len().saturating_mul(2),
            InputFrame::I420 { frame } => frame.data.len(),
            InputFrame::Opus { frame } => frame.payload.len(),
            InputFrame::H264 { frame } => frame.payload.len(),
            InputFrame::H264Simulcast { frames } => frames
                .iter()
                .fold(0usize, |n, frame| n.saturating_add(frame.payload.len())),
        };
        if bytes > MAX_FRAME_BYTES {
            return Err(Error::InvalidInput(
                "frame exceeds the owned input byte limit".into(),
            ));
        }
        let (response, result) = oneshot::channel();
        self.inner
            .media
            .try_send(Submission {
                control,
                slot: self.inner.slot.clone(),
                identity: self.identity(),
                frame,
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Backpressure,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        result.await.map_err(|_| Error::Closed)?
    }
    async fn set_limits(
        &self,
        limits: Vec<EncodingLimits>,
        control: Arc<agent_core::PublicationInputControl>,
    ) -> Result<Vec<EncodingState>, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if limits.len() > 3 {
            return Err(Error::InvalidInput("at most three encoding limits".into()));
        }
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Limits {
                control,
                slot: self.inner.slot.clone(),
                identity: self.identity(),
                limits,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
    /// Read the owning graph's actual native capture clock. Anchor a relative
    /// video timeline here once, then add its strictly increasing capture offsets.
    /// Replaced or retired source handles cannot read a newer graph's clock.
    async fn capture_time_us(&self) -> Result<i64, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::CaptureTime {
                slot: self.inner.slot.clone(),
                identity: self.identity(),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

struct StreamLease {
    closed: Arc<Signal>,
    leases_changed: Arc<Notify>,
    receiver: Mutex<broadcast::Receiver<MediaStreamUpdate>>,
}
impl StreamLease {
    fn close(&self) {
        self.closed.close();
        if let Ok(mut receiver) = self.receiver.try_lock() {
            *receiver = receiver.resubscribe();
        }
    }
}
impl Drop for StreamLease {
    fn drop(&mut self) {
        self.leases_changed.notify_one();
    }
}
pub struct RemoteMedia {
    observation: Option<agent_core::MediaObservationControl>,
    slot: MediaSlot,
    generation: u64,
    receiver: u64,
    lease: Arc<StreamLease>,
    commands: mpsc::Sender<Command>,
}
impl RemoteMedia {
    pub fn source_track_id(&self) -> Option<&str> {
        self.observation
            .as_ref()?
            .target
            .as_ref()
            .map(|target| target.source.as_str())
    }

    pub fn slot(&self) -> &MediaSlot {
        &self.slot
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn receiver(&self) -> u64 {
        self.receiver
    }
    pub async fn next(&self) -> MediaStreamUpdate {
        let mut receiver = self.lease.receiver.lock().await;
        if self.lease.closed.is_closed() {
            *receiver = receiver.resubscribe();
            return MediaStreamUpdate::Closed;
        }
        let value = tokio::select! {
            _ = self.lease.closed.wait() => { *receiver = receiver.resubscribe(); return MediaStreamUpdate::Closed; },
            result = receiver.recv() => result,
        };
        // Closing a generation invalidates buffered observations as well as
        // callbacks. Values already returned to the application remain owned.
        if self.lease.closed.is_closed() {
            *receiver = receiver.resubscribe();
            return MediaStreamUpdate::Closed;
        }
        match value {
            Ok(value) => value,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                MediaStreamUpdate::Lagged { skipped }
            }
            Err(broadcast::error::RecvError::Closed) => MediaStreamUpdate::Closed,
        }
    }
    pub async fn close(&self) {
        self.lease.close();
        self.lease.leases_changed.notify_one();
    }
    pub async fn request_keyframe(&self) -> Result<(), Error> {
        if self.lease.closed.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Keyframe {
                slot: self.slot.clone(),
                generation: self.generation,
                receiver: self.receiver,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

struct SourceObservation {
    lease: Weak<SourceLease>,
    identity: SourceIdentity,
    last_rates: Option<VideoRateControl>,
}
struct StreamObservation {
    mode: ReceiveMode,
    generation: u64,
    receiver: u64,
    sender: broadcast::Sender<MediaStreamUpdate>,
    leases: Vec<Weak<StreamLease>>,
    native_dropped: u64,
    native_decoded_dropped: u64,
}
struct HttpCompletion {
    operation: OperationId,
    result: Result<agent_core::HttpResponse, String>,
}

struct Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    config: Config,
    core: agent_core::Agent,
    // A generation is destroyed before the factory closure (which may own the
    // controlled world's affine services). There is no closed-peer history.
    peer: Option<NativePeer<R>>,
    peer_owner: Option<O>,
    runtime: F,
    http: Arc<dyn AsyncHttpClient>,
    commands: mpsc::Receiver<Command>,
    command_tx: mpsc::Sender<Command>,
    media: mpsc::Receiver<Submission>,
    media_tx: mpsc::Sender<Submission>,
    snapshot: watch::Sender<Snapshot>,
    snapshot_events: broadcast::Sender<Snapshot>,
    statistics: watch::Sender<Option<NativeStatistics>>,
    events: broadcast::Sender<AgentEvent>,
    abort: Arc<Signal>,
    finished: Arc<Signal>,
    leases_changed: Arc<Notify>,
    sources: BTreeMap<MediaSlot, SourceObservation>,
    publications: BTreeMap<u64, Weak<PublicationLease>>,
    inputs: BTreeMap<u64, RegisteredInput>,
    subscriptions: BTreeMap<u64, Weak<SubscriptionLease>>,
    observations: BTreeMap<agent_core::MediaObservationId, ObservationIo>,
    streams: BTreeMap<MediaSlot, StreamObservation>,
    http_tasks: BTreeMap<OperationId, JoinHandle<()>>,
    http_tx: mpsc::Sender<HttpCompletion>,
    http_rx: mpsc::Receiver<HttpCompletion>,
    timers: BTreeMap<TimerId, Instant>,
    close_waiters: Vec<oneshot::Sender<Result<(), Error>>>,
    next_playout: Option<Instant>,
    next_stats: Option<Instant>,
}

async fn deadline(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    async fn run(mut self) {
        loop {
            if self.abort.is_closed() {
                break;
            }
            self.retire_dropped();
            self.drain_effects();
            let more = self.progress_peer();
            self.drain_effects();
            self.progress_observations();
            self.drain_effects();
            self.publish().await;
            if more {
                tokio::task::yield_now().await;
                continue;
            }
            let timer = self.timers.values().copied().min();
            tokio::select! {
                _ = self.abort.wait() => break,
                _ = self.leases_changed.notified() => {},
                _ = std::future::poll_fn(|cx| match self.peer.as_mut() { Some(peer) => peer.poll_ready(cx), None => std::task::Poll::Pending }) => {},
                command = self.commands.recv() => match command { Some(command) => self.command(command), None => break },
                submission = self.media.recv() => if let Some(submission) = submission {
                    let result = if !self.input_submission_current(&submission.control) {
                        Err(Error::Closed)
                    } else {
                        self.peer.as_mut().ok_or(Error::Closed).and_then(|peer| peer.submit(&submission.slot, &submission.identity, submission.frame).map_err(Error::from))
                    };
                    let _ = submission.response.send(result);
                },
                completion = self.http_rx.recv() => if let Some(completion) = completion {
                    self.http_tasks.remove(&completion.operation);
                    self.accept(HostEvent::Http(match completion.result {
                        Ok(response) => HttpEvent::Response { operation: completion.operation, response },
                        Err(message) => HttpEvent::Failed { operation: completion.operation, message },
                    }));
                },
                _ = deadline(timer) => {
                    let now = Instant::now();
                    let due: Vec<_> = self.timers.iter().filter(|(_, at)| **at <= now).map(|(&id, _)| id).collect();
                    for timer in due { self.timers.remove(&timer); self.accept(HostEvent::Timer(TimerEvent::Fired { timer })); }
                },
                _ = deadline(self.next_playout) => {
                    self.next_playout = self.later(PLAYOUT);
                    self.frames(true);
                },
                _ = deadline(self.next_stats) => {
                    self.next_stats = self.later(STATS_INTERVAL);
                    if let Some(peer) = &mut self.peer && let Err(error) = peer.request_stats() { self.fail(Error::from(error)); }
                },
            }
        }
        self.shutdown();
    }

    fn accept(&mut self, event: HostEvent) {
        #[cfg(feature = "sim")]
        if let HostEvent::Rtc(event) = &event {
            let (generation, kind) = match event {
                RtcEvent::OfferCreated { generation, .. } => (generation, "offer_created"),
                RtcEvent::AnswerApplied { generation } => (generation, "answer_applied"),
                RtcEvent::MediaPrepared { generation, .. } => (generation, "media_prepared"),
                RtcEvent::MediaObserved { generation, .. } => (generation, "media_observed"),
                RtcEvent::BootstrapMediaPrepared { generation, .. } => {
                    (generation, "bootstrap_media_prepared")
                }
                RtcEvent::BootstrapRejected { generation, .. } => {
                    (generation, "bootstrap_rejected")
                }
                RtcEvent::Connected { generation } => (generation, "connected"),
                RtcEvent::Disconnected { generation } => (generation, "disconnected"),
                RtcEvent::Failed { generation, .. } => (generation, "failed"),
                RtcEvent::Closed { generation } => (generation, "closed"),
            };
            // Inputs only: a stale generation may legitimately be ignored by core.
            tracing::debug!(target: "pulsebeam_sim_recovery", ?generation, kind,
                "native core RTC input");
        }
        if let Err(error) = self.core.handle(event) {
            self.fail(Error::Core(error));
        }
    }
    fn fail(&self, error: Error) {
        let _ = self
            .events
            .send(AgentEvent::RuntimeFailed(error.to_string()));
    }
    fn later(&self, after: Duration) -> Option<Instant> {
        let at = Instant::now().checked_add(after);
        if at.is_none() {
            self.fail(Error::Rtc(
                "native actor deadline exceeds the host clock range".into(),
            ));
            self.abort.close();
        }
        at
    }
    fn peer_mut(&mut self) -> Result<&mut NativePeer<R>, Error> {
        self.peer.as_mut().ok_or(Error::NotReady)
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::ForeignDesired { desired, response } => {
                let result = self
                    .core
                    .snapshot()
                    .desired_revision
                    .checked_add(1)
                    .ok_or_else(|| agent_core::ffi::AgentError {
                        code: agent_core::ffi::ErrorCode::InvalidDesiredState,
                        message: "desired revision space exhausted".into(),
                    })
                    .and_then(|revision| desired.into_core(revision))
                    .and_then(|desired| {
                        self.core
                            .command(AgentCommand::ReplaceDesired(desired))
                            .map_err(Into::into)
                    });
                let _ = response.send(result);
            }
            Command::Core { command, response } => {
                let _ = response.send(self.core.command(command).map_err(Error::Core));
            }
            Command::Limits {
                control,
                slot,
                identity,
                limits,
                response,
            } => {
                let result = if !self.input_submission_current(&control) {
                    Err(Error::Closed)
                } else {
                    self.peer_mut().and_then(|peer| {
                        peer.set_limits(&slot, &identity, &limits)
                            .map_err(Error::from)
                    })
                };
                let _ = response.send(result);
            }
            Command::CaptureTime {
                slot,
                identity,
                response,
            } => {
                let result = self
                    .peer_mut()
                    .and_then(|peer| peer.capture_time_us(&slot, &identity).map_err(Error::from));
                let _ = response.send(result);
            }
            Command::Keyframe {
                slot,
                generation,
                receiver,
                response,
            } => {
                let result = self.peer_mut().and_then(|peer| {
                    if peer.generation.get() != generation
                        || !peer
                            .receivers
                            .get(&slot)
                            .is_some_and(|value| value.id.get() == receiver)
                    {
                        Err(Error::Closed)
                    } else {
                        peer.request_keyframe(&slot).map_err(Error::from)
                    }
                });
                let _ = response.send(result);
            }
            Command::Close { response } => {
                if self.close_waiters.len() >= COMMANDS {
                    let _ = response.send(Err(Error::Backpressure));
                    return;
                }
                let desired = DesiredState {
                    revision: self.core.snapshot().desired_revision.saturating_add(1),
                    connected: false,
                    ..DesiredState::default()
                };
                match self.core.command(AgentCommand::ReplaceDesired(desired)) {
                    Ok(()) => self.close_waiters.push(response),
                    Err(error) => {
                        let _ = response.send(Err(Error::Core(error)));
                    }
                }
            }
            Command::Publication { spec, response } => {
                if !response.is_closed() {
                    let result = self.claim_publication(spec);
                    let _ = response.send(result);
                }
            }
            Command::VideoSubscription {
                selector,
                observation,
                response,
            } => {
                if !response.is_closed() {
                    let result = self.claim_subscription(selector, observation);
                    let _ = response.send(result);
                }
            }
            Command::AudioSubscription { selector, response } => {
                if !response.is_closed() {
                    let result = self.claim_audio_subscription(selector);
                    let _ = response.send(result);
                }
            }
            Command::AudioPolicy { policy, response } => {
                if !response.is_closed() {
                    let result = self.claim_audio_policy(policy);
                    let _ = response.send(result);
                }
            }
            Command::ObserveSubscription {
                owner,
                source,
                mode,
                response,
            } => {
                self.observe_subscription(owner, source, mode, response);
            }
            Command::PublicationInput { owner, response } => {
                if !response.is_closed() {
                    let result = self.register_input(owner);
                    let _ = response.send(result);
                }
            }
            Command::ClosePublicationInput {
                operation,
                response,
            } => {
                let result = self.close_input(operation);
                let _ = response.send(result);
            }
            Command::Resource { command, response } => {
                let release = match &command {
                    agent_core::ResourceCommand::Release { owner } => Some(*owner),
                    _ => None,
                };
                let result = self.core.resource(command).map_err(Error::Core);
                if result.is_ok() {
                    if let Some(lease) = release
                        .and_then(|owner| self.publications.get(&owner))
                        .and_then(Weak::upgrade)
                    {
                        lease.closed.close();
                    }
                }
                if result.is_ok()
                    && let Some(lease) = release
                        .and_then(|owner| self.subscriptions.get(&owner))
                        .and_then(Weak::upgrade)
                {
                    lease.closed.close();
                }
                let _ = response.send(result);
            }
        }
    }

    fn create_source(&mut self, slot: MediaSlot) -> Result<PhysicalSource, Error> {
        if !matches!(slot, MediaSlot::LocalAudio(_) | MediaSlot::LocalVideo(_)) {
            return Err(Error::UnknownSlot(slot));
        }
        let identity = self.peer_mut()?.replace_source(&slot)?;
        self.close_source_observation(&slot);
        let inner = Arc::new(SourceLease {
            slot: slot.clone(),
            identity: identity.clone(),
            closed: Arc::new(Signal::default()),
            leases_changed: self.leases_changed.clone(),
            commands: self.command_tx.clone(),
            media: self.media_tx.clone(),
        });
        self.sources.insert(
            slot,
            SourceObservation {
                lease: Arc::downgrade(&inner),
                identity,
                last_rates: None,
            },
        );
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(
                self.peer
                    .as_ref()
                    .and_then(|peer| peer.source_identity(&inner.slot)),
                Some(inner.identity.clone()),
                "an admitted source lease must name the current native source"
            );
            debug_assert!(self.sources.get(&inner.slot).is_some_and(|source| {
                source.identity == inner.identity
                    && std::ptr::eq(source.lease.as_ptr(), Arc::as_ptr(&inner))
            }));
        }
        Ok(PhysicalSource { inner })
    }
    fn close_source_observation(&mut self, slot: &MediaSlot) {
        if let Some(source) = self.sources.remove(slot)
            && let Some(lease) = source.lease.upgrade()
        {
            lease.closed.close();
        }
    }
    fn observe(&mut self, slot: MediaSlot, mode: ReceiveMode) -> Result<RemoteMedia, Error> {
        self.retire_dropped();
        if !matches!(slot, MediaSlot::RemoteAudio(_) | MediaSlot::RemoteVideo(_)) {
            return Err(Error::UnknownSlot(slot));
        }
        if mode == ReceiveMode::EncodedAndDecoded && !matches!(slot, MediaSlot::RemoteVideo(_)) {
            return Err(Error::InvalidInput(
                "combined observation is video-only".into(),
            ));
        }
        if self
            .peer
            .as_ref()
            .is_none_or(|peer| !peer.receivers.contains_key(&slot))
        {
            return Err(
                if self.core.source_preparation(&slot).readiness == SourceReadiness::Failed {
                    Error::Rtc("receiver preparation failed".into())
                } else {
                    Error::NotReady
                },
            );
        }
        if let Some(stream) = self.streams.get(&slot) {
            if stream.mode != mode {
                return Err(Error::InvalidInput(
                    "receiver already has a different native mode".into(),
                ));
            }
            if stream.leases.len() >= OBSERVERS {
                return Err(Error::Backpressure);
            }
        }
        let observer = self.config.decoded_video_observer.clone();
        let encoded = self.config.encoded_video_observer.clone();
        let track = self
            .peer_mut()?
            .attach(&slot, mode, observer.as_ref(), encoded.as_ref())?;
        let stream = self.streams.entry(slot.clone()).or_insert_with(|| {
            let (sender, _) = broadcast::channel(STREAM_FRAMES);
            StreamObservation {
                mode,
                generation: track.generation,
                receiver: track.receiver,
                sender,
                leases: Vec::new(),
                native_dropped: 0,
                native_decoded_dropped: 0,
            }
        });
        let receiver = stream.sender.subscribe();
        let lease = Arc::new(StreamLease {
            closed: Arc::new(Signal::default()),
            leases_changed: self.leases_changed.clone(),
            receiver: Mutex::new(receiver),
        });
        stream.leases.push(Arc::downgrade(&lease));
        debug_assert_eq!(stream.generation, track.generation);
        debug_assert_eq!(stream.receiver, track.receiver);
        debug_assert_eq!(stream.mode, mode);
        debug_assert!(stream.leases.len() <= OBSERVERS);
        let _ = stream.sender.send(MediaStreamUpdate::Track { track });
        let generation = stream.generation;
        let receiver = stream.receiver;
        if mode == ReceiveMode::Decoded
            && matches!(slot, MediaSlot::RemoteAudio(_))
            && self.next_playout.is_none()
        {
            self.next_playout = self.later(PLAYOUT);
        }
        Ok(RemoteMedia {
            observation: None,
            slot,
            generation,
            receiver,
            lease,
            commands: self.command_tx.clone(),
        })
    }

    fn retire_dropped(&mut self) {
        self.retire_publications();
        self.retire_subscriptions();
        let dropped: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, source)| source.lease.upgrade().is_none())
            .map(|(slot, source)| (slot.clone(), source.identity.clone()))
            .collect();
        for (slot, identity) in dropped {
            if let Some(peer) = &mut self.peer
                && let Err(error) = peer.close_source(&slot, &identity)
            {
                self.fail(error.into());
            }
            self.sources.remove(&slot);
        }
        for stream in self.streams.values_mut() {
            stream.leases.retain(|lease| {
                lease
                    .upgrade()
                    .is_some_and(|lease| !lease.closed.is_closed())
            });
        }
        let dropped: Vec<_> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.leases.is_empty())
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in dropped {
            self.close_stream(&slot);
        }
        if !self.streams.iter().any(|(slot, stream)| {
            matches!(slot, MediaSlot::RemoteAudio(_)) && stream.mode == ReceiveMode::Decoded
        }) {
            self.next_playout = None;
        }
    }

    fn drain_effects(&mut self) {
        while let Some(effect) = self.core.next_effect() {
            match effect {
                Effect::PublicationInput(control) => self.input_control(control),
                Effect::MediaObservation(control) => self.observation_control(control),
                Effect::Rtc(effect) => match effect {
                    RtcEffect::CreateOffer {
                        generation,
                        data_channels,
                    } => {
                        #[cfg(feature = "sim")]
                        tracing::debug!(target: "pulsebeam_sim_recovery", ?generation,
                            "native create-offer effect started");
                        self.retire_peer();
                        #[cfg(feature = "sim")]
                        tracing::debug!(target: "pulsebeam_sim_recovery", ?generation,
                            "native peer construction started after retirement");
                        let result = (self.runtime)().and_then(|(runtime, owner)| {
                            NativePeer::new(
                                runtime,
                                &self.config.media,
                                generation,
                                &[],
                                &data_channels,
                            )
                            .map(|peer| (peer, owner))
                            .map_err(Error::from)
                        });
                        match result {
                            Ok((peer, owner)) => {
                                #[cfg(feature = "sim")]
                                tracing::debug!(target: "pulsebeam_sim_recovery", ?generation,
                                    "native peer construction completed");
                                self.peer = Some(peer);
                                self.peer_owner = Some(owner);
                                self.next_stats = self.later(STATS_INTERVAL);
                            }
                            Err(error) => {
                                #[cfg(feature = "sim")]
                                tracing::warn!(?generation, error = %error, "native peer construction failed");
                                self.accept(HostEvent::Rtc(RtcEvent::Failed {
                                    generation,
                                    message: error.to_string(),
                                }));
                            }
                        }
                    }
                    RtcEffect::ApplyAnswer {
                        generation,
                        answer,
                        capabilities,
                    } => {
                        let result = self.peer_mut().and_then(|peer| {
                            if peer.generation != generation {
                                Err(Error::Closed)
                            } else {
                                peer.answer(answer, capabilities).map_err(Error::from)
                            }
                        });
                        if let Err(error) = result {
                            self.accept(HostEvent::Rtc(RtcEvent::Failed {
                                generation,
                                message: error.to_string(),
                            }));
                        }
                    }
                    RtcEffect::CreateDataChannel {
                        generation,
                        channel,
                        specification,
                    } => {
                        let result = self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                            .ok_or(Error::Closed)
                            .and_then(|peer| {
                                peer.create_data_channel(channel, &specification)
                                    .map_err(Error::from)
                            });
                        if let Err(error) = result {
                            self.accept(HostEvent::DataChannel(
                                agent_core::DataChannelEvent::Prepared {
                                    generation,
                                    channel,
                                    preparation: Err(error.to_string()),
                                },
                            ));
                        }
                    }
                    RtcEffect::CloseDataChannel {
                        generation,
                        channel,
                    } => {
                        if let Some(peer) = self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                            && let Err(error) = peer.close_data_channel(channel)
                        {
                            self.accept(HostEvent::DataChannel(
                                agent_core::DataChannelEvent::Prepared {
                                    generation,
                                    channel,
                                    preparation: Err(error.to_string()),
                                },
                            ));
                        }
                    }
                    RtcEffect::PrepareMedia {
                        generation,
                        operation,
                        requests,
                    } => {
                        let event = match self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                        {
                            Some(peer) => peer.prepare_media(operation, requests),
                            None => Some(HostEvent::Rtc(RtcEvent::MediaPrepared {
                                generation,
                                operation,
                                slots: Vec::new(),
                                outcome: agent_core::client_sdp::ApplyOutcome::NotApplied,
                                preparation: Err(agent_core::client_sdp::Error::CandidateState),
                            })),
                        };
                        if let Some(event) = event {
                            self.accept(event);
                        }
                    }
                    RtcEffect::AllocateMedia {
                        generation,
                        operation,
                        allocations,
                    } => {
                        let event = match self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                        {
                            Some(peer) => {
                                peer.allocate_media(operation, allocations, &self.config.media)
                            }
                            None => Some(HostEvent::Rtc(RtcEvent::MediaPrepared {
                                generation,
                                operation,
                                slots: Vec::new(),
                                outcome: agent_core::client_sdp::ApplyOutcome::NotApplied,
                                preparation: Err(agent_core::client_sdp::Error::CandidateState),
                            })),
                        };
                        if let Some(event) = event {
                            self.accept(event);
                        }
                    }
                    RtcEffect::RetireMedia {
                        generation,
                        operation,
                        mids,
                    } => {
                        let event = match self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                        {
                            Some(peer) => peer.retire_media(operation, mids),
                            None => Some(HostEvent::Rtc(RtcEvent::MediaPrepared {
                                generation,
                                operation,
                                slots: Vec::new(),
                                outcome: agent_core::client_sdp::ApplyOutcome::NotApplied,
                                preparation: Err(agent_core::client_sdp::Error::CandidateState),
                            })),
                        };
                        if let Some(event) = event {
                            self.accept(event);
                        }
                    }
                    RtcEffect::CancelMediaPreparation {
                        generation,
                        operation,
                    } => {
                        if let Some(event) = self
                            .peer
                            .as_mut()
                            .filter(|peer| peer.generation == generation)
                            .and_then(|peer| peer.cancel_media(operation))
                        {
                            self.accept(event);
                        }
                    }
                    RtcEffect::Close { generation } => {
                        if self
                            .peer
                            .as_ref()
                            .is_some_and(|peer| peer.generation == generation)
                        {
                            self.retire_peer();
                        }
                        self.accept(HostEvent::Rtc(RtcEvent::Closed { generation }));
                    }
                },
                Effect::DataChannel(effect) => {
                    let event = match self.peer.as_mut() {
                        Some(peer) => peer.send(effect),
                        None => {
                            let agent_core::DataChannelEffect::Send {
                                operation,
                                generation,
                                channel,
                                ..
                            } = effect;
                            Some(HostEvent::DataChannel(
                                agent_core::DataChannelEvent::SendFailed {
                                    operation,
                                    generation,
                                    channel,
                                    message: "native generation is closed".into(),
                                },
                            ))
                        }
                    };
                    if let Some(event) = event {
                        self.accept(event);
                    }
                }
                Effect::Http(effect) => self.http_effect(effect),
                Effect::Timer(TimerEffect::Schedule { timer, after }) => {
                    if let Some(at) = self.later(after) {
                        self.timers.insert(timer, at);
                    }
                }
                Effect::Timer(TimerEffect::Cancel { timer }) => {
                    self.timers.remove(&timer);
                }
            }
        }
    }

    fn progress_peer(&mut self) -> bool {
        let Some(peer) = &mut self.peer else {
            return false;
        };
        let generation = peer.generation;
        let progress = match peer.progress() {
            Ok(progress) => progress,
            Err(error) => {
                #[cfg(feature = "sim")]
                tracing::warn!(?generation, error = %error, "native peer progress failed");
                self.accept(HostEvent::Rtc(RtcEvent::Failed {
                    generation,
                    message: error.to_string(),
                }));
                return false;
            }
        };
        self.retire_changed_media();
        for event in progress.events {
            self.accept(event);
        }
        for observation in progress.observations {
            match observation {
                Observation::Track { slot, track } => {
                    if let Some(stream) = self.streams.get(&slot) {
                        let _ = stream.sender.send(MediaStreamUpdate::Track { track });
                    }
                }
                Observation::TrackRemoved { slot } => {
                    self.close_stream(&slot);
                }
                Observation::Statistics(stats) => {
                    self.statistics.send_replace(Some(stats));
                }
                Observation::ChannelMessagesDropped {
                    channel,
                    messages,
                    bytes,
                } => {
                    let _ = self.events.send(AgentEvent::ChannelMessagesDropped {
                        generation: generation.get(),
                        channel: channel.get(),
                        messages,
                        bytes,
                    });
                }
            }
        }
        let more_frames = self.frames(false);
        self.producer_feedback();
        progress.more || more_frames
    }

    fn frames(&mut self, playout: bool) -> bool {
        let mut more = false;
        let slots: Vec<_> = self.streams.keys().cloned().collect();
        for slot in slots {
            let pcm = self.streams.get(&slot).is_some_and(|stream| {
                stream.mode == ReceiveMode::Decoded && matches!(slot, MediaSlot::RemoteAudio(_))
            });
            // The 10 ms playout turn is PCM-only. Other sinks drain on the
            // ordinary ready-work turn, never twice before the actor yields.
            if pcm != playout {
                continue;
            }
            let budget = if pcm {
                1
            } else if self
                .streams
                .get(&slot)
                .is_some_and(|stream| stream.mode == ReceiveMode::EncodedAndDecoded)
            {
                COMBINED_FRAME_BUDGET
            } else {
                FRAME_BUDGET
            };
            for index in 0..budget {
                let result = self
                    .peer
                    .as_mut()
                    .ok_or(Error::Closed)
                    .and_then(|peer| peer.frame(&slot, playout).map_err(Error::from));
                match result {
                    Ok(Some(frame)) => {
                        if !pcm && index.saturating_add(1) == budget {
                            more = true;
                        }
                        if let Some(stream) = self.streams.get(&slot) {
                            let _ = stream.sender.send(MediaStreamUpdate::Frame { frame });
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        self.fail(error);
                        self.close_stream(&slot);
                        break;
                    }
                }
            }
            let drops = self
                .peer
                .as_mut()
                .and_then(|peer| peer.dropped_frames(&slot).ok());
            if let (Some(drops), Some(stream)) = (drops, self.streams.get_mut(&slot))
                && drops > stream.native_dropped
            {
                let skipped = drops.saturating_sub(stream.native_dropped);
                tracing::warn!(?slot, mode = ?stream.mode, skipped, drops, "native receive sink dropped media");
                stream.native_dropped = drops;
                let _ = stream
                    .sender
                    .send(MediaStreamUpdate::NativeDropped { skipped });
            }
            if self
                .streams
                .get(&slot)
                .is_some_and(|stream| stream.mode == ReceiveMode::EncodedAndDecoded)
            {
                let drops = self
                    .peer
                    .as_mut()
                    .and_then(|peer| peer.dropped_decoded_frames(&slot).ok());
                if let (Some(drops), Some(stream)) = (drops, self.streams.get_mut(&slot))
                    && drops > stream.native_decoded_dropped
                {
                    let skipped = drops.saturating_sub(stream.native_decoded_dropped);
                    stream.native_decoded_dropped = drops;
                    let _ = stream
                        .sender
                        .send(MediaStreamUpdate::NativeDecodedDropped { skipped });
                }
            }
        }
        more
    }
    fn producer_feedback(&mut self) {
        let Some(peer) = &mut self.peer else {
            return;
        };
        for (slot, source) in &mut self.sources {
            if !matches!(slot, MediaSlot::LocalVideo(_))
                || self.config.media.video_input != VideoInput::EncodedH264
            {
                continue;
            }
            let Ok(feedback) = peer.feedback(slot, &source.identity) else {
                continue;
            };
            let changed = feedback.keyframe_requested
                || feedback
                    .encoding_keyframes_requested
                    .iter()
                    .any(|value| *value)
                || feedback.encoder_error.is_some()
                || feedback.rates != source.last_rates;
            source.last_rates.clone_from(&feedback.rates);
            if changed {
                for input in self.inputs.values() {
                    if input
                        .target
                        .borrow()
                        .as_ref()
                        .is_some_and(|target| target.source.identity() == source.identity)
                    {
                        if let Some(lease) = input.lease.upgrade() {
                            let _ = lease.feedback.send(feedback.clone());
                        }
                    }
                }
                let _ = self.events.send(AgentEvent::ProducerFeedback {
                    slot: slot.clone(),
                    source: source.identity.clone(),
                    feedback,
                });
            }
        }
    }
    fn retire_changed_media(&mut self) {
        let peer = self.peer.as_ref();
        let sources = self
            .sources
            .iter()
            .filter_map(|(slot, source)| {
                if peer.and_then(|peer| peer.source_identity(slot)) != Some(source.identity.clone())
                {
                    Some(slot.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let streams = self
            .streams
            .iter()
            .filter_map(|(slot, stream)| {
                if peer.is_none_or(|peer| {
                    peer.generation.get() != stream.generation
                        || peer
                            .receivers
                            .get(slot)
                            .is_none_or(|receiver| receiver.id.get() != stream.receiver)
                }) {
                    Some(slot.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for slot in sources {
            self.close_source_observation(&slot);
        }
        for slot in streams {
            self.close_stream(&slot);
        }
    }

    fn close_stream(&mut self, slot: &MediaSlot) {
        if let Some(stream) = self.streams.remove(slot) {
            for lease in stream.leases {
                if let Some(lease) = lease.upgrade() {
                    lease.close();
                }
            }
            if let Some(peer) = &mut self.peer
                && peer.generation.get() == stream.generation
                && peer.receivers.get(slot).is_some_and(|receiver| {
                    receiver.id.get() == stream.receiver && receiver.mode.is_some()
                })
                && let Err(error) = peer.detach(slot)
            {
                self.fail(error.into());
            }
        }
    }
    fn retire_peer(&mut self) {
        let slots: Vec<_> = self.sources.keys().cloned().collect();
        for slot in slots {
            self.close_source_observation(&slot);
        }
        let slots: Vec<_> = self.streams.keys().cloned().collect();
        for slot in slots {
            self.close_stream(&slot);
        }
        if let Some(mut peer) = self.peer.take() {
            match peer.shutdown() {
                Ok(events) => {
                    for event in events {
                        self.accept(event);
                    }
                }
                Err(error) => self.fail(error.into()),
            }
        }
        self.peer_owner.take();
        self.statistics.send_replace(None);
        self.next_playout = None;
        self.next_stats = None;
    }
    async fn publish(&mut self) {
        let snapshot = self.core.snapshot().clone();
        if *self.snapshot.borrow() != snapshot {
            #[cfg(feature = "sim")]
            tracing::debug!(target: "pulsebeam_sim_recovery",
                generation = ?snapshot.generation, connection = ?snapshot.connection,
                participant = ?snapshot.participant_id, "native core snapshot changed");
            self.snapshot.send_replace(snapshot.clone());
            let _ = self.snapshot_events.send(snapshot.clone());
        }
        // A single recovery input can release an entire ordered history.
        // Give active observers a turn without growing the bounded event bus.
        let mut published = 0usize;
        while let Some(notification) = self.core.next_notification() {
            let _ = self.events.send(AgentEvent::Core(notification));
            published = published.saturating_add(1);
            if published >= EVENTS / 2 {
                tokio::task::yield_now().await;
                published = 0;
            }
        }
        if snapshot.connection == agent_core::ConnectionState::Disconnected && self.peer.is_none() {
            for response in self.close_waiters.drain(..) {
                let _ = response.send(Ok(()));
            }
        }
    }
    fn http_effect(&mut self, effect: HttpEffect) {
        match effect {
            HttpEffect::Cancel { operation } => {
                #[cfg(feature = "sim")]
                tracing::debug!(target: "pulsebeam_sim_recovery", ?operation,
                    "native signaling HTTP cancellation");
                if let Some(task) = self.http_tasks.remove(&operation) {
                    task.abort();
                }
            }
            HttpEffect::Request {
                operation,
                generation: _generation,
                request,
            } => {
                #[cfg(feature = "sim")]
                tracing::debug!(target: "pulsebeam_sim_recovery", ?operation, generation = ?_generation,
                    method = ?request.method, "native signaling HTTP started");
                let method = match request.method {
                    HttpMethod::Post => http::Method::POST,
                    HttpMethod::Patch => http::Method::PATCH,
                    HttpMethod::Delete => http::Method::DELETE,
                };
                let mut builder = http::Request::builder().method(method).uri(request.uri);
                for header in request.headers {
                    builder = builder.header(header.name, header.value);
                }
                let request = match builder.body(request.body) {
                    Ok(request) => request,
                    Err(error) => {
                        self.accept(HostEvent::Http(HttpEvent::Failed {
                            operation,
                            message: error.to_string(),
                        }));
                        return;
                    }
                };
                let client = self.http.clone();
                let completions = self.http_tx.clone();
                let task = tokio::spawn(async move {
                    #[cfg(feature = "sim")]
                    let started = Instant::now();
                    let result = execute_signaling_http(client.as_ref(), request).await;
                    #[cfg(feature = "sim")]
                    tracing::debug!(target: "pulsebeam_sim_recovery", ?operation, generation = ?_generation,
                        elapsed_ms = started.elapsed().as_millis(),
                        status = ?result.as_ref().ok().map(|response| response.status),
                        failed = result.is_err(), "native signaling HTTP completed");
                    let _ = completions.send(HttpCompletion { operation, result }).await;
                });
                if let Some(old) = self.http_tasks.insert(operation, task) {
                    old.abort();
                }
            }
        }
    }
    fn shutdown(&mut self) {
        self.close_subscription_handles();
        for lease in self.publications.values().filter_map(Weak::upgrade) {
            lease.closed.close();
        }
        for input in self.inputs.values() {
            if let Some(lease) = input.lease.upgrade() {
                lease.closed.close();
            }
            input.target.send_replace(None);
        }
        self.inputs.clear();
        self.publications.clear();
        for (_, task) in std::mem::take(&mut self.http_tasks) {
            task.abort();
        }
        self.timers.clear();
        self.retire_peer();
        for waiter in self.close_waiters.drain(..) {
            let _ = waiter.send(Err(Error::Closed));
        }
        self.finished.close();
    }
}
impl<R, F, O> Drop for Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn execute_signaling_http(
    client: &dyn AsyncHttpClient,
    request: http::Request<Vec<u8>>,
) -> Result<agent_core::HttpResponse, String> {
    let response = tokio::time::timeout(SIGNALING_HTTP_TIMEOUT, client.execute(request))
        .await
        .map_err(|_| "signaling HTTP request timed out".to_string())?
        .map_err(|error| error.to_string())?;
    Ok(agent_core::HttpResponse {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value.to_str().ok().map(|value| HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_string(),
                })
            })
            .collect(),
        body: response.into_body(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PendingHttp(Arc<AtomicBool>);
    impl AsyncHttpClient for PendingHttp {
        fn execute(
            &self,
            _request: pulsebeam_core::net::HttpRequest,
        ) -> pulsebeam_core::net::HttpResult<'_> {
            struct CancelOnDrop(Arc<AtomicBool>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            Box::pin(async move {
                let _guard = CancelOnDrop(self.0.clone());
                std::future::pending().await
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn signaling_timeout_drops_pending_host_io_for_core_to_retry() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let client = PendingHttp(cancelled.clone());
        let start = Instant::now();
        let request = http::Request::post("http://localhost/session")
            .body(Vec::new())
            .unwrap();
        assert_eq!(
            execute_signaling_http(&client, request).await,
            Err("signaling HTTP request timed out".into())
        );
        assert_eq!(start.elapsed(), SIGNALING_HTTP_TIMEOUT);
        assert!(cancelled.load(Ordering::SeqCst));
    }

    fn config() -> Config {
        Config::new(agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "token".into(),
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
        })
    }

    // No connect intent is submitted here, so no SDK graph or network witness
    // is claimed. This exercises actor ownership and terminal observation rules.
    #[tokio::test]
    async fn close_and_abort_are_idempotent_and_streams_terminate_with_retained_handles() {
        let agent = Agent::spawn(config(), Host::new(Box::new(reqwest::Client::new())))
            .await
            .unwrap();
        let events = agent.events();
        let (_, snapshots) = agent.snapshot_events();
        let publication = agent
            .publication(agent_core::TrackSpec::Video {
                label: "camera".into(),
            })
            .await
            .unwrap();
        let input = publication.input().await.unwrap();
        let subscription = agent
            .video_subscription(
                agent_core::TrackSelector {
                    participant_external_id: "remote".into(),
                    label: "camera".into(),
                },
                agent_core::VideoObservation {
                    visible: true,
                    height: 720,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut pending = Box::pin(subscription.observe(ReceiveMode::Encoded));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                std::future::Future::poll(pending.as_mut(), cx).is_pending()
            ))
            .await
        );
        subscription.close().await.unwrap();
        assert!(pending.await.is_err());
        assert!(subscription.is_closed());
        let audio = agent
            .audio_subscription(agent_core::TrackSelector {
                participant_external_id: "remote".into(),
                label: "microphone".into(),
            })
            .await
            .unwrap();
        assert!(
            audio
                .configure_video(agent_core::VideoObservation::default())
                .await
                .is_err()
        );
        let mut audio_pending = Box::pin(audio.observe(ReceiveMode::Decoded));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                std::future::Future::poll(audio_pending.as_mut(), cx).is_pending()
            ))
            .await
        );
        audio.close().await.unwrap();
        assert!(audio_pending.await.is_err());
        assert!(audio.is_closed());
        assert!(!input.is_closed());
        assert!(matches!(
            input.capture_time_us().await,
            Err(Error::NotReady)
        ));
        agent.close().await.unwrap();
        agent.close().await.unwrap();
        assert_eq!(
            agent.snapshot().connection,
            agent_core::ConnectionState::Disconnected
        );
        assert!(!input.is_closed());
        assert!(matches!(
            input.capture_time_us().await,
            Err(Error::NotReady)
        ));
        publication.close().await.unwrap();
        assert!(input.is_closed());
        agent.abort().await.unwrap();
        agent.abort().await.unwrap();
        assert!(matches!(events.try_next(), Some(StreamUpdate::Closed)));
        assert!(matches!(events.next().await, StreamUpdate::Closed));
        assert!(matches!(snapshots.try_next(), Some(StreamUpdate::Closed)));
        assert!(matches!(snapshots.next().await, StreamUpdate::Closed));
    }

    #[test]
    fn runtime_services_retire_with_unadmitted_native_graphs() {
        use std::{cell::Cell, rc::Rc, time::Duration};

        let _serial = crate::native_media::NATIVE_TEST_SERIAL.lock().unwrap();
        let world = Rc::new(rtc::ControlledWorld::acquire(13, Duration::from_secs(10)).unwrap());
        let network = world.create_network().unwrap();
        let endpoint = Rc::new(
            network
                .register_endpoint("10.0.0.2".parse().unwrap())
                .unwrap(),
        );
        let created = Rc::new(Cell::new(0));
        let retired = Rc::new(Cell::new(0));
        struct Owner(Rc<Cell<usize>>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(tokio::task::LocalSet::new().run_until(async {
            let (agent, actor) = Agent::managed_with_services(
                config(),
                Host::new(Box::new(PendingHttp(Arc::new(AtomicBool::new(false))))),
                {
                    let world = world.clone();
                    let created = created.clone();
                    let retired = retired.clone();
                    move || {
                        let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
                        created.set(created.get() + 1);
                        Ok((runtime, Owner(retired.clone())))
                    }
                },
            )
            .unwrap();
            let actor = tokio::task::spawn_local(actor);
            let events = agent.events();
            let publication = agent
                .publication(agent_core::TrackSpec::Audio {
                    label: "microphone".into(),
                })
                .await
                .unwrap();
            publication.set_muted(true).await.unwrap();
            let input = publication.input().await.unwrap();
            let feedback = input.feedback();
            assert!(matches!(input.submit(pcm()).await, Err(Error::NotReady)));
            for round in 1..=2 {
                agent
                    .replace_desired(DesiredState {
                        revision: agent.snapshot().desired_revision + 1,
                        connected: true,
                        ..DesiredState::default()
                    })
                    .await
                    .unwrap();
                if round == 1 {
                    publication.set_muted(false).await.unwrap();
                }
                for turn in 0..4096 {
                    if agent.snapshot().connection == agent_core::ConnectionState::Joining {
                        break;
                    }
                    assert!(
                        turn < 4095,
                        "round {round}: native offer did not reach HTTP admission; snapshot {:?}; events {:?}",
                        agent.snapshot(),
                        std::iter::from_fn(|| events.try_next()).collect::<Vec<_>>()
                    );
                    let progress = world.pump(64);
                    assert!(!progress.blocked);
                    if progress.dispatched == 0 {
                        if let Some(next) = world.next_deadline() {
                            world.advance(next.saturating_sub(world.now())).unwrap();
                        }
                    }
                    tokio::task::yield_now().await;
                }
                assert!(matches!(input.target(), Err(Error::NotReady)));
                assert!(matches!(input.capture_time_us().await, Err(Error::NotReady)));
                assert!(matches!(input.submit(pcm()).await, Err(Error::NotReady)));
                assert!(!input.is_closed());
                assert_eq!(created.get(), round);
                assert_eq!(retired.get(), round - 1);
                agent.close().await.unwrap();
                assert!(!input.is_closed());
                assert!(matches!(input.capture_time_us().await, Err(Error::NotReady)));
                assert!(feedback.try_next().is_none());
                assert_eq!(retired.get(), round);
            }
            publication.close().await.unwrap();
            assert!(input.is_closed());
            assert!(matches!(feedback.try_next(), Some(StreamUpdate::Closed)));
            agent.abort().await.unwrap();
            actor.await.unwrap();
            assert_eq!(retired.get(), 2);
        }));
    }

    fn source_fixture() -> (
        PhysicalSource,
        Arc<agent_core::PublicationInputControl>,
        mpsc::Receiver<Submission>,
    ) {
        // Queue ownership only, not native admission or target readiness.
        let mut core = agent_core::Agent::new(config().session).unwrap();
        let agent_core::ResourceResult::Publication { owner, .. } = core
            .resource(agent_core::ResourceCommand::ClaimPublication {
                spec: agent_core::TrackSpec::Audio {
                    label: "microphone".into(),
                },
            })
            .unwrap()
        else {
            panic!("publication");
        };
        let agent_core::ResourceResult::PublicationInput { operation, .. } = core
            .resource(agent_core::ResourceCommand::BeginPublicationInput {
                owner,
                input: agent_core::PublicationInputIntent::Replace,
                present: true,
                muted: None,
            })
            .unwrap()
        else {
            panic!("input");
        };
        core.resource(agent_core::ResourceCommand::CompletePublicationInput {
            operation,
            succeeded: true,
        })
        .unwrap();
        let control = std::iter::from_fn(|| core.next_effect())
            .filter_map(|effect| match effect {
                Effect::PublicationInput(control) => Some(control),
                _ => None,
            })
            .last()
            .unwrap();
        let (commands, _) = mpsc::channel(COMMANDS);
        let (media, receiver) = mpsc::channel(MEDIA_COMMANDS);
        (
            PhysicalSource {
                inner: Arc::new(SourceLease {
                    slot: MediaSlot::LocalAudio("a0".into()),
                    identity: SourceIdentity {
                        generation: 1,
                        source: 1,
                    },
                    closed: Arc::new(Signal::default()),
                    leases_changed: Arc::new(Notify::new()),
                    commands,
                    media,
                }),
            },
            Arc::new(control),
            receiver,
        )
    }
    fn pcm() -> InputFrame {
        InputFrame::Pcm {
            frame: PcmFrame {
                sample_rate_hz: 48_000,
                channels: 1,
                samples: vec![0; 480],
                timestamp_us: 0,
            },
        }
    }

    #[tokio::test]
    async fn owned_submission_has_explicit_byte_capacity_backpressure_and_closed_outcomes() {
        let (source, control, mut receiver) = source_fixture();
        let oversized = InputFrame::I420 {
            frame: I420Frame {
                width: 1,
                height: 1,
                timestamp_us: 0,
                rotation_degrees: 0,
                data: vec![0; MAX_FRAME_BYTES + 1],
            },
        };
        assert!(matches!(
            source.submit(oversized, control.clone()).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        for _ in 0..MEDIA_COMMANDS {
            let (response, _) = oneshot::channel();
            source
                .inner
                .media
                .try_send(Submission {
                    control: control.clone(),
                    slot: source.inner.slot.clone(),
                    identity: source.identity(),
                    frame: pcm(),
                    response,
                })
                .unwrap();
        }
        assert!(matches!(
            source.submit(pcm(), control.clone()).await,
            Err(Error::Backpressure)
        ));
        // Native admission currency only, not peer or source preparation.
        let (_agent, mut actor) = Agent::parts(
            config(),
            Host::new(Box::new(PendingHttp(Arc::new(AtomicBool::new(false))))),
            || Ok((rtc::ThreadedRuntime, ())),
        )
        .unwrap();
        let publication = actor
            .claim_publication(agent_core::TrackSpec::Audio {
                label: "microphone".into(),
            })
            .unwrap();
        let owner = *actor.publications.keys().next().unwrap();
        let input = actor.register_input(owner).unwrap();
        let feedback = input.feedback();
        let latest_control = |core: &mut agent_core::Agent| {
            Arc::new(
                std::iter::from_fn(|| core.next_effect())
                    .filter_map(|effect| match effect {
                        Effect::PublicationInput(control) => Some(control),
                        _ => None,
                    })
                    .last()
                    .unwrap(),
            )
        };
        let original = latest_control(&mut actor.core);
        assert!(actor.input_submission_current(&original));
        for muted in [true, false] {
            actor
                .core
                .resource(agent_core::ResourceCommand::SetPublicationMuted { owner, muted })
                .unwrap();
        }
        let resumed = latest_control(&mut actor.core);
        assert_ne!(original, resumed);
        assert!(actor.input_submission_current(&resumed));
        let (response, _) = oneshot::channel();
        let mut forged = Submission {
            control: original,
            slot: source.slot().clone(),
            identity: source.identity(),
            frame: pcm(),
            response,
        };
        assert!(!actor.input_submission_current(&forged.control));
        forged.control = resumed.clone();
        assert!(actor.input_submission_current(&forged.control));

        let replacement = actor.register_input(owner).unwrap();
        let replacement_control = latest_control(&mut actor.core);
        assert!(input.is_closed());
        assert!(matches!(feedback.try_next(), Some(StreamUpdate::Closed)));
        assert_ne!(
            resumed.operation.input(),
            replacement_control.operation.input()
        );
        assert!(!actor.input_submission_current(&forged.control));
        forged.control = replacement_control.clone();
        assert!(actor.input_submission_current(&forged.control));
        actor.inputs[&owner].lease.upgrade().unwrap().closed.close();
        assert!(
            actor
                .core
                .publication_input_control_current(&replacement_control)
        );
        assert!(!actor.input_submission_current(&forged.control));

        let current = actor.register_input(owner).unwrap();
        forged.control = latest_control(&mut actor.core);
        assert!(actor.input_submission_current(&forged.control));
        actor.publications[&owner].upgrade().unwrap().closed.close();
        assert!(
            actor
                .core
                .publication_input_control_current(&forged.control)
        );
        assert!(!actor.input_submission_current(&forged.control));
        assert!(publication.is_closed());
        assert!(!current.is_closed());

        source.inner.closed.close();
        assert!(matches!(
            source.submit(pcm(), control).await,
            Err(Error::Closed)
        ));
        assert!(matches!(source.capture_time_us().await, Err(Error::Closed)));
    }

    #[test]
    fn one_media_drain_turn_and_drop_notice_fit_the_observation_queue() {
        let (sender, mut receiver) = broadcast::channel(STREAM_FRAMES);
        // Queue arithmetic only. These updates are not native loss evidence.
        for index in 0..FRAME_BUDGET {
            sender
                .send(MediaStreamUpdate::NativeDropped {
                    skipped: index as u64,
                })
                .unwrap();
        }
        sender
            .send(MediaStreamUpdate::NativeDropped { skipped: u64::MAX })
            .unwrap();
        for index in 0..FRAME_BUDGET {
            assert!(matches!(
                receiver.try_recv(),
                Ok(MediaStreamUpdate::NativeDropped { skipped }) if skipped == index as u64
            ));
        }
        assert!(matches!(
            receiver.try_recv(),
            Ok(MediaStreamUpdate::NativeDropped { skipped: u64::MAX })
        ));
    }

    #[test]
    fn combined_media_turn_and_both_drop_notices_fit_the_unchanged_queue() {
        let (sender, mut receiver) = broadcast::channel(STREAM_FRAMES);
        // Queue arithmetic only, not simulated native frames or loss evidence.
        for index in 0..COMBINED_FRAME_BUDGET {
            sender
                .send(MediaStreamUpdate::NativeDropped {
                    skipped: index as u64,
                })
                .unwrap();
        }
        sender
            .send(MediaStreamUpdate::NativeDropped { skipped: u64::MAX })
            .unwrap();
        sender
            .send(MediaStreamUpdate::NativeDecodedDropped { skipped: u64::MAX })
            .unwrap();
        for index in 0..COMBINED_FRAME_BUDGET {
            assert!(
                matches!(receiver.try_recv(), Ok(MediaStreamUpdate::NativeDropped { skipped }) if skipped == index as u64)
            );
        }
        assert!(matches!(
            receiver.try_recv(),
            Ok(MediaStreamUpdate::NativeDropped { skipped: u64::MAX })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(MediaStreamUpdate::NativeDecodedDropped { skipped: u64::MAX })
        ));
    }

    // These are synthetic owned application values for queue/lifetime checks,
    // not decoded media injected into an RTC or an SFU acceptance scenario.
    #[tokio::test]
    async fn retiring_a_stream_clears_queued_frames_without_invalidating_returned_values() {
        let (sender, receiver) = broadcast::channel(STREAM_FRAMES);
        let (commands, _) = mpsc::channel(COMMANDS);
        let lease = Arc::new(StreamLease {
            closed: Arc::new(Signal::default()),
            leases_changed: Arc::new(Notify::new()),
            receiver: Mutex::new(receiver),
        });
        let stream = RemoteMedia {
            observation: None,
            slot: MediaSlot::RemoteAudio(0),
            generation: 1,
            receiver: 7,
            lease: lease.clone(),
            commands,
        };
        let frame = RemoteFrame {
            track: RemoteTrack {
                generation: 1,
                receiver: 7,
                track_id: "native-track".into(),
                mid: "4".into(),
                media_index: 4,
            },
            metadata: FrameMetadata::default(),
            frame: OutputFrame::Pcm {
                sample_rate_hz: 48_000,
                channels: 1,
                samples: vec![13, 17],
            },
        };
        sender
            .send(MediaStreamUpdate::Frame {
                frame: frame.clone(),
            })
            .unwrap();
        let returned = match stream.next().await {
            MediaStreamUpdate::Frame { frame } => frame,
            other => panic!("expected owned frame, got {other:?}"),
        };
        sender
            .send(MediaStreamUpdate::Frame {
                frame: frame.clone(),
            })
            .unwrap();
        stream.close().await;
        assert!(matches!(stream.next().await, MediaStreamUpdate::Closed));
        assert!(matches!(
            lease.receiver.lock().await.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(returned, frame);
        assert!(matches!(
            stream.request_keyframe().await,
            Err(Error::Closed)
        ));
    }

    #[test]
    fn public_handles_are_send_and_sync_without_moving_a_managed_engine() {
        fn handles<T: Send + Sync>() {}
        handles::<Agent>();
        handles::<Publication>();
        handles::<PublicationInput>();
        handles::<RemoteMedia>();
    }
}
