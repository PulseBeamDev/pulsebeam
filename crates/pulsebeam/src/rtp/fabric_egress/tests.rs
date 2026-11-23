use super::*;
use pulsebeam_core::fabric::{Extension, Header};

fn packet<'a>(profile: Profile, extensions: &[Extension<'a>]) -> FabricPacket<&'static [u8]> {
    FabricPacket::new(
        Header {
            profile,
            marker: true,
            sequence: 6,
            timestamp: 7,
            ingress_ntp32: 8,
        },
        extensions,
        &b"opaque"[..],
    )
    .unwrap()
}

fn policy(vad: Vad, capture: CaptureClock, same_topology: bool) -> SourceMetadata {
    SourceMetadata {
        vad,
        capture,
        same_topology,
    }
}

#[test]
fn output_vad_default_and_asymmetric_omission() {
    let packet = packet(
        Profile::Opus,
        &[Extension {
            kind: ExtensionKind::AudioLevel,
            value: &[0x95],
        }],
    );
    for (attributes, source, emitted) in [
        (None, Vad::On, true),
        (None, Vad::Off, false),
        (Some("vad=off"), Vad::Off, true),
        (Some("vad=off"), Vad::On, true),
    ] {
        let endpoint = EndpointHints::new(
            Profile::Opus,
            ExtensionForm::OneByte,
            &[Mapping {
                id: 12,
                uri: "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
                attributes,
            }],
        )
        .unwrap();
        let projection = endpoint.project(policy(source, CaptureClock::Unmapped, true), &packet);
        assert_eq!(
            projection.iter().next(),
            emitted.then_some((12, &[0x95][..]))
        );
    }
}

#[test]
fn capture_clock_offset_is_signed_and_missing_is_not_zero() {
    let endpoint = EndpointHints::new(
        Profile::Opus,
        ExtensionForm::TwoByte,
        &[Mapping {
            id: 42,
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time",
            attributes: None,
        }],
    )
    .unwrap();
    let original = [1, 2, 3, 4, 5, 6, 7, 8, 0xff, 0xff, 0xff, 0xfe, 0, 0, 0, 0];
    let shifted = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 1, 0, 0, 0, 0];
    let long = packet(
        Profile::Opus,
        &[Extension {
            kind: ExtensionKind::AbsoluteCaptureTime,
            value: &original,
        }],
    );
    let short = packet(
        Profile::Opus,
        &[Extension {
            kind: ExtensionKind::AbsoluteCaptureTime,
            value: &[1, 2, 3, 4, 5, 6, 7, 8],
        }],
    );
    for (packet, clock, expected) in [
        (&long, CaptureClock::SameSender, Some(original.as_slice())),
        (
            &long,
            CaptureClock::SenderDelta(3_i64 << 32),
            Some(shifted.as_slice()),
        ),
        (&long, CaptureClock::SenderDelta(i64::MIN), None),
        (&long, CaptureClock::Unmapped, None),
        (&short, CaptureClock::SenderDelta(3_i64 << 32), None),
    ] {
        let projection = endpoint.project(policy(Vad::On, clock, true), packet);
        assert_eq!(projection.iter().next(), expected.map(|bytes| (42, bytes)));
    }
}

#[test]
fn rewritten_dependency_descriptor_must_fit_the_negotiated_form() {
    let mappings = [Mapping {
        id: 14,
        uri: pulsebeam_core::dd::URI,
        attributes: None,
    }];
    for (form, max) in [
        (ExtensionForm::OneByte, 16),
        (ExtensionForm::TwoByte, 255),
        (ExtensionForm::Mixed, 255),
    ] {
        let endpoint = EndpointHints::new(Profile::H264, form, &mappings).unwrap();
        assert!(endpoint.dependency_hint(2).is_none());
        assert_eq!(endpoint.dependency_hint(3).unwrap().id, 14);
        assert_eq!(endpoint.dependency_hint(max).unwrap().id, 14);
        assert!(endpoint.dependency_hint(max + 1).is_none());
    }
    let unmapped = EndpointHints::new(Profile::H264, ExtensionForm::Mixed, &[]).unwrap();
    assert!(unmapped.dependency_hint(3).is_none());
}

#[test]
fn whole_source_hints_respect_topology_form_and_presence() {
    let hdr = [0; 28];
    let mappings = [
        Mapping {
            id: 4,
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
            attributes: None,
        },
        Mapping {
            id: 5,
            uri: "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            attributes: None,
        },
        Mapping {
            id: 7,
            uri: pulsebeam_core::dd::URI,
            attributes: None,
        },
    ];
    let present = packet(
        Profile::H264,
        &[
            Extension {
                kind: ExtensionKind::ColorSpace,
                value: &hdr,
            },
            Extension {
                kind: ExtensionKind::VideoLayersAllocation,
                value: &[0],
            },
            Extension {
                kind: ExtensionKind::DependencyDescriptor,
                value: &[0xc0, 0, 1],
            },
        ],
    );
    let absent = packet(Profile::H264, &[]);
    for (form, same, expected) in [
        (ExtensionForm::OneByte, true, vec![(4, &[0][..])]),
        (
            ExtensionForm::TwoByte,
            true,
            vec![(4, &[0][..]), (5, hdr.as_slice())],
        ),
        (ExtensionForm::Mixed, false, vec![(5, hdr.as_slice())]),
    ] {
        let endpoint = EndpointHints::new(Profile::H264, form, &mappings).unwrap();
        let policy = policy(Vad::On, CaptureClock::Unmapped, same);
        assert_eq!(
            endpoint
                .project(policy, &present)
                .iter()
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(endpoint.project(policy, &absent).iter().count(), 0);
    }
}
