use crate::ffi;

/// Actual owned SDK Resolution, including its signed metadata domain.
/// Mutation does not impose encoder-installation constraints on a value.
pub struct Resolution {
    native: cxx::UniquePtr<ffi::NativeRtpResolution>,
}
// SAFETY: the concrete SDK aggregate contains only two owned integers. It has
// no external references, lazy state, callbacks or thread affinity. Shared
// operations read/copy; exclusive operations require &mut self.
unsafe impl Send for Resolution {}
unsafe impl Sync for Resolution {}
impl Default for Resolution {
    fn default() -> Self {
        Self::new(0, 0)
    }
}
impl Clone for Resolution {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_resolution_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_resolution_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for Resolution {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::rtp_resolution_equal(self.native(), rhs.native())
    }
}
impl Eq for Resolution {}
impl Resolution {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpResolution>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpResolution {
        self.native.as_ref().unwrap()
    }
    pub fn new(width: i32, height: i32) -> Self {
        Self::from_native(ffi::rtp_resolution_new(width, height))
    }
    pub fn width(&self) -> i32 {
        ffi::rtp_resolution_width(self.native())
    }
    pub fn height(&self) -> i32 {
        ffi::rtp_resolution_height(self.native())
    }
    pub fn set_width(&mut self, width: i32) {
        ffi::rtp_resolution_set_width(self.native.pin_mut(), width);
    }
    pub fn set_height(&mut self, height: i32) {
        ffi::rtp_resolution_set_height(self.native.pin_mut(), height);
    }
    /// Calls SDK PixelCount iff its signed-int multiplication is representable.
    /// None rejects only overflow, not harmless negative/zero dimensions.
    pub fn pixel_count(&self) -> Option<i32> {
        let mut valid = false;
        let result = ffi::rtp_resolution_pixel_count(self.native(), &mut valid);
        valid.then_some(result)
    }
    pub fn to_pair(&self) -> (i32, i32) {
        let pair = ffi::rtp_resolution_to_pair(self.native());
        (pair.first, pair.second)
    }
}
impl std::fmt::Debug for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolution")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

/// Open native enum domain. Unknown discriminants remain metadata values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcpFeedbackType {
    None,
    Ccm,
    Lntf,
    Nack,
    Remb,
    TransportCc,
    Ccfb,
    Other(i32),
}
impl RtcpFeedbackType {
    pub const MAX: Self = Self::Ccfb;
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Ccm,
            2 => Self::Lntf,
            3 => Self::Nack,
            4 => Self::Remb,
            5 => Self::TransportCc,
            6 => Self::Ccfb,
            value => Self::Other(value),
        }
    }
    pub(crate) fn to_native(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Ccm => 1,
            Self::Lntf => 2,
            Self::Nack => 3,
            Self::Remb => 4,
            Self::TransportCc => 5,
            Self::Ccfb => 6,
            Self::Other(value) => value,
        }
    }
    /// Actual SDK AbslStringify. Unknown values safely produce an empty string.
    pub fn native_string(self) -> String {
        ffi::rtcp_feedback_type_string(self.to_native())
    }
    pub fn optional_native_string(value: Option<Self>) -> String {
        ffi::rtcp_feedback_optional_type_string(
            value.is_some(),
            value.map(Self::to_native).unwrap_or(0),
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcpFeedbackMessageType {
    GenericNack,
    Pli,
    Fir,
    Other(i32),
}
impl RtcpFeedbackMessageType {
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            0 => Self::GenericNack,
            1 => Self::Pli,
            2 => Self::Fir,
            value => Self::Other(value),
        }
    }
    pub(crate) fn to_native(self) -> i32 {
        match self {
            Self::GenericNack => 0,
            Self::Pli => 1,
            Self::Fir => 2,
            Self::Other(value) => value,
        }
    }
}

