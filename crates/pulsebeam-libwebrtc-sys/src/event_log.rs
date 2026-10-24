use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    num::NonZeroUsize,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{ffi, provider_panic::catch_provider};

/// Destination for native encoded RTC event-log bytes, not decoded events.
///
/// Calls are serialized, but may arrive on different native threads. A failed
/// write or panic permanently deactivates the destination. Implementations must
/// not reenter the session or wait for application-owner work. Drop happens only
/// after native logging has relinquished the output. Controlled destinations
/// must not introduce autonomous threads, physical sockets, or wall-clock work.
pub trait EventLogOutputSink: Send + 'static {
    fn is_active(&self) -> bool;
    fn write(&mut self, bytes: &[u8]) -> bool;
    fn flush(&mut self) {}
}

/// Native event-log output period. `None` on peer start selects the native
/// default; `Immediate` requests output as soon as events are received.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventLogOutputPeriod {
    Immediate,
    Periodic(Duration),
}

impl EventLogOutputPeriod {
    pub(crate) fn milliseconds(self) -> Result<i64, EventLogError> {
        match self {
            Self::Immediate => Ok(0),
            Self::Periodic(duration) => {
                let milliseconds = i64::try_from(duration.as_millis())
                    .map_err(|_| EventLogError::InvalidOutputPeriod)?;
                if milliseconds == 0 || duration.subsec_nanos() % 1_000_000 != 0 {
                    return Err(EventLogError::InvalidOutputPeriod);
                }
                Ok(milliseconds)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventLogError {
    InvalidOutputPeriod,
    InvalidFileName,
    InvalidTimestamp,
    UnsupportedEventKind,
    InvalidEventEnum,
    InvalidRate,
    InvalidRtpPacket,
    InvalidRtpExtensionId,
    InvalidDependencyStructure,
    IncompleteEventData,
    InactiveOutput,
    NativeRejected,
    Closed,
    StopPending,
    Busy,
    WrongSequence,
    MissingRuntimeAuthority,
    UnqualifiedEnvironment,
    ProviderFailed,
}

impl fmt::Display for EventLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidOutputPeriod => "event-log period must be whole positive milliseconds within i64 range",
            Self::InvalidFileName => "event-log filename must be UTF-8 and contain no NUL",
            Self::InvalidTimestamp => "event timestamp must be a finite native microsecond value",
            Self::UnsupportedEventKind => "native event kind does not support this construction or copy operation",
            Self::InvalidEventEnum => "event enum value is not a native domain value",
            Self::InvalidRate => "finite event data rate must be nonnegative and below i64::MAX",
            Self::InvalidDependencyStructure => "dependency structure violates native count, template, or resolution index preconditions",
            Self::InvalidRtpPacket => "native RTP packet parsing failed",
            Self::InvalidRtpExtensionId => "RTP extension ID must be between 1 and 255",
            Self::IncompleteEventData => "this event snapshot requires additional constructor inputs",
            Self::InactiveOutput => "event-log output is inactive or could not be opened",
            Self::NativeRejected => "native event logger rejected the output",
            Self::Closed => "peer, session or logger is closed",
            Self::StopPending => "native event-log stop has not completed",
            Self::Busy => "event logger is executing a callback or draining deferred native events",
            Self::WrongSequence => "event-log operation requires its creator sequence",
            Self::MissingRuntimeAuthority => "stock event logging requires a creator retirement scope or exclusive threaded execution",
            Self::UnqualifiedEnvironment => "event-log creation requires known task-queue execution semantics",
            Self::ProviderFailed => "the selected event-log logger or factory failed",
        })
    }
}

impl std::error::Error for EventLogError {}

/// An owned output destination, consumed when native logging starts.
///
/// It may be moved into an actor, but does not allow shared native calls.
/// Failure to start logging still releases the consumed output. File output is
/// explicitly requested and is never created by an ordinary peer constructor.
pub struct EventLogOutput {
    native: cxx::UniquePtr<ffi::NativeEventLogOutput>,
    _exclusive: PhantomData<Cell<()>>,
}

