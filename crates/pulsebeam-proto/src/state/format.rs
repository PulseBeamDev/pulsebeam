use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
};

use super::{CodecFormat, CodecParameter, validation::ValidationError};

// These are adapter interpretations, not the public wire's set of codecs.
pub mod codec_format {
    use super::{H264Parameters, OpusParameters, Vp8Parameters, Vp9Parameters};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Codec {
        Opus(OpusParameters),
        H264(H264Parameters),
        Vp8(Vp8Parameters),
        Vp9(Vp9Parameters),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpusParameters {
    pub stereo: Option<bool>,
    pub source_stereo: Option<bool>,
    pub in_band_fec: Option<bool>,
    pub dtx: Option<bool>,
    pub constant_bitrate: Option<bool>,
    pub max_average_bitrate_bps: Option<u32>,
    pub max_playback_rate_hz: Option<u32>,
    pub max_capture_rate_hz: Option<u32>,
    pub min_packet_time_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct H264Parameters {
    pub profile_level_id: Option<u32>,
    pub level_asymmetry_allowed: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vp8Parameters {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vp9Parameters {
    pub profile_id: Option<u32>,
}

impl CodecFormat {
    /// Format envelope for a stock single-primary native QUIC media source.
    /// A peer must still negotiate the codec and any required extensions.
    pub fn native_quic_baseline(mime: &str) -> Option<Self> {
        use codec_format::Codec;
        Some(match mime {
            "audio/opus" => Codec::Opus(OpusParameters {
                min_packet_time_ms: Some(10),
                in_band_fec: Some(true),
                ..Default::default()
            })
            .into(),
            "video/h264" => Codec::H264(H264Parameters {
                profile_level_id: Some(0x42e01f),
                level_asymmetry_allowed: Some(true),
            })
            .into(),
            "video/vp8" => Codec::Vp8(Vp8Parameters {}).into(),
            "video/vp9" => Codec::Vp9(Vp9Parameters::default()).into(),
            "video/av1" => Self {
                mime_type: mime.into(),
                clock_rate: 90_000,
                channels: None,
                parameters: STOCK_AV1_PARAMETERS
                    .iter()
                    .map(|&(name, value)| CodecParameter {
                        name: name.into(),
                        value: value.into(),
                    })
                    .collect(),
            },
            _ => return None,
        })
    }

    pub fn known_codec(&self) -> Option<codec_format::Codec> {
        self.parse_known_codec().ok().flatten()
    }

    pub fn parameter_map(&self) -> BTreeMap<String, String> {
        self.parameters
            .iter()
            .map(|parameter| (parameter.name.clone(), parameter.value.clone()))
            .collect()
    }

    pub fn parse_known_codec(&self) -> Result<Option<codec_format::Codec>, ValidationError> {
        use codec_format::Codec;
        let parameters = self.parameter_map();
        let value = |name: &str| parameters.get(name).map(String::as_str);
        let integer = |name: &str| {
            value(name)
                .map(|v| v.parse::<u32>().map_err(|_| ValidationError::Format))
                .transpose()
        };
        let flag = |name: &str| {
            value(name)
                .map(|v| match v {
                    "0" => Ok(false),
                    "1" => Ok(true),
                    _ => Err(ValidationError::Format),
                })
                .transpose()
        };
        Ok(match self.mime_type.as_str() {
            "audio/opus" if self.clock_rate == 48000 && self.channels == Some(2) => {
                Some(Codec::Opus(OpusParameters {
                    stereo: flag("stereo")?,
                    source_stereo: flag("sprop-stereo")?,
                    in_band_fec: flag("useinbandfec")?,
                    dtx: flag("usedtx")?,
                    constant_bitrate: flag("cbr")?,
                    max_average_bitrate_bps: integer("maxaveragebitrate")?,
                    max_playback_rate_hz: integer("maxplaybackrate")?,
                    max_capture_rate_hz: integer("sprop-maxcapturerate")?,
                    min_packet_time_ms: integer("minptime")?,
                }))
            }
            "video/h264"
                if self.clock_rate == 90000
                    && self.channels.is_none()
                    && value("packetization-mode") == Some("1") =>
            {
                // These constraints need decoder/packetizer support beyond the
                // admitted in-band, packetization-mode=1 transport profile.
                if [
                    "sprop-parameter-sets",
                    "max-recv-level",
                    "sprop-level-parameter-sets",
                    "sprop-interleaving-depth",
                    "sprop-deint-buf-req",
                ]
                .iter()
                .any(|name| parameters.contains_key(*name))
                {
                    return Ok(None);
                }
                let profile = value("profile-level-id").unwrap_or("42e01f");
                if profile.len() != 6 || !profile.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(ValidationError::Format);
                }
                Some(Codec::H264(H264Parameters {
                    profile_level_id: Some(
                        u32::from_str_radix(profile, 16).map_err(|_| ValidationError::Format)?,
                    ),
                    level_asymmetry_allowed: flag("level-asymmetry-allowed")?,
                }))
            }
            "video/vp8" if self.clock_rate == 90000 && self.channels.is_none() => {
                Some(Codec::Vp8(Vp8Parameters {}))
            }
            "video/vp9" if self.clock_rate == 90000 && self.channels.is_none() => {
                Some(Codec::Vp9(Vp9Parameters {
                    profile_id: integer("profile-id")?,
                }))
            }
            _ => None,
        })
    }
}

impl From<codec_format::Codec> for CodecFormat {
    fn from(codec: codec_format::Codec) -> Self {
        use codec_format::Codec;
        let mut parameters = BTreeMap::<String, String>::new();
        let (mime_type, clock_rate, channels) = match codec {
            Codec::Opus(opus) => {
                for (name, value) in [
                    ("stereo", opus.stereo),
                    ("sprop-stereo", opus.source_stereo),
                    ("useinbandfec", opus.in_band_fec),
                    ("usedtx", opus.dtx),
                    ("cbr", opus.constant_bitrate),
                ] {
                    if let Some(value) = value {
                        parameters.insert(name.into(), u8::from(value).to_string());
                    }
                }
                for (name, value) in [
                    ("maxaveragebitrate", opus.max_average_bitrate_bps),
                    ("maxplaybackrate", opus.max_playback_rate_hz),
                    ("sprop-maxcapturerate", opus.max_capture_rate_hz),
                    ("minptime", opus.min_packet_time_ms),
                ] {
                    if let Some(value) = value {
                        parameters.insert(name.into(), value.to_string());
                    }
                }
                ("audio/opus", 48000, Some(2))
            }
            Codec::H264(h264) => {
                parameters.insert("packetization-mode".into(), "1".into());
                if let Some(profile) = h264.profile_level_id {
                    parameters.insert("profile-level-id".into(), format!("{profile:06x}"));
                }
                if let Some(value) = h264.level_asymmetry_allowed {
                    parameters.insert(
                        "level-asymmetry-allowed".into(),
                        u8::from(value).to_string(),
                    );
                }
                ("video/h264", 90000, None)
            }
            Codec::Vp8(_) => ("video/vp8", 90000, None),
            Codec::Vp9(vp9) => {
                if let Some(profile) = vp9.profile_id {
                    parameters.insert("profile-id".into(), profile.to_string());
                }
                ("video/vp9", 90000, None)
            }
        };
        Self {
            mime_type: mime_type.into(),
            clock_rate,
            channels,
            parameters: parameters
                .into_iter()
                .map(|(name, value)| CodecParameter { name, value })
                .collect(),
        }
    }
}

macro_rules! features {
    ($name:ident { $($variant:ident => $spelling:literal),* $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),* }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $spelling),* } }
        }
        impl TryFrom<&str> for $name {
            type Error = ValidationError;
            fn try_from(value: &str) -> Result<Self, Self::Error> { match value { $($spelling => Ok(Self::$variant)),*, _ => Err(ValidationError::Capability) } }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self { value.as_str().into() }
        }
    };
}

