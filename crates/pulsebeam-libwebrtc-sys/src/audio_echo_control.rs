//! Owning pinned EchoCanceller3Factory and EchoControl operations.
use crate::{
    AudioProcessingBuffer, AudioProcessingBufferMut, AudioProcessingModule, AudioProcessorError,
    EchoCanceller3Config, Environment, NeuralResidualEchoEstimator, ffi,
};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

/// Native processing rate. Stock AEC3 requires at least 16 kHz; caller
/// controllers can also use the native single-band 8-kHz format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum EchoControlSampleRate {
    Hz8000 = 8000,
    Hz16000 = 16000,
    Hz32000 = 32000,
    Hz48000 = 48000,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoControlMetrics {
    pub echo_return_loss: f64,
    pub echo_return_loss_enhancement: f64,
    pub delay_ms: i32,
}

pub struct EchoControlFactory {
    pub(crate) native: cxx::UniquePtr<ffi::NativeApmCustomModule>,
    failure: Option<crate::AudioProcessingCustomFailureObserver>,
    _not_sync: PhantomData<Cell<()>>,
}
// Caller dispatch is serialized around a Send-only provider. Creation and
// unique handoff remain exclusive, and retained environments are thread-safe.
unsafe impl Send for EchoControlFactory {}
impl EchoControlFactory {
    pub fn from_provider(provider: impl crate::EchoControlFactoryProvider) -> Self {
        let failure = crate::AudioProcessingCustomFailureObserver::default();
        let module = Box::new(crate::audio_processor_custom::RustApmCustomModule {
            provider: std::sync::Mutex::new(Some(
                crate::audio_processor_custom::Provider::EchoFactory(Box::new(provider)),
            )),
            failure: failure.clone(),
        });
        Self {
            native: ffi::audio_processor_echo_factory_provider_new(module),
            failure: Some(failure),
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> Option<crate::AudioProcessingCustomFailureObserver> {
        self.failure.clone()
    }
    pub(crate) fn bind_environment(&mut self, environment: &Environment) {
        ffi::audio_processor_echo_factory_environment(
            self.native.pin_mut(),
            Box::new(crate::audio_echo_factory_provider::RustEchoEnvironment(
                environment.clone(),
            )),
        );
    }
    /// Exact native default constructor, not the builtin builder's separate
    /// default-multichannel configuration policy.
    pub fn stock() -> Self {
        Self::construct(0, None, None)
    }
    pub fn with_config(config: EchoCanceller3Config) -> Self {
        Self::construct(1, Some(config), None)
    }
    pub fn with_multichannel_config(
        config: EchoCanceller3Config,
        multichannel: Option<EchoCanceller3Config>,
    ) -> Self {
        Self::construct(2, Some(config), multichannel)
    }
    fn construct(
        kind: u8,
        mono: Option<EchoCanceller3Config>,
        multi: Option<EchoCanceller3Config>,
    ) -> Self {
        let selection = ffi::FfiAec3Selection {
            mono_present: mono.is_some(),
            mono: mono.map_or_else(ffi::FfiEchoCanceller3Config::default, |v| v.ffi()),
            multi_present: multi.is_some(),
            multi: multi.map_or_else(ffi::FfiEchoCanceller3Config::default, |v| v.ffi()),
        };
        Self {
            native: ffi::audio_processor_echo_factory_new(kind, &selection),
            failure: None,
            _not_sync: PhantomData,
        }
    }
    /// Exact four-argument native Create overload, without a neural estimator.
    pub fn create(
        &mut self,
        environment: &Environment,
        rate: EchoControlSampleRate,
        render_channels: i32,
        capture_channels: i32,
    ) -> Result<EchoControl<'static>, AudioProcessorError> {
        self.create_impl(
            environment,
            rate,
            render_channels,
            capture_channels,
            false,
            None,
        )
    }
    /// Exact five-argument overload, including its legitimate nullable argument.
    /// A supplied estimator is exclusively borrowed until the controller dies.
    pub fn create_with_neural<'a>(
        &mut self,
        environment: &Environment,
        rate: EchoControlSampleRate,
        render_channels: i32,
        capture_channels: i32,
        neural: Option<&'a mut NeuralResidualEchoEstimator>,
    ) -> Result<EchoControl<'a>, AudioProcessorError> {
        self.create_impl(
            environment,
            rate,
            render_channels,
            capture_channels,
            true,
            neural,
        )
    }
    fn create_impl<'a>(
        &mut self,
        environment: &Environment,
        rate: EchoControlSampleRate,
        render_channels: i32,
        capture_channels: i32,
        neural_overload: bool,
        neural: Option<&'a mut NeuralResidualEchoEstimator>,
    ) -> Result<EchoControl<'a>, AudioProcessorError> {
        if render_channels <= 0 || capture_channels <= 0 {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        let pointer = neural.as_ref().map_or(std::ptr::null(), |v| {
            v.native.as_ref().expect("owned estimator") as *const ffi::NativeApmCustomModule
        });
        let environment = environment.clone();
        let mut error = 0;
        self.bind_environment(&environment);
        // The private pointer cannot outlive this product's exclusive Rust loan.
        let native = unsafe {
            ffi::audio_echo_control_create(
                self.native.pin_mut(),
                environment.native(),
                rate as i32,
                render_channels,
                capture_channels,
                neural_overload,
                pointer,
                &mut error,
            )
        };
        AudioProcessingModule::admission(error)?;
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(EchoControl {
            native,
            failure: self.failure.clone(),
            _environment: environment,
            _neural: PhantomData,
            _sequence: PhantomData,
        })
    }
}

