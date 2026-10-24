use crate::ffi;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcErrorType {
    None,
    UnsupportedOperation,
    UnsupportedParameter,
    InvalidParameter,
    InvalidRange,
    SyntaxError,
    InvalidState,
    InvalidModification,
    NetworkError,
    ResourceExhausted,
    InternalError,
    OperationErrorWithData,
    Other(i32),
}
impl RtcErrorType {
    pub fn native_name(self) -> Option<String> {
        let mut valid = false;
        let text = ffi::rtc_error_type_name(self.to_native(), &mut valid);
        valid.then_some(text)
    }
    pub(crate) fn to_native(self) -> i32 {
        match self {
            Self::None => 0,
            Self::UnsupportedOperation => 1,
            Self::UnsupportedParameter => 2,
            Self::InvalidParameter => 3,
            Self::InvalidRange => 4,
            Self::SyntaxError => 5,
            Self::InvalidState => 6,
            Self::InvalidModification => 7,
            Self::NetworkError => 8,
            Self::ResourceExhausted => 9,
            Self::InternalError => 10,
            Self::OperationErrorWithData => 11,
            Self::Other(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::UnsupportedOperation,
            2 => Self::UnsupportedParameter,
            3 => Self::InvalidParameter,
            4 => Self::InvalidRange,
            5 => Self::SyntaxError,
            6 => Self::InvalidState,
            7 => Self::InvalidModification,
            8 => Self::NetworkError,
            9 => Self::ResourceExhausted,
            10 => Self::InternalError,
            11 => Self::OperationErrorWithData,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcErrorDetailType {
    None,
    DataChannelFailure,
    DtlsFailure,
    FingerprintFailure,
    SctpFailure,
    SdpSyntaxError,
    HardwareEncoderNotAvailable,
    HardwareEncoderError,
    Other(i32),
}
impl RtcErrorDetailType {
    pub fn native_name(self) -> Option<String> {
        let mut valid = false;
        let text = ffi::rtc_error_detail_name(self.to_native(), &mut valid);
        valid.then_some(text)
    }
    fn to_native(self) -> i32 {
        match self {
            Self::None => 0,
            Self::DataChannelFailure => 1,
            Self::DtlsFailure => 2,
            Self::FingerprintFailure => 3,
            Self::SctpFailure => 4,
            Self::SdpSyntaxError => 5,
            Self::HardwareEncoderNotAvailable => 6,
            Self::HardwareEncoderError => 7,
            Self::Other(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::DataChannelFailure,
            2 => Self::DtlsFailure,
            3 => Self::FingerprintFailure,
            4 => Self::SctpFailure,
            5 => Self::SdpSyntaxError,
            6 => Self::HardwareEncoderNotAvailable,
            7 => Self::HardwareEncoderError,
            value => Self::Other(value),
        }
    }
}

/// Owns the actual native RTCError, including its mutable StringBuilder. Unknown
/// discriminants remain observable; only native's unchecked name conversion is
/// guarded. This is not a cached error DTO or a flattened PeerError category.
pub struct RtcError {
    native: cxx::UniquePtr<ffi::NativeRtcError>,
}
// SAFETY: pinned RTCError owns enum/optional scalar fields and a StringBuilder
// containing an owned std::string. Shared calls only read/copy that storage;
// mutation and borrowed builder access require exclusive &mut self. Byte views
// remain tied to that exclusive loan. No native callback/independent owner can
// retain or mutate this native value, and size()'s const-cast only reads storage.
unsafe impl Send for RtcError {}
unsafe impl Sync for RtcError {}
impl Default for RtcError {
    fn default() -> Self {
        Self::from_native(ffi::rtc_error_default())
    }
}
impl Clone for RtcError {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtc_error_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtc_error_assign(self.native.pin_mut(), source.native());
    }
}
impl RtcError {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtcError>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    fn native(&self) -> &ffi::NativeRtcError {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiRtcError {
        ffi::rtc_error_fields(self.native())
    }
    pub fn new(kind: RtcErrorType) -> Self {
        Self::from_native(ffi::rtc_error_for_type(kind.to_native()))
    }
    /// Native string_view accepts arbitrary bytes; text inputs work directly.
    pub fn with_message(kind: RtcErrorType, message: impl AsRef<[u8]>) -> Self {
        Self::from_native(ffi::rtc_error_new(kind.to_native(), message.as_ref()))
    }
    fn factory(kind: RtcErrorType, message: impl AsRef<[u8]>) -> Self {
        Self::from_native(ffi::rtc_error_factory(kind.to_native(), message.as_ref()))
    }
    /// Calls native RTCError::OK().
    pub fn success() -> Self {
        Self::factory(RtcErrorType::None, "")
    }
    pub fn invalid_parameter(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::InvalidParameter, message)
    }
    pub fn invalid_state(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::InvalidState, message)
    }
    pub fn invalid_modification(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::InvalidModification, message)
    }
    pub fn unsupported_operation(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::UnsupportedOperation, message)
    }
    pub fn unsupported_parameter(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::UnsupportedParameter, message)
    }
    pub fn invalid_range(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::InvalidRange, message)
    }
    pub fn syntax_error(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::SyntaxError, message)
    }
    pub fn network_error(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::NetworkError, message)
    }
    pub fn resource_exhausted(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::ResourceExhausted, message)
    }
    pub fn internal_error(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::InternalError, message)
    }
    pub fn operation_error_with_data(message: impl AsRef<[u8]>) -> Self {
        Self::factory(RtcErrorType::OperationErrorWithData, message)
    }
    pub fn kind(&self) -> RtcErrorType {
        RtcErrorType::from_native(self.fields().kind)
    }
    /// Lossless native const-char* accessor bytes, including termination at NUL.
    /// Use [`Self::string_builder`] for full native storage across embedded NULs.
    pub fn message_bytes(&self) -> Vec<u8> {
        self.fields().message_bytes
    }
    /// UTF-8-lossy text convenience over message_bytes(). Native char insertion
    /// can produce non-UTF-8 bytes; use the byte getter to preserve them exactly.
    pub fn message(&self) -> String {
        String::from_utf8_lossy(&self.message_bytes()).into_owned()
    }
    pub fn string_builder(&mut self) -> RtcErrorMessageBuilder<'_> {
        RtcErrorMessageBuilder { error: self }
    }
    pub fn error_detail(&self) -> RtcErrorDetailType {
        RtcErrorDetailType::from_native(self.fields().detail)
    }
    pub fn sctp_cause_code(&self) -> Option<u16> {
        let value = self.fields();
        value.has_sctp_cause_code.then_some(value.sctp_cause_code)
    }
    pub fn is_ok(&self) -> bool {
        ffi::rtc_error_is_ok(self.native())
    }
    pub fn set_kind(&mut self, kind: RtcErrorType) {
        ffi::rtc_error_set_type(self.native.pin_mut(), kind.to_native());
    }
    pub fn set_message(&mut self, message: impl AsRef<[u8]>) {
        ffi::rtc_error_set_message(self.native.pin_mut(), message.as_ref());
    }
    pub fn set_error_detail(&mut self, detail: RtcErrorDetailType) {
        ffi::rtc_error_set_detail(self.native.pin_mut(), detail.to_native());
    }
    pub fn set_sctp_cause_code(&mut self, cause: u16) {
        ffi::rtc_error_set_sctp_cause(self.native.pin_mut(), cause);
    }
    /// Appends through the native StringBuilder's string insertion operation.
    pub fn append_text(&mut self, text: &str) {
        ffi::rtc_error_append_text(self.native.pin_mut(), text);
    }
    /// Consumes this owner through the native rvalue streaming operator and move
    /// constructor. No callback-scoped builder pointer is returned to Rust.
    pub fn with_appended_text(self, text: &str) -> Self {
        self.with_appended_bytes(text.as_bytes())
    }
    pub fn with_appended_bytes(self, bytes: &[u8]) -> Self {
        Self::from_native(ffi::rtc_error_with_appended_bytes(self.native, bytes))
    }
    /// Actual SDK rvalue streaming, preserving the error's other fields.
    pub fn with_appended_byte(self, value: u8) -> Self {
        Self::from_native(ffi::rtc_error_stream_byte(self.native, value))
    }
    pub fn with_appended_i32(self, value: i32) -> Self {
        Self::from_native(ffi::rtc_error_stream_i32(self.native, value))
    }
    pub fn with_appended_u32(self, value: u32) -> Self {
        Self::from_native(ffi::rtc_error_stream_u32(self.native, value))
    }
    pub fn with_appended_i64(self, value: i64) -> Self {
        Self::from_native(ffi::rtc_error_stream_i64(self.native, value))
    }
    pub fn with_appended_u64(self, value: u64) -> Self {
        Self::from_native(ffi::rtc_error_stream_u64(self.native, value))
    }
    pub fn with_appended_f32(self, value: f32) -> Self {
        Self::from_native(ffi::rtc_error_stream_f32(self.native, value))
    }
    pub fn with_appended_f64(self, value: f64) -> Self {
        Self::from_native(ffi::rtc_error_stream_f64(self.native, value))
    }
    pub fn with_appended_bool(self, value: bool) -> Self {
        Self::from_native(ffi::rtc_error_stream_bool(self.native, value))
    }
    /// On a platform-width conversion failure the unchanged owner is returned.
    pub fn with_appended_native_long(
        self,
        value: i64,
    ) -> Result<Self, (Self, NativeLongOutOfRange)> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_long(self.native, value, &mut valid));
        if valid {
            Ok(result)
        } else {
            Err((result, NativeLongOutOfRange))
        }
    }
    pub fn with_appended_native_unsigned_long(
        self,
        value: u64,
    ) -> Result<Self, (Self, NativeLongOutOfRange)> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_unsigned_long(
            self.native,
            value,
            &mut valid,
        ));
        if valid {
            Ok(result)
        } else {
            Err((result, NativeLongOutOfRange))
        }
    }
    /// Actual SDK rvalue streaming. Unsafe nested MIME conversion returns this unchanged owner.
    pub fn with_appended_codec(self, value: &crate::RtpCodec) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_codec(
            self.native,
            value.native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }
    /// Actual SDK rvalue streaming. Unsafe nested MIME conversion returns this unchanged owner.
    pub fn with_appended_encoding(
        self,
        value: &crate::RtpEncodingParameters,
    ) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_encoding(
            self.native,
            value.native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }

    /// SDK rvalue insertion; formatting rejection returns this unchanged owner.
    pub fn with_appended_rtp_transceiver_direction(
        self,
        value: crate::RtpTransceiverDirectionValue,
    ) -> Self {
        Self::from_native(ffi::rtc_error_stream_rtp_direction(
            self.native,
            value.raw(),
        ))
    }
    pub fn with_appended_rtp_codec_capability(self, value: &crate::RtpCodecCapability) -> Self {
        Self::from_native(ffi::rtc_error_stream_rtp_codec_capability(
            self.native,
            value.native(),
        ))
    }
    pub fn with_appended_rtp_source(self, value: &crate::RtpSource) -> Self {
        Self::from_native(ffi::rtc_error_stream_rtp_source(
            self.native,
            value.native(),
        ))
    }
    pub fn with_appended_sdp_video_format(
        self,
        value: &crate::SdpVideoFormat,
    ) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_sdp_video_format(
            self.native,
            value.native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }
    pub fn with_appended_rtp_parameters(self, value: &crate::RtpParameters) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_rtp_parameters(
            self.native,
            value.native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }
    pub fn with_appended_codec_parameters(
        self,
        value: &crate::RtpCodecParameters,
    ) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_codec_parameters(
            self.native,
            value.native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }
    pub fn with_appended_header_extension_id(self, value: &crate::RtpHeaderExtensionId) -> Self {
        Self::from_native(ffi::rtc_error_stream_header_extension_id(
            self.native,
            value.native(),
        ))
    }
    pub fn with_appended_rtp_extension(self, value: &crate::RtpExtension) -> Self {
        Self::from_native(ffi::rtc_error_stream_rtp_extension(
            self.native,
            value.native(),
        ))
    }
    pub fn with_appended_rtp_extension_ref(self, value: &crate::RtpExtensionRef<'_>) -> Self {
        Self::from_native(ffi::rtc_error_stream_rtp_extension_ref(
            self.native,
            value.native(),
        ))
    }
    pub fn with_appended_rtcp_parameters(self, value: &crate::RtcpParameters) -> Self {
        let mut valid = false;
        Self::from_native(ffi::rtc_error_stream_rtcp_parameters(
            self.native,
            value.native(),
            &mut valid,
        ))
    }
    pub fn with_appended_media_type(self, kind: crate::MediaType) -> Result<Self, Self> {
        let mut valid = false;
        let result = Self::from_native(ffi::rtc_error_stream_media_type(
            self.native,
            kind.to_native(),
            &mut valid,
        ));
        if valid { Ok(result) } else { Err(result) }
    }
    pub fn with_appended_feedback_type(self, kind: crate::RtcpFeedbackType) -> Self {
        let mut valid = false;
        Self::from_native(ffi::rtc_error_stream_feedback_type(
            self.native,
            kind.to_native(),
            &mut valid,
        ))
    }
    pub fn with_appended_optional_feedback_type(
        self,
        kind: Option<crate::RtcpFeedbackType>,
    ) -> Self {
        let mut valid = false;
        Self::from_native(ffi::rtc_error_stream_optional_feedback_type(
            self.native,
            kind.is_some(),
            kind.map(crate::RtcpFeedbackType::to_native).unwrap_or(0),
            &mut valid,
        ))
    }
    pub fn move_assign(&mut self, source: Self) {
        ffi::rtc_error_move_assign(self.native.pin_mut(), source.native);
    }
    /// Native AbslStringify; None means an unknown type would index native's
    /// name array out of bounds. The object's kind/message remain unchanged.
    pub fn native_string(&self) -> Option<String> {
        self.native_string_bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
    pub fn native_string_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtc_error_string(self.native(), &mut valid);
        valid.then_some(bytes)
    }
}
/// Exclusive, lifetime-bound facade over RTCError::string_builder(). The
/// borrowed byte view is invalidated only by mutation, which Rust's borrowing
/// rules disallow until the view is no longer used. No raw native pointer escapes.
pub struct RtcErrorMessageBuilder<'a> {
    error: &'a mut RtcError,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLongOutOfRange;
