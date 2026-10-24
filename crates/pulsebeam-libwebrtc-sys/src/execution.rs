use std::{fmt, marker::PhantomData, rc::Rc, sync::Arc, time::Duration};

use crate::ffi;

const NO_DEADLINE: i64 = i64::MIN;
/// Largest timestamp within both WebRTC's nanosecond clock and NTP era zero.
pub const MAX_CONTROLLED_TIME: Duration =
    Duration::from_micros((u32::MAX as u64 - 2_208_988_800) * 1_000_000 + 999_999);

// Non-Send closures enter native code only through creator-thread cooperative
// queues. Threaded posting methods continue to require Send closures.
pub(crate) struct RustTask(Option<Box<dyn FnOnce() + 'static>>, bool);

// This box owns only Environment dependencies, never the Rc driver. The native
// creator table retains it until all SDK copies release their scope ticket.
pub(crate) struct RustRuntimeEnvironmentRetention {
    environment: Option<Environment>,
}
impl Drop for RustRuntimeEnvironmentRetention {
    fn drop(&mut self) {
        let environment = self.environment.take();
        let _ = crate::provider_panic::catch_provider(|| drop(environment));
    }
}
impl Drop for RustTask {
    fn drop(&mut self) {
        let task = self.0.take();
        let _ = crate::provider_panic::catch_provider(|| drop(task));
    }
}

#[allow(
    clippy::boxed_local,
    reason = "CXX transfers ownership through this fixed Box ABI."
)]
pub(crate) fn run_task(mut task: Box<RustTask>) {
    let task_context = task.1;
    if let Some(task) = task.0.take() {
        if task_context {
            let _ = crate::provider_panic::catch_provider(task);
        } else {
            let _ = crate::provider_panic::catch_task(task);
        }
    }
}

pub(crate) fn boxed_task(task: impl FnOnce() + Send + 'static) -> Box<RustTask> {
    Box::new(RustTask(Some(Box::new(task)), false))
}

// A Current() snapshot grants posting, not an owning control-plane loan. It
// may be an SDK-internal encoder queue whose deletion would wait on this task.
pub(crate) fn boxed_context_task(task: impl FnOnce() + Send + 'static) -> Box<RustTask> {
    Box::new(RustTask(Some(Box::new(task)), true))
}

fn duration_micros(duration: Duration) -> Option<i64> {
    i64::try_from(duration.as_micros()).ok()
}

/// A handle to WebRTC's process system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl SystemClock {
    pub fn now(self) -> Duration {
        Duration::from_micros(ffi::system_clock_time_us() as u64)
    }
}

/// A caller-advanced clock shared by environments and cooperative queues.
#[derive(Clone)]
pub struct ManualClock(Arc<ManualClockInner>);

struct ManualClockInner {
    native: cxx::UniquePtr<ffi::NativeManualClock>,
}

// SAFETY: pinned `webrtc::SimulatedClock` stores time atomically, and the
// repository adapter exposes only its documented concurrent read/advance API.
unsafe impl Send for ManualClockInner {}
// SAFETY: see the `Send` rationale; all exposed native operations are atomic.
unsafe impl Sync for ManualClockInner {}

impl ManualClock {
    pub fn new(initial_time: Duration) -> Result<Self, BuildEnvironmentError> {
        if initial_time > MAX_CONTROLLED_TIME {
            return Err(BuildEnvironmentError::TimestampOutOfRange);
        }
        let initial_time_us =
            duration_micros(initial_time).ok_or(BuildEnvironmentError::TimestampOutOfRange)?;
        let native = ffi::new_manual_clock(initial_time_us);
        if native.is_null() {
            return Err(BuildEnvironmentError::NativeConstructionFailed);
        }
        Ok(Self(Arc::new(ManualClockInner { native })))
    }

    pub fn now(&self) -> Duration {
        Duration::from_micros(ffi::manual_clock_time_us(self.native()) as u64)
    }

    /// Durable inclusive range enforced by native construction and advances.
    pub fn time_bounds(&self) -> crate::ClockTimeBounds {
        crate::ClockTimeBounds::new(
            crate::FiniteTimestamp::from_microseconds(0).expect("finite zero"),
            crate::FiniteTimestamp::from_microseconds(MAX_CONTROLLED_TIME.as_micros() as i64)
                .expect("finite controlled upper bound"),
        )
        .expect("ordered controlled bounds")
    }

    pub fn advance(&self, delta: Duration) -> Result<(), BuildEnvironmentError> {
        let delta_us = duration_micros(delta).ok_or(BuildEnvironmentError::TimestampOutOfRange)?;
        if ffi::advance_manual_clock(self.native(), delta_us) {
            Ok(())
        } else {
            Err(BuildEnvironmentError::TimestampOutOfRange)
        }
    }

    pub(crate) fn native(&self) -> &ffi::NativeManualClock {
        self.0.native.as_ref().expect("validated manual clock")
    }
}

/// WebRTC task queue priority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum QueuePriority {
    #[default]
    Normal = 0,
    High = 1,
    Video = 2,
    Audio = 3,
    Low = 4,
}

/// Thread-safe factory for either WebRTC's default queues or caller-driven queues.
#[derive(Clone)]
pub struct TaskQueueFactory(Arc<TaskQueueFactoryInner>);

struct TaskQueueFactoryInner {
    native: cxx::UniquePtr<ffi::NativeTaskQueueFactory>,
    clock: Option<ManualClock>,
    provider: Option<Arc<crate::task_queue_provider::FactoryState>>,
    _environment: Option<Box<Environment>>,
}

// SAFETY: `TaskQueueFactory` requires thread-safe implementations. The default
// upstream factory satisfies that contract and the cooperative adapter guards
// all shared scheduler state with a mutex. Environment delegates preserve that
// same selected factory and retain its thread-safe environment/lease ownership.
unsafe impl Send for TaskQueueFactoryInner {}
// SAFETY: see the `Send` rationale; factory methods take shared references.
unsafe impl Sync for TaskQueueFactoryInner {}

impl TaskQueueFactory {
    pub fn default_threaded() -> Result<Self, BuildEnvironmentError> {
        Self::from_native(ffi::new_default_task_queue_factory(), None, None)
    }

    pub fn cooperative(clock: &ManualClock) -> Result<Self, BuildEnvironmentError> {
        Self::from_native(
            ffi::new_cooperative_task_queue_factory(clock.native()),
            Some(clock.clone()),
            None,
        )
    }

    /// Shared by public products and engine queues in environments using this
    /// factory. This is a production runtime, not a controlled world.
    pub fn from_provider(
        provider: impl crate::TaskQueueFactoryProvider,
    ) -> Result<Self, BuildEnvironmentError> {
        let (state, bridge) = crate::task_queue_provider::FactoryState::register(provider);
        Self::from_native(
            ffi::new_provider_task_queue_factory(bridge),
            None,
            Some(state),
        )
    }

