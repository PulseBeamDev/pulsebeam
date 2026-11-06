use std::collections::BTreeSet;

pub(crate) mod policy;

use pulsebeam_proto::state::{self as wire, validation};
#[cfg(test)]
use pulsebeam_rtc::RtpConfigurationPatch;
use pulsebeam_rtc::{
    PacketFeedbackKind, RtpCapabilities, RtpCodecParameters, RtpConfigurationError,
    RtpConfigurationLimits,
};

pub(crate) fn capabilities() -> wire::RtpCapabilities {
    wire::RtpCapabilities {
        codecs: vec![
            wire::RtpCodecCapability {
                format: Some(wire::CodecFormat::from(wire::codec_format::Codec::Opus(
                    wire::OpusParameters {
                        min_packet_time_ms: Some(10),
                        in_band_fec: Some(true),
                        ..Default::default()
                    },
                ))),
                feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                rtx_supported: false,
                scalability_modes: vec![],
            },
            wire::RtpCodecCapability {
                format: Some(wire::CodecFormat::from(wire::codec_format::Codec::H264(
                    wire::H264Parameters {
                        profile_level_id: Some(0x42e01f),
                        level_asymmetry_allowed: Some(true),
                    },
                ))),
                feedback: vec![
                    wire::RtcpFeedback::TransportCc.as_str().into(),
                    wire::RtcpFeedback::Nack.as_str().into(),
                    wire::RtcpFeedback::NackPli.as_str().into(),
                    wire::RtcpFeedback::CcmFir.as_str().into(),
                ],
                rtx_supported: true,
                scalability_modes: vec![
                    wire::RtpScalabilityMode::L1t1.as_str().into(),
                    wire::RtpScalabilityMode::L1t3.as_str().into(),
                ],
            },
        ],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionRid
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionRepairedRid
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionAudioLevel
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionVideoLayersAllocation
                .as_str()
                .into(),
        ],
        rtcp_modes: vec![
            wire::RtcpMode::Compound.as_str().into(),
            wire::RtcpMode::ReducedSize.as_str().into(),
        ],
    }
}

#[cfg(test)]
pub(crate) fn initialize(
    connection: &mut pulsebeam_rtc::RtcConnection,
    capabilities: &wire::RtpCapabilities,
) -> Result<(), RtpConfigurationError> {
    connection.set_rtp_capabilities(transport_capabilities(capabilities)?)?;
    connection.update_rtp(RtpConfigurationPatch::default())
}

pub(crate) fn transport_capabilities(
    capabilities: &wire::RtpCapabilities,
) -> Result<RtpCapabilities, RtpConfigurationError> {
    validation::validate_capabilities(capabilities).map_err(|_| RtpConfigurationError::Invalid)?;
    let codecs = capabilities
        .codecs
        .iter()
        .map(|capability| {
            let mut codec = codec_parameters(
                capability
                    .format
                    .as_ref()
                    .ok_or(RtpConfigurationError::Invalid)?,
            )?;
            codec.feedback = capability
                .feedback
                .iter()
                .map(|feedback| feedback_name(feedback).map(str::to_owned))
                .collect::<Result<_, _>>()?;
            Ok(pulsebeam_rtc::RtpCodecCapability {
                parameters: codec,
                repair: capability.rtx_supported,
            })
        })
        .collect::<Result<_, RtpConfigurationError>>()?;
    let extensions = capabilities
        .header_extensions
        .iter()
        .map(|extension| extension_uri(extension).map(str::to_owned))
        .collect::<Result<_, _>>()?;
    Ok(RtpCapabilities {
        codecs,
        extensions,
        feedback: PacketFeedbackKind::TransportWide,
        limits: RtpConfigurationLimits::default(),
    })
}

fn codec_parameters(
    format: &wire::CodecFormat,
) -> Result<RtpCodecParameters, RtpConfigurationError> {
    let parameters =
        wire::sdp::codec_parameters(format).map_err(|_| RtpConfigurationError::Invalid)?;
    Ok(RtpCodecParameters {
        name: parameters.name,
        clock_rate: parameters.clock_rate,
        channels: parameters.channels,
        fmtp: parameters.fmtp,
        feedback: BTreeSet::new(),
    })
}

