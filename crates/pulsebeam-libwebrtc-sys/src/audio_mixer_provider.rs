//! Caller-owned mixer implementation and scoped access to its registered sources.
use crate::{AudioFrame, AudioMixerError, AudioMixerFailureObserver, MixerAudioFrame, ffi};
use std::{
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::Mutex,
};

/// Registration identity, not an SSRC or a pointer supplied by an application.
/// Re-adding a removed native source creates a new identity. Tokens may outlive
/// removal, but cannot access a source except through a current mix context.
pub struct AudioMixerSourceToken {
    native: cxx::UniquePtr<ffi::NativeMixerSourceToken>,
}
// Token operations only clone/compare immutable registration identity. They
// never dereference the SDK source; that requires a scoped, locked input loan.
unsafe impl Send for AudioMixerSourceToken {}
unsafe impl Sync for AudioMixerSourceToken {}
impl Clone for AudioMixerSourceToken {
    fn clone(&self) -> Self {
        Self {
            native: ffi::mixer_token_clone(self.inner()),
        }
    }
}
impl PartialEq for AudioMixerSourceToken {
    fn eq(&self, other: &Self) -> bool {
        ffi::mixer_token_equal(self.inner(), other.inner())
    }
}
impl Eq for AudioMixerSourceToken {}
impl AudioMixerSourceToken {
    fn inner(&self) -> &ffi::NativeMixerSourceToken {
        self.native.as_ref().expect("owned mixer source token")
    }
}

/// The native mix call holds the registration lock for this entire loan.
/// No raw SDK source or callable source view can escape the callback. Foreign
/// and retired tokens are errors, including when a native address is reused.
pub struct AudioMixerInputs<'a> {
    native: &'a ffi::NativeMixerInputs,
    _sequence: PhantomData<Rc<()>>,
}
impl AudioMixerInputs<'_> {
    pub fn ssrc(&mut self, source: &AudioMixerSourceToken) -> Result<i32, AudioMixerError> {
        let mut error = 0;
        let value = ffi::mixer_input_ssrc(self.native, source.inner(), &mut error);
        admission(error)?;
        Ok(value)
    }
    pub fn preferred_sample_rate(
        &mut self,
        source: &AudioMixerSourceToken,
    ) -> Result<i32, AudioMixerError> {
        let mut error = 0;
        let value = ffi::mixer_input_rate(self.native, source.inner(), &mut error);
        admission(error)?;
        Ok(value)
    }
    /// Native mix sources are driven at an AudioProcessing::NativeRate. This is
    /// the SDK mix contract, not a preference whitelist: preferred rates remain
    /// open integers and the caller chooses its own output-rate policy.
    pub fn audio_frame(
        &mut self,
        source: &AudioMixerSourceToken,
        rate: i32,
    ) -> Result<MixerAudioFrame, AudioMixerError> {
        let mut error = 0;
        let result = ffi::mixer_input_frame(self.native, source.inner(), rate, &mut error);
        admission(error)?;
        if result.disposition == 2 {
            return Ok(MixerAudioFrame::Error);
        }
        if result.frame.is_null() {
            return Err(AudioMixerError::InvalidFrame);
        }
        let frame = AudioFrame::from_native(result.frame);
        Ok(if result.disposition == 1 {
            MixerAudioFrame::Muted(frame)
        } else {
            MixerAudioFrame::Normal(frame)
        })
    }
}
/// Registration-time source information. Unlike the mix context it cannot drive
/// GetAudioFrame on a native registration/retirement thread.
pub struct AudioMixerSourceInfo<'a> {
    native: &'a ffi::NativeMixerInputs,
    _sequence: PhantomData<Rc<()>>,
}
impl AudioMixerSourceInfo<'_> {
    pub fn ssrc(&mut self, source: &AudioMixerSourceToken) -> Result<i32, AudioMixerError> {
        let mut error = 0;
        let value = ffi::mixer_input_ssrc(self.native, source.inner(), &mut error);
        admission(error)?;
        Ok(value)
    }
    pub fn preferred_sample_rate(
        &mut self,
        source: &AudioMixerSourceToken,
    ) -> Result<i32, AudioMixerError> {
        let mut error = 0;
        let value = ffi::mixer_input_rate(self.native, source.inner(), &mut error);
        admission(error)?;
        Ok(value)
    }
}
fn admission(error: u8) -> Result<(), AudioMixerError> {
    match error {
        0 => Ok(()),
        1 => Err(AudioMixerError::ReentrantCall),
        2 => Err(AudioMixerError::InvalidPreferredRate),
        3 => Err(AudioMixerError::InvalidFrame),
        5 => Err(AudioMixerError::InvalidSource),
        6 => Err(AudioMixerError::InvalidSampleRate),
        _ => Err(AudioMixerError::CallbackRejected),
    }
}

