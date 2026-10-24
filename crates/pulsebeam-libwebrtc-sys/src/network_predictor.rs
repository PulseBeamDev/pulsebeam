use crate::provider_panic::catch_provider as catch_unwind;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Exact native bandwidth-usage domain, including the SDK sentinel and unknown
/// signed states. The predictor interface's scoped enum has an int underlying
/// type, so forwarding an unknown state does not manufacture an invalid enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandwidthUsage {
    Normal,
    Underusing,
    Overusing,
    Last,
    Unknown(i32),
}
impl BandwidthUsage {
    pub fn native_bits(self) -> i32 {
        match self {
            Self::Normal => 0,
            Self::Underusing => 1,
            Self::Overusing => 2,
            Self::Last => 3,
            Self::Unknown(value) => value,
        }
    }
    pub(crate) fn event_bits(self) -> Result<i32, crate::EventLogError> {
        match self.native_bits() {
            value @ 0..=2 => Ok(value),
            _ => Err(crate::EventLogError::InvalidEventEnum),
        }
    }
}
impl From<i32> for BandwidthUsage {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Normal,
            1 => Self::Underusing,
            2 => Self::Overusing,
            3 => Self::Last,
            other => Self::Unknown(other),
        }
    }
}

/// Owned predictor product used by native congestion-control packet processing.
/// Values are signed milliseconds exactly as passed by the SDK, not durations
/// or unsigned clock values. Calls are serialized; Send, not Sync, is required.
/// Providers must not block or reenter the calling session. Controlled providers
/// must perform only caller-controlled synchronous work.
pub trait NetworkStatePredictor: Send + 'static {
    fn update(
        &mut self,
        send_time_ms: i64,
        arrival_time_ms: i64,
        network_state: BandwidthUsage,
    ) -> BandwidthUsage;
}
/// Full native factory contract. None is the SDK's nullable product, not a
/// construction-time replacement by a stock predictor. Each product owns its
/// state independently of the factory. Creation and destruction may happen on
/// different native role sequences; no callback may require actor reentry.
pub trait NetworkStatePredictorFactoryProvider: Send + 'static {
    fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>>;
}
struct FactoryState {
    provider: Mutex<Option<Box<dyn NetworkStatePredictorFactoryProvider>>>,
    failed: AtomicBool,
}
impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
/// Shareable factory registration retained by the native peer factory for every
/// borrowed CallConfig reference. Clones do not share individual product state.
#[derive(Clone)]
pub struct NetworkStatePredictorFactory(Arc<FactoryState>);
impl NetworkStatePredictorFactory {
    pub fn from_provider(provider: impl NetworkStatePredictorFactoryProvider) -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(Box::new(provider))),
            failed: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}