fn feedback_name(value: &str) -> Result<&str, RtpConfigurationError> {
    wire::sdp::feedback_name(value).map_err(|_| RtpConfigurationError::Invalid)
}

fn extension_uri(value: &str) -> Result<&str, RtpConfigurationError> {
    wire::sdp::extension_uri(value).map_err(|_| RtpConfigurationError::Invalid)
}

pub(crate) fn publication_binding(
    parameters: &wire::RtpParameters,
    kind: wire::TrackKind,
    capabilities: &wire::RtpCapabilities,
) -> Result<pulsebeam_rtc::RtpBinding, wire::FailureCode> {
    binding(parameters, kind, capabilities, true)
}

pub(crate) fn receiving_binding(
    parameters: &wire::RtpParameters,
    kind: wire::TrackKind,
    capabilities: &wire::RtpCapabilities,
) -> Result<pulsebeam_rtc::RtpBinding, wire::FailureCode> {
    binding(parameters, kind, capabilities, false)
}

fn binding(
    parameters: &wire::RtpParameters,
    kind: wire::TrackKind,
    capabilities: &wire::RtpCapabilities,
    publication: bool,
) -> Result<pulsebeam_rtc::RtpBinding, wire::FailureCode> {
    use wire::FailureCode as Failure;
    validation::validate_rtp(parameters, kind, publication)
        .map_err(|_| Failure::InvalidConfiguration)?;
    let codec = parameters
        .codec
        .as_ref()
        .ok_or(Failure::InvalidConfiguration)?;
    let format = codec.format.as_ref().ok_or(Failure::InvalidConfiguration)?;
    // The fixed RTC backend currently admits exact normalized format envelopes.
    // This is not directional source/decoder compatibility or an intersection.
    let capability = capabilities
        .codecs
        .iter()
        .find(|capability| {
            capability
                .format
                .as_ref()
                .is_some_and(|supported| validation::formats_equivalent(format, supported))
        })
        .ok_or(Failure::Unsupported)?;
    let rtcp = parameters
        .rtcp
        .as_ref()
        .ok_or(Failure::InvalidConfiguration)?;
    let mode = if rtcp.reduced_size {
        wire::RtcpMode::ReducedSize
    } else {
        wire::RtcpMode::Compound
    };
    if !capabilities
        .rtcp_modes
        .iter()
        .any(|value| value == mode.as_str())
        || codec.rtx_payload_type.is_some() && !capability.rtx_supported
        || codec
            .feedback
            .iter()
            .any(|value| !capability.feedback.contains(value))
        || parameters
            .header_extensions
            .iter()
            .any(|extension| !capabilities.header_extensions.contains(&extension.uri))
        || parameters.encodings.iter().any(|encoding| {
            encoding
                .scalability_mode
                .as_ref()
                .is_some_and(|mode| !capability.scalability_modes.contains(mode))
        })
    {
        return Err(Failure::Unsupported);
    }
    let mut selected = codec_parameters(format).map_err(failure_code)?;
    selected.feedback = codec
        .feedback
        .iter()
        .map(|value| feedback_name(value).map(str::to_owned))
        .collect::<Result<_, _>>()
        .map_err(failure_code)?;
    Ok(pulsebeam_rtc::RtpBinding {
        mid: parameters.mid.clone(),
        kind: if kind == wire::TrackKind::Audio {
            pulsebeam_rtc::MediaKind::Audio
        } else {
            pulsebeam_rtc::MediaKind::Video
        },
        codec: pulsebeam_rtc::RtpCodec {
            payload_type: u8::try_from(codec.payload_type.ok_or(Failure::InvalidConfiguration)?)
                .map_err(|_| Failure::InvalidConfiguration)?,
            repair_payload_type: codec
                .rtx_payload_type
                .map(u8::try_from)
                .transpose()
                .map_err(|_| Failure::InvalidConfiguration)?,
            parameters: selected,
        },
        rtcp: pulsebeam_rtc::RtcpParameters {
            cname: rtcp.cname.clone(),
            reduced_size: rtcp.reduced_size,
        },
        extensions: parameters
            .header_extensions
            .iter()
            .map(|extension| {
                Ok(pulsebeam_rtc::RtpExtensionMapping {
                    id: u8::try_from(extension.id).map_err(|_| Failure::InvalidConfiguration)?,
                    uri: extension_uri(&extension.uri)
                        .map_err(failure_code)?
                        .to_owned(),
                })
            })
            .collect::<Result<_, Failure>>()?,
        encodings: parameters
            .encodings
            .iter()
            .map(|encoding| {
                Ok(pulsebeam_rtc::RtpEncoding {
                    primary: encoding.ssrc.ok_or(Failure::InvalidConfiguration)?,
                    repair: encoding.rtx_ssrc,
                    rid: (!encoding.rid.is_empty()).then(|| encoding.rid.clone()),
                    active: false,
                })
            })
            .collect::<Result<_, Failure>>()?,
    })
}

