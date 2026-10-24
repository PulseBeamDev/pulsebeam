use crate::{NativeLongOutOfRange, ffi};

/// Owns the actual native dynamic StringBuilder. Text is a convenience input;
/// byte views preserve the native string_view/char domains without UTF-8 loss.
pub struct StringBuilder {
    native: cxx::UniquePtr<ffi::NativeStringBuilder>,
}
// SAFETY: the SDK class owns only a std::string. Shared methods read/copy it;
// mutation requires &mut self and borrowed bytes remain tied to that loan.
// There are no retained callbacks, external owners or thread-affine members.
unsafe impl Send for StringBuilder {}
unsafe impl Sync for StringBuilder {}
impl Default for StringBuilder {
    fn default() -> Self {
        Self::from_native(ffi::string_builder_default())
    }
}
impl Clone for StringBuilder {
    fn clone(&self) -> Self {
        Self::from_native(ffi::string_builder_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::string_builder_assign(self.native.pin_mut(), source.native());
    }
}
impl StringBuilder {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeStringBuilder>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeStringBuilder {
        self.native.as_ref().unwrap()
    }
    pub(crate) fn native_mut(&mut self) -> std::pin::Pin<&mut ffi::NativeStringBuilder> {
        self.native.pin_mut()
    }
    pub fn new(text: impl AsRef<[u8]>) -> Self {
        Self::from_native(ffi::string_builder_new(text.as_ref()))
    }
    pub fn as_bytes(&self) -> &[u8] {
        ffi::string_builder_bytes(self.native())
    }
    pub fn len(&self) -> usize {
        ffi::string_builder_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&mut self) {
        ffi::string_builder_clear(self.native.pin_mut());
    }
    /// Delegates Release without an invented reset of moved-from native storage.
    pub fn release(&mut self) -> Vec<u8> {
        ffi::string_builder_release(self.native.pin_mut())
    }
    pub fn append_bytes(&mut self, value: &[u8]) {
        ffi::string_builder_append_bytes(self.native.pin_mut(), value);
    }
    pub fn append_text(&mut self, value: &str) {
        self.append_bytes(value.as_bytes());
    }
    pub fn append_byte(&mut self, value: u8) {
        ffi::string_builder_append_byte(self.native.pin_mut(), value);
    }
    pub fn append_i32(&mut self, value: i32) {
        ffi::string_builder_append_i32(self.native.pin_mut(), value);
    }
    pub fn append_u32(&mut self, value: u32) {
        ffi::string_builder_append_u32(self.native.pin_mut(), value);
    }
    pub fn append_i64(&mut self, value: i64) {
        ffi::string_builder_append_i64(self.native.pin_mut(), value);
    }
    pub fn append_u64(&mut self, value: u64) {
        ffi::string_builder_append_u64(self.native.pin_mut(), value);
    }
    pub fn append_native_long(&mut self, value: i64) -> Result<(), NativeLongOutOfRange> {
        ffi::string_builder_append_long(self.native.pin_mut(), value)
            .then_some(())
            .ok_or(NativeLongOutOfRange)
    }
    pub fn append_native_unsigned_long(&mut self, value: u64) -> Result<(), NativeLongOutOfRange> {
        ffi::string_builder_append_unsigned_long(self.native.pin_mut(), value)
            .then_some(())
            .ok_or(NativeLongOutOfRange)
    }
    pub fn append_f32(&mut self, value: f32) {
        ffi::string_builder_append_f32(self.native.pin_mut(), value);
    }
    pub fn append_f64(&mut self, value: f64) {
        ffi::string_builder_append_f64(self.native.pin_mut(), value);
    }
    /// Native template insertion; false leaves output unchanged when MIME conversion is unsafe.
    pub fn append_codec(&mut self, value: &crate::RtpCodec) -> bool {
        ffi::string_builder_append_codec(self.native.pin_mut(), value.native())
    }
    /// Native template insertion; false leaves output unchanged when MIME conversion is unsafe.
    pub fn append_encoding(&mut self, value: &crate::RtpEncodingParameters) -> bool {
        ffi::string_builder_append_encoding(self.native.pin_mut(), value.native())
    }

    /// SDK insertion; false leaves output unchanged for unsafe nested formatting.
    pub fn append_rtp_transceiver_direction(&mut self, value: crate::RtpTransceiverDirectionValue) {
        ffi::string_builder_append_rtp_direction(self.native.pin_mut(), value.raw());
    }
    pub fn append_rtp_codec_capability(&mut self, value: &crate::RtpCodecCapability) {
        ffi::string_builder_append_rtp_codec_capability(self.native.pin_mut(), value.native());
    }
    pub fn append_rtp_source(&mut self, value: &crate::RtpSource) {
        ffi::string_builder_append_rtp_source(self.native.pin_mut(), value.native());
    }
    pub fn append_sdp_video_format(&mut self, value: &crate::SdpVideoFormat) -> bool {
        ffi::string_builder_append_sdp_video_format(self.native.pin_mut(), value.native())
    }
    pub fn append_rtp_parameters(&mut self, value: &crate::RtpParameters) -> bool {
        ffi::string_builder_append_rtp_parameters(self.native.pin_mut(), value.native())
    }
    pub fn append_codec_parameters(&mut self, value: &crate::RtpCodecParameters) -> bool {
        ffi::string_builder_append_codec_parameters(self.native.pin_mut(), value.native())
    }
    pub fn append_header_extension_id(&mut self, value: &crate::RtpHeaderExtensionId) {
        ffi::string_builder_append_header_extension_id(self.native.pin_mut(), value.native());
    }
    pub fn append_rtp_extension(&mut self, value: &crate::RtpExtension) {
        ffi::string_builder_append_rtp_extension(self.native.pin_mut(), value.native());
    }
    pub fn append_rtp_extension_ref(&mut self, value: &crate::RtpExtensionRef<'_>) {
        ffi::string_builder_append_rtp_extension_ref(self.native.pin_mut(), value.native());
    }
    pub fn append_rtcp_parameters(&mut self, value: &crate::RtcpParameters) {
        ffi::string_builder_append_rtcp_parameters(self.native.pin_mut(), value.native());
    }
    pub fn append_media_type(&mut self, kind: crate::MediaType) -> bool {
        ffi::string_builder_append_media_type(self.native.pin_mut(), kind.to_native())
    }
    pub fn append_feedback_type(&mut self, kind: crate::RtcpFeedbackType) {
        ffi::string_builder_append_feedback_type(self.native.pin_mut(), kind.to_native());
    }
    pub fn append_optional_feedback_type(&mut self, kind: Option<crate::RtcpFeedbackType>) {
        ffi::string_builder_append_optional_feedback_type(
            self.native.pin_mut(),
            kind.is_some(),
            kind.map(crate::RtcpFeedbackType::to_native).unwrap_or(0),
        );
    }
}
impl std::fmt::Debug for StringBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StringBuilder")
            .field("bytes", &self.as_bytes())
            .finish()
    }
}
