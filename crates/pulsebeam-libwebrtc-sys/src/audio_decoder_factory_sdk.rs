//! Real SDK decoder-factory dispatch and persistent standalone handles.
//! Builder/NetEq installation still needs installed-consumer admission.
use crate::{
    AudioCodecPairId, AudioCodecSpec, AudioDecoder, AudioDecoderFactoryRegistration,
    AudioFactoryError, AudioFactoryFailureKind, AudioFactoryFailureObserver, AudioFactoryOperation,
    Environment, SdpAudioFormat, ffi,
};
use std::sync::{Arc, Mutex};

pub(crate) struct RustAudioDecoderFactoryCall {
    registration: AudioDecoderFactoryRegistration,
    environment: Option<Environment>,
    // Explicit per-operation error receipt. Never caches a query/codec or joins
    // an unrelated SDK identity; each synchronous operation owns this request.
    outcome: Arc<Mutex<Option<AudioFactoryError>>>,
}
impl RustAudioDecoderFactoryCall {
    fn report(&self, error: AudioFactoryError) {
        self.registration.failure_observer().report(error);
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(error);
    }
}
fn receipt(outcome: &Mutex<Option<AudioFactoryError>>) -> Result<(), AudioFactoryError> {
    match *outcome.lock().unwrap_or_else(|error| error.into_inner()) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
impl AudioDecoderFactoryRegistration {
    /// Retain one actual refcounted SDK factory across standalone operations.
    /// No callback at construction; this does not enable peer/NetEq installation.
    pub fn sdk_factory(&self) -> CallerAudioDecoderFactory {
        let owner = Box::new(RustAudioDecoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: Arc::new(Mutex::new(None)),
        });
        CallerAudioDecoderFactory(Arc::new(DecoderSdkFactoryState {
            native: Mutex::new(ffi::audio_caller_decoder_factory_new(owner)),
            registration: self.clone(),
        }))
    }
    /// Invoke the real SDK GetSupportedDecoders virtual. Arbitrary bytes and
    /// metadata remain representable; callback failure is not an empty success.
    pub fn supported_decoders_via_sdk(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioDecoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: outcome.clone(),
        });
        let values = ffi::audio_caller_decoder_formats(call);
        receipt(&outcome)?;
        Ok(values
            .into_iter()
            .map(AudioCodecSpec::from_native)
            .collect())
    }
    /// Real SDK IsSupportedDecoder virtual, without stock-parser restrictions
    /// on arbitrary caller formats or silent unsupported results on failure.
    pub fn is_supported_decoder_via_sdk(
        &self,
        format: &SdpAudioFormat,
    ) -> Result<bool, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioDecoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: outcome.clone(),
        });
        let supported = ffi::audio_caller_decoder_supported(call, &format.to_native());
        receipt(&outcome)?;
        Ok(supported)
    }
    /// Real current SDK Create, consuming a uniquely owned returned product.
    /// Create is not prefiltered by the provider's support-query result.
    pub fn create_via_sdk(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        self.create_sdk(environment, format, false, None)
    }
    /// Distinct compatibility SDK Create, preserving absent versus actual pair
    /// identity. Shared returned products/frames reject instead of being cloned.
    pub fn create_with_codec_pair_id_via_sdk(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        pair: Option<&AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        self.create_sdk(environment, format, true, pair)
    }
    fn create_sdk(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        compatibility: bool,
        pair: Option<&AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioDecoderFactoryCall {
            registration: self.clone(),
            environment: Some(environment.clone()),
            outcome: outcome.clone(),
        });
        let absent = cxx::UniquePtr::null();
        let native = ffi::audio_caller_decoder_create(
            call,
            environment.native(),
            &format.to_native(),
            compatibility,
            pair.map_or(&absent, AudioCodecPairId::native),
        );
        receipt(&outcome)?;
        Ok(if native.is_null() {
            None
        } else {
            Some(AudioDecoder::from_native(native, environment.clone()))
        })
    }
}

