//! Owning neural estimator and exact 64-sample / 65-bin borrowed projections.
use crate::audio_processor_custom::{Provider, RustApmCustomModule};
use crate::{
    Aec3Suppressor, AudioProcessingCustomFailureKind, AudioProcessingCustomFailureObserver,
    AudioProcessingCustomOperation, AudioProcessingModule, AudioProcessorError,
    EchoCanceller3Config, ffi,
};
use std::{cell::Cell, marker::PhantomData, sync::Mutex};

#[derive(Clone, Copy)]
pub struct AudioProcessingRows<'a, const N: usize> {
    samples: &'a [f32],
}
impl<'a, const N: usize> AudioProcessingRows<'a, N> {
    pub fn from_rows(rows: &'a [[f32; N]]) -> Option<Self> {
        if N == 0 {
            None
        } else {
            Some(Self {
                samples: rows.as_flattened(),
            })
        }
    }
    fn from_flat(samples: &'a [f32]) -> Option<Self> {
        if N == 0 || !samples.len().is_multiple_of(N) {
            None
        } else {
            Some(Self { samples })
        }
    }
    pub fn len(&self) -> usize {
        self.samples.len() / N
    }
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
    pub fn row(&self, index: usize) -> Option<&'a [f32; N]> {
        let first = index.checked_mul(N)?;
        self.samples
            .get(first..first.checked_add(N)?)?
            .try_into()
            .ok()
    }
    pub fn samples(&self) -> &'a [f32] {
        self.samples
    }
}
#[derive(Clone, Copy)]
pub struct NeuralEchoRenderBlock<'a> {
    bands: usize,
    channels: usize,
    samples: &'a [f32],
}
impl<'a> NeuralEchoRenderBlock<'a> {
    pub fn from_samples(bands: usize, channels: usize, samples: &'a [f32]) -> Option<Self> {
        let len = bands.checked_mul(channels)?.checked_mul(64)?;
        // Native Block::GetIndex and its allocation product use int arithmetic.
        if bands > i32::MAX as usize
            || channels > i32::MAX as usize
            || len > i32::MAX as usize
            || len != samples.len()
        {
            return None;
        }
        Some(Self {
            bands,
            channels,
            samples,
        })
    }
    pub fn bands(&self) -> usize {
        self.bands
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn samples_per_band(&self) -> usize {
        64
    }
    pub fn band_sample_rate_hz(&self) -> i32 {
        16000
    }
    pub fn samples(&self) -> &'a [f32] {
        self.samples
    }
    pub fn band_channel(&self, band: usize, channel: usize) -> Option<&'a [f32; 64]> {
        if band >= self.bands || channel >= self.channels {
            return None;
        }
        let offset = band
            .checked_mul(self.channels)?
            .checked_add(channel)?
            .checked_mul(64)?;
        self.samples
            .get(offset..offset.checked_add(64)?)?
            .try_into()
            .ok()
    }
}
#[derive(Clone, Copy)]
pub struct NeuralEchoEstimateInput<'a> {
    pub render: NeuralEchoRenderBlock<'a>,
    pub microphone: AudioProcessingRows<'a, 64>,
    pub linear_error: AudioProcessingRows<'a, 64>,
    pub linear_echo_spectrum: AudioProcessingRows<'a, 65>,
    pub microphone_spectrum: AudioProcessingRows<'a, 65>,
    pub linear_error_spectrum: AudioProcessingRows<'a, 65>,
    pub dominant_nearend: bool,
    pub conservative_output_channels: usize,
    pub unbounded_output_channels: usize,
}
impl NeuralEchoEstimateInput<'_> {
    pub(crate) fn ffi(&self) -> Option<ffi::FfiNeuralEchoInput> {
        for channels in [
            self.conservative_output_channels,
            self.unbounded_output_channels,
        ] {
            if channels.checked_mul(65)? > isize::MAX as usize / std::mem::size_of::<f32>() {
                return None;
            }
        }
        Some(ffi::FfiNeuralEchoInput {
            render_bands: self.render.bands,
            render_channels: self.render.channels,
            render: self.render.samples.to_vec(),
            microphone: self.microphone.samples.to_vec(),
            linear_error: self.linear_error.samples.to_vec(),
            linear_echo_spectrum: self.linear_echo_spectrum.samples.to_vec(),
            microphone_spectrum: self.microphone_spectrum.samples.to_vec(),
            linear_error_spectrum: self.linear_error_spectrum.samples.to_vec(),
            dominant_nearend: self.dominant_nearend,
            conservative_channels: self.conservative_output_channels,
            unbounded_channels: self.unbounded_output_channels,
        })
    }
}
pub struct NeuralEchoEstimate {
    pub conservative: Vec<[f32; 65]>,
    pub unbounded: Vec<[f32; 65]>,
}
pub trait NeuralResidualEchoEstimatorProvider: Send + 'static {
    fn estimate(
        &mut self,
        input: NeuralEchoEstimateInput<'_>,
    ) -> Result<NeuralEchoEstimate, String>;
    fn adjust_config(&mut self, config: Aec3Suppressor) -> Result<Aec3Suppressor, String>;
    fn reset(&mut self) -> Result<(), String>;
}
pub(crate) fn apm_neural_estimate(
    module: &RustApmCustomModule,
    f: ffi::FfiNeuralEchoInput,
) -> ffi::FfiNeuralEchoResult {
    let failed = || ffi::FfiNeuralEchoResult {
        failed: true,
        conservative: Vec::new(),
        unbounded: Vec::new(),
    };
    let view = (|| {
        Some(NeuralEchoEstimateInput {
            render: NeuralEchoRenderBlock::from_samples(
                f.render_bands,
                f.render_channels,
                &f.render,
            )?,
            microphone: AudioProcessingRows::from_flat(&f.microphone)?,
            linear_error: AudioProcessingRows::from_flat(&f.linear_error)?,
            linear_echo_spectrum: AudioProcessingRows::from_flat(&f.linear_echo_spectrum)?,
            microphone_spectrum: AudioProcessingRows::from_flat(&f.microphone_spectrum)?,
            linear_error_spectrum: AudioProcessingRows::from_flat(&f.linear_error_spectrum)?,
            dominant_nearend: f.dominant_nearend,
            conservative_output_channels: f.conservative_channels,
            unbounded_output_channels: f.unbounded_channels,
        })
    })();
    let Some(view) = view else {
        module.failure.record(
            AudioProcessingCustomOperation::Estimate,
            AudioProcessingCustomFailureKind::InvalidBridgeValue,
        );
        return failed();
    };
    let value = module.call(AudioProcessingCustomOperation::Estimate, |p| match p {
        Provider::Neural(p) => p.estimate(view),
        _ => Err("neural estimator kind mismatch".into()),
    });
    let Some(value) = value else {
        return failed();
    };
    if value.conservative.len() != f.conservative_channels
        || value.unbounded.len() != f.unbounded_channels
    {
        module.failure.record(
            AudioProcessingCustomOperation::Estimate,
            AudioProcessingCustomFailureKind::InvalidBridgeValue,
        );
        return failed();
    }
    if value
        .conservative
        .iter()
        .chain(value.unbounded.iter())
        .flatten()
        .any(|v| !v.is_finite())
    {
        module.failure.record(
            AudioProcessingCustomOperation::Estimate,
            AudioProcessingCustomFailureKind::InvalidAudio,
        );
        return failed();
    }
    // Negative and extreme finite spectrum domains still require consumer-specific
    // qualification. This is not certified by shape/finiteness alone.
    ffi::FfiNeuralEchoResult {
        failed: false,
        conservative: value.conservative.into_iter().flatten().collect(),
        unbounded: value.unbounded.into_iter().flatten().collect(),
    }
}
pub(crate) fn apm_neural_adjust(
    module: &RustApmCustomModule,
    f: ffi::FfiEchoCanceller3Config,
    valid: &mut bool,
) -> ffi::FfiEchoCanceller3Config {
    let mut config = EchoCanceller3Config::from_ffi(f);
    let value = module.call(AudioProcessingCustomOperation::AdjustConfig, |p| match p {
        Provider::Neural(p) => p.adjust_config(config.suppressor.clone()),
        _ => Err("neural estimator kind mismatch".into()),
    });
    *valid = value.is_some();
    if let Some(value) = value {
        config.suppressor = value;
    }
    config.ffi()
}
pub(crate) fn apm_neural_adjust_invalid(module: &RustApmCustomModule) {
    module.failure.record(
        AudioProcessingCustomOperation::AdjustConfig,
        AudioProcessingCustomFailureKind::InvalidBridgeValue,
    );
}
pub(crate) fn apm_neural_invalid(module: &RustApmCustomModule) {
    module.failure.record(
        AudioProcessingCustomOperation::Estimate,
        AudioProcessingCustomFailureKind::InvalidBridgeValue,
    );
}
pub(crate) fn apm_neural_reset(module: &RustApmCustomModule) -> bool {
    module
        .call(AudioProcessingCustomOperation::Reset, |p| match p {
            Provider::Neural(p) => p.reset(),
            _ => Err("neural estimator kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn estimate_result(
    f: ffi::FfiNeuralEchoResult,
) -> Result<NeuralEchoEstimate, AudioProcessorError> {
    if f.failed {
        return Err(AudioProcessorError::CallbackFailed);
    }
    if !f.conservative.len().is_multiple_of(65) || !f.unbounded.len().is_multiple_of(65) {
        return Err(AudioProcessorError::InvalidDimensions);
    }
    let rows = |values: &[f32]| values.as_chunks::<65>().0.to_vec();
    Ok(NeuralEchoEstimate {
        conservative: rows(&f.conservative),
        unbounded: rows(&f.unbounded),
    })
}
pub struct NeuralResidualEchoEstimator {
    pub(crate) native: cxx::UniquePtr<ffi::NativeApmCustomModule>,
    failure: AudioProcessingCustomFailureObserver,
    _not_sync: PhantomData<Cell<()>>,
}
// Unique callback adapter contains only serialized Send provider and atomic status.
unsafe impl Send for NeuralResidualEchoEstimator {}
impl NeuralResidualEchoEstimator {
    pub fn from_provider(provider: impl NeuralResidualEchoEstimatorProvider) -> Self {
        let failure = AudioProcessingCustomFailureObserver::default();
        let module = Box::new(RustApmCustomModule {
            provider: Mutex::new(Some(Provider::Neural(Box::new(provider)))),
            failure: failure.clone(),
        });
        Self {
            native: ffi::audio_processor_neural_new(module),
            failure,
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> AudioProcessingCustomFailureObserver {
        self.failure.clone()
    }
    pub fn reset(&mut self) -> Result<(), AudioProcessorError> {
        AudioProcessingModule::admission(ffi::audio_processor_neural_reset(self.native.pin_mut()))
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
        let f =
            ffi::audio_processor_neural_adjust(self.native.pin_mut(), &config.ffi(), &mut error);
        AudioProcessingModule::admission(error)?;
        Ok(EchoCanceller3Config::from_ffi(f).suppressor)
    }
    pub fn estimate(
        &mut self,
        input: NeuralEchoEstimateInput<'_>,
    ) -> Result<NeuralEchoEstimate, AudioProcessorError> {
        let f = input.ffi().ok_or(AudioProcessorError::InvalidDimensions)?;
        let f = ffi::audio_processor_neural_estimate(self.native.pin_mut(), f);
        estimate_result(f)
    }
}
