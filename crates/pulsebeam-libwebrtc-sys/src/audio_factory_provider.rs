//! Owned standalone caller-factory dispatch. Per-operation SDK factory drivers
//! and persistent standalone SDK handles live in audio_encoder_factory_sdk and
//! audio_decoder_factory_sdk. Safe installed-consumer admission remains open;
//! neither registration nor caller SDK handle is accepted by peer/builders.
use crate::{
    AudioCodecInfo, AudioCodecPairId, AudioCodecSpec, AudioDecoder, AudioEncoder, CodecError,
    Environment, OwnedAudioEncoderOptions, SdpAudioFormat,
};
use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Independent creation inputs. Copies retain the actual supplied environment,
/// including Rust-only leases, and an actual SDK Options value copy.
#[derive(Clone)]
pub struct AudioEncoderCreationContext {
    pub environment: Environment,
    pub format: SdpAudioFormat,
    pub options: OwnedAudioEncoderOptions,
}

/// Owned inputs common to both current decoder factory Create overloads.
#[derive(Clone)]
pub struct AudioDecoderCreationContext {
    pub environment: Environment,
    pub format: SdpAudioFormat,
    // Only tracks the SDK's mutually delegating default-method path. It is not
    // codec metadata, a query cache, or a restriction on user-returned values.
    default_forwarding: u8,
}
impl AudioDecoderCreationContext {
    fn enter_default(&mut self, method: u8) -> Result<(), CodecError> {
        if self.default_forwarding & method != 0 {
            // Neither Create overload was implemented. The SDK would recurse
            // forever; reject that actual unsafe default path, not as null.
            return Err(CodecError::ConstructionFailed);
        }
        self.default_forwarding |= method;
        Ok(())
    }
}

/// Mutable callbacks are serialized across caller threads. Do not wait for or
/// synchronously reenter the owning actor/factory in a callback or destructor.
/// Discovery and Query are independent from Create; dispatch does not query or
/// invent support before calling Create. Unsupported results are ordinary None.
/// Returned encoders are independently owning native products. Implementing a
/// native caller encoder adapter and installing this factory in the SDK remain
/// separate obligations, not capabilities supplied by this trait.
pub trait AudioEncoderFactoryProvider: Send + 'static {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec>;
    fn query_encoder(&mut self, format: &SdpAudioFormat) -> Option<AudioCodecInfo>;
    fn create(
        &mut self,
        context: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError>;
}