impl fmt::Display for NativeLongOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value is not representable by this platform's native long type")
    }
}
impl std::error::Error for NativeLongOutOfRange {}
impl RtcErrorMessageBuilder<'_> {
    pub fn as_bytes(&mut self) -> &[u8] {
        ffi::rtc_error_builder_bytes(self.error.native.pin_mut())
    }
    /// Actual StringBuilder copy construction, independent of this loan/owner.
    pub fn to_owned(&self) -> crate::StringBuilder {
        crate::StringBuilder::from_native(ffi::rtc_error_copy_message_builder(self.error.native()))
    }
    pub fn assign_from(&mut self, source: &crate::StringBuilder) {
        ffi::rtc_error_assign_message_builder(self.error.native.pin_mut(), source.native());
    }
    pub fn len(&self) -> usize {
        ffi::rtc_error_builder_size(self.error.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&mut self) {
        ffi::rtc_error_builder_clear(self.error.native.pin_mut());
    }
    /// Native Release, including native moved-from storage semantics. This does
    /// not synthesize a Clear/reset after moving out the message bytes.
    pub fn release(&mut self) -> Vec<u8> {
        ffi::rtc_error_builder_release(self.error.native.pin_mut())
    }
    pub fn append_text(&mut self, value: &str) {
        self.error.append_text(value);
    }
    pub fn append_bytes(&mut self, value: &[u8]) {
        ffi::rtc_error_append_bytes(self.error.native.pin_mut(), value);
    }
    /// Inserts the native one-byte char, not a UTF-8 encoding of a Unicode char.
    pub fn append_byte(&mut self, value: u8) {
        ffi::rtc_error_append_byte(self.error.native.pin_mut(), value);
    }
    pub fn append_i32(&mut self, value: i32) {
        ffi::rtc_error_append_i32(self.error.native.pin_mut(), value);
    }
    pub fn append_u32(&mut self, value: u32) {
        ffi::rtc_error_append_u32(self.error.native.pin_mut(), value);
    }
    pub fn append_i64(&mut self, value: i64) {
        ffi::rtc_error_append_i64(self.error.native.pin_mut(), value);
    }
    pub fn append_u64(&mut self, value: u64) {
        ffi::rtc_error_append_u64(self.error.native.pin_mut(), value);
    }
    /// Calls the actual native long overload. Only representability at the
    /// platform-dependent integer conversion is checked, not an invented cap.
    pub fn append_native_long(&mut self, value: i64) -> Result<(), NativeLongOutOfRange> {
        ffi::rtc_error_append_long(self.error.native.pin_mut(), value)
            .then_some(())
            .ok_or(NativeLongOutOfRange)
    }
    pub fn append_native_unsigned_long(&mut self, value: u64) -> Result<(), NativeLongOutOfRange> {
        ffi::rtc_error_append_unsigned_long(self.error.native.pin_mut(), value)
            .then_some(())
            .ok_or(NativeLongOutOfRange)
    }
    pub fn append_f32(&mut self, value: f32) {
        ffi::rtc_error_append_f32(self.error.native.pin_mut(), value);
    }
    pub fn append_f64(&mut self, value: f64) {
        ffi::rtc_error_append_f64(self.error.native.pin_mut(), value);
    }
    pub fn append_codec(&mut self, value: &crate::RtpCodec) -> bool {
        ffi::rtc_error_builder_append_codec(self.error.native.pin_mut(), value.native())
    }
    pub fn append_encoding(&mut self, value: &crate::RtpEncodingParameters) -> bool {
        ffi::rtc_error_builder_append_encoding(self.error.native.pin_mut(), value.native())
    }

    /// SDK insertion; false leaves the borrowed builder unchanged on rejection.
    pub fn append_rtp_transceiver_direction(&mut self, value: crate::RtpTransceiverDirectionValue) {
        ffi::rtc_error_builder_append_rtp_direction(self.error.native.pin_mut(), value.raw());
    }
    pub fn append_rtp_codec_capability(&mut self, value: &crate::RtpCodecCapability) {
        ffi::rtc_error_builder_append_rtp_codec_capability(
            self.error.native.pin_mut(),
            value.native(),
        );
    }
    pub fn append_rtp_source(&mut self, value: &crate::RtpSource) {
        ffi::rtc_error_builder_append_rtp_source(self.error.native.pin_mut(), value.native());
    }
    pub fn append_sdp_video_format(&mut self, value: &crate::SdpVideoFormat) -> bool {
        ffi::rtc_error_builder_append_sdp_video_format(self.error.native.pin_mut(), value.native())
    }
    pub fn append_rtp_parameters(&mut self, value: &crate::RtpParameters) -> bool {
        ffi::rtc_error_builder_append_rtp_parameters(self.error.native.pin_mut(), value.native())
    }
    pub fn append_codec_parameters(&mut self, value: &crate::RtpCodecParameters) -> bool {
        ffi::rtc_error_builder_append_codec_parameters(self.error.native.pin_mut(), value.native())
    }
    pub fn append_header_extension_id(&mut self, value: &crate::RtpHeaderExtensionId) {
        ffi::rtc_error_builder_append_header_extension_id(
            self.error.native.pin_mut(),
            value.native(),
        );
    }
    pub fn append_rtp_extension(&mut self, value: &crate::RtpExtension) {
        ffi::rtc_error_builder_append_rtp_extension(self.error.native.pin_mut(), value.native());
    }
    pub fn append_rtp_extension_ref(&mut self, value: &crate::RtpExtensionRef<'_>) {
        ffi::rtc_error_builder_append_rtp_extension_ref(
            self.error.native.pin_mut(),
            value.native(),
        );
    }
    pub fn append_rtcp_parameters(&mut self, value: &crate::RtcpParameters) {
        ffi::rtc_error_builder_append_rtcp_parameters(self.error.native.pin_mut(), value.native());
    }
    pub fn append_media_type(&mut self, kind: crate::MediaType) -> bool {
        ffi::rtc_error_builder_append_media_type(self.error.native.pin_mut(), kind.to_native())
    }
    pub fn append_feedback_type(&mut self, kind: crate::RtcpFeedbackType) {
        ffi::rtc_error_builder_append_feedback_type(self.error.native.pin_mut(), kind.to_native());
    }
    pub fn append_optional_feedback_type(&mut self, kind: Option<crate::RtcpFeedbackType>) {
        ffi::rtc_error_builder_append_optional_feedback_type(
            self.error.native.pin_mut(),
            kind.is_some(),
            kind.map(crate::RtcpFeedbackType::to_native).unwrap_or(0),
        );
    }
}
impl fmt::Debug for RtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtcError")
            .field("kind", &self.kind())
            .field("detail", &self.error_detail())
            .field("message", &self.message())
            .field("sctp_cause_code", &self.sctp_cause_code())
            .finish()
    }
}
impl fmt::Display for RtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for RtcError {}
