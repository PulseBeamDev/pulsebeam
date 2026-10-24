use crate::provider_panic::catch_provider as catch_unwind;
use crate::{FiniteTimestamp, NtpTime, TimeValueError, Timestamp, ffi};
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, Ordering},
};

/// Both virtual methods of the pinned Clock interface. Reads and conversion
/// can run concurrently on any engine role thread. Current time must be finite
/// because native Clock's integral convenience queries require it. The SDK's
/// consumers assume an advancing clock; choose an epoch and progress suitable
/// for the configured native consumers. Negative finite values remain available
/// without imposing a process-clock or NTP-era-zero restriction.
///
/// NTP conversion receives the exact native timestamp state, including infinity
/// when explicitly passed to the converter. Return all NTP bits, including zero
/// for invalid/unavailable absolute time. Callbacks must not synchronously wait
/// for or reenter their owning actor. Destructors must not panic.
pub trait ClockProvider: Send + Sync + 'static {
    fn current_time(&self) -> FiniteTimestamp;
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime;
}
impl<T: ClockProvider + ?Sized> ClockProvider for Arc<T> {
    fn current_time(&self) -> FiniteTimestamp {
        (**self).current_time()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        (**self).convert_timestamp_to_ntp_time(timestamp)
    }
}

/// Inclusive bounds on every current-time result from a retaining clock.
/// Bounds are enforced by the selected adapter, not a promise required of a
/// safe caller. They do not constrain timestamp-to-NTP conversion inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ClockTimeBounds {
    minimum: FiniteTimestamp,
    maximum: FiniteTimestamp,
}
impl ClockTimeBounds {
    pub fn new(minimum: FiniteTimestamp, maximum: FiniteTimestamp) -> Result<Self, TimeValueError> {
        if minimum > maximum {
            return Err(TimeValueError::OutOfRange);
        }
        Ok(Self { minimum, maximum })
    }
    /// Existing unrestricted clocks retain every finite native timestamp.
    pub fn all_finite() -> Self {
        Self {
            minimum: FiniteTimestamp::from_microseconds(i64::MIN + 1).expect("finite lower bound"),
            maximum: FiniteTimestamp::from_microseconds(i64::MAX - 1).expect("finite upper bound"),
        }
    }
    pub const fn minimum(self) -> FiniteTimestamp {
        self.minimum
    }
    pub const fn maximum(self) -> FiniteTimestamp {
        self.maximum
    }
    pub const fn contains(self, timestamp: FiniteTimestamp) -> bool {
        timestamp.microseconds() >= self.minimum.microseconds()
            && timestamp.microseconds() <= self.maximum.microseconds()
    }
    // The pinned real-time singleton divides signed int64 nanoseconds by 1000.
    // TimeNanos() returns int64 with or without a testing-clock hook, so this
    // bound does not need a sample or a production lease. Real-time NTP
    // calibration has separate lease requirements.
    pub(crate) fn system_time() -> Self {
        Self {
            minimum: FiniteTimestamp::from_microseconds(i64::MIN / 1000)
                .expect("finite system lower bound"),
            maximum: FiniteTimestamp::from_microseconds(i64::MAX / 1000)
                .expect("finite system upper bound"),
        }
    }
}

struct ProviderState {
    provider: Option<Box<dyn ClockProvider>>,
    bounds: ClockTimeBounds,
    failed: AtomicBool,
    last_time: AtomicI64,
    monotonic_time: Option<std::sync::Mutex<Option<i64>>>,
}
impl Drop for ProviderState {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_unwind(|| drop(provider));
    }
}
pub(crate) struct RustClockProvider(Arc<ProviderState>);