/// The exact selected implementation receives native AddSource/RemoveSource and
/// Mix calls. Returning false from add is ordinary native refusal, distinct from
/// Err/panic. Native removal always retires access even if the provider fails.
/// Callbacks are serialized, including native add/remove on other threads.
pub trait AudioMixerProvider: Send + 'static {
    fn add_source(
        &mut self,
        source: AudioMixerSourceToken,
        info: &mut AudioMixerSourceInfo<'_>,
    ) -> Result<bool, AudioMixerError>;
    fn remove_source(
        &mut self,
        source: AudioMixerSourceToken,
        info: &mut AudioMixerSourceInfo<'_>,
    ) -> Result<(), AudioMixerError>;
    fn mix(
        &mut self,
        channels: usize,
        inputs: &mut AudioMixerInputs<'_>,
    ) -> Result<AudioFrame, AudioMixerError>;
}
pub(crate) struct RustMixerProvider {
    provider: Mutex<Option<Box<dyn AudioMixerProvider>>>,
    failure: AudioMixerFailureObserver,
}
impl RustMixerProvider {
    pub(crate) fn new(
        provider: Box<dyn AudioMixerProvider>,
        failure: AudioMixerFailureObserver,
    ) -> Self {
        Self {
            provider: Mutex::new(Some(provider)),
            failure,
        }
    }
    fn call<T>(
        &self,
        f: impl FnOnce(&mut dyn AudioMixerProvider) -> Result<T, AudioMixerError>,
    ) -> Result<T, AudioMixerError> {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let result = (|| {
            let _scope = crate::audio_mixer::Scope::enter(self as *const Self as usize)?;
            let mut provider = self.provider.lock().unwrap_or_else(|e| e.into_inner());
            match catch_unwind(AssertUnwindSafe(|| match provider.as_deref_mut() {
                Some(p) => f(p),
                None => Err(AudioMixerError::CallbackRejected),
            })) {
                Ok(result) => result,
                Err(payload) => {
                    crate::audio_processor_caller_dump::dispose_panic(payload);
                    Err(AudioMixerError::CallbackPanicked)
                }
            }
        })();
        if let Err(error) = result {
            self.failure.record(error);
        }
        result
    }
}
impl Drop for RustMixerProvider {
    fn drop(&mut self) {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(provider))) {
            crate::audio_processor_caller_dump::dispose_panic(payload);
            self.failure.record(AudioMixerError::CallbackPanicked);
        }
    }
}
pub(crate) fn mixer_provider_add(
    provider: &RustMixerProvider,
    token: cxx::UniquePtr<ffi::NativeMixerSourceToken>,
    inputs: &ffi::NativeMixerInputs,
    failed: &mut bool,
) -> bool {
    let mut info = AudioMixerSourceInfo {
        native: inputs,
        _sequence: PhantomData,
    };
    match provider.call(|p| p.add_source(AudioMixerSourceToken { native: token }, &mut info)) {
        Ok(added) => {
            *failed = false;
            added
        }
        Err(_) => {
            *failed = true;
            false
        }
    }
}
pub(crate) fn mixer_provider_remove(
    provider: &RustMixerProvider,
    token: cxx::UniquePtr<ffi::NativeMixerSourceToken>,
    inputs: &ffi::NativeMixerInputs,
) -> bool {
    let mut info = AudioMixerSourceInfo {
        native: inputs,
        _sequence: PhantomData,
    };
    provider
        .call(|p| p.remove_source(AudioMixerSourceToken { native: token }, &mut info))
        .is_ok()
}
pub(crate) fn mixer_provider_mix(
    provider: &RustMixerProvider,
    channels: usize,
    inputs: &ffi::NativeMixerInputs,
) -> ffi::FfiMixerFrame {
    let mut inputs = AudioMixerInputs {
        native: inputs,
        _sequence: PhantomData,
    };
    match provider.call(|p| p.mix(channels, &mut inputs)) {
        Ok(frame) => ffi::FfiMixerFrame {
            disposition: 0,
            frame: frame.native,
            failed: false,
        },
        Err(_) => ffi::FfiMixerFrame {
            disposition: 2,
            frame: cxx::UniquePtr::null(),
            failed: true,
        },
    }
}
pub(crate) fn mixer_provider_invalid(provider: &RustMixerProvider) {
    provider.failure.record(AudioMixerError::InvalidFrame);
}