    pub fn is_provider_backed(&self) -> bool {
        self.0.provider.is_some()
    }
    pub fn has_failed(&self) -> bool {
        self.0
            .provider
            .as_ref()
            .is_some_and(|state| state.has_failed())
    }

    fn from_environment(
        environment: &Environment,
        selected: &Self,
    ) -> Result<Self, BuildEnvironmentError> {
        let native = ffi::environment_task_queue_factory(environment.native(), selected.native());
        if native.is_null() {
            return Err(if selected.is_cooperative() {
                BuildEnvironmentError::NativeConstructionFailed
            } else {
                BuildEnvironmentError::ProductionRuntimeUnavailable
            });
        }
        Ok(Self(Arc::new(TaskQueueFactoryInner {
            native,
            clock: selected.0.clock.clone(),
            provider: selected.0.provider.clone(),
            _environment: Some(Box::new(environment.clone())),
        })))
    }

    fn from_native(
        native: cxx::UniquePtr<ffi::NativeTaskQueueFactory>,
        clock: Option<ManualClock>,
        provider: Option<Arc<crate::task_queue_provider::FactoryState>>,
    ) -> Result<Self, BuildEnvironmentError> {
        if native.is_null() {
            Err(BuildEnvironmentError::NativeConstructionFailed)
        } else {
            Ok(Self(Arc::new(TaskQueueFactoryInner {
                native,
                clock,
                provider,
                _environment: None,
            })))
        }
    }

    pub fn create_queue(
        &self,
        name: &str,
        priority: QueuePriority,
    ) -> Result<TaskQueue, BuildEnvironmentError> {
        self.check_current()?;
        if name.as_bytes().contains(&0) {
            return Err(BuildEnvironmentError::InvalidQueueName);
        }
        self.create_queue_bytes(name.as_bytes(), priority)
    }

    /// Exact native name, including NUL and non-UTF-8 bytes.
    pub fn create_queue_bytes(
        &self,
        name: &[u8],
        priority: QueuePriority,
    ) -> Result<TaskQueue, BuildEnvironmentError> {
        self.check_current()?;
        let native = ffi::create_task_queue(self.native(), name, priority as u8);
        if native.is_null() {
            Err(BuildEnvironmentError::NativeConstructionFailed)
        } else {
            Ok(TaskQueue {
                native,
                factory: self.clone(),
                driver: None,
                _creator_sequence: PhantomData,
            })
        }
    }

    /// Dispatch up to 1,024 ready cooperative tasks without moving time.
    /// Returns zero for a default threaded factory or off the cooperative
    /// factory's creator thread. Work may remain ready; use `pump` for a
    /// caller-selected budget and a remaining-work snapshot.
    pub fn run_ready(&self) -> usize {
        self.pump(1_024).map_or(0, |result| result.dispatched)
    }

    /// Dispatch at most `budget` top-level tasks. Nested lifecycle yields do
    /// not count against it. A zero budget never dispatches work.
    pub fn pump(&self, budget: usize) -> Result<PumpResult, BuildEnvironmentError> {
        self.check_current()?;
        let blocked = controlled_dispatch_blocked();
        let turn = (!blocked).then(controlled_owner_turn).flatten();
        let dispatched = if blocked {
            0
        } else {
            ffi::pump_ready_tasks(self.native(), budget)
        };
        drop(turn);
        let next_deadline = self.next_deadline();
        let ready = self
            .0
            .clock
            .as_ref()
            .is_some_and(|clock| next_deadline.is_some_and(|deadline| deadline <= clock.now()));
        Ok(PumpResult {
            dispatched,
            blocked,
            ready,
            next_deadline,
        })
    }

    fn check_current(&self) -> Result<(), BuildEnvironmentError> {
        if ffi::task_queue_factory_is_current(self.native()) {
            Ok(())
        } else {
            Err(BuildEnvironmentError::WrongThread)
        }
    }

    /// Returns the next cooperative deadline, or `None` when no work is queued.
    pub fn next_deadline(&self) -> Option<Duration> {
        let deadline = ffi::next_task_deadline_us(self.native());
        (deadline != NO_DEADLINE).then(|| Duration::from_micros(deadline as u64))
    }

    pub fn is_cooperative(&self) -> bool {
        ffi::task_queue_factory_is_cooperative(self.native())
    }

    fn native(&self) -> &ffi::NativeTaskQueueFactory {
        self.0
            .native
            .as_ref()
            .expect("validated task queue factory")
    }
}

