use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_core::ffi as core_ffi;
use pulsebeam_core::dd::{MANDATORY_LEN, MAX_DD_LEN, RawDependencyDescriptor};
use pulsebeam_core::net::UdpSocket;
use str0m::media::{Frequency, MediaTime, SimulcastLayer};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use crate::{
    Agent as RuntimeAgent, AgentEvent as RuntimeEvent, Config, Error as RuntimeError, Host,
    MediaFrame as RuntimeFrame, RemoteMedia, TransportStatistics as RuntimeStatistics,
};

const OBSERVATION_CAPACITY: usize = 32;
const MAX_ENCODED_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VideoEncoding {
    pub slot: String,
    pub layers: Vec<String>,
    pub temporal_layers: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NativeAgentConfig {
    pub session: core_ffi::AgentConfig,
    pub udp_bind_address: Option<String>,
    pub tcp_server: Option<String>,
    pub video_encodings: Vec<VideoEncoding>,
    pub dependency_descriptor: bool,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NativeError {
    #[error("agent operation failed: {error:?}")]
    Failure { error: core_ffi::AgentError },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NativeEvent {
    Core {
        notification: core_ffi::Notification,
    },
    KeyframeRequested {
        slot: String,
        encoding: Option<String>,
    },
    RuntimeFailed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
#[allow(
    clippy::large_enum_variant,
    reason = "UniFFI stream outcomes carry portable snapshots by value"
)]
pub enum SnapshotUpdate {
    Snapshot { snapshot: core_ffi::Snapshot },
    Lagged { skipped: u64 },
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum EventUpdate {
    Event { event: NativeEvent },
    Lagged { skipped: u64 },
    Closed,
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum MediaUpdate {
    Frame { frame: core_ffi::MediaFrame },
    Lagged { skipped: u64 },
    Closed,
}

enum ControlCommand {
    ReplaceDesired {
        desired: core_ffi::DesiredState,
        response: oneshot::Sender<Result<(), NativeError>>,
    },
    SendTopic {
        publisher: core_ffi::TopicPublisher,
        payload: Vec<u8>,
        response: oneshot::Sender<Result<(), NativeError>>,
    },
    SendFrame {
        audio: bool,
        slot: String,
        encoding: Option<String>,
        frame: core_ffi::MediaFrame,
        response: oneshot::Sender<Result<(), NativeError>>,
    },
    Observe {
        audio: bool,
        slot: u8,
        response: oneshot::Sender<Result<RemoteMedia, NativeError>>,
    },
    Reconnect {
        response: oneshot::Sender<Result<(), NativeError>>,
    },
    Close {
        response: oneshot::Sender<Result<(), NativeError>>,
    },
    Abort {
        response: oneshot::Sender<Result<(), NativeError>>,
    },
}

#[derive(uniffi::Object)]
pub struct Agent {
    runtime: RuntimeAgent,
    commands: mpsc::Sender<ControlCommand>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Agent {
    #[uniffi::constructor]
    pub async fn new(config: NativeAgentConfig) -> Result<Arc<Self>, NativeError> {
        let bind_address = parse_address(
            config.udp_bind_address.as_deref().unwrap_or("0.0.0.0:0"),
            "UDP bind address",
        )?;
        let runtime_config = runtime_config(config)?;
        let udp = UdpSocket::bind(bind_address)
            .await
            .map_err(RuntimeError::Io)
            .map_err(native_runtime_error)?;
        let host = Host::new(Box::new(reqwest::Client::new()), udp);
        let runtime = RuntimeAgent::spawn(runtime_config, host)
            .await
            .map_err(native_runtime_error)?;
        Ok(Self::from_runtime(runtime))
    }

    pub async fn replace_desired(
        &self,
        desired: core_ffi::DesiredState,
    ) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::ReplaceDesired {
            desired,
            response,
        })
        .await
    }

    pub async fn send_topic(
        &self,
        publisher: core_ffi::TopicPublisher,
        payload: Vec<u8>,
    ) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::SendTopic {
            publisher,
            payload,
            response,
        })
        .await
    }

    pub fn local_video(&self, slot: String) -> Arc<LocalMediaSender> {
        Arc::new(LocalMediaSender {
            audio: false,
            slot,
            commands: self.commands.clone(),
        })
    }

    pub fn local_audio(&self, slot: String) -> Arc<LocalMediaSender> {
        Arc::new(LocalMediaSender {
            audio: true,
            slot,
            commands: self.commands.clone(),
        })
    }

    pub async fn remote_video(&self, slot: u8) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        self.remote_media(false, slot).await
    }

    pub async fn remote_audio(&self, slot: u8) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        self.remote_media(true, slot).await
    }

    pub fn snapshot(&self) -> core_ffi::Snapshot {
        (&self.runtime.snapshot()).into()
    }

    pub fn snapshots(&self) -> Arc<SnapshotStream> {
        let (initial, events) = self.runtime.snapshot_events();
        SnapshotStream::spawn(initial, events)
    }

    pub fn events(&self) -> Arc<EventStream> {
        EventStream::spawn(self.runtime.events())
    }

    pub fn statistics(&self) -> core_ffi::TransportStatistics {
        runtime_statistics(&self.runtime.statistics().borrow())
    }

    pub async fn reconnect(&self) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::Reconnect {
            response,
        })
        .await
    }

    pub async fn close(&self) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::Close {
            response,
        })
        .await
    }

    pub async fn abort(&self) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::Abort {
            response,
        })
        .await
    }
}

