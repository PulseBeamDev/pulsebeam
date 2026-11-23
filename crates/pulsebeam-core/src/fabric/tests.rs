#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "fixture offsets and assertions pin exact immutable wire bytes"
)]

use std::ops::Range;

use data_encoding::{BASE64, HEXLOWER};

use super::*;
use ExtensionKind::*;
use Profile::*;

// The image bytes below live independently of the codec. Expected field
// boundaries are absolute offsets, not results from its decoder or registry.
fn golden(name: &str) -> Vec<u8> {
    include_str!("appendix-a.vectors")
        .lines()
        .find_map(|line| {
            let mut words = line.split_ascii_whitespace();
            if words.next()? != name {
                return None;
            }
            let encoding = if words.next()? == "base64" {
                &BASE64
            } else {
                &HEXLOWER
            };
            Some(encoding.decode(words.next()?.as_bytes()).unwrap())
        })
        .unwrap()
}

struct Golden {
    name: &'static str,
    header: Header,
    payload_at: usize,
    metadata: &'static [(ExtensionKind, Range<usize>)],
}

macro_rules! image {
    ($name:literal, $pt:ident, $seq:expr, $ts:expr, $m:expr, $payload:expr $(, $ext:ident @ $begin:literal .. $end:literal)*) => {
        Golden {
            name: $name,
            header: Header {
                profile: $pt,
                sequence: $seq,
                timestamp: $ts,
                marker: $m,
                ingress_ntp32: 0x0102_0304,
            },
            payload_at: $payload,
            metadata: &[$(($ext, $begin..$end)),*],
        }
    };
}

const LOCAL: &[Golden] = &[
    image!("O1", Opus, u32::MAX, 4_294_966_336, true, 32, DependencyDescriptor @ 25..31),
    image!("O2", Opus, 0, 0, false, 32, DependencyDescriptor @ 25..31),
    image!("H1", H264, 0, 90_000, false, 36, DependencyDescriptor @ 25..33),
    image!("H2", H264, 1, 90_000, false, 28, DependencyDescriptor @ 25..28),
    image!("H3", H264, 2, 90_000, true, 28, DependencyDescriptor @ 25..28),
    image!("H4", H264, 3, 93_000, true, 28, DependencyDescriptor @ 25..28),
    image!("HF1", H264, 0, 90_000, false, 36, DependencyDescriptor @ 25..33),
    image!("HF2", H264, 1, 90_000, false, 28, DependencyDescriptor @ 25..28),
    image!("HF3", H264, 2, 90_000, false, 28, DependencyDescriptor @ 25..28),
    image!("HF4", H264, 3, 90_000, true, 28, DependencyDescriptor @ 25..28),
    image!("A1", Av1, 7, 90_000, true, 76, DependencyDescriptor @ 27..56, AbsoluteCaptureTime @ 58..74),
    image!("AF1", Av1, 7, 90_000, false, 32, DependencyDescriptor @ 25..31),
    image!("AF2", Av1, 8, 90_000, true, 28, DependencyDescriptor @ 25..28),
    image!("D1", Data, 0, 65_535, true, 24),
    image!("D2", Data, 1, 65_536, true, 24),
    image!("D3", Data, 2, 65_537, false, 24),
    image!("D4", Data, 3, 65_537, true, 24),
    image!("ND1", H264, 0, 90_000, false, 24),
    image!("ND2", Av1, 1, 90_000, true, 24),
    image!("X1", Opus, 0x21, 0, false, 28, AudioLevel @ 25..26),
    image!("X2", H264, 0x22, 90_000, true, 44,
        VideoOrientation @ 25..26, VideoLayersAllocation @ 27..31,
        VideoContentType @ 32..33, ColorSpace @ 34..38, PlayoutDelay @ 39..42),
    image!("X3", Av1, 0x23, 90_000, false, 56, ColorSpace @ 27..55),
    image!("X4", Av1, 0x24, 90_000, true, 48, VideoLayersAllocation @ 27..47),
    image!("X5", Opus, 0x25, 0, false, 36, AbsoluteCaptureTime @ 25..33),
    image!("X6", Opus, 0x26, 0, false, 48, DependencyDescriptor @ 25..28, AbsoluteCaptureTime @ 29..45),
];