/// A sequence-bound WebRTC task queue.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::TaskQueue>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::TaskQueue>();
/// ```
pub struct TaskQueue {
    native: cxx::UniquePtr<ffi::NativeTaskQueue>,
    factory: TaskQueueFactory,
    driver: Option<ControlledPeerDriver>,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl TaskQueue {
    pub(crate) fn native_for_consumer(&self) -> Option<&ffi::NativeTaskQueue> {
        self.native.as_ref()
    }
    pub fn identity(&self) -> Option<crate::TaskQueueIdentity> {
        self.native
            .as_ref()
            .map(|native| crate::TaskQueueIdentity(ffi::task_queue_identity(native)))
    }
    pub fn name_bytes(&self) -> Vec<u8> {
        self.native
            .as_ref()
            .map_or_else(Vec::new, ffi::task_queue_name)
    }
    pub fn is_current(&self) -> bool {
        self.native.as_ref().is_some_and(ffi::task_queue_is_current)
    }
    pub fn has_failed(&self) -> bool {
        self.native.as_ref().is_some_and(ffi::task_queue_has_failed)
    }

    /// Exact native delays and both precision modes. Custom providers and SDK
    /// backends receive native values unchanged; backend arithmetic/rounding is
    /// SDK-owned. Cooperative queues require finite signed values; negatives
    /// become immediately due and out-of-domain deadlines are discarded by the
    /// caller-owned scheduler.
    /// Success indicates native posting, not an execution acknowledgement.
    /// Location is the pinned standalone SDK's empty value.
    pub fn post_delayed_native(
        &self,
        delay: crate::TimeDelta,
        precision: crate::TaskDelayPrecision,
        _location: crate::TaskQueueLocation,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<bool, BuildEnvironmentError> {
        self.factory.check_current()?;
        let Some(native) = self.native.as_ref() else {
            return Ok(false);
        };
        native_delay_result(ffi::post_native_delayed_task(
            native,
            delay.native_microsecond_bits(),
            precision == crate::TaskDelayPrecision::High,
            boxed_task(task),
        ))
    }

    /// Post caller-thread work. Reject threaded factories before retaining the
    /// closure; the native cooperative queue only runs/destroys it on this
    /// queue's creator thread. This permits cooperative self-reposting work.
    pub fn post_local(&self, task: impl FnOnce() + 'static) -> Result<bool, BuildEnvironmentError> {
        if !self.factory.is_cooperative() {
            return Err(BuildEnvironmentError::NotCooperative);
        }
        self.factory.check_current()?;
        Ok(self.native.as_ref().is_some_and(|native| {
            ffi::post_task(native, Box::new(RustTask(Some(Box::new(task)), false)))
        }))
    }

    pub fn post(&self, task: impl FnOnce() + Send + 'static) -> bool {
        self.native
            .as_ref()
            .is_some_and(|native| ffi::post_task(native, boxed_task(task)))
    }

    pub fn post_delayed(
        &self,
        delay: Duration,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<bool, BuildEnvironmentError> {
        let delay_us = duration_micros(delay).ok_or(BuildEnvironmentError::TimestampOutOfRange)?;
        if self.factory.0.clock.as_ref().is_some_and(|clock| {
            clock
                .now()
                .checked_add(delay)
                .is_none_or(|deadline| deadline > MAX_CONTROLLED_TIME)
        }) {
            return Err(BuildEnvironmentError::TimestampOutOfRange);
        }
        self.post_delayed_native(
            crate::TimeDelta::from_microseconds(delay_us)
                .map_err(|_| BuildEnvironmentError::UnsupportedNativeDelay)?,
            crate::TaskDelayPrecision::Low,
            crate::TaskQueueLocation,
            task,
        )
    }

    pub fn close(&mut self) {
        drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
    }
}

#[path = "environment_factory.rs"]
mod environment_factory;
pub use environment_factory::EnvironmentFactory;
pub(crate) use environment_factory::{
    RustEnvironmentFactoryRandomness, RustEnvironmentFactoryRetention,
    environment_factory_has_randomness, environment_factory_randomness,
};

/// A thread-safe, copyable WebRTC environment.
///
/// SDK copies from a managed runtime retain a creator-side retirement ticket,
/// not an affine driver owner. Release those copies and dependent products
/// before exiting the creator runtime's scope. Final runtime teardown drains
/// native-ready lifecycle work and waits for outstanding owning aliases; an
/// alias must not wait for that teardown while continuing to retain its ticket.
pub struct Environment {
    native: cxx::UniquePtr<ffi::NativeEnvironment>,
    _clock: Option<ManualClock>,
    _custom_clock: bool,
    _clock_handle: Option<crate::Clock>,
    clock_bounds: crate::ClockTimeBounds,
    // SDK creation contexts own queues through the native Environment itself.
    // They must not fabricate a Rust factory handle for a different selection.
    _task_queue_factory: Option<TaskQueueFactory>,
    _randomness: Option<Arc<RandomnessState>>,
}

impl Clone for Environment {
    fn clone(&self) -> Self {
        let native = self
            .native
            .as_ref()
            .map_or_else(cxx::UniquePtr::null, |native| {
                ffi::clone_environment(native)
            });
        Self {
            native,
            _clock: self._clock.clone(),
            _custom_clock: self._custom_clock,
            _clock_handle: self._clock_handle.clone(),
            clock_bounds: self.clock_bounds,
            _task_queue_factory: self._task_queue_factory.clone(),
            _randomness: self._randomness.clone(),
        }
    }
}

// SAFETY: the pinned `webrtc::Environment` contract explicitly declares the
// value thread-safe; retained dependencies also satisfy their thread contracts.
unsafe impl Send for Environment {}
// SAFETY: see the `Send` rationale; the adapter exposes shared reads only.
unsafe impl Sync for Environment {}

impl Environment {
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::default()
    }

    /// Compatibility duration query. Signed clock values require timestamp().
    /// Panics for negative or infinite time instead of silently wrapping it.
    pub fn now(&self) -> Duration {
        self.try_now()
            .expect("nonnegative environment time; use timestamp() for signed values")
    }

    pub fn try_now(&self) -> Result<Duration, crate::TimeValueError> {
        let value = self.timestamp().microseconds()?;
        let value = u64::try_from(value).map_err(|_| crate::TimeValueError::OutOfRange)?;
        Ok(Duration::from_micros(value))
    }

    /// Exact owned state of the effective native environment clock.
    pub fn timestamp(&self) -> crate::Timestamp {
        crate::Timestamp::from_native_microsecond_bits(ffi::environment_timestamp(self.native()))
    }

    /// Identity-checked evidence that the native clock is nondecreasing.
    /// Arbitrary providers, rewindable simulated clocks and the default SDK
    /// singleton without a production lease do not carry this guarantee.
    pub fn clock_is_monotonic(&self) -> bool {
        ffi::environment_clock_time_bounds(self.native()).monotonic
    }

    /// Owned copy of the actual native environment's immutable trial view.
    pub fn field_trials(&self) -> crate::FieldTrialsSnapshot {
        crate::FieldTrialsSnapshot::from_native(ffi::environment_field_trials(self.native()))
    }

    /// Conservative bounds on every current-time result, not a clock sample.
    /// Installed clock handles retain their enforced bounds in native copies.
    /// The default SDK singleton divides signed nanoseconds by 1000, including
    /// when process testing time is selected. Its range needs no clock sample.
    pub fn clock_time_bounds(&self) -> crate::ClockTimeBounds {
        self.clock_bounds
    }

    pub(crate) fn cooperative_queues(&self) -> Option<bool> {
        self._task_queue_factory
            .as_ref()
            .map(TaskQueueFactory::is_cooperative)
    }

    pub(crate) fn uses_system_clock(&self) -> bool {
        self._clock.is_none() && !self._custom_clock
    }

    /// Own a delegate to this exact SDK clock, with its environment and Rust
    /// leases retained independently. This does not echo the builder's clock.
    /// A default system clock needs a production lease for safe NTP conversion.
    pub fn clock(&self) -> Result<crate::Clock, EnvironmentClockError> {
        let environment = self.native.as_ref().ok_or(EnvironmentClockError::Closed)?;
        let selected = self
            ._clock_handle
            .as_ref()
            .map_or(std::ptr::null(), |clock| clock.native() as *const _);
        // SAFETY: the optional pointer is either null or a live selected clock
        // retained by this environment. Identity-bound metadata is propagated
        // only by constructors and from_sdk's exact native pointer comparison.
        let native =
            unsafe { ffi::environment_clock(environment, selected, self._clock.is_some()) };
        if native.is_null() {
            return Err(EnvironmentClockError::RealTimeClockUnavailable);
        }
        Ok(crate::Clock::from_environment(native, self))
    }

    /// Own the exact SDK task-queue factory and its environment. Created public
    /// queues remain caller-owned, even when the SDK factory also creates engine
    /// lifecycle queues. Cooperative operations keep the original creator sequence.
    pub fn task_queue_factory(&self) -> Result<TaskQueueFactory, BuildEnvironmentError> {
        self.native.as_ref().ok_or(BuildEnvironmentError::Closed)?;
        let selected = self
            ._task_queue_factory
            .as_ref()
            .ok_or(BuildEnvironmentError::UnqualifiedEnvironment)?;
        TaskQueueFactory::from_environment(self, selected)
    }

    /// Own the exact installed SDK event logger and its dependencies. This is
    /// an alias of the installed utility, not a newly created factory product.
    pub fn event_log(&self) -> Result<crate::RtcEventLogUtility, crate::EventLogError> {
        self.native.as_ref().ok_or(crate::EventLogError::Closed)?;
        crate::RtcEventLogUtility::from_environment(self)
    }

    pub(crate) fn manual_clock_source(&self) -> Option<&ManualClock> {
        self._clock.as_ref()
    }

    pub(crate) fn selected_clock_handle(&self) -> Option<&crate::Clock> {
        self._clock_handle.as_ref()
    }

    pub fn close(&mut self) {
        // Destroy the native environment before releasing this handle's Rust
        // dependency leases. Independent environment/product owners keep theirs.
        drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
        // Native teardown precedes the Rust capability owners it can call.
        self._task_queue_factory = None;
        self._clock = None;
        self._clock_handle = None;
        self._randomness = None;
    }

    pub(crate) fn bind_driver(
        &self,
        driver: &ControlledPeerDriver,
    ) -> Result<Self, BuildEnvironmentError> {
        let native = ffi::bind_driver_environment(
            driver.native(),
            self.native(),
            Box::new(RustRuntimeEnvironmentRetention {
                environment: Some(self.clone()),
            }),
        );
        if native.is_null() {
            return Err(BuildEnvironmentError::WrongThread);
        }
        let mut result = self.clone();
        result.native = native;
        Ok(result)
    }

    pub(crate) fn uses_controlled_clock(&self, clock: &ManualClock) -> bool {
        self._task_queue_factory.as_ref().is_some_and(|factory| {
            factory.is_cooperative()
                && factory.check_current().is_ok()
                && ffi::task_queue_factory_uses_clock(factory.native(), clock.native())
        })
    }

    /// Own the exact SDK creation argument. Native shared owners retain its
    /// dependencies; no queue, clock or field-trial provider is substituted.
    pub(crate) fn from_sdk(
        native: cxx::UniquePtr<ffi::NativeEnvironment>,
        retention: Option<&Environment>,
    ) -> Self {
        let native = match retention {
            Some(selected) => ffi::bind_environment_selection(native, selected.native()),
            None => native,
        };
        let bounds =
            ffi::environment_clock_time_bounds(native.as_ref().expect("retained SDK environment"));
        let clock_bounds = if bounds.known {
            crate::ClockTimeBounds::new(
                crate::FiniteTimestamp::from_microseconds(bounds.minimum_us)
                    .expect("finite enforced lower bound"),
                crate::FiniteTimestamp::from_microseconds(bounds.maximum_us)
                    .expect("finite enforced upper bound"),
            )
            .expect("ordered enforced clock bounds")
        } else {
            crate::ClockTimeBounds::all_finite()
        };
        let actual = native.as_ref().expect("retained SDK environment");
        let selected_clock = retention
            .filter(|selected| ffi::environment_clock_identity_matches(actual, selected.native()));
        let selected_queue = retention.filter(|selected| {
            ffi::environment_queue_factory_identity_matches(actual, selected.native())
        });
        Self {
            native,
            _clock: selected_clock.and_then(|selected| selected._clock.clone()),
            _custom_clock: selected_clock.is_none_or(|selected| selected._custom_clock),
            _clock_handle: selected_clock.and_then(|selected| selected._clock_handle.clone()),
            clock_bounds,
            _task_queue_factory: selected_queue
                .and_then(|selected| selected._task_queue_factory.clone()),
            // The process randomness lease is Rust-owned, not an SDK component.
            _randomness: retention.and_then(|environment| environment._randomness.clone()),
        }
    }

    pub(crate) fn native(&self) -> &ffi::NativeEnvironment {
        self.native.as_ref().expect("validated environment")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentClockError {
    Closed,
    RealTimeClockUnavailable,
}
impl fmt::Display for EnvironmentClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "environment is closed",
            Self::RealTimeClockUnavailable => {
                "system clock cannot retain a production lease while testing time is active"
            }
        })
    }
}
impl std::error::Error for EnvironmentClockError {}