impl Agent {
    pub fn from_runtime(runtime: RuntimeAgent) -> Arc<Self> {
        let topology = runtime.topology().clone();
        let (commands, receiver) = mpsc::channel(OBSERVATION_CAPACITY);
        tokio::spawn(run_control(runtime.clone(), topology, receiver));
        Arc::new(Self { runtime, commands })
    }

    async fn remote_media(
        &self,
        audio: bool,
        slot: u8,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        let media = request(&self.commands, |response| ControlCommand::Observe {
            audio,
            slot,
            response,
        })
        .await?;
        Ok(RemoteMediaReceiver::spawn(media))
    }
}

#[derive(uniffi::Object)]
pub struct LocalMediaSender {
    audio: bool,
    slot: String,
    commands: mpsc::Sender<ControlCommand>,
}

#[uniffi::export(async_runtime = "tokio")]
impl LocalMediaSender {
    pub async fn send(&self, frame: core_ffi::MediaFrame) -> Result<(), NativeError> {
        self.send_encoding(None, frame).await
    }

    pub async fn send_encoding(
        &self,
        encoding: Option<String>,
        frame: core_ffi::MediaFrame,
    ) -> Result<(), NativeError> {
        request(&self.commands, |response| ControlCommand::SendFrame {
            audio: self.audio,
            slot: self.slot.clone(),
            encoding,
            frame,
            response,
        })
        .await
    }
}

enum SnapshotRequest {
    Next(oneshot::Sender<SnapshotUpdate>),
}

