use crate::{ScalabilityMode, ffi};
use std::{collections::BTreeMap, fmt};

/// Complete byte-preserving snapshot of the native SDP video format. Codec
/// identity and exact value equality deliberately have different semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SdpVideoFormatFields {
    pub name: Vec<u8>,
    pub parameters: BTreeMap<Vec<u8>, Vec<u8>>,
    pub scalability_modes: Vec<ScalabilityMode>,
    pub packetization: Option<Vec<u8>>,
    pub tx_mode: Option<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SdpVideoFormatError {
    /// Only reached native naming/logging rejects unknown mode metadata.
    UnsafeScalabilityModeFormatting,
    /// Fuzzy matching increments a native signed int for equal parameters.
    MatchingParameterCountOverflow,
}
impl fmt::Display for SdpVideoFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsafeScalabilityModeFormatting => {
                "native SDP format string consumption asserts on an unnamed scalability mode"
            }
            Self::MatchingParameterCountOverflow => {
                "native SDP fuzzy matching would overflow its signed parameter counter"
            }
        })
    }
}
impl std::error::Error for SdpVideoFormatError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WellKnownSdpVideoFormat {
    Vp8,
    H264,
    H265,
    Vp9Profile0,
    Vp9Profile1,
    Vp9Profile2,
    Vp9Profile3,
    Av1Profile0,
    Av1Profile1,
}
impl WellKnownSdpVideoFormat {
    fn raw(self) -> u8 {
        match self {
            Self::Vp8 => 0,
            Self::H264 => 1,
            Self::H265 => 2,
            Self::Vp9Profile0 => 3,
            Self::Vp9Profile1 => 4,
            Self::Vp9Profile2 => 5,
            Self::Vp9Profile3 => 6,
            Self::Av1Profile0 => 7,
            Self::Av1Profile1 => 8,
        }
    }
}
fn parameters(input: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<ffi::FfiRtpCodecParameter> {
    input
        .iter()
        .map(|(key, value)| ffi::FfiRtpCodecParameter {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}
pub(crate) fn sdp_video_formats_from_list(
    list: cxx::UniquePtr<ffi::NativeSdpVideoFormatList>,
) -> Vec<SdpVideoFormat> {
    let list = list.as_ref().expect("owned native format list");
    assert!(
        ffi::sdp_video_format_list_valid(list),
        "factory callback failed; use try_supported_format_values for a fallible query"
    );
    (0..ffi::sdp_video_format_list_len(list))
        .map(|index| SdpVideoFormat::from_native(ffi::sdp_video_format_list_get(list, index)))
        .collect()
}
fn modes(input: &[ScalabilityMode]) -> Vec<u8> {
    input.iter().map(|mode| mode.raw()).collect()
}
impl SdpVideoFormatFields {
    fn native(self) -> ffi::FfiSdpVideoFormatFields {
        ffi::FfiSdpVideoFormatFields {
            name: self.name,
            parameters: parameters(&self.parameters),
            scalability_modes: modes(&self.scalability_modes),
            has_packetization: self.packetization.is_some(),
            packetization: self.packetization.unwrap_or_default(),
            has_tx_mode: self.tx_mode.is_some(),
            tx_mode: self.tx_mode.unwrap_or_default(),
        }
    }
}

/// Actual owning SDK SdpVideoFormat, not the legacy codec DTO. There is no
/// default native constructor; `new` calls its explicit name constructor.
pub struct SdpVideoFormat {
    pub(crate) native: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
}
// SAFETY: SDK value owns byte strings, map, enum vector and optional strings.
// Shared methods read/copy; mutation is exclusive. No engine/queue/callback.
unsafe impl Send for SdpVideoFormat {}
unsafe impl Sync for SdpVideoFormat {}
impl SdpVideoFormat {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeSdpVideoFormat>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeSdpVideoFormat {
        self.native.as_ref().unwrap()
    }
    pub fn new(name: impl AsRef<[u8]>) -> Self {
        Self::from_native(ffi::sdp_video_format_new(name.as_ref()))
    }
    pub fn with_parameters(name: impl AsRef<[u8]>, values: &BTreeMap<Vec<u8>, Vec<u8>>) -> Self {
        Self::from_native(ffi::sdp_video_format_with_parameters(
            name.as_ref(),
            &parameters(values),
            &[],
            false,
        ))
    }
    pub fn with_parameters_and_modes(
        name: impl AsRef<[u8]>,
        values: &BTreeMap<Vec<u8>, Vec<u8>>,
        scalability_modes: &[ScalabilityMode],
    ) -> Self {
        Self::from_native(ffi::sdp_video_format_with_parameters(
            name.as_ref(),
            &parameters(values),
            &modes(scalability_modes),
            true,
        ))
    }
    /// SDK copy-with-modes overload retains both optional protocol fields.
    pub fn with_scalability_modes(&self, scalability_modes: &[ScalabilityMode]) -> Self {
        Self::from_native(ffi::sdp_video_format_with_modes(
            self.native(),
            &modes(scalability_modes),
        ))
    }
    pub fn well_known(format: WellKnownSdpVideoFormat) -> Self {
        Self::from_native(ffi::sdp_video_format_named(format.raw()))
    }
    pub fn fields(&self) -> SdpVideoFormatFields {
        let fields = ffi::sdp_video_format_fields(self.native());
        SdpVideoFormatFields {
            name: fields.name,
            parameters: fields
                .parameters
                .into_iter()
                .map(|pair| (pair.key, pair.value))
                .collect(),
            scalability_modes: fields
                .scalability_modes
                .into_iter()
                .map(ScalabilityMode::from_raw)
                .collect(),
            packetization: fields.has_packetization.then_some(fields.packetization),
            tx_mode: fields.has_tx_mode.then_some(fields.tx_mode),
        }
    }
    /// Complete field replacement preserves unnamed mode metadata, empty
    /// present optionals and arbitrary byte strings without normalization.
    pub fn set_fields(&mut self, fields: SdpVideoFormatFields) {
        ffi::sdp_video_format_set_fields(self.native.pin_mut(), fields.native());
    }
    pub fn assign_from(&mut self, source: &Self) {
        ffi::sdp_video_format_assign(self.native.pin_mut(), source.native());
    }
    pub fn move_assign(&mut self, source: Self) {
        ffi::sdp_video_format_move_assign(self.native.pin_mut(), source.native);
    }
    pub fn sdk_move(self) -> Self {
        Self::from_native(ffi::sdp_video_format_move(self.native))
    }
    pub fn is_same_codec(&self, other: &Self) -> bool {
        ffi::sdp_video_format_same_codec(self.native(), other.native())
    }
    fn list(formats: &[Self]) -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
        let mut list = ffi::sdp_video_format_list_new();
        for format in formats {
            ffi::sdp_video_format_list_add(list.pin_mut(), format.native());
        }
        list
    }
    pub fn is_codec_in_list(&self, formats: &[Self]) -> bool {
        ffi::sdp_video_format_in_list(self.native(), Self::list(formats).as_ref().unwrap())
    }
    /// Native fuzzy matching returns the complete selected supported value and
    /// preserves first-wins ties, including zero parameter matches. String guards
    /// apply only when the SDK's exact logging branch reaches ToString.
    pub fn fuzzy_match(
        &self,
        supported_formats: &[Self],
    ) -> Result<Option<Self>, SdpVideoFormatError> {
        let list = Self::list(supported_formats);
        let mut status = 0;
        let result =
            ffi::sdp_video_format_fuzzy_match(list.as_ref().unwrap(), self.native(), &mut status);
        match status {
            0 => Ok((!result.is_null()).then(|| Self::from_native(result))),
            1 => Err(SdpVideoFormatError::UnsafeScalabilityModeFormatting),
            2 => Err(SdpVideoFormatError::MatchingParameterCountOverflow),
            _ => unreachable!("closed SDP format bridge status"),
        }
    }
    pub fn native_string_bytes(&self) -> Result<Vec<u8>, SdpVideoFormatError> {
        let mut valid = false;
        let bytes = ffi::sdp_video_format_string(self.native(), &mut valid);
        if valid {
            Ok(bytes)
        } else {
            Err(SdpVideoFormatError::UnsafeScalabilityModeFormatting)
        }
    }
}
impl Clone for SdpVideoFormat {
    fn clone(&self) -> Self {
        Self::from_native(ffi::sdp_video_format_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        self.assign_from(source);
    }
}
impl PartialEq for SdpVideoFormat {
    fn eq(&self, other: &Self) -> bool {
        ffi::sdp_video_format_equal(self.native(), other.native())
    }
}
impl Eq for SdpVideoFormat {}
impl fmt::Debug for SdpVideoFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SdpVideoFormat")
            .field(&self.fields())
            .finish()
    }
}

/// Actual SDK two-int RenderResolution value, including negative/zero metadata.
pub struct RenderResolution {
    pub(crate) native: cxx::UniquePtr<ffi::NativeRenderResolution>,
}
// SAFETY: independent SDK pair of integers, shared reads/exclusive assignment.
unsafe impl Send for RenderResolution {}
unsafe impl Sync for RenderResolution {}
impl RenderResolution {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRenderResolution>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRenderResolution {
        self.native.as_ref().unwrap()
    }
    pub fn new(width: i32, height: i32) -> Self {
        Self::from_native(ffi::render_resolution_new(width, height))
    }
    pub fn width(&self) -> i32 {
        ffi::render_resolution_width(self.native())
    }
    pub fn height(&self) -> i32 {
        ffi::render_resolution_height(self.native())
    }
    pub fn valid(&self) -> bool {
        ffi::render_resolution_valid(self.native())
    }
    pub fn assign_from(&mut self, source: &Self) {
        ffi::render_resolution_assign(self.native.pin_mut(), source.native());
    }
}
impl Default for RenderResolution {
    fn default() -> Self {
        Self::from_native(ffi::render_resolution_default())
    }
}
impl Clone for RenderResolution {
    fn clone(&self) -> Self {
        Self::from_native(ffi::render_resolution_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        self.assign_from(source);
    }
}
impl PartialEq for RenderResolution {
    fn eq(&self, other: &Self) -> bool {
        ffi::render_resolution_equal(self.native(), other.native())
    }
}
impl Eq for RenderResolution {}
impl fmt::Debug for RenderResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderResolution")
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}
