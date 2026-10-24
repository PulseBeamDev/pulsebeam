//! Native SDK header-extension values and SDK-owned lookup collections.
use crate::{RtpHeaderExtensionId, ffi};

/// SDK unscoped Filter values. Its legal C++ conversion domain is 0..=3,
/// including the unnamed value 3. Other raw values are retained here but
/// rejected before a native enum conversion, not reported as an SDK RTCError.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpExtensionFilter {
    DiscardEncrypted,
    PreferEncrypted,
    RequireEncrypted,
    Other(i32),
}
impl RtpExtensionFilter {
    pub fn to_native(self) -> i32 {
        match self {
            Self::DiscardEncrypted => 0,
            Self::PreferEncrypted => 1,
            Self::RequireEncrypted => 2,
            Self::Other(raw) => raw,
        }
    }
    pub fn from_native(raw: i32) -> Self {
        match raw {
            0 => Self::DiscardEncrypted,
            1 => Self::PreferEncrypted,
            2 => Self::RequireEncrypted,
            raw => Self::Other(raw),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpExtensionFilterRangeError(pub i32);
impl std::fmt::Display for RtpExtensionFilterRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RtpExtension::Filter raw value {} is outside its C++ enum conversion domain 0..=3",
            self.0
        )
    }
}
impl std::error::Error for RtpExtensionFilterRangeError {}