#[derive(uniffi::Object)]
pub struct SnapshotStream {
    requests: mpsc::Sender<SnapshotRequest>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SnapshotStream {
    pub async fn next(&self) -> SnapshotUpdate {
        let (response, result) = oneshot::channel();
        if self
            .requests
            .send(SnapshotRequest::Next(response))
            .await
            .is_err()
        {
            return SnapshotUpdate::Closed;
        }
        result.await.unwrap_or(SnapshotUpdate::Closed)
    }
}

impl SnapshotStream {
    fn spawn(
        initial: agent_core::Snapshot,
        source: broadcast::Receiver<agent_core::Snapshot>,
    ) -> Arc<Self> {
        let (requests, receiver) = mpsc::channel(OBSERVATION_CAPACITY);
        tokio::spawn(run_broadcast_stream(
            source,
            receiver,
            vec![initial],
            |snapshot| SnapshotUpdate::Snapshot {
                snapshot: (&snapshot).into(),
            },
            |skipped| SnapshotUpdate::Lagged { skipped },
            || SnapshotUpdate::Closed,
            Some(|snapshot| snapshot.version),
        ));
        Arc::new(Self { requests })
    }
}

enum EventRequest {
    Next(oneshot::Sender<EventUpdate>),
}

#[derive(uniffi::Object)]
pub struct EventStream {
    requests: mpsc::Sender<EventRequest>,
}

#[uniffi::export(async_runtime = "tokio")]
impl EventStream {
    pub async fn next(&self) -> EventUpdate {
        let (response, result) = oneshot::channel();
        if self
            .requests
            .send(EventRequest::Next(response))
            .await
            .is_err()
        {
            return EventUpdate::Closed;
        }
        result.await.unwrap_or(EventUpdate::Closed)
    }
}

impl EventStream {
    fn spawn(source: broadcast::Receiver<RuntimeEvent>) -> Arc<Self> {
        let (requests, receiver) = mpsc::channel(OBSERVATION_CAPACITY);
        tokio::spawn(run_broadcast_stream(
            source,
            receiver,
            Vec::new(),
            |event| EventUpdate::Event {
                event: native_event(event),
            },
            |skipped| EventUpdate::Lagged { skipped },
            || EventUpdate::Closed,
            None,
        ));
        Arc::new(Self { requests })
    }
}

enum MediaRequest {
    Next(oneshot::Sender<MediaUpdate>),
}

#[derive(uniffi::Object)]
pub struct RemoteMediaReceiver {
    requests: mpsc::Sender<MediaRequest>,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteMediaReceiver {
    pub async fn next(&self) -> MediaUpdate {
        let (response, result) = oneshot::channel();
        if self
            .requests
            .send(MediaRequest::Next(response))
            .await
            .is_err()
        {
            return MediaUpdate::Closed;
        }
        result.await.unwrap_or(MediaUpdate::Closed)
    }
}

impl RemoteMediaReceiver {
    fn spawn(media: RemoteMedia) -> Arc<Self> {
        let (requests, receiver) = mpsc::channel(OBSERVATION_CAPACITY);
        tokio::spawn(run_remote_media(media, receiver));
        Arc::new(Self { requests })
    }
}

async fn run_control(
    runtime: RuntimeAgent,
    topology: agent_core::MediaTopology,
    mut commands: mpsc::Receiver<ControlCommand>,
) {
    let mut desired_owner = DesiredOwner {
        topology,
        next_revision: runtime.snapshot().desired_revision.saturating_add(1),
    };
    while let Some(command) = commands.recv().await {
        match command {
            ControlCommand::ReplaceDesired { desired, response } => {
                let result = match desired_owner.prepare(desired) {
                    Ok(desired) => runtime
                        .replace_desired(desired)
                        .await
                        .map_err(native_runtime_error),
                    Err(error) => Err(NativeError::Failure { error }),
                };
                if result.is_ok() {
                    desired_owner.accept();
                }
                let _ = response.send(result);
            }
            ControlCommand::SendTopic {
                publisher,
                payload,
                response,
            } => {
                let result = runtime
                    .send_topic(agent_core::TopicSend {
                        publisher: publisher.into(),
                        payload,
                    })
                    .await
                    .map_err(native_runtime_error);
                let _ = response.send(result);
            }
            ControlCommand::SendFrame {
                audio,
                slot,
                encoding,
                frame,
                response,
            } => {
                let result = if encoding.as_ref().is_some_and(String::is_empty) {
                    Err(invalid_configuration("media encoding name cannot be empty"))
                } else {
                    match runtime_frame(frame) {
                        Ok(frame) => {
                            let sender = if audio {
                                runtime.local_audio(slot)
                            } else {
                                runtime.local_media(slot)
                            };
                            sender
                                .send_encoding(encoding, frame)
                                .await
                                .map_err(native_runtime_error)
                        }
                        Err(error) => Err(error),
                    }
                };
                let _ = response.send(result);
            }
            ControlCommand::Observe {
                audio,
                slot,
                response,
            } => {
                let result = if audio {
                    runtime.remote_audio(slot).await
                } else {
                    runtime.remote_video(slot).await
                }
                .map_err(native_runtime_error);
                let _ = response.send(result);
            }
            ControlCommand::Reconnect { response } => {
                let _ = response.send(runtime.reconnect().await.map_err(native_runtime_error));
            }
            ControlCommand::Close { response } => match runtime.begin_close().await {
                Ok(completion) => {
                    tokio::spawn(async move {
                        let result = completion
                            .await
                            .map_err(|_| closed_error())
                            .and_then(|result| result.map_err(native_runtime_error));
                        let _ = response.send(result);
                    });
                }
                Err(error) => {
                    let _ = response.send(Err(native_runtime_error(error)));
                }
            },
            ControlCommand::Abort { response } => {
                let _ = response.send(runtime.abort().await.map_err(native_runtime_error));
                break;
            }
        }
    }
}

struct DesiredOwner {
    topology: agent_core::MediaTopology,
    next_revision: u64,
}

impl DesiredOwner {
    fn prepare(
        &self,
        desired: core_ffi::DesiredState,
    ) -> Result<agent_core::DesiredState, core_ffi::AgentError> {
        desired.into_core(self.next_revision, &self.topology)
    }

