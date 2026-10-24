//! Owning custom APM modules and borrowed full-band FloatS16 callback views.
use crate::{
    AudioProcessingModule, AudioProcessingRuntimeSetting, AudioProcessorConfig,
    AudioProcessorError, Environment, ffi,
};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy)]
pub struct AudioProcessingBuffer<'a> {
    rate: i32,
    channels: usize,
    frames: usize,
    samples: &'a [f32],
}
fn planar_shape(rate: i32, channels: usize, length: usize) -> Result<usize, AudioProcessorError> {
    let stream = crate::AudioProcessingStream {
        sample_rate_hz: rate,
        channels,
    };
    let frames = stream.frame_size()?;
    let count = stream.sample_count()?;
    // AudioBuffer's backing ChannelBuffer and callback channel arguments use
    // signed native dimensions. This is an allocation/index bound, not a rate
    // whitelist: retain native floor(rate/100), including nonstandard rates.
    if frames == 0 || channels == 0 || count > i32::MAX as usize {
        return Err(AudioProcessorError::InvalidDimensions);
    }
    if length != count {
        return Err(AudioProcessorError::BufferTooShort);
    }
    Ok(frames)
}
impl<'a> AudioProcessingBuffer<'a> {
    /// Borrow one initialized 10-ms channel-major frame in FloatS16 units.
    /// No resampling, normalization or implicit tail is performed.
    pub fn from_planar(
        rate: i32,
        channels: usize,
        samples: &'a [f32],
    ) -> Result<Self, AudioProcessorError> {
        let frames = planar_shape(rate, channels, samples.len())?;
        Ok(Self {
            rate,
            channels,
            frames,
            samples,
        })
    }
    pub fn sample_rate_hz(&self) -> i32 {
        self.rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn frames(&self) -> usize {
        self.frames
    }
    /// Full-band channel-major samples in FloatS16 units, not normalized PCM.
    pub fn samples(&self) -> &'a [f32] {
        self.samples
    }
    pub fn channel(&self, channel: usize) -> Option<&'a [f32]> {
        if channel >= self.channels {
            return None;
        }
        let start = channel.checked_mul(self.frames)?;
        self.samples.get(start..start.checked_add(self.frames)?)
    }
}
pub struct AudioProcessingBufferMut<'a> {
    rate: i32,
    channels: usize,
    frames: usize,
    samples: &'a mut [f32],
}
impl<'a> AudioProcessingBufferMut<'a> {
    pub fn from_planar(
        rate: i32,
        channels: usize,
        samples: &'a mut [f32],
    ) -> Result<Self, AudioProcessorError> {
        let frames = planar_shape(rate, channels, samples.len())?;
        Ok(Self {
            rate,
            channels,
            frames,
            samples,
        })
    }
    pub fn sample_rate_hz(&self) -> i32 {
        self.rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn frames(&self) -> usize {
        self.frames
    }
    pub fn as_read(&self) -> AudioProcessingBuffer<'_> {
        AudioProcessingBuffer {
            rate: self.rate,
            channels: self.channels,
            frames: self.frames,
            samples: self.samples,
        }
    }
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }
    pub fn channel_mut(&mut self, channel: usize) -> Option<&mut [f32]> {
        if channel >= self.channels {
            return None;
        }
        let start = channel.checked_mul(self.frames)?;
        self.samples.get_mut(start..start.checked_add(self.frames)?)
    }
}
/// Calls are serialized. A provider need not be Sync, including after native
/// unique handoff. Same-provider recursive calls are refused before its Mutex.
pub trait AudioProcessingCustomProcessorProvider: Send + 'static {
    fn initialize(&mut self, sample_rate_hz: i32, channels: i32) -> Result<(), String>;
    fn process(&mut self, audio: AudioProcessingBufferMut<'_>) -> Result<(), String>;
    fn to_string(&mut self) -> Result<Vec<u8>, String>;
    /// The pinned native default implementation is a no-op.
    fn set_runtime_setting(
        &mut self,
        _setting: AudioProcessingRuntimeSetting,
    ) -> Result<(), String> {
        Ok(())
    }
}
pub trait AudioProcessingCustomAnalyzerProvider: Send + 'static {
    fn initialize(&mut self, sample_rate_hz: i32, channels: i32) -> Result<(), String>;
    fn analyze(&mut self, audio: AudioProcessingBuffer<'_>) -> Result<(), String>;
    fn to_string(&mut self) -> Result<Vec<u8>, String>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioProcessingEchoDetectorMetrics {
    pub echo_likelihood: Option<f64>,
    pub echo_likelihood_recent_max: Option<f64>,
}
pub trait AudioProcessingEchoDetectorProvider: Send + 'static {
    fn initialize(
        &mut self,
        capture_rate: i32,
        capture_channels: i32,
        render_rate: i32,
        render_channels: i32,
    ) -> Result<(), String>;
    fn analyze_render(&mut self, samples: &[f32]) -> Result<(), String>;
    fn analyze_capture(&mut self, samples: &[f32]) -> Result<(), String>;
    fn metrics(&mut self) -> Result<AudioProcessingEchoDetectorMetrics, String>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioProcessingCustomOperation {
    Create,
    Initialize,
    Process,
    Analyze,
    Metrics,
    Estimate,
    AdjustConfig,
    Reset,
    ToString,
    RuntimeSetting,
    SetAudioBufferDelay,
    SetCaptureOutputUsage,
    ActiveProcessing,
    Destroy,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioProcessingCustomFailureKind {
    Rejected(String),
    Panicked,
    Reentrant,
    InvalidAudio,
    InvalidBridgeValue,
    UnqualifiedRenderRange,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioProcessingCustomFailure {
    pub operation: AudioProcessingCustomOperation,
    pub kind: AudioProcessingCustomFailureKind,
}
#[derive(Default)]
struct Failures {
    first: Mutex<Option<AudioProcessingCustomFailure>>,
    count: AtomicU64,
}
#[derive(Clone, Default)]
pub struct AudioProcessingCustomFailureObserver(Arc<Failures>);
impl AudioProcessingCustomFailureObserver {
    pub fn failure(&self) -> Option<AudioProcessingCustomFailure> {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn failure_count(&self) -> u64 {
        self.0.count.load(Ordering::Acquire)
    }
    pub(crate) fn record(
        &self,
        operation: AudioProcessingCustomOperation,
        kind: AudioProcessingCustomFailureKind,
    ) {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(AudioProcessingCustomFailure { operation, kind });
        self.0.count.fetch_add(1, Ordering::Release);
    }
}
pub(crate) enum Provider {
    Processor(Box<dyn AudioProcessingCustomProcessorProvider>),
    Analyzer(Box<dyn AudioProcessingCustomAnalyzerProvider>),
    EchoDetector(Box<dyn AudioProcessingEchoDetectorProvider>),
    Neural(Box<dyn crate::NeuralResidualEchoEstimatorProvider>),
    EchoControl(
        Box<dyn crate::EchoControlProvider>,
        crate::audio_echo_dependencies::RustEchoControlContext,
    ),
    EchoFactory(Box<dyn crate::EchoControlFactoryProvider>),
}
pub(crate) struct RustApmCustomModule {
    pub(crate) provider: Mutex<Option<Provider>>,
    pub(crate) failure: AudioProcessingCustomFailureObserver,
}
thread_local! { static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) }; }
struct Scope;
impl Scope {
    fn enter(id: usize) -> Option<Self> {
        ACTIVE.with(|a| {
            let mut a = a.borrow_mut();
            if a.contains(&id) {
                None
            } else {
                a.push(id);
                Some(Self)
            }
        })
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            a.borrow_mut().pop();
        });
    }
}
impl RustApmCustomModule {
    pub(crate) fn call<T>(
        &self,
        operation: AudioProcessingCustomOperation,
        f: impl FnOnce(&mut Provider) -> Result<T, String>,
    ) -> Option<T> {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let Some(_scope) = Scope::enter(self as *const Self as usize) else {
            self.failure
                .record(operation, AudioProcessingCustomFailureKind::Reentrant);
            return None;
        };
        let mut provider = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        let result = catch_unwind(AssertUnwindSafe(|| match provider.as_mut() {
            Some(p) => f(p),
            None => Err("retired custom module".into()),
        }));
        match result {
            Ok(Ok(value)) => Some(value),
            Ok(Err(message)) => {
                self.failure.record(
                    operation,
                    AudioProcessingCustomFailureKind::Rejected(message),
                );
                None
            }
            Err(payload) => {
                crate::audio_processor_caller_dump::dispose_panic(payload);
                self.failure
                    .record(operation, AudioProcessingCustomFailureKind::Panicked);
                None
            }
        }
    }
    fn dispose(&mut self) -> bool {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(provider) = provider else {
            return true;
        };
        match catch_unwind(AssertUnwindSafe(|| drop(provider))) {
            Ok(()) => true,
            Err(payload) => {
                crate::audio_processor_caller_dump::dispose_panic(payload);
                self.failure.record(
                    AudioProcessingCustomOperation::Destroy,
                    AudioProcessingCustomFailureKind::Panicked,
                );
                false
            }
        }
    }
}
impl Drop for RustApmCustomModule {
    fn drop(&mut self) {
        self.dispose();
    }
}
pub(crate) fn apm_custom_dispose(module: &mut RustApmCustomModule) -> bool {
    module.dispose()
}
pub(crate) fn apm_custom_initialize(
    module: &RustApmCustomModule,
    rate: i32,
    channels: i32,
) -> bool {
    module
        .call(AudioProcessingCustomOperation::Initialize, |p| match p {
            Provider::Processor(p) => p.initialize(rate, channels),
            Provider::Analyzer(p) => p.initialize(rate, channels),
            _ => Err("component kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_custom_description(module: &RustApmCustomModule, valid: &mut bool) -> Vec<u8> {
    let value = module.call(AudioProcessingCustomOperation::ToString, |p| match p {
        Provider::Processor(p) => p.to_string(),
        Provider::Analyzer(p) => p.to_string(),
        _ => Err("component kind mismatch".into()),
    });
    *valid = value.is_some();
    value.unwrap_or_default()
}
pub(crate) fn apm_custom_process(
    module: &RustApmCustomModule,
    rate: i32,
    channels: usize,
    frames: usize,
    samples: &mut [f32],
) -> bool {
    if channels.checked_mul(frames) != Some(samples.len()) {
        module.failure.record(
            AudioProcessingCustomOperation::Process,
            AudioProcessingCustomFailureKind::InvalidBridgeValue,
        );
        return false;
    }
    let success = module
        .call(AudioProcessingCustomOperation::Process, |p| match p {
            Provider::Processor(p) => p.process(AudioProcessingBufferMut {
                rate,
                channels,
                frames,
                samples,
            }),
            _ => Err("processor kind mismatch".into()),
        })
        .is_some();
    if !success {
        return false;
    }
    if samples.iter().any(|v| !v.is_finite()) {
        module.failure.record(
            AudioProcessingCustomOperation::Process,
            AudioProcessingCustomFailureKind::InvalidAudio,
        );
        return false;
    }
    true
}
pub(crate) fn apm_custom_analyze(
    module: &RustApmCustomModule,
    rate: i32,
    channels: usize,
    frames: usize,
    samples: &[f32],
) -> bool {
    if channels.checked_mul(frames) != Some(samples.len()) {
        module.failure.record(
            AudioProcessingCustomOperation::Analyze,
            AudioProcessingCustomFailureKind::InvalidBridgeValue,
        );
        return false;
    }
    module
        .call(AudioProcessingCustomOperation::Analyze, |p| match p {
            Provider::Analyzer(p) => p.analyze(AudioProcessingBuffer {
                rate,
                channels,
                frames,
                samples,
            }),
            _ => Err("analyzer kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_custom_unqualified_render(module: &RustApmCustomModule) {
    module.failure.record(
        AudioProcessingCustomOperation::Process,
        AudioProcessingCustomFailureKind::UnqualifiedRenderRange,
    );
}
pub(crate) fn apm_custom_runtime(
    module: &RustApmCustomModule,
    f: ffi::FfiApmRuntimeSetting,
) -> bool {
    let Some(setting) = AudioProcessingRuntimeSetting::from_ffi(f) else {
        module.failure.record(
            AudioProcessingCustomOperation::RuntimeSetting,
            AudioProcessingCustomFailureKind::InvalidBridgeValue,
        );
        return false;
    };
    module
        .call(
            AudioProcessingCustomOperation::RuntimeSetting,
            |p| match p {
                Provider::Processor(p) => p.set_runtime_setting(setting),
                _ => Err("processor kind mismatch".into()),
            },
        )
        .is_some()
}

pub(crate) fn apm_echo_initialize(
    module: &RustApmCustomModule,
    capture_rate: i32,
    capture_channels: i32,
    render_rate: i32,
    render_channels: i32,
) -> bool {
    module
        .call(AudioProcessingCustomOperation::Initialize, |p| match p {
            Provider::EchoDetector(p) => {
                p.initialize(capture_rate, capture_channels, render_rate, render_channels)
            }
            _ => Err("detector kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_analyze(
    module: &RustApmCustomModule,
    render: bool,
    samples: &[f32],
) -> bool {
    module
        .call(AudioProcessingCustomOperation::Analyze, |p| match p {
            Provider::EchoDetector(p) => {
                if render {
                    p.analyze_render(samples)
                } else {
                    p.analyze_capture(samples)
                }
            }
            _ => Err("detector kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_metrics(module: &RustApmCustomModule) -> ffi::FfiApmEchoDetectorMetrics {
    let value = module.call(AudioProcessingCustomOperation::Metrics, |p| match p {
        Provider::EchoDetector(p) => p.metrics(),
        _ => Err("detector kind mismatch".into()),
    });
    let mut f = ffi::FfiApmEchoDetectorMetrics {
        failed: value.is_none(),
        likelihood_present: false,
        likelihood: 0.0,
        recent_max_present: false,
        recent_max: 0.0,
    };
    if let Some(value) = value {
        f.likelihood_present = value.echo_likelihood.is_some();
        f.likelihood = value.echo_likelihood.unwrap_or(0.0);
        f.recent_max_present = value.echo_likelihood_recent_max.is_some();
        f.recent_max = value.echo_likelihood_recent_max.unwrap_or(0.0);
    }
    f
}
/// Clones retain the exact same native refcounted detector and serialized provider.
pub struct AudioProcessingEchoDetector {
    pub(crate) native: cxx::UniquePtr<ffi::NativeApmCustomModule>,
    failure: AudioProcessingCustomFailureObserver,
    _not_sync: PhantomData<Cell<()>>,
}
unsafe impl Send for AudioProcessingEchoDetector {}
impl Clone for AudioProcessingEchoDetector {
    fn clone(&self) -> Self {
        Self {
            native: ffi::audio_processor_echo_detector_clone(
                self.native.as_ref().expect("owned detector"),
            ),
            failure: self.failure.clone(),
            _not_sync: PhantomData,
        }
    }
}
impl AudioProcessingEchoDetector {
    pub fn from_provider(provider: impl AudioProcessingEchoDetectorProvider) -> Self {
        let failure = AudioProcessingCustomFailureObserver::default();
        let native = ffi::audio_processor_echo_detector_new(Box::new(RustApmCustomModule {
            provider: Mutex::new(Some(Provider::EchoDetector(Box::new(provider)))),
            failure: failure.clone(),
        }));
        Self {
            native,
            failure,
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> AudioProcessingCustomFailureObserver {
        self.failure.clone()
    }
    pub fn initialize(
        &mut self,
        capture_rate: i32,
        capture_channels: i32,
        render_rate: i32,
        render_channels: i32,
    ) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_echo_detector_initialize(
            self.native.pin_mut(),
            capture_rate,
            capture_channels,
            render_rate,
            render_channels,
        ))
    }
    pub fn analyze_render(&mut self, samples: &[f32]) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_echo_detector_analyze(
            self.native.pin_mut(),
            true,
            samples,
        ))
    }
    pub fn analyze_capture(&mut self, samples: &[f32]) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_echo_detector_analyze(
            self.native.pin_mut(),
            false,
            samples,
        ))
    }
    pub fn metrics(&mut self) -> Result<AudioProcessingEchoDetectorMetrics, AudioProcessorError> {
        let f = ffi::audio_processor_echo_detector_metrics(self.native.pin_mut());
        if f.failed {
            return Err(AudioProcessorError::CallbackFailed);
        }
        Ok(AudioProcessingEchoDetectorMetrics {
            echo_likelihood: f.likelihood_present.then_some(f.likelihood),
            echo_likelihood_recent_max: f.recent_max_present.then_some(f.recent_max),
        })
    }
}

pub struct AudioProcessingCustomProcessor {
    pub(crate) native: cxx::UniquePtr<ffi::NativeApmCustomModule>,
    failure: AudioProcessingCustomFailureObserver,
    _not_sync: PhantomData<Cell<()>>,
}
pub struct AudioProcessingCustomAnalyzer {
    pub(crate) native: cxx::UniquePtr<ffi::NativeApmCustomModule>,
    failure: AudioProcessingCustomFailureObserver,
    _not_sync: PhantomData<Cell<()>>,
}
// The opaque objects contain only a uniquely owned adapter, shared atomic status
// and a Mutex-serialized Send Rust provider; no SDK thread-affine DSP object.
unsafe impl Send for AudioProcessingCustomProcessor {}
unsafe impl Send for AudioProcessingCustomAnalyzer {}
impl AudioProcessingCustomProcessor {
    /// Dispatch the actual native CustomProcessing::Process operation. Initialize
    /// this product for the same format first. Failed working audio is never
    /// copied back, including rejection, panic and nonfinite caller output.
    pub fn process(
        &mut self,
        audio: AudioProcessingBufferMut<'_>,
    ) -> Result<(), AudioProcessorError> {
        let mut error = 0;
        let result = ffi::audio_processor_custom_audio(
            self.native.pin_mut(),
            audio.rate,
            audio.channels,
            audio.samples,
            false,
            &mut error,
        );
        AudioProcessingModule::admission(error)?;
        if result.len() != audio.samples.len() {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        audio.samples.copy_from_slice(&result);
        Ok(())
    }
    /// Direct CustomProcessing delivery, not posting to APM's queues. Inactive
    /// payloads and the native default setting remain intact.
    pub fn set_runtime_setting(
        &mut self,
        setting: AudioProcessingRuntimeSetting,
    ) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_custom_runtime(
            self.native.pin_mut(),
            &setting.ffi(),
        ))
    }
    pub fn from_provider(provider: impl AudioProcessingCustomProcessorProvider) -> Self {
        let failure = AudioProcessingCustomFailureObserver::default();
        let native = ffi::audio_processor_custom_new(
            Box::new(RustApmCustomModule {
                provider: Mutex::new(Some(Provider::Processor(Box::new(provider)))),
                failure: failure.clone(),
            }),
            false,
        );
        Self {
            native,
            failure,
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> AudioProcessingCustomFailureObserver {
        self.failure.clone()
    }
    pub fn description(&mut self) -> Result<Vec<u8>, AudioProcessorError> {
        let mut error = 0;
        let value = ffi::audio_processor_custom_description(self.native.pin_mut(), &mut error);
        AudioProcessingModule::admission(error)?;
        Ok(value)
    }
    pub fn initialize(&mut self, rate: i32, channels: i32) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_custom_initialize(
            self.native.pin_mut(),
            rate,
            channels,
        ))
    }
}
impl AudioProcessingCustomAnalyzer {
    pub fn analyze(&mut self, audio: AudioProcessingBuffer<'_>) -> Result<(), AudioProcessorError> {
        let mut error = 0;
        let _ = ffi::audio_processor_custom_audio(
            self.native.pin_mut(),
            audio.rate,
            audio.channels,
            audio.samples,
            true,
            &mut error,
        );
        AudioProcessingModule::admission(error)
    }
    pub fn from_provider(provider: impl AudioProcessingCustomAnalyzerProvider) -> Self {
        let failure = AudioProcessingCustomFailureObserver::default();
        let native = ffi::audio_processor_custom_new(
            Box::new(RustApmCustomModule {
                provider: Mutex::new(Some(Provider::Analyzer(Box::new(provider)))),
                failure: failure.clone(),
            }),
            true,
        );
        Self {
            native,
            failure,
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> AudioProcessingCustomFailureObserver {
        self.failure.clone()
    }
    pub fn description(&mut self) -> Result<Vec<u8>, AudioProcessorError> {
        let mut error = 0;
        let value = ffi::audio_processor_custom_description(self.native.pin_mut(), &mut error);
        AudioProcessingModule::admission(error)?;
        Ok(value)
    }
    pub fn initialize(&mut self, rate: i32, channels: i32) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_custom_initialize(
            self.native.pin_mut(),
            rate,
            channels,
        ))
    }
}
/// Owning selection into the actual builtin SDK builder, not a substitute APM.
/// Owning one-shot form of native CustomAudioProcessing. Build returns the
/// supplied module and its original dependencies, ignoring the extra environment.
/// No shared mutable standalone alias or replacement processor is introduced.
pub struct CustomAudioProcessorBuilder {
    processor: AudioProcessingModule,
}
impl CustomAudioProcessorBuilder {
    pub fn new(processor: AudioProcessingModule) -> Self {
        Self { processor }
    }
    pub fn build(
        mut self,
        environment: &Environment,
    ) -> Result<AudioProcessingModule, AudioProcessorError> {
        if !ffi::audio_processor_custom_build(self.processor.native_pin(), environment.native()) {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(self.processor)
    }
}
#[derive(Default)]
pub struct BuiltinAudioProcessorBuilder {
    config: AudioProcessorConfig,
    linear_echo: bool,
    echo_config: Option<(
        crate::EchoCanceller3Config,
        Option<crate::EchoCanceller3Config>,
    )>,
    neural: Option<crate::NeuralResidualEchoEstimator>,
    echo_factory: Option<crate::EchoControlFactory>,
    capture_post: Option<AudioProcessingCustomProcessor>,
    render_pre: Option<AudioProcessingCustomProcessor>,
    analyzer: Option<AudioProcessingCustomAnalyzer>,
    echo_detector: Option<AudioProcessingEchoDetector>,
}
impl BuiltinAudioProcessorBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn config(mut self, config: AudioProcessorConfig) -> Self {
        self.config = config;
        self
    }
    /// Request export from the actual selected mono/multichannel AEC3 configs.
    pub fn linear_echo_output(mut self, enabled: bool) -> Self {
        self.linear_echo = enabled;
        self
    }
    /// Select native AEC3 inputs. Absent multichannel configuration means reuse
    /// the mono configuration, as in the pinned SDK, not a synthesized default.
    pub fn echo_canceller_config(
        mut self,
        mono: crate::EchoCanceller3Config,
        multichannel: Option<crate::EchoCanceller3Config>,
    ) -> Self {
        self.echo_config = Some((mono, multichannel));
        self
    }
    pub fn capture_post_processing(mut self, processor: AudioProcessingCustomProcessor) -> Self {
        self.capture_post = Some(processor);
        self
    }
    pub fn render_pre_processing(mut self, processor: AudioProcessingCustomProcessor) -> Self {
        self.render_pre = Some(processor);
        self
    }
    pub fn capture_analyzer(mut self, analyzer: AudioProcessingCustomAnalyzer) -> Self {
        self.analyzer = Some(analyzer);
        self
    }
    /// The actual selected factory supersedes echo_canceller_config as in the
    /// SDK. Configure linear export on that factory itself; linear_echo_output
    /// cannot mutate its immutable native constructor inputs.
    pub fn echo_control_factory(mut self, factory: crate::EchoControlFactory) -> Self {
        self.echo_factory = Some(factory);
        self
    }
    pub fn neural_residual_echo_estimator(
        mut self,
        estimator: crate::NeuralResidualEchoEstimator,
    ) -> Self {
        self.neural = Some(estimator);
        self
    }
    pub fn echo_detector(mut self, detector: AudioProcessingEchoDetector) -> Self {
        self.echo_detector = Some(detector);
        self
    }
    pub fn build(
        mut self,
        environment: &Environment,
    ) -> Result<AudioProcessingModule, AudioProcessorError> {
        let environment = environment.clone();
        let mut error = 0;
        if let Some(factory) = self.echo_factory.as_mut() {
            factory.bind_environment(&environment);
        }
        let echo_config = match self.echo_config {
            Some((mono, multi)) => ffi::FfiAec3Selection {
                mono_present: true,
                mono: mono.ffi(),
                multi_present: multi.is_some(),
                multi: multi.map_or_else(ffi::FfiEchoCanceller3Config::default, |c| c.ffi()),
            },
            None => ffi::FfiAec3Selection {
                mono_present: false,
                mono: ffi::FfiEchoCanceller3Config::default(),
                multi_present: false,
                multi: ffi::FfiEchoCanceller3Config::default(),
            },
        };
        let native = ffi::audio_processor_new_custom(
            environment.native(),
            &self.config.ffi(),
            self.linear_echo,
            &echo_config,
            self.capture_post
                .map_or_else(cxx::UniquePtr::null, |p| p.native),
            self.render_pre
                .map_or_else(cxx::UniquePtr::null, |p| p.native),
            self.analyzer
                .map_or_else(cxx::UniquePtr::null, |p| p.native),
            self.echo_detector
                .map_or_else(cxx::UniquePtr::null, |p| p.native),
            self.neural.map_or_else(cxx::UniquePtr::null, |p| p.native),
            self.echo_factory
                .map_or_else(cxx::UniquePtr::null, |p| p.native),
            &mut error,
        );
        AudioProcessingModule::admission(error)?;
        AudioProcessingModule::from_native(native, environment)
    }
}
