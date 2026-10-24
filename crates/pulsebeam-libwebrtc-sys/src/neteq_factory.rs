//! Owning factory callbacks for the pinned NetEq creation contract.
use crate::{
    AudioDecoderFactory, Environment, NetEq, NetEqConfig, NetEqFailureObserver, NetEqProvider, ffi,
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Actual owned creation inputs, not a replacement environment or factory.
/// Copies can outlive the callback, registration and product. The decoder view
/// retains the supplied SDK reference; capacity-sensitive operations require
/// exact selected-builtin provenance. A null SDK reference remains None.
#[derive(Clone)]
pub struct NetEqCreationContext {
    pub environment: Environment,
    pub config: NetEqConfig,
    pub decoder_factory: Option<AudioDecoderFactory>,
}

/// Factory callbacks are serialized, including callbacks from different SDK
/// sequences. Products must be independent owned values. Neither callback nor
/// destructor may wait for or reenter the owning peer/session. Controlled
/// providers must perform only caller-driven work, without autonomous progress.
///
/// Returning an error records construction failure. SDK installation must use
/// a non-null failed product for that request, never choose stock implicitly.
/// An ordinary rejection does not disable creation of unrelated products; a
/// panic disables future callbacks on this registration.
pub trait NetEqFactoryProvider: Send + 'static {
    fn create(
        &mut self,
        context: NetEqCreationContext,
    ) -> Result<Box<dyn NetEqProvider>, NetEqFactoryError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetEqFactoryError {
    Rejected(String),
    Panicked,
    ReentrantCall,
}
impl std::fmt::Display for NetEqFactoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => write!(f, "NetEq creation rejected: {message}"),
            Self::Panicked => f.write_str("NetEq factory callback panicked"),
            Self::ReentrantCall => f.write_str("NetEq factory callback reentered its registration"),
        }
    }
}
impl std::error::Error for NetEqFactoryError {}

#[derive(Clone, Default)]
pub struct NetEqFactoryFailureObserver(Arc<Mutex<Option<NetEqFactoryError>>>);
impl NetEqFactoryFailureObserver {
    /// First construction or destructor failure, retained after registration
    /// destruction. Product callback failures have their own observers.
    pub fn failure(&self) -> Option<NetEqFactoryError> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn report(&self, failure: NetEqFactoryError) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(failure);
    }
}
struct FactoryState {
    provider: Mutex<Option<Box<dyn NetEqFactoryProvider>>>,
    panicked: AtomicBool,
    observer: NetEqFactoryFailureObserver,
}
use crate::provider_panic::catch_provider as catch_unwind;

impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if catch_unwind(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.observer.report(NetEqFactoryError::Panicked);
        }
    }
}
/// Shareable owned registration. A mutex makes Send sufficient for a mutable
/// factory provider. Each returned product has independent serialized state.
#[derive(Clone)]
pub struct NetEqFactory(Arc<FactoryState>);
impl NetEqFactory {
    pub fn from_provider(provider: impl NetEqFactoryProvider) -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(Box::new(provider))),
            panicked: AtomicBool::new(false),
            observer: NetEqFactoryFailureObserver::default(),
        }))
    }
    pub fn failure_observer(&self) -> NetEqFactoryFailureObserver {
        self.0.observer.clone()
    }
    /// Invoke the native factory contract with retained input owners. This is
    /// standalone creation, not SDK receive-channel installation: raw getters
    /// remain raw, subject to the initialized bounded GetAudio contract.
    pub fn create(
        &self,
        environment: &Environment,
        config: &NetEqConfig,
        decoder_factory: &AudioDecoderFactory,
    ) -> Result<NetEq, NetEqFactoryError> {
        self.create_with_decoder_factory(environment, config, Some(decoder_factory))
    }
    /// Pass the exact optional factory to the native Create contract and owned
    /// callback context. None does not select a builtin or a failed product.
    pub fn create_with_decoder_factory(
        &self,
        environment: &Environment,
        config: &NetEqConfig,
        decoder_factory: Option<&AudioDecoderFactory>,
    ) -> Result<NetEq, NetEqFactoryError> {
        let factory =
            decoder_factory.map_or(std::ptr::null(), |factory| factory.native() as *const _);
        // SAFETY: the optional factory owner outlives synchronous dispatch. The
        // callback receives its own retained SDK reference, or genuine absence.
        let output = unsafe {
            ffi::neteq_factory_create(
                Box::new(RustNetEqFactory {
                    factory: self.clone(),
                    environment: environment.clone(),
                }),
                environment.native(),
                &config.to_native(),
                factory,
            )
        };
        match output.failure {
            1 => Err(NetEqFactoryError::Rejected(output.message)),
            2 => Err(NetEqFactoryError::Panicked),
            3 => Err(NetEqFactoryError::ReentrantCall),
            _ => Ok(NetEq::from_factory_native(
                output.product,
                environment.clone(),
                output.observer.0.clone(),
            )),
        }
    }
}