/// Owns the actual SDK RtpExtension. URI bytes and the full signed ID metadata
/// domain are lossless. Only explicit constructors gate SDK ID DCHECKs.
pub struct RtpExtension {
    native: cxx::UniquePtr<ffi::NativeRtpExtension>,
}
// SAFETY: SDK value contains owned string, scalar strong alias and bool only.
// Shared reads have no mutation/cache; mutation needs an exclusive Rust borrow.
unsafe impl Send for RtpExtension {}
unsafe impl Sync for RtpExtension {}
impl RtpExtension {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpExtension>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpExtension {
        self.native.as_ref().unwrap()
    }
    fn optional(native: cxx::UniquePtr<ffi::NativeRtpExtension>) -> Option<Self> {
        if native.is_null() {
            None
        } else {
            Some(Self::from_native(native))
        }
    }
    /// Calls the two-argument SDK constructor after guarding its ID DCHECK.
    pub fn new(uri: impl AsRef<[u8]>, id: &RtpHeaderExtensionId) -> Option<Self> {
        Self::optional(ffi::rtp_extension_new(
            uri.as_ref(),
            id.native(),
            false,
            false,
        ))
    }
    pub fn new_encrypted(
        uri: impl AsRef<[u8]>,
        id: &RtpHeaderExtensionId,
        encrypt: bool,
    ) -> Option<Self> {
        Self::optional(ffi::rtp_extension_new(
            uri.as_ref(),
            id.native(),
            true,
            encrypt,
        ))
    }
    /// Represents the SDK's deprecated int overload, with the same DCHECK guard.
    pub fn new_int(uri: impl AsRef<[u8]>, id: i32) -> Option<Self> {
        Self::optional(ffi::rtp_extension_new_int(uri.as_ref(), id, false, false))
    }
    pub fn new_int_encrypted(uri: impl AsRef<[u8]>, id: i32, encrypt: bool) -> Option<Self> {
        Self::optional(ffi::rtp_extension_new_int(uri.as_ref(), id, true, encrypt))
    }
    pub fn uri_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_fields(self.native()).uri_bytes
    }
    pub fn set_uri(&mut self, uri: impl AsRef<[u8]>) {
        ffi::rtp_extension_set_uri(self.native.pin_mut(), uri.as_ref());
    }
    /// Independent SDK ID copy. Use set_id to assign it back.
    pub fn id(&self) -> RtpHeaderExtensionId {
        RtpHeaderExtensionId::from_native(ffi::rtp_extension_id(self.native()))
    }
    /// SDK metadata assignment permits unset and out-of-range IDs.
    pub fn set_id(&mut self, id: &RtpHeaderExtensionId) {
        ffi::rtp_extension_set_id(self.native.pin_mut(), id.native());
    }
    pub fn encrypt(&self) -> bool {
        ffi::rtp_extension_fields(self.native()).encrypt
    }
    pub fn set_encrypt(&mut self, encrypt: bool) {
        ffi::rtp_extension_set_encrypt(self.native.pin_mut(), encrypt);
    }
    pub fn to_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_to_string(self.native())
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_string(self.native())
    }
    /// SDK RtpExtension::kEncryptHeaderExtensionsUri, copied from the native constant.
    pub fn encrypt_header_extensions_uri() -> Vec<u8> {
        ffi::rtp_extension_encrypt_header_extensions_uri()
    }
    /// SDK RtpExtension::kAudioLevelUri, copied from the native constant.
    pub fn audio_level_uri() -> Vec<u8> {
        ffi::rtp_extension_audio_level_uri()
    }
    /// SDK RtpExtension::kTimestampOffsetUri, copied from the native constant.
    pub fn timestamp_offset_uri() -> Vec<u8> {
        ffi::rtp_extension_timestamp_offset_uri()
    }
    /// SDK RtpExtension::kAbsSendTimeUri, copied from the native constant.
    pub fn abs_send_time_uri() -> Vec<u8> {
        ffi::rtp_extension_abs_send_time_uri()
    }
    /// SDK RtpExtension::kAbsoluteCaptureTimeUri, copied from the native constant.
    pub fn absolute_capture_time_uri() -> Vec<u8> {
        ffi::rtp_extension_absolute_capture_time_uri()
    }
    /// SDK RtpExtension::kVideoRotationUri, copied from the native constant.
    pub fn video_rotation_uri() -> Vec<u8> {
        ffi::rtp_extension_video_rotation_uri()
    }
    /// SDK RtpExtension::kVideoContentTypeUri, copied from the native constant.
    pub fn video_content_type_uri() -> Vec<u8> {
        ffi::rtp_extension_video_content_type_uri()
    }
    /// SDK RtpExtension::kVideoTimingUri, copied from the native constant.
    pub fn video_timing_uri() -> Vec<u8> {
        ffi::rtp_extension_video_timing_uri()
    }
    /// SDK RtpExtension::kGenericFrameDescriptorUri00, copied from the native constant.
    pub fn generic_frame_descriptor_uri_00() -> Vec<u8> {
        ffi::rtp_extension_generic_frame_descriptor_uri_00()
    }
    /// SDK RtpExtension::kDependencyDescriptorUri, copied from the native constant.
    pub fn dependency_descriptor_uri() -> Vec<u8> {
        ffi::rtp_extension_dependency_descriptor_uri()
    }
    /// SDK RtpExtension::kVideoLayersAllocationUri, copied from the native constant.
    pub fn video_layers_allocation_uri() -> Vec<u8> {
        ffi::rtp_extension_video_layers_allocation_uri()
    }
    /// SDK RtpExtension::kTransportSequenceNumberUri, copied from the native constant.
    pub fn transport_sequence_number_uri() -> Vec<u8> {
        ffi::rtp_extension_transport_sequence_number_uri()
    }
    /// SDK RtpExtension::kTransportSequenceNumberV2Uri, copied from the native constant.
    pub fn transport_sequence_number_v2_uri() -> Vec<u8> {
        ffi::rtp_extension_transport_sequence_number_v2_uri()
    }
    /// SDK RtpExtension::kPlayoutDelayUri, copied from the native constant.
    pub fn playout_delay_uri() -> Vec<u8> {
        ffi::rtp_extension_playout_delay_uri()
    }
    /// SDK RtpExtension::kColorSpaceUri, copied from the native constant.
    pub fn color_space_uri() -> Vec<u8> {
        ffi::rtp_extension_color_space_uri()
    }
    /// SDK RtpExtension::kMidUri, copied from the native constant.
    pub fn mid_uri() -> Vec<u8> {
        ffi::rtp_extension_mid_uri()
    }
    /// SDK RtpExtension::kRidUri, copied from the native constant.
    pub fn rid_uri() -> Vec<u8> {
        ffi::rtp_extension_rid_uri()
    }
    /// SDK RtpExtension::kRepairedRidUri, copied from the native constant.
    pub fn repaired_rid_uri() -> Vec<u8> {
        ffi::rtp_extension_repaired_rid_uri()
    }
    /// SDK RtpExtension::kVideoFrameTrackingIdUri, copied from the native constant.
    pub fn video_frame_tracking_id_uri() -> Vec<u8> {
        ffi::rtp_extension_video_frame_tracking_id_uri()
    }
    /// SDK RtpExtension::kCsrcAudioLevelsUri, copied from the native constant.
    pub fn csrc_audio_levels_uri() -> Vec<u8> {
        ffi::rtp_extension_csrc_audio_levels_uri()
    }
    /// SDK RtpExtension::kCorruptionDetectionUri, copied from the native constant.
    pub fn corruption_detection_uri() -> Vec<u8> {
        ffi::rtp_extension_corruption_detection_uri()
    }
    /// SDK deprecated RtpExtension::kMinId alias.
    pub fn min_id() -> RtpHeaderExtensionId {
        RtpHeaderExtensionId::from_native(ffi::rtp_extension_min_id())
    }
    /// SDK deprecated RtpExtension::kMaxId alias.
    pub fn max_id() -> RtpHeaderExtensionId {
        RtpHeaderExtensionId::from_native(ffi::rtp_extension_max_id())
    }
    /// SDK deprecated RtpExtension::kOneByteHeaderExtensionMaxId alias.
    pub fn one_byte_max_id() -> RtpHeaderExtensionId {
        RtpHeaderExtensionId::from_native(ffi::rtp_extension_one_byte_max_id())
    }
    /// SDK value-size limit, not an ID bound.
    pub fn max_value_size() -> i32 {
        ffi::rtp_extension_max_value_size()
    }
    /// SDK value-size limit, not an ID bound.
    pub fn one_byte_max_value_size() -> i32 {
        ffi::rtp_extension_one_byte_max_value_size()
    }
    pub fn is_supported_for_audio(uri: impl AsRef<[u8]>) -> bool {
        ffi::rtp_extension_supported_audio(uri.as_ref())
    }
    pub fn is_supported_for_video(uri: impl AsRef<[u8]>) -> bool {
        ffi::rtp_extension_supported_video(uri.as_ref())
    }
    pub fn is_encryption_supported(uri: impl AsRef<[u8]>) -> bool {
        ffi::rtp_extension_encryption_supported(uri.as_ref())
    }
}
impl Default for RtpExtension {
    fn default() -> Self {
        Self::from_native(ffi::rtp_extension_default())
    }
}
impl Clone for RtpExtension {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_extension_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_extension_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpExtension {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_extension_equal(self.native(), other.native())
    }
}
impl Eq for RtpExtension {}
impl std::fmt::Debug for RtpExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpExtension")
            .field("uri_bytes", &self.uri_bytes())
            .field("id", &self.id())
            .field("encrypt", &self.encrypt())
            .finish()
    }
}