pub(crate) fn failure_code(error: RtpConfigurationError) -> wire::FailureCode {
    crate::participant::policy::ExecutionFailureKind::from(error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_bootstrap_preserves_declared_formats_and_enters_strict_admission() {
        let defaults = capabilities();
        assert!(
            transport_capabilities(&defaults)
                .unwrap()
                .extensions
                .iter()
                .any(|uri| uri
                    == "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00")
        );
        let default_h264 = defaults
            .codecs
            .into_iter()
            .find(|codec| {
                matches!(
                    codec
                        .format
                        .as_ref()
                        .and_then(wire::CodecFormat::known_codec),
                    Some(wire::codec_format::Codec::H264(_))
                )
            })
            .unwrap();
        assert_eq!(
            default_h264.scalability_modes,
            [
                wire::RtpScalabilityMode::L1t1.as_str(),
                wire::RtpScalabilityMode::L1t3.as_str(),
            ]
        );
        assert!(
            defaults.header_extensions.contains(
                &(wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor
                    .as_str()
                    .into())
            )
        );
        let opus = wire::codec_format::Codec::Opus(wire::OpusParameters {
            stereo: Some(false),
            source_stereo: Some(false),
            in_band_fec: Some(true),
            dtx: Some(false),
            constant_bitrate: Some(false),
            max_average_bitrate_bps: Some(64000),
            max_playback_rate_hz: Some(48000),
            max_capture_rate_hz: Some(48000),
            min_packet_time_ms: Some(10),
        });
        let opus = wire::CodecFormat::from(opus);
        let opus_parameters = codec_parameters(&opus).unwrap();
        assert_eq!(opus_parameters.clock_rate, 48000);
        assert_eq!(opus_parameters.channels, Some(2));
        for (key, value) in [
            ("stereo", "0"),
            ("sprop-stereo", "0"),
            ("useinbandfec", "1"),
            ("usedtx", "0"),
            ("cbr", "0"),
            ("maxaveragebitrate", "64000"),
            ("maxplaybackrate", "48000"),
            ("sprop-maxcapturerate", "48000"),
            ("minptime", "10"),
        ] {
            assert_eq!(
                opus_parameters.fmtp.get(key).map(String::as_str),
                Some(value)
            );
        }
        let formats = [
            opus,
            wire::CodecFormat::from(wire::codec_format::Codec::H264(wire::H264Parameters {
                profile_level_id: Some(0x42e01f),
                level_asymmetry_allowed: Some(true),
            })),
            wire::CodecFormat::from(wire::codec_format::Codec::Vp8(wire::Vp8Parameters {})),
            wire::CodecFormat::from(wire::codec_format::Codec::Vp9(wire::Vp9Parameters {
                profile_id: Some(2),
            })),
        ];
        let caps = wire::RtpCapabilities {
            codecs: formats
                .into_iter()
                .map(|format| {
                    let rtx_supported = matches!(
                        format.known_codec(),
                        Some(wire::codec_format::Codec::Vp8(_))
                    );
                    wire::RtpCodecCapability {
                        format: Some(format),
                        feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                        rtx_supported,
                        scalability_modes: vec![],
                    }
                })
                .collect(),
            header_extensions: capabilities().header_extensions,
            rtcp_modes: vec![
                wire::RtcpMode::Compound.into(),
                wire::RtcpMode::ReducedSize.into(),
            ],
        };
        let mut accepted = crate::control::application_only_test_connection();
        initialize(&mut accepted.connection, &caps).unwrap();
        assert_eq!(
            accepted.connection.rtp_configuration(),
            Some(pulsebeam_rtc::RtpConfiguration::default())
        );
        let h264 = codec_parameters(caps.codecs.get(1).unwrap().format.as_ref().unwrap()).unwrap();
        assert_eq!(
            h264.fmtp.get("profile-level-id").map(String::as_str),
            Some("42e01f")
        );
        assert_eq!(
            h264.fmtp.get("packetization-mode").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            h264.fmtp.get("level-asymmetry-allowed").map(String::as_str),
            Some("1")
        );
        assert!(
            codec_parameters(caps.codecs.get(2).unwrap().format.as_ref().unwrap())
                .unwrap()
                .fmtp
                .is_empty()
        );
        let mut parameters =
            codec_parameters(caps.codecs.get(3).unwrap().format.as_ref().unwrap()).unwrap();
        parameters.feedback.insert("transport-cc".into());
        assert_eq!(
            parameters.fmtp.get("profile-id").map(String::as_str),
            Some("2")
        );
        let binding = pulsebeam_rtc::RtpBinding {
            mid: "typed".into(),
            kind: pulsebeam_rtc::MediaKind::Video,
            codec: pulsebeam_rtc::RtpCodec {
                payload_type: 98,
                parameters,
                repair_payload_type: None,
            },
            rtcp: pulsebeam_rtc::RtcpParameters {
                cname: "prepared-test-source".into(),
                reduced_size: false,
            },
            extensions: vec![
                pulsebeam_rtc::RtpExtensionMapping {
                    id: 1,
                    uri: pulsebeam_rtc::MID_EXTENSION.into(),
                },
                pulsebeam_rtc::RtpExtensionMapping {
                    id: 2,
                    uri: pulsebeam_rtc::TWCC_EXTENSION.into(),
                },
                pulsebeam_rtc::RtpExtensionMapping {
                    id: 3,
                    uri: "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00"
                        .into(),
                },
            ],
            encodings: vec![pulsebeam_rtc::RtpEncoding {
                primary: 0,
                repair: None,
                rid: None,
                active: true,
            }],
        };
        let mut prohibited = binding.clone();
        prohibited.codec.repair_payload_type = Some(99);
        prohibited.encodings.first_mut().unwrap().repair = Some(1);
        assert_eq!(
            accepted.connection.update_rtp(RtpConfigurationPatch {
                inbound: vec![prohibited],
                ..Default::default()
            }),
            Err(RtpConfigurationError::Unsupported)
        );
        assert_eq!(
            accepted.connection.rtp_configuration(),
            Some(pulsebeam_rtc::RtpConfiguration::default())
        );
        accepted
            .connection
            .update_rtp(RtpConfigurationPatch {
                inbound: vec![binding.clone()],
                ..Default::default()
            })
            .unwrap();
        let configured = Some(pulsebeam_rtc::RtpConfiguration {
            inbound: vec![binding],
            outbound: vec![],
        });
        assert_eq!(accepted.connection.rtp_configuration(), configured);
        let mut permitted = configured
            .as_ref()
            .unwrap()
            .inbound
            .first()
            .unwrap()
            .clone();
        permitted.mid = "permitted".into();
        permitted.codec.payload_type = 96;
        permitted.codec.parameters =
            codec_parameters(caps.codecs.get(2).unwrap().format.as_ref().unwrap()).unwrap();
        permitted
            .codec
            .parameters
            .feedback
            .insert("transport-cc".into());
        permitted.codec.repair_payload_type = Some(97);
        permitted.encodings.first_mut().unwrap().primary = 2;
        permitted.encodings.first_mut().unwrap().repair = Some(3);
        accepted
            .connection
            .update_rtp(RtpConfigurationPatch {
                inbound: vec![permitted.clone()],
                ..Default::default()
            })
            .unwrap();
        let configured = accepted.connection.rtp_configuration();
        assert_eq!(configured.as_ref().unwrap().inbound.len(), 2);
        assert!(configured.as_ref().unwrap().inbound.contains(&permitted));
        let mut invalid = caps;
        invalid.header_extensions.push(String::new());
        assert_eq!(
            initialize(&mut accepted.connection, &invalid),
            Err(RtpConfigurationError::Invalid)
        );
        assert_eq!(accepted.connection.rtp_configuration(), configured);
    }
}