/// Current and compatibility Create remain distinct. Implement either overload
/// or both: their defaults preserve the SDK's direction of delegation. If both
/// are left at their defaults, dispatch reports construction failure before the
/// SDK's unbounded recursive default path. The unsafe legacy decoder operation
/// family is not exposed by this contract.
pub trait AudioDecoderFactoryProvider: Send + 'static {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec>;
    fn is_supported_decoder(&mut self, format: &SdpAudioFormat) -> bool;
    fn create(
        &mut self,
        mut context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        context.enter_default(1)?;
        self.create_with_codec_pair_id(context, None)
    }
    fn create_with_codec_pair_id(
        &mut self,
        mut context: AudioDecoderCreationContext,
        _codec_pair_id: Option<AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        context.enter_default(2)?;
        self.create(context)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFactoryOperation {
    SupportedEncoders,
    QueryEncoder,
    CreateEncoder,
    SupportedDecoders,
    IsSupportedDecoder,
    CreateDecoder,
    CreateDecoderWithCodecPairId,
    Destroy,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFactoryFailureKind {
    Provider(CodecError),
    Panicked,
    ReentrantCall,
    /// An earlier callback panic disabled this registration. The observer
    /// retains the original operation and cause, rather than replacing it.
    Disabled,
    /// The returned mutable product or its frames still have other owners.
    ProductNotUnique,
    /// A consuming native forwarding envelope violated its private contract.
    ProductTransferFailed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFactoryError {
    pub operation: AudioFactoryOperation,
    pub kind: AudioFactoryFailureKind,
}
impl std::fmt::Display for AudioFactoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audio factory {:?}: {:?}", self.operation, self.kind)
    }
}
impl std::error::Error for AudioFactoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AudioFactoryFailureKind::Provider(error) => Some(error),
            _ => None,
        }
    }
}
#[derive(Default)]
struct FailureState {
    first: Mutex<Option<AudioFactoryError>>,
    disabled: AtomicBool,
}
/// First callback/reentry/construction/destructor failure. Observers outlive
/// registrations. Rejection/reentry is per-call; only panic disables callbacks.
#[derive(Clone, Default)]
pub struct AudioFactoryFailureObserver(Arc<FailureState>);
impl AudioFactoryFailureObserver {
    pub fn failure(&self) -> Option<AudioFactoryError> {
        *self
            .0
            .first
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
    pub fn is_disabled(&self) -> bool {
        self.0.disabled.load(Ordering::Acquire)
    }
    pub(crate) fn report(&self, error: AudioFactoryError) {
        self.0
            .first
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(error);
    }
    fn disable(&self) {
        self.0.disabled.store(true, Ordering::Release);
    }
}

thread_local! {
    // Addresses identify only live Arc-owned dispatch states on this thread;
    // they are never dereferenced or exposed as application/native identities.
    static ACTIVE_FACTORIES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    // Separate from callback dispatch: the SDK virtual must still enter the
    // provider once. This outer scope rejects reentry before a handle mutex,
    // including via a different persistent handle of the same registration.
    static ACTIVE_SDK_FACTORIES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}
pub(crate) struct AudioSdkFactoryScope(usize);
impl Drop for AudioSdkFactoryScope {
    fn drop(&mut self) {
        ACTIVE_SDK_FACTORIES.with(|active| {
            let mut active = active.borrow_mut();
            let index = active
                .iter()
                .rposition(|id| *id == self.0)
                .expect("active SDK factory scope");
            active.remove(index);
        });
    }
}
struct ActiveFactory(usize);
impl ActiveFactory {
    fn enter(identity: usize) -> Option<Self> {
        ACTIVE_FACTORIES.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&identity) {
                None
            } else {
                active.push(identity);
                Some(Self(identity))
            }
        })
    }
}
impl Drop for ActiveFactory {
    fn drop(&mut self) {
        ACTIVE_FACTORIES.with(|active| {
            let mut active = active.borrow_mut();
            let index = active
                .iter()
                .rposition(|identity| *identity == self.0)
                .expect("active factory scope");
            active.remove(index);
        });
    }
}
struct FactoryState<P: ?Sized> {
    provider: Mutex<Option<Box<P>>>,
    observer: AudioFactoryFailureObserver,
}
impl<P: ?Sized> FactoryState<P> {
    fn sdk_scope(
        &self,
        operation: AudioFactoryOperation,
    ) -> Result<AudioSdkFactoryScope, AudioFactoryError> {
        self.sdk_entry(operation)?;
        let identity = self as *const Self as *const () as usize;
        let scope = ACTIVE_SDK_FACTORIES.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&identity) {
                None
            } else {
                active.push(identity);
                Some(AudioSdkFactoryScope(identity))
            }
        });
        scope.ok_or_else(|| {
            let error = AudioFactoryError {
                operation,
                kind: AudioFactoryFailureKind::ReentrantCall,
            };
            self.observer.report(error);
            error
        })
    }
    fn sdk_entry(&self, operation: AudioFactoryOperation) -> Result<(), AudioFactoryError> {
        let identity = self as *const Self as *const () as usize;
        let kind = if self.observer.is_disabled() {
            Some(AudioFactoryFailureKind::Disabled)
        } else if ACTIVE_FACTORIES.with(|active| active.borrow().contains(&identity)) {
            Some(AudioFactoryFailureKind::ReentrantCall)
        } else {
            None
        };
        if let Some(kind) = kind {
            let error = AudioFactoryError { operation, kind };
            self.observer.report(error);
            Err(error)
        } else {
            Ok(())
        }
    }
    fn invoke<R>(
        &self,
        operation: AudioFactoryOperation,
        call: impl FnOnce(&mut P) -> Result<R, CodecError>,
    ) -> Result<R, AudioFactoryError> {
        let failure = |kind| {
            let error = AudioFactoryError { operation, kind };
            self.observer.report(error);
            error
        };
        if self.observer.is_disabled() {
            return Err(failure(AudioFactoryFailureKind::Disabled));
        }
        let _active = ActiveFactory::enter(self as *const Self as *const () as usize)
            .ok_or_else(|| failure(AudioFactoryFailureKind::ReentrantCall))?;
        let mut provider = self
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A different thread may have panicked while this invocation waited.
        if self.observer.is_disabled() {
            return Err(failure(AudioFactoryFailureKind::Disabled));
        }
        let provider = provider
            .as_deref_mut()
            .expect("live factory callback owner");
        // The lock guard is outside catch_unwind, so a caught callback panic
        // cannot poison the state mutex or drop the provider inside unwinding.
        match catch_unwind(AssertUnwindSafe(|| call(provider))) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(failure(AudioFactoryFailureKind::Provider(error))),
            Err(payload) => {
                let error = failure(AudioFactoryFailureKind::Panicked);
                self.observer.disable();
                crate::panic_payload::discard(payload);
                Err(error)
            }
        }
    }
}
impl<P: ?Sized> Drop for FactoryState<P> {
    fn drop(&mut self) {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(provider))) {
            self.observer.report(AudioFactoryError {
                operation: AudioFactoryOperation::Destroy,
                kind: AudioFactoryFailureKind::Panicked,
            });
            self.observer.disable();
            crate::panic_payload::discard(payload);
        }
    }
}

