use crate::provider_panic::catch_provider as catch_unwind;
use crate::{QueuePriority, TimeDelta, ffi};
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The pinned standalone SDK's Location is an empty value. Chromium's extended
/// source-location interface is not compiled into this artifact.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskQueueLocation;
impl TaskQueueLocation {
    pub const fn current() -> Self {
        Self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TaskDelayPrecision {
    #[default]
    Low,
    High,
}

/// Owned native address-equality token, not a borrowed pointer or queue owner.
/// It grants no posting or deletion capability. Addresses can be reused after
/// retirement, so this is not a globally unique, persistent resource identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TaskQueueIdentity(pub(crate) usize);
impl TaskQueueIdentity {
    pub fn is_current(self) -> bool {
        ffi::task_queue_current_identity() == self.0
    }
}

/// A native post can fail before admission or if the registered
/// scheduler fails during admission. Successful admission does not guarantee
/// execution before native queue retirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskQueuePostError {
    /// This thread has no current queue with the snapshot's address identity.
    NotCurrent,
    /// The custom queue is retiring or its provider has failed.
    QueueUnavailable,
    /// This built-in backend cannot safely convert the requested native delay.
    UnsupportedNativeDelay,
}
impl std::fmt::Display for TaskQueuePostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotCurrent => "task queue is not current on this thread",
            Self::QueueUnavailable => "task queue is retiring or its provider has failed",
            Self::UnsupportedNativeDelay => {
                "delay cannot be safely converted by this native scheduling backend"
            }
        })
    }
}
impl std::error::Error for TaskQueuePostError {}

/// Owned snapshot of the current native queue's identity and name. It may be
/// retained after its callback or parent resources retire; it owns no engine
/// object. Posting resolves the live native Current() on the calling thread and
/// compares its address, never dereferencing the snapshot's stored token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskQueueContext {
    identity: TaskQueueIdentity,
    name: Vec<u8>,
}
impl TaskQueueContext {
    pub fn current() -> Option<Self> {
        let id = ffi::task_queue_current_identity();
        (id != 0).then(|| Self {
            identity: TaskQueueIdentity(id),
            name: ffi::task_queue_current_name(),
        })
    }
    pub fn identity(&self) -> TaskQueueIdentity {
        self.identity
    }
    pub fn name_bytes(&self) -> &[u8] {
        &self.name
    }
    pub fn is_current(&self) -> bool {
        self.identity.is_current()
    }
    /// Post asynchronously through the native queue that is current on this
    /// thread. This works for engine-created queues without exposing an owner
    /// or pointer. It cannot post from another thread or retain the queue.
    /// This may be an SDK-internal queue: its callbacks cannot synchronously
    /// close peers. Post control work to a caller-owned queue instead.
    /// Address reuse has the same meaning as `TaskQueueIdentity`: this is a
    /// current-address comparison, not a persistent resource identifier.
    /// Native shutdown can cancel accepted work. A provider failure reported
    /// after posting does not roll back work already delivered to its scheduler.
    pub fn post(&self, task: impl FnOnce() + Send + 'static) -> Result<(), TaskQueuePostError> {
        match ffi::task_queue_current_post(
            self.identity.0,
            crate::execution::boxed_context_task(task),
        ) {
            0 => Ok(()),
            1 => Err(TaskQueuePostError::NotCurrent),
            _ => Err(TaskQueuePostError::QueueUnavailable),
        }
    }
    /// Post an exact native delay asynchronously through the current queue.
    /// Custom providers and SDK backends receive signed/infinite states unchanged;
    /// backend arithmetic and precision/rounding remain native. Caller-owned
    /// cooperative queues require finite delays; see
    /// [`crate::TaskQueue::post_delayed_native`].
    /// This has the same current-address and shutdown semantics as `post`.
    pub fn post_delayed(
        &self,
        delay: TimeDelta,
        precision: TaskDelayPrecision,
        _location: TaskQueueLocation,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<(), TaskQueuePostError> {
        match ffi::task_queue_current_post_delayed(
            self.identity.0,
            delay.native_microsecond_bits(),
            precision == TaskDelayPrecision::High,
            crate::execution::boxed_context_task(task),
        ) {
            0 => Ok(()),
            1 => Err(TaskQueuePostError::NotCurrent),
            3 => Err(TaskQueuePostError::UnsupportedNativeDelay),
            _ => Err(TaskQueuePostError::QueueUnavailable),
        }
    }
}

/// Thread-safe construction of native queue products. Names are exact bytes,
/// including NUL and non-UTF-8. Each product receives its own native identity.
/// Creation and product callbacks may run concurrently on arbitrary engine
/// roles. Callbacks must not synchronously wait for or reenter their owning
/// actor; callbacks and destructors must not panic.
///
/// A healthy runtime must keep delivering work until deletion. A panic yields
/// a rejecting product, not a null SDK queue; operations requiring queue
/// progress can then stall. Factory diagnostics latch any provider failure.
pub trait TaskQueueFactoryProvider: Send + Sync + 'static {
    fn create_task_queue(&self, name: &[u8], priority: QueuePriority)
    -> Box<dyn TaskQueueProvider>;
}
impl<T: TaskQueueFactoryProvider + ?Sized> TaskQueueFactoryProvider for Arc<T> {
    fn create_task_queue(
        &self,
        name: &[u8],
        priority: QueuePriority,
    ) -> Box<dyn TaskQueueProvider> {
        (**self).create_task_queue(name, priority)
    }
}