features!(RtcpMode {
    Unspecified => "",
    Compound => "compound",
    ReducedSize => "reduced-size",
});

features!(RtcpFeedback {
    Unspecified => "",
    Nack => "nack",
    NackPli => "nack pli",
    CcmFir => "ccm fir",
    TransportCc => "transport-cc",
});

features!(RtpHeaderExtensionType {
    RtpHeaderExtensionUnspecified => "",
    RtpHeaderExtensionMid => "urn:ietf:params:rtp-hdrext:sdes:mid",
    RtpHeaderExtensionRid => "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
    RtpHeaderExtensionRepairedRid => "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
    RtpHeaderExtensionTransportCcDraft01 => "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
    RtpHeaderExtensionDependencyDescriptor => "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
    RtpHeaderExtensionAudioLevel => "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
    RtpHeaderExtensionVideoLayersAllocation => "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
});

features!(RtpScalabilityMode {
    Unspecified => "",
    L1t1 => "L1T1",
    L1t2 => "L1T2",
    L1t3 => "L1T3",
    L2t1 => "L2T1",
    L2t1h => "L2T1h",
    L2t2 => "L2T2",
    L2t2h => "L2T2h",
    L2t3 => "L2T3",
    L2t3h => "L2T3h",
    L3t1 => "L3T1",
    L3t1h => "L3T1h",
    L3t2 => "L3T2",
    L3t2h => "L3T2h",
    L3t3 => "L3T3",
    L3t3h => "L3T3h",
    S2t1 => "S2T1",
    S2t1h => "S2T1h",
    S2t2 => "S2T2",
    S2t2h => "S2T2h",
    S2t3 => "S2T3",
    S2t3h => "S2T3h",
    S3t1 => "S3T1",
    S3t1h => "S3T1h",
    S3t2 => "S3T2",
    S3t2h => "S3T2h",
    S3t3 => "S3T3",
    S3t3h => "S3T3h",
    L2t2Key => "L2T2_KEY",
    L2t2KeyShift => "L2T2_KEY_SHIFT",
    L2t3Key => "L2T3_KEY",
    L2t3KeyShift => "L2T3_KEY_SHIFT",
    L3t1Key => "L3T1_KEY",
    L3t2Key => "L3T2_KEY",
    L3t2KeyShift => "L3T2_KEY_SHIFT",
    L3t3Key => "L3T3_KEY",
    L3t3KeyShift => "L3T3_KEY_SHIFT",
});

