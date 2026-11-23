use std::num::NonZeroU16;

use super::*;
use crate::fabric::Extension;

const H264: [u8; 17] = [3, 1, 0, 2, 0, 0, 0, 0, 0, 1, 0x80, 0, 3, 5, 6, 7, 8];
const LIVE: [u8; 14] = [0x80, 0xe6, 0x80, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0x65, 0x88];

#[test]
fn live_packet_matches_literal_layout_and_uses_installed_position() {
    let window = Window::from_tuple(H264, 0).unwrap();
    let header = Header {
        profile: Profile::H264,
        marker: true,
        sequence: 0x18000,
        timestamp: 0x0102_0304,
        ingress_ntp32: 42,
    };
    let packet = FabricPacket::new(header, &[], [0x65, 0x88]).unwrap();
    let mut bytes = [0xff; 14];
    assert_eq!(window.write(0x18000, &packet, &mut bytes), Ok(14));
    assert_eq!(bytes, LIVE);
    let (position, decoded) = window.decode(&LIVE).unwrap();
    assert_eq!(position, 0x18000);
    assert_eq!(decoded.remaining_hops(), 6);
    assert!(decoded.same_content(&packet.arrival_view(7).unwrap()));
    assert_eq!(window.position(0xffff), Ok(0x1ffff));
    assert_eq!(window.position(0), Err(WireError::Window));
    let mut next = H264;
    next[9..12].copy_from_slice(&[2, 0, 0]);
    next[16] = 9;
    let next = Window::from_tuple(next, 0).unwrap();
    assert_eq!(next.position(0), Ok(0x20000));
    assert_eq!(next.decode(&LIVE), Err(WireError::RouteKey));
}

#[test]
fn media_keeps_both_extension_forms_and_minimum_padding() {
    let window = Window::from_tuple(H264, 1 << 2).unwrap();
    for (block, expected_form) in [
        (&[0xbe, 0xde, 0, 1, 0x22, 0xc0, 0, 0][..], Form::One),
        (&[0x10, 0, 0, 2, 2, 3, 0xc0, 0, 0, 0, 0, 0][..], Form::Two),
    ] {
        let mut bytes = LIVE[..12].to_vec();
        bytes[0] = 0x90;
        bytes.extend_from_slice(block);
        bytes.extend_from_slice(&LIVE[12..]);
        let (position, decoded) = window.decode(&bytes).unwrap();
        assert_eq!(decoded.form, expected_form);
        let fresh = FabricPacket::pack(
            decoded.header(),
            &[Extension {
                kind: ExtensionKind::DependencyDescriptor,
                value: &[0xc0, 0, 0],
            }],
            decoded.payload(),
            Some(expected_form),
        )
        .unwrap();
        assert!(decoded.same_content(&fresh.arrival_view(6).unwrap()));
        let mut legacy = [0; 32];
        let legacy_len = fresh.write_local(&mut legacy).unwrap();
        assert_eq!(legacy[12..14], [0xbe, 0xde]);
        assert!(
            BorrowedPacket::decode_local(&legacy[..legacy_len])
                .unwrap()
                .same_content(&decoded)
        );
        assert!(decoded.extensions().eq([Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value: &[0xc0, 0, 0],
        }]));
        let mut next = [0; 26];
        let len = window.write(position, &decoded, &mut next).unwrap();
        bytes[1] = 0xe5;
        assert_eq!(next[..len], bytes);
        bytes[11] = 9;
        assert_eq!(window.decode(&bytes), Err(WireError::RouteKey));
    }
}

