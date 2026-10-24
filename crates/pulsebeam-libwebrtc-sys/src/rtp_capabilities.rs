//! Complete native SDK capability metadata, independent of a live peer.
use crate::{RtpCodec, RtpHeaderExtensionId, ffi};

/// Transceiver operation failure with the full owning native SDK error.
/// Binding and safety failures happen before SDK dispatch and are not
/// synthesized native RTCError completions.
#[derive(Debug)]
pub enum RtpTransceiverOperationError {
    Binding(crate::PeerError),
    Native(crate::RtcError),
    /// Installing an unknown preferred direction reaches a fatal deferred
    /// SDK codec-selection switch. Pure direction/header metadata is unchanged.
    UnsafeDirection(i32),
}
/// Capability-installation spelling of the shared transceiver operation error.
pub type RtpCapabilityInstallError = RtpTransceiverOperationError;
impl std::fmt::Display for RtpTransceiverOperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binding(error) => std::fmt::Display::fmt(error, f),
            Self::Native(error) => write!(f, "native RTP transceiver operation failed: {error:?}"),
            Self::UnsafeDirection(raw) => write!(
                f,
                "preferred direction {raw} reaches a fatal deferred SDK codec-selection switch"
            ),
        }
    }
}
impl std::error::Error for RtpTransceiverOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::Native(error) => Some(error),
            Self::UnsafeDirection(_) => None,
        }
    }
}

/// Exact uint8_t scalability enum metadata, including unrecognized raw values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ScalabilityMode(u8);
impl ScalabilityMode {
    pub const fn from_raw(raw: u8) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u8 {
        self.0
    }
    pub fn all() -> Vec<Self> {
        ffi::rtp_scalability_modes_all()
            .into_iter()
            .map(Self)
            .collect()
    }
    pub fn native_name_bytes(self) -> Option<Vec<u8>> {
        Self::optional_native_name_bytes(Some(self))
    }
    /// SDK optional overload prints "nullopt" for absence. Unknown present
    /// metadata is rejected only before its fatal SDK string conversion.
    pub fn optional_native_name_bytes(value: Option<Self>) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_scalability_mode_string(
            value.is_some(),
            value.map_or(0, Self::raw),
            &mut valid,
        );
        valid.then_some(bytes)
    }
    pub const L1T1: Self = Self(0);
    pub const L1T2: Self = Self(1);
    pub const L1T3: Self = Self(2);
    pub const L2T1: Self = Self(3);
    pub const L2T1H: Self = Self(4);
    pub const L2T1_KEY: Self = Self(5);
    pub const L2T2: Self = Self(6);
    pub const L2T2H: Self = Self(7);
    pub const L2T2_KEY: Self = Self(8);
    pub const L2T2_KEY_SHIFT: Self = Self(9);
    pub const L2T3: Self = Self(10);
    pub const L2T3H: Self = Self(11);
    pub const L2T3_KEY: Self = Self(12);
    pub const L3T1: Self = Self(13);
    pub const L3T1H: Self = Self(14);
    pub const L3T1_KEY: Self = Self(15);
    pub const L3T2: Self = Self(16);
    pub const L3T2H: Self = Self(17);
    pub const L3T2_KEY: Self = Self(18);
    pub const L3T3: Self = Self(19);
    pub const L3T3H: Self = Self(20);
    pub const L3T3_KEY: Self = Self(21);
    pub const S2T1: Self = Self(22);
    pub const S2T1H: Self = Self(23);
    pub const S2T2: Self = Self(24);
    pub const S2T2H: Self = Self(25);
    pub const S2T3: Self = Self(26);
    pub const S2T3H: Self = Self(27);
    pub const S3T1: Self = Self(28);
    pub const S3T1H: Self = Self(29);
    pub const S3T2: Self = Self(30);
    pub const S3T2H: Self = Self(31);
    pub const S3T3: Self = Self(32);
    pub const S3T3H: Self = Self(33);
}

/// Complete SDK FEC enum metadata; no artificial closed-enum range restriction.
#[derive(Clone, Copy, Debug)]
pub enum FecMechanism {
    Red,
    RedAndUlpfec,
    Flexfec,
    Other(i32),
}
impl PartialEq for FecMechanism {
    fn eq(&self, other: &Self) -> bool {
        self.raw() == other.raw()
    }
}
impl Eq for FecMechanism {}
impl std::hash::Hash for FecMechanism {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.raw(), state);
    }
}
impl FecMechanism {
    pub const fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Red,
            1 => Self::RedAndUlpfec,
            2 => Self::Flexfec,
            value => Self::Other(value),
        }
    }
    pub const fn raw(self) -> i32 {
        match self {
            Self::Red => 0,
            Self::RedAndUlpfec => 1,
            Self::Flexfec => 2,
            Self::Other(value) => value,
        }
    }
}