#[derive(Default)]
pub struct EnvironmentBuilder {
    field_trials: Option<Arc<dyn crate::FieldTrialsView>>,
    event_log: Option<Box<dyn crate::RtcEventLogger>>,
    clock: Option<ManualClock>,
    custom_clock: Option<crate::Clock>,
    task_queue_factory: Option<TaskQueueFactory>,
    randomness: Option<Arc<RandomnessState>>,
}

impl EnvironmentBuilder {
    /// Install a thread-safe caller logger in the native owning environment
    /// leaf. Native environment copies retain it independently of this builder.
    /// Controlled loggers and outputs must use only caller-controlled work.
    pub fn event_log(mut self, logger: impl crate::RtcEventLogger) -> Self {
        self.event_log = Some(Box::new(logger));
        self
    }

    /// Install owned immutable values or a thread-safe caller provider. With
    /// no override, the native environment retains its default trial source.
    pub fn field_trials(mut self, view: impl crate::FieldTrialsView) -> Self {
        self.field_trials = Some(Arc::new(view));
        self
    }

    /// Select the cooperative/manual clock, replacing a custom registration.
    pub fn clock(mut self, clock: &ManualClock) -> Self {
        self.clock = Some(clock.clone());
        self.custom_clock = None;
        self
    }

    /// Install both virtual clock methods without changing process-global time.
    pub fn clock_provider(self, provider: impl crate::ClockProvider) -> Self {
        self.clock_handle(&crate::Clock::from_provider(provider))
    }

    /// Install a shared owning clock handle. A retained manual-clock utility
    /// preserves its scheduler identity; other custom clocks cannot drive a
    /// cooperative scheduler registered for a different clock.
    pub fn clock_handle(mut self, clock: &crate::Clock) -> Self {
        self.custom_clock = Some(clock.clone());
        self.clock = clock.manual_source().cloned();
        self
    }

    pub fn task_queue_factory(mut self, factory: &TaskQueueFactory) -> Self {
        self.task_queue_factory = Some(factory.clone());
        self
    }

    pub fn randomness(mut self, lease: &RandomnessLease) -> Self {
        self.randomness = Some(lease.0.clone());
        self
    }

