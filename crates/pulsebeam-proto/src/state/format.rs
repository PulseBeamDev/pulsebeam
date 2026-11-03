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
    RtpHeaderExtensionPlayoutDelay => "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
    RtpHeaderExtensionDependencyDescriptor => "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
    RtpHeaderExtensionAudioLevel => "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
    RtpHeaderExtensionVideoLayersAllocation => "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
});

features!(RtpScalabilityMode {
    Unspecified => "",
    L1t1 => "L1T1",
    L1t2 => "L1T2",
    L1t3 => "L1T3",
});

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