pub(crate) struct RustNetEqFailureObserver(pub(crate) NetEqFailureObserver);

pub(crate) struct RustNetEqFactory {
    pub(crate) factory: NetEqFactory,
    // Preserve Rust-only leases too. The native Environment retains actual SDK
    // dependencies, but not the process randomness lease used in controlled use.
    pub(crate) environment: Environment,
}
pub(crate) fn neteq_factory_create_product(
    source: &RustNetEqFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    config: ffi::FfiNetEqConfig,
    decoder_factory: cxx::UniquePtr<ffi::NativeAudioDecoderFactory>,
    sdk_consumer: bool,
) -> ffi::FfiNetEqFactoryCreation {
    let context = NetEqCreationContext {
        environment: Environment::from_sdk(environment, Some(&source.environment)),
        config: NetEqConfig::from_native(config),
        decoder_factory: AudioDecoderFactory::from_native(decoder_factory),
    };
    let scope = crate::neteq_callback_scope::NetEqCallbackScope::enter(Arc::as_ptr(
        &source.factory.0,
    ) as usize);
    let result = if scope.is_none() {
        Err(NetEqFactoryError::ReentrantCall)
    } else if source.factory.0.panicked.load(Ordering::Acquire) {
        Err(NetEqFactoryError::Panicked)
    } else {
        let mut provider = source
            .factory
            .0
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if source.factory.0.panicked.load(Ordering::Acquire) {
            Err(NetEqFactoryError::Panicked)
        } else {
            match catch_unwind(AssertUnwindSafe(|| {
                provider
                    .as_deref_mut()
                    .expect("live NetEq factory provider")
                    .create(context.clone())
            })) {
                Ok(result) => result,
                Err(_) => {
                    source.factory.0.panicked.store(true, Ordering::Release);
                    Err(NetEqFactoryError::Panicked)
                }
            }
        }
    };
    match result {
        Ok(provider) => {
            let observer = NetEqFailureObserver::default();
            let mut product = if sdk_consumer {
                crate::neteq_provider::RustNetEqProvider::new_for_sdk(
                    provider,
                    &context.config,
                    observer.clone(),
                )
            } else {
                crate::neteq_provider::RustNetEqProvider::new(
                    provider,
                    &context.config,
                    observer.clone(),
                )
            };
            product.retain_creation_environment(context.environment.clone());
            ffi::FfiNetEqFactoryCreation {
                product: ffi::neteq_provider_create(
                    context.environment.native(),
                    Box::new(product),
                ),
                failure: 0,
                message: String::new(),
                observer: Box::new(RustNetEqFailureObserver(observer)),
            }
        }
        Err(error) => {
            source.factory.0.observer.report(error.clone());
            let (failure, message) = match error {
                NetEqFactoryError::Rejected(message) => (1, message),
                NetEqFactoryError::Panicked => (2, String::new()),
                NetEqFactoryError::ReentrantCall => (3, String::new()),
            };
            // SDK receive channels dereference Create's result unconditionally.
            // Failure is observable and non-stock. The standalone helper returns
            // the typed error instead of exposing this failed result as success.
            let product = if sdk_consumer {
                ffi::neteq_failed_create_for_sdk(context.config.sample_rate_hz)
            } else {
                ffi::neteq_failed_create(context.config.sample_rate_hz)
            };
            ffi::FfiNetEqFactoryCreation {
                product,
                failure,
                message,
                observer: Box::new(RustNetEqFailureObserver(NetEqFailureObserver::default())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FailingFactory {
        panic: bool,
    }
    impl NetEqFactoryProvider for FailingFactory {
        fn create(
            &mut self,
            _: NetEqCreationContext,
        ) -> Result<Box<dyn NetEqProvider>, NetEqFactoryError> {
            if self.panic {
                panic!("factory creation panic");
            }
            Err(NetEqFactoryError::Rejected("failure contract".into()))
        }
    }
    #[test]
    fn native_create_never_returns_null_or_stock_after_factory_failure() {
        for panic in [false, true] {
            let environment = Environment::builder().build().unwrap();
            let decoder = AudioDecoderFactory::builtin_opus().unwrap();
            let factory = NetEqFactory::from_provider(FailingFactory { panic });
            let observer = factory.failure_observer();
            assert!(ffi::neteq_factory_failed_product_contract(
                Box::new(RustNetEqFactory {
                    factory,
                    environment: environment.clone()
                }),
                environment.native(),
                decoder.native()
            ));
            assert_eq!(
                observer.failure(),
                Some(if panic {
                    NetEqFactoryError::Panicked
                } else {
                    NetEqFactoryError::Rejected("failure contract".into())
                })
            );
        }
    }
}