pub(crate) fn clock_provider_current_time(provider: &RustClockProvider) -> i64 {
    let state = &provider.0;
    if state.failed.load(Ordering::Acquire) {
        return state.last_time.load(Ordering::Acquire);
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        state
            .provider
            .as_ref()
            .expect("live clock provider")
            .current_time()
    }));
    if let Some(mutex) = &state.monotonic_time {
        let mut last = mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failed.load(Ordering::Acquire) {
            return state.last_time.load(Ordering::Acquire);
        }
        if let Ok(time) = result {
            let value = time.microseconds();
            if state.bounds.contains(time) && last.is_none_or(|previous| value >= previous) {
                *last = Some(value);
                state.last_time.store(value, Ordering::Release);
                return value;
            }
        }
        state.failed.store(true, Ordering::Release);
        return state.last_time.load(Ordering::Acquire);
    }
    match result {
        Ok(time) if state.bounds.contains(time) => {
            state
                .last_time
                .store(time.microseconds(), Ordering::Release);
            time.microseconds()
        }
        Ok(_) | Err(_) => {
            state.failed.store(true, Ordering::Release);
            state.last_time.load(Ordering::Acquire)
        }
    }
}
pub(crate) fn clock_provider_convert_ntp(provider: &RustClockProvider, timestamp: i64) -> u64 {
    let state = &provider.0;
    if state.failed.load(Ordering::Acquire) {
        return 0;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        state
            .provider
            .as_ref()
            .expect("live clock provider")
            .convert_timestamp_to_ntp_time(Timestamp::from_native_microsecond_bits(timestamp))
    })) {
        Ok(value) => value.raw(),
        Err(_) => {
            let _guard = state.monotonic_time.as_ref().map(|mutex| {
                mutex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            state.failed.store(true, Ordering::Release);
            0
        }
    }
}

struct ClockState {
    native: cxx::UniquePtr<ffi::NativeClock>,
    provider: Option<Arc<ProviderState>>,
    _environment: Option<Box<crate::Environment>>,
    manual_source: Option<crate::ManualClock>,
    bounds: ClockTimeBounds,
    monotonic: bool,
}
// SAFETY: the native handle owns a shared adapter with no thread-affine state.
// It calls a Send + Sync provider, synchronized native SimulatedClock, or the
// SDK real-time singleton with a retained production lease.
// Environment references retain the adapter independently. No borrowed Clock
// pointer escapes to Rust.
unsafe impl Send for ClockState {}
unsafe impl Sync for ClockState {}

/// Owned, shareable native clock. Clones and installed environments share the
/// same provider or built-in clock; dropping it does not retire a native user.
/// A custom callback panic latches failure and suppresses provider entry. Reads
/// then return the last successful in-range time, and NTP conversion returns
/// invalid zero. Before the first success the fallback is zero, or the nearest
/// endpoint for bounds that exclude zero. Already-entered calls may finish.
#[derive(Clone)]
pub struct Clock(Arc<ClockState>);
impl Clock {
    pub(crate) fn from_environment(
        native: cxx::UniquePtr<ffi::NativeClock>,
        environment: &crate::Environment,
    ) -> Self {
        let monotonic = ffi::clock_is_monotonic(native.as_ref().expect("owned environment clock"));
        Self(Arc::new(ClockState {
            native,
            provider: environment
                .selected_clock_handle()
                .and_then(|clock| clock.0.provider.clone()),
            _environment: Some(Box::new(environment.clone())),
            manual_source: environment.manual_clock_source().cloned(),
            bounds: environment.clock_time_bounds(),
            monotonic,
        }))
    }
    pub(crate) fn manual_source(&self) -> Option<&crate::ManualClock> {
        self.0.manual_source.as_ref()
    }
    pub fn from_provider(provider: impl ClockProvider) -> Self {
        Self::from_bounded_provider(provider, ClockTimeBounds::all_finite())
    }
    /// Opt into a durable current-time range for native consumers with finite
    /// arithmetic. In-range results pass through unchanged. An out-of-range
    /// result latches has_failed(), just like a callback panic, before it can
    /// enter native code. Subsequent reads return the last in-range result.
    /// Before the first success the fallback is zero if in range, otherwise
    /// the endpoint nearest zero. Already-entered callbacks may finish, but
    /// cannot publish an out-of-range time. NTP inputs remain unrestricted.
    pub fn from_bounded_provider(provider: impl ClockProvider, bounds: ClockTimeBounds) -> Self {
        Self::provider_with_contract(provider, bounds, false)
    }
    /// Opt into nondecreasing current time as well as a durable range. A
    /// backwards result latches failure before native entry; later reads keep
    /// the last accepted time. The first successful result may be negative.
    /// Concurrent publications are serialized and cannot overwrite a failure
    /// fallback with an already-entered callback's result. NTP inputs and outputs
    /// retain the provider's independent conversion semantics.
    pub fn from_monotonic_bounded_provider(
        provider: impl ClockProvider,
        bounds: ClockTimeBounds,
    ) -> Self {
        Self::provider_with_contract(provider, bounds, true)
    }
    fn provider_with_contract(
        provider: impl ClockProvider,
        bounds: ClockTimeBounds,
        monotonic: bool,
    ) -> Self {
        let fallback = 0i64.clamp(bounds.minimum.microseconds(), bounds.maximum.microseconds());
        let state = Arc::new(ProviderState {
            provider: Some(Box::new(provider)),
            bounds,
            failed: AtomicBool::new(false),
            last_time: AtomicI64::new(fallback),
            monotonic_time: monotonic.then(|| std::sync::Mutex::new(None)),
        });
        let native = ffi::new_provider_clock(Box::new(RustClockProvider(state.clone())), monotonic);
        Self(Arc::new(ClockState {
            native,
            provider: Some(state),
            _environment: None,
            manual_source: None,
            bounds,
            monotonic,
        }))
    }
    /// Durable nondecreasing-current-time evidence for retaining consumers.
    pub fn is_monotonic(&self) -> bool {
        self.0.monotonic
    }
    /// Guaranteed across clones, concurrent calls, installed environments and
    /// failure fallbacks for as long as the native clock is retained.
    pub fn time_bounds(&self) -> ClockTimeBounds {
        self.0.bounds
    }
    /// Owned SDK real-time clock with native timestamp-to-NTP conversion.
    ///
    /// Conversion, fractional rounding and NTP era interpretation are SDK-owned.
    /// Infinity is rejected by Rust queries and becomes invalid NTP for native
    /// consumers. Custom providers keep their own conversion contract.
    ///
    /// Construction requires the SDK's ordinary valid-system-clock assumptions.
    /// The clock and every retaining environment hold a production lease, so
    /// process testing-clock installation cannot coexist with them. Existing
    /// process-static SDK calibration is preserved, not recomputed from UTC.
    pub fn real_time() -> Result<Self, RealTimeClockUnavailable> {
        let native = ffi::new_real_time_clock();
        if native.is_null() {
            return Err(RealTimeClockUnavailable);
        }
        Ok(Self(Arc::new(ClockState {
            native,
            provider: None,
            _environment: None,
            manual_source: None,
            bounds: ClockTimeBounds::system_time(),
            monotonic: true,
        })))
    }
    pub fn has_failed(&self) -> bool {
        self.0
            .provider
            .as_ref()
            .is_some_and(|provider| provider.failed.load(Ordering::Acquire))
    }
    pub fn current_time(&self) -> Timestamp {
        Timestamp::from_native_microsecond_bits(ffi::provider_clock_timestamp(self.native()))
    }
    pub fn time_in_microseconds(&self) -> i64 {
        self.current_time()
            .microseconds()
            .expect("finite native provider time")
    }
    pub fn time_in_milliseconds(&self) -> i64 {
        self.current_time()
            .millis()
            .expect("finite native provider time")
    }
    /// Native current-time conversion. Built-in clocks delegate NTP arithmetic
    /// to libwebrtc. Custom providers define their own domain, including explicit
    /// invalid NTP results.
    pub fn current_ntp_time(&self) -> Result<NtpTime, TimeValueError> {
        ntp_result(ffi::provider_clock_current_ntp(self.native()))
    }
    pub fn current_ntp_in_milliseconds(&self) -> Result<i64, TimeValueError> {
        self.current_ntp_time().map(NtpTime::millis)
    }
    /// Convert without narrowing custom provider inputs. Built-in clocks require
    /// finite timestamps and delegate every finite value to the native converter;
    /// NTP arithmetic and era interpretation remain libwebrtc-owned.
    pub fn convert_timestamp_to_ntp_time(
        &self,
        timestamp: Timestamp,
    ) -> Result<NtpTime, TimeValueError> {
        ntp_result(ffi::provider_clock_convert_ntp(
            self.native(),
            timestamp.native_microsecond_bits(),
        ))
    }
    /// Validate the pinned integral Millis constructor before calling the
    /// converter. Inputs at that constructor's strict bounds are rejected.
    pub fn convert_timestamp_to_ntp_time_in_milliseconds(
        &self,
        timestamp_ms: i64,
    ) -> Result<i64, TimeValueError> {
        Ok(self
            .convert_timestamp_to_ntp_time(Timestamp::from_millis(timestamp_ms)?)?
            .millis())
    }
    pub(crate) fn native(&self) -> &ffi::NativeClock {
        self.0.native.as_ref().expect("owned native clock")
    }
}

/// A real-time clock cannot be acquired while process testing time is active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealTimeClockUnavailable;
impl std::fmt::Display for RealTimeClockUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("real-time clock is unavailable while process testing time is active")
    }
}
impl std::error::Error for RealTimeClockUnavailable {}