/// Actual native RTCP feedback, not a derived Rust field comparison.
pub struct RtcpFeedback {
    native: cxx::UniquePtr<ffi::NativeRtcpFeedback>,
}
// SAFETY: SDK value owns only enums and an optional enum, without lazy state,
// callbacks, external references or thread affinity. Mutation is exclusive.
unsafe impl Send for RtcpFeedback {}
unsafe impl Sync for RtcpFeedback {}
impl Default for RtcpFeedback {
    fn default() -> Self {
        Self::from_native(ffi::rtcp_feedback_default())
    }
}
impl Clone for RtcpFeedback {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtcp_feedback_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtcp_feedback_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtcpFeedback {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::rtcp_feedback_equal(self.native(), rhs.native())
    }
}
impl Eq for RtcpFeedback {}
impl RtcpFeedback {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtcpFeedback>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtcpFeedback {
        self.native.as_ref().unwrap()
    }
    pub fn for_type(feedback_type: RtcpFeedbackType) -> Self {
        Self::from_native(ffi::rtcp_feedback_for_type(feedback_type.to_native()))
    }
    pub fn new(feedback_type: RtcpFeedbackType, message_type: RtcpFeedbackMessageType) -> Self {
        Self::from_native(ffi::rtcp_feedback_new(
            feedback_type.to_native(),
            message_type.to_native(),
        ))
    }
    pub fn feedback_type(&self) -> RtcpFeedbackType {
        RtcpFeedbackType::from_native(ffi::rtcp_feedback_fields(self.native()).type_code)
    }
    pub fn message_type(&self) -> Option<RtcpFeedbackMessageType> {
        let fields = ffi::rtcp_feedback_fields(self.native());
        fields
            .has_message
            .then(|| RtcpFeedbackMessageType::from_native(fields.message_code))
    }
    pub fn set_feedback_type(&mut self, value: RtcpFeedbackType) {
        ffi::rtcp_feedback_set_type(self.native.pin_mut(), value.to_native());
    }
    pub fn set_message_type(&mut self, value: Option<RtcpFeedbackMessageType>) {
        ffi::rtcp_feedback_set_message(
            self.native.pin_mut(),
            value.is_some(),
            value.map(RtcpFeedbackMessageType::to_native).unwrap_or(0),
        );
    }
}
impl std::fmt::Debug for RtcpFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcpFeedback")
            .field("feedback_type", &self.feedback_type())
            .field("message_type", &self.message_type())
            .finish()
    }
}

