use super::*;
use crate::{Connection, ConnectionConfig, ConnectionEntropy, LocalCandidate, SdpOffer};
use std::time::Instant;

fn codec() -> RtpCodecParameters {
    RtpCodecParameters {
        name: "vp8".into(),
        clock_rate: 90000,
        channels: None,
        fmtp: BTreeMap::new(),
        feedback: ["transport-cc".into(), "nack".into()].into_iter().collect(),
    }
}
fn capabilities() -> RtpCapabilities {
    RtpCapabilities {
        codecs: vec![codec()],
        extensions: [
            MID_EXTENSION,
            TWCC_EXTENSION,
            RID_EXTENSION,
            REPAIRED_RID_EXTENSION,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        repair: true,
        feedback: PacketFeedbackKind::TransportWide,
        limits: RtpConfigurationLimits::default(),
    }
}
fn binding(mid: &str, primary: u32) -> RtpBinding {
    RtpBinding {
        mid: mid.into(),
        kind: MediaKind::Video,
        codec: RtpCodec {
            payload_type: 96,
            parameters: codec(),
            repair_payload_type: Some(97),
        },
        extensions: [
            MID_EXTENSION,
            TWCC_EXTENSION,
            RID_EXTENSION,
            REPAIRED_RID_EXTENSION,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, uri)| RtpExtensionMapping {
            id: u8::try_from(i.checked_add(1).unwrap()).unwrap(),
            uri: uri.into(),
        })
        .collect(),
        encodings: vec![RtpEncoding {
            primary: NonZeroU32::new(primary).unwrap(),
            repair: NonZeroU32::new(primary.checked_add(1).unwrap()),
            rid: None,
            active: true,
        }],
    }
}
fn connection() -> Connection {
    let source = include_str!("../../tests/fixtures/chrome-representative.sdp");
    let (header, _) = source.split_once("m=audio").unwrap();
    let (_, application) = source.split_once("m=application").unwrap();
    let offer = format!(
        "{}m=application{}a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host\n",
        header.replace("BUNDLE audio video data", "BUNDLE data"),
        application
    );
    Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp("192.0.2.2:41000".parse().unwrap())],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        Instant::now(),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([43; 32]),
    )
    .unwrap()
    .connection
}

#[test]
fn data_only_start_declares_feedback_and_atomic_duplex_encoding_configuration() {
    let mut connection = connection();
    assert_eq!(connection.stats().connection.feedback, None);
    let configured = RtpConfiguration {
        inbound: vec![binding("duplex", 100)],
        outbound: vec![binding("duplex", 200)],
    };
    assert_eq!(
        connection.configure_rtp(configured.clone()),
        Err(RtpConfigurationError::CapabilityState)
    );
    connection.set_rtp_capabilities(capabilities()).unwrap();
    assert_eq!(
        connection.stats().connection.feedback,
        Some(PacketFeedbackKind::TransportWide)
    );
    connection.configure_rtp(configured.clone()).unwrap();
    assert_eq!(connection.rtp_configuration(), Some(&configured));
    let mut invalid = configured.clone();
    invalid.inbound.push(binding("extra", 200));
    assert_eq!(
        connection.configure_rtp(invalid),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(connection.rtp_configuration(), Some(&configured));
    assert_eq!(connection.stats().senders[0].ssrc, 200);
    connection.set_rtp_capabilities(capabilities()).unwrap();
    let mut changed = capabilities();
    changed.feedback = PacketFeedbackKind::Rfc8888;
    assert!(connection.set_rtp_capabilities(changed).is_err());
    assert_eq!(connection.rtp_configuration(), Some(&configured));
}

#[test]
fn identity_history_is_bounded_and_failed_updates_do_not_consume_it() {
    let mut connection = connection();
    let mut caps = capabilities();
    caps.limits = RtpConfigurationLimits {
        max_bindings: 2,
        max_active_encodings: 2,
        max_retained_encodings: 2,
    };
    connection.set_rtp_capabilities(caps).unwrap();
    let first = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 100)],
    };
    connection.configure_rtp(first.clone()).unwrap();
    connection
        .configure_rtp(RtpConfiguration::default())
        .unwrap();
    let second = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 200)],
    };
    connection.configure_rtp(second.clone()).unwrap();
    let third = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 300)],
    };
    assert_eq!(
        connection.configure_rtp(third.clone()),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(
        connection.configure_rtp(third),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(connection.rtp_configuration(), Some(&second));
    connection.configure_rtp(first.clone()).unwrap();
    assert_eq!(connection.rtp_configuration(), Some(&first));
    assert_eq!(
        connection.stats().senders.len(),
        2,
        "re-admission retains the physical record"
    );
}

#[test]
fn physical_mutation_and_cross_direction_reuse_are_not_fresh_bindings() {
    let mut connection = connection();
    connection.set_rtp_capabilities(capabilities()).unwrap();
    let first = RtpConfiguration {
        inbound: vec![binding("v", 100)],
        outbound: vec![],
    };
    connection.configure_rtp(first.clone()).unwrap();
    connection
        .configure_rtp(RtpConfiguration::default())
        .unwrap();
    let reversed = RtpConfiguration {
        inbound: vec![],
        outbound: first.inbound.clone(),
    };
    assert_eq!(
        connection.configure_rtp(reversed),
        Err(RtpConfigurationError::Collision)
    );
    let mut changed = first.clone();
    changed.inbound[0].mid = "reused".into();
    assert_eq!(
        connection.configure_rtp(changed),
        Err(RtpConfigurationError::Collision)
    );
    connection.configure_rtp(first.clone()).unwrap();
    let mut extension_order = first;
    extension_order.inbound[0].extensions.reverse();
    connection.configure_rtp(extension_order.clone()).unwrap();
    assert_eq!(connection.rtp_configuration(), Some(&extension_order));
}

#[test]
fn malformed_and_unsupported_envelopes_cannot_partially_replace_accepted_media() {
    let mut connection = connection();
    connection.set_rtp_capabilities(capabilities()).unwrap();
    let configured = RtpConfiguration {
        inbound: vec![binding("v", 100)],
        outbound: vec![binding("a", 200)],
    };
    connection.configure_rtp(configured.clone()).unwrap();
    let mut cases = Vec::new();
    let mut next = configured.clone();
    next.outbound[0].mid = "bad\r\na=ssrc:1".into();
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.payload_type = 80;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.repair_payload_type = Some(96);
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].encodings[0].repair = None;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].extensions[1].id = 1;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].extensions.remove(1);
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.parameters.name = "unknown".into();
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].encodings[0].rid = Some("invalid rid".into());
    cases.push(next);
    for invalid in cases {
        assert!(connection.configure_rtp(invalid).is_err());
        assert_eq!(connection.rtp_configuration(), Some(&configured));
        assert_eq!(connection.stats().senders.len(), 1);
    }
}