// SAFETY: output adapters serialize Rust callbacks and require Send sinks.
// Native file outputs own their file, with no construction-thread affinity.
// This unique handle exposes no shared operations or native pointer access.
unsafe impl Send for EventLogOutput {}

impl EventLogOutput {
    pub fn from_sink(sink: impl EventLogOutputSink) -> Self {
        Self {
            native: ffi::event_log_output_from_sink(Box::new(RustEventLogOutput {
                state: Mutex::new(OutputState {
                    sink: Some(Box::new(sink)),
                    failed: false,
                }),
            })),
            _exclusive: PhantomData,
        }
    }

    /// Use the pinned engine's file output and size-limit semantics. `None`
    /// means unlimited output; a nonzero limit is checked by native writes.
    pub fn file(
        path: impl AsRef<Path>,
        maximum_bytes: Option<NonZeroUsize>,
    ) -> Result<Self, EventLogError> {
        let path = path
            .as_ref()
            .to_str()
            .ok_or(EventLogError::InvalidFileName)?;
        if path.as_bytes().contains(&0) {
            return Err(EventLogError::InvalidFileName);
        }
        let native = ffi::event_log_output_file(path, maximum_bytes.map_or(0, NonZeroUsize::get));
        if native.is_null() {
            return Err(EventLogError::InactiveOutput);
        }
        Ok(Self {
            native,
            _exclusive: PhantomData,
        })
    }

    /// Inspect native activity. A consumed or failed native output is inactive.
    pub fn is_active(&self) -> bool {
        self.native
            .as_ref()
            .is_some_and(ffi::event_log_output_native_active)
    }

    /// Forward bytes through the actual native output, including native file
    /// size-limit handling and Rust sink failure containment.
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        if self.native.is_null() {
            return false;
        }
        ffi::event_log_output_native_write(self.native.pin_mut(), bytes)
    }

    pub fn flush(&mut self) {
        if !self.native.is_null() {
            ffi::event_log_output_native_flush(self.native.pin_mut());
        }
    }

    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeEventLogOutput>) -> Self {
        Self {
            native,
            _exclusive: PhantomData,
        }
    }

    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeEventLogOutput> {
        self.native
    }
}

/// Ordinary event kinds from the pinned `RtcEvent::Type` declaration. Unknown
/// values remain intact, including test-only kinds not instantiated by peers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcEventKind {
    AlrState,
    RouteChange,
    RemoteEstimate,
    AudioNetworkAdaptation,
    AudioPlayout,
    AudioReceiveStreamConfig,
    AudioSendStreamConfig,
    BweUpdateDelayBased,
    BweUpdateLossBased,
    BweUpdateScream,
    DtlsTransportState,
    DtlsWritableState,
    IceCandidatePairConfig,
    IceCandidatePair,
    ProbeClusterCreated,
    ProbeResultFailure,
    ProbeResultSuccess,
    RtcpPacketIncoming,
    RtcpPacketOutgoing,
    RtpPacketIncoming,
    RtpPacketOutgoing,
    VideoReceiveStreamConfig,
    VideoSendStreamConfig,
    FrameDecoded,
    NetEqSetMinimumDelay,
    BeginLog,
    EndLog,
    Unknown(u32),
}

/// Exclusive, owned native event, with no peer or factory-graph dependency.
/// Native packet/header/extension ownership is retained, not reconstructed from
/// a lossy serialization.
pub struct RtcEvent {
    native: cxx::UniquePtr<ffi::NativeRtcEvent>,
}

// SAFETY: all concrete native events own immutable values and buffers. Reads
// share only these immutable values; timestamp mutation requires unique access.
unsafe impl Send for RtcEvent {}
unsafe impl Sync for RtcEvent {}

impl RtcEvent {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtcEvent>) -> Self {
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtcEvent {
        self.native.as_ref().expect("owned native event")
    }
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeRtcEvent> {
        self.native
    }

    /// Use the pinned concrete class's public `Copy` or copy constructor,
    /// preserving native ownership. Begin/end and unknown event classes have
    /// no public copy operation and return `UnsupportedEventKind`.
    pub fn try_clone(&self) -> Result<Self, EventLogError> {
        let native = ffi::rtc_event_copy(self.native());
        if native.is_null() {
            Err(EventLogError::UnsupportedEventKind)
        } else {
            Ok(Self { native })
        }
    }

