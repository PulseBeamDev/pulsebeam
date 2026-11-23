use std::{num::NonZeroU16, time::Duration};

use pulsebeam_core::fabric::{MAX_WAN_RTP_BYTES, WireError};

use super::*;

const ACT: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
const AL: &str = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";
const ENCRYPT: &str = "urn:ietf:params:rtp-hdrext:encrypt";

fn ext(id: u8, uri: &str, attributes: Option<&str>) -> RtpReceiveExtension {
    RtpReceiveExtension {
        id,
        uri: uri.into(),
        attributes: attributes.map(Box::from),
    }
}

fn limits() -> Limits {
    Limits {
        positions: NonZeroU16::new(64).unwrap(),
        max_age: Duration::from_millis(120),
        max_idle: Duration::from_secs(2),
    }
}

fn h264() -> EndpointBinding {
    EndpointBinding {
        mid: "v".into(),
        rid: Some("h".into()),
        primary_ssrc: 11,
        codec: RtpReceiveCodec {
            payload_type: 102,
            name: "h264".into(),
            clock_rate: 90_000,
            channels: None,
            profile: Some(CodecProfile::H264 {
                profile_level_id: Some(0x42e01f),
                packetization_mode: 1,
                level_asymmetry_allowed: false,
            }),
        },
        repair: Some(RepairBinding {
            ssrc: 22,
            payload_type: 120,
        }),
        receive_extensions: Box::new([
            ext(12, pulsebeam_core::dd::URI, None),
            ext(10, ACT, None),
            ext(4, ENCRYPT, Some(AL)),
            ext(7, AL, Some("vad=off")),
        ]),
        extension_form: ExtensionForm::TwoByte,
        allow_mixed_extensions: false,
        qualified_extensions: [
            ExtensionKind::DependencyDescriptor,
            ExtensionKind::AbsoluteCaptureTime,
            ExtensionKind::AudioLevel,
        ]
        .into_iter()
        .collect(),
        capture_clock_ssrc: Some(42),
        allow_csrc_stripping: true,
    }
}

fn wire(payload: &[u8], is_repair: bool) -> Wire<'_> {
    Wire {
        header: RtpHeader {
            sequence: 90,
            ssrc: if is_repair { 22 } else { 11 },
            payload_type: if is_repair { 120 } else { 102 },
            timestamp: 100,
            marker: true,
        },
        padding: 0,
        first_csrc: None,
        extension_profile: Some(0x100e),
        is_repair,
        payload,
    }
}

impl RtpIngressShadow {
    fn normalize_test<'a>(
        &self,
        wire: Wire<'a>,
        elements: impl IntoIterator<Item = (u8, &'a [u8])>,
    ) -> Result<Option<(u16, Normalized<'a>)>, DropReason> {
        self.normalize_encoding(wire, elements.into_iter())
    }
}