/// Owns the SDK vector passed to the static Find/Deduplicate helpers.
/// Lookup results borrow its actual SDK elements; get instead makes a copy.
pub struct RtpExtensions {
    native: cxx::UniquePtr<ffi::NativeRtpExtensionList>,
}
// SAFETY: vector owns only the pure values above. Lookup borrows prevent
// destruction and mutation; all vector mutation requires exclusive access.
unsafe impl Send for RtpExtensions {}
unsafe impl Sync for RtpExtensions {}
impl RtpExtensions {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpExtensionList>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpExtensionList {
        self.native.as_ref().unwrap()
    }
    pub fn len(&self) -> usize {
        ffi::rtp_extension_list_len(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn push(&mut self, value: &RtpExtension) {
        ffi::rtp_extension_list_add(self.native.pin_mut(), value.native());
    }
    pub fn clear(&mut self) {
        ffi::rtp_extension_list_clear(self.native.pin_mut());
    }
    pub fn get(&self, index: usize) -> Option<RtpExtension> {
        RtpExtension::optional(ffi::rtp_extension_list_get(self.native(), index))
    }
    /// Calls SDK FindHeaderExtensionByUri. The result borrows only this list,
    /// not the temporary URI input; no element is copied for the loan.
    pub fn find_by_uri(
        &self,
        uri: impl AsRef<[u8]>,
        filter: RtpExtensionFilter,
    ) -> Result<Option<RtpExtensionRef<'_>>, RtpExtensionFilterRangeError> {
        let mut valid = false;
        let native = ffi::rtp_extension_list_find(
            self.native(),
            uri.as_ref(),
            filter.to_native(),
            &mut valid,
        );
        if !valid {
            return Err(RtpExtensionFilterRangeError(filter.to_native()));
        }
        Ok(self.loan(native))
    }
    pub fn find_by_uri_and_encryption(
        &self,
        uri: impl AsRef<[u8]>,
        encrypt: bool,
    ) -> Option<RtpExtensionRef<'_>> {
        self.loan(ffi::rtp_extension_list_find_encryption(
            self.native(),
            uri.as_ref(),
            encrypt,
        ))
    }
    fn loan(
        &self,
        native: cxx::UniquePtr<ffi::NativeRtpExtensionBorrowed>,
    ) -> Option<RtpExtensionRef<'_>> {
        if native.is_null() {
            None
        } else {
            Some(RtpExtensionRef {
                native,
                _parent: std::marker::PhantomData,
            })
        }
    }
    /// Calls SDK DeduplicateHeaderExtensions; returns an independent SDK vector.
    pub fn deduplicate(
        &self,
        filter: RtpExtensionFilter,
    ) -> Result<Self, RtpExtensionFilterRangeError> {
        let native = ffi::rtp_extension_list_deduplicate(self.native(), filter.to_native());
        if native.is_null() {
            Err(RtpExtensionFilterRangeError(filter.to_native()))
        } else {
            Ok(Self::from_native(native))
        }
    }
}
impl Default for RtpExtensions {
    fn default() -> Self {
        Self::from_native(ffi::rtp_extension_list_new())
    }
}
impl Clone for RtpExtensions {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_extension_list_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_extension_list_assign(self.native.pin_mut(), source.native());
    }
}
impl std::fmt::Debug for RtpExtensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries((0..self.len()).map(|i| self.get(i).unwrap()))
            .finish()
    }
}