    pub fn build(self) -> Result<Environment, BuildEnvironmentError> {
        let factory = match self.task_queue_factory {
            Some(factory) => factory,
            None => TaskQueueFactory::default_threaded()?,
        };
        factory.check_current()?;
        if self
            .custom_clock
            .as_ref()
            .is_some_and(|clock| clock.manual_source().is_none())
            && factory.0.clock.is_some()
        {
            return Err(BuildEnvironmentError::MismatchedCooperativeClock);
        }
        let clock = match (&self.clock, &factory.0.clock) {
            (None, Some(factory_clock)) => Some(factory_clock.clone()),
            (Some(clock), Some(_))
                if !ffi::task_queue_factory_uses_clock(factory.native(), clock.native()) =>
            {
                return Err(BuildEnvironmentError::MismatchedCooperativeClock);
            }
            (clock, _) => clock.clone(),
        };
        let clock_ptr = clock
            .as_ref()
            .map_or(std::ptr::null(), |clock| clock.native() as *const _);
        let custom_clock_ptr = self
            .custom_clock
            .as_ref()
            .map_or(std::ptr::null(), |clock| clock.native() as *const _);
        let clock_bounds = self
            .custom_clock
            .as_ref()
            .map(crate::Clock::time_bounds)
            .or_else(|| clock.as_ref().map(ManualClock::time_bounds))
            .unwrap_or_else(crate::ClockTimeBounds::system_time);
        // SAFETY: null clock pointers select upstream system time; otherwise
        // the selected clock is retained in the returned Environment. A
        // custom handle may also carry its proven manual scheduling anchor.
        // All selected values are valid and retained for this call. Bounds
        // are enforced by this exact selected adapter, not supplied by a caller.
        let native = unsafe {
            ffi::create_environment(
                clock_ptr,
                custom_clock_ptr,
                factory.native(),
                Box::new(crate::RustFieldTrials(self.field_trials)),
                crate::RustRtcEventLog::from_logger(self.event_log),
                clock_bounds.minimum().microseconds(),
                clock_bounds.maximum().microseconds(),
            )
        };
        if native.is_null() {
            return Err(BuildEnvironmentError::NativeConstructionFailed);
        }
        Ok(Environment {
            native,
            _clock: clock,
            _custom_clock: self.custom_clock.is_some(),
            _clock_handle: self.custom_clock,
            clock_bounds,
            _task_queue_factory: Some(factory),
            _randomness: self.randomness,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildEnvironmentError {
    Closed,
    UnqualifiedEnvironment,
    ProductionRuntimeUnavailable,
    InvalidQueueName,
    MismatchedCooperativeClock,
    NativeConstructionFailed,
    TimestampOutOfRange,
    WrongThread,
    NotCooperative,
    UnsupportedNativeDelay,
}

impl fmt::Display for BuildEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "environment is closed",
            Self::ProductionRuntimeUnavailable => {
                "production task queues cannot coexist with controlled execution"
            }
            Self::UnqualifiedEnvironment => {
                "SDK environment has no identity-bound task queue factory registration"
            }
            Self::InvalidQueueName => "task queue names cannot contain a NUL byte",
            Self::MismatchedCooperativeClock => {
                "the environment clock differs from the cooperative factory clock"
            }
            Self::NativeConstructionFailed => "native WebRTC construction failed",
            Self::TimestampOutOfRange => {
                "timestamp is outside the controlled clock's NTP-era-zero range"
            }
            Self::WrongThread => "cooperative execution requires the factory's creator thread",
            Self::NotCooperative => "local tasks require a cooperative queue",
            Self::UnsupportedNativeDelay => {
                "delay cannot be safely converted by this native scheduling backend"
            }
        })
    }
}

impl std::error::Error for BuildEnvironmentError {}

fn native_delay_result(code: u8) -> Result<bool, BuildEnvironmentError> {
    match code {
        0 => Ok(true),
        2 => Err(BuildEnvironmentError::UnsupportedNativeDelay),
        _ => Ok(false),
    }
}

/// Exclusive process-global installation of deterministic WebRTC randomness.
/// Clones retain the same installation without reseeding the generator.
#[derive(Clone)]
pub struct RandomnessLease(Arc<RandomnessState>);

struct RandomnessState {
    native: Option<cxx::UniquePtr<ffi::NativeRandomnessLease>>,
}

// SAFETY: the native generator serializes its mutable PRNG state and WebRTC's
// installation seam is process-global. Exclusivity is enforced before install.
unsafe impl Send for RandomnessState {}
// SAFETY: see the `Send` rationale; calls are serialized by the native mutex.
unsafe impl Sync for RandomnessState {}

impl RandomnessLease {
    pub fn acquire(seed: u64) -> Result<Self, RandomnessLeaseError> {
        let native = ffi::new_seeded_randomness(seed);
        if native.is_null() {
            return Err(RandomnessLeaseError::AlreadyAcquired);
        }
        Ok(Self(Arc::new(RandomnessState {
            native: Some(native),
        })))
    }

    pub fn next_u64(&self) -> u64 {
        ffi::next_seeded_random_u64(self.0.native.as_ref().expect("active randomness lease"))
    }
}

impl Drop for RandomnessState {
    fn drop(&mut self) {
        drop(self.native.take());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomnessLeaseError {
    AlreadyAcquired,
    NativeConstructionFailed,
}

impl fmt::Display for RandomnessLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyAcquired => "WebRTC randomness is already leased",
            Self::NativeConstructionFailed => "failed to install WebRTC randomness",
        })
    }
}

impl std::error::Error for RandomnessLeaseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadStartError;

impl fmt::Display for ThreadStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("failed to initialize WebRTC thread")
    }
}

impl std::error::Error for ThreadStartError {}

macro_rules! thread_handle {
    ($name:ident, $network:literal) => {
        /// An owning, sequence-bound WebRTC execution thread.
        ///
        /// ```compile_fail
        /// fn assert_send<T: Send>() {}
        #[doc = concat!("assert_send::<pulsebeam_webrtc_sys::", stringify!($name), ">();")]
        /// ```
        ///
        /// ```compile_fail
        /// fn assert_sync<T: Sync>() {}
        #[doc = concat!("assert_sync::<pulsebeam_webrtc_sys::", stringify!($name), ">();")]
        /// ```
        pub struct $name {
            native: cxx::UniquePtr<ffi::NativeThread>,
            _creator_sequence: PhantomData<Rc<()>>,
        }

        impl $name {
            pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeThread>) -> Self {
                Self {
                    native,
                    _creator_sequence: PhantomData,
                }
            }

            pub fn start() -> Result<Self, ThreadStartError> {
                let native = ffi::new_thread($network);
                if native.is_null() {
                    Err(ThreadStartError)
                } else {
                    Ok(Self {
                        native,
                        _creator_sequence: PhantomData,
                    })
                }
            }

            pub fn post(&self, task: impl FnOnce() + Send + 'static) -> bool {
                self.native
                    .as_ref()
                    .is_some_and(|native| ffi::thread_post_task(native, boxed_task(task)))
            }

            /// Native Thread delays and both precision modes are passed through
            /// unchanged; SDK threads own their arithmetic and millisecond
            /// rounding. Borrowed controlled threads require finite values.
            pub fn post_delayed_native(
                &self,
                delay: crate::TimeDelta,
                precision: crate::TaskDelayPrecision,
                _location: crate::TaskQueueLocation,
                task: impl FnOnce() + Send + 'static,
            ) -> Result<bool, BuildEnvironmentError> {
                let Some(native) = self.native.as_ref() else {
                    return Ok(false);
                };
                native_delay_result(ffi::thread_post_native_delayed_task(
                    native,
                    delay.native_microsecond_bits(),
                    precision == crate::TaskDelayPrecision::High,
                    boxed_task(task),
                ))
            }

            pub fn post_delayed(
                &self,
                delay: Duration,
                task: impl FnOnce() + Send + 'static,
            ) -> Result<bool, BuildEnvironmentError> {
                let delay_us =
                    duration_micros(delay).ok_or(BuildEnvironmentError::TimestampOutOfRange)?;
                self.post_delayed_native(
                    crate::TimeDelta::from_microseconds(delay_us)
                        .map_err(|_| BuildEnvironmentError::UnsupportedNativeDelay)?,
                    crate::TaskDelayPrecision::Low,
                    crate::TaskQueueLocation,
                    task,
                )
            }

            pub fn close(&mut self) {
                drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
            }

            pub(crate) fn native(&self) -> &ffi::NativeThread {
                self.native.as_ref().expect("validated WebRTC thread")
            }
        }
    };
}