pub const STOCK_AV1_PARAMETERS: [(&str, &str); 3] =
    [("level-idx", "5"), ("profile", "0"), ("tier", "0")];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialPrediction {
    Independent,
    InterLayer,
    KeyPicturesOnly,
}

/// Mode constraints, not an observed frame dependency graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoMode {
    mode: RtpScalabilityMode,
    spatial_layers: u8,
    temporal_layers: u8,
}

/// A requested layer coordinate, not a dependency-descriptor target index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoDecodeTarget {
    spatial: u8,
    temporal: u8,
}

impl VideoDecodeTarget {
    pub const fn spatial(self) -> u8 {
        self.spatial
    }

    pub const fn temporal(self) -> u8 {
        self.temporal
    }
}

/// Layer coordinates from emitted encoder metadata, not sender settings or
/// evidence of a particular scaling ratio, dependency graph, or cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedLayerEvidence {
    spatial: u8,
    temporal: u8,
}

impl EncodedLayerEvidence {
    pub fn from_emitted_layer_ids(spatial: u8, temporal: u8) -> Result<Self, ValidationError> {
        if spatial >= 3 || temporal >= 3 {
            return Err(ValidationError::Binding);
        }
        Ok(Self { spatial, temporal })
    }

    pub const fn fits(self, requested: VideoMode) -> bool {
        self.spatial < requested.spatial_layers && self.temporal < requested.temporal_layers
    }
}