/// Read-only loan of the actual SDK lookup pointer. The parent must remain
/// alive and unmodified. Native loan destruction never dereferences the pointer.
///
/// ```compile_fail
/// use pulsebeam_webrtc_sys::{RtpExtension, RtpExtensions, RtpHeaderExtensionId};
/// let mut values = RtpExtensions::default();
/// values.push(&RtpExtension::new(b"x", &RtpHeaderExtensionId::new(1)).unwrap());
/// let loan = values.find_by_uri_and_encryption(b"x", false).unwrap();
/// values.clear(); // cannot invalidate the live SDK element loan
/// assert_eq!(loan.id().value(), 1);
/// ```
pub struct RtpExtensionRef<'a> {
    native: cxx::UniquePtr<ffi::NativeRtpExtensionBorrowed>,
    _parent: std::marker::PhantomData<&'a RtpExtensions>,
}
// SAFETY: parent is Sync and its immutable borrow spans this loan. All reads
// touch only the pure SDK value; native pointer holder has no mutable state.
unsafe impl Send for RtpExtensionRef<'_> {}
unsafe impl Sync for RtpExtensionRef<'_> {}
impl RtpExtensionRef<'_> {
    pub(crate) fn native(&self) -> &ffi::NativeRtpExtensionBorrowed {
        self.native.as_ref().unwrap()
    }
    pub fn to_owned(&self) -> RtpExtension {
        RtpExtension::from_native(ffi::rtp_extension_borrowed_clone(self.native()))
    }
    pub fn uri_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_borrowed_fields(self.native()).uri_bytes
    }
    pub fn id(&self) -> RtpHeaderExtensionId {
        RtpHeaderExtensionId::from_native(ffi::rtp_extension_borrowed_id(self.native()))
    }
    pub fn encrypt(&self) -> bool {
        ffi::rtp_extension_borrowed_fields(self.native()).encrypt
    }
    pub fn to_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_borrowed_to_string(self.native())
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_extension_borrowed_string(self.native())
    }
}
impl std::fmt::Debug for RtpExtensionRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.to_owned(), f)
    }
}