    fn accept(&mut self) {
        debug_assert_ne!(
            self.next_revision,
            u64::MAX,
            "native desired revision space exhausted"
        );
        self.next_revision = self.next_revision.saturating_add(1);
    }
}

trait StreamRequest<U> {
    fn response(self) -> oneshot::Sender<U>;
}

impl StreamRequest<SnapshotUpdate> for SnapshotRequest {
    fn response(self) -> oneshot::Sender<SnapshotUpdate> {
        match self {
            Self::Next(response) => response,
        }
    }
}

impl StreamRequest<EventUpdate> for EventRequest {
    fn response(self) -> oneshot::Sender<EventUpdate> {
        match self {
            Self::Next(response) => response,
        }
    }
}

async fn run_broadcast_stream<T, R, U>(
    mut source: broadcast::Receiver<T>,
    mut requests: mpsc::Receiver<R>,
    initial: Vec<T>,
    item: fn(T) -> U,
    lagged: fn(u64) -> U,
    closed_update: fn() -> U,
    sequence: Option<fn(&T) -> u64>,
) where
    T: Clone + Send + 'static,
    R: StreamRequest<U> + Send + 'static,
    U: Send + 'static,
{
    let mut buffered = VecDeque::new();
    let mut pending = VecDeque::new();
    let mut skipped = 0u64;
    for value in initial {
        push_bounded(&mut buffered, &mut skipped, value);
    }
    let mut closed = false;
    let mut last_sequence = sequence.and_then(|value| buffered.iter().map(value).max());
    loop {
        flush_updates(
            &mut pending,
            &mut buffered,
            &mut skipped,
            closed,
            item,
            lagged,
            closed_update,
        );
        if closed {
            let Some(request) = requests.recv().await else {
                return;
            };
            pending.push_back(request.response());
            continue;
        }
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return };
                pending.push_back(request.response());
            }
            received = source.recv() => match received {
                Ok(value) => {
                    let next_sequence = sequence.map(|sequence| sequence(&value));
                    if next_sequence.zip(last_sequence).is_none_or(|(next, last)| next > last) {
                        last_sequence = next_sequence.or(last_sequence);
                        push_bounded(&mut buffered, &mut skipped, value);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    skipped = skipped.saturating_add(count);
                }
                Err(broadcast::error::RecvError::Closed) => closed = true,
            }
        }
    }
}

fn flush_updates<T, U>(
    pending: &mut VecDeque<oneshot::Sender<U>>,
    buffered: &mut VecDeque<T>,
    skipped: &mut u64,
    closed: bool,
    item: fn(T) -> U,
    lagged: fn(u64) -> U,
    closed_update: fn() -> U,
) {
    while let Some(response) = pending.pop_front() {
        if response.is_closed() {
            continue;
        }
        let update = if *skipped > 0 {
            let count = std::mem::take(skipped);
            lagged(count)
        } else if let Some(value) = buffered.pop_front() {
            item(value)
        } else if closed {
            closed_update()
        } else {
            pending.push_front(response);
            return;
        };
        let _ = response.send(update);
    }
}