impl Golden {
    fn extensions<'a>(&self, wire: &'a [u8]) -> impl DoubleEndedIterator<Item = Extension<'a>> {
        self.metadata.iter().map(move |(kind, range)| Extension {
            kind: *kind,
            value: &wire[range.clone()],
        })
    }
}

#[test]
fn decodes_rfc_images_against_independent_fields_and_offsets() {
    for expected in LOCAL {
        let wire = golden(expected.name);
        let packet = BorrowedPacket::decode_local(&wire).unwrap();
        assert_eq!(packet.header(), expected.header, "{}", expected.name);
        assert_eq!(packet.remaining_hops(), 7);
        assert_eq!(packet.payload(), &wire[expected.payload_at..]);
        assert!(packet.extensions().eq(expected.extensions(&wire)));
        assert_eq!(packet.legacy_wire_len(), Ok(wire.len()));
    }
}

#[test]
fn encodes_rfc_images_without_using_the_decoder() {
    for expected in LOCAL {
        let wire = golden(expected.name);
        // Source extmap iteration order need not match canonical ID order.
        let extensions: Vec<_> = expected.extensions(&wire).rev().collect();
        let packet =
            FabricPacket::new(expected.header, &extensions, &wire[expected.payload_at..]).unwrap();
        let mut output = [0xee; MAX_PACKET_BYTES];
        let size = packet.write_local(&mut output).unwrap();
        assert_eq!(&output[..size], wire, "{}", expected.name);
        assert!(output[size..].iter().all(|&byte| byte == 0xee));
    }
}

#[test]
fn hop_only_changes_key_and_h_and_network_ingress_restores_local_ssrc() {
    let source = golden("D2");
    let fixture = LOCAL.iter().find(|fixture| fixture.name == "D2").unwrap();
    let packet = FabricPacket::new(fixture.header, &[], &source[24..]).unwrap();
    let key = NonZeroU32::new(0x0123_0045).unwrap();
    let mut network = [0; 51];
    let mut local = [0; 51];
    packet.write_network(key, &mut network).unwrap();
    assert_eq!(network.as_slice(), golden("N1"));
    assert_eq!(packet.remaining_hops(), 7);
    assert_eq!(
        BorrowedPacket::decode_network(&source),
        Err(WireError::RouteKey)
    );
    assert_eq!(
        BorrowedPacket::decode_local(&network),
        Err(WireError::RouteKey)
    );
    let mut undecremented = network;
    undecremented[23] = 0xe0;
    assert_eq!(
        BorrowedPacket::decode_network(&undecremented),
        Err(WireError::NetworkHop)
    );
    for expected in (0..=6).rev() {
        let (decoded, received_key) = BorrowedPacket::decode_network(&network).unwrap();
        assert_eq!(received_key, key);
        assert_eq!(decoded.remaining_hops(), expected);
        assert_eq!(decoded.header(), fixture.header);
        assert_eq!(decoded.payload(), &source[24..]);
        decoded.write_local(&mut local).unwrap();
        let mut expectation = source.clone();
        expectation[23] = expected << 5;
        assert_eq!(local.as_slice(), expectation);
        if expected == 0 {
            assert_eq!(
                decoded.write_network(key, &mut local),
                Err(WireError::HopExhausted)
            );
        } else {
            let next_key = NonZeroU32::new(7).unwrap();
            decoded.write_network(next_key, &mut local).unwrap();
            expectation[8..12].copy_from_slice(&next_key.get().to_be_bytes());
            expectation[23] = (expected - 1) << 5;
            assert_eq!(local.as_slice(), expectation);
            // Check this hop, then continue with the original fixture route key.
            decoded.write_network(key, &mut local).unwrap();
            network = local;
        }
    }
}