#[test]
fn dd_required_encoding_drops_missing_or_malformed_descriptors() {
    let endpoint = h264();
    let binding = pulsebeam_rtc::RtpReceiveBinding {
        mid: &endpoint.mid,
        rid: endpoint.rid.as_deref(),
        primary_ssrc: endpoint.primary_ssrc,
        primary_codec: &endpoint.codec,
        repair_payload_type: endpoint.repair.map(|repair| repair.payload_type),
        declared_repair_ssrc: endpoint.repair.map(|repair| repair.ssrc),
        receive_extensions: &endpoint.receive_extensions,
        allow_mixed_extensions: true,
    };
    assert!(
        RtpIngressShadow::bind_dd_encoding(pulsebeam_rtc::RtpReceiveBinding {
            receive_extensions: &endpoint.receive_extensions[1..],
            ..binding
        })
        .is_none()
    );
    let ingress = RtpIngressShadow::bind_dd_encoding(binding).unwrap();
    for (repair, payload) in [(false, &[3][..]), (true, &[0, 9, 3][..])] {
        assert!(matches!(
            ingress.normalize_test(wire(payload, repair), []),
            Err(DropReason::MissingDependencyDescriptor)
        ));
        assert!(matches!(
            ingress.normalize_test(wire(payload, repair), [(12, &[0xc0, 0, 1, 0x80][..])]),
            Err(DropReason::InvalidDependencyDescriptor(_))
        ));
        assert!(
            ingress
                .normalize_test(wire(payload, repair), [(12, &[0x80, 0, 1][..])])
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn normalized_repair_binds_osn_and_only_qualified_capture_clock() {
    let shadow = RtpIngressShadow::new(h264(), Anchors::default(), limits()).unwrap();
    let incoming = [0x12, 0x34, 0xa1, 0xa2];
    let hints = [
        (4, &[2, 3][..]),
        (10, &[0; 8][..]),
        (12, &[0x80, 0, 1][..]),
        (7, &[1, 2][..]),
    ];
    for (csrc, expect_act) in [(None, false), (Some(43), false), (Some(42), true)] {
        let packet = Wire {
            first_csrc: csrc,
            ..wire(&incoming, true)
        };
        let (sequence, normalized) = shadow.normalize_test(packet, hints).unwrap().unwrap();
        assert_eq!(sequence, 0x1234);
        let Input::Media(media) = normalized.as_input() else {
            panic!("expected media");
        };
        assert_eq!(media.payload, &[0xa1, 0xa2]);
        assert_eq!(media.timestamp, 100);
        assert!(media.marker && media.is_repair);
        assert_eq!(media.extensions.len(), if expect_act { 2 } else { 1 });
        assert_eq!(
            media.extensions.last().unwrap().kind,
            ExtensionKind::DependencyDescriptor
        );
        assert_eq!(
            media
                .extensions
                .iter()
                .any(|ext| ext.kind == ExtensionKind::AbsoluteCaptureTime),
            expect_act
        );
    }
}

#[test]
fn fixed_two_byte_rejects_changed_form_and_duplicate_selected_ids() {
    let shadow = RtpIngressShadow::new(h264(), Anchors::default(), limits()).unwrap();
    let wrong_form = Wire {
        extension_profile: Some(0xbede),
        ..wire(b"opaque", false)
    };
    assert!(matches!(
        shadow.normalize_test(wrong_form, []),
        Err(DropReason::ExtensionForm)
    ));
    let duplicate = [(12, &[0xc0, 0, 1][..]), (12, &[0xc0, 0, 1][..])];
    assert!(matches!(
        shadow.normalize_test(wire(b"opaque", false), duplicate),
        Err(DropReason::DuplicateExtension)
    ));
    let mut binding = h264();
    binding.allow_csrc_stripping = false;
    let shadow = RtpIngressShadow::new(binding, Anchors::default(), limits()).unwrap();
    let mixer = Wire {
        first_csrc: Some(42),
        ..wire(b"opaque", false)
    };
    assert!(matches!(
        shadow.normalize_test(mixer, []),
        Err(DropReason::MixerAttribution)
    ));
}

#[test]
fn remapped_hint_has_same_canonical_value_across_negotiated_forms() {
    let hint = [(12, &[0xc0, 0, 1][..])];
    for (form, mixed, expected) in [
        (ExtensionForm::OneByte, false, [true, false]),
        (ExtensionForm::TwoByte, false, [false, true]),
        (ExtensionForm::Mixed, true, [true, true]),
    ] {
        let mut binding = h264();
        binding.extension_form = form;
        binding.allow_mixed_extensions = mixed;
        let shadow = RtpIngressShadow::new(binding, Anchors::default(), limits()).unwrap();
        for (profile, allowed) in [0xbede, 0x1007].into_iter().zip(expected) {
            let input = Wire {
                extension_profile: Some(profile),
                ..wire(b"opaque", false)
            };
            let result = shadow.normalize_test(input, hint);
            if allowed {
                let (_, normalized) = result.unwrap().unwrap();
                let Input::Media(media) = normalized.as_input() else {
                    panic!("media expected");
                };
                assert_eq!(
                    media.extensions,
                    &[Extension {
                        kind: ExtensionKind::DependencyDescriptor,
                        value: hint[0].1
                    }]
                );
            } else {
                assert!(matches!(result, Err(DropReason::ExtensionForm)));
            }
        }
    }
}

#[test]
fn only_padded_primary_reserves_position() {
    let shadow = RtpIngressShadow::new(h264(), Anchors::default(), limits()).unwrap();
    for repair in [false, true] {
        let padded = Wire {
            padding: 20,
            ..wire(&[], repair)
        };
        let input = shadow.normalize_test(padded, []).unwrap();
        if repair {
            assert!(input.is_none());
        } else {
            let (sequence, normalized) = input.unwrap();
            assert_eq!(sequence, 90);
            assert!(matches!(normalized.as_input(), Input::PrimaryPadding));
        }
    }
    assert!(matches!(
        shadow.normalize_test(wire(&[], false), []),
        Err(DropReason::EmptyMedia)
    ));
    assert!(matches!(
        shadow.normalize_test(
            Wire {
                padding: 20,
                ..wire(&[], false)
            },
            [(12, &[0xc0, 0, 1][..])]
        ),
        Err(DropReason::PaddingWithDependencyDescriptor)
    ));
    let malformed_hint = [(12, &[0, 1][..])];
    assert!(matches!(
        shadow.normalize_test(
            Wire {
                padding: 20,
                ..wire(&[], false)
            },
            malformed_hint
        ),
        Err(DropReason::Admission(Rejected::Wire(
            WireError::ExtensionValue
        )))
    ));
    assert!(matches!(
        shadow.normalize_test(wire(&[1], false), [(12, &[0xc0, 0, 1, 0x80][..])]),
        Err(DropReason::InvalidDependencyDescriptor(
            pulsebeam_core::dd::DdReadError::Truncated
        ))
    ));
    for payload in [&[0_u8][..], &[0, 1][..]] {
        assert!(matches!(
            shadow.normalize_test(wire(payload, true), []),
            Err(DropReason::RepairPayload)
        ));
    }
}

#[test]
fn negotiated_repair_changes_are_rejected_even_before_repair_arrives() {
    let mut binding = h264();
    assert!(binding.matches_repair(Some(120), Some(22)));
    assert!(binding.matches_repair(Some(120), None));
    assert!(!binding.matches_repair(Some(121), Some(22)));
    assert!(!binding.matches_repair(Some(120), Some(23)));
    assert!(!binding.matches_repair(None, None));
    binding.repair = None;
    assert!(binding.matches_repair(None, None));
    assert!(!binding.matches_repair(Some(120), None));
}

#[test]
fn installed_codec_and_extmap_have_to_be_qualified() {
    let mut binding = h264();
    binding.codec.profile = Some(CodecProfile::H264 {
        profile_level_id: None,
        packetization_mode: 0,
        level_asymmetry_allowed: false,
    });
    assert!(matches!(
        RtpIngressShadow::new(binding, Anchors::default(), limits()),
        Err(BindError::Codec)
    ));
    let mut binding = h264();
    binding.extension_form = ExtensionForm::Mixed;
    assert!(matches!(
        RtpIngressShadow::new(binding, Anchors::default(), limits()),
        Err(BindError::Extensions)
    ));
    let mut binding = h264();
    binding.receive_extensions = Box::new([
        ext(8, pulsebeam_core::dd::URI, None),
        ext(9, pulsebeam_core::dd::URI, None),
    ]);
    assert!(matches!(
        RtpIngressShadow::new(binding, Anchors::default(), limits()),
        Err(BindError::Extensions)
    ));
}

#[test]
fn av1_binding_uses_av1_wire_profile_or_rejects_unbounded_level() {
    let mut binding = h264();
    binding.codec.name = "av1".into();
    binding.codec.profile = Some(CodecProfile::Av1 {
        profile: 1,
        level_idx: 31,
        tier: 0,
    });
    assert!(matches!(
        RtpIngressShadow::new(binding, Anchors::default(), limits()),
        Err(BindError::Codec)
    ));
    let mut binding = h264();
    binding.codec.name = "av1".into();
    binding.codec.profile = Some(CodecProfile::Av1 {
        profile: 1,
        level_idx: 12,
        tier: 0,
    });
    let mut shadow = RtpIngressShadow::new(binding, Anchors::default(), limits()).unwrap();
    let (sequence, media) = shadow
        .normalize_test(wire(b"opaque AV1", false), [])
        .unwrap()
        .unwrap();
    let now = Instant::now();
    assert!(matches!(
        shadow.observer.as_mut().unwrap().admit(now, now, 1, sequence, media.as_input()),
        Ok(Admission::Media { packet, .. }) if packet.header().profile == Profile::Av1 && packet.payload() == b"opaque AV1"
    ));
}

#[test]
fn co_occurring_metadata_is_not_stripped_to_fit() {
    let mut binding = h264();
    let extra = [
        (
            6,
            "urn:3gpp:video-orientation",
            ExtensionKind::VideoOrientation,
        ),
        (
            13,
            "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
            ExtensionKind::VideoLayersAllocation,
        ),
        (
            14,
            "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type",
            ExtensionKind::VideoContentType,
        ),
        (
            16,
            "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            ExtensionKind::ColorSpace,
        ),
    ];
    let mut extmap = binding.receive_extensions.into_vec();
    binding
        .qualified_extensions
        .retain(|kind| *kind != ExtensionKind::AudioLevel);
    for (id, uri, kind) in extra {
        extmap.push(ext(id, uri, None));
        binding.qualified_extensions.push(kind);
    }
    binding.receive_extensions = extmap.into_boxed_slice();
    let mut shadow = RtpIngressShadow::new(binding, Anchors::default(), limits()).unwrap();
    // Four streams of four spatial layers with four temporal rates each. Rates
    // occupy 170 bytes and geometry occupies 80, plus five allocation bytes.
    let mut vla = vec![0x3f, 0xff, 0xff, 0xff, 0xff];
    for _ in 0..42 {
        vla.extend_from_slice(&[0xff, 0xff, 1]);
    }
    for _ in 0..22 {
        vla.extend_from_slice(&[0xff, 1]);
    }
    vla.extend_from_slice(&[0; 80]);
    assert_eq!(vla.len(), 255);
    let hints = [
        (12, &[0; 255][..]), // DD length envelope; no claim of qualified template context.
        (10, &[0; 16][..]),
        (6, &[0][..]),
        (13, vla.as_slice()),
        (14, &[1][..]),
        (16, &[0; 28][..]),
    ];
    // 257 DD + 18 ACT + 3 CVO + 257 VLA + 3 content + 30 HDR.
    let budget = MAX_WAN_RTP_BYTES.checked_sub(16 + 568).unwrap();
    let now = Instant::now();
    let oversized = [0; MAX_WAN_RTP_BYTES];
    let input = Wire {
        first_csrc: Some(42),
        ..wire(&oversized[..budget + 1], false)
    };
    let (sequence, media) = shadow.normalize_test(input, hints).unwrap().unwrap();
    let Admission::Media {
        first: true,
        packet,
        ..
    } = shadow
        .observer
        .as_mut()
        .unwrap()
        .admit(now, now, 1, sequence, media.as_input())
        .unwrap()
    else {
        panic!("the logical packet must retain all authenticated media");
    };
    assert_eq!(packet.wan_plaintext_len(), MAX_WAN_RTP_BYTES + 1);
    assert_eq!(packet.legacy_wire_len(), Err(WireError::Oversize));
    assert_eq!(packet.extensions().count(), hints.len());
}

#[test]
fn endpoint_identity_uses_one_original_across_retry_wrap_and_cutoff() {
    for peer_repair in [false, true] {
        prove_endpoint_identity(peer_repair);
    }
}

fn prove_endpoint_identity(peer_repair: bool) {
    use crate::route::fabric::local::{Emission, Rejection, SourceFanout};
    use pulsebeam_runtime::time::{Duration, Instant as ShardInstant};

    let mut fanout = SourceFanout::new(Emission::new(Profile::H264, &[]).unwrap());
    let identity = MediaIdentity::new(
        Profile::H264,
        Anchors::default(),
        Limits {
            positions: NonZeroU16::new(4).unwrap(),
            max_age: Duration::from_millis(120),
            max_idle: Duration::from_secs(2),
        },
    )
    .unwrap();
    assert!(fanout.bind_media_identity(identity));
    let mut participants = slotmap::SlotMap::with_key();
    let destination = participants.insert(());
    let branch = fanout
        .install_branch(destination, Emission::new(Profile::H264, &[]).unwrap())
        .unwrap();
    let now = ShardInstant::now();
    let input =
        |arrival: ShardInstant, sequence, bytes: Option<&[u8]>, protected_bytes| MediaIngress {
            arrival: arrival.into(),
            ready_at: arrival.into(),
            ingress_ntp32: 1,
            position: Position::LowSequence(sequence),
            protected_bytes: NonZeroU16::new(protected_bytes),
            media: bytes.map(|payload| OwnedCopy {
                payload: payload.into(),
                marker: true,
                timestamp: 30,
                is_repair: false,
                extensions: ArrayVec::new(),
                extension_width: None,
            }),
        };
    let (position, hops, report) = fanout
        .offer_endpoint_media(
            now,
            &input(now, u16::MAX, Some(b"original"), 100),
            peer_repair,
            |arrival, _| assert_eq!(arrival, now),
        )
        .unwrap()
        .unwrap();
    assert_eq!(position, u64::from(u16::MAX));
    assert_eq!(report.enqueued, 1);
    let first = fanout
        .transit(position, hops)
        .unwrap()
        .packet
        .payload()
        .as_ptr();
    let retry = now.checked_add(Duration::from_millis(1)).unwrap();
    let (_, _, report) = fanout
        .offer_endpoint_media(
            retry,
            &input(retry, u16::MAX, Some(b"original"), 110),
            peer_repair,
            |_, _| panic!("duplicate must not advance publisher clock"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(report.duplicates, 1);
    let transit = fanout.transit(position, hops).unwrap();
    assert_eq!(transit.work.arrival(), now);
    assert_eq!(transit.packet.payload().as_ptr(), first);
    assert_eq!(
        transit.packet.source_protected_bytes(),
        NonZeroU16::new(100)
    );

    assert!(
        fanout
            .offer_endpoint_media(
                retry,
                &input(retry, 3, None, 0),
                peer_repair,
                |_, _| panic!("padding is not a clock sample")
            )
            .unwrap()
            .is_none()
    );
    if peer_repair {
        let later = now.checked_add(Duration::from_millis(121)).unwrap();
        let (original, expiry) = fanout.cached_peer_media(position, None, later).unwrap();
        assert_eq!(original.payload().as_ptr(), first);
        assert!(fanout.cached_peer_media(position, None, expiry).is_none());
        assert!(fanout.take(later, branch, destination).is_none());
    } else {
        assert!(fanout.transit(position, hops).is_none());
        assert!(fanout.take(retry, branch, destination).is_none());
    }
    assert_eq!(
        fanout.offer_endpoint_media(
            retry,
            &input(retry, u16::MAX, Some(b"original"), 100),
            peer_repair,
            |_, _| panic!("stale packet must not advance publisher clock"),
        ),
        Err(Rejection::Stale)
    );
    let (position, _, _) = fanout
        .offer_endpoint_media(
            retry,
            &input(retry, 4, Some(b"new"), 120),
            peer_repair,
            |arrival, _| assert_eq!(arrival, retry),
        )
        .unwrap()
        .unwrap();
    assert_eq!(position, 65_540);
    assert_eq!(
        fanout.offer_endpoint_media(
            retry,
            &input(retry, 4, Some(b"changed"), 120),
            peer_repair,
            |_, _| panic!("conflicting packet must not advance publisher clock")
        ),
        Err(Rejection::Conflict)
    );
    assert!(fanout.transit(position, hops).is_none());
    assert!(fanout.take(retry, branch, destination).is_none());
}