async fn run_remote_media(mut media: RemoteMedia, mut requests: mpsc::Receiver<MediaRequest>) {
    let mut buffered = VecDeque::new();
    let mut pending = VecDeque::new();
    let mut skipped = 0u64;
    let mut closed = false;
    loop {
        flush_updates(
            &mut pending,
            &mut buffered,
            &mut skipped,
            closed,
            |frame| MediaUpdate::Frame {
                frame: portable_frame(frame),
            },
            |skipped| MediaUpdate::Lagged { skipped },
            || MediaUpdate::Closed,
        );
        if closed {
            let Some(MediaRequest::Next(response)) = requests.recv().await else {
                return;
            };
            pending.push_back(response);
            continue;
        }
        tokio::select! {
            request = requests.recv() => {
                let Some(MediaRequest::Next(response)) = request else { return };
                pending.push_back(response);
            }
            frame = media.recv_frame() => match frame {
                Ok(frame) => push_bounded(&mut buffered, &mut skipped, frame),
                Err(_) => closed = true,
            }
        }
    }
}

fn push_bounded<T>(buffered: &mut VecDeque<T>, skipped: &mut u64, value: T) {
    if buffered.len() == OBSERVATION_CAPACITY {
        let _ = buffered.pop_front();
        *skipped = skipped.saturating_add(1);
    }
    buffered.push_back(value);
}

async fn request<T>(
    commands: &mpsc::Sender<ControlCommand>,
    command: impl FnOnce(oneshot::Sender<Result<T, NativeError>>) -> ControlCommand,
) -> Result<T, NativeError> {
    let (response, result) = oneshot::channel();
    commands
        .send(command(response))
        .await
        .map_err(|_| closed_error())?;
    result.await.map_err(|_| closed_error())?
}

fn runtime_config(config: NativeAgentConfig) -> Result<Config, NativeError> {
    let tcp_server = config
        .tcp_server
        .as_deref()
        .map(|value| parse_address(value, "TCP server"))
        .transpose()?;
    let session = config
        .session
        .into_core()
        .map_err(|error| NativeError::Failure { error })?;
    let topology = session.topology.clone();
    let mut video_encodings = BTreeMap::new();
    let mut video_temporal_layers = BTreeMap::new();
    for encoding in config.video_encodings {
        if !topology.local_video.contains(&encoding.slot) {
            return Err(invalid_configuration(format!(
                "video encoding references unknown slot {:?}",
                encoding.slot
            )));
        }
        if encoding.layers.iter().any(String::is_empty) {
            return Err(invalid_configuration(
                "simulcast layer names cannot be empty",
            ));
        }
        let unique_layers: BTreeSet<_> = encoding.layers.iter().collect();
        if unique_layers.len() != encoding.layers.len() {
            return Err(invalid_configuration(
                "simulcast layer names must be unique within a slot",
            ));
        }
        let layers = encoding
            .layers
            .iter()
            .map(|layer| SimulcastLayer::new(layer))
            .collect();
        if video_encodings
            .insert(encoding.slot.clone(), layers)
            .is_some()
        {
            return Err(invalid_configuration(format!(
                "video slot {:?} has duplicate encoding configuration",
                encoding.slot
            )));
        }
        if let Some(count) = encoding.temporal_layers {
            if !(1..=8).contains(&count) {
                return Err(invalid_configuration(
                    "temporal layer count must be between 1 and 8",
                ));
            }
            video_temporal_layers.insert(encoding.slot, count);
        }
    }
    let mut runtime = Config::new(session);
    runtime.tcp_server = tcp_server;
    runtime.video_encodings = video_encodings;
    runtime.video_temporal_layers = video_temporal_layers;
    runtime.dependency_descriptor = config.dependency_descriptor;
    Ok(runtime)
}