/// Owns the real native controller, persistent render/capture split filters and
/// its exact environment. No raw buffer or borrowed estimator escapes publicly.
pub struct EchoControl<'a> {
    native: cxx::UniquePtr<ffi::NativeEchoControl>,
    failure: Option<crate::AudioProcessingCustomFailureObserver>,
    _environment: Environment,
    _neural: PhantomData<&'a mut NeuralResidualEchoEstimator>,
    _sequence: PhantomData<Rc<()>>,
}
impl EchoControl<'static> {
    /// Construct an owning native adapter for this exact serialized provider.
    /// This is not a stock AEC3 controller or an installed factory handoff.
    pub fn from_provider(
        environment: &Environment,
        rate: EchoControlSampleRate,
        render_channels: i32,
        capture_channels: i32,
        provider: impl crate::EchoControlProvider,
    ) -> Result<Self, AudioProcessorError> {
        let failure = crate::AudioProcessingCustomFailureObserver::default();
        let module = crate::audio_processor_custom::RustApmCustomModule {
            provider: std::sync::Mutex::new(Some(
                crate::audio_processor_custom::Provider::EchoControl(
                    Box::new(provider),
                    crate::audio_echo_dependencies::RustEchoControlContext {
                        environment: environment.clone(),
                        rate: rate as i32,
                        render_channels,
                        capture_channels,
                    },
                ),
            )),
            failure: failure.clone(),
        };
        let mut error = 0;
        let native = ffi::audio_echo_control_from_provider(
            rate as i32,
            render_channels,
            capture_channels,
            Box::new(module),
            &mut error,
        );
        AudioProcessingModule::admission(error)?;
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(Self {
            native,
            failure: Some(failure),
            _environment: environment.clone(),
            _neural: PhantomData,
            _sequence: PhantomData,
        })
    }
}
impl<'a> EchoControl<'a> {
    pub(crate) fn from_context_native(
        native: cxx::UniquePtr<ffi::NativeEchoControl>,
        environment: Environment,
        failure: Option<crate::AudioProcessingCustomFailureObserver>,
    ) -> Result<Self, AudioProcessorError> {
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(Self {
            native,
            failure,
            _environment: environment,
            _neural: PhantomData,
            _sequence: PhantomData,
        })
    }
    pub(crate) fn into_factory_transfer(mut self) -> cxx::UniquePtr<ffi::NativeEchoControl> {
        ffi::audio_echo_control_retain_environment(
            self.native.pin_mut(),
            Box::new(crate::audio_echo_factory_provider::RustEchoEnvironment(
                self._environment,
            )),
        );
        self.native
    }
    pub fn failure_observer(&self) -> Option<crate::AudioProcessingCustomFailureObserver> {
        self.failure.clone()
    }
    pub fn analyze_render(
        &mut self,
        audio: AudioProcessingBuffer<'_>,
    ) -> Result<(), AudioProcessorError> {
        self.audio(0, audio, false, false).map(|_| ())
    }
    pub fn analyze_capture(
        &mut self,
        audio: AudioProcessingBuffer<'_>,
    ) -> Result<(), AudioProcessorError> {
        self.audio(1, audio, false, false).map(|_| ())
    }
    pub fn process_capture(
        &mut self,
        audio: AudioProcessingBufferMut<'_>,
        level_change: bool,
    ) -> Result<(), AudioProcessorError> {
        let result = self.audio(2, audio.as_read(), level_change, false)?;
        if result.samples.len() != audio.as_read().samples().len() {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        let mut audio = audio;
        audio.samples_mut().copy_from_slice(&result.samples);
        Ok(())
    }
    pub fn process_capture_with_linear(
        &mut self,
        audio: AudioProcessingBufferMut<'_>,
        linear_output: Option<AudioProcessingBufferMut<'_>>,
        level_change: bool,
    ) -> Result<(), AudioProcessorError> {
        if let Some(linear) = &linear_output
            && (linear.sample_rate_hz() != 16000
                || linear.channels() != audio.channels()
                || linear.frames() != 160)
        {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        let result = self.audio(3, audio.as_read(), level_change, linear_output.is_some())?;
        if result.samples.len() != audio.as_read().samples().len()
            || linear_output
                .as_ref()
                .is_some_and(|v| v.as_read().samples().len() != result.linear.len())
        {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        let mut audio = audio;
        audio.samples_mut().copy_from_slice(&result.samples);
        if let Some(mut linear) = linear_output {
            linear.samples_mut().copy_from_slice(&result.linear);
        }
        Ok(())
    }
    fn audio(
        &mut self,
        operation: u8,
        audio: AudioProcessingBuffer<'_>,
        level_change: bool,
        linear: bool,
    ) -> Result<ffi::FfiEchoControlAudio, AudioProcessorError> {
        let result = ffi::audio_echo_control_audio(
            self.native.pin_mut(),
            operation,
            audio.sample_rate_hz(),
            audio.channels(),
            audio.samples(),
            level_change,
            linear,
        );
        AudioProcessingModule::admission(result.error)?;
        Ok(result)
    }
    pub fn metrics(&mut self) -> Result<EchoControlMetrics, AudioProcessorError> {
        let f = ffi::audio_echo_control_metrics(self.native.pin_mut());
        AudioProcessingModule::admission(f.error)?;
        Ok(EchoControlMetrics {
            echo_return_loss: f.echo_return_loss,
            echo_return_loss_enhancement: f.echo_return_loss_enhancement,
            delay_ms: f.delay_ms,
        })
    }
    pub fn set_audio_buffer_delay(&mut self, delay_ms: i32) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_echo_control_delay(
            self.native.pin_mut(),
            delay_ms,
        ))
    }
    pub fn set_capture_output_usage(&mut self, used: bool) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_echo_control_usage(self.native.pin_mut(), used))
    }
    pub fn active_processing(&mut self) -> Result<bool, AudioProcessorError> {
        let mut error = 0;
        let value = ffi::audio_echo_control_active(self.native.pin_mut(), &mut error);
        AudioProcessingModule::admission(error)?;
        Ok(value)
    }
}