    pub fn kind(&self) -> RtcEventKind {
        match ffi::rtc_event_kind(self.native()) {
            0 => RtcEventKind::AlrState,
            1 => RtcEventKind::RouteChange,
            2 => RtcEventKind::RemoteEstimate,
            3 => RtcEventKind::AudioNetworkAdaptation,
            4 => RtcEventKind::AudioPlayout,
            5 => RtcEventKind::AudioReceiveStreamConfig,
            6 => RtcEventKind::AudioSendStreamConfig,
            7 => RtcEventKind::BweUpdateDelayBased,
            8 => RtcEventKind::BweUpdateLossBased,
            9 => RtcEventKind::BweUpdateScream,
            10 => RtcEventKind::DtlsTransportState,
            11 => RtcEventKind::DtlsWritableState,
            12 => RtcEventKind::IceCandidatePairConfig,
            13 => RtcEventKind::IceCandidatePair,
            14 => RtcEventKind::ProbeClusterCreated,
            15 => RtcEventKind::ProbeResultFailure,
            16 => RtcEventKind::ProbeResultSuccess,
            17 => RtcEventKind::RtcpPacketIncoming,
            18 => RtcEventKind::RtcpPacketOutgoing,
            19 => RtcEventKind::RtpPacketIncoming,
            20 => RtcEventKind::RtpPacketOutgoing,
            21 => RtcEventKind::VideoReceiveStreamConfig,
            22 => RtcEventKind::VideoSendStreamConfig,
            23 => RtcEventKind::FrameDecoded,
            24 => RtcEventKind::NetEqSetMinimumDelay,
            0x2501580 => RtcEventKind::BeginLog,
            0x2501581 => RtcEventKind::EndLog,
            other => RtcEventKind::Unknown(other),
        }
    }
    /// Copy the event's scalar/configuration/byte payload accessors. RTP values
    /// include raw header bytes, not decoded extension values. The original
    /// native event remains owned here.
    pub fn data(&self) -> crate::RtcEventData {
        crate::RtcEventData::from_native(self.kind(), ffi::rtc_event_payload(self.native()))
    }

    pub fn timestamp_us(&self) -> i64 {
        ffi::rtc_event_timestamp_us(self.native())
    }
    pub fn timestamp_ms(&self) -> i64 {
        self.timestamp_us() / 1000
    }
    pub fn is_config_event(&self) -> bool {
        ffi::rtc_event_is_config(self.native())
    }
    pub fn group_key(&self) -> u32 {
        ffi::rtc_event_group_key(self.native())
    }
    pub fn set_timestamp_us(&mut self, timestamp_us: i64) -> Result<(), EventLogError> {
        if timestamp_us == i64::MIN || timestamp_us == i64::MAX {
            return Err(EventLogError::InvalidTimestamp);
        }
        ffi::rtc_event_set_timestamp(self.native.pin_mut(), timestamp_us);
        Ok(())
    }
}

/// A retained, read-only native environment value for provider construction.
/// This owns the native leaf, including clocks and task-queue factory values,
/// and does not import a local factory or peer into the application actor.
#[derive(Clone)]
pub struct EventLogEnvironment {
    environment: crate::Environment,
}
impl EventLogEnvironment {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeEnvironment>,
        retention: Option<&crate::Environment>,
    ) -> Self {
        Self {
            environment: crate::Environment::from_sdk(native, retention),
        }
    }

    /// The exact SDK creation environment, including identity-matched
    /// scheduling capabilities and the installation's Rust-only leases.
    pub fn environment(&self) -> &crate::Environment {
        &self.environment
    }

    pub fn time_us(&self) -> i64 {
        ffi::environment_time_us(self.environment.native())
    }
    /// Exact native CurrentTime, independent of the legacy microsecond query.
    pub fn timestamp(&self) -> crate::Timestamp {
        self.environment.timestamp()
    }
    pub fn field_trials(&self) -> crate::FieldTrialsSnapshot {
        self.environment.field_trials()
    }
}