fn runtime_frame(frame: core_ffi::MediaFrame) -> Result<RuntimeFrame, NativeError> {
    if frame.data.is_empty() || frame.data.len() > MAX_ENCODED_FRAME_BYTES {
        return Err(invalid_configuration(format!(
            "encoded frame size must be between 1 and {MAX_ENCODED_FRAME_BYTES} bytes"
        )));
    }
    let frequency = Frequency::new(frame.clock_rate)
        .ok_or_else(|| invalid_configuration("encoded frame clock rate must be non-zero"))?;
    if frame
        .audio_level_dbov
        .is_some_and(|level| !(-127..=0).contains(&level))
    {
        return Err(invalid_configuration(
            "encoded audio level must be between -127 and 0 dBov",
        ));
    }
    if frame.voice_activity.is_some() && frame.audio_level_dbov.is_none() {
        return Err(invalid_configuration(
            "encoded voice activity requires an audio level",
        ));
    }
    let resolution = match (frame.width, frame.height, frame.frames_per_second) {
        (None, None, None) => None,
        (Some(width), Some(height), Some(frames_per_second))
            if width > 0 && height > 0 && frames_per_second > 0 =>
        {
            Some((width, height, frames_per_second))
        }
        _ => {
            return Err(invalid_configuration(
                "encoded frame resolution requires positive width, height, and frame rate",
            ));
        }
    };
    let dependency_descriptor = frame
        .dependency_descriptor
        .map(|bytes| {
            if !(MANDATORY_LEN..=MAX_DD_LEN).contains(&bytes.len()) {
                return Err(invalid_configuration(format!(
                    "dependency descriptor must contain between {MANDATORY_LEN} and {MAX_DD_LEN} bytes"
                )));
            }
            Ok(RawDependencyDescriptor(bytes.into_iter().collect()))
        })
        .transpose()?;
    if frame
        .temporal_layers
        .is_some_and(|count| !(1..=8).contains(&count))
    {
        return Err(invalid_configuration(
            "temporal layer count must be between 1 and 8",
        ));
    }
    let abs_capture_time = frame
        .absolute_capture_time_unix_us
        .map(|micros| {
            UNIX_EPOCH
                .checked_add(Duration::from_micros(micros))
                .ok_or_else(|| invalid_configuration("absolute capture time is out of range"))
        })
        .transpose()?;
    Ok(RuntimeFrame {
        ts: MediaTime::new(frame.timestamp, frequency),
        data: Arc::from(frame.data),
        capture_time: Instant::now(),
        abs_capture_time,
        contiguous: frame.contiguous,
        is_keyframe: frame.keyframe,
        audio_level: frame.audio_level_dbov,
        voice_activity: frame.voice_activity,
        target_bitrate_bps: frame.target_bitrate_bps,
        resolution,
        dependency_descriptor,
        temporal_layers: frame.temporal_layers,
    })
}

fn portable_frame(frame: RuntimeFrame) -> core_ffi::MediaFrame {
    let (width, height, frames_per_second) = frame
        .resolution
        .map_or((None, None, None), |(width, height, fps)| {
            (Some(width), Some(height), Some(fps))
        });
    core_ffi::MediaFrame {
        timestamp: frame.ts.numer(),
        clock_rate: frame.ts.denom(),
        data: frame.data.as_ref().to_vec(),
        absolute_capture_time_unix_us: frame.abs_capture_time.and_then(system_time_unix_micros),
        contiguous: frame.contiguous,
        keyframe: frame.is_keyframe,
        audio_level_dbov: frame.audio_level,
        voice_activity: frame.voice_activity,
        target_bitrate_bps: frame.target_bitrate_bps,
        width,
        height,
        frames_per_second,
        dependency_descriptor: frame
            .dependency_descriptor
            .map(|descriptor| descriptor.0.into_iter().collect()),
        temporal_layers: frame.temporal_layers,
    }
}

fn native_event(event: RuntimeEvent) -> NativeEvent {
    match event {
        RuntimeEvent::Core(notification) => NativeEvent::Core {
            notification: notification.into(),
        },
        RuntimeEvent::KeyframeRequested { slot, encoding } => {
            NativeEvent::KeyframeRequested { slot, encoding }
        }
        RuntimeEvent::RuntimeFailed(message) => NativeEvent::RuntimeFailed { message },
    }
}

