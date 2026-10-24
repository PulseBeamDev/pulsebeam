//! Runtime-neutral observation for SDK controls without native error returns.
use crate::{AudioEncoderError, ffi};

/// Open operation tag for rejected SDK forwarding controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioEncoderSdkOperation(pub u8);
impl AudioEncoderSdkOperation {
    pub const SET_TARGET_BITRATE: Self = Self(1);
    pub const TARGET_AUDIO_BITRATE: Self = Self(2);
    pub const UPLINK_BANDWIDTH: Self = Self(3);
    pub const UPLINK_ALLOCATION: Self = Self(4);
    pub const PACKET_LOSS: Self = Self(5);
    pub const RECOVERABLE_PACKET_LOSS: Self = Self(6);
    pub const ENABLE_ADAPTOR: Self = Self(7);
    pub const QUERY_ENCODER: Self = Self(8);
    pub const CREATE_ENCODER: Self = Self(9);
    pub const ENCODE_ADAPTATION: Self = Self(10);
    pub const OVERHEAD: Self = Self(11);
    pub const RTT: Self = Self(12);
    pub const RECEIVER_FRAME_RANGE: Self = Self(13);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioEncoderSdkFailure {
    pub operation: AudioEncoderSdkOperation,
    pub error: AudioEncoderError,
}
/// First rejected SDK control. This owns only a shared atomic failure record,
/// not an encoder/backend/environment or a mutable SDK object.
pub struct AudioEncoderSdkFailureObserver {
    native: cxx::UniquePtr<ffi::NativeAudioEncoderFailureObserver>,
}
// SAFETY: native reads/clones share only an atomic first-failure record.
unsafe impl Send for AudioEncoderSdkFailureObserver {}
unsafe impl Sync for AudioEncoderSdkFailureObserver {}
impl AudioEncoderSdkFailureObserver {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioEncoderFailureObserver>,
    ) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub fn failure(&self) -> Option<AudioEncoderSdkFailure> {
        let code = ffi::audio_encoder_sdk_failure(self.native.as_ref().unwrap());
        if code == 0 {
            return None;
        }
        Some(AudioEncoderSdkFailure {
            operation: AudioEncoderSdkOperation((code >> 8) as u8),
            error: match code & 0xff {
                1 => AudioEncoderError::InvalidControl,
                2 => AudioEncoderError::UnsafeClock,
                3 => AudioEncoderError::UnsupportedControl,
                4 => AudioEncoderError::UnsafeAdaptor,
                _ => AudioEncoderError::NativeInvariant,
            },
        })
    }
}
impl Clone for AudioEncoderSdkFailureObserver {
    fn clone(&self) -> Self {
        Self::from_native(ffi::audio_encoder_sdk_failure_clone(
            self.native.as_ref().unwrap(),
        ))
    }
}