thread_handle!(NetworkThread, true);
thread_handle!(WorkerThread, false);
thread_handle!(SignalingThread, false);

impl NetworkThread {
    pub(crate) fn into_native(mut self) -> cxx::UniquePtr<ffi::NativeThread> {
        std::mem::replace(&mut self.native, cxx::UniquePtr::null())
    }
}

/// One caller-owned, sequence-bound WebRTC thread for peer network, worker,
/// and signaling roles. It never starts an OS thread. Its budgeted pump also
/// dispatches cooperative media queues sharing the manual clock. Only one such
/// driver may be active per process because WebRTC's thread clock is global.
/// Native devices are not supported with this driver.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::ControlledPeerDriver>();
/// ```
#[derive(Clone)]
pub struct ControlledPeerDriver(Rc<DriverInner>);
#[derive(Clone)]
pub(crate) struct WeakControlledPeerDriver(std::rc::Weak<DriverInner>);
impl WeakControlledPeerDriver {
    pub(crate) fn upgrade(&self) -> Option<ControlledPeerDriver> {
        self.0.upgrade().map(ControlledPeerDriver)
    }
}

// These are real ownership edges back to this driver's SDK graph, not IDs or
// label-based joins. Weak edges avoid idle cycles. Only an enclosing native
// dispatch/notification turn upgrades them, and all strong leases drain after
// the SDK call returns, never from inside a provider callback.
#[derive(Default)]
struct DriverOwners {
    factories: Vec<std::rc::Weak<crate::peer::FactoryInner>>,
    peers: Vec<std::rc::Weak<crate::peer::PeerInner>>,
    held_factories: Vec<Rc<crate::peer::FactoryInner>>,
    held_peers: Vec<Rc<crate::peer::PeerInner>>,
    depth: usize,
    retiring: bool,
}
impl DriverOwners {
    fn prune(&mut self) {
        self.factories.retain(|owner| owner.strong_count() != 0);
        self.peers.retain(|owner| owner.strong_count() != 0);
    }
}
thread_local! {
    static CURRENT_DRIVER: std::cell::RefCell<std::rc::Weak<DriverInner>> = const { std::cell::RefCell::new(std::rc::Weak::new()) };
}
pub(crate) struct ControlledOwnerTurn {
    driver: ControlledPeerDriver,
}
impl Drop for ControlledOwnerTurn {
    fn drop(&mut self) {
        let drain = {
            let mut owners = self.driver.0.owners.borrow_mut();
            owners.depth -= 1;
            if owners.depth == 0 && !owners.retiring {
                owners.retiring = true;
                true
            } else {
                false
            }
        };
        if !drain {
            return;
        }
        loop {
            let (peers, factories) = {
                let mut owners = self.driver.0.owners.borrow_mut();
                if owners.held_peers.is_empty() && owners.held_factories.is_empty() {
                    owners.prune();
                    owners.retiring = false;
                    break;
                }
                (
                    std::mem::take(&mut owners.held_peers),
                    std::mem::take(&mut owners.held_factories),
                )
            };
            // Native destruction may invoke providers and add new owners. No
            // RefCell loan spans destructors; nested turns append to the next
            // batch rather than retiring inside the current native destructor.
            drop(peers);
            drop(factories);
        }
    }
}
fn controlled_dispatch_blocked() -> bool {
    CURRENT_DRIVER
        .with(|current| current.borrow().upgrade())
        .is_some_and(|driver| {
            driver.owners.borrow().retiring || crate::provider_panic::callback_active()
        })
}
pub(crate) fn controlled_owner_turn() -> Option<ControlledOwnerTurn> {
    CURRENT_DRIVER
        .with(|current| current.borrow().upgrade())
        .map(|inner| ControlledPeerDriver(inner).owner_turn())
}

