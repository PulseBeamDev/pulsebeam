//! SDP spelling of the represented typed RTP formats and features.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
};

use super::{
    CodecFormat, RtcpFeedback, RtpHeaderExtensionType, codec_format,
    validation::{self, ValidationError},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecParameters {
    pub name: &'static str,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub fmtp: BTreeMap<String, String>,
}

pub fn codec_parameters(format: &CodecFormat) -> Result<CodecParameters, ValidationError> {
    validation::validate_format(format)?;
    let mut fmtp = BTreeMap::new();
    let (name, clock_rate, channels) = match format.codec.as_ref() {
        Some(codec_format::Codec::Opus(opus)) => {
            for (key, value) in [
                ("stereo", opus.stereo),
                ("sprop-stereo", opus.source_stereo),
                ("useinbandfec", opus.in_band_fec),
                ("usedtx", opus.dtx),
                ("cbr", opus.constant_bitrate),
            ] {
                if let Some(value) = value {
                    fmtp.insert(key.into(), u8::from(value).to_string());
                }
            }
            for (key, value) in [
                ("maxaveragebitrate", opus.max_average_bitrate_bps),
                ("maxplaybackrate", opus.max_playback_rate_hz),
                ("sprop-maxcapturerate", opus.max_capture_rate_hz),
                ("minptime", opus.min_packet_time_ms),
            ] {
                if let Some(value) = value {
                    fmtp.insert(key.into(), value.to_string());
                }
            }
            ("opus", 48000, Some(2))
        }
        Some(codec_format::Codec::H264(h264)) => {
            let profile = h264.profile_level_id.ok_or(ValidationError::Format)?;
            fmtp.insert("profile-level-id".into(), format!("{profile:06x}"));
            fmtp.insert("packetization-mode".into(), "1".into());
            fmtp.insert(
                "level-asymmetry-allowed".into(),
                u8::from(h264.level_asymmetry_allowed.unwrap_or(false)).to_string(),
            );
            ("h264", 90000, None)
        }
        Some(codec_format::Codec::Vp8(_)) => ("vp8", 90000, None),
        Some(codec_format::Codec::Vp9(vp9)) => {
            fmtp.insert("profile-id".into(), vp9.profile_id.unwrap_or(0).to_string());
            ("vp9", 90000, None)
        }
        None => return Err(ValidationError::Format),
    };
    Ok(CodecParameters {
        name,
        clock_rate,
        channels,
        fmtp,
    })
}

pub fn feedback_name(value: i32) -> Result<&'static str, ValidationError> {
    match RtcpFeedback::try_from(value) {
        Ok(RtcpFeedback::TransportCc) => Ok("transport-cc"),
        Ok(RtcpFeedback::Nack) => Ok("nack"),
        Ok(RtcpFeedback::NackPli) => Ok("nack pli"),
        Ok(RtcpFeedback::CcmFir) => Ok("ccm fir"),
        _ => Err(ValidationError::Capability),
    }
}

pub fn extension_uri(value: i32) -> Result<&'static str, ValidationError> {
    match RtpHeaderExtensionType::try_from(value) {
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionMid) => {
            Ok("urn:ietf:params:rtp-hdrext:sdes:mid")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionRid) => {
            Ok("urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionRepairedRid) => {
            Ok("urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01) => {
            Ok("http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay) => {
            Ok("http://www.webrtc.org/experiments/rtp-hdrext/playout-delay")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor) => Ok(
            "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
        ),
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionAudioLevel) => {
            Ok("urn:ietf:params:rtp-hdrext:ssrc-audio-level")
        }
        Ok(RtpHeaderExtensionType::RtpHeaderExtensionVideoLayersAllocation) => {
            Ok("http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00")
        }
        _ => Err(ValidationError::Capability),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OpusParameters;

    #[test]
    fn optional_false_and_absence_keep_distinct_sdp_spellings() -> Result<(), ValidationError> {
        let mut opus = OpusParameters::default();
        let format = |opus| CodecFormat {
            codec: Some(codec_format::Codec::Opus(opus)),
        };
        let absent = codec_parameters(&format(opus.clone()))?;
        opus.stereo = Some(false);
        opus.in_band_fec = Some(true);
        let explicit = codec_parameters(&format(opus))?;
        assert!(!absent.fmtp.contains_key("stereo"));
        assert_eq!(explicit.fmtp.get("stereo").map(String::as_str), Some("0"));
        assert_eq!(
            explicit.fmtp.get("useinbandfec").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            (explicit.name, explicit.clock_rate, explicit.channels),
            ("opus", 48000, Some(2))
        );
        assert!(codec_parameters(&CodecFormat::default()).is_err());
        Ok(())
    }
}
