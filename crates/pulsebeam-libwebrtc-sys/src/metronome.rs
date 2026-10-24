//! Owned encode/decode metronomes with worker-sequence tick delivery.
use crate::provider_panic::catch_provider as catch_unwind;
use crate::{TimeDelta, ffi};
use std::{
    fmt,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Thread-compatible native metronome. Calls are serialized, but can migrate
/// between native worker threads when a registration is shared by factories.
/// Do not synchronously wait for or reenter the owning application actor.
///
/// Periods preserve native signed/infinite TimeDelta states. Choose a period
/// compatible with the consuming engine's clock arithmetic and scheduling.
/// Controlled factories require caller-driven implementations, without timer
/// threads or wall-clock progress. The bridge itself creates neither.
pub trait MetronomeProvider: Send + 'static {
    fn tick_period(&self) -> TimeDelta;
    /// The native default is a no-op. Retain the ticket until the next tick,
    /// then signal it once. Dropping it declines the callback, which can stall
    /// consumers requiring metronome progress. No engine cancellation API is
    /// implied by owning this one-shot delivery ticket.
    fn request_call_on_next_tick(&mut self, tick: MetronomeTick) {
        drop(tick);
    }
}

/// Shareable owned registration for either native metronome dependency.
/// Clones share the Rust provider and its failure latch. Native adapters own
/// independent pending requests and retire them before their worker is freed.
#[derive(Clone)]
pub struct Metronome(Arc<State>);
struct State {
    provider: Mutex<Option<Box<dyn MetronomeProvider>>>,
    failed: AtomicBool,
}
impl Drop for State {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(|| drop(provider));
    }
}
impl Metronome {
    pub fn from_provider(provider: impl MetronomeProvider) -> Self {
        Self(Arc::new(State {
            provider: Mutex::new(Some(Box::new(provider))),
            failed: AtomicBool::new(false),
        }))
    }
    /// Callback panics are contained at the native boundary and latch failure.
    /// Failed adapters return an infinite period and decline new requests.
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    pub(crate) fn bridge(&self) -> Box<RustMetronome> {
        Box::new(RustMetronome(Some(self.clone())))
    }
    pub(crate) fn absent() -> Box<RustMetronome> {
        Box::new(RustMetronome(None))
    }
}

/// Move-only one-shot tick. It can be signaled from any thread; delivery is
/// always asynchronous on the native requesting worker sequence. Retained
/// tickets cannot access a retired metronome or worker. Dropping an unsubmitted
/// ticket disposes the native callback on that same worker sequence.
pub struct MetronomeTick {
    native: cxx::UniquePtr<ffi::NativeMetronomeTick>,
}
// SAFETY: native submission and retirement share a cancellation mutex. Only
// admission uses the raw worker pointer, while it is protected from retirement.
// Invocation and capture disposal are dispatched onto the requesting worker.
unsafe impl Send for MetronomeTick {}
impl MetronomeTick {
    /// Acceptance is not acknowledgement of callback execution: factory
    /// retirement can cancel queued delivery, as with native client safety flags.
    pub fn signal(self) -> Result<(), MetronomeTickError> {
        if ffi::metronome_tick_signal(self.native) {
            Ok(())
        } else {
            Err(MetronomeTickError::Retired)
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetronomeTickError {
    Retired,
}
impl fmt::Display for MetronomeTickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the requesting metronome has retired")
    }
}
impl std::error::Error for MetronomeTickError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Field;
    impl Drop for Field {
        fn drop(&mut self) {
            panic!("payload field cleanup");
        }
    }
    struct Payload(Field);
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("payload cleanup");
        }
    }
    struct HostileProvider(Arc<AtomicUsize>);
    impl MetronomeProvider for HostileProvider {
        fn tick_period(&self) -> TimeDelta {
            std::panic::panic_any(Payload(Field));
        }
    }
    impl Drop for HostileProvider {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Payload(Field));
        }
    }
    #[test]
    fn hostile_period_and_final_provider_drop_remain_contained() {
        let drops = Arc::new(AtomicUsize::new(0));
        let metronome = Metronome::from_provider(HostileProvider(drops.clone()));
        let bridge = metronome.bridge();
        assert_eq!(metronome_tick_period(&bridge), i64::MAX);
        assert!(metronome.has_failed());
        assert_eq!(metronome_tick_period(&bridge), i64::MAX);
        drop(metronome);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(bridge);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!crate::provider_panic::callback_active());
    }
}

pub(crate) struct RustMetronome(Option<Metronome>);
pub(crate) fn metronome_present(source: &RustMetronome) -> bool {
    source.0.is_some()
}
pub(crate) fn metronome_tick_period(source: &RustMetronome) -> i64 {
    let Some(handle) = &source.0 else {
        return i64::MAX;
    };
    if handle.has_failed() {
        return i64::MAX;
    }
    let Ok(provider) = handle.0.provider.lock() else {
        handle.0.failed.store(true, Ordering::Release);
        return i64::MAX;
    };
    let Some(provider) = provider.as_ref() else {
        return i64::MAX;
    };
    match catch_unwind(AssertUnwindSafe(|| provider.tick_period())) {
        Ok(period) => period.native_microsecond_bits(),
        Err(_) => {
            handle.0.failed.store(true, Ordering::Release);
            i64::MAX
        }
    }
}
pub(crate) fn metronome_request_tick(
    source: &RustMetronome,
    native: cxx::UniquePtr<ffi::NativeMetronomeTick>,
) {
    let tick = MetronomeTick { native };
    let Some(handle) = &source.0 else {
        return;
    };
    if handle.has_failed() {
        return;
    }
    let Ok(mut provider) = handle.0.provider.lock() else {
        handle.0.failed.store(true, Ordering::Release);
        return;
    };
    let Some(provider) = provider.as_mut() else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| {
        provider.request_call_on_next_tick(tick)
    }))
    .is_err()
    {
        handle.0.failed.store(true, Ordering::Release);
    }
}
