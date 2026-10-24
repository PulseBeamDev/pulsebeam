//! Complete owning SDK transceiver initialization metadata.
use crate::{RtpEncodingParameters, RtpTransceiverDirectionValue, ffi};

/// An actual SDK RtpTransceiverInit value with all three public fields.
///
/// Child snapshots are independent copies. Arbitrary stream-ID bytes, raw
/// directions and encoding metadata are retained here; constructing this value
/// does not install it, validate it, or configure a sender. Install it through
/// `PeerConnection::add_transceiver` (trackless) or the audio/video
/// `add_*_transceiver_with_init` methods. Consumers preserve native validation
/// and reject values that would trigger unsafe native arithmetic or assertions.
/// The SDK declares no equality or formatting operation for this struct.
pub struct RtpTransceiverInit {
    native: cxx::UniquePtr<ffi::NativeRtpTransceiverInit>,
}
// SAFETY: The native value owns only an enum, byte strings and full encoding
// values. No retained origin, handles, callbacks, lazy state or thread-affine
// destruction. Shared methods only read/copy; mutations require &mut self.
unsafe impl Send for RtpTransceiverInit {}
unsafe impl Sync for RtpTransceiverInit {}
impl Default for RtpTransceiverInit {
    fn default() -> Self {
        Self::from_native(ffi::rtp_transceiver_init_default())
    }
}
impl Clone for RtpTransceiverInit {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_transceiver_init_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_transceiver_init_assign(self.native.pin_mut(), source.native());
    }
}
impl RtpTransceiverInit {
    fn from_native(native: cxx::UniquePtr<ffi::NativeRtpTransceiverInit>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpTransceiverInit {
        self.native.as_ref().unwrap()
    }
    pub fn direction(&self) -> RtpTransceiverDirectionValue {
        RtpTransceiverDirectionValue::from_raw(ffi::rtp_transceiver_init_direction(self.native()))
    }
    pub fn set_direction(&mut self, direction: RtpTransceiverDirectionValue) {
        ffi::rtp_transceiver_init_set_direction(self.native.pin_mut(), direction.raw());
    }
    pub fn stream_ids(&self) -> Vec<Vec<u8>> {
        (0..ffi::rtp_transceiver_init_stream_ids_len(self.native()))
            .map(|index| ffi::rtp_transceiver_init_stream_id_get(self.native(), index))
            .collect()
    }
    /// Replace all IDs, preserving empty strings, arbitrary bytes, duplicates
    /// and order. Strings are not narrowed through UTF-8 or C-string decoding.
    pub fn set_stream_ids(&mut self, ids: &[Vec<u8>]) {
        ffi::rtp_transceiver_init_stream_ids_clear(self.native.pin_mut());
        for id in ids {
            ffi::rtp_transceiver_init_stream_id_add(self.native.pin_mut(), id);
        }
    }
    pub fn send_encodings(&self) -> Vec<RtpEncodingParameters> {
        (0..ffi::rtp_transceiver_init_encodings_len(self.native()))
            .map(|index| {
                RtpEncodingParameters::from_native(ffi::rtp_transceiver_init_encoding_get(
                    self.native(),
                    index,
                ))
            })
            .collect()
    }
    /// Replace full encoding values without installing, normalizing, sorting
    /// or rejecting their standalone metadata domains.
    pub fn set_send_encodings(&mut self, encodings: &[RtpEncodingParameters]) {
        ffi::rtp_transceiver_init_encodings_clear(self.native.pin_mut());
        for encoding in encodings {
            ffi::rtp_transceiver_init_encoding_add(self.native.pin_mut(), encoding.native());
        }
    }
}
impl std::fmt::Debug for RtpTransceiverInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rust metadata diagnostics only, not a claimed native stringify contract.
        f.debug_struct("RtpTransceiverInit")
            .field("direction", &self.direction())
            .field("stream_ids", &self.stream_ids())
            .field("send_encodings", &self.send_encodings())
            .finish()
    }
}