/// Exactly-once native async-stop completion. Call `complete` only after all
/// writes cease and the output is released. Drop also completes, preventing a
/// panic or cancellation from leaving native teardown waiting indefinitely.
/// This has no timeout and must not be retained forever. In controlled use,
/// completion must run on caller-controlled work, not an autonomous thread.
pub struct EventLogStopCompletion {
    native: cxx::UniquePtr<ffi::NativeEventLogStop>,
}
// SAFETY: native async completion is invoked once, exclusively, by its owner;
// RtcEventLog's async contract permits completion on an output execution queue.
unsafe impl Send for EventLogStopCompletion {}
impl EventLogStopCompletion {
    pub fn complete(self) {
        ffi::event_log_stop_complete(self.native);
    }
}

/// Thread-safe, retained implementation of native RTC event logging.
///
/// Calls can occur on multiple native threads. `stop` must wait for output
/// release; async stop must retain its completion until release. Reentering the
/// actor is forbidden. A callback panic deactivates future start/log calls;
/// cleanup callbacks are still attempted and no panic crosses the native ABI.
/// Controlled registrations must perform only caller-controlled work.
pub trait RtcEventLogger: Send + Sync + 'static {
    fn start(&self, output: EventLogOutput, period: EventLogOutputPeriod) -> bool;
    fn stop(&self);
    fn stop_async(&self, completion: EventLogStopCompletion) {
        self.stop();
        completion.complete();
    }
    fn log(&self, event: RtcEvent);
}

/// Factory called on the native worker sequence for each new peer. Concurrent
/// factories/peers may invoke it concurrently; registration and logger ownership
/// are independent of the Rust builder's lifetime. Native creation requires a
/// non-null result. A factory panic is contained and supplies a rejecting
/// native adapter, never a stock replacement. An owned environment view may
/// be retained; construction must not reenter the actor or require owner work.
pub trait RtcEventLogFactory: Send + Sync + 'static {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger>;
}

impl<T: RtcEventLogger + ?Sized> RtcEventLogger for Arc<T> {
    fn start(&self, output: EventLogOutput, period: EventLogOutputPeriod) -> bool {
        (**self).start(output, period)
    }
    fn stop(&self) {
        (**self).stop();
    }
    fn stop_async(&self, completion: EventLogStopCompletion) {
        (**self).stop_async(completion);
    }
    fn log(&self, event: RtcEvent) {
        (**self).log(event);
    }
}
impl<T: RtcEventLogger + ?Sized> RtcEventLogger for Box<T> {
    fn start(&self, output: EventLogOutput, period: EventLogOutputPeriod) -> bool {
        (**self).start(output, period)
    }
    fn stop(&self) {
        (**self).stop();
    }
    fn stop_async(&self, completion: EventLogStopCompletion) {
        (**self).stop_async(completion);
    }
    fn log(&self, event: RtcEvent) {
        (**self).log(event);
    }
}
impl<T: RtcEventLogFactory + ?Sized> RtcEventLogFactory for Arc<T> {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        (**self).create(environment)
    }
}

