//! Owning estimator products consumed by the selected native GoogCC factory.
use crate::provider_panic::catch_provider;
use crate::{
    FieldTrialsSnapshot, NetworkRouteChange, NetworkStateEstimate, PacketResult,
    TransportPacketsFeedback, ffi,
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

/// Full pinned, non-concurrent estimator contract. Calls are serialized by the
/// bridge. Providers must not block or reenter the calling session. Controlled
/// providers must do only synchronous caller-driven work.
pub trait NetworkStateEstimator: Send + 'static {
    fn get_current_estimate(&mut self) -> Option<NetworkStateEstimate>;
    fn on_transport_packets_feedback(&mut self, feedback: TransportPacketsFeedback);
    fn on_received_packet(&mut self, _packet: PacketResult) {}
    fn on_route_change(&mut self, route: NetworkRouteChange);
}

/// Creates independent owning products. None preserves the SDK's optional
/// estimator, never a fabricated estimate or stock estimator substitute.
/// `field_trials` is an owning copy of the actual supplied SDK view, or None
/// only when the native Create caller supplied a null pointer. GoogCC supplies
/// its actual controller environment's field trials.
pub trait NetworkStateEstimatorFactoryProvider: Send + 'static {
    fn create(
        &mut self,
        field_trials: Option<FieldTrialsSnapshot>,
    ) -> Option<Box<dyn NetworkStateEstimator>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkStateEstimatorFailure {
    Panicked,
    InvalidUnits,
}
fn report(observer: &AtomicU8, kind: u8) {
    let _ = observer.compare_exchange(0, kind, Ordering::AcqRel, Ordering::Acquire);
}
struct FactoryState {
    provider: Mutex<Option<Box<dyn NetworkStateEstimatorFactoryProvider>>>,
    failed: AtomicBool,
    observed_failure: Arc<AtomicU8>,
}
impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if catch_provider(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.failed.store(true, Ordering::Release);
            report(&self.observed_failure, 1);
        }
    }
}
/// Shareable registration. Install through GoogCcFactoryConfig and an explicit
/// NetworkControllerFactory::goog_cc selection. Clones do not share products.
#[derive(Clone)]
pub struct NetworkStateEstimatorFactory(Arc<FactoryState>);
impl NetworkStateEstimatorFactory {
    pub fn from_provider(provider: impl NetworkStateEstimatorFactoryProvider) -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(Box::new(provider))),
            failed: AtomicBool::new(false),
            observed_failure: Arc::new(AtomicU8::new(0)),
        }))
    }
    /// Factory-level panic. Product failures do not disable healthy siblings.
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    /// First observed factory/product bridge failure, not optional absence.
    pub fn observed_failure(&self) -> Option<NetworkStateEstimatorFailure> {
        match self.0.observed_failure.load(Ordering::Acquire) {
            1 => Some(NetworkStateEstimatorFailure::Panicked),
            2 => Some(NetworkStateEstimatorFailure::InvalidUnits),
            _ => None,
        }
    }
}
pub(crate) struct RustNetworkStateEstimatorFactory(pub Option<NetworkStateEstimatorFactory>);
pub(crate) struct RustNetworkStateEstimator {
    provider: Mutex<Option<Box<dyn NetworkStateEstimator>>>,
    failed: AtomicBool,
    observer: Arc<AtomicU8>,
    // Retain the actual input view independently of the factory and callback.
    _field_trials: Option<FieldTrialsSnapshot>,
}
impl Drop for RustNetworkStateEstimator {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if catch_provider(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.failed.store(true, Ordering::Release);
            report(&self.observer, 1);
        }
    }
}
pub(crate) fn network_estimator_factory_present(source: &RustNetworkStateEstimatorFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn network_estimator_create(
    source: &RustNetworkStateEstimatorFactory,
    trials: cxx::UniquePtr<ffi::NativeFieldTrials>,
) -> Box<RustNetworkStateEstimator> {
    let trials = if trials.is_null() {
        None
    } else {
        Some(FieldTrialsSnapshot::from_native(trials))
    };
    let observer = source
        .0
        .as_ref()
        .map(|factory| factory.0.observed_failure.clone())
        .unwrap_or_else(|| Arc::new(AtomicU8::new(0)));
    let mut product = None;
    if let Some(factory) = &source.0 {
        let mut provider = factory.0.provider.lock().unwrap_or_else(|e| e.into_inner());
        if !factory.has_failed() {
            match catch_provider(AssertUnwindSafe(|| {
                provider
                    .as_deref_mut()
                    .and_then(|provider| provider.create(trials.clone()))
            })) {
                Ok(value) => product = value,
                Err(_) => {
                    factory.0.failed.store(true, Ordering::Release);
                    report(&observer, 1);
                }
            }
        }
    }
    Box::new(RustNetworkStateEstimator {
        provider: Mutex::new(product),
        failed: AtomicBool::new(false),
        observer,
        _field_trials: trials,
    })
}
pub(crate) fn network_estimator_present(source: &RustNetworkStateEstimator) -> bool {
    source
        .provider
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}
impl RustNetworkStateEstimator {
    fn with<T>(&self, fallback: T, call: impl FnOnce(&mut dyn NetworkStateEstimator) -> T) -> T {
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let mut provider = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let Some(provider) = provider.as_deref_mut() else {
            return fallback;
        };
        match catch_provider(AssertUnwindSafe(|| call(provider))) {
            Ok(value) => value,
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                report(&self.observer, 1);
                fallback
            }
        }
    }
}
pub(crate) fn network_estimator_estimate(
    source: &RustNetworkStateEstimator,
) -> ffi::FfiNcOptionalEstimate {
    source.with(Default::default(), |provider| {
        match provider.get_current_estimate() {
            Some(value) => ffi::FfiNcOptionalEstimate {
                present: true,
                value: value.native(),
            },
            None => Default::default(),
        }
    })
}
pub(crate) fn network_estimator_feedback(
    source: &RustNetworkStateEstimator,
    value: ffi::FfiNcTransportPacketsFeedback,
) {
    source.with((), |provider| {
        provider.on_transport_packets_feedback(TransportPacketsFeedback::from_native(value))
    });
}
pub(crate) fn network_estimator_received(
    source: &RustNetworkStateEstimator,
    value: ffi::FfiNcPacketResult,
) {
    source.with((), |provider| {
        provider.on_received_packet(PacketResult::from_native(value))
    });
}
pub(crate) fn network_estimator_route(
    source: &RustNetworkStateEstimator,
    value: ffi::FfiNcNetworkRouteChange,
) {
    source.with((), |provider| {
        provider.on_route_change(NetworkRouteChange::from_native(value))
    });
}
pub(crate) fn network_estimator_invalid(source: &RustNetworkStateEstimator, kind: u8) {
    source.failed.store(true, Ordering::Release);
    report(&source.observer, kind);
}