#[test]
fn protected_wan_primary_does_not_grow_beyond_publisher_datagram() {
    let window = Window::new(Profile::Opus, 0, 0, NonZeroU32::new(11).unwrap(), 0).unwrap();
    let packet = FabricPacket::new(
        Header {
            profile: Profile::Opus,
            marker: false,
            sequence: 1,
            timestamp: 960,
            ingress_ntp32: 0,
        },
        &[],
        [0xf8, 0xff, 0xfe],
    )
    .unwrap();
    let mut out = [0; 64];
    let too_small = packet
        .clone()
        .with_source_protected_bytes(NonZeroU16::new(30).unwrap());
    assert_eq!(
        window.write(1, &too_small, &mut out),
        Err(WireError::Oversize)
    );
    let exact = packet.with_source_protected_bytes(NonZeroU16::new(31).unwrap());
    assert_eq!(window.write(1, &exact, &mut out), Ok(15));
}

#[test]
fn opus_window_preserves_publisher_budget_in_existing_extension_word() {
    let legacy = Window::new(Profile::Opus, 0, 3, NonZeroU32::new(11).unwrap(), 1 << 4).unwrap();
    let window = legacy.with_opus_source_budget().unwrap();
    assert_eq!(
        Window::from_tuple(window.to_tuple(), window.emission()),
        Ok(window)
    );
    assert_ne!(window.to_tuple(), legacy.to_tuple());
    let packet = FabricPacket::new(
        Header {
            profile: Profile::Opus,
            marker: true,
            sequence: 1,
            timestamp: 960,
            ingress_ntp32: 0,
        },
        &[Extension {
            kind: ExtensionKind::AudioLevel,
            value: &[0x81],
        }],
        vec![0xf8; 1_200].into_boxed_slice(),
    )
    .unwrap()
    .with_source_protected_bytes(NonZeroU16::new(1_240).unwrap());
    let mut out = [0; MAX_WAN_RTP_BYTES];
    assert_eq!(window.write(1, &packet, &mut out), Ok(MAX_WAN_RTP_BYTES));
    assert_eq!(&out[12..20], &[0xbe, 0xde, 0, 1, 0x40, 0x81, 0xa0, 4]);
    let (_, received) = window.decode(&out).unwrap();
    assert!(received.into_owned().same_content(&packet));
    assert_eq!(
        window.decode(&out).unwrap().1.source_protected_bytes(),
        packet.source_protected_bytes()
    );
    assert!(legacy.decode(&out).is_err());
    out[19] = 0;
    assert_eq!(window.decode(&out), Err(WireError::ExtensionValue));
    assert_eq!(legacy.write(1, &packet, &mut out), Ok(MAX_WAN_RTP_BYTES));
    assert_eq!(
        window.decode(&out).unwrap().1.source_protected_bytes(),
        None
    );
    let repair = window
        .repair_window(1, NonZeroU32::new(12).unwrap())
        .unwrap();
    assert_eq!(repair.write(1, &packet, &mut out), Ok(MAX_WAN_RTP_BYTES));
    assert_eq!(
        repair.decode(&out).unwrap().1.source_protected_bytes(),
        packet.source_protected_bytes()
    );
}

#[test]
fn accepts_through_envelope_and_rejects_next_byte() {
    let mut tuple = H264;
    tuple[16] = 0x0b;
    let window = Window::from_tuple(tuple, 0).unwrap();
    const BASE: [u8; 12] = [0x80, 0x66, 0x80, 0, 1, 2, 3, 4, 5, 6, 7, 0x0b];
    let cap = MAX_WAN_RTP_BYTES;
    for (len, seq_low) in [(cap - 1, 0), (cap, 1), (cap + 1, 2)] {
        let mut image = BASE.to_vec();
        image[3] = seq_low;
        image.resize(len, 0x61);
        if len > cap {
            assert_eq!(window.decode(&image), Err(WireError::Oversize));
            let header = Header {
                sequence: 0x18002,
                profile: Profile::H264,
                marker: false,
                timestamp: 0,
                ingress_ntp32: 0,
            };
            let packet = FabricPacket::new(header, &[], &image[12..]).unwrap();
            let mut out = [0; MAX_WAN_RTP_BYTES + 1];
            assert_eq!(
                window.write(0x18002, &packet, &mut out),
                Err(WireError::Oversize)
            );
        } else {
            let (position, packet) = window.decode(&image).unwrap();
            assert_eq!(packet.wan_plaintext_len(), len);
            assert_eq!(packet.legacy_wire_len(), Err(WireError::Oversize));
            let mut out = [0; MAX_WAN_RTP_BYTES];
            assert_eq!(window.write(position, &packet, &mut out), Ok(len));
            let repair_label = NonZeroU32::new(12).unwrap();
            let repair = window.repair_window(position, repair_label).unwrap();
            let mut repaired = [0; MAX_WAN_RTP_BYTES];
            assert_eq!(repair.write(position, &packet, &mut repaired), Ok(len));

            image[1] = 0x65;
            assert_eq!(out[..len], image);
            image[8..12].copy_from_slice(&repair_label.get().to_be_bytes());
            assert_eq!(repaired[..len], image);
            assert_eq!(repair.decode(&repaired[..len]), window.decode(&out[..len]));
        }
    }
}

