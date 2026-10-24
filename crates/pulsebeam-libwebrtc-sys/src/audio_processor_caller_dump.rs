//! Unique caller AEC dump ownership and exact borrowed native callback values.
use crate::{AudioProcessingFormats, AudioProcessingRuntimeSetting, AudioProcessingStream, ffi};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct AudioProcessingDumpConfig {
    pub aec_enabled: bool,
    pub aec_delay_agnostic_enabled: bool,
    pub aec_drift_compensation_enabled: bool,
    pub aec_extended_filter_enabled: bool,
    pub aec_suppression_level: i32,
    pub agc_enabled: bool,
    pub agc_mode: i32,
    pub agc_limiter_enabled: bool,
    pub hpf_enabled: bool,
    pub ns_enabled: bool,
    pub ns_level: i32,
    pub transient_suppression_enabled: bool,
    pub noise_robust_agc_enabled: bool,
    pub pre_amplifier_enabled: bool,
    pub pre_amplifier_fixed_gain_factor: f32,
    pub experiments_description: Vec<u8>,
    pub api_config_string: Vec<u8>,
}
impl AudioProcessingDumpConfig {
    fn from_ffi(f: ffi::FfiApmDumpConfig) -> Self {
        Self {
            aec_enabled: f.aec_enabled,
            aec_delay_agnostic_enabled: f.aec_delay_agnostic_enabled,
            aec_drift_compensation_enabled: f.aec_drift_compensation_enabled,
            aec_extended_filter_enabled: f.aec_extended_filter_enabled,
            aec_suppression_level: f.aec_suppression_level,
            agc_enabled: f.agc_enabled,
            agc_mode: f.agc_mode,
            agc_limiter_enabled: f.agc_limiter_enabled,
            hpf_enabled: f.hpf_enabled,
            ns_enabled: f.ns_enabled,
            ns_level: f.ns_level,
            transient_suppression_enabled: f.transient_suppression_enabled,
            noise_robust_agc_enabled: f.noise_robust_agc_enabled,
            pre_amplifier_enabled: f.pre_amplifier_enabled,
            pre_amplifier_fixed_gain_factor: f.pre_amplifier_fixed_gain_factor,
            experiments_description: f.experiments_description,
            api_config_string: f.api_config_string,
        }
    }
    fn ffi(&self) -> ffi::FfiApmDumpConfig {
        ffi::FfiApmDumpConfig {
            aec_enabled: self.aec_enabled,
            aec_delay_agnostic_enabled: self.aec_delay_agnostic_enabled,
            aec_drift_compensation_enabled: self.aec_drift_compensation_enabled,
            aec_extended_filter_enabled: self.aec_extended_filter_enabled,
            aec_suppression_level: self.aec_suppression_level,
            agc_enabled: self.agc_enabled,
            agc_mode: self.agc_mode,
            agc_limiter_enabled: self.agc_limiter_enabled,
            hpf_enabled: self.hpf_enabled,
            ns_enabled: self.ns_enabled,
            ns_level: self.ns_level,
            transient_suppression_enabled: self.transient_suppression_enabled,
            noise_robust_agc_enabled: self.noise_robust_agc_enabled,
            pre_amplifier_enabled: self.pre_amplifier_enabled,
            pre_amplifier_fixed_gain_factor: self.pre_amplifier_fixed_gain_factor,
            experiments_description: self.experiments_description.clone(),
            api_config_string: self.api_config_string.clone(),
        }
    }
}
impl Default for AudioProcessingDumpConfig {
    fn default() -> Self {
        Self::from_ffi(ffi::audio_processor_dump_config_default())
    }
}
impl PartialEq for AudioProcessingDumpConfig {
    fn eq(&self, other: &Self) -> bool {
        ffi::audio_processor_dump_config_equal(&self.ffi(), &other.ffi())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingDumpState {
    pub delay: i32,
    pub drift: i32,
    pub applied_input_volume: Option<i32>,
    pub keypress: bool,
}
/// Values are copied without changing units, layouts or the native callback's
/// dimensions. Planar channels occupy consecutive samples_per_channel rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioProcessingDumpSamples<'a> {
    Interleaved {
        channels: usize,
        samples_per_channel: usize,
        samples: &'a [i16],
    },
    PlanarFrame {
        channels: usize,
        samples_per_channel: usize,
        samples: &'a [f32],
    },
    Mono(&'a [f32]),
    PlanarPointers {
        channels: usize,
        samples_per_channel: usize,
        samples: &'a [f32],
    },
}
#[derive(Debug)]
pub enum AudioProcessingDumpEvent<'a> {
    Initialize {
        formats: AudioProcessingFormats,
        time_now_ms: i64,
    },
    CaptureInput(AudioProcessingDumpSamples<'a>),
    CaptureOutput(AudioProcessingDumpSamples<'a>),
    ProcessingState(AudioProcessingDumpState),
    CaptureStream,
    RenderStream(AudioProcessingDumpSamples<'a>),
    RuntimeSetting(AudioProcessingRuntimeSetting),
    Config(&'a AudioProcessingDumpConfig),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioProcessingDumpOperation {
    Initialize,
    CaptureInput,
    CaptureOutput,
    ProcessingState,
    CaptureStream,
    RenderStream,
    RuntimeSetting,
    Config,
    Destroy,
    Bridge,
}
impl AudioProcessingDumpEvent<'_> {
    pub fn operation(&self) -> AudioProcessingDumpOperation {
        use AudioProcessingDumpOperation as O;
        match self {
            Self::Initialize { .. } => O::Initialize,
            Self::CaptureInput(_) => O::CaptureInput,
            Self::CaptureOutput(_) => O::CaptureOutput,
            Self::ProcessingState(_) => O::ProcessingState,
            Self::CaptureStream => O::CaptureStream,
            Self::RenderStream(_) => O::RenderStream,
            Self::RuntimeSetting(_) => O::RuntimeSetting,
            Self::Config(_) => O::Config,
        }
    }
}
/// The native interface permits concurrent Write calls. Implementations must
/// therefore be Send + Sync. Each event and its samples are borrowed only for
/// this call. All native overloads remain distinguishable in the typed events.
pub trait AudioProcessingDumpProvider: Send + Sync + 'static {
    fn record(&self, event: AudioProcessingDumpEvent<'_>) -> Result<(), String>;
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioProcessingDumpFailureKind {
    Rejected(String),
    Panicked,
    InvalidBridgeValue,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioProcessingDumpFailure {
    pub operation: AudioProcessingDumpOperation,
    pub kind: AudioProcessingDumpFailureKind,
}
#[derive(Default)]
struct Failures {
    first: Mutex<Option<AudioProcessingDumpFailure>>,
    count: AtomicU64,
}
#[derive(Clone, Default)]
pub struct AudioProcessingDumpFailureObserver(Arc<Failures>);
impl AudioProcessingDumpFailureObserver {
    pub fn failure(&self) -> Option<AudioProcessingDumpFailure> {
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
        operation: AudioProcessingDumpOperation,
        kind: AudioProcessingDumpFailureKind,
    ) {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(AudioProcessingDumpFailure { operation, kind });
        self.0.count.fetch_add(1, Ordering::Release);
    }
}
/// An owning, non-cloneable implementation of the native AecDump interface.
/// Attaching consumes it; the independent observer survives native destruction.
pub struct AudioProcessingDump {
    inner: RustCallerApmDump,
}
impl AudioProcessingDump {
    pub fn from_provider(provider: impl AudioProcessingDumpProvider) -> Self {
        Self {
            inner: RustCallerApmDump {
                provider: Some(Box::new(provider)),
                failure: AudioProcessingDumpFailureObserver::default(),
            },
        }
    }
    pub fn failure_observer(&self) -> AudioProcessingDumpFailureObserver {
        self.inner.failure.clone()
    }
    pub(crate) fn into_inner(self) -> Box<RustCallerApmDump> {
        Box::new(self.inner)
    }
}
pub(crate) struct RustCallerApmDump {
    provider: Option<Box<dyn AudioProcessingDumpProvider>>,
    failure: AudioProcessingDumpFailureObserver,
}
pub(crate) use crate::panic_payload::discard as dispose_panic;
impl RustCallerApmDump {
    fn call(
        &self,
        operation: AudioProcessingDumpOperation,
        f: impl FnOnce() -> Result<(), String>,
    ) -> bool {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(Ok(())) => true,
            Ok(Err(message)) => {
                self.failure
                    .record(operation, AudioProcessingDumpFailureKind::Rejected(message));
                false
            }
            Err(payload) => {
                dispose_panic(payload);
                self.failure
                    .record(operation, AudioProcessingDumpFailureKind::Panicked);
                false
            }
        }
    }
    fn event(&self, event: AudioProcessingDumpEvent<'_>) -> bool {
        self.call(event.operation(), || {
            self.provider
                .as_ref()
                .expect("live native dump callback")
                .record(event)
        })
    }
    fn invalid(&self) -> bool {
        self.failure.record(
            AudioProcessingDumpOperation::Bridge,
            AudioProcessingDumpFailureKind::InvalidBridgeValue,
        );
        false
    }
    fn dispose(&mut self) -> bool {
        let Some(provider) = self.provider.take() else {
            return true;
        };
        self.call(AudioProcessingDumpOperation::Destroy, || {
            drop(provider);
            Ok(())
        })
    }
}
impl Drop for RustCallerApmDump {
    fn drop(&mut self) {
        self.dispose();
    }
}
pub(crate) fn apm_dump_dispose(dump: &mut RustCallerApmDump) -> bool {
    dump.dispose()
}
pub(crate) fn apm_dump_initialize(
    dump: &RustCallerApmDump,
    f: ffi::FfiApmFormats,
    time_now_ms: i64,
) -> bool {
    dump.event(AudioProcessingDumpEvent::Initialize {
        formats: AudioProcessingFormats {
            input: AudioProcessingStream {
                sample_rate_hz: f.input_rate,
                channels: f.input_channels,
            },
            output: AudioProcessingStream {
                sample_rate_hz: f.output_rate,
                channels: f.output_channels,
            },
            reverse_input: AudioProcessingStream {
                sample_rate_hz: f.reverse_input_rate,
                channels: f.reverse_input_channels,
            },
            reverse_output: AudioProcessingStream {
                sample_rate_hz: f.reverse_output_rate,
                channels: f.reverse_output_channels,
            },
        },
        time_now_ms,
    })
}
pub(crate) fn apm_dump_audio<'a>(
    dump: &RustCallerApmDump,
    operation: u8,
    layout: u8,
    channels: usize,
    frames: usize,
    integers: &'a [i16],
    floats: &'a [f32],
) -> bool {
    let audio = match layout {
        0 => AudioProcessingDumpSamples::Interleaved {
            channels,
            samples_per_channel: frames,
            samples: integers,
        },
        1 => AudioProcessingDumpSamples::PlanarFrame {
            channels,
            samples_per_channel: frames,
            samples: floats,
        },
        2 => AudioProcessingDumpSamples::Mono(floats),
        3 => AudioProcessingDumpSamples::PlanarPointers {
            channels,
            samples_per_channel: frames,
            samples: floats,
        },
        _ => return dump.invalid(),
    };
    dump.event(match operation {
        0 => AudioProcessingDumpEvent::CaptureInput(audio),
        1 => AudioProcessingDumpEvent::CaptureOutput(audio),
        2 => AudioProcessingDumpEvent::RenderStream(audio),
        _ => return dump.invalid(),
    })
}
pub(crate) fn apm_dump_state(
    dump: &RustCallerApmDump,
    delay: i32,
    drift: i32,
    volume_present: bool,
    volume: i32,
    keypress: bool,
) -> bool {
    dump.event(AudioProcessingDumpEvent::ProcessingState(
        AudioProcessingDumpState {
            delay,
            drift,
            applied_input_volume: volume_present.then_some(volume),
            keypress,
        },
    ))
}
pub(crate) fn apm_dump_capture(dump: &RustCallerApmDump) -> bool {
    dump.event(AudioProcessingDumpEvent::CaptureStream)
}
pub(crate) fn apm_dump_runtime(dump: &RustCallerApmDump, f: ffi::FfiApmRuntimeSetting) -> bool {
    use AudioProcessingRuntimeSetting as S;
    let setting = match f.kind {
        0 => S::Unspecified,
        1 => S::CapturePreGain(f.float_value),
        2 => S::CapturePostGain(f.float_value),
        3 => S::CompressionGainDb(f.int_value),
        4 => S::CaptureFixedPostGainDb(f.float_value),
        5 => S::PlayoutVolumeChange(f.int_value),
        6 => S::CustomRenderSetting(f.float_value),
        7 => S::PlayoutAudioDeviceChange(crate::AudioPlayoutDeviceInfo {
            id: f.device_id,
            max_volume: f.device_max_volume,
        }),
        8 => S::CaptureOutputUsed(f.bool_value),
        _ => return dump.invalid(),
    };
    dump.event(AudioProcessingDumpEvent::RuntimeSetting(setting))
}
pub(crate) fn apm_dump_config(dump: &RustCallerApmDump, f: ffi::FfiApmDumpConfig) -> bool {
    dump.event(AudioProcessingDumpEvent::Config(
        &AudioProcessingDumpConfig::from_ffi(f),
    ))
}