fn runtime_statistics(value: &RuntimeStatistics) -> core_ffi::TransportStatistics {
    core_ffi::TransportStatistics {
        bytes_sent: value.bytes_sent,
        bytes_received: value.bytes_received,
        round_trip_time_ms: value
            .round_trip_time
            .map(|duration| duration.as_secs_f64() * 1_000.0),
        receive_loss: value.receive_loss,
        keyframe_requests: value.keyframe_requests,
        received_packets: value.received_packets,
        sent_packets: value.sent_packets,
        unroutable_media_dropped: value.unroutable_media_dropped,
    }
}

fn parse_address(value: &str, label: &str) -> Result<SocketAddr, NativeError> {
    value.parse().map_err(|_| {
        invalid_configuration(format!("{label} must be an IP address and port: {value:?}"))
    })
}

fn system_time_unix_micros(value: SystemTime) -> Option<u64> {
    let micros = value.duration_since(UNIX_EPOCH).ok()?.as_micros();
    u64::try_from(micros).ok()
}

fn native_runtime_error(error: RuntimeError) -> NativeError {
    let error = match error {
        RuntimeError::Core(error) => error.into(),
        RuntimeError::Closed => core_ffi::AgentError {
            code: core_ffi::ErrorCode::Closed,
            message: error.to_string(),
        },
        error => core_ffi::AgentError {
            code: core_ffi::ErrorCode::Runtime,
            message: error.to_string(),
        },
    };
    NativeError::Failure { error }
}

fn invalid_configuration(message: impl Into<String>) -> NativeError {
    NativeError::Failure {
        error: core_ffi::AgentError {
            code: core_ffi::ErrorCode::InvalidConfiguration,
            message: message.into(),
        },
    }
}

