//! Owning native RTC event-log factories, products and installed utility views.
use crate::{
    Environment, EventLogError, EventLogOutput, EventLogOutputPeriod, RtcEvent, RtcEventLogFactory,
    RtcEventLogger, ffi,
};
use std::{
    cell::Cell,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

struct ObservedFactory {
    provider: Arc<dyn RtcEventLogFactory>,
    failed: AtomicBool,
}
impl RtcEventLogFactory for ObservedFactory {
    fn create(&self, environment: crate::EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        struct Invocation<'a> {
            failed: &'a AtomicBool,
            returned: bool,
        }
        impl Drop for Invocation<'_> {
            fn drop(&mut self) {
                if !self.returned {
                    self.failed.store(true, Ordering::Release);
                }
            }
        }
        if self.failed.load(Ordering::Acquire) {
            panic!("selected event-log factory previously failed");
        }
        let mut invocation = Invocation {
            failed: &self.failed,
            returned: false,
        };
        let logger = self.provider.create(environment);
        invocation.returned = true;
        logger
    }
}

/// Shareable native factory selection. Each Create invokes the selected SDK
/// factory with the supplied environment; products do not borrow this handle.
#[derive(Clone)]
pub struct RtcEventLogFactoryHandle(Option<Arc<ObservedFactory>>);
impl RtcEventLogFactoryHandle {
    pub fn builtin() -> Self {
        Self(None)
    }
    pub fn from_provider(provider: impl RtcEventLogFactory) -> Self {
        Self(Some(Arc::new(ObservedFactory {
            provider: Arc::new(provider),
            failed: AtomicBool::new(false),
        })))
    }
    pub fn has_failed(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|factory| factory.failed.load(Ordering::Acquire))
    }
    pub(crate) fn registration(&self) -> Option<Arc<dyn RtcEventLogFactory>> {
        self.0
            .as_ref()
            .map(|factory| factory.clone() as Arc<dyn RtcEventLogFactory>)
    }

    /// Create on this caller sequence, without creating an owner worker.
    /// Active stock logging requires either a runtime-bound creator retirement
    /// scope or exclusive threaded execution. A bare cooperative environment
    /// lacks the lifecycle authority needed for native synchronous shutdown.
    /// The SDK kill-switch null product does not require an output queue.
    pub fn create(&self, environment: &Environment) -> Result<RtcEventLogHandle, EventLogError> {
        let controlled = environment
            .cooperative_queues()
            .ok_or(EventLogError::UnqualifiedEnvironment)?;
        if self.has_failed() {
            return Err(EventLogError::ProviderFailed);
        }
        let factory = self
            .registration()
            .map_or_else(cxx::UniquePtr::null, |provider| {
                ffi::new_rtc_event_log_factory(Box::new(crate::RustRtcEventLogFactory(
                    Some(provider),
                    environment.clone(),
                )))
            });
        let mut error = 0;
        let native = ffi::event_log_product_create(
            factory,
            environment.native(),
            Box::new(RustEventLogRetention {
                _environment: environment.clone(),
            }),
            controlled,
            &mut error,
        );
        if self.has_failed() {
            return Err(EventLogError::ProviderFailed);
        }
        if native.is_null() {
            return Err(match error {
                6 => EventLogError::MissingRuntimeAuthority,
                7 => EventLogError::ProviderFailed,
                _ => EventLogError::NativeRejected,
            });
        }
        Ok(RtcEventLogHandle {
            native,
            _sequence: PhantomData,
        })
    }
}

pub(crate) struct RustEventLogRetention {
    pub(crate) _environment: Environment,
}

/// A caller-sequence native logger owner. All operations require exclusive
/// owner access, preserving native start/stop affinity without shared handles.
/// Threaded drop can wait for native output retirement, just like the SDK
/// destructor. With a creator retirement scope, pending stock output and custom
/// stops retain their resources through completion instead of waiting in a
/// released view/product or creating an executor.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<pulsebeam_webrtc_sys::RtcEventLogHandle>();
/// ```
pub struct RtcEventLogHandle {
    native: cxx::UniquePtr<ffi::NativeRtcEventLogProduct>,
    _sequence: PhantomData<Rc<()>>,
}
impl RtcEventLogHandle {
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeRtcEventLogProduct> {
        self.native
    }
    pub fn log(&mut self, event: RtcEvent) -> Result<(), EventLogError> {
        status(ffi::event_log_product_log(
            self.native.pin_mut(),
            event.into_native(),
        ))
    }
    pub fn start(
        &mut self,
        output: EventLogOutput,
        period: EventLogOutputPeriod,
    ) -> Result<(), EventLogError> {
        let period = period.milliseconds()?;
        status(ffi::event_log_product_start(
            self.native.pin_mut(),
            output.into_native(),
            period,
        ))
    }
    pub fn stop(&mut self) -> Result<(), EventLogError> {
        status(ffi::event_log_product_stop(self.native.pin_mut()))
    }
    /// Initiate the actual native callback-based stop. No wait, clock advance,
    /// worker or executor is added. Other calls reject until native completion.
    /// Dropping the observer does not cancel admitted output shutdown.
    pub fn stop_async(&mut self) -> Result<RtcEventLogStopOperation, EventLogError> {
        let state = Arc::new(Mutex::new(StopState {
            result: None,
            waker: None,
        }));
        status(ffi::event_log_product_stop_async(
            self.native.pin_mut(),
            Box::new(RustEventLogProductStop(state.clone())),
        ))?;
        Ok(RtcEventLogStopOperation {
            state,
            consumed: false,
            _exclusive: PhantomData,
        })
    }
    pub fn close(&mut self) {
        if let Some(native) = self.native.as_mut() {
            ffi::event_log_product_close(native);
        }
    }
}

