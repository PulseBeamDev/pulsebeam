//! Factory-scoped software audio processing and its live diagnostic state.
//!
//! This affects decoded PCM capture, not already encoded Opus packets. A
//! processing module is created only when explicitly configured on the factory.

use crate::ffi;

/// WebRTC's built-in noise suppression strength.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NoiseSuppression {
    #[default]
    Off,
    Low,
    Moderate,
    High,
    VeryHigh,
}

/// WebRTC's built-in automatic gain controller. Adaptive analog requires an
/// audio device with microphone gain feedback; use adaptive digital for a
/// source without such a device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GainControl {
    #[default]
    Off,
    AdaptiveAnalog,
    AdaptiveDigital,
    FixedDigital,
    AdaptiveDigitalV2,
}

/// Initial software APM settings, fixed when the factory is built. This does
/// not request or prove that platform hardware processing is running.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioProcessingConfig {
    pub echo_cancellation: bool,
    pub noise_suppression: NoiseSuppression,
    pub gain_control: GainControl,
}

impl AudioProcessingConfig {
    pub(crate) fn ffi(self) -> ffi::FfiAudioProcessingConfig {
        ffi::FfiAudioProcessingConfig {
            enabled: true,
            echo_cancellation: self.echo_cancellation,
            noise_suppression: self.noise_suppression as u8,
            gain_control: self.gain_control as u8,
        }
    }
}

/// Request for a local raw-audio track. Platform-only mode cannot fall back
/// to software when an effect is unavailable; automatic mode can.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessingChoice {
    #[default]
    Disabled,
    Automatic,
    Platform,
    Software,
}

/// Track-level requests. The engine's processing module is shared across
/// tracks, so concurrent tracks with different options are not isolated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioProcessingOptions {
    pub echo_cancellation: ProcessingChoice,
    pub noise_suppression: ProcessingChoice,
    pub gain_control: ProcessingChoice,
}

/// Actual implementation reported by the shared voice engine. `Unknown` is
/// not an assertion that processing is disabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessingImplementation {
    Unknown,
    Disabled,
    Software,
    Platform,
    SoftwareAndPlatform,
}

impl ProcessingImplementation {
    fn from_native(value: u8) -> Self {
        match value {
            1 => Self::Disabled,
            2 => Self::Software,
            3 => Self::Platform,
            4 => Self::SoftwareAndPlatform,
            _ => Self::Unknown,
        }
    }
}

/// Diagnostic snapshot. `software_active` is `None` when libwebrtc cannot
/// determine whether the component is active. Platform availability does not
/// imply that the device has started the effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessingComponentState {
    pub software_active: Option<bool>,
    pub platform_available: bool,
    pub platform_active: Option<bool>,
    pub effective: ProcessingImplementation,
}

impl ProcessingComponentState {
    fn from_native(software: i8, available: bool, platform: i8, effective: u8) -> Self {
        fn tristate(value: i8) -> Option<bool> {
            match value {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        }
        Self {
            software_active: tristate(software),
            platform_available: available,
            platform_active: tristate(platform),
            effective: ProcessingImplementation::from_native(effective),
        }
    }
}

/// Snapshot of factory-wide AEC, NS, AGC and high-pass-filter status. Echo
/// includes an owning selected controller independently of the native config bit;
/// the SDK's uncorrected config-derived echo report remains available separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingState {
    /// A failed/reentrant snapshot has unknown components, not fabricated state.
    pub failure: Option<crate::AudioProcessorError>,
    pub has_software_module: bool,
    pub echo_cancellation: ProcessingComponentState,
    /// Exact SDK echo diagnostic before accounting for the owning factory input.
    /// It is config-derived and can report disabled while a selected controller
    /// is processing. No platform observation is changed by the correction.
    pub native_reported_echo_cancellation: ProcessingComponentState,
    pub noise_suppression: ProcessingComponentState,
    pub gain_control: ProcessingComponentState,
    pub high_pass_filter: ProcessingComponentState,
}

impl From<ffi::FfiAudioProcessingState> for AudioProcessingState {
    fn from(s: ffi::FfiAudioProcessingState) -> Self {
        if let Some(failure) = crate::AudioProcessingModule::admission(s.binding_error).err() {
            let unknown = ProcessingComponentState::from_native(-1, false, -1, 0);
            return Self {
                failure: Some(failure),
                has_software_module: s.has_module,
                echo_cancellation: unknown,
                native_reported_echo_cancellation: unknown,
                noise_suppression: unknown,
                gain_control: unknown,
                high_pass_filter: unknown,
            };
        }
        Self {
            failure: crate::AudioProcessingModule::admission(s.binding_error).err(),
            has_software_module: s.has_module,
            echo_cancellation: ProcessingComponentState::from_native(
                s.echo_software,
                s.echo_platform_available,
                s.echo_platform,
                s.echo_effective,
            ),
            native_reported_echo_cancellation: ProcessingComponentState::from_native(
                s.echo_reported_software,
                s.echo_platform_available,
                s.echo_platform,
                s.echo_reported_effective,
            ),
            noise_suppression: ProcessingComponentState::from_native(
                s.noise_software,
                s.noise_platform_available,
                s.noise_platform,
                s.noise_effective,
            ),
            gain_control: ProcessingComponentState::from_native(
                s.gain_software,
                s.gain_platform_available,
                s.gain_platform,
                s.gain_effective,
            ),
            high_pass_filter: ProcessingComponentState::from_native(
                s.highpass_software,
                s.highpass_platform_available,
                s.highpass_platform,
                s.highpass_effective,
            ),
        }
    }
}