pub(crate) fn audio_caller_decoder_environment(
    source: &RustAudioDecoderFactoryCall,
) -> cxx::UniquePtr<ffi::NativeEnvironment> {
    source
        .environment
        .as_ref()
        .map_or_else(cxx::UniquePtr::null, |environment| {
            ffi::clone_environment(environment.native())
        })
}
pub(crate) fn audio_caller_decoder_formats_callback(
    source: &RustAudioDecoderFactoryCall,
) -> Vec<ffi::FfiAudioCodecSpec> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.registration.supported_decoders() {
        Ok(values) => values
            .into_iter()
            .map(|value| ffi::FfiAudioCodecSpec {
                format: value.format.to_native(),
                info: value.info.to_native(),
            })
            .collect(),
        Err(error) => {
            source.report(error);
            Vec::new()
        }
    }
}
pub(crate) fn audio_caller_decoder_supported_callback(
    source: &RustAudioDecoderFactoryCall,
    format: ffi::FfiSdpAudioFormat,
) -> bool {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source
        .registration
        .is_supported_decoder(&SdpAudioFormat::from_native(format))
    {
        Ok(value) => value,
        Err(error) => {
            source.report(error);
            false
        }
    }
}
pub(crate) fn audio_caller_decoder_create_callback(
    source: &RustAudioDecoderFactoryCall,
    native_environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    format: ffi::FfiSdpAudioFormat,
    compatibility: bool,
    pair: cxx::UniquePtr<ffi::NativeAudioCodecPairId>,
) -> cxx::UniquePtr<ffi::NativeAudioDecoder> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let environment = Environment::from_sdk(native_environment, source.environment.as_ref());
    let format = SdpAudioFormat::from_native(format);
    let pair = if pair.is_null() {
        None
    } else {
        Some(AudioCodecPairId::from_native(pair))
    };
    let operation = if compatibility {
        AudioFactoryOperation::CreateDecoderWithCodecPairId
    } else {
        AudioFactoryOperation::CreateDecoder
    };
    let result = if compatibility {
        source
            .registration
            .create_with_codec_pair_id(&environment, &format, pair.as_ref())
    } else {
        source.registration.create(&environment, &format)
    };
    let product = match result {
        Ok(Some(product)) => product,
        Ok(None) => return cxx::UniquePtr::null(),
        Err(error) => {
            source.report(error);
            return cxx::UniquePtr::null();
        }
    };
    let transfer = match product.try_into_factory_transfer() {
        Ok(transfer) => transfer,
        Err(_original) => {
            source.report(AudioFactoryError {
                operation,
                kind: AudioFactoryFailureKind::ProductNotUnique,
            });
            return cxx::UniquePtr::null();
        }
    };
    // Construct the actual SDK-derived forwarding envelope. It retains the
    // returned product's actual environment, even if the provider deliberately
    // used a different environment from its supplied creation argument.
    let product = ffi::audio_decoder_forward_product(Box::new(transfer));
    if product.is_null() {
        audio_caller_decoder_transfer_failed(source, compatibility);
    }
    product
}
struct DecoderSdkFactoryState {
    native: Mutex<cxx::UniquePtr<ffi::NativeCallerAudioDecoderFactory>>,
    registration: AudioDecoderFactoryRegistration,
}
// SAFETY: this private wrapper retains one refcounted caller SDK factory. All
// virtual dispatch/scoped request mutation is serialized by native's mutex and
// Rust mutable callbacks by the registration. Call borrows are synchronous and
// outputs own their bytes/contexts/products. No SDK factory accessor is exposed.
unsafe impl Send for DecoderSdkFactoryState {}
unsafe impl Sync for DecoderSdkFactoryState {}
/// Persistent standalone caller SDK decoder factory, not AudioDecoderFactory
/// or an installable peer/NetEq dependency. Clones retain one SDK instance.
#[derive(Clone)]
pub struct CallerAudioDecoderFactory(Arc<DecoderSdkFactoryState>);
impl CallerAudioDecoderFactory {
    pub fn failure_observer(&self) -> AudioFactoryFailureObserver {
        self.0.registration.failure_observer()
    }
    fn invoke<T>(
        &self,
        operation: AudioFactoryOperation,
        environment: Option<&Environment>,
        call: impl FnOnce(&ffi::NativeCallerAudioDecoderFactory, &RustAudioDecoderFactoryCall) -> T,
    ) -> Result<T, AudioFactoryError> {
        let _scope = self.0.registration.sdk_scope(operation)?;
        let native = self.0.native.lock().unwrap_or_else(|e| e.into_inner());
        self.0.registration.sdk_entry(operation)?;
        let outcome = Arc::new(Mutex::new(None));
        let source = RustAudioDecoderFactoryCall {
            registration: self.0.registration.clone(),
            environment: environment.cloned(),
            outcome: outcome.clone(),
        };
        let value = call(
            native.as_ref().expect("owned SDK caller decoder factory"),
            &source,
        );
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn supported_decoders(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        let values = self.invoke(
            AudioFactoryOperation::SupportedDecoders,
            None,
            ffi::audio_caller_decoder_factory_formats,
        )?;
        Ok(values
            .into_iter()
            .map(AudioCodecSpec::from_native)
            .collect())
    }
    pub fn is_supported_decoder(&self, format: &SdpAudioFormat) -> Result<bool, AudioFactoryError> {
        self.invoke(
            AudioFactoryOperation::IsSupportedDecoder,
            None,
            |native, source| {
                ffi::audio_caller_decoder_factory_supported(native, source, &format.to_native())
            },
        )
    }
    pub fn create(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        self.create_sdk(environment, format, false, None)
    }
    pub fn create_with_codec_pair_id(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        pair: Option<&AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        self.create_sdk(environment, format, true, pair)
    }
    fn create_sdk(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        compatibility: bool,
        pair: Option<&AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, AudioFactoryError> {
        let operation = if compatibility {
            AudioFactoryOperation::CreateDecoderWithCodecPairId
        } else {
            AudioFactoryOperation::CreateDecoder
        };
        let absent = cxx::UniquePtr::null();
        let native = self.invoke(operation, Some(environment), |native, source| {
            ffi::audio_caller_decoder_factory_create(
                native,
                source,
                environment.native(),
                &format.to_native(),
                compatibility,
                pair.map_or(&absent, AudioCodecPairId::native),
            )
        })?;
        Ok(if native.is_null() {
            None
        } else {
            Some(AudioDecoder::from_native(native, environment.clone()))
        })
    }
}
pub(crate) fn audio_caller_decoder_transfer_failed(
    source: &RustAudioDecoderFactoryCall,
    compatibility: bool,
) {
    source.report(AudioFactoryError {
        operation: if compatibility {
            AudioFactoryOperation::CreateDecoderWithCodecPairId
        } else {
            AudioFactoryOperation::CreateDecoder
        },
        kind: AudioFactoryFailureKind::ProductTransferFailed,
    });
}