impl VideoMode {
    /// Interpret a requested mode for a supported RTP format. Generic capability
    /// exchange deliberately keeps its original strings instead.
    pub fn for_format(format: &CodecFormat, requested: &str) -> Result<Self, ValidationError> {
        if super::validation::validate_format(format)? {
            return Err(ValidationError::Format);
        }
        let mode = RtpScalabilityMode::try_from(requested)?;
        let [_, spatial, _, temporal, ..] = mode.as_str().as_bytes() else {
            return Err(ValidationError::Capability);
        };
        let spatial_layers = spatial
            .checked_sub(b'0')
            .ok_or(ValidationError::Capability)?;
        let temporal_layers = temporal
            .checked_sub(b'0')
            .ok_or(ValidationError::Capability)?;
        let supports_scalable = match format.mime_type.as_str() {
            "video/av1" if format.clock_rate == 90000 && format.channels.is_none() => {
                let parameters = format.parameter_map();
                if parameters.len() != STOCK_AV1_PARAMETERS.len()
                    || !STOCK_AV1_PARAMETERS.iter().all(|&(key, expected)| {
                        parameters.get(key).is_some_and(|value| value == expected)
                    })
                {
                    return Err(ValidationError::Format);
                }
                true
            }
            _ => match format.parse_known_codec()? {
                Some(codec_format::Codec::Vp9(vp9)) if vp9.profile_id.unwrap_or(0) == 0 => true,
                Some(codec_format::Codec::Vp8(_) | codec_format::Codec::H264(_)) => false,
                _ => return Err(ValidationError::Format),
            },
        };
        if !supports_scalable && spatial_layers != 1 {
            return Err(ValidationError::Capability);
        }
        Ok(Self {
            mode,
            spatial_layers,
            temporal_layers,
        })
    }

    pub const fn mode(self) -> RtpScalabilityMode {
        self.mode
    }

    pub const fn spatial_layers(self) -> u8 {
        self.spatial_layers
    }

    pub const fn temporal_layers(self) -> u8 {
        self.temporal_layers
    }

    pub fn decode_target(
        self,
        spatial: u8,
        temporal: u8,
    ) -> Result<VideoDecodeTarget, ValidationError> {
        if spatial >= self.spatial_layers || temporal >= self.temporal_layers {
            return Err(ValidationError::Settings);
        }
        Ok(VideoDecodeTarget { spatial, temporal })
    }

    pub fn spatial_prediction(self) -> SpatialPrediction {
        let name = self.mode.as_str();
        if self.spatial_layers == 1 || name.starts_with('S') {
            SpatialPrediction::Independent
        } else if name.contains("_KEY") {
            SpatialPrediction::KeyPicturesOnly
        } else {
            SpatialPrediction::InterLayer
        }
    }

    pub fn spatial_ratio(self) -> Option<(u8, u8)> {
        if self.spatial_layers == 1 {
            None
        } else if self.mode.as_str().ends_with('h') {
            Some((3, 2))
        } else {
            Some((2, 1))
        }
    }

    pub fn shifts_temporal_phase(self) -> bool {
        self.mode.as_str().ends_with("_SHIFT")
    }
}

#[cfg(test)]
mod video_mode_tests {
    use super::*;

    #[test]
    fn product_matrix() {
        let single = ["L1T1", "L1T2", "L1T3"];
        let scalable = [
            "L2T1",
            "L2T1h",
            "L2T2",
            "L2T2h",
            "L2T3",
            "L2T3h",
            "L3T1",
            "L3T1h",
            "L3T2",
            "L3T2h",
            "L3T3",
            "L3T3h",
            "S2T1",
            "S2T1h",
            "S2T2",
            "S2T2h",
            "S2T3",
            "S2T3h",
            "S3T1",
            "S3T1h",
            "S3T2",
            "S3T2h",
            "S3T3",
            "S3T3h",
            "L2T2_KEY",
            "L2T2_KEY_SHIFT",
            "L2T3_KEY",
            "L2T3_KEY_SHIFT",
            "L3T1_KEY",
            "L3T2_KEY",
            "L3T2_KEY_SHIFT",
            "L3T3_KEY",
            "L3T3_KEY_SHIFT",
        ];
        let mut accepted = 0;
        for mime_type in ["video/av1", "video/vp9", "video/vp8", "video/h264"] {
            let format = CodecFormat {
                mime_type: mime_type.into(),
                clock_rate: 90000,
                channels: None,
                parameters: if mime_type == "video/h264" {
                    alloc::vec![CodecParameter {
                        name: "packetization-mode".into(),
                        value: "1".into(),
                    }]
                } else if mime_type == "video/av1" {
                    [("profile", "0"), ("level-idx", "5"), ("tier", "0")]
                        .into_iter()
                        .map(|(name, value)| CodecParameter {
                            name: name.into(),
                            value: value.into(),
                        })
                        .collect()
                } else {
                    Default::default()
                },
            };
            for mode in single.into_iter().chain(scalable) {
                let expected =
                    single.contains(&mode) || matches!(mime_type, "video/av1" | "video/vp9");
                let actual = VideoMode::for_format(&format, mode).is_ok();
                assert_eq!(actual, expected, "{mime_type} {mode}");
                accepted += usize::from(actual);
            }
            for invalid in [
                "",
                "l1t1",
                "L3T0",
                "L1T2h",
                "S1T2",
                "S2T2_KEY",
                "L3T1_KEY_SHIFT",
                "L3T3h_KEY",
            ] {
                assert!(
                    VideoMode::for_format(&format, invalid).is_err(),
                    "{invalid}"
                );
            }
        }
        assert_eq!(accepted, 78);
    }