/// Scheduler for one native queue. Schedule tickets asynchronously, not inline
/// from a posting callback. The adapter rejects that inline delivery before
/// entering native work. Concurrent or reordered delivery cannot overlap native
/// tasks or reorder immediate tasks: the adapter serializes and gates FIFO.
/// An accepted successor may await submission/cancellation of an earlier ticket.
///
/// Delays preserve exact signed/infinite native TimeDelta states. The scheduler
/// decides their timing, including unsupported/infinite delay policy; no native
/// deadline arithmetic is performed by this adapter. High precision omits the
/// low-precision 17ms allowance but neither mode guarantees OS timer precision.
/// The adapter may schedule an internal drain-notification ticket after inline
/// cancellation releases an already accepted FIFO-gated successor. It uses the
/// same asynchronous delivery/cancellation contract as ordinary work.
/// Dropped tickets cancel their tasks. Immediate capture destruction runs with
/// native Current() set to this queue, even when canceled off-thread.
pub trait TaskQueueProvider: Send + Sync + 'static {
    /// Native default virtual result, independent of the requested factory name.
    fn queue_name(&self) -> Vec<u8> {
        b"unnamed".to_vec()
    }
    fn post_task(&self, task: TaskQueueTask, location: TaskQueueLocation);
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        delay: TimeDelta,
        precision: TaskDelayPrecision,
        location: TaskQueueLocation,
    );
    /// Called once after native execution is quiescent and pending captures are
    /// canceled, including after a callback failure. Retained tickets are inert.
    /// This is not called from a running task on the queue. It must not wait for
    /// the owning actor or tasks on this now-retired queue.
    fn delete(&self) {}
}
impl<T: TaskQueueProvider + ?Sized> TaskQueueProvider for Arc<T> {
    fn queue_name(&self) -> Vec<u8> {
        (**self).queue_name()
    }
    fn post_task(&self, task: TaskQueueTask, location: TaskQueueLocation) {
        (**self).post_task(task, location);
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        delay: TimeDelta,
        precision: TaskDelayPrecision,
        location: TaskQueueLocation,
    ) {
        (**self).post_delayed_task(task, delay, precision, location);
    }
    fn delete(&self) {
        (**self).delete();
    }
}

/// Move-only, revocable ticket for native work. It contains no borrowed engine
/// pointer. Drop cancels; execute consumes it once and returns whether delivery
/// was accepted, not whether its FIFO-gated callable has already completed.
pub struct TaskQueueTask(cxx::UniquePtr<ffi::NativeTaskQueueTicket>);
// SAFETY: the ticket holds a shared synchronized slot, not an independently
// callable SDK function. Native submission serializes, establishes queue TLS,
// and checks retirement; deletion revokes captures before freeing the identity.
unsafe impl Send for TaskQueueTask {}
impl TaskQueueTask {
    pub fn execute(mut self) -> bool {
        let _turn = crate::execution::controlled_owner_turn();
        ffi::task_queue_ticket_submit(std::mem::replace(&mut self.0, cxx::UniquePtr::null()))
    }
}
impl Drop for TaskQueueTask {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(std::mem::replace(&mut self.0, cxx::UniquePtr::null()));
    }
}

