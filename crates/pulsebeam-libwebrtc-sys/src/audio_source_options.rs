//! Owning values of the pinned native audio source options.

use crate::ffi;

/// Binding admission errors, not an invented result of the SDK's void setters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioSourceControlError {
    UnknownProcessingMode,
    InvalidVolume,
    MissingAudioTrack,
    UnknownStatus(u8),
}
impl AudioSourceControlError {
    pub(crate) fn result(code: u8) -> Result<(), Self> {
        match code {
            0 => Ok(()),
            1 => Err(Self::UnknownProcessingMode),
            2 => Err(Self::InvalidVolume),
            3 => Err(Self::MissingAudioTrack),
            code => Err(Self::UnknownStatus(code)),
        }
    }
}
impl std::fmt::Display for AudioSourceControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProcessingMode => {
                f.write_str("processing mode is outside the pinned native enum")
            }
            Self::InvalidVolume => f.write_str("native volume must be finite and in 0..=10"),
            Self::MissingAudioTrack => f.write_str("sender has no installed audio track"),
            Self::UnknownStatus(code) => write!(f, "unknown native source control status {code}"),
        }
    }
}
impl std::error::Error for AudioSourceControlError {}

/// Implementation choice for an enabled native processing component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioProcessingMode {
    Automatic,
    Platform,
    Software,
    /// Preserves an unrecognized native value without substituting a mode.
    Unknown(i32),
}
impl AudioProcessingMode {
    fn native(self) -> i32 {
        match self {
            Self::Automatic => 0,
            Self::Platform => 1,
            Self::Software => 2,
            Self::Unknown(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Automatic,
            1 => Self::Platform,
            2 => Self::Software,
            value => Self::Unknown(value),
        }
    }
}

/// Full Linux `AudioOptions` values. Absence differs from explicit false/zero.
/// These are source options, not a report of effective voice-engine settings.
/// In particular, carrying an ANA configuration does not enable an unavailable
/// adaptor or add a new encoder implementation.
#[derive(Clone, Debug)]
pub struct AudioSourceOptions {
    pub echo_cancellation: Option<bool>,
    pub echo_cancellation_mode: Option<AudioProcessingMode>,
    pub auto_gain_control: Option<bool>,
    pub auto_gain_control_mode: Option<AudioProcessingMode>,
    pub noise_suppression: Option<bool>,
    pub noise_suppression_mode: Option<AudioProcessingMode>,
    pub highpass_filter: Option<bool>,
    pub highpass_filter_mode: Option<AudioProcessingMode>,
    pub stereo_swapping: Option<bool>,
    pub audio_jitter_buffer_max_packets: Option<i32>,
    pub audio_jitter_buffer_fast_accelerate: Option<bool>,
    pub audio_jitter_buffer_min_delay_ms: Option<i32>,
    pub audio_network_adaptor: Option<bool>,
    /// Native serialized configuration bytes, not necessarily UTF-8.
    pub audio_network_adaptor_config: Option<Vec<u8>>,
    pub init_recording_on_send: Option<bool>,
}

impl Default for AudioSourceOptions {
    fn default() -> Self {
        Self::from_native(ffi::audio_options_default())
    }
}
impl PartialEq for AudioSourceOptions {
    fn eq(&self, other: &Self) -> bool {
        ffi::audio_options_equal(&self.ffi(), &other.ffi())
    }
}
impl Eq for AudioSourceOptions {}
impl AudioSourceOptions {
    /// Native SetAll: present fields replace corresponding fields; absent
    /// changes leave existing values unchanged. False and zero are present.
    pub fn set_all(&mut self, changes: &Self) {
        *self = Self::from_native(ffi::audio_options_set_all(&self.ffi(), &changes.ffi()));
    }
    /// Native diagnostic text. The SDK deliberately omits ANA config contents.
    pub fn native_description(&self) -> String {
        ffi::audio_options_description(&self.ffi())
    }
    pub(crate) fn ffi(&self) -> ffi::FfiAudioSourceOptions {
        fn boolean(value: Option<bool>) -> i8 {
            value.map_or(-1, i8::from)
        }
        fn integer(value: Option<i32>) -> ffi::FfiOptionalI32 {
            ffi::FfiOptionalI32 {
                present: value.is_some(),
                value: value.unwrap_or(0),
            }
        }
        fn mode(value: Option<AudioProcessingMode>) -> ffi::FfiOptionalI32 {
            integer(value.map(AudioProcessingMode::native))
        }
        ffi::FfiAudioSourceOptions {
            echo_cancellation: boolean(self.echo_cancellation),
            echo_cancellation_mode: mode(self.echo_cancellation_mode),
            auto_gain_control: boolean(self.auto_gain_control),
            auto_gain_control_mode: mode(self.auto_gain_control_mode),
            noise_suppression: boolean(self.noise_suppression),
            noise_suppression_mode: mode(self.noise_suppression_mode),
            highpass_filter: boolean(self.highpass_filter),
            highpass_filter_mode: mode(self.highpass_filter_mode),
            stereo_swapping: boolean(self.stereo_swapping),
            audio_jitter_buffer_max_packets: integer(self.audio_jitter_buffer_max_packets),
            audio_jitter_buffer_fast_accelerate: boolean(self.audio_jitter_buffer_fast_accelerate),
            audio_jitter_buffer_min_delay_ms: integer(self.audio_jitter_buffer_min_delay_ms),
            audio_network_adaptor: boolean(self.audio_network_adaptor),
            has_adaptor_config: self.audio_network_adaptor_config.is_some(),
            adaptor_config: self
                .audio_network_adaptor_config
                .clone()
                .unwrap_or_default(),
            init_recording_on_send: boolean(self.init_recording_on_send),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiAudioSourceOptions) -> Self {
        fn boolean(value: i8) -> Option<bool> {
            (value >= 0).then_some(value != 0)
        }
        fn integer(value: ffi::FfiOptionalI32) -> Option<i32> {
            value.present.then_some(value.value)
        }
        fn mode(value: ffi::FfiOptionalI32) -> Option<AudioProcessingMode> {
            integer(value).map(AudioProcessingMode::from_native)
        }
        Self {
            echo_cancellation: boolean(value.echo_cancellation),
            echo_cancellation_mode: mode(value.echo_cancellation_mode),
            auto_gain_control: boolean(value.auto_gain_control),
            auto_gain_control_mode: mode(value.auto_gain_control_mode),
            noise_suppression: boolean(value.noise_suppression),
            noise_suppression_mode: mode(value.noise_suppression_mode),
            highpass_filter: boolean(value.highpass_filter),
            highpass_filter_mode: mode(value.highpass_filter_mode),
            stereo_swapping: boolean(value.stereo_swapping),
            audio_jitter_buffer_max_packets: integer(value.audio_jitter_buffer_max_packets),
            audio_jitter_buffer_fast_accelerate: boolean(value.audio_jitter_buffer_fast_accelerate),
            audio_jitter_buffer_min_delay_ms: integer(value.audio_jitter_buffer_min_delay_ms),
            audio_network_adaptor: boolean(value.audio_network_adaptor),
            audio_network_adaptor_config: value.has_adaptor_config.then_some(value.adaptor_config),
            init_recording_on_send: boolean(value.init_recording_on_send),
        }
    }
}

/// Owning snapshot of the actual native track. No source/track handle or
/// cached requested state is substituted when a sender is detached.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioTrackStatus {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    pub state: AudioTrackState,
    pub volume: f64,
    pub signal_level: Option<i32>,
}
impl AudioTrackStatus {
    pub(crate) fn from_native(value: ffi::FfiAudioTrackStatus) -> Option<Self> {
        value.present.then(|| Self {
            id: value.id,
            kind: value.kind,
            enabled: value.enabled,
            state: AudioTrackState::from_native(value.state),
            volume: value.volume,
            signal_level: value
                .signal_level
                .present
                .then_some(value.signal_level.value),
        })
    }
}
impl crate::RtpSender {
    /// Invoke the installed audio track's native void volume setter. Success
    /// means the call returned, not that this source implements attenuation.
    pub fn set_audio_track_volume(&self, volume: f64) -> Result<(), AudioSourceControlError> {
        AudioSourceControlError::result(ffi::rtp_sender_set_audio_track_volume(
            self.peer.native(),
            self.native(),
            volume,
        ))
    }
    pub fn audio_track_status(&self) -> Option<AudioTrackStatus> {
        AudioTrackStatus::from_native(ffi::rtp_sender_audio_track_status(
            self.peer.native(),
            self.native(),
        ))
    }
}

/// Native media-source state, including initializing and muted states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioSourceState {
    Initializing,
    Live,
    Ended,
    Muted,
    Unknown(u8),
}
impl AudioSourceState {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Initializing,
            1 => Self::Live,
            2 => Self::Ended,
            3 => Self::Muted,
            value => Self::Unknown(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTrackState {
    Live,
    Ended,
    Unknown(u8),
}
impl AudioTrackState {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Live,
            1 => Self::Ended,
            value => Self::Unknown(value),
        }
    }
}