/// Lossless entry snapshot of the native std::map string/string field.
/// Setters use native map assignment; later duplicate keys replace earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpCodecParameter {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// Owns an actual SDK RtpCodec, including its native collections and helpers.
pub struct RtpCodec {
    native: cxx::UniquePtr<ffi::NativeRtpCodec>,
}
// SAFETY: concrete SDK base value owns strings, optional integers and collections
// of the pure RTCP values above. No references, callbacks, lazy state or thread
// affinity; shared methods read/copy and mutation requires &mut self.
unsafe impl Send for RtpCodec {}
unsafe impl Sync for RtpCodec {}
impl Default for RtpCodec {
    fn default() -> Self {
        Self::from_native(ffi::rtp_codec_default())
    }
}
impl Clone for RtpCodec {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_codec_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_codec_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpCodec {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::rtp_codec_equal(self.native(), rhs.native())
    }
}
impl Eq for RtpCodec {}
impl RtpCodec {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpCodec>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpCodec {
        self.native.as_ref().unwrap()
    }
    pub fn name_bytes(&self) -> Vec<u8> {
        ffi::rtp_codec_fields(self.native()).name_bytes
    }
    pub fn kind(&self) -> crate::MediaType {
        crate::MediaType::from_native(ffi::rtp_codec_fields(self.native()).kind)
    }
    pub fn clock_rate(&self) -> Option<i32> {
        let fields = ffi::rtp_codec_fields(self.native());
        fields.has_clock_rate.then_some(fields.clock_rate)
    }
    pub fn num_channels(&self) -> Option<i32> {
        let fields = ffi::rtp_codec_fields(self.native());
        fields.has_num_channels.then_some(fields.num_channels)
    }
    pub fn set_name(&mut self, name: impl AsRef<[u8]>) {
        ffi::rtp_codec_set_name(self.native.pin_mut(), name.as_ref());
    }
    pub fn set_kind(&mut self, kind: crate::MediaType) {
        ffi::rtp_codec_set_kind(self.native.pin_mut(), kind.to_native());
    }
    pub fn set_clock_rate(&mut self, rate: Option<i32>) {
        ffi::rtp_codec_set_clock_rate(self.native.pin_mut(), rate.is_some(), rate.unwrap_or(0));
    }
    pub fn set_num_channels(&mut self, count: Option<i32>) {
        ffi::rtp_codec_set_num_channels(self.native.pin_mut(), count.is_some(), count.unwrap_or(0));
    }
    pub fn rtcp_feedback(&self) -> Vec<RtcpFeedback> {
        (0..ffi::rtp_codec_feedback_len(self.native()))
            .map(|index| {
                RtcpFeedback::from_native(ffi::rtp_codec_feedback_get(self.native(), index))
            })
            .collect()
    }
    pub fn set_rtcp_feedback(&mut self, values: &[RtcpFeedback]) {
        ffi::rtp_codec_feedback_clear(self.native.pin_mut());
        for value in values {
            ffi::rtp_codec_feedback_add(self.native.pin_mut(), value.native());
        }
    }
    pub fn parameters(&self) -> Vec<RtpCodecParameter> {
        ffi::rtp_codec_parameters(self.native())
            .into_iter()
            .map(|entry| RtpCodecParameter {
                key: entry.key,
                value: entry.value,
            })
            .collect()
    }
    pub fn set_parameters(&mut self, values: &[RtpCodecParameter]) {
        let native: Vec<_> = values
            .iter()
            .map(|entry| ffi::FfiRtpCodecParameter {
                key: entry.key.clone(),
                value: entry.value.clone(),
            })
            .collect();
        ffi::rtp_codec_set_parameters(self.native.pin_mut(), &native);
    }
    /// SDK MIME bytes; None guards only the native kind-conversion assertion.
    pub fn mime_type_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let value = ffi::rtp_codec_mime_type(self.native(), &mut valid);
        valid.then_some(value)
    }
    /// SDK AbslStringify bytes, with the same conversion safety boundary.
    pub fn native_string_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let value = ffi::rtp_codec_string(self.native(), &mut valid);
        valid.then_some(value)
    }
    pub fn is_resiliency_codec(&self) -> bool {
        ffi::rtp_codec_is_resiliency(self.native())
    }
    pub fn is_media_codec(&self) -> bool {
        ffi::rtp_codec_is_media(self.native())
    }
    /// Calls WriteFmtpParameters on this SDK object's actual map field.
    pub fn write_fmtp_parameters(&self, output: &mut crate::StringBuilder) -> bool {
        ffi::rtp_codec_write_fmtp(self.native(), output.native_mut())
    }
    /// Calls ParseFmtpParameterSet on the actual map, retaining the complete
    /// returned SDK RTCError rather than synthesizing a Rust validation error.
    pub fn parse_fmtp_parameter_set(&mut self, line: impl AsRef<[u8]>) -> crate::RtcError {
        crate::RtcError::from_native(ffi::rtp_codec_parse_fmtp(
            self.native.pin_mut(),
            line.as_ref(),
        ))
    }
}
impl std::fmt::Debug for RtpCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpCodec")
            .field("name_bytes", &self.name_bytes())
            .field("kind", &self.kind())
            .field("clock_rate", &self.clock_rate())
            .field("num_channels", &self.num_channels())
            .field("rtcp_feedback", &self.rtcp_feedback())
            .field("parameters", &self.parameters())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    VeryLow,
    Low,
    Medium,
    High,
    Other(i32),
}
impl Priority {
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            0 => Self::VeryLow,
            1 => Self::Low,
            2 => Self::Medium,
            3 => Self::High,
            value => Self::Other(value),
        }
    }
    pub(crate) fn to_native(self) -> i32 {
        match self {
            Self::VeryLow => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Other(value) => value,
        }
    }
}

