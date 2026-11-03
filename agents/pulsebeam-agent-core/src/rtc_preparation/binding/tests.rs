use super::*;
use crate::{
    MediaDirection, MediaKind, MediaSlot, SlotBinding,
    client_sdp::{
        Bindings, Codec, CodecParameters, Direction, Encoding, Extension, MediaKey, RepairCodec,
        RtcpParameters, RuntimeEncoding,
    },
    rtc_preparation::{bootstrap_media_key, prepare_bootstrap_bindings},
};
use alloc::{string::String, vec};

pub(crate) fn fixture(format: wire::CodecFormat) -> (Binding, wire::RtpCapabilities) {
    let parameters = sdp::codec_parameters(&format).unwrap();
    let audio = parameters.channels.is_some();
    let capability = wire::RtpCodecCapability {
        format: Some(format),
        feedback: vec![wire::RtcpFeedback::TransportCc as i32],
        rtx_supported: !audio,
        scalability_modes: if audio {
            vec![]
        } else {
            vec![wire::RtpScalabilityMode::L1t1 as i32]
        },
    };
    let capabilities = wire::RtpCapabilities {
        codecs: vec![capability],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid as i32,
            wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01 as i32,
            wire::RtpHeaderExtensionType::RtpHeaderExtensionVideoLayersAllocation as i32,
        ],
        rtcp_modes: vec![wire::RtcpMode::Compound as i32],
    };
    let binding = Binding {
        key: MediaKey("source".into()),
        group: None,
        mid: "v".into(),
        kind: if audio { Kind::Audio } else { Kind::Video },
        codec: Codec {
            payload_type: 0,
            parameters: CodecParameters {
                name: parameters.name.into(),
                clock_rate: parameters.clock_rate,
                channels: parameters.channels,
                fmtp: parameters.fmtp,
                feedback: ["transport-cc".into()].into_iter().collect(),
            },
            repair: (!audio).then_some(RepairCodec {
                payload_type: 97,
                clock_rate: 90000,
            }),
        },
        extensions: capabilities
            .header_extensions
            .iter()
            .enumerate()
            .map(|(index, kind)| Extension {
                id: if index == 0 {
                    1
                } else {
                    (19 + index).try_into().unwrap()
                },
                uri: sdp::extension_uri(*kind).unwrap().into(),
                direction: Direction::SendRecv,
            })
            .collect(),
        encodings: vec![Encoding {
            rid: None,
            primary: Some(0),
            repair: (!audio).then_some(1),
            paused: false,
            runtime: RuntimeEncoding::default(),
        }],
        rtcp: RtcpParameters {
            cname: Some("actual-source".into()),
            reduced_size: false,
        },
    };
    (binding, capabilities)
}

fn video() -> wire::CodecFormat {
    wire::CodecFormat {
        codec: Some(wire::codec_format::Codec::Vp8(wire::Vp8Parameters {})),
    }
}

#[test]
fn actual_envelope_recovery_preserves_format_presence_zero_and_extension_ids() {
    for codec in [
        wire::codec_format::Codec::Opus(wire::OpusParameters {
            stereo: Some(false),
            source_stereo: Some(false),
            in_band_fec: Some(true),
            dtx: Some(false),
            min_packet_time_ms: Some(10),
            ..Default::default()
        }),
        wire::codec_format::Codec::H264(wire::H264Parameters {
            profile_level_id: Some(0x42e01f),
            level_asymmetry_allowed: Some(false),
        }),
        wire::codec_format::Codec::Vp8(wire::Vp8Parameters {}),
        wire::codec_format::Codec::Vp9(wire::Vp9Parameters {
            profile_id: Some(2),
        }),
    ] {
        let format = wire::CodecFormat { codec: Some(codec) };
        let (binding, capabilities) = fixture(format);
        let actual = prepared_rtp_binding(&binding, &capabilities, true).unwrap();
        let Some(wire::track_binding::Transport::Rtp(actual)) = actual.transport else {
            panic!("RTP expected");
        };
        let codec = actual.codec.unwrap();
        assert_eq!(codec.format, Some(format));
        assert_eq!(codec.payload_type, Some(0));
        assert_eq!(
            codec.rtx_payload_type,
            binding
                .codec
                .repair
                .as_ref()
                .map(|repair| u32::from(repair.payload_type))
        );
        assert_eq!(actual.encodings[0].ssrc, Some(0));
        assert_eq!(actual.encodings[0].rtx_ssrc, binding.encodings[0].repair);
        assert_eq!(actual.encodings[0].scalability_mode, None);
        assert_eq!(actual.header_extensions[1].id, 20);
        assert_eq!(actual.rtcp.unwrap().cname, "actual-source");
    }
}

