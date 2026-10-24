//! Scoped, initialized full/split-band echo-control callbacks.
use crate::audio_processor_custom::{Provider, RustApmCustomModule};
use crate::{
    AudioProcessingCustomFailureKind, AudioProcessingCustomOperation as Operation,
    AudioProcessorError, EchoControlMetrics, ffi,
};
use std::{marker::PhantomData, pin::Pin, rc::Rc};

/// Read-only native buffer snapshot. Full-band storage and split-band storage
/// are not synthesized from one another. Capture analysis precedes splitting,
/// so multi-band views are absent there. One-band views alias the full band.
pub struct EchoControlAudio<'a> {
    native: &'a ffi::NativeEchoAudio,
    _sequence: PhantomData<Rc<()>>,
}
impl EchoControlAudio<'_> {
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::audio_echo_view_shape(self.native).rate
    }
    pub fn channels(&self) -> usize {
        ffi::audio_echo_view_shape(self.native).channels
    }
    pub fn frames(&self) -> usize {
        ffi::audio_echo_view_shape(self.native).frames
    }
    pub fn bands(&self) -> usize {
        ffi::audio_echo_view_shape(self.native).bands
    }
    pub fn frames_per_band(&self) -> usize {
        ffi::audio_echo_view_shape(self.native).band_frames
    }
    pub fn split_bands_available(&self) -> bool {
        ffi::audio_echo_view_shape(self.native).split_ready
    }
    pub fn channel(&self, channel: usize) -> Option<&[f32]> {
        let mut valid = false;
        let samples = ffi::audio_echo_view_channel(self.native, channel, false, 0, &mut valid);
        valid.then_some(samples)
    }
    pub fn band(&self, channel: usize, band: usize) -> Option<&[f32]> {
        let mut valid = false;
        let samples = ffi::audio_echo_view_channel(self.native, channel, true, band, &mut valid);
        valid.then_some(samples)
    }
}
/// Mutable initialized capture snapshot. Changes commit only after successful
/// callback validation. Native merge semantics are preserved: at multiband rates
/// capture DSP changes split bands, and the native merge determines full output.
pub struct EchoControlAudioMut<'a> {
    native: Pin<&'a mut ffi::NativeEchoAudio>,
    _sequence: PhantomData<Rc<()>>,
}
impl EchoControlAudioMut<'_> {
    pub fn as_read(&self) -> EchoControlAudio<'_> {
        EchoControlAudio {
            native: self.native.as_ref().get_ref(),
            _sequence: PhantomData,
        }
    }
    pub fn channel_mut(&mut self, channel: usize) -> Option<&mut [f32]> {
        let mut valid = false;
        let samples =
            ffi::audio_echo_view_channel_mut(self.native.as_mut(), channel, false, 0, &mut valid);
        valid.then_some(samples)
    }
    pub fn band_mut(&mut self, channel: usize, band: usize) -> Option<&mut [f32]> {
        let mut valid = false;
        let samples =
            ffi::audio_echo_view_channel_mut(self.native.as_mut(), channel, true, band, &mut valid);
        valid.then_some(samples)
    }
}
/// Output-only linear frame. The binding never reads its uninitialized native
/// storage and never seeds it with pretend output. Every channel must be written
/// explicitly before a successful callback can commit anything to native output.
pub struct EchoControlLinearOutput<'a> {
    native: Pin<&'a mut ffi::NativeEchoLinearOutput>,
    _sequence: PhantomData<Rc<()>>,
}
impl EchoControlLinearOutput<'_> {
    pub fn sample_rate_hz(&self) -> i32 {
        (self.frames() * 100) as i32
    }
    pub fn channels(&self) -> usize {
        ffi::audio_echo_linear_channels(self.native.as_ref().get_ref())
    }
    pub fn frames(&self) -> usize {
        ffi::audio_echo_linear_frames(self.native.as_ref().get_ref())
    }
    pub fn write_channel(
        &mut self,
        channel: usize,
        samples: &[f32],
    ) -> Result<(), AudioProcessorError> {
        if ffi::audio_echo_linear_write(self.native.as_mut(), channel, samples) {
            Ok(())
        } else {
            Err(AudioProcessorError::InvalidDimensions)
        }
    }
}

