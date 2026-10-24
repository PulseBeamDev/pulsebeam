//! Typed values for the pinned APM runtime-setting factories.
use crate::ffi;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioPlayoutDeviceInfo {
    pub id: i32,
    pub max_volume: i32,
}

/// Values are independent of consuming admission. In particular an inactive
/// pre/post-gain payload is retained verbatim, including nonfinite values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AudioProcessingRuntimeSetting {
    #[default]
    /// Native default. Posting this DCHECKs in asserting SDK builds.
    Unspecified,
    /// Linear factor, not decibels. Deferred until capture processing.
    CapturePreGain(f32),
    CapturePostGain(f32),
    /// SDK factory domain: 0 through 90 dB, inclusive.
    CompressionGainDb(i32),
    /// SDK runtime domain: 0 through 90 dB. GC2 reconstruction has a stricter
    /// DCHECK domain; see the operation-local reinitialization error.
    CaptureFixedPostGainDb(f32),
    /// Posts to both queues. A false acknowledgment does not roll back either.
    PlayoutVolumeChange(i32),
    CustomRenderSetting(f32),
    PlayoutAudioDeviceChange(AudioPlayoutDeviceInfo),
    CaptureOutputUsed(bool),
}
impl AudioProcessingRuntimeSetting {
    pub(crate) fn from_ffi(f: ffi::FfiApmRuntimeSetting) -> Option<Self> {
        Some(match f.kind {
            0 => Self::Unspecified,
            1 => Self::CapturePreGain(f.float_value),
            2 => Self::CapturePostGain(f.float_value),
            3 => Self::CompressionGainDb(f.int_value),
            4 => Self::CaptureFixedPostGainDb(f.float_value),
            5 => Self::PlayoutVolumeChange(f.int_value),
            6 => Self::CustomRenderSetting(f.float_value),
            7 => Self::PlayoutAudioDeviceChange(AudioPlayoutDeviceInfo {
                id: f.device_id,
                max_volume: f.device_max_volume,
            }),
            8 => Self::CaptureOutputUsed(f.bool_value),
            _ => return None,
        })
    }
    pub(crate) fn ffi(self) -> ffi::FfiApmRuntimeSetting {
        let mut f = ffi::FfiApmRuntimeSetting {
            kind: 0,
            float_value: 0.0,
            int_value: 0,
            device_id: 0,
            device_max_volume: 0,
            bool_value: false,
        };
        match self {
            Self::Unspecified => {}
            Self::CapturePreGain(v) => {
                f.kind = 1;
                f.float_value = v;
            }
            Self::CapturePostGain(v) => {
                f.kind = 2;
                f.float_value = v;
            }
            Self::CompressionGainDb(v) => {
                f.kind = 3;
                f.int_value = v;
            }
            Self::CaptureFixedPostGainDb(v) => {
                f.kind = 4;
                f.float_value = v;
            }
            Self::PlayoutVolumeChange(v) => {
                f.kind = 5;
                f.int_value = v;
            }
            Self::CustomRenderSetting(v) => {
                f.kind = 6;
                f.float_value = v;
            }
            Self::PlayoutAudioDeviceChange(v) => {
                f.kind = 7;
                f.device_id = v.id;
                f.device_max_volume = v.max_volume;
            }
            Self::CaptureOutputUsed(v) => {
                f.kind = 8;
                f.bool_value = v;
            }
        }
        f
    }
}
