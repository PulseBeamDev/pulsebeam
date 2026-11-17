//! Factory-scoped software audio processing configuration.
//!
//! This configures the factory's native processing module at construction. It
//! does not report platform state or promise that processing is active.

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

/// Legacy per-track request retained at the session boundary for source
/// compatibility. Vanilla WebRTC has no equivalent; requests are rejected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessingChoice {
    #[default]
    Disabled,
    Automatic,
    Platform,
    Software,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioProcessingOptions {
    pub echo_cancellation: ProcessingChoice,
    pub noise_suppression: ProcessingChoice,
    pub gain_control: ProcessingChoice,
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
