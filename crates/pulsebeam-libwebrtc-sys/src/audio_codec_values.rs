use std::collections::BTreeMap;

use crate::{CodecError, ffi};

/// Complete native SDP audio format. Native strings are byte-preserving,
/// including embedded NUL and non-UTF-8 bytes; parameter keys are case-sensitive.
#[derive(Clone, Debug)]
pub struct SdpAudioFormat {
    pub name: Vec<u8>,
    pub clockrate_hz: i32,
    pub num_channels: usize,
    pub parameters: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl SdpAudioFormat {
    pub fn new(name: impl Into<Vec<u8>>, clockrate_hz: i32, num_channels: usize) -> Self {
        Self {
            name: name.into(),
            clockrate_hz,
            num_channels,
            parameters: BTreeMap::new(),
        }
    }

    /// Native offer/answer matching disregards parameters and compares names
    /// with ASCII case folding. It does not perform codec capability validation.
    pub fn matches(&self, other: &Self) -> bool {
        self.name.eq_ignore_ascii_case(&other.name)
            && self.clockrate_hz == other.clockrate_hz
            && self.num_channels == other.num_channels
    }

    pub fn to_native_description(&self) -> Vec<u8> {
        ffi::audio_format_description(&self.to_native())
    }

    pub(crate) fn to_native(&self) -> ffi::FfiSdpAudioFormat {
        ffi::FfiSdpAudioFormat {
            name: self.name.clone(),
            clockrate_hz: self.clockrate_hz,
            num_channels: self.num_channels,
            parameters: self
                .parameters
                .iter()
                .map(|(key, value)| ffi::FfiAudioCodecParameter {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }

    pub(crate) fn from_native(value: ffi::FfiSdpAudioFormat) -> Self {
        Self {
            name: value.name,
            clockrate_hz: value.clockrate_hz,
            num_channels: value.num_channels,
            parameters: value
                .parameters
                .into_iter()
                .map(|p| (p.key, p.value))
                .collect(),
        }
    }
}
impl PartialEq for SdpAudioFormat {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other) && self.parameters == other.parameters
    }
}
impl Eq for SdpAudioFormat {}

/// Full native audio implementation metadata. Values are not inferred from SDP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioCodecInfo {
    pub sample_rate_hz: i32,
    pub num_channels: usize,
    pub default_bitrate_bps: i32,
    pub min_bitrate_bps: i32,
    pub max_bitrate_bps: i32,
    pub allow_comfort_noise: bool,
    pub supports_network_adaption: bool,
}
impl AudioCodecInfo {
    /// Actual three-argument SDK constructor. Only assertions enabled in the
    /// matched native artifact are guarded. Construction does not authorize
    /// feeding this metadata to a media-engine or codec consumer.
    pub fn new(
        sample_rate_hz: i32,
        num_channels: usize,
        bitrate_bps: i32,
    ) -> Result<Self, CodecError> {
        let mut valid = false;
        let value = ffi::audio_codec_info_create(
            sample_rate_hz,
            num_channels,
            bitrate_bps,
            bitrate_bps,
            bitrate_bps,
            false,
            &mut valid,
        );
        if valid {
            Ok(Self::from_native(value))
        } else {
            Err(CodecError::InvalidConfiguration)
        }
    }

    /// Actual five-argument SDK constructor, retaining native defaults and
    /// every field unchanged. Enabled constructor DCHECKs are guarded locally;
    /// release artifacts preserve standalone metadata outside those domains.
    pub fn with_bitrate_range(
        sample_rate_hz: i32,
        num_channels: usize,
        default_bitrate_bps: i32,
        min_bitrate_bps: i32,
        max_bitrate_bps: i32,
    ) -> Result<Self, CodecError> {
        let mut valid = false;
        let value = ffi::audio_codec_info_create(
            sample_rate_hz,
            num_channels,
            default_bitrate_bps,
            min_bitrate_bps,
            max_bitrate_bps,
            true,
            &mut valid,
        );
        if valid {
            Ok(Self::from_native(value))
        } else {
            Err(CodecError::InvalidConfiguration)
        }
    }

    /// Actual SDK HasFixedBitrate, guarding only the reached bitrate DCHECKs
    /// in the matched artifact. Unrelated sample-rate/channel metadata remains
    /// intact; release artifacts preserve the SDK's direct equality behavior.
    pub fn has_fixed_bitrate(&self) -> Result<bool, CodecError> {
        let mut valid = false;
        let value = ffi::audio_codec_info_has_fixed_bitrate(&self.to_native(), &mut valid);
        if valid {
            Ok(value)
        } else {
            Err(CodecError::InvalidConfiguration)
        }
    }

    pub fn to_native_description(&self) -> Vec<u8> {
        ffi::audio_codec_info_description(&self.to_native())
    }

    pub(crate) fn to_native(&self) -> ffi::FfiAudioCodecInfo {
        ffi::FfiAudioCodecInfo {
            sample_rate_hz: self.sample_rate_hz,
            num_channels: self.num_channels,
            default_bitrate_bps: self.default_bitrate_bps,
            min_bitrate_bps: self.min_bitrate_bps,
            max_bitrate_bps: self.max_bitrate_bps,
            allow_comfort_noise: self.allow_comfort_noise,
            supports_network_adaption: self.supports_network_adaption,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiAudioCodecInfo) -> Self {
        Self {
            sample_rate_hz: value.sample_rate_hz,
            num_channels: value.num_channels,
            default_bitrate_bps: value.default_bitrate_bps,
            min_bitrate_bps: value.min_bitrate_bps,
            max_bitrate_bps: value.max_bitrate_bps,
            allow_comfort_noise: value.allow_comfort_noise,
            supports_network_adaption: value.supports_network_adaption,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioCodecSpec {
    pub format: SdpAudioFormat,
    pub info: AudioCodecInfo,
}
impl AudioCodecSpec {
    pub fn to_native_description(&self) -> Vec<u8> {
        ffi::audio_codec_spec_description(&ffi::FfiAudioCodecSpec {
            format: self.format.to_native(),
            info: self.info.to_native(),
        })
    }
    pub(crate) fn from_native(value: ffi::FfiAudioCodecSpec) -> Self {
        Self {
            format: SdpAudioFormat::from_native(value.format),
            info: AudioCodecInfo::from_native(value.info),
        }
    }
}
