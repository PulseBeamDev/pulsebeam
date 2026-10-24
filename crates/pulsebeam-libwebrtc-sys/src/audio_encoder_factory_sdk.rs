//! Real SDK encoder-factory dispatch and persistent standalone handles.
//! Neither registration nor caller handle is an installable builder dependency.
use crate::{
    AudioCodecInfo, AudioCodecSpec, AudioEncoder, AudioEncoderFactoryRegistration,
    AudioFactoryError, AudioFactoryFailureKind, AudioFactoryFailureObserver, AudioFactoryOperation,
    Environment, OwnedAudioEncoderOptions, SdpAudioFormat, ffi,
};
use std::sync::{Arc, Mutex};

pub(crate) struct RustAudioEncoderFactoryCall {
    registration: AudioEncoderFactoryRegistration,
    environment: Option<Environment>,
    // Per-operation binding error receipt, not cached capability/query output.
    outcome: Arc<Mutex<Option<AudioFactoryError>>>,
}
impl RustAudioEncoderFactoryCall {
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
impl AudioEncoderFactoryRegistration {
    /// Retain one refcounted SDK factory and this registration across operations.
    /// No callback runs at construction, and no peer/actor admission is implied.
    pub fn sdk_factory(&self) -> CallerAudioEncoderFactory {
        let owner = Box::new(RustAudioEncoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: Arc::new(Mutex::new(None)),
        });
        CallerAudioEncoderFactory(Arc::new(EncoderSdkFactoryState {
            native: Mutex::new(ffi::audio_caller_encoder_factory_new(owner)),
            registration: self.clone(),
        }))
    }
    /// Real SDK GetSupportedEncoders, preserving arbitrary standalone metadata.
    pub fn supported_encoders_via_sdk(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioEncoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: outcome.clone(),
        });
        let values = ffi::audio_caller_encoder_formats(call);
        receipt(&outcome)?;
        Ok(values
            .into_iter()
            .map(AudioCodecSpec::from_native)
            .collect())
    }
    /// Real SDK QueryAudioEncoder. Mutable query dispositions remain uncached;
    /// callback failure is an error, not an ordinary native unsupported result.
    pub fn query_encoder_via_sdk(
        &self,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioCodecInfo>, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioEncoderFactoryCall {
            registration: self.clone(),
            environment: None,
            outcome: outcome.clone(),
        });
        let value = ffi::audio_caller_encoder_query(call, &format.to_native());
        receipt(&outcome)?;
        Ok(value
            .present
            .then(|| AudioCodecInfo::from_native(value.info)))
    }
    /// Real SDK Create, passing an actual SDK Options copy without prequery or
    /// format-name/product inference. The returned mutable product must be unique.
    pub fn create_via_sdk(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        options: &OwnedAudioEncoderOptions,
    ) -> Result<Option<AudioEncoder>, AudioFactoryError> {
        let outcome = Arc::new(Mutex::new(None));
        let call = Box::new(RustAudioEncoderFactoryCall {
            registration: self.clone(),
            environment: Some(environment.clone()),
            outcome: outcome.clone(),
        });
        let native = ffi::audio_caller_encoder_create(
            call,
            environment.native(),
            &format.to_native(),
            options.native(),
        );
        receipt(&outcome)?;
        Ok(if native.is_null() {
            None
        } else {
            Some(AudioEncoder::from_native(native, environment.clone()))
        })
    }
}
pub(crate) fn audio_caller_encoder_environment(
    source: &RustAudioEncoderFactoryCall,
) -> cxx::UniquePtr<ffi::NativeEnvironment> {
    source
        .environment
        .as_ref()
        .map_or_else(cxx::UniquePtr::null, |environment| {
            ffi::clone_environment(environment.native())
        })
}
pub(crate) fn audio_caller_encoder_formats_callback(
    source: &RustAudioEncoderFactoryCall,
) -> Vec<ffi::FfiAudioCodecSpec> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.registration.supported_encoders() {
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
pub(crate) fn audio_caller_encoder_query_callback(
    source: &RustAudioEncoderFactoryCall,
    format: ffi::FfiSdpAudioFormat,
) -> ffi::FfiAudioEncoderQuery {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source
        .registration
        .query_encoder(&SdpAudioFormat::from_native(format))
    {
        Ok(Some(info)) => ffi::FfiAudioEncoderQuery {
            valid: true,
            present: true,
            info: info.to_native(),
        },
        Ok(None) => ffi::FfiAudioEncoderQuery {
            valid: true,
            present: false,
            info: empty_info(),
        },
        Err(error) => {
            source.report(error);
            ffi::FfiAudioEncoderQuery {
                valid: false,
                present: false,
                info: empty_info(),
            }
        }
    }
}
// Unobserved storage for an absent optional, not synthesized supported metadata.
fn empty_info() -> ffi::FfiAudioCodecInfo {
    ffi::FfiAudioCodecInfo {
        sample_rate_hz: 0,
        num_channels: 0,
        default_bitrate_bps: 0,
        min_bitrate_bps: 0,
        max_bitrate_bps: 0,
        allow_comfort_noise: false,
        supports_network_adaption: false,
    }
}
pub(crate) fn audio_caller_encoder_create_callback(
    source: &RustAudioEncoderFactoryCall,
    native_environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    format: ffi::FfiSdpAudioFormat,
    options: cxx::UniquePtr<ffi::NativeAudioEncoderOptions>,
) -> cxx::UniquePtr<ffi::NativeAudioEncoder> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let environment = Environment::from_sdk(native_environment, source.environment.as_ref());
    let format = SdpAudioFormat::from_native(format);
    let options = OwnedAudioEncoderOptions::from_native(options);
    let product = match source.registration.create(&environment, &format, &options) {
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
                operation: AudioFactoryOperation::CreateEncoder,
                kind: AudioFactoryFailureKind::ProductNotUnique,
            });
            return cxx::UniquePtr::null();
        }
    };
    let product = ffi::audio_encoder_forward_product(Box::new(transfer));
    if product.is_null() {
        audio_caller_encoder_transfer_failed(source);
    }
    product
}
struct EncoderSdkFactoryState {
    // Release the actual SDK owner (and its Rust callback owner) before the
    // retaining registration. The native pointer is never exposed for builders.
    native: Mutex<cxx::UniquePtr<ffi::NativeCallerAudioEncoderFactory>>,
    registration: AudioEncoderFactoryRegistration,
}
// SAFETY: one private refcounted SDK adapter lives in this wrapper. All virtual
// dispatch and scoped request-pointer mutation take native's mutex; the Rust
// registration separately serializes mutable callbacks. Borrowed requests are
// synchronous, outputs are owned, and no raw factory accessor is exposed.
unsafe impl Send for EncoderSdkFactoryState {}
unsafe impl Sync for EncoderSdkFactoryState {}
/// Persistent owning standalone SDK caller factory. Clones share one actual SDK
/// instance and serialized dispatch. This is deliberately not AudioEncoderFactory
/// and cannot be selected by peer or production builders (#37/#38 admission).
#[derive(Clone)]
pub struct CallerAudioEncoderFactory(Arc<EncoderSdkFactoryState>);
impl CallerAudioEncoderFactory {
    pub fn failure_observer(&self) -> AudioFactoryFailureObserver {
        self.0.registration.failure_observer()
    }
    fn invoke<T>(
        &self,
        operation: AudioFactoryOperation,
        environment: Option<&Environment>,
        call: impl FnOnce(&ffi::NativeCallerAudioEncoderFactory, &RustAudioEncoderFactoryCall) -> T,
    ) -> Result<T, AudioFactoryError> {
        // Scope keys the registration, not just this handle. Same-registration
        // reentry via another SDK handle is refused before either native mutex.
        let _scope = self.0.registration.sdk_scope(operation)?;
        let native = self.0.native.lock().unwrap_or_else(|e| e.into_inner());
        self.0.registration.sdk_entry(operation)?; // Recheck panic after waiting.
        let outcome = Arc::new(Mutex::new(None));
        let source = RustAudioEncoderFactoryCall {
            registration: self.0.registration.clone(),
            environment: environment.cloned(),
            outcome: outcome.clone(),
        };
        let value = call(
            native.as_ref().expect("owned SDK caller encoder factory"),
            &source,
        );
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn supported_encoders(&self) -> Result<Vec<AudioCodecSpec>, AudioFactoryError> {
        let values = self.invoke(
            AudioFactoryOperation::SupportedEncoders,
            None,
            ffi::audio_caller_encoder_factory_formats,
        )?;
        Ok(values
            .into_iter()
            .map(AudioCodecSpec::from_native)
            .collect())
    }
    pub fn query_encoder(
        &self,
        format: &SdpAudioFormat,
    ) -> Result<Option<AudioCodecInfo>, AudioFactoryError> {
        let value = self.invoke(
            AudioFactoryOperation::QueryEncoder,
            None,
            |native, source| {
                ffi::audio_caller_encoder_factory_query(native, source, &format.to_native())
            },
        )?;
        Ok(value
            .present
            .then(|| AudioCodecInfo::from_native(value.info)))
    }
    /// Independent actual SDK Create, passing exact Options and Environment.
    /// Returns only uniquely transferred existing native products, not a caller
    /// Encode adapter or stock fallback; no implicit support/Query occurs.
    pub fn create(
        &self,
        environment: &Environment,
        format: &SdpAudioFormat,
        options: &OwnedAudioEncoderOptions,
    ) -> Result<Option<AudioEncoder>, AudioFactoryError> {
        let native = self.invoke(
            AudioFactoryOperation::CreateEncoder,
            Some(environment),
            |native, source| {
                ffi::audio_caller_encoder_factory_create(
                    native,
                    source,
                    environment.native(),
                    &format.to_native(),
                    options.native(),
                )
            },
        )?;
        Ok(if native.is_null() {
            None
        } else {
            Some(AudioEncoder::from_native(native, environment.clone()))
        })
    }
}
pub(crate) fn audio_caller_encoder_transfer_failed(source: &RustAudioEncoderFactoryCall) {
    source.report(AudioFactoryError {
        operation: AudioFactoryOperation::CreateEncoder,
        kind: AudioFactoryFailureKind::ProductTransferFailed,
    });
}