    #[test]
    fn requested_modes_require_both_the_profile_and_intersection() {
        use crate::state::{RtpCodecCapability, validation::negotiated_video_mode};

        let format = CodecFormat {
            mime_type: "video/vp9".into(),
            clock_rate: 90_000,
            ..Default::default()
        };
        let mut capability = RtpCodecCapability {
            format: Some(format.clone()),
            scalability_modes: ["S3T2h", "L3T3_KEY_SHIFT", "future-mode"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        };
        let mode = negotiated_video_mode(&format, &capability, "S3T2h").unwrap();
        assert_eq!((mode.spatial_layers(), mode.temporal_layers()), (3, 2));
        assert_eq!(mode.spatial_ratio(), Some((3, 2)));
        assert_eq!(mode.spatial_prediction(), SpatialPrediction::Independent);
        assert!(!mode.shifts_temporal_phase());
        let target = mode.decode_target(2, 1).unwrap();
        assert_eq!((target.spatial(), target.temporal()), (2, 1));
        assert!(mode.decode_target(3, 1).is_err());
        assert!(mode.decode_target(2, 2).is_err());
        assert!(
            EncodedLayerEvidence::from_emitted_layer_ids(2, 1)
                .unwrap()
                .fits(mode)
        );
        assert!(
            !EncodedLayerEvidence::from_emitted_layer_ids(2, 2)
                .unwrap()
                .fits(mode)
        );
        assert!(EncodedLayerEvidence::from_emitted_layer_ids(3, 0).is_err());
        let mode = negotiated_video_mode(&format, &capability, "L3T3_KEY_SHIFT").unwrap();
        assert_eq!(mode.spatial_ratio(), Some((2, 1)));
        assert_eq!(
            mode.spatial_prediction(),
            SpatialPrediction::KeyPicturesOnly
        );
        assert!(mode.shifts_temporal_phase());
        for invalid in ["future-mode", "L1T1"] {
            assert!(negotiated_video_mode(&format, &capability, invalid).is_err());
        }
        let unsupported_profile = CodecFormat {
            parameters: alloc::vec![CodecParameter {
                name: "profile-id".into(),
                value: "2".into(),
            }],
            ..format.clone()
        };
        capability.format = Some(unsupported_profile.clone());
        assert!(negotiated_video_mode(&format, &capability, "S3T2h").is_err());
        assert!(negotiated_video_mode(&unsupported_profile, &capability, "S3T2h").is_err());
    }
}

pub(crate) fn canonical_parameters(format: &CodecFormat) -> BTreeMap<String, String> {
    let mut parameters = format.parameter_map();
    if let Some(codec) = format.known_codec() {
        parameters.extend(CodecFormat::from(codec).parameter_map());
    }
    parameters
}

pub(crate) fn normalized_parameters(format: &CodecFormat) -> BTreeMap<String, String> {
    let mut parameters = canonical_parameters(format);
    let defaults: &[(&str, &str)] = match format.mime_type.as_str() {
        "audio/opus" => &[
            ("maxplaybackrate", "48000"),
            ("sprop-maxcapturerate", "48000"),
        ],
        "video/h264" => &[
            ("level-asymmetry-allowed", "0"),
            ("profile-level-id", "42e01f"),
            ("packetization-mode", "0"),
        ],
        "video/vp9" => &[("profile-id", "0")],
        _ => &[],
    };
    for (name, default) in defaults {
        if parameters.get(*name).is_some_and(|value| value == default) {
            parameters.remove(*name);
        }
    }
    parameters
}