/// Complete owned SDK encoding value. Storage imposes no installation policy.
/// Native equality deliberately omits scalability_mode and request_key_frame.
pub struct RtpEncodingParameters {
    native: cxx::UniquePtr<ffi::NativeRtpEncoding>,
}
// SAFETY: SDK value owns primitives, optional primitives, byte strings, uint32
// vectors, Resolution and RtpCodec values. No retained references/callbacks,
// lazy state or native origin. Shared methods read/copy; mutation is exclusive.
unsafe impl Send for RtpEncodingParameters {}
unsafe impl Sync for RtpEncodingParameters {}
impl Default for RtpEncodingParameters {
    fn default() -> Self {
        Self::from_native(ffi::rtp_encoding_default())
    }
}
impl Clone for RtpEncodingParameters {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_encoding_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_encoding_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpEncodingParameters {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::rtp_encoding_equal(self.native(), rhs.native())
    }
}
// No Eq: native floating-point fields can contain NaNs.
impl RtpEncodingParameters {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpEncoding>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpEncoding {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiRtpEncodingFields {
        ffi::rtp_encoding_fields(self.native())
    }
    pub fn ssrc(&self) -> Option<u32> {
        let fields = self.fields();
        fields.has_ssrc.then_some(fields.ssrc)
    }
    pub fn set_ssrc(&mut self, value: Option<u32>) {
        ffi::rtp_encoding_set_ssrc(self.native.pin_mut(), value.is_some(), value.unwrap_or(0));
    }
    pub fn bitrate_priority(&self) -> f64 {
        self.fields().bitrate_priority
    }
    pub fn set_bitrate_priority(&mut self, value: f64) {
        ffi::rtp_encoding_set_bitrate_priority(self.native.pin_mut(), value);
    }
    pub fn network_priority(&self) -> Priority {
        Priority::from_native(self.fields().network_priority)
    }
    pub fn set_network_priority(&mut self, value: Priority) {
        ffi::rtp_encoding_set_network_priority(self.native.pin_mut(), value.to_native());
    }
    pub fn max_bitrate_bps(&self) -> Option<i32> {
        let fields = self.fields();
        fields.has_max_bitrate_bps.then_some(fields.max_bitrate_bps)
    }
    pub fn set_max_bitrate_bps(&mut self, value: Option<i32>) {
        ffi::rtp_encoding_set_max_bitrate_bps(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0),
        );
    }
    pub fn min_bitrate_bps(&self) -> Option<i32> {
        let fields = self.fields();
        fields.has_min_bitrate_bps.then_some(fields.min_bitrate_bps)
    }
    pub fn set_min_bitrate_bps(&mut self, value: Option<i32>) {
        ffi::rtp_encoding_set_min_bitrate_bps(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0),
        );
    }
    pub fn max_framerate(&self) -> Option<f64> {
        let fields = self.fields();
        fields.has_max_framerate.then_some(fields.max_framerate)
    }
    pub fn set_max_framerate(&mut self, value: Option<f64>) {
        ffi::rtp_encoding_set_max_framerate(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0.0),
        );
    }
    pub fn num_temporal_layers(&self) -> Option<i32> {
        let fields = self.fields();
        fields
            .has_num_temporal_layers
            .then_some(fields.num_temporal_layers)
    }
    pub fn set_num_temporal_layers(&mut self, value: Option<i32>) {
        ffi::rtp_encoding_set_num_temporal_layers(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0),
        );
    }
    pub fn scale_resolution_down_by(&self) -> Option<f64> {
        let fields = self.fields();
        fields
            .has_scale_resolution_down_by
            .then_some(fields.scale_resolution_down_by)
    }
    pub fn set_scale_resolution_down_by(&mut self, value: Option<f64>) {
        ffi::rtp_encoding_set_scale_resolution_down_by(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0.0),
        );
    }
    pub fn active(&self) -> bool {
        self.fields().active
    }
    pub fn set_active(&mut self, value: bool) {
        ffi::rtp_encoding_set_active(self.native.pin_mut(), value);
    }
    pub fn request_key_frame(&self) -> bool {
        self.fields().request_key_frame
    }
    pub fn set_request_key_frame(&mut self, value: bool) {
        ffi::rtp_encoding_set_request_key_frame(self.native.pin_mut(), value);
    }
    pub fn adaptive_ptime(&self) -> bool {
        self.fields().adaptive_ptime
    }
    pub fn set_adaptive_ptime(&mut self, value: bool) {
        ffi::rtp_encoding_set_adaptive_ptime(self.native.pin_mut(), value);
    }
    pub fn csrcs(&self) -> Option<Vec<u32>> {
        let fields = self.fields();
        fields.has_csrcs.then_some(fields.csrcs)
    }
    pub fn set_csrcs(&mut self, value: Option<&[u32]>) {
        ffi::rtp_encoding_set_csrcs(self.native.pin_mut(), value.is_some(), value.unwrap_or(&[]));
    }
    pub fn rid_bytes(&self) -> Vec<u8> {
        self.fields().rid
    }
    pub fn set_rid(&mut self, value: impl AsRef<[u8]>) {
        ffi::rtp_encoding_set_rid(self.native.pin_mut(), value.as_ref());
    }
    pub fn scalability_mode_bytes(&self) -> Option<Vec<u8>> {
        let fields = self.fields();
        fields
            .has_scalability_mode
            .then_some(fields.scalability_mode)
    }
    pub fn set_scalability_mode(&mut self, value: Option<&[u8]>) {
        ffi::rtp_encoding_set_scalability_mode(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(&[]),
        );
    }
    pub fn scale_resolution_down_to(&self) -> Option<Resolution> {
        let native = ffi::rtp_encoding_scale_to(self.native());
        if native.is_null() {
            None
        } else {
            Some(Resolution::from_native(native))
        }
    }
    pub fn set_scale_resolution_down_to(&mut self, value: Option<&Resolution>) {
        match value {
            Some(value) => ffi::rtp_encoding_set_scale_to(self.native.pin_mut(), value.native()),
            None => ffi::rtp_encoding_clear_scale_to(self.native.pin_mut()),
        }
    }
    pub fn codec(&self) -> Option<RtpCodec> {
        let native = ffi::rtp_encoding_codec(self.native());
        if native.is_null() {
            None
        } else {
            Some(RtpCodec::from_native(native))
        }
    }
    pub fn set_codec(&mut self, value: Option<&RtpCodec>) {
        match value {
            Some(value) => ffi::rtp_encoding_set_codec(self.native.pin_mut(), value.native()),
            None => ffi::rtp_encoding_clear_codec(self.native.pin_mut()),
        }
    }
    /// Calls SDK AbslStringify. Only a present codec's unsafe MIME conversion
    /// is rejected; signed dimensions and non-finite metadata remain formatable.
    pub fn native_string_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let value = ffi::rtp_encoding_string(self.native(), &mut valid);
        valid.then_some(value)
    }
}
impl std::fmt::Debug for RtpEncodingParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpEncodingParameters")
            .field("ssrc", &self.ssrc())
            .field("csrcs", &self.csrcs())
            .field("bitrate_priority", &self.bitrate_priority())
            .field("network_priority", &self.network_priority())
            .field("max_bitrate_bps", &self.max_bitrate_bps())
            .field("min_bitrate_bps", &self.min_bitrate_bps())
            .field("max_framerate", &self.max_framerate())
            .field("num_temporal_layers", &self.num_temporal_layers())
            .field("scale_resolution_down_by", &self.scale_resolution_down_by())
            .field("scale_resolution_down_to", &self.scale_resolution_down_to())
            .field("scalability_mode_bytes", &self.scalability_mode_bytes())
            .field("active", &self.active())
            .field("rid_bytes", &self.rid_bytes())
            .field("request_key_frame", &self.request_key_frame())
            .field("adaptive_ptime", &self.adaptive_ptime())
            .field("codec", &self.codec())
            .finish()
    }
}