fn closed_error() -> NativeError {
    NativeError::Failure {
        error: core_ffi::AgentError {
            code: core_ffi::ErrorCode::Closed,
            message: "native agent is closed".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portable_config() -> core_ffi::AgentConfig {
        core_ffi::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            room_id: "room".into(),
            request_headers: Vec::new(),
            topology: core_ffi::MediaTopology {
                local_video: vec!["camera".into()],
                local_audio: vec!["microphone".into()],
                remote_video: 1,
                remote_audio: 1,
            },
            manual_subscriptions: true,
            retry: core_ffi::RetryPolicy::default(),
        }
    }

    #[test]
    fn native_config_rejects_unknown_encoding_slots() {
        let error = runtime_config(NativeAgentConfig {
            session: portable_config(),
            udp_bind_address: None,
            tcp_server: None,
            video_encodings: vec![VideoEncoding {
                slot: "unknown".into(),
                layers: vec!["high".into()],
                temporal_layers: None,
            }],
            dependency_descriptor: true,
        })
        .unwrap_err();

        let NativeError::Failure { error } = error;
        assert_eq!(error.code, core_ffi::ErrorCode::InvalidConfiguration);
    }

    #[test]
    fn encoded_frame_round_trips_metadata_and_bytes() {
        let portable = core_ffi::MediaFrame {
            timestamp: 4_500,
            clock_rate: 90_000,
            data: vec![1, 2, 3],
            absolute_capture_time_unix_us: Some(123_456),
            contiguous: true,
            keyframe: true,
            audio_level_dbov: None,
            voice_activity: None,
            target_bitrate_bps: Some(800_000),
            width: Some(640),
            height: Some(360),
            frames_per_second: Some(30),
            dependency_descriptor: Some(vec![0x80, 0, 1]),
            temporal_layers: Some(2),
        };

        assert_eq!(
            portable_frame(runtime_frame(portable.clone()).unwrap()),
            portable
        );
    }

    #[test]
    fn encoded_frame_rejects_malformed_metadata() {
        let valid = core_ffi::MediaFrame {
            timestamp: 0,
            clock_rate: 48_000,
            data: vec![1],
            absolute_capture_time_unix_us: None,
            contiguous: true,
            keyframe: false,
            audio_level_dbov: None,
            voice_activity: None,
            target_bitrate_bps: None,
            width: None,
            height: None,
            frames_per_second: None,
            dependency_descriptor: None,
            temporal_layers: None,
        };

        let mut frame = valid.clone();
        frame.dependency_descriptor = Some(vec![1, 2]);
        assert!(matches!(
            runtime_frame(frame),
            Err(NativeError::Failure {
                error: core_ffi::AgentError {
                    code: core_ffi::ErrorCode::InvalidConfiguration,
                    ..
                }
            })
        ));

        let mut frame = valid.clone();
        frame.audio_level_dbov = Some(1);
        assert!(runtime_frame(frame).is_err());

        let mut frame = valid;
        frame.voice_activity = Some(true);
        assert!(runtime_frame(frame).is_err());
    }

    #[tokio::test]
    async fn cancelled_snapshot_next_does_not_block_later_observation() {
        let (snapshots_tx, snapshots_rx) = broadcast::channel(2);
        let snapshots = SnapshotStream::spawn(agent_core::Snapshot::default(), snapshots_rx);
        let _ = snapshots.next().await;
        let pending = tokio::spawn({
            let snapshots = Arc::clone(&snapshots);
            async move { snapshots.next().await }
        });
        tokio::task::yield_now().await;
        pending.abort();

        snapshots_tx
            .send(agent_core::Snapshot {
                version: 1,
                desired_revision: 1,
                ..agent_core::Snapshot::default()
            })
            .unwrap();
        let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
            panic!("snapshot stream closed")
        };
        assert_eq!(snapshot.desired_revision, 1);
    }

    #[test]
    fn desired_owner_assigns_revisions_only_after_acceptance() {
        let topology = portable_config().into_core().unwrap().topology;
        let mut owner = DesiredOwner {
            topology,
            next_revision: 4,
        };

        assert_eq!(
            owner
                .prepare(core_ffi::DesiredState::default())
                .unwrap()
                .revision,
            4
        );
        assert_eq!(
            owner
                .prepare(core_ffi::DesiredState::default())
                .unwrap()
                .revision,
            4
        );
        owner.accept();
        assert_eq!(
            owner
                .prepare(core_ffi::DesiredState::default())
                .unwrap()
                .revision,
            5
        );
    }

    #[tokio::test]
    async fn snapshot_overflow_is_explicit_and_retains_order() {
        let (_source, source) = broadcast::channel(1);
        let (requests, receiver) = mpsc::channel(OBSERVATION_CAPACITY);
        let initial = (0..40)
            .map(|version| agent_core::Snapshot {
                version,
                ..agent_core::Snapshot::default()
            })
            .collect();
        tokio::spawn(run_broadcast_stream(
            source,
            receiver,
            initial,
            |snapshot| SnapshotUpdate::Snapshot {
                snapshot: (&snapshot).into(),
            },
            |skipped| SnapshotUpdate::Lagged { skipped },
            || SnapshotUpdate::Closed,
            Some(|snapshot| snapshot.version),
        ));
        let stream = SnapshotStream { requests };

        assert_eq!(stream.next().await, SnapshotUpdate::Lagged { skipped: 8 });
        let SnapshotUpdate::Snapshot { snapshot } = stream.next().await else {
            panic!("oldest retained snapshot missing")
        };
        assert_eq!(snapshot.version, 8);
    }

    #[tokio::test]
    async fn event_stream_preserves_order_and_stops_when_dropped() {
        let (events, source) = broadcast::channel(4);
        let stream = EventStream::spawn(source);
        events
            .send(RuntimeEvent::RuntimeFailed("first".into()))
            .unwrap();
        events
            .send(RuntimeEvent::RuntimeFailed("second".into()))
            .unwrap();

        assert_eq!(
            stream.next().await,
            EventUpdate::Event {
                event: NativeEvent::RuntimeFailed {
                    message: "first".into()
                }
            }
        );
        assert_eq!(
            stream.next().await,
            EventUpdate::Event {
                event: NativeEvent::RuntimeFailed {
                    message: "second".into()
                }
            }
        );
        drop(stream);
        tokio::task::yield_now().await;
        assert_eq!(events.receiver_count(), 0);
    }
}
