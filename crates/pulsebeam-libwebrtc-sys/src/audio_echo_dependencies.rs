//! Scoped dependencies of the exact native echo controller.
use crate::{
    Aec3Suppressor, AudioProcessingModule, AudioProcessorError, EchoCanceller3Config, Environment,
    NeuralEchoEstimate, NeuralEchoEstimateInput, ffi,
};
use std::{marker::PhantomData, pin::Pin, rc::Rc};

pub(crate) struct RustEchoControlContext {
    pub(crate) environment: Environment,
    pub(crate) rate: i32,
    pub(crate) render_channels: i32,
    pub(crate) capture_channels: i32,
}
/// Dependencies available only during a serialized controller callback. The
/// estimator is the actual creation input, not a new provider or stock fallback.
pub struct EchoControlDependencies<'a> {
    pub(crate) context: &'a RustEchoControlContext,
    pub(crate) native: Pin<&'a mut ffi::NativeEchoDependencies>,
    pub(crate) _sequence: PhantomData<Rc<()>>,
}
impl EchoControlDependencies<'_> {
    pub fn environment(&self) -> &Environment {
        &self.context.environment
    }
    pub fn sample_rate_hz(&self) -> i32 {
        self.context.rate
    }
    pub fn render_channels(&self) -> i32 {
        self.context.render_channels
    }
    pub fn capture_channels(&self) -> i32 {
        self.context.capture_channels
    }
    pub fn neural_estimator(&mut self) -> Option<NeuralResidualEchoEstimatorLoan<'_>> {
        loan(self.native.as_mut())
    }
}
/// Exclusive callback-scoped access. No reference, raw pointer, Send/Sync handle
/// or destructor access can outlive the actual SDK estimator's operational life.
pub struct NeuralResidualEchoEstimatorLoan<'a> {
    native: Pin<&'a mut ffi::NativeEchoDependencies>,
    _sequence: PhantomData<Rc<()>>,
}
pub(crate) fn loan(
    native: Pin<&mut ffi::NativeEchoDependencies>,
) -> Option<NeuralResidualEchoEstimatorLoan<'_>> {
    if ffi::audio_echo_dependency_present(native.as_ref().get_ref()) {
        Some(NeuralResidualEchoEstimatorLoan {
            native,
            _sequence: PhantomData,
        })
    } else {
        None
    }
}
impl NeuralResidualEchoEstimatorLoan<'_> {
    pub fn reset(&mut self) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_echo_dependency_reset(self.native.as_mut()))
    }
    pub fn adjust_config(
        &mut self,
        suppressor: &Aec3Suppressor,
    ) -> Result<Aec3Suppressor, AudioProcessorError> {
        let config = EchoCanceller3Config {
            suppressor: suppressor.clone(),
            ..Default::default()
        };
        let mut error = 0;
        let result =
            ffi::audio_echo_dependency_adjust(self.native.as_mut(), &config.ffi(), &mut error);
        AudioProcessingModule::admission(error)?;
        Ok(EchoCanceller3Config::from_ffi(result).suppressor)
    }
    pub fn estimate(
        &mut self,
        input: NeuralEchoEstimateInput<'_>,
    ) -> Result<NeuralEchoEstimate, AudioProcessorError> {
        let input = input.ffi().ok_or(AudioProcessorError::InvalidDimensions)?;
        crate::audio_processor_neural::estimate_result(ffi::audio_echo_dependency_estimate(
            self.native.as_mut(),
            input,
        ))
    }
}