/// Owned SDK strong alias with the full uint16 storage domain. No Default:
/// the SDK derived class has no default constructor.
pub struct PriorityValue {
    native: cxx::UniquePtr<ffi::NativePriorityValue>,
}
// SAFETY: SDK PriorityValue contains only an owned uint16. Borrowed scalar
// views remain tied to this owner; mutable views require exclusive access.
unsafe impl Send for PriorityValue {}
unsafe impl Sync for PriorityValue {}
impl PriorityValue {
    fn from_native(native: cxx::UniquePtr<ffi::NativePriorityValue>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    fn native(&self) -> &ffi::NativePriorityValue {
        self.native.as_ref().unwrap()
    }
    pub fn new(value: u16) -> Self {
        Self::from_native(ffi::priority_value_new(value))
    }
    /// Guards the SDK enum constructor's fatal default branch. This guard
    /// does not restrict raw uint16 construction or storage.
    pub fn from_priority(value: Priority) -> Option<Self> {
        let native = ffi::priority_value_from_priority(value.to_native());
        if native.is_null() {
            None
        } else {
            Some(Self::from_native(native))
        }
    }
    pub fn value(&self) -> u16 {
        *self.value_ref()
    }
    pub fn value_ref(&self) -> &u16 {
        ffi::priority_value_ref(self.native())
    }
    pub fn value_mut(&mut self) -> &mut u16 {
        ffi::priority_value_mut(self.native.pin_mut())
    }
    pub fn arrow_value_ref(&self) -> &u16 {
        ffi::priority_value_arrow(self.native())
    }
    pub fn arrow_value_mut(&mut self) -> &mut u16 {
        ffi::priority_value_arrow_mut(self.native.pin_mut())
    }
    pub fn as_underlying(&self) -> &u16 {
        ffi::priority_value_conversion(self.native())
    }
    pub fn into_value(self) -> u16 {
        ffi::priority_value_into_value(self.native)
    }
    pub fn into_const_value(self) -> u16 {
        ffi::priority_value_into_const_value(self.native)
    }
    pub fn into_deref(self) -> u16 {
        ffi::priority_value_into_deref(self.native)
    }
    pub fn into_const_deref(self) -> u16 {
        ffi::priority_value_into_const_deref(self.native)
    }
    /// Calls SDK AbslHashValue via absl::Hash. Process-local, not serialization.
    pub fn native_hash(&self) -> usize {
        ffi::priority_value_hash(self.native())
    }
}
impl Clone for PriorityValue {
    fn clone(&self) -> Self {
        Self::from_native(ffi::priority_value_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::priority_value_assign(self.native.pin_mut(), source.native());
    }
}
impl std::ops::Deref for PriorityValue {
    type Target = u16;
    fn deref(&self) -> &u16 {
        ffi::priority_value_deref(self.native())
    }
}
impl std::ops::DerefMut for PriorityValue {
    fn deref_mut(&mut self) -> &mut u16 {
        ffi::priority_value_deref_mut(self.native.pin_mut())
    }
}
impl PartialEq for PriorityValue {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::priority_value_equal(self.native(), rhs.native())
    }
    #[allow(
        clippy::partialeq_ne_impl,
        reason = "Dispatch the independent native inequality operator."
    )]
    fn ne(&self, rhs: &Self) -> bool {
        ffi::priority_value_not_equal(self.native(), rhs.native())
    }
}
impl Eq for PriorityValue {}
impl PartialOrd for PriorityValue {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(rhs))
    }
    fn lt(&self, rhs: &Self) -> bool {
        ffi::priority_value_less(self.native(), rhs.native())
    }
    fn le(&self, rhs: &Self) -> bool {
        ffi::priority_value_less_equal(self.native(), rhs.native())
    }
    fn gt(&self, rhs: &Self) -> bool {
        ffi::priority_value_greater(self.native(), rhs.native())
    }
    fn ge(&self, rhs: &Self) -> bool {
        ffi::priority_value_greater_equal(self.native(), rhs.native())
    }
}
impl Ord for PriorityValue {
    fn cmp(&self, rhs: &Self) -> std::cmp::Ordering {
        if self < rhs {
            std::cmp::Ordering::Less
        } else if self > rhs {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}
impl std::fmt::Debug for PriorityValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PriorityValue")
            .field(self.value_ref())
            .finish()
    }
}