/// Complete SDK direction metadata, distinct from the legacy four-state actor
/// convenience enum. Unknown fixed-underlying signed values remain storable.
#[derive(Clone, Copy, Debug)]
pub enum RtpTransceiverDirectionValue {
    SendReceive,
    SendOnly,
    ReceiveOnly,
    Inactive,
    Stopped,
    Other(i32),
}
impl RtpTransceiverDirectionValue {
    pub const fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::SendReceive,
            1 => Self::SendOnly,
            2 => Self::ReceiveOnly,
            3 => Self::Inactive,
            4 => Self::Stopped,
            value => Self::Other(value),
        }
    }
    pub const fn raw(self) -> i32 {
        match self {
            Self::SendReceive => 0,
            Self::SendOnly => 1,
            Self::ReceiveOnly => 2,
            Self::Inactive => 3,
            Self::Stopped => 4,
            Self::Other(value) => value,
        }
    }
    /// Exact SDK AbslStringify. Unrecognized metadata safely emits no bytes.
    pub fn native_string_bytes(self) -> Vec<u8> {
        ffi::rtp_direction_string(self.raw())
    }
}
impl PartialEq for RtpTransceiverDirectionValue {
    fn eq(&self, other: &Self) -> bool {
        self.raw() == other.raw()
    }
}
impl Eq for RtpTransceiverDirectionValue {}
impl std::hash::Hash for RtpTransceiverDirectionValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.raw(), state);
    }
}

/// Entire owned SDK RtpCodecCapability. Child snapshots are independent copies.
pub struct RtpCodecCapability {
    native: cxx::UniquePtr<ffi::NativeRtpCodecCapability>,
}
// SAFETY: owned primitive/string/vector/map metadata only. Shared access reads;
// all writes are exclusively borrowed and destruction has no engine affinity.
unsafe impl Send for RtpCodecCapability {}
unsafe impl Sync for RtpCodecCapability {}
impl Default for RtpCodecCapability {
    fn default() -> Self {
        Self::from_native(ffi::rtp_codec_capability_default())
    }
}
impl Clone for RtpCodecCapability {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_codec_capability_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_codec_capability_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpCodecCapability {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_codec_capability_equal(self.native(), other.native())
    }
}
impl Eq for RtpCodecCapability {}
impl RtpCodecCapability {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpCodecCapability>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpCodecCapability {
        self.native.as_ref().unwrap()
    }
}

/// Entire owned SDK RtpHeaderExtensionCapability. Child snapshots are independent copies.
pub struct RtpHeaderExtensionCapabilityValue {
    native: cxx::UniquePtr<ffi::NativeRtpHeaderExtensionCapability>,
}
// SAFETY: owned primitive/string/vector/map metadata only. Shared access reads;
// all writes are exclusively borrowed and destruction has no engine affinity.
unsafe impl Send for RtpHeaderExtensionCapabilityValue {}
unsafe impl Sync for RtpHeaderExtensionCapabilityValue {}
impl Default for RtpHeaderExtensionCapabilityValue {
    fn default() -> Self {
        Self::from_native(ffi::rtp_header_capability_default())
    }
}
impl Clone for RtpHeaderExtensionCapabilityValue {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_header_capability_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_header_capability_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpHeaderExtensionCapabilityValue {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_header_capability_equal(self.native(), other.native())
    }
}
impl Eq for RtpHeaderExtensionCapabilityValue {}
impl RtpHeaderExtensionCapabilityValue {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeRtpHeaderExtensionCapability>,
    ) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpHeaderExtensionCapability {
        self.native.as_ref().unwrap()
    }
}

/// Entire owned SDK RtpCapabilities. Child snapshots are independent copies.
pub struct RtpCapabilities {
    native: cxx::UniquePtr<ffi::NativeRtpCapabilities>,
}
// SAFETY: owned primitive/string/vector/map metadata only. Shared access reads;
// all writes are exclusively borrowed and destruction has no engine affinity.
unsafe impl Send for RtpCapabilities {}
unsafe impl Sync for RtpCapabilities {}
impl Default for RtpCapabilities {
    fn default() -> Self {
        Self::from_native(ffi::rtp_capabilities_default())
    }
}
impl Clone for RtpCapabilities {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_capabilities_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_capabilities_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpCapabilities {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_capabilities_equal(self.native(), other.native())
    }
}
impl Eq for RtpCapabilities {}
impl RtpCapabilities {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpCapabilities>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpCapabilities {
        self.native.as_ref().unwrap()
    }
}

