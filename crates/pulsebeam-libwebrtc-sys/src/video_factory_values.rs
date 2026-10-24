//! Complete owning inputs for video factory discovery and support queries.
use crate::{
    CodecParameter, CodecSupport, Resolution, ScalabilityMode, SdpVideoFormat, VideoCodecFormat,
    VideoDecoderFactoryHandle, VideoEncoderFactoryHandle, VideoResolution, VideoScalabilityMode,
    ffi,
};
use std::{
    fmt,
    sync::{Arc, Mutex},
};

/// Actual owning inputs supplied to the native video factory Create callback.
/// Environment clones retain native dependencies and the per-installation
/// Rust-only randomness lease. Neither format nor environment is substituted.
#[derive(Clone)]
pub struct VideoCodecCreationContext {
    pub environment: crate::Environment,
    pub format: SdpVideoFormat,
}
/// Complete byte-aware caller factory contract. Creation receives the actual
/// SDK environment; products must own any callback/application dependencies.
/// Calls can run concurrently on native engine threads. Never wait for or
/// reenter the owning peer/actor from a callback or destructor.
pub trait VideoEncoderFactoryProvider: Send + Sync + 'static {
    fn query_support_legacy(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.query_support(format, mode, None)
    }
    fn encoder_selector(&self) -> Option<crate::OwnedVideoEncoderSelector> {
        None
    }
    fn supported_formats(&self) -> Vec<SdpVideoFormat>;
    fn implementations(&self) -> Vec<SdpVideoFormat> {
        self.supported_formats()
    }
    fn query_support(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError>;
    fn create(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn crate::VideoEncoder>, crate::CodecError>;
}
pub trait VideoDecoderFactoryProvider: Send + Sync + 'static {
    fn query_support_legacy(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.query_support(format, reference_scaling, None)
    }
    fn supported_formats(&self) -> Vec<SdpVideoFormat>;
    fn query_support(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError>;
    fn create(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn crate::VideoDecoder>, crate::CodecError>;
}
struct OwnedEncoderFactory<T>(T);
impl<T: VideoEncoderFactoryProvider> crate::VideoEncoderFactory for OwnedEncoderFactory<T> {
    fn query_support_legacy_value(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.query_support_legacy(format, mode)
    }
    fn encoder_selector(&self) -> Option<crate::OwnedVideoEncoderSelector> {
        self.0.encoder_selector()
    }
    fn supported_format_values(&self) -> Vec<SdpVideoFormat> {
        self.0.supported_formats()
    }
    fn implementation_values(&self) -> Vec<SdpVideoFormat> {
        self.0.implementations()
    }
    fn query_support_value(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.query_support(format, mode, resolution)
    }
    fn create_with_context(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn crate::VideoEncoder>, crate::CodecError> {
        self.0.create(context)
    }
    // These legacy methods are not entered by the native factory adapter. Their
    // signatures cannot carry complete bytes or a creation Environment.
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        unreachable!("owned factory uses native full-value dispatch")
    }
    fn create(
        &self,
        _: &VideoCodecFormat,
    ) -> Result<Box<dyn crate::VideoEncoder>, crate::CodecError> {
        unreachable!("owned factory uses creation-context dispatch")
    }
}
struct OwnedDecoderFactory<T>(T);
impl<T: VideoDecoderFactoryProvider> crate::VideoDecoderFactory for OwnedDecoderFactory<T> {
    fn query_support_legacy_value(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.query_support_legacy(format, reference_scaling)
    }
    fn supported_format_values(&self) -> Vec<SdpVideoFormat> {
        self.0.supported_formats()
    }
    fn query_support_value(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.query_support(format, reference_scaling, resolution)
    }
    fn create_with_context(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn crate::VideoDecoder>, crate::CodecError> {
        self.0.create(context)
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        unreachable!("owned factory uses native full-value dispatch")
    }
    fn create(
        &self,
        _: &VideoCodecFormat,
    ) -> Result<Box<dyn crate::VideoDecoder>, crate::CodecError> {
        unreachable!("owned factory uses creation-context dispatch")
    }
}
pub(crate) struct RustVideoFactoryEnvironment(pub(crate) crate::Environment);
pub(crate) fn video_factory_environment_clone(
    source: &RustVideoFactoryEnvironment,
) -> Box<RustVideoFactoryEnvironment> {
    Box::new(RustVideoFactoryEnvironment(source.0.clone()))
}
pub(crate) fn video_factory_environment_native(
    source: &RustVideoFactoryEnvironment,
) -> cxx::UniquePtr<ffi::NativeEnvironment> {
    ffi::clone_environment(source.0.native())
}
pub(crate) fn video_factory_environment_from_sdk(
    source: cxx::UniquePtr<ffi::NativeEnvironment>,
) -> Box<RustVideoFactoryEnvironment> {
    Box::new(RustVideoFactoryEnvironment(crate::Environment::from_sdk(
        source, None,
    )))
}

/// Binding conversion/failure, distinct from a native unsupported answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFactoryQueryError {
    LegacyFormatBytes,
    LegacyOptionalFormatFields,
    LegacyUnknownScalabilityMode,
    LegacyModeBytes,
    LegacySignedResolution,
    ProviderPanicked,
    UnsafeNativeFormatFormatting,
    NativeParameterCountOverflow,
    /// A codec info callback or its actual SDK consumption handoff failed.
    CodecInfoFailed,
    CodecFeedbackFailed,
    CodecCreationFailed,
}
impl VideoFactoryQueryError {
    pub(crate) fn raw(self) -> u8 {
        match self {
            Self::LegacyFormatBytes => 1,
            Self::LegacyOptionalFormatFields => 2,
            Self::LegacyUnknownScalabilityMode => 3,
            Self::LegacyModeBytes => 4,
            Self::LegacySignedResolution => 5,
            Self::ProviderPanicked => 6,
            Self::UnsafeNativeFormatFormatting => 7,
            Self::NativeParameterCountOverflow => 8,
            Self::CodecInfoFailed => 9,
            Self::CodecFeedbackFailed => 10,
            Self::CodecCreationFailed => 11,
        }
    }
    pub(crate) fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => None,
            1 => Some(Self::LegacyFormatBytes),
            2 => Some(Self::LegacyOptionalFormatFields),
            3 => Some(Self::LegacyUnknownScalabilityMode),
            4 => Some(Self::LegacyModeBytes),
            5 => Some(Self::LegacySignedResolution),
            6 => Some(Self::ProviderPanicked),
            7 => Some(Self::UnsafeNativeFormatFormatting),
            8 => Some(Self::NativeParameterCountOverflow),
            9 => Some(Self::CodecInfoFailed),
            10 => Some(Self::CodecFeedbackFailed),
            11 => Some(Self::CodecCreationFailed),
            _ => unreachable!("closed video factory query status"),
        }
    }
}
impl fmt::Display for VideoFactoryQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::LegacyFormatBytes => {
                "legacy video provider cannot represent non-UTF-8 format bytes"
            }
            Self::LegacyOptionalFormatFields => {
                "legacy video provider cannot represent packetization or tx_mode fields"
            }
            Self::LegacyUnknownScalabilityMode => {
                "legacy video provider cannot represent unnamed scalability metadata"
            }
            Self::LegacyModeBytes => {
                "legacy video provider cannot represent non-UTF-8 query mode bytes"
            }
            Self::LegacySignedResolution => {
                "legacy video provider cannot represent negative query resolution"
            }
            Self::ProviderPanicked => "video factory provider panicked",
            Self::UnsafeNativeFormatFormatting => {
                "native factory query may assert while naming unknown format scalability metadata"
            }
            Self::NativeParameterCountOverflow => {
                "native factory fuzzy matching may overflow its signed parameter counter"
            }
            Self::CodecInfoFailed => "codec information callback or SDK handoff failed",
            Self::CodecFeedbackFailed => "codec feedback callback or SDK handoff failed",
            Self::CodecCreationFailed => "caller codec creation failed",
        })
    }
}
impl std::error::Error for VideoFactoryQueryError {}