/// Owns the actual SDK value, independent of a sender or factory.
pub struct RtpCodecParameters {
    native: cxx::UniquePtr<ffi::NativeRtpCodecParameters>,
}
// SAFETY: owned primitive/string/vector/map metadata only; shared reads do not
// mutate, and all mutation requires an exclusive Rust borrow.
unsafe impl Send for RtpCodecParameters {}
unsafe impl Sync for RtpCodecParameters {}
impl Default for RtpCodecParameters {
    fn default() -> Self {
        Self::from_native(ffi::rtp_codec_parameters_default())
    }
}
impl Clone for RtpCodecParameters {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_codec_parameters_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_codec_parameters_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpCodecParameters {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_codec_parameters_equal(self.native(), other.native())
    }
}
impl Eq for RtpCodecParameters {}
impl RtpCodecParameters {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpCodecParameters>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpCodecParameters {
        self.native.as_ref().unwrap()
    }
    pub fn payload_type(&self) -> i32 {
        ffi::rtp_codec_parameters_payload_type(self.native())
    }
    pub fn set_payload_type(&mut self, value: i32) {
        ffi::rtp_codec_parameters_set_payload_type(self.native.pin_mut(), value);
    }
    /// Independent owned SDK base-value copy, including all inherited fields.
    /// Mutation changes this parameter only after set_base_codec is called.
    pub fn base_codec(&self) -> RtpCodec {
        RtpCodec::from_native(ffi::rtp_codec_parameters_base(self.native()))
    }
    /// Assigns the complete inherited base subobject, preserving payload_type.
    pub fn set_base_codec(&mut self, codec: &RtpCodec) {
        ffi::rtp_codec_parameters_set_base(self.native.pin_mut(), codec.native());
    }
    pub fn mime_type_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_codec_parameters_mime_type(self.native(), &mut valid);
        valid.then_some(bytes)
    }
    pub fn native_string_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_codec_parameters_string(self.native(), &mut valid);
        valid.then_some(bytes)
    }
    pub fn is_resiliency_codec(&self) -> bool {
        ffi::rtp_codec_parameters_is_resiliency(self.native())
    }
    pub fn is_media_codec(&self) -> bool {
        ffi::rtp_codec_parameters_is_media(self.native())
    }
}
impl std::fmt::Debug for RtpCodecParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpCodecParameters")
            .field("payload_type", &self.payload_type())
            .field("base_codec", &self.base_codec())
            .finish()
    }
}