impl RtpCodecCapability {
    pub fn base_codec(&self) -> RtpCodec {
        RtpCodec::from_native(ffi::rtp_codec_capability_base(self.native()))
    }
    /// Assigns every inherited field without replacing capability-only fields.
    pub fn set_base_codec(&mut self, value: &RtpCodec) {
        ffi::rtp_codec_capability_set_base(self.native.pin_mut(), value.native());
    }
    pub fn preferred_payload_type(&self) -> Option<i32> {
        let fields = ffi::rtp_codec_capability_fields(self.native());
        fields.has_payload_type.then_some(fields.payload_type)
    }
    pub fn set_preferred_payload_type(&mut self, value: Option<i32>) {
        ffi::rtp_codec_capability_set_payload_type(
            self.native.pin_mut(),
            value.is_some(),
            value.unwrap_or(0),
        );
    }
    pub fn scalability_modes(&self) -> Vec<ScalabilityMode> {
        ffi::rtp_codec_capability_modes(self.native())
            .into_iter()
            .map(ScalabilityMode::from_raw)
            .collect()
    }
    pub fn set_scalability_modes(&mut self, value: &[ScalabilityMode]) {
        let raw: Vec<_> = value.iter().map(|mode| mode.raw()).collect();
        ffi::rtp_codec_capability_set_modes(self.native.pin_mut(), &raw);
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_codec_capability_string(self.native())
    }
    pub fn mime_type_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_codec_capability_mime_type(self.native(), &mut valid);
        valid.then_some(bytes)
    }
    pub fn is_resiliency_codec(&self) -> bool {
        ffi::rtp_codec_capability_is_resiliency(self.native())
    }
    pub fn is_media_codec(&self) -> bool {
        ffi::rtp_codec_capability_is_media(self.native())
    }
}
impl std::fmt::Debug for RtpCodecCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpCodecCapability")
            .field("base_codec", &self.base_codec())
            .field("preferred_payload_type", &self.preferred_payload_type())
            .field("scalability_modes", &self.scalability_modes())
            .finish()
    }
}