/// First binding query, codec-info/feedback or destruction failure. This observer retains no native
/// factory or codec. Native unsupported queries are not binding failures.
#[derive(Clone, Default)]
pub struct VideoFactoryFailureObserver(Arc<Mutex<Option<VideoFactoryQueryError>>>);
impl VideoFactoryFailureObserver {
    pub fn failure(&self) -> Option<VideoFactoryQueryError> {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }
    pub(crate) fn report(&self, error: VideoFactoryQueryError) {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_or_insert(error);
    }
}

impl From<&VideoCodecFormat> for SdpVideoFormat {
    fn from(format: &VideoCodecFormat) -> Self {
        let modes = ScalabilityMode::all();
        let mut result = SdpVideoFormat::new(format.name.as_bytes());
        let mut fields = result.fields();
        // Preserve the old native std::map::emplace duplicate semantics:
        // the first declaration wins, not Rust collect's last declaration.
        for parameter in &format.parameters {
            fields
                .parameters
                .entry(parameter.key.as_bytes().to_vec())
                .or_insert_with(|| parameter.value.as_bytes().to_vec());
        }
        fields.scalability_modes = format
            .scalability_modes
            .iter()
            .map(|mode| {
                *modes
                    .iter()
                    .find(|candidate| {
                        candidate.native_name_bytes().as_deref() == Some(mode.as_str().as_bytes())
                    })
                    .expect("validated legacy scalability mode")
            })
            .collect();
        result.set_fields(fields);
        result
    }
}
impl TryFrom<&SdpVideoFormat> for VideoCodecFormat {
    type Error = VideoFactoryQueryError;
    fn try_from(format: &SdpVideoFormat) -> Result<Self, Self::Error> {
        let fields = format.fields();
        if fields.packetization.is_some() || fields.tx_mode.is_some() {
            return Err(VideoFactoryQueryError::LegacyOptionalFormatFields);
        }
        let text =
            |bytes| String::from_utf8(bytes).map_err(|_| VideoFactoryQueryError::LegacyFormatBytes);
        let name = text(fields.name)?;
        let parameters = fields
            .parameters
            .into_iter()
            .map(|(key, value)| {
                Ok(CodecParameter {
                    key: text(key)?,
                    value: text(value)?,
                })
            })
            .collect::<Result<_, Self::Error>>()?;
        let scalability_modes = fields
            .scalability_modes
            .into_iter()
            .map(|mode| {
                let bytes = mode
                    .native_name_bytes()
                    .ok_or(VideoFactoryQueryError::LegacyUnknownScalabilityMode)?;
                Ok(VideoScalabilityMode::from_native(
                    String::from_utf8(bytes).expect("SDK mode names are ASCII"),
                ))
            })
            .collect::<Result<_, Self::Error>>()?;
        Ok(Self {
            name,
            parameters,
            scalability_modes,
        })
    }
}
pub(crate) fn legacy_resolution(
    resolution: Option<&Resolution>,
) -> Result<Option<VideoResolution>, VideoFactoryQueryError> {
    resolution
        .map(|resolution| {
            Ok(VideoResolution {
                width: u32::try_from(resolution.width())
                    .map_err(|_| VideoFactoryQueryError::LegacySignedResolution)?,
                height: u32::try_from(resolution.height())
                    .map_err(|_| VideoFactoryQueryError::LegacySignedResolution)?,
            })
        })
        .transpose()
}
pub(crate) fn native_failed_formats() -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
    let mut value = ffi::sdp_video_format_list_new();
    ffi::sdp_video_format_list_set_valid(value.pin_mut(), false);
    value
}
pub(crate) fn checked_formats(
    value: cxx::UniquePtr<ffi::NativeSdpVideoFormatList>,
) -> Result<Vec<SdpVideoFormat>, VideoFactoryQueryError> {
    if value
        .as_ref()
        .is_none_or(|v| !ffi::sdp_video_format_list_valid(v))
    {
        return Err(VideoFactoryQueryError::ProviderPanicked);
    }
    Ok(crate::video_codec_values::sdp_video_formats_from_list(
        value,
    ))
}
pub(crate) fn native_formats(
    formats: Vec<SdpVideoFormat>,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
    let mut list = ffi::sdp_video_format_list_new();
    for format in formats {
        ffi::sdp_video_format_list_add(list.pin_mut(), format.native());
    }
    list
}
pub(crate) fn support_result(
    result: ffi::FfiVideoFactorySupport,
) -> Result<CodecSupport, VideoFactoryQueryError> {
    if let Some(error) = VideoFactoryQueryError::from_raw(result.error) {
        return Err(error);
    }
    Ok(CodecSupport {
        supported: result.support.supported,
        power_efficient: result.support.power_efficient,
    })
}
pub(crate) fn ffi_support(
    result: Result<CodecSupport, VideoFactoryQueryError>,
) -> ffi::FfiVideoFactorySupport {
    match result {
        Ok(support) => ffi::FfiVideoFactorySupport {
            support: ffi::FfiCodecSupport {
                supported: support.supported,
                power_efficient: support.power_efficient,
            },
            error: 0,
        },
        Err(error) => ffi::FfiVideoFactorySupport {
            support: ffi::FfiCodecSupport {
                supported: false,
                power_efficient: false,
            },
            error: error.raw(),
        },
    }
}
impl VideoEncoderFactoryHandle {
    pub fn try_supported_format_values(
        &self,
    ) -> Result<Vec<SdpVideoFormat>, VideoFactoryQueryError> {
        checked_formats(ffi::video_encoder_formats_value(self.native()))
    }
    pub fn try_implementation_values(&self) -> Result<Vec<SdpVideoFormat>, VideoFactoryQueryError> {
        checked_formats(ffi::video_encoder_implementations_value(self.native()))
    }
    /// Actual selected SDK GetEncoderSelector, preserving native null.
    pub fn encoder_selector(
        &self,
    ) -> Result<Option<crate::OwnedVideoEncoderSelector>, VideoFactoryQueryError> {
        let mut valid = false;
        let native = ffi::video_encoder_factory_selector(self.native(), &mut valid);
        if valid {
            Ok(crate::OwnedVideoEncoderSelector::from_native(native))
        } else {
            Err(VideoFactoryQueryError::ProviderPanicked)
        }
    }
    pub fn query_support_legacy_value(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        // SAFETY: no resolution pointer is supplied to this synchronous overload.
        support_result(unsafe {
            ffi::video_encoder_query_value(
                self.native(),
                format.native(),
                mode.is_some(),
                mode.unwrap_or_default(),
                std::ptr::null(),
                true,
            )
        })
    }
    pub fn from_provider(
        provider: impl VideoEncoderFactoryProvider,
    ) -> Result<Self, crate::CodecError> {
        Self::new(OwnedEncoderFactory(provider))
    }
    /// Actual selected SDK query with complete owning format, optional mode
    /// bytes and optional signed resolution. None differs from Some(empty/zero).
    pub fn query_support_value(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        // SAFETY: optional pointer refers to an owning value held through this
        // synchronous call. Native code neither stores nor mutates the pointer.
        let result = unsafe {
            ffi::video_encoder_query_value(
                self.native(),
                format.native(),
                mode.is_some(),
                mode.unwrap_or_default(),
                resolution.map_or(std::ptr::null(), |value| value.native()),
                false,
            )
        };
        support_result(result)
    }
    pub fn try_supported_formats(&self) -> Result<Vec<VideoCodecFormat>, VideoFactoryQueryError> {
        self.supported_format_values()
            .iter()
            .map(VideoCodecFormat::try_from)
            .collect()
    }
}
impl VideoDecoderFactoryHandle {
    pub fn try_supported_format_values(
        &self,
    ) -> Result<Vec<SdpVideoFormat>, VideoFactoryQueryError> {
        checked_formats(ffi::video_decoder_formats_value(self.native()))
    }
    pub fn query_support_legacy_value(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        // SAFETY: no resolution pointer is supplied to this synchronous overload.
        support_result(unsafe {
            ffi::video_decoder_query_value(
                self.native(),
                format.native(),
                reference_scaling,
                std::ptr::null(),
                true,
            )
        })
    }
    pub fn from_provider(
        provider: impl VideoDecoderFactoryProvider,
    ) -> Result<Self, crate::CodecError> {
        Self::new(OwnedDecoderFactory(provider))
    }
    pub fn query_support_value(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        // SAFETY: see encoder query; resolution is a synchronous owning loan.
        let result = unsafe {
            ffi::video_decoder_query_value(
                self.native(),
                format.native(),
                reference_scaling,
                resolution.map_or(std::ptr::null(), |value| value.native()),
                false,
            )
        };
        support_result(result)
    }
    pub fn try_supported_formats(&self) -> Result<Vec<VideoCodecFormat>, VideoFactoryQueryError> {
        self.supported_format_values()
            .iter()
            .map(VideoCodecFormat::try_from)
            .collect()
    }
}