struct DriverInner {
    owners: std::cell::RefCell<DriverOwners>,
    native: cxx::UniquePtr<ffi::NativeDriverThread>,
    clock: ManualClock,
    next_peer_id: std::cell::Cell<u64>,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl ControlledPeerDriver {
    pub(crate) fn downgrade(&self) -> WeakControlledPeerDriver {
        WeakControlledPeerDriver(Rc::downgrade(&self.0))
    }
    fn from_inner(inner: DriverInner) -> Self {
        let value = Self(Rc::new(inner));
        CURRENT_DRIVER.with(|current| *current.borrow_mut() = Rc::downgrade(&value.0));
        value
    }
    pub(crate) fn owner_turn(&self) -> ControlledOwnerTurn {
        {
            let mut owners = self.0.owners.borrow_mut();
            if owners.depth == 0 && !owners.retiring {
                owners.prune();
                let factories = owners
                    .factories
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .collect::<Vec<_>>();
                let peers = owners
                    .peers
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .collect::<Vec<_>>();
                owners.held_factories.extend(factories);
                owners.held_peers.extend(peers);
            }
            owners.depth += 1;
        }
        ControlledOwnerTurn {
            driver: self.clone(),
        }
    }
    pub(crate) fn register_factory(&self, owner: &Rc<crate::peer::FactoryInner>) {
        let mut owners = self.0.owners.borrow_mut();
        owners.prune();
        owners.factories.push(Rc::downgrade(owner));
        if owners.depth != 0 || owners.retiring {
            owners.held_factories.push(owner.clone());
        }
    }
    pub(crate) fn register_peer(&self, owner: &Rc<crate::peer::PeerInner>) {
        let mut owners = self.0.owners.borrow_mut();
        owners.prune();
        owners.peers.push(Rc::downgrade(owner));
        if owners.depth != 0 || owners.retiring {
            owners.held_peers.push(owner.clone());
        }
    }
    pub fn new(clock: &ManualClock) -> Result<Self, ThreadStartError> {
        let native = ffi::new_driver_thread(clock.native());
        if native.is_null() {
            Err(ThreadStartError)
        } else {
            Ok(Self::from_inner(DriverInner {
                owners: std::cell::RefCell::new(DriverOwners::default()),
                native,
                clock: clock.clone(),
                next_peer_id: std::cell::Cell::new(1),
                _creator_sequence: PhantomData,
            }))
        }
    }

    /// Dispatch up to 1,024 ready tasks without moving time or waiting.
    /// Returns true on the creator thread (including when idle), false
    /// off-thread or when provider/retirement reentry blocks dispatch.
    /// Success does not imply a drain; use `pump` for a
    /// caller-selected budget and a remaining-work snapshot.
    pub fn run_ready(&self) -> bool {
        if !self.is_current() {
            return false;
        }
        !self.pump(1_024).blocked
    }

    /// Dispatch at most `budget` tasks across this driver's peer and media
    /// queues. All queues sharing its clock use deadline then posting order.
    /// Provider callbacks and owner retirement return `blocked` without
    /// recursively executing native work or advancing time.
    pub fn pump(&self, budget: usize) -> PumpResult {
        // Do not recursively execute SDK work while a native provider still
        // has a live callback loan or owner retirement is in progress.
        let blocked = self.0.owners.borrow().retiring || crate::provider_panic::callback_active();
        let turn = (!blocked).then(|| self.owner_turn());
        let dispatched = if blocked {
            0
        } else {
            ffi::driver_pump(self.native(), budget)
        };
        drop(turn);
        let next_deadline = self.next_deadline();
        PumpResult {
            dispatched,
            blocked,
            ready: next_deadline.is_some_and(|deadline| deadline <= self.0.clock.now()),
            next_deadline,
        }
    }

    /// Earliest pending deadline across native peer and media task queues,
    /// with microsecond precision. Pending external packets are caller-owned.
    pub fn next_deadline(&self) -> Option<Duration> {
        let deadline = ffi::driver_next_deadline_us(self.native());
        (deadline != NO_DEADLINE).then(|| Duration::from_micros(deadline as u64))
    }

    pub(crate) fn is_current(&self) -> bool {
        ffi::driver_is_current(self.native())
    }

    pub(crate) fn allocate_peer_id(&self) -> Option<u64> {
        let id = self.0.next_peer_id.get();
        self.0.next_peer_id.set(id.checked_add(1)?);
        Some(id)
    }

    pub(crate) fn clock(&self) -> &ManualClock {
        &self.0.clock
    }

    pub(crate) fn borrow_thread(&self) -> cxx::UniquePtr<ffi::NativeThread> {
        ffi::borrow_driver_thread(self.native())
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn native(&self) -> &ffi::NativeDriverThread {
        self.0.native.as_ref().expect("validated controlled driver")
    }
}

/// Snapshot after a caller-selected finite dispatch budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PumpResult {
    pub dispatched: usize,
    /// No task ran because dispatch was requested from a provider callback or
    /// during owner retirement. The deadline/ready fields remain real snapshots.
    pub blocked: bool,
    pub ready: bool,
    pub next_deadline: Option<Duration>,
}

/// Exclusive seeded, caller-thread-controlled execution domain.
///
/// Acquisition installs the process clock and non-cryptographic random hook
/// atomically, or leaves existing hooks untouched. Peer factories, networks
/// and queues retain the driver until their caller-thread teardown finishes.
/// Production peers and independent parallel worlds must not coexist with it.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::ControlledWorld>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::ControlledWorld>();
/// ```
pub struct ControlledWorld {
    factory: TaskQueueFactory,
    clock: ManualClock,
    driver: ControlledPeerDriver,
}

impl ControlledWorld {
    pub fn acquire(seed: u64, initial_time: Duration) -> Result<Self, WorldAcquireError> {
        let clock = ManualClock::new(initial_time).map_err(WorldAcquireError::Environment)?;
        let factory =
            TaskQueueFactory::cooperative(&clock).map_err(WorldAcquireError::Environment)?;
        let native = ffi::new_seeded_driver_thread(clock.native(), seed);
        if native.is_null() {
            return Err(WorldAcquireError::HooksUnavailable);
        }
        let driver = ControlledPeerDriver::from_inner(DriverInner {
            owners: std::cell::RefCell::new(DriverOwners::default()),
            native,
            clock: clock.clone(),
            next_peer_id: std::cell::Cell::new(1),
            _creator_sequence: PhantomData,
        });
        Ok(Self {
            factory,
            clock,
            driver,
        })
    }

    pub fn now(&self) -> Duration {
        self.clock.now()
    }

    /// Move virtual time without executing any queued work. Fractional
    /// microseconds are truncated; time is never advanced by pumping/yielding.
    pub fn advance(&self, delta: Duration) -> Result<(), BuildEnvironmentError> {
        self.clock.advance(delta)
    }

    pub fn pump(&self, budget: usize) -> PumpResult {
        self.driver.pump(budget)
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        self.driver.next_deadline()
    }

    pub fn driver(&self) -> &ControlledPeerDriver {
        &self.driver
    }

    pub fn create_queue(
        &self,
        name: &str,
        priority: QueuePriority,
    ) -> Result<TaskQueue, BuildEnvironmentError> {
        let mut queue = self.factory.create_queue(name, priority)?;
        queue.driver = Some(self.driver.clone());
        Ok(queue)
    }

    /// Create a packet network on this world's timeline without exposing a
    /// separately sendable clock handle. Delivery and shaping remain external.
    pub fn create_network(&self) -> Result<crate::ControlledSimulatedNetwork, crate::NetworkError> {
        crate::ControlledSimulatedNetwork::new(&self.clock, &self.driver)
    }

    /// Select this world's services for the common session API. This does not
    /// select a media profile or start workers; ordinary builder options select
    /// network providers, codecs, processing and explicit native devices.
    pub fn runtime(&self) -> Result<crate::ManagedRuntime, BuildEnvironmentError> {
        let environment = Environment::builder()
            .task_queue_factory(&self.factory)
            .build()?;
        Ok(crate::ManagedRuntime::new(environment, self.driver.clone()))
    }