impl RtpHeaderExtensionCapabilityValue {
    pub fn new(uri: impl AsRef<[u8]>) -> Self {
        Self::construct(uri.as_ref(), &RtpHeaderExtensionId::not_set(), false, 0, 0)
    }
    pub fn with_id(uri: impl AsRef<[u8]>, id: &RtpHeaderExtensionId) -> Self {
        Self::construct(uri.as_ref(), id, false, 0, 1)
    }
    pub fn with_direction(
        uri: impl AsRef<[u8]>,
        id: &RtpHeaderExtensionId,
        direction: RtpTransceiverDirectionValue,
    ) -> Self {
        Self::construct(uri.as_ref(), id, false, direction.raw(), 2)
    }
    pub fn with_preferences(
        uri: impl AsRef<[u8]>,
        id: &RtpHeaderExtensionId,
        encrypt: bool,
        direction: RtpTransceiverDirectionValue,
    ) -> Self {
        Self::construct(uri.as_ref(), id, encrypt, direction.raw(), 3)
    }
    /// Equivalent to the SDK deprecated integer convenience overload.
    pub fn with_raw_id(uri: impl AsRef<[u8]>, id: i32) -> Self {
        Self::with_id(uri, &RtpHeaderExtensionId::new(id))
    }
    pub fn with_raw_id_and_direction(
        uri: impl AsRef<[u8]>,
        id: i32,
        direction: RtpTransceiverDirectionValue,
    ) -> Self {
        Self::with_direction(uri, &RtpHeaderExtensionId::new(id), direction)
    }
    pub fn with_raw_preferences(
        uri: impl AsRef<[u8]>,
        id: i32,
        encrypt: bool,
        direction: RtpTransceiverDirectionValue,
    ) -> Self {
        Self::with_preferences(uri, &RtpHeaderExtensionId::new(id), encrypt, direction)
    }
    fn construct(
        uri: &[u8],
        id: &RtpHeaderExtensionId,
        encrypt: bool,
        direction: i32,
        constructor: u8,
    ) -> Self {
        Self::from_native(ffi::rtp_header_capability_new(
            uri,
            id.native(),
            encrypt,
            direction,
            constructor,
        ))
    }
    pub fn uri_bytes(&self) -> Vec<u8> {
        ffi::rtp_header_capability_fields(self.native()).uri_bytes
    }
    pub fn set_uri(&mut self, uri: impl AsRef<[u8]>) {
        ffi::rtp_header_capability_set_uri(self.native.pin_mut(), uri.as_ref());
    }
    pub fn preferred_id(&self) -> Option<RtpHeaderExtensionId> {
        let value = ffi::rtp_header_capability_id(self.native());
        (!value.is_null()).then(|| RtpHeaderExtensionId::from_native(value))
    }
    pub fn set_preferred_id(&mut self, id: Option<&RtpHeaderExtensionId>) {
        if let Some(id) = id {
            ffi::rtp_header_capability_set_id(self.native.pin_mut(), id.native());
        } else {
            ffi::rtp_header_capability_clear_id(self.native.pin_mut());
        }
    }
    pub fn preferred_encrypt(&self) -> bool {
        ffi::rtp_header_capability_fields(self.native()).encrypt
    }
    pub fn set_preferred_encrypt(&mut self, encrypt: bool) {
        ffi::rtp_header_capability_set_encrypt(self.native.pin_mut(), encrypt);
    }
    pub fn direction(&self) -> RtpTransceiverDirectionValue {
        RtpTransceiverDirectionValue::from_raw(self.direction_raw())
    }
    pub fn set_direction(&mut self, direction: RtpTransceiverDirectionValue) {
        self.set_direction_raw(direction.raw());
    }
    /// Exact signed SDK enum metadata, not the closed legacy direction DTO.
    pub fn direction_raw(&self) -> i32 {
        ffi::rtp_header_capability_fields(self.native()).direction
    }
    pub fn set_direction_raw(&mut self, direction: i32) {
        ffi::rtp_header_capability_set_direction(self.native.pin_mut(), direction);
    }
}
impl std::fmt::Debug for RtpHeaderExtensionCapabilityValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpHeaderExtensionCapabilityValue")
            .field("uri_bytes", &self.uri_bytes())
            .field("preferred_id", &self.preferred_id())
            .field("preferred_encrypt", &self.preferred_encrypt())
            .field("direction_raw", &self.direction_raw())
            .finish()
    }
}

impl RtpCapabilities {
    pub fn codecs(&self) -> Vec<RtpCodecCapability> {
        (0..ffi::rtp_capabilities_codecs_len(self.native()))
            .map(|index| {
                RtpCodecCapability::from_native(ffi::rtp_capabilities_codecs_get(
                    self.native(),
                    index,
                ))
            })
            .collect()
    }
    pub fn set_codecs(&mut self, values: &[RtpCodecCapability]) {
        ffi::rtp_capabilities_codecs_clear(self.native.pin_mut());
        for value in values {
            ffi::rtp_capabilities_codecs_add(self.native.pin_mut(), value.native());
        }
    }
    pub fn header_extensions(&self) -> Vec<RtpHeaderExtensionCapabilityValue> {
        (0..ffi::rtp_capabilities_header_extensions_len(self.native()))
            .map(|index| {
                RtpHeaderExtensionCapabilityValue::from_native(
                    ffi::rtp_capabilities_header_extensions_get(self.native(), index),
                )
            })
            .collect()
    }
    pub fn set_header_extensions(&mut self, values: &[RtpHeaderExtensionCapabilityValue]) {
        ffi::rtp_capabilities_header_extensions_clear(self.native.pin_mut());
        for value in values {
            ffi::rtp_capabilities_header_extensions_add(self.native.pin_mut(), value.native());
        }
    }
    pub fn fec(&self) -> Vec<FecMechanism> {
        ffi::rtp_capabilities_fec(self.native())
            .into_iter()
            .map(FecMechanism::from_raw)
            .collect()
    }
    pub fn set_fec(&mut self, values: &[FecMechanism]) {
        let raw: Vec<_> = values.iter().map(|value| value.raw()).collect();
        ffi::rtp_capabilities_set_fec(self.native.pin_mut(), &raw);
    }
}
impl std::fmt::Debug for RtpCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpCapabilities")
            .field("codecs", &self.codecs())
            .field("header_extensions", &self.header_extensions())
            .field("fec", &self.fec())
            .finish()
    }
}
