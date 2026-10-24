//! Complete pinned AudioProcessing::Config values. Defaults come from the SDK.
use crate::ffi;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ApmDownmixMethod {
    AverageChannels = 0,
    UseFirstChannel = 1,
    Adaptive = 2,
}
impl ApmDownmixMethod {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::AverageChannels,
            1 => Self::UseFirstChannel,
            2 => Self::Adaptive,
            _ => panic!("invalid pinned APM enum"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ApmNoiseSuppressionLevel {
    Low = 0,
    Moderate = 1,
    High = 2,
    VeryHigh = 3,
}
impl ApmNoiseSuppressionLevel {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Low,
            1 => Self::Moderate,
            2 => Self::High,
            3 => Self::VeryHigh,
            _ => panic!("invalid pinned APM enum"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ApmGainController1Mode {
    AdaptiveAnalog = 0,
    AdaptiveDigital = 1,
    FixedDigital = 2,
}
impl ApmGainController1Mode {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::AdaptiveAnalog,
            1 => Self::AdaptiveDigital,
            2 => Self::FixedDigital,
            _ => panic!("invalid pinned APM enum"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ApmClippingPredictorMode {
    ClippingEventPrediction = 0,
    AdaptiveStepClippingPeakPrediction = 1,
    FixedStepClippingPeakPrediction = 2,
}
impl ApmClippingPredictorMode {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::ClippingEventPrediction,
            1 => Self::AdaptiveStepClippingPeakPrediction,
            2 => Self::FixedStepClippingPeakPrediction,
            _ => panic!("invalid pinned APM enum"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioProcessorConfig {
    pub pipeline: ApmPipeline,
    pub pre_amplifier: ApmPreAmplifier,
    pub capture_level_adjustment: ApmCaptureLevelAdjustment,
    pub high_pass_filter: ApmHighPassFilter,
    pub echo_canceller: ApmEchoCanceller,
    pub noise_suppression: ApmNoiseSuppression,
    pub transient_suppression: ApmTransientSuppression,
    pub gain_controller1: ApmGainController1,
    pub gain_controller2: ApmGainController2,
}
impl Default for AudioProcessorConfig {
    fn default() -> Self {
        Self::from_ffi(ffi::audio_processor_config_default())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmPipeline {
    pub maximum_internal_processing_rate: i32,
    pub multi_channel_render: bool,
    pub multi_channel_capture: bool,
    pub capture_downmix_method: ApmDownmixMethod,
    pub capture_downmix_method_stereo_aec: ApmDownmixMethod,
}
impl Default for ApmPipeline {
    fn default() -> Self {
        AudioProcessorConfig::default().pipeline
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmPreAmplifier {
    pub enabled: bool,
    pub fixed_gain_factor: f32,
}
impl Default for ApmPreAmplifier {
    fn default() -> Self {
        AudioProcessorConfig::default().pre_amplifier
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmCaptureLevelAdjustment {
    pub enabled: bool,
    pub pre_gain_factor: f32,
    pub post_gain_factor: f32,
    pub analog_mic_gain_emulation: ApmAnalogMicGainEmulation,
}
impl Default for ApmCaptureLevelAdjustment {
    fn default() -> Self {
        AudioProcessorConfig::default().capture_level_adjustment
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmAnalogMicGainEmulation {
    pub enabled: bool,
    pub initial_level: i32,
}
impl Default for ApmAnalogMicGainEmulation {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .capture_level_adjustment
            .analog_mic_gain_emulation
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmHighPassFilter {
    pub enabled: bool,
    pub apply_in_full_band: bool,
}
impl Default for ApmHighPassFilter {
    fn default() -> Self {
        AudioProcessorConfig::default().high_pass_filter
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmEchoCanceller {
    pub enabled: bool,
    pub export_linear_aec_output: bool,
    pub enforce_high_pass_filtering: bool,
}
impl Default for ApmEchoCanceller {
    fn default() -> Self {
        AudioProcessorConfig::default().echo_canceller
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmNoiseSuppression {
    pub enabled: bool,
    pub level: ApmNoiseSuppressionLevel,
    pub analyze_linear_aec_output_when_available: bool,
}
impl Default for ApmNoiseSuppression {
    fn default() -> Self {
        AudioProcessorConfig::default().noise_suppression
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmTransientSuppression {
    pub enabled: bool,
}
impl Default for ApmTransientSuppression {
    fn default() -> Self {
        AudioProcessorConfig::default().transient_suppression
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmGainController1 {
    pub enabled: bool,
    pub mode: ApmGainController1Mode,
    pub target_level_dbfs: i32,
    pub compression_gain_db: i32,
    pub enable_limiter: bool,
    pub analog_gain_controller: ApmAnalogGainController,
}
impl Default for ApmGainController1 {
    fn default() -> Self {
        AudioProcessorConfig::default().gain_controller1
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmAnalogGainController {
    pub enabled: bool,
    pub startup_min_volume: i32,
    pub clipped_level_min: i32,
    pub enable_digital_adaptive: bool,
    pub clipped_level_step: i32,
    pub clipped_ratio_threshold: f32,
    pub clipped_wait_frames: i32,
    pub clipping_predictor: ApmClippingPredictor,
}
impl Default for ApmAnalogGainController {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .gain_controller1
            .analog_gain_controller
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmClippingPredictor {
    pub enabled: bool,
    pub mode: ApmClippingPredictorMode,
    pub window_length: i32,
    pub reference_window_length: i32,
    pub reference_window_delay: i32,
    pub clipping_threshold: f32,
    pub crest_factor_margin: f32,
    pub use_predicted_step: bool,
}
impl Default for ApmClippingPredictor {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .gain_controller1
            .analog_gain_controller
            .clipping_predictor
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmGainController2 {
    pub enabled: bool,
    pub input_volume_controller: ApmInputVolumeController,
    pub adaptive_digital: ApmAdaptiveDigital,
    pub fixed_digital: ApmFixedDigital,
}
impl Default for ApmGainController2 {
    fn default() -> Self {
        AudioProcessorConfig::default().gain_controller2
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmInputVolumeController {
    pub enabled: bool,
}
impl Default for ApmInputVolumeController {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .gain_controller2
            .input_volume_controller
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmAdaptiveDigital {
    pub enabled: bool,
    pub headroom_db: f32,
    pub max_gain_db: f32,
    pub initial_gain_db: f32,
    pub max_gain_change_db_per_second: f32,
    pub max_output_noise_level_dbfs: f32,
}
impl Default for ApmAdaptiveDigital {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .gain_controller2
            .adaptive_digital
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApmFixedDigital {
    pub gain_db: f32,
}
impl Default for ApmFixedDigital {
    fn default() -> Self {
        AudioProcessorConfig::default()
            .gain_controller2
            .fixed_digital
    }
}

impl ApmCaptureLevelAdjustment {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.capture_level_adjustment = *self;
        right.capture_level_adjustment = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 0)
    }
}

impl ApmAnalogMicGainEmulation {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.capture_level_adjustment.analog_mic_gain_emulation = *self;
        right.capture_level_adjustment.analog_mic_gain_emulation = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 1)
    }
}

impl ApmGainController1 {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.gain_controller1 = *self;
        right.gain_controller1 = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 2)
    }
}

impl ApmGainController2 {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.gain_controller2 = *self;
        right.gain_controller2 = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 3)
    }
}

impl ApmInputVolumeController {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.gain_controller2.input_volume_controller = *self;
        right.gain_controller2.input_volume_controller = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 4)
    }
}

impl ApmAdaptiveDigital {
    /// Exact SDK group equality, including fields omitted by the pin's operator.
    pub fn native_equals(&self, other: &Self) -> bool {
        let mut left = AudioProcessorConfig::default();
        let mut right = left;
        left.gain_controller2.adaptive_digital = *self;
        right.gain_controller2.adaptive_digital = *other;
        ffi::audio_processor_config_equal(&left.ffi(), &right.ffi(), 5)
    }
}

impl AudioProcessorConfig {
    pub(crate) fn ffi(self) -> ffi::FfiApmConfig {
        ffi::FfiApmConfig {
            pipeline_maximum_internal_processing_rate: self
                .pipeline
                .maximum_internal_processing_rate,
            pipeline_multi_channel_render: self.pipeline.multi_channel_render,
            pipeline_multi_channel_capture: self.pipeline.multi_channel_capture,
            pipeline_capture_downmix_method: self.pipeline.capture_downmix_method as i32,
            pipeline_capture_downmix_method_stereo_aec: self
                .pipeline
                .capture_downmix_method_stereo_aec
                as i32,
            pre_amplifier_enabled: self.pre_amplifier.enabled,
            pre_amplifier_fixed_gain_factor: self.pre_amplifier.fixed_gain_factor,
            capture_level_adjustment_enabled: self.capture_level_adjustment.enabled,
            capture_level_adjustment_pre_gain_factor: self.capture_level_adjustment.pre_gain_factor,
            capture_level_adjustment_post_gain_factor: self
                .capture_level_adjustment
                .post_gain_factor,
            capture_level_adjustment_analog_mic_gain_emulation_enabled: self
                .capture_level_adjustment
                .analog_mic_gain_emulation
                .enabled,
            capture_level_adjustment_analog_mic_gain_emulation_initial_level: self
                .capture_level_adjustment
                .analog_mic_gain_emulation
                .initial_level,
            high_pass_filter_enabled: self.high_pass_filter.enabled,
            high_pass_filter_apply_in_full_band: self.high_pass_filter.apply_in_full_band,
            echo_canceller_enabled: self.echo_canceller.enabled,
            echo_canceller_export_linear_aec_output: self.echo_canceller.export_linear_aec_output,
            echo_canceller_enforce_high_pass_filtering: self
                .echo_canceller
                .enforce_high_pass_filtering,
            noise_suppression_enabled: self.noise_suppression.enabled,
            noise_suppression_level: self.noise_suppression.level as i32,
            noise_suppression_analyze_linear_aec_output_when_available: self
                .noise_suppression
                .analyze_linear_aec_output_when_available,
            transient_suppression_enabled: self.transient_suppression.enabled,
            gain_controller1_enabled: self.gain_controller1.enabled,
            gain_controller1_mode: self.gain_controller1.mode as i32,
            gain_controller1_target_level_dbfs: self.gain_controller1.target_level_dbfs,
            gain_controller1_compression_gain_db: self.gain_controller1.compression_gain_db,
            gain_controller1_enable_limiter: self.gain_controller1.enable_limiter,
            gain_controller1_analog_gain_controller_enabled: self
                .gain_controller1
                .analog_gain_controller
                .enabled,
            gain_controller1_analog_gain_controller_startup_min_volume: self
                .gain_controller1
                .analog_gain_controller
                .startup_min_volume,
            gain_controller1_analog_gain_controller_clipped_level_min: self
                .gain_controller1
                .analog_gain_controller
                .clipped_level_min,
            gain_controller1_analog_gain_controller_enable_digital_adaptive: self
                .gain_controller1
                .analog_gain_controller
                .enable_digital_adaptive,
            gain_controller1_analog_gain_controller_clipped_level_step: self
                .gain_controller1
                .analog_gain_controller
                .clipped_level_step,
            gain_controller1_analog_gain_controller_clipped_ratio_threshold: self
                .gain_controller1
                .analog_gain_controller
                .clipped_ratio_threshold,
            gain_controller1_analog_gain_controller_clipped_wait_frames: self
                .gain_controller1
                .analog_gain_controller
                .clipped_wait_frames,
            gain_controller1_analog_gain_controller_clipping_predictor_enabled: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .enabled,
            gain_controller1_analog_gain_controller_clipping_predictor_mode: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .mode
                as i32,
            gain_controller1_analog_gain_controller_clipping_predictor_window_length: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .window_length,
            gain_controller1_analog_gain_controller_clipping_predictor_reference_window_length:
                self.gain_controller1
                    .analog_gain_controller
                    .clipping_predictor
                    .reference_window_length,
            gain_controller1_analog_gain_controller_clipping_predictor_reference_window_delay: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .reference_window_delay,
            gain_controller1_analog_gain_controller_clipping_predictor_clipping_threshold: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .clipping_threshold,
            gain_controller1_analog_gain_controller_clipping_predictor_crest_factor_margin: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .crest_factor_margin,
            gain_controller1_analog_gain_controller_clipping_predictor_use_predicted_step: self
                .gain_controller1
                .analog_gain_controller
                .clipping_predictor
                .use_predicted_step,
            gain_controller2_enabled: self.gain_controller2.enabled,
            gain_controller2_input_volume_controller_enabled: self
                .gain_controller2
                .input_volume_controller
                .enabled,
            gain_controller2_adaptive_digital_enabled: self
                .gain_controller2
                .adaptive_digital
                .enabled,
            gain_controller2_adaptive_digital_headroom_db: self
                .gain_controller2
                .adaptive_digital
                .headroom_db,
            gain_controller2_adaptive_digital_max_gain_db: self
                .gain_controller2
                .adaptive_digital
                .max_gain_db,
            gain_controller2_adaptive_digital_initial_gain_db: self
                .gain_controller2
                .adaptive_digital
                .initial_gain_db,
            gain_controller2_adaptive_digital_max_gain_change_db_per_second: self
                .gain_controller2
                .adaptive_digital
                .max_gain_change_db_per_second,
            gain_controller2_adaptive_digital_max_output_noise_level_dbfs: self
                .gain_controller2
                .adaptive_digital
                .max_output_noise_level_dbfs,
            gain_controller2_fixed_digital_gain_db: self.gain_controller2.fixed_digital.gain_db,
        }
    }
    pub(crate) fn from_ffi(v: ffi::FfiApmConfig) -> Self {
        AudioProcessorConfig {
    pipeline: ApmPipeline {
    maximum_internal_processing_rate: v.pipeline_maximum_internal_processing_rate,
    multi_channel_render: v.pipeline_multi_channel_render,
    multi_channel_capture: v.pipeline_multi_channel_capture,
    capture_downmix_method: ApmDownmixMethod::from_native(v.pipeline_capture_downmix_method),
    capture_downmix_method_stereo_aec: ApmDownmixMethod::from_native(v.pipeline_capture_downmix_method_stereo_aec),
},
    pre_amplifier: ApmPreAmplifier {
    enabled: v.pre_amplifier_enabled,
    fixed_gain_factor: v.pre_amplifier_fixed_gain_factor,
},
    capture_level_adjustment: ApmCaptureLevelAdjustment {
    enabled: v.capture_level_adjustment_enabled,
    pre_gain_factor: v.capture_level_adjustment_pre_gain_factor,
    post_gain_factor: v.capture_level_adjustment_post_gain_factor,
    analog_mic_gain_emulation: ApmAnalogMicGainEmulation {
    enabled: v.capture_level_adjustment_analog_mic_gain_emulation_enabled,
    initial_level: v.capture_level_adjustment_analog_mic_gain_emulation_initial_level,
},
},
    high_pass_filter: ApmHighPassFilter {
    enabled: v.high_pass_filter_enabled,
    apply_in_full_band: v.high_pass_filter_apply_in_full_band,
},
    echo_canceller: ApmEchoCanceller {
    enabled: v.echo_canceller_enabled,
    export_linear_aec_output: v.echo_canceller_export_linear_aec_output,
    enforce_high_pass_filtering: v.echo_canceller_enforce_high_pass_filtering,
},
    noise_suppression: ApmNoiseSuppression {
    enabled: v.noise_suppression_enabled,
    level: ApmNoiseSuppressionLevel::from_native(v.noise_suppression_level),
    analyze_linear_aec_output_when_available: v.noise_suppression_analyze_linear_aec_output_when_available,
},
    transient_suppression: ApmTransientSuppression {
    enabled: v.transient_suppression_enabled,
},
    gain_controller1: ApmGainController1 {
    enabled: v.gain_controller1_enabled,
    mode: ApmGainController1Mode::from_native(v.gain_controller1_mode),
    target_level_dbfs: v.gain_controller1_target_level_dbfs,
    compression_gain_db: v.gain_controller1_compression_gain_db,
    enable_limiter: v.gain_controller1_enable_limiter,
    analog_gain_controller: ApmAnalogGainController {
    enabled: v.gain_controller1_analog_gain_controller_enabled,
    startup_min_volume: v.gain_controller1_analog_gain_controller_startup_min_volume,
    clipped_level_min: v.gain_controller1_analog_gain_controller_clipped_level_min,
    enable_digital_adaptive: v.gain_controller1_analog_gain_controller_enable_digital_adaptive,
    clipped_level_step: v.gain_controller1_analog_gain_controller_clipped_level_step,
    clipped_ratio_threshold: v.gain_controller1_analog_gain_controller_clipped_ratio_threshold,
    clipped_wait_frames: v.gain_controller1_analog_gain_controller_clipped_wait_frames,
    clipping_predictor: ApmClippingPredictor {
    enabled: v.gain_controller1_analog_gain_controller_clipping_predictor_enabled,
    mode: ApmClippingPredictorMode::from_native(v.gain_controller1_analog_gain_controller_clipping_predictor_mode),
    window_length: v.gain_controller1_analog_gain_controller_clipping_predictor_window_length,
    reference_window_length: v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_length,
    reference_window_delay: v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_delay,
    clipping_threshold: v.gain_controller1_analog_gain_controller_clipping_predictor_clipping_threshold,
    crest_factor_margin: v.gain_controller1_analog_gain_controller_clipping_predictor_crest_factor_margin,
    use_predicted_step: v.gain_controller1_analog_gain_controller_clipping_predictor_use_predicted_step,
},
},
},
    gain_controller2: ApmGainController2 {
    enabled: v.gain_controller2_enabled,
    input_volume_controller: ApmInputVolumeController {
    enabled: v.gain_controller2_input_volume_controller_enabled,
},
    adaptive_digital: ApmAdaptiveDigital {
    enabled: v.gain_controller2_adaptive_digital_enabled,
    headroom_db: v.gain_controller2_adaptive_digital_headroom_db,
    max_gain_db: v.gain_controller2_adaptive_digital_max_gain_db,
    initial_gain_db: v.gain_controller2_adaptive_digital_initial_gain_db,
    max_gain_change_db_per_second: v.gain_controller2_adaptive_digital_max_gain_change_db_per_second,
    max_output_noise_level_dbfs: v.gain_controller2_adaptive_digital_max_output_noise_level_dbfs,
},
    fixed_digital: ApmFixedDigital {
    gain_db: v.gain_controller2_fixed_digital_gain_db,
},
},
}
    }
    /// The SDK's own description of this value, not a binding configuration echo.
    pub fn to_native_string(&self) -> String {
        ffi::audio_processor_config_string(&self.ffi())
    }
}