#[test]
fn rejects_rfc_negative_vectors_and_all_reserved_hop_bits() {
    for (name, offset, byte) in [
        ("X1", 0, 0xb0),
        ("X1", 23, 0xe1),
        ("X1", 24, 0xa0),
        ("X1", 24, 0x10),
        ("X1", 26, 0x01),
        ("X1", 1, 0x61),
        ("X2", 39, 0x10),
        ("X3", 26, 0x1a),
    ] {
        let mut wire = golden(name);
        wire[offset] = byte;
        assert!(
            BorrowedPacket::decode_local(&wire).is_err(),
            "{name}[{offset}]"
        );
    }
    for (name, offset) in [("X1", 23), ("X4", 24)] {
        for bit in 0..5 {
            let mut wire = golden(name);
            wire[offset] |= 1 << bit;
            assert_eq!(
                BorrowedPacket::decode_local(&wire),
                Err(WireError::CoreMetadata)
            );
        }
    }
}

#[test]
fn rejects_noncanonical_extension_form_order_and_length() {
    let image = golden("X6");
    for end in 0..=48 {
        assert!(BorrowedPacket::decode_local(&image[..end]).is_err());
    }
    for (offset, byte) in [
        (12, 0x10), // unknown profile
        (15, 0xff), // declared block overruns image
        (16, 0x26), // no mandatory ID 1 at the start
        (24, 0x2e), // incorrect element extent
        (24, 0x00), // interior zero before later elements
        (24, 0xf0), // RFC 8285 stop ID is not canonical
    ] {
        let mut wire = image.clone();
        wire[offset] = byte;
        assert!(BorrowedPacket::decode_local(&wire).is_err(), "[{offset}]");
    }
    let mut extra_word = image.clone();
    extra_word[15] += 1;
    extra_word.splice(48..48, [0; 4]);
    assert_eq!(
        BorrowedPacket::decode_local(&extra_word),
        Err(WireError::ExtensionLayout)
    );

    let mut unnecessarily_two_byte = golden("ND1");
    unnecessarily_two_byte[12..24].copy_from_slice(&[0x10, 0, 0, 3, 1, 7, 1, 2, 3, 4, 0, 0]);
    unnecessarily_two_byte.splice(24..24, [0xe0, 0, 0, 0]);
    assert_eq!(
        BorrowedPacket::decode_local(&unnecessarily_two_byte),
        Err(WireError::ExtensionLayout)
    );

    // Swap two individually valid elements without changing any other bytes.
    let source = golden("X2");
    let mut reordered = source.clone();
    reordered[24..29].copy_from_slice(&source[26..31]);
    reordered[29..31].copy_from_slice(&source[24..26]);
    assert_eq!(
        BorrowedPacket::decode_local(&reordered),
        Err(WireError::ExtensionLayout)
    );

    let mut wrong_appbits = golden("X4");
    wrong_appbits[13] = 1;
    assert_eq!(
        BorrowedPacket::decode_local(&wrong_appbits),
        Err(WireError::ExtensionLayout)
    );

    for pt in (0..96).chain(100..128) {
        let mut wire = image.clone();
        wire[1] = pt;
        assert_eq!(
            BorrowedPacket::decode_local(&wire),
            Err(WireError::PayloadType)
        );
    }
}

