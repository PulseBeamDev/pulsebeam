//! Complete independently owned SDK RTP parameter values.
use crate::{RtcpParameters, RtpCodecParameters, RtpEncodingParameters, RtpExtensions, ffi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationPreference {
    MaintainFramerateAndResolution,
    MaintainFramerate,
    MaintainResolution,
    Balanced,
    Other(i32),
}
impl DegradationPreference {
    /// SDK DISABLED alias has exactly the same native value as the modern name.
    pub const DISABLED: Self = Self::MaintainFramerateAndResolution;
    pub fn to_native(self) -> i32 {
        match self {
            Self::MaintainFramerateAndResolution => 0,
            Self::MaintainFramerate => 1,
            Self::MaintainResolution => 2,
            Self::Balanced => 3,
            Self::Other(raw) => raw,
        }
    }
    pub fn from_native(raw: i32) -> Self {
        match raw {
            0 => Self::MaintainFramerateAndResolution,
            1 => Self::MaintainFramerate,
            2 => Self::MaintainResolution,
            3 => Self::Balanced,
            raw => Self::Other(raw),
        }
    }
    /// Calls SDK DegradationPreferenceToString, guarding its fatal default.
    /// This does not restrict storing unknown representable metadata values.
    pub fn native_string_bytes(self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_degradation_preference_string(self.to_native(), &mut valid);
        valid.then_some(bytes)
    }
}

/// Owns the complete actual SDK RtpParameters value. All child accessors make
/// independent SDK copies; setter methods assign them back to this value.
/// No sender/factory origin is invented for metadata. SDK transaction identity
/// is preserved as bytes, but SDK equality intentionally excludes it.
pub struct RtpParameters {
    native: cxx::UniquePtr<ffi::NativeRtpParameters>,
}
// SAFETY: SDK value contains only owned pure metadata values, vectors and
// strings. Shared reads do not mutate/cache; mutation needs exclusive access.
unsafe impl Send for RtpParameters {}
unsafe impl Sync for RtpParameters {}
impl RtpParameters {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpParameters>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpParameters {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiRtpParametersFields {
        ffi::rtp_parameters_fields(self.native())
    }
    pub fn transaction_id_bytes(&self) -> Vec<u8> {
        self.fields().transaction_id_bytes
    }
    pub fn set_transaction_id(&mut self, bytes: impl AsRef<[u8]>) {
        ffi::rtp_parameters_set_transaction_id(self.native.pin_mut(), bytes.as_ref());
    }
    pub fn mid_bytes(&self) -> Vec<u8> {
        self.fields().mid_bytes
    }
    pub fn set_mid(&mut self, bytes: impl AsRef<[u8]>) {
        ffi::rtp_parameters_set_mid(self.native.pin_mut(), bytes.as_ref());
    }
    pub fn degradation_preference(&self) -> Option<DegradationPreference> {
        let fields = self.fields();
        fields
            .has_degradation_preference
            .then(|| DegradationPreference::from_native(fields.degradation_preference))
    }
    pub fn set_degradation_preference(&mut self, value: Option<DegradationPreference>) {
        ffi::rtp_parameters_set_degradation_preference(
            self.native.pin_mut(),
            value.is_some(),
            value.map(DegradationPreference::to_native).unwrap_or(0),
        );
    }
    pub fn rtcp(&self) -> RtcpParameters {
        RtcpParameters::from_native(ffi::rtp_parameters_rtcp(self.native()))
    }
    pub fn set_rtcp(&mut self, value: &RtcpParameters) {
        ffi::rtp_parameters_set_rtcp(self.native.pin_mut(), value.native());
    }
    pub fn header_extensions(&self) -> RtpExtensions {
        RtpExtensions::from_native(ffi::rtp_parameters_header_extensions(self.native()))
    }
    pub fn set_header_extensions(&mut self, values: &RtpExtensions) {
        ffi::rtp_parameters_set_header_extensions(self.native.pin_mut(), values.native());
    }
    pub fn codecs(&self) -> Vec<RtpCodecParameters> {
        (0..ffi::rtp_parameters_codecs_len(self.native()))
            .map(|index| {
                RtpCodecParameters::from_native(ffi::rtp_parameters_codecs_get(
                    self.native(),
                    index,
                ))
            })
            .collect()
    }
    pub fn set_codecs(&mut self, values: &[RtpCodecParameters]) {
        ffi::rtp_parameters_codecs_clear(self.native.pin_mut());
        for value in values {
            ffi::rtp_parameters_codecs_add(self.native.pin_mut(), value.native());
        }
    }
    pub fn encodings(&self) -> Vec<RtpEncodingParameters> {
        (0..ffi::rtp_parameters_encodings_len(self.native()))
            .map(|index| {
                RtpEncodingParameters::from_native(ffi::rtp_parameters_encodings_get(
                    self.native(),
                    index,
                ))
            })
            .collect()
    }
    pub fn set_encodings(&mut self, values: &[RtpEncodingParameters]) {
        ffi::rtp_parameters_encodings_clear(self.native.pin_mut());
        for value in values {
            ffi::rtp_parameters_encodings_add(self.native.pin_mut(), value.native());
        }
    }
    pub fn is_mixed_codec(&self) -> bool {
        ffi::rtp_parameters_is_mixed_codec(self.native())
    }
    /// SDK AbslStringify, with consumption guards for invalid MIME kinds and
    /// degradation preferences. Rejection leaves the metadata unchanged.
    pub fn native_string_bytes(&self) -> Option<Vec<u8>> {
        let mut valid = false;
        let bytes = ffi::rtp_parameters_string(self.native(), &mut valid);
        valid.then_some(bytes)
    }
}
impl Default for RtpParameters {
    fn default() -> Self {
        Self::from_native(ffi::rtp_parameters_default())
    }
}
impl Clone for RtpParameters {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_parameters_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_parameters_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpParameters {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_parameters_equal(self.native(), other.native())
    }
    #[allow(
        clippy::partialeq_ne_impl,
        reason = "Dispatch the independent native inequality operator."
    )]
    fn ne(&self, other: &Self) -> bool {
        ffi::rtp_parameters_not_equal(self.native(), other.native())
    }
}
// No Eq: SDK encoding comparison includes unconstrained floating metadata.
impl std::fmt::Debug for RtpParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpParameters")
            .field("transaction_id_bytes", &self.transaction_id_bytes())
            .field("mid_bytes", &self.mid_bytes())
            .field("codecs", &self.codecs())
            .field("header_extensions", &self.header_extensions())
            .field("encodings", &self.encodings())
            .field("rtcp", &self.rtcp())
            .field("degradation_preference", &self.degradation_preference())
            .finish()
    }
}