pub(crate) struct RustNetworkStatePredictorFactory(pub Option<NetworkStatePredictorFactory>);
pub(crate) struct RustNetworkStatePredictor {
    provider: Mutex<Option<Box<dyn NetworkStatePredictor>>>,
    failed: AtomicBool,
}
impl Drop for RustNetworkStatePredictor {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
pub(crate) fn network_predictor_factory_present(source: &RustNetworkStatePredictorFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn network_predictor_create(
    source: &RustNetworkStatePredictorFactory,
) -> Box<RustNetworkStatePredictor> {
    let mut product = None;
    if let Some(factory) = &source.0
        && !factory.has_failed()
    {
        let mut provider = factory
            .0
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !factory.has_failed() {
            match catch_unwind(AssertUnwindSafe(|| {
                provider
                    .as_deref_mut()
                    .and_then(|provider| provider.create_network_state_predictor())
            })) {
                Ok(value) => product = value,
                Err(_) => factory.0.failed.store(true, Ordering::Release),
            }
        }
    }
    Box::new(RustNetworkStatePredictor {
        provider: Mutex::new(product),
        failed: AtomicBool::new(false),
    })
}
pub(crate) fn network_predictor_present(source: &RustNetworkStatePredictor) -> bool {
    source
        .provider
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
}
pub(crate) fn network_predictor_update(
    source: &RustNetworkStatePredictor,
    send_time_ms: i64,
    arrival_time_ms: i64,
    network_state: i32,
) -> i32 {
    if source.failed.load(Ordering::Acquire) {
        return network_state;
    }
    let mut provider = source
        .provider
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if source.failed.load(Ordering::Acquire) {
        return network_state;
    }
    let Some(provider) = provider.as_deref_mut() else {
        return network_state;
    };
    match catch_unwind(AssertUnwindSafe(|| {
        provider.update(send_time_ms, arrival_time_ms, network_state.into())
    })) {
        Ok(value) => value.native_bits(),
        Err(_) => {
            source.failed.store(true, Ordering::Release);
            network_state
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::atomic::AtomicUsize};
    type Calls = Arc<Mutex<Vec<(i64, i64, BandwidthUsage)>>>;
    struct Product {
        calls: Calls,
        mutable: Cell<usize>,
        drops: Arc<AtomicUsize>,
    }
    impl NetworkStatePredictor for Product {
        fn update(&mut self, send: i64, arrival: i64, state: BandwidthUsage) -> BandwidthUsage {
            self.mutable.set(self.mutable.get() + 1);
            self.calls.lock().unwrap().push((send, arrival, state));
            BandwidthUsage::Overusing
        }
    }
    impl Drop for Product {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Factory {
        calls: Calls,
        creates: Cell<usize>,
        drops: Arc<AtomicUsize>,
    }
    impl NetworkStatePredictorFactoryProvider for Factory {
        fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>> {
            self.creates.set(self.creates.get() + 1);
            Some(Box::new(Product {
                calls: self.calls.clone(),
                mutable: Cell::new(0),
                drops: self.drops.clone(),
            }))
        }
    }
    #[test]
    fn native_factory_products_keep_signed_values_after_factory_retirement() {
        fn shared<T: Send + Sync>() {}
        shared::<NetworkStatePredictorFactory>();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let registration = NetworkStatePredictorFactory::from_provider(Factory {
            calls: calls.clone(),
            creates: Cell::new(0),
            drops: drops.clone(),
        });
        let result = crate::ffi::network_predictor_native_probe(
            Box::new(RustNetworkStatePredictorFactory(Some(registration))),
            &[
                crate::ffi::FfiNetworkPredictorInput {
                    send_time_ms: i64::MIN,
                    arrival_time_ms: i64::MAX,
                    state: -17,
                },
                crate::ffi::FfiNetworkPredictorInput {
                    send_time_ms: -1,
                    arrival_time_ms: 0,
                    state: 3,
                },
            ],
        );
        assert_eq!(result, [2, 2]);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (i64::MIN, i64::MAX, BandwidthUsage::Unknown(-17)),
                (-1, 0, BandwidthUsage::Last)
            ]
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn factory_null_and_panics_preserve_nullable_native_products() {
        struct Null;
        impl NetworkStatePredictorFactoryProvider for Null {
            fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>> {
                None
            }
        }
        let null = NetworkStatePredictorFactory::from_provider(Null);
        let source = RustNetworkStatePredictorFactory(Some(null.clone()));
        assert!(network_predictor_factory_present(&source));
        assert!(!network_predictor_present(&network_predictor_create(
            &source
        )));
        assert!(!null.has_failed());
        struct Panic;
        impl NetworkStatePredictorFactoryProvider for Panic {
            fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>> {
                panic!("factory panic");
            }
        }
        impl Drop for Panic {
            fn drop(&mut self) {
                panic!("factory drop panic");
            }
        }
        let registration = NetworkStatePredictorFactory::from_provider(Panic);
        let source = RustNetworkStatePredictorFactory(Some(registration.clone()));
        for _ in 0..2 {
            assert!(!network_predictor_present(&network_predictor_create(
                &source
            )));
        }
        assert!(registration.has_failed());
        drop(source);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(registration))).is_ok());
    }
    #[test]
    fn native_update_preserves_unknown_return_states() {
        struct Passthrough;
        impl NetworkStatePredictor for Passthrough {
            fn update(&mut self, _: i64, _: i64, state: BandwidthUsage) -> BandwidthUsage {
                state
            }
        }
        impl NetworkStatePredictorFactoryProvider for Passthrough {
            fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>> {
                Some(Box::new(Passthrough))
            }
        }
        let states = [i32::MIN, -17, 0, 1, 2, 3, i32::MAX];
        let inputs = states.map(|state| crate::ffi::FfiNetworkPredictorInput {
            send_time_ms: -1,
            arrival_time_ms: 1,
            state,
        });
        let source = RustNetworkStatePredictorFactory(Some(
            NetworkStatePredictorFactory::from_provider(Passthrough),
        ));
        assert_eq!(
            crate::ffi::network_predictor_native_probe(Box::new(source), &inputs),
            states
        );
    }
    #[test]
    fn concurrent_factory_admission_rechecks_failure_under_lock() {
        struct Panic(Arc<AtomicUsize>);
        impl NetworkStatePredictorFactoryProvider for Panic {
            fn create_network_state_predictor(&mut self) -> Option<Box<dyn NetworkStatePredictor>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("creation failure");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = NetworkStatePredictorFactory::from_provider(Panic(calls.clone()));
        let source = Arc::new(RustNetworkStatePredictorFactory(Some(registration.clone())));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let joins: Vec<_> = (0..8)
            .map(|_| {
                let source = source.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    assert!(!network_predictor_present(&network_predictor_create(
                        &source
                    )));
                })
            })
            .collect();
        for join in joins {
            join.join().unwrap();
        }
        assert!(registration.has_failed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn product_panics_latch_passthrough_without_crossing_abi() {
        struct Panic(Arc<AtomicUsize>);
        impl NetworkStatePredictor for Panic {
            fn update(&mut self, _: i64, _: i64, _: BandwidthUsage) -> BandwidthUsage {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("update panic");
            }
        }
        impl Drop for Panic {
            fn drop(&mut self) {
                panic!("product drop panic");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let source = RustNetworkStatePredictor {
            provider: Mutex::new(Some(Box::new(Panic(calls.clone())))),
            failed: AtomicBool::new(false),
        };
        assert_eq!(network_predictor_update(&source, -1, 1, 2), 2);
        assert_eq!(
            network_predictor_update(&source, i64::MIN, i64::MAX, -17),
            -17
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(source.failed.load(Ordering::Acquire));
        assert!(catch_unwind(AssertUnwindSafe(|| drop(source))).is_ok());
    }
}