#[test]
fn hop_payload_types_are_mux_safe_and_stop_at_local_delivery() {
    let window = Window::from_tuple(H264, 0).unwrap();
    for (hops, pt, forwarded_pt) in [
        (0, 0x60, None),
        (1, 0x61, Some(0x60)),
        (2, 0x62, Some(0x61)),
        (3, 0x63, Some(0x62)),
        (4, 0x64, Some(0x63)),
        (5, 0x65, Some(0x64)),
        (6, 0x66, Some(0x65)),
    ] {
        for marker in [0, 0x80] {
            let mut wire = LIVE;
            wire[1] = pt | marker;
            let (position, packet) = window.decode(&wire).unwrap();
            assert_eq!(packet.remaining_hops(), hops);
            assert_eq!(packet.header().marker, marker != 0);
            let mut out = [0xff; LIVE.len()];
            if let Some(expected) = forwarded_pt {
                assert_eq!(window.write(position, &packet, &mut out), Ok(LIVE.len()));
                wire[1] = expected | marker;
                assert_eq!(out, wire);
            } else {
                assert_eq!(
                    window.write(position, &packet, &mut out),
                    Err(WireError::HopExhausted)
                );
                assert_eq!(out, [0xff; LIVE.len()]);
            }
        }
    }
}

#[test]
fn window_and_packet_reject_unselected_or_malformed_wire() {
    for (at, value) in [(0, 2), (2, 1), (11, 1), (12, 4)] {
        let mut tuple = H264;
        tuple[at] = value;
        assert_eq!(Window::from_tuple(tuple, 0), Err(WireError::Window));
    }
    let window = Window::from_tuple(H264, 1 << 2).unwrap();
    for (at, value, error) in [
        (0, 0xa0, WireError::Header),
        (0, 0x81, WireError::Header),
        (1, 95, WireError::PayloadType),
        (1, 104, WireError::PayloadType),
        (1, 103, WireError::NetworkHop),
        (2, 0, WireError::Window),
    ] {
        let mut packet = LIVE;
        packet[at] = value;
        assert_eq!(window.decode(&packet), Err(error));
    }
    for block in [
        &[0xbe, 0xde, 0, 1, 0x12, 0xc0, 0, 0][..],
        &[0x10, 1, 0, 1, 2, 2, 0xc0, 0][..],
        &[0xbe, 0xde, 0, 2, 0x22, 0xc0, 0, 0][..],
        &[0xbe, 0xde, 0, 1, 0, 0, 0, 0][..],
        &[0xbe, 0xde, 0, 2, 0x22, 0xc0, 0, 0, 0x22, 0xc0, 0, 0][..],
        &[0xbe, 0xde, 0, 2, 0x22, 0xc0, 0, 0, 0, 0, 0, 0][..],
    ] {
        let mut bytes = LIVE[..12].to_vec();
        bytes[0] = 0x90;
        bytes.extend_from_slice(block);
        assert_eq!(window.decode(&bytes), Err(WireError::ExtensionLayout));
    }
}
