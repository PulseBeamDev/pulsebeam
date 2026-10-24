//! Owned transport/observer providers. Callback output storage is initialized
//! before Rust sees it. Void native callback failures remain observer-visible;
//! initialized fallback output is not a successful custom audio result.
use crate::ffi;
use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDevicePcmSpec {
    pub frames: usize,
    /// The SDK's nBytesPerSample is bytes per *interleaved frame* in the actual
    /// ADM transport consumer, not bytes per channel sample.
    pub bytes_per_frame: usize,
    pub channels: usize,
    pub sample_rate_hz: i64,
    pub bits_per_sample: i32,
}
#[derive(Clone, Copy, Debug)]
pub struct AudioDeviceCapture<'a> {
    pub spec: AudioDevicePcmSpec,
    pub samples: &'a [u8],
    pub total_delay_ms: u32,
    pub clock_drift: i32,
    pub current_mic_level: u32,
    pub key_pressed: bool,
    pub estimated_capture_time_ns: Option<i64>,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioDeviceCaptureResult {
    pub status: i32,
    pub new_mic_level: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct AudioDevicePlayoutResult {
    pub status: i32,
    pub frames: usize,
    pub elapsed_time_ms: i64,
    pub ntp_time_ms: i64,
}
#[derive(Clone, Copy, Debug)]
pub struct AudioDeviceRenderTiming {
    pub elapsed_time_ms: i64,
    pub ntp_time_ms: i64,
}
pub trait AudioTransportProvider: Send + 'static {
    fn recorded_data(
        &mut self,
        data: AudioDeviceCapture<'_>,
    ) -> Result<AudioDeviceCaptureResult, String>;
    fn need_play_data(
        &mut self,
        spec: AudioDevicePcmSpec,
        output: &mut [u8],
    ) -> Result<AudioDevicePlayoutResult, String>;
    fn pull_render_data(
        &mut self,
        spec: AudioDevicePcmSpec,
        output: &mut [u8],
    ) -> Result<AudioDeviceRenderTiming, String>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDeviceSpeechActivity {
    Started,
    Ended,
    Unknown(i32),
}
pub trait AudioDeviceObserverProvider: Send + 'static {
    fn devices_updated(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn speech_activity(&mut self, _event: AudioDeviceSpeechActivity) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDeviceCallbackOperation {
    RecordedData,
    NeedPlayData,
    PullRenderData,
    DevicesUpdated,
    SpeechActivity,
    Destroy,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioDeviceCallbackFailureKind {
    Rejected(String),
    Panicked,
    Reentrant,
    InvalidExtent,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceCallbackFailure {
    pub operation: AudioDeviceCallbackOperation,
    pub kind: AudioDeviceCallbackFailureKind,
}
#[derive(Default)]
struct Failures {
    first: Mutex<Option<AudioDeviceCallbackFailure>>,
    count: AtomicU64,
}
#[derive(Clone, Default)]
pub struct AudioDeviceCallbackFailureObserver(Arc<Failures>);
impl AudioDeviceCallbackFailureObserver {
    pub fn failure(&self) -> Option<AudioDeviceCallbackFailure> {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn failure_count(&self) -> u64 {
        self.0.count.load(Ordering::Acquire)
    }
    fn record(
        &self,
        operation: AudioDeviceCallbackOperation,
        kind: AudioDeviceCallbackFailureKind,
    ) {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(AudioDeviceCallbackFailure { operation, kind });
        self.0.count.fetch_add(1, Ordering::Release);
    }
}
enum Provider {
    Transport(Box<dyn AudioTransportProvider>),
    Observer(Box<dyn AudioDeviceObserverProvider>),
}
pub(crate) struct RustAudioDeviceCallbacks {
    provider: Mutex<Option<Provider>>,
    failure: AudioDeviceCallbackFailureObserver,
}
thread_local! { static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) }; }
struct Scope;
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|v| {
            v.borrow_mut().pop();
        });
    }
}
impl RustAudioDeviceCallbacks {
    fn call<T>(
        &self,
        operation: AudioDeviceCallbackOperation,
        f: impl FnOnce(&mut Provider) -> Result<T, String>,
    ) -> Option<T> {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let entered = ACTIVE.with(|v| {
            let mut v = v.borrow_mut();
            let id = self as *const Self as usize;
            if v.contains(&id) {
                false
            } else {
                v.push(id);
                true
            }
        });
        if !entered {
            self.failure
                .record(operation, AudioDeviceCallbackFailureKind::Reentrant);
            return None;
        }
        let _scope = Scope;
        let mut provider = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        match catch_unwind(AssertUnwindSafe(|| match provider.as_mut() {
            Some(p) => f(p),
            None => Err("retired device callback".into()),
        })) {
            Ok(Ok(v)) => Some(v),
            Ok(Err(message)) => {
                self.failure
                    .record(operation, AudioDeviceCallbackFailureKind::Rejected(message));
                None
            }
            Err(payload) => {
                crate::audio_processor_caller_dump::dispose_panic(payload);
                self.failure
                    .record(operation, AudioDeviceCallbackFailureKind::Panicked);
                None
            }
        }
    }
    fn dispose(&mut self) {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(provider))) {
            crate::audio_processor_caller_dump::dispose_panic(payload);
            self.failure.record(
                AudioDeviceCallbackOperation::Destroy,
                AudioDeviceCallbackFailureKind::Panicked,
            );
        }
    }
}
impl Drop for RustAudioDeviceCallbacks {
    fn drop(&mut self) {
        self.dispose();
    }
}
pub(crate) fn audio_device_callbacks_dispose(callbacks: &mut RustAudioDeviceCallbacks) {
    callbacks.dispose();
}
pub(crate) fn audio_device_callbacks_invalid(callbacks: &RustAudioDeviceCallbacks, operation: u8) {
    let operation = match operation {
        0 => AudioDeviceCallbackOperation::RecordedData,
        1 => AudioDeviceCallbackOperation::NeedPlayData,
        _ => AudioDeviceCallbackOperation::PullRenderData,
    };
    callbacks
        .failure
        .record(operation, AudioDeviceCallbackFailureKind::InvalidExtent);
}
fn spec(f: &ffi::FfiAudioDevicePcm) -> AudioDevicePcmSpec {
    AudioDevicePcmSpec {
        frames: f.frames,
        bytes_per_frame: f.bytes_per_frame,
        channels: f.channels,
        sample_rate_hz: f.rate,
        bits_per_sample: f.bits,
    }
}
fn result() -> ffi::FfiAudioDeviceCallbackResult {
    ffi::FfiAudioDeviceCallbackResult {
        failed: true,
        status: -1,
        frames: 0,
        mic_level: 0,
        elapsed_ms: -1,
        ntp_ms: -1,
    }
}
pub(crate) fn audio_device_recorded(
    callbacks: &RustAudioDeviceCallbacks,
    f: ffi::FfiAudioDevicePcm,
    data: &[u8],
) -> ffi::FfiAudioDeviceCallbackResult {
    let mut r = result();
    if f.frames.checked_mul(f.bytes_per_frame) != Some(data.len()) {
        audio_device_callbacks_invalid(callbacks, 0);
        return r;
    }
    let value = callbacks.call(AudioDeviceCallbackOperation::RecordedData, |p| match p {
        Provider::Transport(p) => p.recorded_data(AudioDeviceCapture {
            spec: spec(&f),
            samples: data,
            total_delay_ms: f.delay_ms,
            clock_drift: f.clock_drift,
            current_mic_level: f.mic_level,
            key_pressed: f.key_pressed,
            estimated_capture_time_ns: f.capture_time_present.then_some(f.capture_time_ns),
        }),
        _ => Err("transport kind mismatch".into()),
    });
    if let Some(v) = value {
        r.failed = false;
        r.status = v.status;
        r.mic_level = v.new_mic_level;
    }
    r
}
pub(crate) fn audio_device_render(
    callbacks: &RustAudioDeviceCallbacks,
    f: ffi::FfiAudioDevicePcm,
    pull: bool,
    data: &mut [u8],
) -> ffi::FfiAudioDeviceCallbackResult {
    let mut r = result();
    if f.frames.checked_mul(f.bytes_per_frame) != Some(data.len()) {
        audio_device_callbacks_invalid(callbacks, if pull { 2 } else { 1 });
        return r;
    }
    if pull {
        let value = callbacks.call(AudioDeviceCallbackOperation::PullRenderData, |p| match p {
            Provider::Transport(p) => p.pull_render_data(spec(&f), data),
            _ => Err("transport kind mismatch".into()),
        });
        if let Some(v) = value {
            r.failed = false;
            r.status = 0;
            r.frames = f.frames;
            r.elapsed_ms = v.elapsed_time_ms;
            r.ntp_ms = v.ntp_time_ms;
        }
    } else {
        let value = callbacks.call(AudioDeviceCallbackOperation::NeedPlayData, |p| match p {
            Provider::Transport(p) => p.need_play_data(spec(&f), data),
            _ => Err("transport kind mismatch".into()),
        });
        if let Some(v) = value {
            if v.frames > f.frames {
                audio_device_callbacks_invalid(callbacks, 1);
                return r;
            }
            r.failed = false;
            r.status = v.status;
            r.frames = v.frames;
            r.elapsed_ms = v.elapsed_time_ms;
            r.ntp_ms = v.ntp_time_ms;
        }
    }
    r
}
pub(crate) fn audio_device_observed(
    callbacks: &RustAudioDeviceCallbacks,
    speech: bool,
    event: i32,
) {
    let operation = if speech {
        AudioDeviceCallbackOperation::SpeechActivity
    } else {
        AudioDeviceCallbackOperation::DevicesUpdated
    };
    callbacks.call(operation, |p| match p {
        Provider::Observer(p) => {
            if speech {
                p.speech_activity(match event {
                    0 => AudioDeviceSpeechActivity::Started,
                    1 => AudioDeviceSpeechActivity::Ended,
                    n => AudioDeviceSpeechActivity::Unknown(n),
                })
            } else {
                p.devices_updated()
            }
        }
        _ => Err("observer kind mismatch".into()),
    });
}
pub struct AudioTransport {
    #[cfg_attr(
        not(feature = "native"),
        allow(
            dead_code,
            reason = "Core retains the owning adapter without platform registration."
        )
    )]
    pub(crate) native: cxx::UniquePtr<ffi::NativeAudioTransport>,
    failure: AudioDeviceCallbackFailureObserver,
}
pub struct AudioDeviceObserver {
    #[cfg_attr(
        not(feature = "native"),
        allow(
            dead_code,
            reason = "Core retains the owning adapter without platform registration."
        )
    )]
    pub(crate) native: cxx::UniquePtr<ffi::NativeAudioDeviceObserver>,
    failure: AudioDeviceCallbackFailureObserver,
}
// Opaque callback adapters have no affine SDK resources and serialize Send providers.
unsafe impl Send for AudioTransport {}
unsafe impl Send for AudioDeviceObserver {}
impl AudioTransport {
    pub fn from_provider(provider: impl AudioTransportProvider) -> Self {
        let failure = AudioDeviceCallbackFailureObserver::default();
        let native = ffi::audio_transport_new(Box::new(RustAudioDeviceCallbacks {
            provider: Mutex::new(Some(Provider::Transport(Box::new(provider)))),
            failure: failure.clone(),
        }));
        Self { native, failure }
    }
    pub fn failure_observer(&self) -> AudioDeviceCallbackFailureObserver {
        self.failure.clone()
    }
}
impl AudioDeviceObserver {
    pub fn from_provider(provider: impl AudioDeviceObserverProvider) -> Self {
        let failure = AudioDeviceCallbackFailureObserver::default();
        let native = ffi::audio_device_observer_new(Box::new(RustAudioDeviceCallbacks {
            provider: Mutex::new(Some(Provider::Observer(Box::new(provider)))),
            failure: failure.clone(),
        }));
        Self { native, failure }
    }
    pub fn failure_observer(&self) -> AudioDeviceCallbackFailureObserver {
        self.failure.clone()
    }
}