#[test]
fn missing_or_unrepresentable_facts_are_not_defaulted_or_dropped() {
    let (binding, capabilities) = fixture(video());
    let mut primary = binding.clone();
    primary.encodings[0].primary = None;
    let mut repair = binding.clone();
    repair.encodings[0].repair = None;
    let mut cname = binding.clone();
    cname.rtcp.cname = None;
    for pending in [primary, repair, cname] {
        assert_eq!(
            prepared_rtp_binding(&pending, &capabilities, true),
            Err(Error::IdentityPending)
        );
        assert_eq!(
            prepared_rtp_binding(&pending, &capabilities, false),
            Err(Error::Observation)
        );
    }
    let mut cases = Vec::new();
    let mut fmtp = binding.clone();
    fmtp.codec
        .parameters
        .fmtp
        .insert("unknown".into(), "1".into());
    cases.push(fmtp.clone());
    fmtp.encodings[0].primary = None;
    assert!(matches!(
        prepared_rtp_binding(&fmtp, &capabilities, true),
        Err(Error::Envelope(_))
    ));
    let mut feedback = binding.clone();
    feedback.codec.parameters.feedback.insert("unknown".into());
    cases.push(feedback);
    let mut extension = binding.clone();
    extension.extensions[0].uri = "unknown".into();
    cases.push(extension);
    let mut repair = binding.clone();
    repair.codec.repair.as_mut().unwrap().clock_rate = 48000;
    cases.push(repair);
    let mut scalable = binding.clone();
    scalable.encodings[0].runtime.scalability_mode = Some("L1T2".into());
    cases.push(scalable);
    let mut mode = binding.clone();
    mode.rtcp.reduced_size = true;
    cases.push(mode);
    let mut baseline = binding.clone();
    baseline.extensions.remove(1);
    cases.push(baseline);
    let mut collision = binding.clone();
    collision.encodings[0].repair = collision.encodings[0].primary;
    cases.push(collision);
    let mut extension_id = binding.clone();
    extension_id.extensions[1].id = extension_id.extensions[0].id;
    cases.push(extension_id);
    let mut kind = binding.clone();
    kind.kind = Kind::Audio;
    cases.push(kind);
    for mut case in cases {
        assert!(
            matches!(prepared_rtp_binding(&case, &capabilities, true), Err(error) if error != Error::IdentityPending)
        );
        case.rtcp.cname = None;
        assert!(matches!(
            prepared_rtp_binding(&case, &capabilities, true),
            Err(Error::Envelope(_))
        ));
    }
    let mut scalable = binding;
    scalable.encodings[0].runtime.scalability_mode = Some("L1T1".into());
    let wire::track_binding::Transport::Rtp(actual) =
        prepared_rtp_binding(&scalable, &capabilities, true)
            .unwrap()
            .transport
            .unwrap()
    else {
        panic!("RTP expected");
    };
    assert_eq!(
        actual.encodings[0].scalability_mode,
        Some(wire::RtpScalabilityMode::L1t1 as i32)
    );
}

#[test]
fn bootstrap_conversion_keeps_resource_failures_local_and_rejects_retargeting() {
    let (mut valid, capabilities) = fixture(video());
    let slots = ["first", "second"].map(|name| SlotBinding {
        slot: MediaSlot::LocalVideo(name.into()),
        mid: name.into(),
        media_index: 1,
        kind: MediaKind::Video,
        direction: MediaDirection::SendOnly,
    });
    valid.mid = "first".into();
    valid.key = bootstrap_media_key(&slots[0].slot);
    valid.encodings[0].paused = true;
    valid.encodings[0].runtime.active = Some(false);
    valid.encodings[0].runtime.max_bitrate = Some(123_456);
    valid.encodings[0].runtime.max_framerate = Some(24.0);
    let mut incomplete = valid.clone();
    incomplete.mid = "second".into();
    incomplete.key = bootstrap_media_key(&slots[1].slot);
    incomplete.encodings[0].primary = None;
    let facts = Bindings {
        publications: vec![valid, incomplete],
        receptions: vec![],
    };
    let converted = prepare_bootstrap_bindings(facts.clone(), &slots, &capabilities).unwrap();
    let prepared = converted[&slots[0].slot].as_ref().unwrap();
    assert_eq!(prepared.encodings, facts.publications[0].encodings);
    assert!(prepared.binding.transport.is_some());
    assert_eq!(converted[&slots[1].slot], Err(Error::IdentityPending));
    let mut wrong_key = facts.clone();
    wrong_key.publications[1].key = MediaKey("first".into());
    assert_eq!(
        prepare_bootstrap_bindings(wrong_key, &slots, &capabilities),
        Err(Error::Observation)
    );
    let mut wrong_mid = facts;
    wrong_mid.publications[1].mid = String::from("unrelated");
    assert_eq!(
        prepare_bootstrap_bindings(wrong_mid, &slots, &capabilities),
        Err(Error::Observation)
    );
}
