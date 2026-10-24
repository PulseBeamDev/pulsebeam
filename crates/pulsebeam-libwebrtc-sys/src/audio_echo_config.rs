//! Typed pinned AEC3 configuration. Defaults and explicit validation are native.
//! SDK Validate performs fixups; it is not a complete DSP safety certificate.
use crate::ffi;

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Buffering {
    pub excess_render_detection_interval_blocks: usize,
    pub max_allowed_excess_render_blocks: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3DelaySelectionThresholds {
    pub initial: i32,
    pub converged: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3AlignmentMixing {
    pub downmix: bool,
    pub adaptive_selection: bool,
    pub activity_power_threshold: f32,
    pub prefer_first_two_channels: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Delay {
    pub default_delay: usize,
    pub down_sampling_factor: usize,
    pub num_filters: usize,
    pub delay_headroom_samples: usize,
    pub hysteresis_limit_blocks: usize,
    pub fixed_capture_delay_samples: usize,
    pub delay_estimate_smoothing: f32,
    pub delay_estimate_smoothing_delay_found: f32,
    pub delay_candidate_detection_threshold: f32,
    pub delay_selection_thresholds: Aec3DelaySelectionThresholds,
    pub use_external_delay_estimator: bool,
    pub log_warning_on_delay_changes: bool,
    pub render_alignment_mixing: Aec3AlignmentMixing,
    pub capture_alignment_mixing: Aec3AlignmentMixing,
    pub detect_pre_echo: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3RefinedFilter {
    pub length_blocks: usize,
    pub leakage_converged: f32,
    pub leakage_diverged: f32,
    pub error_floor: f32,
    pub error_ceil: f32,
    pub noise_gate: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3CoarseFilter {
    pub length_blocks: usize,
    pub rate: f32,
    pub noise_gate: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Filter {
    pub refined: Aec3RefinedFilter,
    pub coarse: Aec3CoarseFilter,
    pub refined_initial: Aec3RefinedFilter,
    pub coarse_initial: Aec3CoarseFilter,
    pub config_change_duration_blocks: usize,
    pub initial_state_seconds: f32,
    pub coarse_reset_hangover_blocks: i32,
    pub conservative_initial_phase: bool,
    pub enable_coarse_filter_output_usage: bool,
    pub use_linear_filter: bool,
    pub high_pass_filter_echo_reference: bool,
    pub export_linear_aec_output: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Erle {
    pub min: f32,
    pub max_l: f32,
    pub max_h: f32,
    pub onset_detection: bool,
    pub num_sections: usize,
    pub clamp_quality_estimate_to_zero: bool,
    pub clamp_quality_estimate_to_one: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3EpStrength {
    pub default_gain: f32,
    pub default_len: f32,
    pub nearend_len: f32,
    pub echo_can_saturate: bool,
    pub bounded_erl: bool,
    pub erle_onset_compensation_in_dominant_nearend: bool,
    pub use_conservative_tail_frequency_response: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3EchoAudibility {
    pub low_render_limit: f32,
    pub normal_render_limit: f32,
    pub floor_power: f32,
    pub audibility_threshold_lf: f32,
    pub audibility_threshold_mf: f32,
    pub audibility_threshold_hf: f32,
    pub use_stationarity_properties: bool,
    pub use_stationarity_properties_at_init: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3RenderLevels {
    pub active_render_limit: f32,
    pub poor_excitation_render_limit: f32,
    pub poor_excitation_render_limit_ds8: f32,
    pub render_power_gain_db: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3EchoRemovalControl {
    pub has_clock_drift: bool,
    pub linear_and_stable_echo_path: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3EchoModel {
    pub noise_floor_hold: usize,
    pub min_noise_floor_power: f32,
    pub stationary_gate_slope: f32,
    pub noise_gate_power: f32,
    pub noise_gate_slope: f32,
    pub render_pre_window_size: usize,
    pub render_post_window_size: usize,
    pub model_reverb_in_nonlinear_mode: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3ComfortNoise {
    pub noise_floor_dbfs: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3MaskingThresholds {
    pub enr_transparent: f32,
    pub enr_suppress: f32,
    pub emr_transparent: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Tuning {
    pub mask_lf: Aec3MaskingThresholds,
    pub mask_hf: Aec3MaskingThresholds,
    pub max_inc_factor: f32,
    pub max_dec_factor_lf: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3DominantNearendDetection {
    pub enr_threshold: f32,
    pub enr_exit_threshold: f32,
    pub snr_threshold: f32,
    pub hold_duration: i32,
    pub trigger_threshold: i32,
    pub use_during_initial_phase: bool,
    pub use_unbounded_echo_spectrum: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3SubbandRegion {
    pub low: usize,
    pub high: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3SubbandNearendDetection {
    pub nearend_average_blocks: usize,
    pub subband1: Aec3SubbandRegion,
    pub subband2: Aec3SubbandRegion,
    pub nearend_threshold: f32,
    pub snr_threshold: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3HighBandsSuppression {
    pub enr_threshold: f32,
    pub max_gain_during_echo: f32,
    pub anti_howling_activation_threshold: f32,
    pub anti_howling_gain: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3HighFrequencySuppression {
    pub limiting_gain_band: i32,
    pub bands_in_limiting_gain: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3Suppressor {
    pub nearend_average_blocks: usize,
    pub normal_tuning: Aec3Tuning,
    pub nearend_tuning: Aec3Tuning,
    pub lf_smoothing_during_initial_phase: bool,
    pub last_permanent_lf_smoothing_band: i32,
    pub last_lf_smoothing_band: i32,
    pub last_lf_band: i32,
    pub first_hf_band: i32,
    pub dominant_nearend_detection: Aec3DominantNearendDetection,
    pub subband_nearend_detection: Aec3SubbandNearendDetection,
    pub use_subband_nearend_detection: bool,
    pub high_bands_suppression: Aec3HighBandsSuppression,
    pub high_frequency_suppression: Aec3HighFrequencySuppression,
    pub floor_first_increase: f32,
    pub conservative_hf_suppression: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aec3MultiChannel {
    pub detect_stereo_content: bool,
    pub stereo_detection_threshold: f32,
    pub stereo_detection_timeout_threshold_seconds: i32,
    pub stereo_detection_hysteresis_seconds: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EchoCanceller3Config {
    pub buffering: Aec3Buffering,
    pub delay: Aec3Delay,
    pub filter: Aec3Filter,
    pub erle: Aec3Erle,
    pub ep_strength: Aec3EpStrength,
    pub echo_audibility: Aec3EchoAudibility,
    pub render_levels: Aec3RenderLevels,
    pub echo_removal_control: Aec3EchoRemovalControl,
    pub echo_model: Aec3EchoModel,
    pub comfort_noise: Aec3ComfortNoise,
    pub suppressor: Aec3Suppressor,
    pub multi_channel: Aec3MultiChannel,
}

impl Default for EchoCanceller3Config {
    fn default() -> Self {
        Self::from_ffi(ffi::audio_echo_config_default(false))
    }
}
impl EchoCanceller3Config {
    pub fn default_multichannel() -> Self {
        Self::from_ffi(ffi::audio_echo_config_default(true))
    }
    /// Run the actual SDK Validate, applying its fixups to this value and
    /// returning true exactly when the SDK says no correction was needed.
    pub fn validate(&mut self) -> bool {
        let mut f = self.ffi();
        let valid = ffi::audio_echo_config_validate(&mut f);
        *self = Self::from_ffi(f);
        valid
    }
    pub(crate) fn from_ffi(f: ffi::FfiEchoCanceller3Config) -> Self {
        EchoCanceller3Config {
            buffering: Aec3Buffering {
                excess_render_detection_interval_blocks: f
                    .buffering_excess_render_detection_interval_blocks,
                max_allowed_excess_render_blocks: f.buffering_max_allowed_excess_render_blocks,
            },
            delay: Aec3Delay {
                default_delay: f.delay_default_delay,
                down_sampling_factor: f.delay_down_sampling_factor,
                num_filters: f.delay_num_filters,
                delay_headroom_samples: f.delay_delay_headroom_samples,
                hysteresis_limit_blocks: f.delay_hysteresis_limit_blocks,
                fixed_capture_delay_samples: f.delay_fixed_capture_delay_samples,
                delay_estimate_smoothing: f.delay_delay_estimate_smoothing,
                delay_estimate_smoothing_delay_found: f.delay_delay_estimate_smoothing_delay_found,
                delay_candidate_detection_threshold: f.delay_delay_candidate_detection_threshold,
                delay_selection_thresholds: Aec3DelaySelectionThresholds {
                    initial: f.delay_delay_selection_thresholds_initial,
                    converged: f.delay_delay_selection_thresholds_converged,
                },
                use_external_delay_estimator: f.delay_use_external_delay_estimator,
                log_warning_on_delay_changes: f.delay_log_warning_on_delay_changes,
                render_alignment_mixing: Aec3AlignmentMixing {
                    downmix: f.delay_render_alignment_mixing_downmix,
                    adaptive_selection: f.delay_render_alignment_mixing_adaptive_selection,
                    activity_power_threshold: f
                        .delay_render_alignment_mixing_activity_power_threshold,
                    prefer_first_two_channels: f
                        .delay_render_alignment_mixing_prefer_first_two_channels,
                },
                capture_alignment_mixing: Aec3AlignmentMixing {
                    downmix: f.delay_capture_alignment_mixing_downmix,
                    adaptive_selection: f.delay_capture_alignment_mixing_adaptive_selection,
                    activity_power_threshold: f
                        .delay_capture_alignment_mixing_activity_power_threshold,
                    prefer_first_two_channels: f
                        .delay_capture_alignment_mixing_prefer_first_two_channels,
                },
                detect_pre_echo: f.delay_detect_pre_echo,
            },
            filter: Aec3Filter {
                refined: Aec3RefinedFilter {
                    length_blocks: f.filter_refined_length_blocks,
                    leakage_converged: f.filter_refined_leakage_converged,
                    leakage_diverged: f.filter_refined_leakage_diverged,
                    error_floor: f.filter_refined_error_floor,
                    error_ceil: f.filter_refined_error_ceil,
                    noise_gate: f.filter_refined_noise_gate,
                },
                coarse: Aec3CoarseFilter {
                    length_blocks: f.filter_coarse_length_blocks,
                    rate: f.filter_coarse_rate,
                    noise_gate: f.filter_coarse_noise_gate,
                },
                refined_initial: Aec3RefinedFilter {
                    length_blocks: f.filter_refined_initial_length_blocks,
                    leakage_converged: f.filter_refined_initial_leakage_converged,
                    leakage_diverged: f.filter_refined_initial_leakage_diverged,
                    error_floor: f.filter_refined_initial_error_floor,
                    error_ceil: f.filter_refined_initial_error_ceil,
                    noise_gate: f.filter_refined_initial_noise_gate,
                },
                coarse_initial: Aec3CoarseFilter {
                    length_blocks: f.filter_coarse_initial_length_blocks,
                    rate: f.filter_coarse_initial_rate,
                    noise_gate: f.filter_coarse_initial_noise_gate,
                },
                config_change_duration_blocks: f.filter_config_change_duration_blocks,
                initial_state_seconds: f.filter_initial_state_seconds,
                coarse_reset_hangover_blocks: f.filter_coarse_reset_hangover_blocks,
                conservative_initial_phase: f.filter_conservative_initial_phase,
                enable_coarse_filter_output_usage: f.filter_enable_coarse_filter_output_usage,
                use_linear_filter: f.filter_use_linear_filter,
                high_pass_filter_echo_reference: f.filter_high_pass_filter_echo_reference,
                export_linear_aec_output: f.filter_export_linear_aec_output,
            },
            erle: Aec3Erle {
                min: f.erle_min,
                max_l: f.erle_max_l,
                max_h: f.erle_max_h,
                onset_detection: f.erle_onset_detection,
                num_sections: f.erle_num_sections,
                clamp_quality_estimate_to_zero: f.erle_clamp_quality_estimate_to_zero,
                clamp_quality_estimate_to_one: f.erle_clamp_quality_estimate_to_one,
            },
            ep_strength: Aec3EpStrength {
                default_gain: f.ep_strength_default_gain,
                default_len: f.ep_strength_default_len,
                nearend_len: f.ep_strength_nearend_len,
                echo_can_saturate: f.ep_strength_echo_can_saturate,
                bounded_erl: f.ep_strength_bounded_erl,
                erle_onset_compensation_in_dominant_nearend: f
                    .ep_strength_erle_onset_compensation_in_dominant_nearend,
                use_conservative_tail_frequency_response: f
                    .ep_strength_use_conservative_tail_frequency_response,
            },
            echo_audibility: Aec3EchoAudibility {
                low_render_limit: f.echo_audibility_low_render_limit,
                normal_render_limit: f.echo_audibility_normal_render_limit,
                floor_power: f.echo_audibility_floor_power,
                audibility_threshold_lf: f.echo_audibility_audibility_threshold_lf,
                audibility_threshold_mf: f.echo_audibility_audibility_threshold_mf,
                audibility_threshold_hf: f.echo_audibility_audibility_threshold_hf,
                use_stationarity_properties: f.echo_audibility_use_stationarity_properties,
                use_stationarity_properties_at_init: f
                    .echo_audibility_use_stationarity_properties_at_init,
            },
            render_levels: Aec3RenderLevels {
                active_render_limit: f.render_levels_active_render_limit,
                poor_excitation_render_limit: f.render_levels_poor_excitation_render_limit,
                poor_excitation_render_limit_ds8: f.render_levels_poor_excitation_render_limit_ds8,
                render_power_gain_db: f.render_levels_render_power_gain_db,
            },
            echo_removal_control: Aec3EchoRemovalControl {
                has_clock_drift: f.echo_removal_control_has_clock_drift,
                linear_and_stable_echo_path: f.echo_removal_control_linear_and_stable_echo_path,
            },
            echo_model: Aec3EchoModel {
                noise_floor_hold: f.echo_model_noise_floor_hold,
                min_noise_floor_power: f.echo_model_min_noise_floor_power,
                stationary_gate_slope: f.echo_model_stationary_gate_slope,
                noise_gate_power: f.echo_model_noise_gate_power,
                noise_gate_slope: f.echo_model_noise_gate_slope,
                render_pre_window_size: f.echo_model_render_pre_window_size,
                render_post_window_size: f.echo_model_render_post_window_size,
                model_reverb_in_nonlinear_mode: f.echo_model_model_reverb_in_nonlinear_mode,
            },
            comfort_noise: Aec3ComfortNoise {
                noise_floor_dbfs: f.comfort_noise_noise_floor_dbfs,
            },
            suppressor: Aec3Suppressor {
                nearend_average_blocks: f.suppressor_nearend_average_blocks,
                normal_tuning: Aec3Tuning {
                    mask_lf: Aec3MaskingThresholds {
                        enr_transparent: f.suppressor_normal_tuning_mask_lf_enr_transparent,
                        enr_suppress: f.suppressor_normal_tuning_mask_lf_enr_suppress,
                        emr_transparent: f.suppressor_normal_tuning_mask_lf_emr_transparent,
                    },
                    mask_hf: Aec3MaskingThresholds {
                        enr_transparent: f.suppressor_normal_tuning_mask_hf_enr_transparent,
                        enr_suppress: f.suppressor_normal_tuning_mask_hf_enr_suppress,
                        emr_transparent: f.suppressor_normal_tuning_mask_hf_emr_transparent,
                    },
                    max_inc_factor: f.suppressor_normal_tuning_max_inc_factor,
                    max_dec_factor_lf: f.suppressor_normal_tuning_max_dec_factor_lf,
                },
                nearend_tuning: Aec3Tuning {
                    mask_lf: Aec3MaskingThresholds {
                        enr_transparent: f.suppressor_nearend_tuning_mask_lf_enr_transparent,
                        enr_suppress: f.suppressor_nearend_tuning_mask_lf_enr_suppress,
                        emr_transparent: f.suppressor_nearend_tuning_mask_lf_emr_transparent,
                    },
                    mask_hf: Aec3MaskingThresholds {
                        enr_transparent: f.suppressor_nearend_tuning_mask_hf_enr_transparent,
                        enr_suppress: f.suppressor_nearend_tuning_mask_hf_enr_suppress,
                        emr_transparent: f.suppressor_nearend_tuning_mask_hf_emr_transparent,
                    },
                    max_inc_factor: f.suppressor_nearend_tuning_max_inc_factor,
                    max_dec_factor_lf: f.suppressor_nearend_tuning_max_dec_factor_lf,
                },
                lf_smoothing_during_initial_phase: f.suppressor_lf_smoothing_during_initial_phase,
                last_permanent_lf_smoothing_band: f.suppressor_last_permanent_lf_smoothing_band,
                last_lf_smoothing_band: f.suppressor_last_lf_smoothing_band,
                last_lf_band: f.suppressor_last_lf_band,
                first_hf_band: f.suppressor_first_hf_band,
                dominant_nearend_detection: Aec3DominantNearendDetection {
                    enr_threshold: f.suppressor_dominant_nearend_detection_enr_threshold,
                    enr_exit_threshold: f.suppressor_dominant_nearend_detection_enr_exit_threshold,
                    snr_threshold: f.suppressor_dominant_nearend_detection_snr_threshold,
                    hold_duration: f.suppressor_dominant_nearend_detection_hold_duration,
                    trigger_threshold: f.suppressor_dominant_nearend_detection_trigger_threshold,
                    use_during_initial_phase: f
                        .suppressor_dominant_nearend_detection_use_during_initial_phase,
                    use_unbounded_echo_spectrum: f
                        .suppressor_dominant_nearend_detection_use_unbounded_echo_spectrum,
                },
                subband_nearend_detection: Aec3SubbandNearendDetection {
                    nearend_average_blocks: f
                        .suppressor_subband_nearend_detection_nearend_average_blocks,
                    subband1: Aec3SubbandRegion {
                        low: f.suppressor_subband_nearend_detection_subband1_low,
                        high: f.suppressor_subband_nearend_detection_subband1_high,
                    },
                    subband2: Aec3SubbandRegion {
                        low: f.suppressor_subband_nearend_detection_subband2_low,
                        high: f.suppressor_subband_nearend_detection_subband2_high,
                    },
                    nearend_threshold: f.suppressor_subband_nearend_detection_nearend_threshold,
                    snr_threshold: f.suppressor_subband_nearend_detection_snr_threshold,
                },
                use_subband_nearend_detection: f.suppressor_use_subband_nearend_detection,
                high_bands_suppression: Aec3HighBandsSuppression {
                    enr_threshold: f.suppressor_high_bands_suppression_enr_threshold,
                    max_gain_during_echo: f.suppressor_high_bands_suppression_max_gain_during_echo,
                    anti_howling_activation_threshold: f
                        .suppressor_high_bands_suppression_anti_howling_activation_threshold,
                    anti_howling_gain: f.suppressor_high_bands_suppression_anti_howling_gain,
                },
                high_frequency_suppression: Aec3HighFrequencySuppression {
                    limiting_gain_band: f.suppressor_high_frequency_suppression_limiting_gain_band,
                    bands_in_limiting_gain: f
                        .suppressor_high_frequency_suppression_bands_in_limiting_gain,
                },
                floor_first_increase: f.suppressor_floor_first_increase,
                conservative_hf_suppression: f.suppressor_conservative_hf_suppression,
            },
            multi_channel: Aec3MultiChannel {
                detect_stereo_content: f.multi_channel_detect_stereo_content,
                stereo_detection_threshold: f.multi_channel_stereo_detection_threshold,
                stereo_detection_timeout_threshold_seconds: f
                    .multi_channel_stereo_detection_timeout_threshold_seconds,
                stereo_detection_hysteresis_seconds: f
                    .multi_channel_stereo_detection_hysteresis_seconds,
            },
        }
    }
    pub(crate) fn ffi(&self) -> ffi::FfiEchoCanceller3Config {
        ffi::FfiEchoCanceller3Config {
            buffering_excess_render_detection_interval_blocks: self
                .buffering
                .excess_render_detection_interval_blocks,
            buffering_max_allowed_excess_render_blocks: self
                .buffering
                .max_allowed_excess_render_blocks,
            delay_default_delay: self.delay.default_delay,
            delay_down_sampling_factor: self.delay.down_sampling_factor,
            delay_num_filters: self.delay.num_filters,
            delay_delay_headroom_samples: self.delay.delay_headroom_samples,
            delay_hysteresis_limit_blocks: self.delay.hysteresis_limit_blocks,
            delay_fixed_capture_delay_samples: self.delay.fixed_capture_delay_samples,
            delay_delay_estimate_smoothing: self.delay.delay_estimate_smoothing,
            delay_delay_estimate_smoothing_delay_found: self
                .delay
                .delay_estimate_smoothing_delay_found,
            delay_delay_candidate_detection_threshold: self
                .delay
                .delay_candidate_detection_threshold,
            delay_delay_selection_thresholds_initial: self.delay.delay_selection_thresholds.initial,
            delay_delay_selection_thresholds_converged: self
                .delay
                .delay_selection_thresholds
                .converged,
            delay_use_external_delay_estimator: self.delay.use_external_delay_estimator,
            delay_log_warning_on_delay_changes: self.delay.log_warning_on_delay_changes,
            delay_render_alignment_mixing_downmix: self.delay.render_alignment_mixing.downmix,
            delay_render_alignment_mixing_adaptive_selection: self
                .delay
                .render_alignment_mixing
                .adaptive_selection,
            delay_render_alignment_mixing_activity_power_threshold: self
                .delay
                .render_alignment_mixing
                .activity_power_threshold,
            delay_render_alignment_mixing_prefer_first_two_channels: self
                .delay
                .render_alignment_mixing
                .prefer_first_two_channels,
            delay_capture_alignment_mixing_downmix: self.delay.capture_alignment_mixing.downmix,
            delay_capture_alignment_mixing_adaptive_selection: self
                .delay
                .capture_alignment_mixing
                .adaptive_selection,
            delay_capture_alignment_mixing_activity_power_threshold: self
                .delay
                .capture_alignment_mixing
                .activity_power_threshold,
            delay_capture_alignment_mixing_prefer_first_two_channels: self
                .delay
                .capture_alignment_mixing
                .prefer_first_two_channels,
            delay_detect_pre_echo: self.delay.detect_pre_echo,
            filter_refined_length_blocks: self.filter.refined.length_blocks,
            filter_refined_leakage_converged: self.filter.refined.leakage_converged,
            filter_refined_leakage_diverged: self.filter.refined.leakage_diverged,
            filter_refined_error_floor: self.filter.refined.error_floor,
            filter_refined_error_ceil: self.filter.refined.error_ceil,
            filter_refined_noise_gate: self.filter.refined.noise_gate,
            filter_coarse_length_blocks: self.filter.coarse.length_blocks,
            filter_coarse_rate: self.filter.coarse.rate,
            filter_coarse_noise_gate: self.filter.coarse.noise_gate,
            filter_refined_initial_length_blocks: self.filter.refined_initial.length_blocks,
            filter_refined_initial_leakage_converged: self.filter.refined_initial.leakage_converged,
            filter_refined_initial_leakage_diverged: self.filter.refined_initial.leakage_diverged,
            filter_refined_initial_error_floor: self.filter.refined_initial.error_floor,
            filter_refined_initial_error_ceil: self.filter.refined_initial.error_ceil,
            filter_refined_initial_noise_gate: self.filter.refined_initial.noise_gate,
            filter_coarse_initial_length_blocks: self.filter.coarse_initial.length_blocks,
            filter_coarse_initial_rate: self.filter.coarse_initial.rate,
            filter_coarse_initial_noise_gate: self.filter.coarse_initial.noise_gate,
            filter_config_change_duration_blocks: self.filter.config_change_duration_blocks,
            filter_initial_state_seconds: self.filter.initial_state_seconds,
            filter_coarse_reset_hangover_blocks: self.filter.coarse_reset_hangover_blocks,
            filter_conservative_initial_phase: self.filter.conservative_initial_phase,
            filter_enable_coarse_filter_output_usage: self.filter.enable_coarse_filter_output_usage,
            filter_use_linear_filter: self.filter.use_linear_filter,
            filter_high_pass_filter_echo_reference: self.filter.high_pass_filter_echo_reference,
            filter_export_linear_aec_output: self.filter.export_linear_aec_output,
            erle_min: self.erle.min,
            erle_max_l: self.erle.max_l,
            erle_max_h: self.erle.max_h,
            erle_onset_detection: self.erle.onset_detection,
            erle_num_sections: self.erle.num_sections,
            erle_clamp_quality_estimate_to_zero: self.erle.clamp_quality_estimate_to_zero,
            erle_clamp_quality_estimate_to_one: self.erle.clamp_quality_estimate_to_one,
            ep_strength_default_gain: self.ep_strength.default_gain,
            ep_strength_default_len: self.ep_strength.default_len,
            ep_strength_nearend_len: self.ep_strength.nearend_len,
            ep_strength_echo_can_saturate: self.ep_strength.echo_can_saturate,
            ep_strength_bounded_erl: self.ep_strength.bounded_erl,
            ep_strength_erle_onset_compensation_in_dominant_nearend: self
                .ep_strength
                .erle_onset_compensation_in_dominant_nearend,
            ep_strength_use_conservative_tail_frequency_response: self
                .ep_strength
                .use_conservative_tail_frequency_response,
            echo_audibility_low_render_limit: self.echo_audibility.low_render_limit,
            echo_audibility_normal_render_limit: self.echo_audibility.normal_render_limit,
            echo_audibility_floor_power: self.echo_audibility.floor_power,
            echo_audibility_audibility_threshold_lf: self.echo_audibility.audibility_threshold_lf,
            echo_audibility_audibility_threshold_mf: self.echo_audibility.audibility_threshold_mf,
            echo_audibility_audibility_threshold_hf: self.echo_audibility.audibility_threshold_hf,
            echo_audibility_use_stationarity_properties: self
                .echo_audibility
                .use_stationarity_properties,
            echo_audibility_use_stationarity_properties_at_init: self
                .echo_audibility
                .use_stationarity_properties_at_init,
            render_levels_active_render_limit: self.render_levels.active_render_limit,
            render_levels_poor_excitation_render_limit: self
                .render_levels
                .poor_excitation_render_limit,
            render_levels_poor_excitation_render_limit_ds8: self
                .render_levels
                .poor_excitation_render_limit_ds8,
            render_levels_render_power_gain_db: self.render_levels.render_power_gain_db,
            echo_removal_control_has_clock_drift: self.echo_removal_control.has_clock_drift,
            echo_removal_control_linear_and_stable_echo_path: self
                .echo_removal_control
                .linear_and_stable_echo_path,
            echo_model_noise_floor_hold: self.echo_model.noise_floor_hold,
            echo_model_min_noise_floor_power: self.echo_model.min_noise_floor_power,
            echo_model_stationary_gate_slope: self.echo_model.stationary_gate_slope,
            echo_model_noise_gate_power: self.echo_model.noise_gate_power,
            echo_model_noise_gate_slope: self.echo_model.noise_gate_slope,
            echo_model_render_pre_window_size: self.echo_model.render_pre_window_size,
            echo_model_render_post_window_size: self.echo_model.render_post_window_size,
            echo_model_model_reverb_in_nonlinear_mode: self
                .echo_model
                .model_reverb_in_nonlinear_mode,
            comfort_noise_noise_floor_dbfs: self.comfort_noise.noise_floor_dbfs,
            suppressor_nearend_average_blocks: self.suppressor.nearend_average_blocks,
            suppressor_normal_tuning_mask_lf_enr_transparent: self
                .suppressor
                .normal_tuning
                .mask_lf
                .enr_transparent,
            suppressor_normal_tuning_mask_lf_enr_suppress: self
                .suppressor
                .normal_tuning
                .mask_lf
                .enr_suppress,
            suppressor_normal_tuning_mask_lf_emr_transparent: self
                .suppressor
                .normal_tuning
                .mask_lf
                .emr_transparent,
            suppressor_normal_tuning_mask_hf_enr_transparent: self
                .suppressor
                .normal_tuning
                .mask_hf
                .enr_transparent,
            suppressor_normal_tuning_mask_hf_enr_suppress: self
                .suppressor
                .normal_tuning
                .mask_hf
                .enr_suppress,
            suppressor_normal_tuning_mask_hf_emr_transparent: self
                .suppressor
                .normal_tuning
                .mask_hf
                .emr_transparent,
            suppressor_normal_tuning_max_inc_factor: self.suppressor.normal_tuning.max_inc_factor,
            suppressor_normal_tuning_max_dec_factor_lf: self
                .suppressor
                .normal_tuning
                .max_dec_factor_lf,
            suppressor_nearend_tuning_mask_lf_enr_transparent: self
                .suppressor
                .nearend_tuning
                .mask_lf
                .enr_transparent,
            suppressor_nearend_tuning_mask_lf_enr_suppress: self
                .suppressor
                .nearend_tuning
                .mask_lf
                .enr_suppress,
            suppressor_nearend_tuning_mask_lf_emr_transparent: self
                .suppressor
                .nearend_tuning
                .mask_lf
                .emr_transparent,
            suppressor_nearend_tuning_mask_hf_enr_transparent: self
                .suppressor
                .nearend_tuning
                .mask_hf
                .enr_transparent,
            suppressor_nearend_tuning_mask_hf_enr_suppress: self
                .suppressor
                .nearend_tuning
                .mask_hf
                .enr_suppress,
            suppressor_nearend_tuning_mask_hf_emr_transparent: self
                .suppressor
                .nearend_tuning
                .mask_hf
                .emr_transparent,
            suppressor_nearend_tuning_max_inc_factor: self.suppressor.nearend_tuning.max_inc_factor,
            suppressor_nearend_tuning_max_dec_factor_lf: self
                .suppressor
                .nearend_tuning
                .max_dec_factor_lf,
            suppressor_lf_smoothing_during_initial_phase: self
                .suppressor
                .lf_smoothing_during_initial_phase,
            suppressor_last_permanent_lf_smoothing_band: self
                .suppressor
                .last_permanent_lf_smoothing_band,
            suppressor_last_lf_smoothing_band: self.suppressor.last_lf_smoothing_band,
            suppressor_last_lf_band: self.suppressor.last_lf_band,
            suppressor_first_hf_band: self.suppressor.first_hf_band,
            suppressor_dominant_nearend_detection_enr_threshold: self
                .suppressor
                .dominant_nearend_detection
                .enr_threshold,
            suppressor_dominant_nearend_detection_enr_exit_threshold: self
                .suppressor
                .dominant_nearend_detection
                .enr_exit_threshold,
            suppressor_dominant_nearend_detection_snr_threshold: self
                .suppressor
                .dominant_nearend_detection
                .snr_threshold,
            suppressor_dominant_nearend_detection_hold_duration: self
                .suppressor
                .dominant_nearend_detection
                .hold_duration,
            suppressor_dominant_nearend_detection_trigger_threshold: self
                .suppressor
                .dominant_nearend_detection
                .trigger_threshold,
            suppressor_dominant_nearend_detection_use_during_initial_phase: self
                .suppressor
                .dominant_nearend_detection
                .use_during_initial_phase,
            suppressor_dominant_nearend_detection_use_unbounded_echo_spectrum: self
                .suppressor
                .dominant_nearend_detection
                .use_unbounded_echo_spectrum,
            suppressor_subband_nearend_detection_nearend_average_blocks: self
                .suppressor
                .subband_nearend_detection
                .nearend_average_blocks,
            suppressor_subband_nearend_detection_subband1_low: self
                .suppressor
                .subband_nearend_detection
                .subband1
                .low,
            suppressor_subband_nearend_detection_subband1_high: self
                .suppressor
                .subband_nearend_detection
                .subband1
                .high,
            suppressor_subband_nearend_detection_subband2_low: self
                .suppressor
                .subband_nearend_detection
                .subband2
                .low,
            suppressor_subband_nearend_detection_subband2_high: self
                .suppressor
                .subband_nearend_detection
                .subband2
                .high,
            suppressor_subband_nearend_detection_nearend_threshold: self
                .suppressor
                .subband_nearend_detection
                .nearend_threshold,
            suppressor_subband_nearend_detection_snr_threshold: self
                .suppressor
                .subband_nearend_detection
                .snr_threshold,
            suppressor_use_subband_nearend_detection: self.suppressor.use_subband_nearend_detection,
            suppressor_high_bands_suppression_enr_threshold: self
                .suppressor
                .high_bands_suppression
                .enr_threshold,
            suppressor_high_bands_suppression_max_gain_during_echo: self
                .suppressor
                .high_bands_suppression
                .max_gain_during_echo,
            suppressor_high_bands_suppression_anti_howling_activation_threshold: self
                .suppressor
                .high_bands_suppression
                .anti_howling_activation_threshold,
            suppressor_high_bands_suppression_anti_howling_gain: self
                .suppressor
                .high_bands_suppression
                .anti_howling_gain,
            suppressor_high_frequency_suppression_limiting_gain_band: self
                .suppressor
                .high_frequency_suppression
                .limiting_gain_band,
            suppressor_high_frequency_suppression_bands_in_limiting_gain: self
                .suppressor
                .high_frequency_suppression
                .bands_in_limiting_gain,
            suppressor_floor_first_increase: self.suppressor.floor_first_increase,
            suppressor_conservative_hf_suppression: self.suppressor.conservative_hf_suppression,
            multi_channel_detect_stereo_content: self.multi_channel.detect_stereo_content,
            multi_channel_stereo_detection_threshold: self.multi_channel.stereo_detection_threshold,
            multi_channel_stereo_detection_timeout_threshold_seconds: self
                .multi_channel
                .stereo_detection_timeout_threshold_seconds,
            multi_channel_stereo_detection_hysteresis_seconds: self
                .multi_channel
                .stereo_detection_hysteresis_seconds,
        }
    }
}
