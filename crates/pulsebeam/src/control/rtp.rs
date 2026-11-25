use std::collections::BTreeSet;

pub(crate) mod policy;

use pulsebeam_proto::state::{self as wire, validation};
#[cfg(test)]
use pulsebeam_rtc::RtpConfigurationPatch;
use pulsebeam_rtc::{
    PacketFeedbackKind, RtpCapabilities, RtpCodecParameters, RtpConfigurationError,
    RtpConfigurationLimits,
};

use wire::STOCK_AV1_PARAMETERS;

pub(crate) fn is_stock_av1(codec: &RtpCodecParameters) -> bool {
    codec.name == "av1"
        && codec.clock_rate == 90_000
        && codec.channels.is_none()
        && codec.fmtp.len() == STOCK_AV1_PARAMETERS.len()
        && STOCK_AV1_PARAMETERS.iter().all(|&(name, value)| {
            codec
                .fmtp
                .get(name)
                .is_some_and(|parameter| parameter == value)
        })
}

/// Fabric subset of the RTC format compiler. AV1 fallback is restricted to
/// the explicitly negotiated stock format; other VP9 profiles are RTC-only.
pub(crate) fn fabric_profile(
    codec: &RtpCodecParameters,
) -> Option<pulsebeam_core::fabric::Profile> {
    use pulsebeam_core::fabric::Profile;
    use pulsebeam_rtc::CodecProfile;
    match codec.profile()? {
        CodecProfile::Opus => Some(Profile::Opus),
        CodecProfile::H264 {
            packetization_mode: 1,
            ..
        } => Some(Profile::H264),
        CodecProfile::Vp8 => Some(Profile::Vp8),
        CodecProfile::Vp9 { profile_id: 0 } => Some(Profile::Vp9),
        CodecProfile::Av1 { .. } if is_stock_av1(codec) => Some(Profile::Av1),
        _ => None,
    }
}

fn opus_format() -> wire::CodecFormat {
    wire::CodecFormat::from(wire::codec_format::Codec::Opus(wire::OpusParameters {
        min_packet_time_ms: Some(10),
        in_band_fec: Some(true),
        ..Default::default()
    }))
}

fn h264_format() -> wire::CodecFormat {
    wire::CodecFormat::from(wire::codec_format::Codec::H264(wire::H264Parameters {
        profile_level_id: Some(0x42e01f),
        level_asymmetry_allowed: Some(true),
    }))
}