pub(crate) struct FactoryState {
    provider: Option<Box<dyn TaskQueueFactoryProvider>>,
    disabled: AtomicBool,
    failed: AtomicBool,
}
impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_unwind(|| drop(provider));
    }
}
impl FactoryState {
    pub(crate) fn register(
        provider: impl TaskQueueFactoryProvider,
    ) -> (Arc<Self>, Box<RustTaskQueueFactory>) {
        let state = Arc::new(Self {
            provider: Some(Box::new(provider)),
            disabled: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        (state.clone(), Box::new(RustTaskQueueFactory(state)))
    }
    pub(crate) fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}
pub(crate) struct RustTaskQueueFactory(Arc<FactoryState>);
pub(crate) struct RustTaskQueueProvider {
    provider: Option<Box<dyn TaskQueueProvider>>,
    failed: AtomicBool,
    factory: Arc<FactoryState>,
}
impl Drop for RustTaskQueueProvider {
    fn drop(&mut self) {
        let provider = self.provider.take();
        if catch_unwind(|| drop(provider)).is_err() {
            task_queue_provider_mark_failed(self);
        }
    }
}
pub(crate) fn task_queue_provider_create(
    factory: &RustTaskQueueFactory,
    name: &[u8],
    priority: u8,
) -> Box<RustTaskQueueProvider> {
    let state = &factory.0;
    let provider = if state.disabled.load(Ordering::Acquire) {
        None
    } else {
        let priority = match priority {
            1 => QueuePriority::High,
            2 => QueuePriority::Video,
            3 => QueuePriority::Audio,
            4 => QueuePriority::Low,
            _ => QueuePriority::Normal,
        };
        match catch_unwind(AssertUnwindSafe(|| {
            state
                .provider
                .as_ref()
                .expect("live queue factory")
                .create_task_queue(name, priority)
        })) {
            Ok(provider) => Some(provider),
            Err(_) => {
                state.disabled.store(true, Ordering::Release);
                state.failed.store(true, Ordering::Release);
                None
            }
        }
    };
    Box::new(RustTaskQueueProvider {
        failed: AtomicBool::new(provider.is_none()),
        provider,
        factory: state.clone(),
    })
}
pub(crate) fn task_queue_provider_present(provider: &RustTaskQueueProvider) -> bool {
    provider.provider.is_some()
}
pub(crate) fn task_queue_provider_failed(provider: &RustTaskQueueProvider) -> bool {
    provider.failed.load(Ordering::Acquire)
}
pub(crate) fn task_queue_provider_mark_failed(provider: &RustTaskQueueProvider) {
    provider.failed.store(true, Ordering::Release);
    provider.factory.failed.store(true, Ordering::Release);
}
pub(crate) fn task_queue_provider_name(provider: &RustTaskQueueProvider) -> Vec<u8> {
    if task_queue_provider_failed(provider) {
        return b"unnamed".to_vec();
    }
    let Some(source) = &provider.provider else {
        return b"unnamed".to_vec();
    };
    match catch_unwind(AssertUnwindSafe(|| source.queue_name())) {
        Ok(name) => name,
        Err(_) => {
            task_queue_provider_mark_failed(provider);
            b"unnamed".to_vec()
        }
    }
}
pub(crate) fn task_queue_provider_post(
    provider: &RustTaskQueueProvider,
    task: cxx::UniquePtr<ffi::NativeTaskQueueTicket>,
    delayed: bool,
    delay: i64,
    high_precision: bool,
) {
    if task_queue_provider_failed(provider) {
        return;
    }
    let Some(source) = &provider.provider else {
        return;
    };
    let task = TaskQueueTask(task);
    if catch_unwind(AssertUnwindSafe(|| {
        if delayed {
            source.post_delayed_task(
                task,
                TimeDelta::from_native_microsecond_bits(delay),
                if high_precision {
                    TaskDelayPrecision::High
                } else {
                    TaskDelayPrecision::Low
                },
                TaskQueueLocation,
            );
        } else {
            source.post_task(task, TaskQueueLocation);
        }
    }))
    .is_err()
    {
        task_queue_provider_mark_failed(provider);
    }
}
pub(crate) fn task_queue_provider_delete(provider: &RustTaskQueueProvider) {
    if let Some(source) = &provider.provider
        && catch_unwind(AssertUnwindSafe(|| source.delete())).is_err()
    {
        task_queue_provider_mark_failed(provider);
    }
}