#[test]
fn exact_mtu_with_core_only_and_worst_video_metadata() {
    let dd = [0x2a; 255];
    let act = [0x44; 16];
    let mut vla = vec![0x3f];
    vla.extend([0xff; 4]);
    for rate in 0..64 {
        let bytes = match rate {
            0..26 => 8,
            26 => 5,
            _ => 1,
        };
        if bytes == 1 {
            vla.push(1);
        } else {
            vla.push(0x81);
            vla.extend(std::iter::repeat_n(0x80, bytes - 2));
            vla.push(0);
        }
    }
    assert_eq!(vla.len(), 255);
    let full_video = [
        (DependencyDescriptor, &dd[..]),
        (AbsoluteCaptureTime, &act),
        (VideoOrientation, &[0xf]),
        (VideoLayersAllocation, &vla),
        (VideoContentType, &[1]),
        (ColorSpace, &[0; 28]),
        (PlayoutDelay, &[0; 3]),
    ]
    .map(|(kind, value)| Extension { kind, value });
    for (extensions, max_payload, expected_form) in [
        (&[][..], 1_128, [0xbe, 0xde]),
        (&full_video[..], 552, [0x10, 0x00]),
    ] {
        let opaque = vec![0xfa; max_payload];
        let header = image!("test", Av1, 55, 99, true, 0).header;
        let packet = FabricPacket::new(header, extensions, &opaque).unwrap();
        let mut wire = vec![0; 1_152];
        assert_eq!(packet.write_local(&mut wire), Ok(1_152));
        assert_eq!(wire[12..14], expected_form);
        let parsed = BorrowedPacket::decode_local(&wire).unwrap();
        assert_eq!(parsed.payload(), opaque);
        assert!(parsed.extensions().eq(extensions.iter().copied()));
        assert_eq!(
            packet.write_local(&mut wire[..1_151]),
            Err(WireError::OutputTooSmall)
        );

        if !extensions.is_empty() {
            // First VLA rate in the maximum co-occurrence image.
            for (offset, bits) in [(314, 0x10), (317, 0x80)] {
                let mut invalid = wire.clone();
                invalid[offset] |= bits;
                assert_eq!(
                    BorrowedPacket::decode_local(&invalid),
                    Err(WireError::ExtensionValue)
                );
            }
        }

        wire.push(0xfa);
        assert_eq!(
            BorrowedPacket::decode_local(&wire),
            Err(WireError::Oversize)
        );
        let logical = FabricPacket::new(header, extensions, vec![0xfa; max_payload + 1])
            .expect("the logical packet is legal on the WAN");
        assert_eq!(logical.legacy_wire_len(), Err(WireError::Oversize));
        assert_eq!(logical.write_local(&mut wire), Err(WireError::Oversize));
        let max_wan_payload = MAX_WAN_RTP_BYTES - logical.wan_plaintext_len() + max_payload + 1;
        let boundary = FabricPacket::new(header, extensions, vec![0xfa; max_wan_payload])
            .expect("exact WAN size");
        assert_eq!(boundary.wan_plaintext_len(), MAX_WAN_RTP_BYTES);
        let above_wan = FabricPacket::new(header, extensions, vec![0xfa; max_wan_payload + 1])
            .expect("logical packet does not inherit the WAN codec limit");
        assert_eq!(above_wan.wan_plaintext_len(), MAX_WAN_RTP_BYTES + 1);
        assert_eq!(above_wan.write_local(&mut wire), Err(WireError::Oversize));
    }
}

#[test]
fn source_content_ignores_local_stamp_and_hop_budget() {
    let header = image!("test", H264, 55, 99, true, 0).header;
    let first = FabricPacket::new(header, &[], [42]).unwrap();
    let copy = FabricPacket::new(
        Header {
            ingress_ntp32: 13,
            ..header
        },
        &[],
        [42],
    )
    .unwrap();
    assert!(first.same_content(&copy));
    assert!(
        first
            .arrival_view(0)
            .unwrap()
            .same_content(&copy.arrival_view(6).unwrap())
    );
    let conflict = FabricPacket::new(header, &[], [43]).unwrap();
    assert!(!first.same_content(&conflict));
}

#[test]
fn legacy_data_is_one_opaque_record_and_has_no_dd() {
    let header = image!("test", Data, 0, 5, true, 0).header;
    let dd = Extension {
        kind: DependencyDescriptor,
        value: &[0; 3],
    };
    let act = Extension {
        kind: AbsoluteCaptureTime,
        value: &[0; 16],
    };
    let record = [0xfa; 1_058];
    assert_eq!(
        FabricPacket::new(header, &[dd], record)
            .unwrap()
            .legacy_wire_len(),
        Err(WireError::ExtensionProfile)
    );
    for size in [0, 21, 1_059] {
        assert_eq!(
            FabricPacket::new(header, &[act], vec![0; size]),
            Err(WireError::PayloadLength)
        );
    }
    let packet = FabricPacket::new(header, &[act], record).unwrap();
    let mut wire = [0; 1_102];
    assert_eq!(packet.write_local(&mut wire), Ok(1_102));
    assert_eq!(
        BorrowedPacket::decode_local(&wire).unwrap().payload(),
        record
    );
    let mut dd_wire = golden("A1");
    dd_wire[1] = 0xe3;
    assert_eq!(
        BorrowedPacket::decode_local(&dd_wire),
        Err(WireError::ExtensionProfile)
    );
}

#[test]
fn rejects_invalid_source_extension_values_and_duplicate_kinds() {
    let header = image!("test", H264, 0, 0, false, 0).header;
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