/// Serialized Send-only native EchoControl implementation. Each overload is a
/// distinct operation; None is the real nullable linear output argument.
pub trait EchoControlProvider: Send + 'static {
    fn analyze_render(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
        audio: EchoControlAudio<'_>,
    ) -> Result<(), String>;
    fn analyze_capture(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
        audio: EchoControlAudio<'_>,
    ) -> Result<(), String>;
    fn process_capture(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
        audio: EchoControlAudioMut<'_>,
        level_change: bool,
    ) -> Result<(), String>;
    fn process_capture_with_linear(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
        audio: EchoControlAudioMut<'_>,
        linear: Option<EchoControlLinearOutput<'_>>,
        level_change: bool,
    ) -> Result<(), String>;
    fn metrics(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
    ) -> Result<EchoControlMetrics, String>;
    fn set_audio_buffer_delay(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
        milliseconds: i32,
    ) -> Result<(), String>;
    fn set_capture_output_usage(
        &mut self,
        _dependencies: crate::EchoControlDependencies<'_>,
        _used: bool,
    ) -> Result<(), String> {
        Ok(())
    }
    fn active_processing(
        &mut self,
        dependencies: crate::EchoControlDependencies<'_>,
    ) -> Result<bool, String>;
}
fn dependencies<'a>(
    context: &'a crate::audio_echo_dependencies::RustEchoControlContext,
    native: Pin<&'a mut ffi::NativeEchoDependencies>,
) -> crate::EchoControlDependencies<'a> {
    crate::EchoControlDependencies {
        context,
        native,
        _sequence: PhantomData,
    }
}
pub(crate) fn apm_echo_control_analyze(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
    audio: &ffi::NativeEchoAudio,
    render: bool,
) -> bool {
    module
        .call(Operation::Analyze, |p| match p {
            Provider::EchoControl(p, context) => {
                let audio = EchoControlAudio {
                    native: audio,
                    _sequence: PhantomData,
                };
                let dependencies = dependencies(context, native);
                if render {
                    p.analyze_render(dependencies, audio)
                } else {
                    p.analyze_capture(dependencies, audio)
                }
            }
            _ => Err("echo controller kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_control_process(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
    audio: Pin<&mut ffi::NativeEchoAudio>,
    linear: Pin<&mut ffi::NativeEchoLinearOutput>,
    linear_overload: bool,
    level_change: bool,
) -> bool {
    module
        .call(Operation::Process, |p| match p {
            Provider::EchoControl(p, context) => {
                let audio = EchoControlAudioMut {
                    native: audio,
                    _sequence: PhantomData,
                };
                let dependencies = dependencies(context, native);
                if linear_overload {
                    let present = ffi::audio_echo_linear_channels(linear.as_ref().get_ref()) != 0;
                    p.process_capture_with_linear(
                        dependencies,
                        audio,
                        present.then_some(EchoControlLinearOutput {
                            native: linear,
                            _sequence: PhantomData,
                        }),
                        level_change,
                    )
                } else {
                    p.process_capture(dependencies, audio, level_change)
                }
            }
            _ => Err("echo controller kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_control_metrics(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
) -> ffi::FfiEchoControlMetrics {
    let value = module.call(Operation::Metrics, |p| match p {
        Provider::EchoControl(p, context) => p.metrics(dependencies(context, native)),
        _ => Err("echo controller kind mismatch".into()),
    });
    match value {
        Some(m) => ffi::FfiEchoControlMetrics {
            error: 0,
            echo_return_loss: m.echo_return_loss,
            echo_return_loss_enhancement: m.echo_return_loss_enhancement,
            delay_ms: m.delay_ms,
        },
        None => ffi::FfiEchoControlMetrics {
            error: 18,
            echo_return_loss: 0.0,
            echo_return_loss_enhancement: 0.0,
            delay_ms: 0,
        },
    }
}
pub(crate) fn apm_echo_control_delay(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
    milliseconds: i32,
) -> bool {
    module
        .call(Operation::SetAudioBufferDelay, |p| match p {
            Provider::EchoControl(p, context) => {
                p.set_audio_buffer_delay(dependencies(context, native), milliseconds)
            }
            _ => Err("echo controller kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_control_usage(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
    used: bool,
) -> bool {
    module
        .call(Operation::SetCaptureOutputUsage, |p| match p {
            Provider::EchoControl(p, context) => {
                p.set_capture_output_usage(dependencies(context, native), used)
            }
            _ => Err("echo controller kind mismatch".into()),
        })
        .is_some()
}
pub(crate) fn apm_echo_control_active(
    module: &RustApmCustomModule,
    native: Pin<&mut ffi::NativeEchoDependencies>,
    valid: &mut bool,
) -> bool {
    let value = module.call(Operation::ActiveProcessing, |p| match p {
        Provider::EchoControl(p, context) => p.active_processing(dependencies(context, native)),
        _ => Err("echo controller kind mismatch".into()),
    });
    *valid = value.is_some();
    value.unwrap_or(false)
}
pub(crate) fn apm_echo_control_invalid(module: &RustApmCustomModule, audio: bool) {
    module.failure.record(
        Operation::Process,
        if audio {
            AudioProcessingCustomFailureKind::InvalidAudio
        } else {
            AudioProcessingCustomFailureKind::InvalidBridgeValue
        },
    );
}