/// Owns the actual SDK value, independent of a sender or factory.
pub struct RtcpParameters {
    native: cxx::UniquePtr<ffi::NativeRtcpParameters>,
}
// SAFETY: owned primitive/string/vector/map metadata only; shared reads do not
// mutate, and all mutation requires an exclusive Rust borrow.
unsafe impl Send for RtcpParameters {}
unsafe impl Sync for RtcpParameters {}
impl Default for RtcpParameters {
    fn default() -> Self {
        Self::from_native(ffi::rtcp_parameters_default())
    }
}
impl Clone for RtcpParameters {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtcp_parameters_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtcp_parameters_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtcpParameters {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtcp_parameters_equal(self.native(), other.native())
    }
}
impl Eq for RtcpParameters {}
impl RtcpParameters {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtcpParameters>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtcpParameters {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiRtcpParametersFields {
        ffi::rtcp_parameters_fields(self.native())
    }
    pub fn ssrc(&self) -> Option<u32> {
        let fields = self.fields();
        fields.has_ssrc.then_some(fields.ssrc)
    }
    pub fn set_ssrc(&mut self, value: Option<u32>) {
        ffi::rtcp_parameters_set_ssrc(self.native.pin_mut(), value.is_some(), value.unwrap_or(0));
    }
    pub fn cname_bytes(&self) -> Vec<u8> {
        self.fields().cname_bytes
    }
    pub fn set_cname(&mut self, value: impl AsRef<[u8]>) {
        ffi::rtcp_parameters_set_cname(self.native.pin_mut(), value.as_ref());
    }
    pub fn reduced_size(&self) -> bool {
        self.fields().reduced_size
    }
    pub fn set_reduced_size(&mut self, value: bool) {
        ffi::rtcp_parameters_set_reduced_size(self.native.pin_mut(), value);
    }
    pub fn mux(&self) -> bool {
        self.fields().mux
    }
    pub fn set_mux(&mut self, value: bool) {
        ffi::rtcp_parameters_set_mux(self.native.pin_mut(), value);
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtcp_parameters_string(self.native())
    }
}
impl std::fmt::Debug for RtcpParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcpParameters")
            .field("ssrc", &self.ssrc())
            .field("cname_bytes", &self.cname_bytes())
            .field("reduced_size", &self.reduced_size())
            .field("mux", &self.mux())
            .finish()
    }
}

