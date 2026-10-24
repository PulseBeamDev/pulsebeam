//! Owning SDK decoder values, distinct from the legacy provider projection.
use crate::ffi;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoDecoderSettingsFields {
    pub buffer_pool_size: Option<i32>,
    pub max_render_width: i32,
    pub max_render_height: i32,
    pub number_of_cores: i32,
    /// Native enum value, including unnamed values within the C++ enum range.
    pub codec_type: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoDecoderSettingsValueError {
    /// A reached SDK DCHECK rejects non-positive cores in this artifact.
    CoreAssertion,
    /// The cast would be outside the non-fixed C++ enum's value range.
    CodecTypeNotRepresentable,
}
impl fmt::Display for VideoDecoderSettingsValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for VideoDecoderSettingsValueError {}

pub struct OwnedVideoDecoderSettings {
    native: cxx::UniquePtr<ffi::NativeVideoDecoderSettings>,
}
// SAFETY: the SDK settings own only integer, enum, optional and resolution
// values. Reads/copies use shared access; assignment/setters require exclusivity.
unsafe impl Send for OwnedVideoDecoderSettings {}
unsafe impl Sync for OwnedVideoDecoderSettings {}
impl Default for OwnedVideoDecoderSettings {
    fn default() -> Self {
        Self {
            native: ffi::video_decoder_settings_new(),
        }
    }
}
impl Clone for OwnedVideoDecoderSettings {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_decoder_settings_clone(self.native()),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::video_decoder_settings_assign(self.native.pin_mut(), source.native());
    }
}
impl fmt::Debug for OwnedVideoDecoderSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedVideoDecoderSettings {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeVideoDecoderSettings>,
    ) -> Option<Self> {
        (!native.is_null()).then_some(Self { native })
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoDecoderSettings {
        self.native.as_ref().unwrap()
    }
    pub fn fields(&self) -> VideoDecoderSettingsFields {
        let value = ffi::video_decoder_settings_fields(self.native());
        VideoDecoderSettingsFields {
            buffer_pool_size: value.has_buffer_pool_size.then_some(value.buffer_pool_size),
            max_render_width: value.width,
            max_render_height: value.height,
            number_of_cores: value.cores,
            codec_type: value.codec_type,
        }
    }
    /// Atomically invoke actual SDK setters. No resolution or pool-size policy
    /// is imposed on independent metadata. Product Configure admission is separate.
    pub fn set_fields(
        &mut self,
        value: VideoDecoderSettingsFields,
    ) -> Result<(), VideoDecoderSettingsValueError> {
        let result = ffi::video_decoder_settings_set_fields(
            self.native.pin_mut(),
            ffi::FfiOwnedVideoDecoderSettings {
                has_buffer_pool_size: value.buffer_pool_size.is_some(),
                buffer_pool_size: value.buffer_pool_size.unwrap_or(0),
                width: value.max_render_width,
                height: value.max_render_height,
                cores: value.number_of_cores,
                codec_type: value.codec_type,
            },
        );
        match result {
            0 => Ok(()),
            1 => Err(VideoDecoderSettingsValueError::CoreAssertion),
            2 => Err(VideoDecoderSettingsValueError::CodecTypeNotRepresentable),
            _ => unreachable!("closed native settings admission status"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoDecoderInfoFields {
    pub implementation_name: Vec<u8>,
    pub is_hardware_accelerated: bool,
}
pub struct OwnedVideoDecoderInfo {
    native: cxx::UniquePtr<ffi::NativeVideoDecoderInfo>,
}
// SAFETY: SDK info owns its std::string and bool, with no callback/engine graph.
// Shared access only reads/copies; mutation requires exclusive access.
unsafe impl Send for OwnedVideoDecoderInfo {}
unsafe impl Sync for OwnedVideoDecoderInfo {}
impl Default for OwnedVideoDecoderInfo {
    fn default() -> Self {
        Self {
            native: ffi::video_decoder_info_new(),
        }
    }
}
impl Clone for OwnedVideoDecoderInfo {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_decoder_info_clone(self.native()),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::video_decoder_info_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for OwnedVideoDecoderInfo {
    fn eq(&self, other: &Self) -> bool {
        ffi::video_decoder_info_equal(self.native(), other.native())
    }
}
impl Eq for OwnedVideoDecoderInfo {}
impl fmt::Debug for OwnedVideoDecoderInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedVideoDecoderInfo {
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeVideoDecoderInfo> {
        self.native
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeVideoDecoderInfo>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoDecoderInfo {
        self.native.as_ref().unwrap()
    }
    pub fn fields(&self) -> VideoDecoderInfoFields {
        let value = ffi::video_decoder_info_fields(self.native());
        VideoDecoderInfoFields {
            implementation_name: value.implementation_name,
            is_hardware_accelerated: value.hardware_accelerated,
        }
    }
    pub fn set_fields(&mut self, value: VideoDecoderInfoFields) {
        ffi::video_decoder_info_set_fields(
            self.native.pin_mut(),
            ffi::FfiOwnedVideoDecoderInfo {
                implementation_name: value.implementation_name,
                hardware_accelerated: value.is_hardware_accelerated,
            },
        );
    }
    /// Actual SDK ToString output, without assuming the native name is UTF-8.
    pub fn to_string_bytes(&self) -> Vec<u8> {
        ffi::video_decoder_info_string(self.native())
    }
}