fn wire(
    primary: u32,
    payload_type: u8,
    sequence: u16,
    mid: &str,
    rid: Option<&str>,
    payload: &[u8],
) -> Vec<u8> {
    let mut bytes = vec![0x90, payload_type];
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&1234u32.to_be_bytes());
    bytes.extend_from_slice(&primary.to_be_bytes());
    let mut extensions = vec![0x10];
    extensions.extend_from_slice(mid.as_bytes());
    if let Some(rid) = rid {
        extensions.push(0x30);
        extensions.extend_from_slice(rid.as_bytes());
    }
    while !extensions.len().is_multiple_of(4) {
        extensions.push(0);
    }
    bytes.extend_from_slice(&[0xbe, 0xde]);
    bytes.extend_from_slice(&u16::try_from(extensions.len() / 4).unwrap().to_be_bytes());
    bytes.extend(extensions);
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn configured_ingress_admits_only_exact_primary_repair_mid_payload_and_rid() {
    let now = Instant::now();
    let mut ingress = crate::ingress::IngressOwner::new(
        Box::new([]),
        32,
        65536,
        9,
        crate::receive_feedback::ReceiveFeedback::new(Some(PacketFeedbackKind::TransportWide), now),
    );
    let mut media = binding("v", 100);
    media.encodings[0].rid = Some("h".into());
    let physical = PhysicalBinding {
        outbound: false,
        binding: media,
    };
    ingress.accept_rtp(now, wire(100, 96, 1, "v", Some("h"), b"undeclared"), None);
    assert!(ingress.poll_event().is_none());
    let update = ingress
        .stage_configuration(&[(physical.clone(), true)], 4)
        .unwrap();
    ingress.commit_configuration(update);
    for invalid in [
        wire(200, 96, 1, "v", Some("h"), b"unknown"),
        wire(100, 96, 1, "x", Some("h"), b"wrong-mid"),
        wire(100, 98, 1, "v", Some("h"), b"wrong-pt"),
        wire(100, 96, 1, "v", Some("l"), b"wrong-rid"),
    ] {
        ingress.accept_rtp(now, invalid, None);
        assert!(ingress.poll_event().is_none());
    }
    ingress.accept_rtp(
        now,
        wire(100, 96, 1, "v", Some("h"), b"primary-source"),
        None,
    );
    assert!(matches!(
        ingress.poll_event(),
        Some(crate::Event::EncodingDiscovered(_))
    ));
    let Some(crate::Event::Rtp { mid, rid, packet }) = ingress.poll_event() else {
        panic!("configured primary")
    };
    assert_eq!(mid.as_ref(), "v");
    assert_eq!(rid.as_deref(), Some("h"));
    assert_eq!(packet.payload.as_ref(), b"primary-source");
    // RTX is normalized to the configured primary, not admitted as a new source.
    ingress.accept_rtp(
        now,
        wire(101, 97, 50, "v", None, b"\0\x02repair-source"),
        None,
    );
    let Some(crate::Event::Rtp { packet, .. }) = ingress.poll_event() else {
        panic!("configured repair")
    };
    assert_eq!(packet.header.ssrc, 100);
    assert_eq!(packet.header.sequence, 2);
    assert_eq!(packet.payload.as_ref(), b"repair-source");
    let update = ingress.stage_configuration(&[], 4).unwrap();
    ingress.commit_configuration(update);
    ingress.accept_rtp(now, wire(100, 96, 3, "v", None, b"withdrawn"), None);
    assert!(ingress.poll_event().is_none());
    assert!(ingress.stats()[0].retired);
    let update = ingress.stage_configuration(&[(physical, true)], 4).unwrap();
    ingress.commit_configuration(update);
    ingress.accept_rtp(now, wire(100, 96, 3, "v", None, b"re-admitted"), None);
    let Some(crate::Event::Rtp { packet, .. }) = ingress.poll_event() else {
        panic!("re-admitted primary without rediscovery")
    };
    assert_eq!(packet.payload.as_ref(), b"re-admitted");
    assert_eq!(ingress.stats()[0].received_packets, 3);
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "exercise the immutable public RTP write payload contract"
)]
fn withdrawal_discards_uncommitted_egress_and_exact_readmission_retains_one_sender() {
    let now = Instant::now();
    let mut egress = crate::egress::MediaEgress::new(
        Box::new([]),
        &[7; 32],
        65536,
        65536,
        now,
        crate::time::ProtocolClock::new(now, std::time::UNIX_EPOCH),
    );
    let physical = PhysicalBinding {
        outbound: true,
        binding: binding("v", 100),
    };
    let packet = || crate::RtpWrite {
        sequence: 65536,
        timestamp: 90000,
        wallclock: now,
        marker: true,
        payload: bytes::Bytes::from_static(b"opaque-source"),
        extensions: vec![],
    };
    let update = egress
        .stage_configuration(&[(physical.clone(), true)], &[7; 32], 4)
        .unwrap();
    egress.commit_configuration(update);
    egress.admit(now, "v", packet()).unwrap();
    assert_eq!(egress.stats().1[0].queued_packets, 1);
    let update = egress.stage_configuration(&[], &[7; 32], 4).unwrap();
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1[0].queued_packets, 0);
    assert_eq!(
        egress.admit(now, "v", packet()).unwrap_err().0,
        crate::WriteError::UnknownStream
    );
    let update = egress
        .stage_configuration(&[(physical, true)], &[7; 32], 4)
        .unwrap();
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1.len(), 1);
    assert_eq!(egress.stats().1[0].ssrc, 100);
    let mut next = packet();
    next.sequence = next.sequence.saturating_add(1);
    egress.admit(now, "v", next).unwrap();
    assert_eq!(egress.stats().1[0].queued_packets, 1);
}