/// Owns the SDK signed-int strong alias. All i32 metadata is constructible;
/// Valid checks 1..=255, and zero is unset. No construction range gate is invented.
pub struct RtpHeaderExtensionId {
    native: cxx::UniquePtr<ffi::NativeRtpHeaderExtensionId>,
}
// SAFETY: SDK RtpHeaderExtensionId contains only an owned int. Borrowed scalar
// views remain tied to this owner; mutable views require exclusive access.
unsafe impl Send for RtpHeaderExtensionId {}
unsafe impl Sync for RtpHeaderExtensionId {}
impl RtpHeaderExtensionId {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpHeaderExtensionId>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpHeaderExtensionId {
        self.native.as_ref().unwrap()
    }
    pub fn new(value: i32) -> Self {
        Self::from_native(ffi::rtp_header_id_new(value))
    }
    pub fn not_set() -> Self {
        Self::from_native(ffi::rtp_header_id_not_set())
    }
    pub fn min_id() -> Self {
        Self::from_native(ffi::rtp_header_id_constant(0))
    }
    pub fn max_id() -> Self {
        Self::from_native(ffi::rtp_header_id_constant(1))
    }
    pub fn one_byte_max_id() -> Self {
        Self::from_native(ffi::rtp_header_id_constant(2))
    }
    /// Calls the SDK implicit int conversion, distinct from the borrowed base conversion.
    pub fn to_int(&self) -> i32 {
        ffi::rtp_header_id_to_int(self.native())
    }
    pub fn is_valid(&self) -> bool {
        ffi::rtp_header_id_valid(self.native())
    }
    pub fn is_set(&self) -> bool {
        ffi::rtp_header_id_is_set(self.native())
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_header_id_string(self.native())
    }
    pub fn value(&self) -> i32 {
        *self.value_ref()
    }
    pub fn value_ref(&self) -> &i32 {
        ffi::rtp_header_id_ref(self.native())
    }
    pub fn value_mut(&mut self) -> &mut i32 {
        ffi::rtp_header_id_ref_mut(self.native.pin_mut())
    }
    pub fn arrow_value_ref(&self) -> &i32 {
        ffi::rtp_header_id_arrow(self.native())
    }
    pub fn arrow_value_mut(&mut self) -> &mut i32 {
        ffi::rtp_header_id_arrow_mut(self.native.pin_mut())
    }
    pub fn as_underlying(&self) -> &i32 {
        ffi::rtp_header_id_conversion(self.native())
    }
    pub fn into_value(self) -> i32 {
        ffi::rtp_header_id_into_value(self.native)
    }
    pub fn into_const_value(self) -> i32 {
        ffi::rtp_header_id_into_const_value(self.native)
    }
    pub fn into_deref(self) -> i32 {
        ffi::rtp_header_id_into_deref(self.native)
    }
    pub fn into_const_deref(self) -> i32 {
        ffi::rtp_header_id_into_const_deref(self.native)
    }
    /// Calls SDK AbslHashValue via absl::Hash. Process-local, not serialization.
    pub fn native_hash(&self) -> usize {
        ffi::rtp_header_id_hash(self.native())
    }
}
impl Clone for RtpHeaderExtensionId {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_header_id_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_header_id_assign(self.native.pin_mut(), source.native());
    }
}
impl std::ops::Deref for RtpHeaderExtensionId {
    type Target = i32;
    fn deref(&self) -> &i32 {
        ffi::rtp_header_id_deref(self.native())
    }
}
impl std::ops::DerefMut for RtpHeaderExtensionId {
    fn deref_mut(&mut self) -> &mut i32 {
        ffi::rtp_header_id_deref_mut(self.native.pin_mut())
    }
}
impl PartialEq for RtpHeaderExtensionId {
    fn eq(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_equal(self.native(), rhs.native())
    }
    #[allow(
        clippy::partialeq_ne_impl,
        reason = "Dispatch the independent native inequality operator."
    )]
    fn ne(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_not_equal(self.native(), rhs.native())
    }
}
impl Eq for RtpHeaderExtensionId {}
impl PartialOrd for RtpHeaderExtensionId {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(rhs))
    }
    fn lt(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_less(self.native(), rhs.native())
    }
    fn le(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_less_equal(self.native(), rhs.native())
    }
    fn gt(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_greater(self.native(), rhs.native())
    }
    fn ge(&self, rhs: &Self) -> bool {
        ffi::rtp_header_id_greater_equal(self.native(), rhs.native())
    }
}
impl Ord for RtpHeaderExtensionId {
    fn cmp(&self, rhs: &Self) -> std::cmp::Ordering {
        if self < rhs {
            std::cmp::Ordering::Less
        } else if self > rhs {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}
impl std::fmt::Debug for RtpHeaderExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RtpHeaderExtensionId")
            .field(self.value_ref())
            .finish()
    }
}

impl Default for RtpHeaderExtensionId {
    fn default() -> Self {
        Self::from_native(ffi::rtp_header_id_default())
    }
}