pub(crate) fn status(value: u8) -> Result<(), EventLogError> {
    match value {
        0 => Ok(()),
        1 => Err(EventLogError::Closed),
        2 => Err(EventLogError::StopPending),
        3 => Err(EventLogError::WrongSequence),
        7 => Err(EventLogError::ProviderFailed),
        8 => Err(EventLogError::Busy),
        _ => Err(EventLogError::NativeRejected),
    }
}

/// Owning view of the exact installed SDK logger, not a factory-created product.
/// Multiple views and SDK producers coordinate async stop through the same
/// native adapter. Closing a view releases it without stopping other owners.
/// Stock control operations retain native sequence rules; log delivery itself
/// is thread-safe across independently obtained views.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<pulsebeam_webrtc_sys::RtcEventLogUtility>();
/// ```
pub struct RtcEventLogUtility {
    native: cxx::UniquePtr<ffi::NativeEventLogUtility>,
    environment: Option<Box<Environment>>,
    _sequence: PhantomData<Rc<()>>,
}
impl RtcEventLogUtility {
    pub(crate) fn from_environment(environment: &Environment) -> Result<Self, EventLogError> {
        let native = ffi::environment_event_log(environment.native());
        if native.is_null() {
            return Err(EventLogError::NativeRejected);
        }
        Ok(Self {
            native,
            environment: Some(Box::new(environment.clone())),
            _sequence: PhantomData,
        })
    }
    pub fn start(
        &mut self,
        output: EventLogOutput,
        period: EventLogOutputPeriod,
    ) -> Result<(), EventLogError> {
        status(ffi::event_log_utility_start(
            self.native.pin_mut(),
            output.into_native(),
            period.milliseconds()?,
        ))
    }
    pub fn log(&mut self, event: RtcEvent) -> Result<(), EventLogError> {
        status(ffi::event_log_utility_log(
            self.native.pin_mut(),
            event.into_native(),
        ))
    }
    pub fn stop(&mut self) -> Result<(), EventLogError> {
        status(ffi::event_log_utility_stop(self.native.pin_mut()))
    }
    pub fn stop_async(&mut self) -> Result<RtcEventLogStopOperation, EventLogError> {
        let environment = self.environment.as_ref().ok_or(EventLogError::Closed)?;
        let state = Arc::new(Mutex::new(StopState {
            result: None,
            waker: None,
        }));
        status(ffi::event_log_utility_stop_async(
            self.native.pin_mut(),
            Box::new(RustEventLogProductStop(state.clone())),
            Box::new(RustEventLogRetention {
                _environment: (**environment).clone(),
            }),
        ))?;
        Ok(RtcEventLogStopOperation {
            state,
            consumed: false,
            _exclusive: PhantomData,
        })
    }
    pub fn close(&mut self) {
        ffi::event_log_utility_close(self.native.pin_mut());
        self.environment = None;
    }
}

#[cfg(test)]
#[path = "environment_event_log_tests.rs"]
mod utility_tests;

struct StopState {
    result: Option<Result<(), EventLogError>>,
    waker: Option<Waker>,
}
impl Drop for StopState {
    fn drop(&mut self) {
        let waker = self.waker.take();
        let _ = crate::provider_panic::catch_provider(|| drop(waker));
    }
}
pub(crate) struct RustEventLogProductStop(Arc<Mutex<StopState>>);
pub(crate) fn event_log_product_stopped(completion: &RustEventLogProductStop, failed: bool) {
    let waker = {
        let mut state = completion
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.result = Some(if failed {
            Err(EventLogError::ProviderFailed)
        } else {
            Ok(())
        });
        state.waker.take()
    };
    if let Some(waker) = waker {
        let _ = crate::provider_panic::catch_provider(|| waker.wake());
    }
}
/// Runtime-neutral, move-only Send completion. Success means native output
/// release has completed, not merely that stop was queued. Only one observer
/// can consume completion; polling after consumption is an application error.
pub struct RtcEventLogStopOperation {
    state: Arc<Mutex<StopState>>,
    consumed: bool,
    _exclusive: PhantomData<Cell<()>>,
}
impl RtcEventLogStopOperation {
    pub fn try_take(&mut self) -> Option<Result<(), EventLogError>> {
        if self.consumed {
            return None;
        }
        let result = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .result
            .take();
        if result.is_some() {
            self.consumed = true;
        }
        result
    }
}
impl Future for RtcEventLogStopOperation {
    type Output = Result<(), EventLogError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.consumed, "event-log stop completion already consumed");
        let result = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(result) = state.result.take() {
                Some(result)
            } else {
                state.waker = Some(cx.waker().clone());
                None
            }
        };
        if let Some(result) = result {
            self.consumed = true;
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