#[test]
fn codec_parameter_model_accepts_opus_vp8_h264_and_representative_vp9_without_enum_conversion() {
    let mut caps = capabilities();
    let mut opus = codec();
    opus.name = "opus".into();
    opus.clock_rate = 48000;
    opus.channels = Some(2);
    opus.fmtp.insert("useinbandfec".into(), "1".into());
    let mut h264 = codec();
    h264.name = "h264".into();
    h264.fmtp.insert("packetization-mode".into(), "1".into());
    h264.fmtp.insert("profile-level-id".into(), "42e01f".into());
    let mut vp9 = codec();
    vp9.name = "vp9".into();
    vp9.fmtp.insert("profile-id".into(), "0".into());
    caps.codecs.extend([opus, h264, vp9]);
    caps.validate().unwrap();
    for parameters in &caps.codecs {
        let mut media = binding("v", 100);
        media.kind = if parameters.name == "opus" {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        media.codec.parameters = parameters.clone();
        let state = RtpConfigurationState {
            capabilities: Some(caps.clone()),
            ..RtpConfigurationState::default()
        };
        let config = RtpConfiguration {
            inbound: vec![media.clone()],
            outbound: vec![],
        };
        let staged = state.stage(&config).unwrap();
        assert_eq!(staged[0].0.binding.codec.parameters, *parameters);
        assert_eq!(
            staged[0].0.ingress_facts(0).payloads[0],
            (96, parameters.clock_rate)
        );
    }
}