fn ntp_result(value: ffi::FfiClockNtpValue) -> Result<NtpTime, TimeValueError> {
    match value.error {
        0 => Ok(NtpTime::from_raw(value.value)),
        1 => Err(TimeValueError::InfiniteValue),
        _ => Err(TimeValueError::OutOfRange),
    }
}

/// Owned SDK SimulatedClock, distinct from the bounded controlled-world
/// ManualClock. The unrestricted constructor accepts every finite signed
/// timestamp, and signed advances retain native atomic wrapping. Only advances
/// landing on an unreadable native Timestamp sentinel are rejected by default.
/// The opt-in bounded constructor also rejects results outside its durable
/// range. A rejected advance never changes time.
///
/// Clones, generic clock views and installed environments share the same native
/// state. Reads and advances are serialized to enforce the sentinel guard even
/// when application threads advance concurrently. This does not install the
/// controlled world's process clock, hooks, or cooperative scheduler.
#[derive(Clone)]
pub struct SimulatedClock(Clock);
impl SimulatedClock {
    pub fn new(initial: Timestamp) -> Result<Self, TimeValueError> {
        Self::bounded(initial, ClockTimeBounds::all_finite())
    }
    /// Add an enforced range without changing existing unrestricted clock
    /// behavior. A wrapped advance outside the range fails without changing
    /// time. The native adapter checks under the same lock as the advance.
    pub fn bounded(initial: Timestamp, bounds: ClockTimeBounds) -> Result<Self, TimeValueError> {
        let initial = initial.finite()?;
        if !bounds.contains(initial) {
            return Err(TimeValueError::OutOfRange);
        }
        let native = ffi::new_simulated_clock(
            initial.microseconds(),
            bounds.minimum.microseconds(),
            bounds.maximum.microseconds(),
        );
        if native.is_null() {
            return Err(TimeValueError::OutOfRange);
        }
        Ok(Self(Clock(Arc::new(ClockState {
            native,
            provider: None,
            _environment: None,
            manual_source: None,
            bounds,
            monotonic: false,
        }))))
    }
    pub fn from_microseconds(initial: i64) -> Result<Self, TimeValueError> {
        Self::new(Timestamp::from_microseconds(initial)?)
    }
    /// Shared read/conversion view for environment and production builders.
    /// Finite NTP inputs delegate to the SDK converter; infinite inputs return
    /// a typed error from Rust and invalid NTP to native consumers.
    pub fn clock(&self) -> &Clock {
        &self.0
    }
    pub fn advance(&self, delta: crate::TimeDelta) -> Result<(), TimeValueError> {
        let delta = delta.microseconds()?;
        if ffi::advance_simulated_clock(self.0.native(), delta) {
            Ok(())
        } else {
            Err(TimeValueError::OutOfRange)
        }
    }
    pub fn advance_microseconds(&self, delta: i64) -> Result<(), TimeValueError> {
        self.advance(crate::TimeDelta::from_microseconds(delta)?)
    }
    pub fn advance_milliseconds(&self, delta: i64) -> Result<(), TimeValueError> {
        self.advance(crate::TimeDelta::from_millis(delta)?)
    }
}
impl std::ops::Deref for SimulatedClock {
    type Target = Clock;
    fn deref(&self) -> &Clock {
        self.clock()
    }
}

#[cfg(test)]
mod real_time_tests {
    use super::*;

    #[test]
    fn real_time_mapping_matches_independent_native_samples() {
        let clock = Clock::real_time().unwrap();
        for _ in 0..32 {
            let sample = ffi::real_time_clock_native_sample(clock.native());
            let timestamp = Timestamp::from_microseconds(sample.timestamp_us).unwrap();
            assert_eq!(
                clock
                    .convert_timestamp_to_ntp_time(timestamp)
                    .unwrap()
                    .raw(),
                sample.ntp
            );
        }
    }
}