pub(crate) struct RustRtcEventLogFactory(
    pub Option<Arc<dyn RtcEventLogFactory>>,
    pub crate::Environment,
);
impl Drop for RustRtcEventLogFactory {
    fn drop(&mut self) {
        let provider = self.0.take();
        let _ = catch_provider(|| drop(provider));
    }
}
pub(crate) struct RustRtcEventLog {
    logger: Option<Box<dyn RtcEventLogger>>,
    failed: AtomicBool,
    // Destroy the provider before releasing its creation dependencies.
    _environment: Option<EventLogEnvironment>,
}
impl RustRtcEventLog {
    pub(crate) fn from_logger(logger: Option<Box<dyn RtcEventLogger>>) -> Box<Self> {
        Box::new(Self {
            logger,
            failed: AtomicBool::new(false),
            _environment: None,
        })
    }
}
impl Drop for RustRtcEventLog {
    fn drop(&mut self) {
        let logger = self.logger.take();
        let _ = catch_provider(|| drop(logger));
    }
}
pub(crate) fn rust_event_log_create(
    factory: &RustRtcEventLogFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
) -> Box<RustRtcEventLog> {
    let environment = EventLogEnvironment::from_native(environment, Some(&factory.1));
    let logger = catch_provider(|| {
        factory
            .0
            .as_ref()
            .map(|factory| factory.create(environment.clone()))
    })
    .ok()
    .flatten();
    let failed = logger.is_none();
    Box::new(RustRtcEventLog {
        logger,
        failed: AtomicBool::new(failed),
        _environment: Some(environment),
    })
}
pub(crate) fn rust_event_log_present(logger: &RustRtcEventLog) -> bool {
    logger.logger.is_some()
}
pub(crate) fn rust_event_log_failed(logger: &RustRtcEventLog) -> bool {
    logger.failed.load(Ordering::Acquire)
}
pub(crate) fn rust_event_log_start(
    logger: &RustRtcEventLog,
    output: cxx::UniquePtr<ffi::NativeEventLogOutput>,
    period_ms: i64,
) -> bool {
    if logger.failed.load(Ordering::Acquire) || period_ms < 0 {
        return false;
    }
    let period = if period_ms == 0 {
        EventLogOutputPeriod::Immediate
    } else {
        EventLogOutputPeriod::Periodic(Duration::from_millis(period_ms as u64))
    };
    let result = catch_provider(|| {
        logger
            .logger
            .as_ref()
            .is_some_and(|logger| logger.start(EventLogOutput::from_native(output), period))
    });
    match result {
        Ok(result) => result,
        Err(_) => {
            logger.failed.store(true, Ordering::Release);
            false
        }
    }
}
pub(crate) fn rust_event_log_stop(logger: &RustRtcEventLog) {
    if catch_provider(|| {
        if let Some(logger) = &logger.logger {
            logger.stop();
        }
    })
    .is_err()
    {
        logger.failed.store(true, Ordering::Release);
    }
}
pub(crate) fn rust_event_log_stop_async(
    logger: &RustRtcEventLog,
    completion: cxx::UniquePtr<ffi::NativeEventLogStop>,
) {
    if catch_provider(|| {
        let completion = EventLogStopCompletion { native: completion };
        if let Some(logger) = &logger.logger {
            logger.stop_async(completion);
        }
    })
    .is_err()
    {
        logger.failed.store(true, Ordering::Release);
    }
}
pub(crate) fn rust_event_log_log(
    logger: &RustRtcEventLog,
    event: cxx::UniquePtr<ffi::NativeRtcEvent>,
) {
    if logger.failed.load(Ordering::Acquire) {
        return;
    }
    if catch_provider(|| {
        if let Some(logger) = &logger.logger {
            logger.log(RtcEvent { native: event });
        }
    })
    .is_err()
    {
        logger.failed.store(true, Ordering::Release);
    }
}

struct OutputState {
    sink: Option<Box<dyn EventLogOutputSink>>,
    failed: bool,
}
impl Drop for OutputState {
    fn drop(&mut self) {
        let sink = self.sink.take();
        let _ = catch_provider(|| drop(sink));
    }
}

pub(crate) struct RustEventLogOutput {
    state: Mutex<OutputState>,
}

pub(crate) fn event_log_output_active(output: &RustEventLogOutput) -> bool {
    let mut state = output
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.failed {
        return false;
    }
    match catch_provider(|| state.sink.as_ref().is_some_and(|sink| sink.is_active())) {
        Ok(true) => true,
        _ => {
            state.failed = true;
            false
        }
    }
}

pub(crate) fn event_log_output_write(output: &RustEventLogOutput, bytes: &[u8]) -> bool {
    let mut state = output
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.failed {
        return false;
    }
    let result = catch_provider(|| {
        state
            .sink
            .as_mut()
            .is_some_and(|sink| sink.is_active() && sink.write(bytes))
    })
    .unwrap_or(false);
    state.failed = !result;
    result
}

pub(crate) fn event_log_output_flush(output: &RustEventLogOutput) {
    let mut state = output
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !state.failed
        && catch_provider(|| {
            if let Some(sink) = &mut state.sink {
                sink.flush();
            }
        })
        .is_err()
    {
        state.failed = true;
    }
}
