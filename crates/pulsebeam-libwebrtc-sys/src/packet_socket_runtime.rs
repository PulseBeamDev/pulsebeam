//! Explicit production ownership of a selected native network role.
use crate::{Environment, NetworkThread, PacketSocketEventError, ffi};
use std::{
    fmt,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSocketRuntimeError {
    IncompatibleExecution,
    RetirementUnavailable,
}
impl fmt::Display for PacketSocketRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IncompatibleExecution => "standalone sockets require an owning production network thread and qualified threaded environment",
            Self::RetirementUnavailable => "cannot reserve standalone network-role retirement",
        })
    }
}
impl std::error::Error for PacketSocketRuntimeError {}

// No role-local products live here. Each native product retains this actual
// owner until its facade, scope, providers and active native frames are gone.
struct Resources {
    thread: cxx::UniquePtr<ffi::NativeThread>,
    environment: Environment,
    _lease: cxx::UniquePtr<ffi::NativeProductionLease>,
}
// Native Thread posting is concurrent; its State stays immutable until exclusive
// Drop. Environment is Send/Sync. The production lease has no shared mutations
// here and native final release uses the process lease mutex. Exclusive resource
// destruction runs off SDK callbacks/owned roles after the last product/frame
// edge, never by moving role-local Rust products to the retirement worker.
unsafe impl Send for Resources {}
unsafe impl Sync for Resources {}
impl Drop for Resources {
    fn drop(&mut self) {
        drop(std::mem::replace(&mut self.thread, cxx::UniquePtr::null()));
        self.environment.close();
        drop(std::mem::replace(&mut self._lease, cxx::UniquePtr::null()));
    }
}
enum Request {
    Waiting,
    Cancelled,
    Retire(Resources),
}
struct RetirementState {
    request: Mutex<Request>,
    ready: Condvar,
}
struct Retirement {
    state: Arc<RetirementState>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Retirement {
    fn new() -> Result<Self, PacketSocketRuntimeError> {
        let state = Arc::new(RetirementState {
            request: Mutex::new(Request::Waiting),
            ready: Condvar::new(),
        });
        let pending = state.clone();
        let worker = std::thread::Builder::new()
            .name("webrtc-socket-retire".into())
            .spawn(move || {
                let request = {
                    let mut slot = pending.request.lock().unwrap_or_else(|e| e.into_inner());
                    while matches!(*slot, Request::Waiting) {
                        slot = pending.ready.wait(slot).unwrap_or_else(|e| e.into_inner());
                    }
                    std::mem::replace(&mut *slot, Request::Cancelled)
                };
                if let Request::Retire(resources) = request {
                    let _ = crate::provider_panic::catch_task(|| drop(resources));
                }
            })
            .map_err(|_| PacketSocketRuntimeError::RetirementUnavailable)?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    fn retire(mut self, resources: Resources) {
        *self.state.request.lock().unwrap_or_else(|e| e.into_inner()) = Request::Retire(resources);
        self.state.ready.notify_one();
        drop(self.worker.take()); // SDK caller must return before native Thread joins.
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            *self.state.request.lock().unwrap_or_else(|e| e.into_inner()) = Request::Cancelled;
            self.state.ready.notify_one();
            if let Err(payload) = worker.join() {
                std::mem::forget(payload);
            }
        }
    }
}
struct RuntimeInner {
    resources: Option<Resources>,
    retirement: Option<Retirement>,
    failed: Arc<AtomicBool>,
}
impl Drop for RuntimeInner {
    fn drop(&mut self) {
        let resources = self.resources.take().unwrap();
        let retirement = self.retirement.take().unwrap();
        if crate::provider_panic::callback_active()
            || ffi::thread_requires_retirement(resources.thread.as_ref().unwrap())
        {
            retirement.retire(resources);
        } else {
            drop(resources); // Role exits before environment/production lease release.
            drop(retirement);
        }
    }
}
/// Owns the selected, already-started SDK network thread and exact environment.
/// Posting uses that actual thread, not an additional application executor.
/// A pre-admitted single-use worker only handles final native Thread destruction
/// from SDK callbacks; it never creates sockets or executes application commands.
#[derive(Clone)]
pub struct PacketSocketRuntime(Arc<RuntimeInner>);
#[derive(Clone)]
pub(crate) struct WeakPacketSocketRuntime(Weak<RuntimeInner>);
impl WeakPacketSocketRuntime {
    pub(crate) fn upgrade(&self) -> Option<PacketSocketRole> {
        self.0.upgrade().map(|owner| PacketSocketRole {
            runtime: PacketSocketRuntime(owner),
            sequence: PhantomData,
        })
    }
}
/// A sequence-local creation capability delivered on the actual selected role.
/// Native socket/listener owners may be retained on that role, never returned
/// across this callback's Send boundary. Their event sources may cross threads.
pub struct PacketSocketRole {
    pub(crate) runtime: PacketSocketRuntime,
    sequence: PhantomData<Rc<()>>,
}
impl PacketSocketRole {
    pub fn environment(&self) -> &Environment {
        &self.runtime.resources().environment
    }
    pub(crate) fn check_current(&self) -> Result<(), PacketSocketEventError> {
        if ffi::thread_is_current(self.runtime.resources().thread.as_ref().unwrap()) {
            Ok(())
        } else {
            Err(PacketSocketEventError::WrongSequence)
        }
    }
}
impl PacketSocketRuntime {
    /// Consumes the actual owning thread, not a Current() snapshot or controlled
    /// borrowed wrapper. All products retain this owner independently of handles.
    pub fn new(
        thread: NetworkThread,
        environment: &Environment,
    ) -> Result<Self, PacketSocketRuntimeError> {
        if environment.cooperative_queues() != Some(false) || !ffi::thread_is_owned(thread.native())
        {
            return Err(PacketSocketRuntimeError::IncompatibleExecution);
        }
        let lease = ffi::new_production_lease();
        if lease.is_null() {
            return Err(PacketSocketRuntimeError::IncompatibleExecution);
        }
        let retirement = Retirement::new()?;
        Ok(Self(Arc::new(RuntimeInner {
            resources: Some(Resources {
                thread: thread.into_native(),
                environment: environment.clone(),
                _lease: lease,
            }),
            retirement: Some(retirement),
            failed: Arc::new(AtomicBool::new(false)),
        })))
    }
    pub(crate) fn downgrade(&self) -> WeakPacketSocketRuntime {
        WeakPacketSocketRuntime(Arc::downgrade(&self.0))
    }
    fn resources(&self) -> &Resources {
        self.0.resources.as_ref().unwrap()
    }
    pub fn environment(&self) -> &Environment {
        &self.resources().environment
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    /// Admission only, not completion. Queued work holds a weak runtime edge;
    /// it is discarded if no runtime/product owner survives until execution.
    /// Callback/capture panic is contained and observable through has_failed.
    /// No socket product crosses threads or synchronous result waits are added.
    pub fn post(&self, work: impl FnOnce(PacketSocketRole) + Send + 'static) -> bool {
        struct Work<F> {
            body: Option<F>,
            failed: Arc<AtomicBool>,
        }
        impl<F> Drop for Work<F> {
            fn drop(&mut self) {
                let body = self.body.take();
                if crate::provider_panic::catch_provider(|| drop(body)).is_err() {
                    self.failed.store(true, Ordering::Release);
                }
            }
        }
        let runtime = Arc::downgrade(&self.0);
        let mut work = Work {
            body: Some(work),
            failed: self.0.failed.clone(),
        };
        ffi::thread_post_task(
            self.resources().thread.as_ref().unwrap(),
            crate::execution::boxed_context_task(move || {
                let Some(owner) = runtime.upgrade() else {
                    return;
                };
                let failed = work.failed.clone();
                if crate::provider_panic::catch_provider(|| {
                    work.body.take().unwrap()(PacketSocketRole {
                        runtime: PacketSocketRuntime(owner),
                        sequence: PhantomData,
                    })
                })
                .is_err()
                {
                    failed.store(true, Ordering::Release);
                }
            }),
        )
    }
}
