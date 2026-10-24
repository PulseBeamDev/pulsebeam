//! Owning current SDK AudioEncoderFactory::Options values.
use crate::{AudioCodecPairId, AudioEncoderOptions, ffi};
use cxx::UniquePtr;

/// Independent native SDK options value. Payload types retain their full signed
/// field domain, including the SDK's -1 default. Pair IDs are copied as actual
/// SDK identities, never regenerated from their numeric projection.
pub struct OwnedAudioEncoderOptions {
    native: UniquePtr<ffi::NativeAudioEncoderOptions>,
}
// Plain SDK value fields, no callbacks, sequences or mutable aliases. Mutators
// require an exclusive Rust borrow; copies own independent native values.
unsafe impl Send for OwnedAudioEncoderOptions {}
unsafe impl Sync for OwnedAudioEncoderOptions {}
impl Default for OwnedAudioEncoderOptions {
    fn default() -> Self {
        Self::from_native(ffi::audio_encoder_options_default())
    }
}
impl Clone for OwnedAudioEncoderOptions {
    fn clone(&self) -> Self {
        Self::from_native(ffi::audio_encoder_options_copy(
            self.native.as_ref().unwrap(),
        ))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::audio_encoder_options_assign(self.native.pin_mut(), source.native.as_ref().unwrap());
    }
}
impl OwnedAudioEncoderOptions {
    pub(crate) fn from_native(native: UniquePtr<ffi::NativeAudioEncoderOptions>) -> Self {
        assert!(!native.is_null(), "owned native audio encoder options");
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeAudioEncoderOptions {
        self.native.as_ref().expect("owned audio encoder options")
    }
    pub fn payload_type(&self) -> i32 {
        ffi::audio_encoder_options_payload_type(self.native.as_ref().unwrap())
    }
    pub fn set_payload_type(&mut self, value: i32) {
        ffi::audio_encoder_options_set_payload_type(self.native.pin_mut(), value);
    }
    pub fn codec_pair_id(&self) -> Option<AudioCodecPairId> {
        let value = ffi::audio_encoder_options_pair_id(self.native.as_ref().unwrap());
        if value.is_null() {
            None
        } else {
            Some(AudioCodecPairId::from_native(value))
        }
    }
    pub fn set_codec_pair_id(&mut self, value: Option<&AudioCodecPairId>) {
        let absent = UniquePtr::null();
        ffi::audio_encoder_options_set_pair_id(
            self.native.pin_mut(),
            value.map_or(&absent, AudioCodecPairId::native),
        );
    }
    pub fn values(&self) -> AudioEncoderOptions {
        AudioEncoderOptions {
            payload_type: self.payload_type(),
            codec_pair_id: self.codec_pair_id(),
        }
    }
    pub fn from_values(value: &AudioEncoderOptions) -> Self {
        let mut result = Self::default();
        result.set_payload_type(value.payload_type);
        result.set_codec_pair_id(value.codec_pair_id.as_ref());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_sdk_options_default_copy_assign_clear_and_signed_payloads() {
        let mut value = OwnedAudioEncoderOptions::default();
        assert_eq!(value.payload_type(), -1);
        assert!(value.codec_pair_id().is_none());
        let id = AudioCodecPairId::create();
        value.set_codec_pair_id(Some(&id));
        for payload in [i32::MIN, -1, 0, 127, 128, i32::MAX] {
            value.set_payload_type(payload);
            let copied = value.clone();
            assert_eq!(copied.payload_type(), payload);
            assert_eq!(copied.codec_pair_id().unwrap(), id);
        }
        let mut assigned = OwnedAudioEncoderOptions::default();
        assigned.clone_from(&value);
        value.set_codec_pair_id(None);
        value.set_payload_type(-1);
        assert_eq!(assigned.codec_pair_id().unwrap(), id);
        assert_eq!(assigned.payload_type(), i32::MAX);
        assert!(value.codec_pair_id().is_none());
        let numeric = id.numeric_representation();
        drop(value);
        drop(id);
        std::thread::spawn(move || {
            assert_eq!(
                assigned.codec_pair_id().unwrap().numeric_representation(),
                numeric
            );
            assert_eq!(
                OwnedAudioEncoderOptions::from_values(&assigned.values()).payload_type(),
                i32::MAX
            );
        })
        .join()
        .unwrap();
    }
}
