//! Owning stock mixer and serialized, retained participant callbacks.
use crate::{AudioFrame, ffi};
use std::{
    cell::RefCell,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioMixerError {
    CreationFailed,
    InvalidChannels,
    InvalidPreferredRate,
    InvalidSampleRate,
    InvalidFrame,
    InvalidSource,
    CallbackRejected,
    CallbackPanicked,
    ReentrantCall,
}
impl std::fmt::Display for AudioMixerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for AudioMixerError {}

/// Native kError is an ordinary source disposition, not a callback failure.
#[derive(Debug)]
pub enum MixerAudioFrame {
    Normal(AudioFrame),
    Muted(AudioFrame),
    Error,
}
/// Calls are serialized even when one source is registered with multiple mixers.
/// Normal and muted frames must describe exactly the requested 10-ms format,
/// with one or two channels. Muted frame storage is never consumed.
pub trait AudioMixerSourceProvider: Send + 'static {
    fn audio_frame(&mut self, sample_rate_hz: i32) -> Result<MixerAudioFrame, AudioMixerError>;
    fn ssrc(&mut self) -> Result<i32, AudioMixerError>;
    fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError>;
}
#[derive(Clone, Default)]
pub struct AudioMixerFailureObserver(Arc<Mutex<Option<AudioMixerError>>>);
impl AudioMixerFailureObserver {
    pub fn failure(&self) -> Option<AudioMixerError> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn record(&self, e: AudioMixerError) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(e);
    }
}
pub(crate) struct RustMixerModule(pub AudioMixerFailureObserver);
pub(crate) fn mixer_module_failed(module: &RustMixerModule, error: u8) {
    module.0.record(match error {
        1 => AudioMixerError::ReentrantCall,
        2 => AudioMixerError::InvalidPreferredRate,
        3 => AudioMixerError::InvalidFrame,
        _ => AudioMixerError::CallbackRejected,
    });
}
thread_local! { static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) }; }
pub(crate) struct Scope(usize);
impl Scope {
    pub(crate) fn enter(id: usize) -> Result<Self, AudioMixerError> {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&id) {
                return Err(AudioMixerError::ReentrantCall);
            }
            active.push(id);
            Ok(Self(id))
        })
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            let mut a = a.borrow_mut();
            debug_assert_eq!(a.pop(), Some(self.0));
        });
    }
}
pub(crate) struct RustMixerSource {
    provider: Mutex<Option<Box<dyn AudioMixerSourceProvider>>>,
    failure: AudioMixerFailureObserver,
}
impl RustMixerSource {
    fn call<T>(
        &self,
        f: impl FnOnce(&mut dyn AudioMixerSourceProvider) -> Result<T, AudioMixerError>,
    ) -> Result<T, AudioMixerError> {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let result = (|| {
            let _scope = Scope::enter(self as *const Self as usize)?;
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
impl Drop for RustMixerSource {
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
pub(crate) fn mixer_source_frame(source: &RustMixerSource, rate: i32) -> ffi::FfiMixerFrame {
    let mut result = ffi::FfiMixerFrame {
        disposition: 2,
        frame: cxx::UniquePtr::null(),
        failed: false,
    };
    match source.call(|p| p.audio_frame(rate)) {
        Ok(MixerAudioFrame::Normal(frame)) => {
            result.disposition = 0;
            result.frame = frame.native;
        }
        Ok(MixerAudioFrame::Muted(frame)) => {
            result.disposition = 1;
            result.frame = frame.native;
        }
        Ok(MixerAudioFrame::Error) => {}
        Err(_) => result.failed = true,
    }
    result
}
pub(crate) fn mixer_source_ssrc(source: &RustMixerSource, valid: &mut bool) -> i32 {
    match source.call(|p| p.ssrc()) {
        Ok(v) => {
            *valid = true;
            v
        }
        Err(_) => {
            *valid = false;
            0
        }
    }
}
pub(crate) fn mixer_source_rate(source: &RustMixerSource, valid: &mut bool) -> i32 {
    match source.call(|p| p.preferred_sample_rate()) {
        Ok(v) => {
            *valid = true;
            v
        }
        Err(_) => {
            *valid = false;
            0
        }
    }
}
pub(crate) fn mixer_source_invalid(source: &RustMixerSource) {
    source.failure.record(AudioMixerError::InvalidFrame);
}

/// Clones identify the exact same participant. Registration retains it even
/// after the outer source handle is dropped. No SSRC-based identity join.
#[derive(Clone)]
pub struct AudioMixerSource {
    native: Arc<cxx::UniquePtr<ffi::NativeMixerSource>>,
    failure: AudioMixerFailureObserver,
}
// SAFETY: native source state is immutable apart from its Mutex-serialized Rust provider.
unsafe impl Send for ffi::NativeMixerSource {}
// SAFETY: concurrent native source calls use that same serialized provider.
unsafe impl Sync for ffi::NativeMixerSource {}
impl AudioMixerSource {
    pub fn from_provider(provider: impl AudioMixerSourceProvider) -> Self {
        let failure = AudioMixerFailureObserver::default();
        let native = ffi::mixer_source_new(Box::new(RustMixerSource {
            provider: Mutex::new(Some(Box::new(provider))),
            failure: failure.clone(),
        }));
        Self {
            native: Arc::new(native),
            failure,
        }
    }
    pub fn failure_observer(&self) -> AudioMixerFailureObserver {
        self.failure.clone()
    }
    fn inner(&self) -> &ffi::NativeMixerSource {
        self.native.as_ref().as_ref().expect("owned mixer source")
    }
    pub fn ssrc(&self) -> Result<i32, AudioMixerError> {
        let mut valid = false;
        let value = ffi::mixer_source_get_ssrc(self.inner(), &mut valid);
        if valid {
            Ok(value)
        } else {
            Err(self
                .failure
                .failure()
                .unwrap_or(AudioMixerError::CallbackRejected))
        }
    }
    /// Direct actual Source::GetAudioFrameWithInfo. Native kError is a normal
    /// disposition and stays distinct from a Rust callback failure. The returned
    /// muted frame preserves the producer's implicit-zero contract.
    pub fn audio_frame(&self, sample_rate_hz: i32) -> Result<MixerAudioFrame, AudioMixerError> {
        let mut error = 0;
        let result = ffi::mixer_source_get_audio(self.inner(), sample_rate_hz, &mut error);
        if error != 0 {
            return Err(match error {
                1 => AudioMixerError::ReentrantCall,
                2 => AudioMixerError::InvalidPreferredRate,
                3 => AudioMixerError::InvalidFrame,
                _ => self
                    .failure
                    .failure()
                    .unwrap_or(AudioMixerError::CallbackRejected),
            });
        }
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
    pub fn preferred_sample_rate(&self) -> Result<i32, AudioMixerError> {
        let mut valid = false;
        let value = ffi::mixer_source_get_rate(self.inner(), &mut valid);
        if valid {
            Ok(value)
        } else {
            Err(self
                .failure
                .failure()
                .unwrap_or(AudioMixerError::CallbackRejected))
        }
    }
}

/// Mixing is sequence-bound; source registration ownership is shared, but no
/// mutable alias of the mixer is exposed. Callback reentry is refused natively
/// before the SDK mixer mutex. The default SDK calculator and limiter are used.
pub struct AudioMixer {
    native: cxx::UniquePtr<ffi::NativeAudioMixer>,
    failure: AudioMixerFailureObserver,
    _sequence: PhantomData<Rc<()>>,
}
impl AudioMixer {
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeAudioMixer> {
        self.native
    }
    pub fn stock() -> Result<Self, AudioMixerError> {
        let failure = AudioMixerFailureObserver::default();
        let native = ffi::audio_mixer_new(Box::new(RustMixerModule(failure.clone())));
        if native.is_null() {
            return Err(AudioMixerError::CreationFailed);
        }
        Ok(Self {
            native,
            failure,
            _sequence: PhantomData,
        })
    }
    /// Select a caller implementation without constructing a stock mixer.
    pub fn from_provider(
        provider: impl crate::AudioMixerProvider,
    ) -> Result<Self, AudioMixerError> {
        Self::from_boxed_provider(Box::new(provider))
    }
    pub(crate) fn from_boxed_provider(
        provider: Box<dyn crate::AudioMixerProvider>,
    ) -> Result<Self, AudioMixerError> {
        let failure = AudioMixerFailureObserver::default();
        let native = ffi::audio_mixer_from_provider(
            Box::new(RustMixerModule(failure.clone())),
            Box::new(crate::audio_mixer_provider::RustMixerProvider::new(
                provider,
                failure.clone(),
            )),
        );
        if native.is_null() {
            return Err(AudioMixerError::CreationFailed);
        }
        Ok(Self {
            native,
            failure,
            _sequence: PhantomData,
        })
    }
    pub fn failure_observer(&self) -> AudioMixerFailureObserver {
        self.failure.clone()
    }
    pub fn add_source(&mut self, source: &AudioMixerSource) -> Result<bool, AudioMixerError> {
        let mut error = 0;
        let added = ffi::audio_mixer_add(self.native.pin_mut(), source.inner(), &mut error);
        if error == 0 {
            Ok(added)
        } else {
            Err(if error == 1 {
                AudioMixerError::ReentrantCall
            } else {
                AudioMixerError::CallbackRejected
            })
        }
    }
    /// Returns false for an absent source without invoking the SDK's asserting
    /// removal operation. An alias of a registered source removes that source.
    pub fn remove_source(&mut self, source: &AudioMixerSource) -> Result<bool, AudioMixerError> {
        let mut error = 0;
        let removed = ffi::audio_mixer_remove(self.native.pin_mut(), source.inner(), &mut error);
        if error == 0 {
            Ok(removed)
        } else {
            Err(if error == 1 {
                AudioMixerError::ReentrantCall
            } else {
                AudioMixerError::CallbackRejected
            })
        }
    }
    pub fn mix(&mut self, channels: usize) -> Result<AudioFrame, AudioMixerError> {
        if channels == 0 {
            return Err(AudioMixerError::InvalidChannels);
        }
        let mut error = 0;
        let frame = ffi::audio_mixer_mix(self.native.pin_mut(), channels, &mut error);
        match error {
            0 if !frame.is_null() => Ok(AudioFrame::from_native(frame)),
            1 => Err(AudioMixerError::ReentrantCall),
            2 => Err(AudioMixerError::InvalidPreferredRate),
            3 => Err(AudioMixerError::InvalidFrame),
            _ => Err(AudioMixerError::CallbackRejected),
        }
    }
}