pub(crate) fn quic_capabilities(directional_video_dd: bool) -> wire::RtpCapabilities {
    let media = if directional_video_dd {
        &[
            "audio/opus",
            "video/h264",
            "video/vp8",
            "video/vp9",
            "video/av1",
        ][..]
    } else {
        &["audio/opus", "video/h264"][..]
    };
    wire::RtpCapabilities {
        codecs: media
            .iter()
            .filter_map(|mime| wire::CodecFormat::native_quic_baseline(mime))
            .map(|format| wire::RtpCodecCapability {
                format: Some(format),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

pub(crate) fn capabilities() -> wire::RtpCapabilities {
    wire::RtpCapabilities {
        codecs: vec![
            wire::RtpCodecCapability {
                format: Some(opus_format()),
                feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                rtx_supported: false,
                scalability_modes: vec![],
            },
            wire::RtpCodecCapability {
                format: Some(h264_format()),
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

pub(crate) fn bootstrap_capabilities(offer: &str) -> wire::RtpCapabilities {
    let av1 = av1_rfc8888_capabilities();
    if transport_capabilities(&av1).is_ok_and(|caps| {
        caps.matches_single_video_rfc8888_offer(&pulsebeam_rtc::SdpOffer::new(offer))
    }) {
        av1
    } else {
        capabilities()
    }
}

fn av1_rfc8888_capabilities() -> wire::RtpCapabilities {
    wire::RtpCapabilities {
        codecs: vec![wire::RtpCodecCapability {
            format: Some(wire::CodecFormat {
                mime_type: "video/av1".into(),
                clock_rate: 90_000,
                channels: None,
                parameters: STOCK_AV1_PARAMETERS
                    .iter()
                    .map(|&(name, value)| wire::CodecParameter {
                        name: name.into(),
                        value: value.into(),
                    })
                    .collect(),
            }),
            feedback: vec![
                wire::RtcpFeedback::Nack.as_str().into(),
                wire::RtcpFeedback::NackPli.as_str().into(),
                "ack ccfb".into(),
            ],
            rtx_supported: true,
            scalability_modes: vec![wire::RtpScalabilityMode::L1t1.as_str().into()],
        }],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid.into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor.into(),
        ],
        rtcp_modes: capabilities().rtcp_modes,
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
    let codecs: Vec<pulsebeam_rtc::RtpCodecCapability> = codecs;
    let feedback = match codecs.as_slice() {
        [av1]
            if is_stock_av1(&av1.parameters)
                && av1.parameters.feedback.contains("ack ccfb")
                && !av1.parameters.feedback.contains("transport-cc") =>
        {
            PacketFeedbackKind::Rfc8888
        }
        _ => PacketFeedbackKind::TransportWide,
    };
    Ok(RtpCapabilities {
        codecs,
        extensions,
        feedback,
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

fn deferred_native_video_mode(mime: &str, mode: wire::RtpScalabilityMode) -> bool {
    use wire::RtpScalabilityMode::*;
    matches!(mime, "video/av1" | "video/vp9")
        && matches!(mode, L2t3KeyShift | L3t2KeyShift | L3t3KeyShift)
        || mime == "video/av1" && matches!(mode, S3t1 | S3t1h | S3t2 | S3t2h | S3t3 | S3t3h)
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
            encoding.scalability_mode.as_deref().is_some_and(|mode| {
                match validation::negotiated_video_mode(format, capability, mode) {
                    Ok(mode) => {
                        publication && deferred_native_video_mode(&format.mime_type, mode.mode())
                    }
                    Err(_) => true,
                }
            })
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
        extmap_allow_mixed: parameters.extmap_allow_mixed,
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
    fn quic_raw_video_capabilities_preserve_the_legacy_intersection() {
        let legacy = quic_capabilities(false);
        let current = quic_capabilities(true);
        assert_eq!(validation::validate_capabilities(&legacy), Ok(()));
        assert_eq!(validation::validate_capabilities(&current), Ok(()));
        assert_eq!(legacy.codecs, current.codecs[..2]);
        assert_eq!(legacy.codecs.len(), 2);
        for (candidate, mime) in
            current.codecs[2..]
                .iter()
                .zip(["video/vp8", "video/vp9", "video/av1"])
        {
            assert!(candidate.scalability_modes.is_empty());
            let format = candidate.format.as_ref().unwrap();
            assert_eq!(format.mime_type, mime);
            let parameters = wire::QuicRtpParameters {
                binding_id: 8,
                incarnation: 9,
                primary_ssrc: 37,
                codec: Some(wire::RtpCodecParameters {
                    payload_type: Some(125),
                    format: Some(format.clone()),
                    ..Default::default()
                }),
                video_dd: Some(wire::QuicVideoDd::Absent as i32),
                ..Default::default()
            };
            let binding =
                validation::QuicRtpBinding::compile(&parameters, wire::TrackKind::Video).unwrap();
            assert_eq!(binding.payload_type(), 125);
            assert_eq!(binding.ssrc().get(), 37);
        }
        assert_eq!(current.codecs.len(), 5);
    }

    #[test]
    fn publication_mode_requires_the_selected_format_and_advertised_intersection() {
        let mut capabilities = capabilities();
        let mut rtp = wire::RtpParameters {
            mid: "camera".into(),
            codec: Some(wire::RtpCodecParameters {
                payload_type: Some(96),
                feedback: vec![wire::RtcpFeedback::TransportCc.into()],
                format: Some(h264_format()),
                ..Default::default()
            }),
            header_extensions: vec![wire::RtpHeaderExtensionParameters {
                uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid.into(),
                id: 1,
            }],
            encodings: vec![wire::RtpEncodingParameters {
                ssrc: Some(42),
                ..Default::default()
            }],
            rtcp: Some(wire::RtcpParameters {
                cname: "source".into(),
                reduced_size: false,
            }),
            ..Default::default()
        };
        assert!(publication_binding(&rtp, wire::TrackKind::Video, &capabilities).is_ok());
        rtp.encodings[0].scalability_mode = Some("L1T3".into());
        assert!(publication_binding(&rtp, wire::TrackKind::Video, &capabilities).is_ok());
        rtp.encodings[0].scalability_mode = Some("L1T2".into());
        assert!(publication_binding(&rtp, wire::TrackKind::Video, &capabilities).is_err());
        for capability in &mut capabilities.codecs {
            if capability.format.as_ref() == Some(&h264_format()) {
                capability.scalability_modes.push("S3T3h".into());
            }
        }
        rtp.encodings[0].scalability_mode = Some("S3T3h".into());
        assert!(publication_binding(&rtp, wire::TrackKind::Video, &capabilities).is_err());
    }

    #[test]
    fn native_publication_rejects_deferred_modes_without_redefining_negotiation() {
        let mut capabilities = quic_capabilities(true);
        let mid = wire::RtpHeaderExtensionType::RtpHeaderExtensionMid.as_str();
        capabilities.header_extensions.push(mid.into());
        capabilities
            .rtcp_modes
            .push(wire::RtcpMode::Compound.into());
        for mime in ["video/av1", "video/vp9"] {
            let capability = capabilities
                .codecs
                .iter_mut()
                .find(|codec| {
                    codec
                        .format
                        .as_ref()
                        .is_some_and(|format| format.mime_type == mime)
                })
                .unwrap();
            let format = capability.format.clone().unwrap();
            let shifted = ["L2T3_KEY_SHIFT", "L3T2_KEY_SHIFT", "L3T3_KEY_SHIFT"];
            let s3 = ["S3T1", "S3T1h", "S3T2", "S3T2h", "S3T3", "S3T3h"];
            let cases = ["L1T1", "L2T2_KEY_SHIFT"]
                .into_iter()
                .map(|mode| (mode, true))
                .chain(shifted.into_iter().map(|mode| (mode, false)))
                .chain(s3.into_iter().map(|mode| (mode, mime == "video/vp9")))
                .collect::<Vec<_>>();
            capability.scalability_modes = cases.iter().map(|(mode, _)| (*mode).into()).collect();
            for (mode, admitted) in cases {
                let rtp = wire::RtpParameters {
                    mid: "camera".into(),
                    header_extensions: vec![wire::RtpHeaderExtensionParameters {
                        uri: mid.into(),
                        id: 1,
                    }],
                    codec: Some(wire::RtpCodecParameters {
                        payload_type: Some(96),
                        format: Some(format.clone()),
                        ..Default::default()
                    }),
                    encodings: vec![wire::RtpEncodingParameters {
                        ssrc: Some(42),
                        scalability_mode: Some(mode.into()),
                        ..Default::default()
                    }],
                    rtcp: Some(wire::RtcpParameters {
                        cname: "source".into(),
                        reduced_size: false,
                    }),
                    ..Default::default()
                };
                assert!(
                    receiving_binding(&rtp, wire::TrackKind::Video, &capabilities).is_ok(),
                    "{mime} {mode} must remain negotiable"
                );
                let result = publication_binding(&rtp, wire::TrackKind::Video, &capabilities);
                if admitted {
                    assert!(result.is_ok(), "{mime} {mode}: {result:?}");
                } else {
                    assert_eq!(
                        result.unwrap_err(),
                        wire::FailureCode::Unsupported,
                        "{mime} {mode}"
                    );
                }
            }
        }
    }

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
            extmap_allow_mixed: None,
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