/// Owning standalone caller registration. Clones share serialized callbacks;
/// returned metadata/contexts/products own their inputs independently. This is
/// not itself a native SDK factory handle. sdk_factory supplies a distinct
/// persistent standalone SDK owner, still not installable into builders.
#[derive(Clone)]
pub struct AudioEncoderFactoryRegistration(Arc<FactoryState<dyn AudioEncoderFactoryProvider>>);
impl AudioEncoderFactoryRegistration {
    pub(crate) fn sdk_scope(
        &self,
        operation: AudioFactoryOperation,
    ) -> Result<AudioSdkFactoryScope, AudioFactoryError> {
        self.0.sdk_scope(operation)
    }
    pub(crate) fn sdk_entry(
        &self,
        operation: AudioFactoryOperation,
    ) -> Result<(), AudioFactoryError> {
        self.0.sdk_entry(operation)
    }
    pub fn from_provider(provider: impl AudioEncoderFactoryProvider) -> Self {
        let provider: Box<dyn AudioEncoderFactoryProvider> = Box::new(provider);
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(provider)),
            observer: AudioFactoryFailureObserver::default(),
        }))
    }
    pub fn failure_observer(&self) -> AudioFactoryFailureObserver {
        self.0.observer.clone()
    }
    pub fn supported_encoders(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        self.0
            .invoke(AudioFactoryOperation::SupportedEncoders, |provider| {
                Ok(provider.supported_encoders())
            })
    }
    pub fn query_encoder(
        &self,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioCodecInfo>, AudioFactoryError> {
        self.0
            .invoke(AudioFactoryOperation::QueryEncoder, |provider| {
                Ok(provider.query_encoder(format))
            })
    }
    pub fn create(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        options: &OwnedAudioEncoderOptions,
    ) -> Result<Option<AudioEncoder>, AudioFactoryError> {
        let context = AudioEncoderCreationContext {
            environment: environment.clone(),
            format: format.clone(),
            options: options.clone(),
        };
        self.0
            .invoke(AudioFactoryOperation::CreateEncoder, |provider| {
                provider.create(context)
            })
    }
}

/// Standalone caller decoder registration, not a selected SDK decoder factory.
/// No stock-format parser or stock capacity proof is inferred from metadata.
#[derive(Clone)]
pub struct AudioDecoderFactoryRegistration(Arc<FactoryState<dyn AudioDecoderFactoryProvider>>);
impl AudioDecoderFactoryRegistration {
    pub(crate) fn sdk_scope(
        &self,
        operation: AudioFactoryOperation,
    ) -> Result<AudioSdkFactoryScope, AudioFactoryError> {
        self.0.sdk_scope(operation)
    }
    pub(crate) fn sdk_entry(
        &self,
        operation: AudioFactoryOperation,
    ) -> Result<(), AudioFactoryError> {
        self.0.sdk_entry(operation)
    }
    pub fn from_provider(provider: impl AudioDecoderFactoryProvider) -> Self {
        let provider: Box<dyn AudioDecoderFactoryProvider> = Box::new(provider);
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(provider)),
            observer: AudioFactoryFailureObserver::default(),
        }))
    }
    pub fn failure_observer(&self) -> AudioFactoryFailureObserver {
        self.0.observer.clone()
    }
    pub fn supported_decoders(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        self.0
            .invoke(AudioFactoryOperation::SupportedDecoders, |provider| {
                Ok(provider.supported_decoders())
            })
    }
    pub fn is_supported_decoder(&self, format: &SdpAudioFormat) -> Result<bool, AudioFactoryError> {
        self.0
            .invoke(AudioFactoryOperation::IsSupportedDecoder, |provider| {
                Ok(provider.is_supported_decoder(format))
            })
    }
    pub fn create(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        let context = AudioDecoderCreationContext {
            environment: environment.clone(),
            format: format.clone(),
            default_forwarding: 0,
        };
        self.0
            .invoke(AudioFactoryOperation::CreateDecoder, |provider| {
                provider.create(context)
            })
    }
    pub fn create_with_codec_pair_id(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        codec_pair_id: Option<&AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        let context = AudioDecoderCreationContext {
            environment: environment.clone(),
            format: format.clone(),
            default_forwarding: 0,
        };
        let pair = codec_pair_id.map(AudioCodecPairId::copy_value);
        self.0.invoke(
            AudioFactoryOperation::CreateDecoderWithCodecPairId,
            |provider| provider.create_with_codec_pair_id(context, pair),
        )
    }
}