    /// Build a factory with matching clock, cooperative queues and driver.
    /// Supply controlled packet providers before `build`.
    pub fn peer_factory_builder(
        &self,
    ) -> Result<crate::PeerConnectionFactoryBuilder, BuildEnvironmentError> {
        let environment = Environment::builder()
            .task_queue_factory(&self.factory)
            .build()?;
        Ok(crate::PeerConnectionFactory::builder()
            .environment(environment)
            .controlled_driver(&self.driver))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorldAcquireError {
    Environment(BuildEnvironmentError),
    HooksUnavailable,
}
impl fmt::Display for WorldAcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(error) => error.fmt(f),
            Self::HooksUnavailable => {
                f.write_str("controlled clock/randomness hooks or caller thread are already owned")
            }
        }
    }
}
impl std::error::Error for WorldAcquireError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn thread_safe_handles_have_positive_traits() {
        assert_send_sync::<ManualClock>();
        assert_send_sync::<TaskQueueFactory>();
        assert_send_sync::<Environment>();
        assert_send_sync::<RandomnessLease>();
    }

    #[test]
    fn sdk_environment_preserves_only_identity_matched_rust_capabilities() {
        let clock = ManualClock::new(Duration::from_micros(100)).unwrap();
        let queues = TaskQueueFactory::cooperative(&clock).unwrap();
        let selected = Environment::builder()
            .clock(&clock)
            .task_queue_factory(&queues)
            .build()
            .unwrap();
        let other = Environment::builder().clock(&clock).build().unwrap();
        let native_copy = ffi::clone_environment(selected.native());
        let actual = native_copy.as_ref().unwrap();
        assert!(
            ffi::environment_clock_identity_matches(actual, selected.native()),
            "SDK clock component identity"
        );
        assert!(
            ffi::environment_queue_factory_identity_matches(actual, selected.native()),
            "SDK queue component identity"
        );
        assert!(
            queues.is_cooperative() && queues.check_current().is_ok(),
            "selected cooperative queue thread"
        );
        assert!(
            ffi::task_queue_factory_uses_clock(queues.native(), clock.native()),
            "selected cooperative queue clock identity"
        );
        let copied = Environment::from_sdk(native_copy, Some(&selected));
        assert!(copied.uses_controlled_clock(&clock));
        assert!(copied.task_queue_factory().unwrap().is_cooperative());
        // An SDK snapshot without selected Rust origin still owns the exact
        // clock, but cannot use Rust metadata to preflight its conversion kind.
        let unqualified = Environment::from_sdk(ffi::clone_environment(selected.native()), None);
        assert!(matches!(
            unqualified.task_queue_factory(),
            Err(BuildEnvironmentError::UnqualifiedEnvironment)
        ));
        let utility = unqualified.clock().unwrap();
        assert_eq!(utility.time_in_microseconds(), 100);
        for timestamp in [
            crate::Timestamp::plus_infinity(),
            crate::Timestamp::minus_infinity(),
        ] {
            assert!(
                !utility
                    .convert_timestamp_to_ntp_time(timestamp)
                    .unwrap()
                    .is_valid()
            );
        }
        // SharedClock rejects infinite unit queries but delegates every finite
        // conversion, including negative subsecond input, to the pinned SDK.
        for micros in [-1, -1_000_000] {
            assert!(
                utility
                    .convert_timestamp_to_ntp_time(
                        crate::Timestamp::from_microseconds(micros).unwrap()
                    )
                    .unwrap()
                    .is_valid()
            );
        }
        // Matching clock identity alone cannot grant cooperative scheduling.
        assert!(!other.uses_controlled_clock(&clock));
        let mismatched =
            Environment::from_sdk(ffi::clone_environment(other.native()), Some(&selected));
        assert!(!mismatched.uses_controlled_clock(&clock));
        assert!(matches!(
            mismatched.task_queue_factory(),
            Err(BuildEnvironmentError::UnqualifiedEnvironment)
        ));
        drop(selected);
        clock.advance(Duration::from_micros(50)).unwrap();
        assert_eq!(copied.timestamp().microseconds().unwrap(), 150);
    }

    #[test]
    fn copied_sdk_context_keeps_exact_dependencies_and_only_matching_clock_bounds() {
        let minimum = crate::FiniteTimestamp::from_microseconds(-100).unwrap();
        let maximum = crate::FiniteTimestamp::from_microseconds(200).unwrap();
        let bounds = crate::ClockTimeBounds::new(minimum, maximum).unwrap();
        let clock =
            crate::SimulatedClock::bounded(crate::Timestamp::from_microseconds(0).unwrap(), bounds)
                .unwrap();
        let environment = Environment::builder()
            .clock_handle(clock.clock())
            .build()
            .unwrap();
        let other = Environment::builder().build().unwrap();
        assert!(ffi::environment_context_retention_contract(
            environment.native(),
            other.native()
        ));
        let copied = environment.clone();
        let native = ffi::environment_clock_time_bounds(copied.native());
        assert!(native.known);
        assert_eq!((native.minimum_us, native.maximum_us), (-100, 200));
        assert_eq!(copied.clock_time_bounds(), bounds);
    }

    #[test]
    fn lifecycle_yield_leaves_self_reposting_caller_work_for_the_pump() {
        use std::cell::{Cell, RefCell};

        fn repost(queue: Rc<RefCell<TaskQueue>>, count: Rc<Cell<usize>>) {
            let next_queue = queue.clone();
            queue
                .borrow()
                .post_local(move || {
                    count.set(count.get() + 1);
                    repost(next_queue, count);
                })
                .unwrap();
        }

        let world = ControlledWorld::acquire(81, Duration::from_secs(99)).unwrap();
        let environment = Environment::builder()
            .task_queue_factory(&world.factory)
            .build()
            .unwrap();
        let utility = environment.task_queue_factory().unwrap();
        let rebound = Environment::builder()
            .task_queue_factory(&utility)
            .build()
            .unwrap();
        let rebound_utility = rebound.task_queue_factory().unwrap();
        let queue = Rc::new(RefCell::new(
            rebound_utility
                .create_queue("caller-reposting", QueuePriority::Normal)
                .unwrap(),
        ));
        let count = Rc::new(Cell::new(0));
        repost(queue.clone(), count.clone());
        assert!(ffi::test_driver_lifecycle_yield(
            world.driver.native(),
            rebound_utility.native(),
        ));
        assert_eq!(count.get(), 0);
        assert_eq!(world.now(), Duration::from_secs(99));
        assert!(world.pump(0).ready);
        assert_eq!(world.pump(3).dispatched, 3);
        assert_eq!(count.get(), 3);
        queue.borrow_mut().close();
    }

    #[test]
    fn captured_driver_cancellation_removes_the_queued_envelope() {
        let world = ControlledWorld::acquire(922, Duration::from_secs(3)).unwrap();
        assert!(ffi::test_driver_capture_cancellation(world.driver.native()));
        assert_eq!(world.pump(1).dispatched, 0);
        assert_eq!(world.now(), Duration::from_secs(3));
    }

    #[test]
    fn rejects_out_of_range_time_without_native_linking() {
        assert_eq!(duration_micros(Duration::from_secs(u64::MAX)), None);
    }
}
