#![allow(
    clippy::unwrap_used,
    reason = "fixture assertions establish construction invariants"
)]

use super::*;
use ExtensionKind::*;
use Profile::*;

#[test]
fn source_content_ignores_hop_budget_but_not_timestamp() {
    let header = Header {
        profile: H264,
        marker: true,
        sequence: 55,
        timestamp: 99,
    };
    let packet = FabricPacket::new(header, &[], [42]).unwrap();
    assert!(
        packet
            .arrival_view(0)
            .unwrap()
            .same_content(&packet.arrival_view(6).unwrap())
    );
    let conflict = FabricPacket::new(
        Header {
            timestamp: 100,
            ..header
        },
        &[],
        [42],
    )
    .unwrap();
    assert!(!packet.same_content(&conflict));
}

#[test]
fn rejects_invalid_source_extension_values_and_duplicate_kinds() {
    let header = Header {
        profile: H264,
        marker: false,
        sequence: 0,
        timestamp: 0,
    };
    for (kind, value) in [
        (DependencyDescriptor, &[0; 2][..]),
        (AbsoluteCaptureTime, &[0; 9][..]),
        (VideoOrientation, &[0x10][..]),
        (VideoLayersAllocation, &[0x01, 0, 0x80][..]), // incomplete LEB128
        (VideoLayersAllocation, &[0x01, 0, 0, 0, 0][..]), // incomplete geometry
        (VideoLayersAllocation, &[0xc1, 0, 0][..]),    // stream index out of range
        (
            VideoLayersAllocation,
            &[1, 0, 0x80, 0x80, 0x80, 0x80, 0x10][..],
        ), // bitrate overflow
        (
            VideoLayersAllocation,
            &[1, 0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0][..],
        ), // ninth LEB128 byte
        (VideoContentType, &[0x02][..]),
        (ColorSpace, &[0; 26][..]),
        (PlayoutDelay, &[0x01, 0, 0][..]),
    ] {
        assert_eq!(
            FabricPacket::new(header, &[Extension { kind, value }], [1]),
            Err(WireError::ExtensionValue)
        );
    }
    let same = Extension {
        kind: VideoOrientation,
        value: &[0],
    };
    assert_eq!(
        FabricPacket::new(header, &[same, same], [1]),
        Err(WireError::ExtensionLayout)
    );
    assert_eq!(
        FabricPacket::new(header, &[], []),
        Err(WireError::PayloadLength)
    );
}
