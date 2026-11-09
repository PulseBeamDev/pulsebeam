use super::*;
use crate::entity::{ConnectionId, TrackKind};
use crate::id::ShardId;
use crate::participant::{RoutedTrackPacket, TrackPacket};
use crate::route::{MediaDelivery, NodeRouteAddress, RouteId};
use crate::rtp::RtpPacket;
use crate::shard::worker::ShardFrame;
use crate::track::DataLane;
use pulsebeam_link::WireCodec;
use pulsebeam_runtime::time::Instant;

fn media(packet: TrackPacket, kind: TrackKind) -> ShardFrame {
    ShardFrame::Media {
        delivery: MediaDelivery {
            address: NodeRouteAddress::new(RouteId::new(ShardId::from(1), 7), 19),
            playout_ntp32: 0x12345678,
        },
        payload: RoutedTrackPacket {
            track_id: ConnectionId::from_bytes([3; 16]).derive_track_id(kind, "source"),
            packet,
        },
    }
}

fn encoded(codec: &ShardCodec, frame: &ShardFrame) -> Vec<u8> {
    let mut wire = vec![0; codec.encoded_len(frame).unwrap()];
    assert_eq!(codec.encode(frame, &mut wire).unwrap(), wire.len());
    wire
}

#[test]
fn delivery_tags_preserve_destination_and_generation() {
    for epoch in [0, 1, u16::MAX] {
        let address = NodeRouteAddress::new(RouteId::new(ShardId::from(2), 71), epoch);
        assert_eq!(delivery_address(delivery_tag(address)), Some(address));
    }
    assert_eq!(delivery_address(DeliveryTag::from_raw(1 << 48)), None);
}

#[test]
fn data_lanes_round_trip_without_trusting_application_destination() {
    let codec = ShardCodec {
        origin: Instant::now(),
    };
    for lane in [DataLane::Latest, DataLane::Lossy, DataLane::Ordered] {
        let frame = media(
            TrackPacket::Data {
                lane,
                bytes: bytes::Bytes::from_static(b"value"),
            },
            TrackKind::Data,
        );
        let wire = encoded(&codec, &frame);
        let ShardFrame::Media { delivery, payload } = codec.decode(&wire).unwrap() else {
            panic!("media expected");
        };
        assert_eq!(
            delivery.address,
            NodeRouteAddress::new(RouteId::from_raw(0), 0)
        );
        assert_eq!(delivery.playout_ntp32, 0x12345678);
        let TrackPacket::Data {
            lane: decoded_lane,
            bytes,
        } = payload.packet
        else {
            panic!("data expected");
        };
        assert_eq!(decoded_lane, lane);
        assert_eq!(bytes.as_ref(), b"value");
        for length in 0..wire.len() {
            assert!(codec.decode(&wire[..length]).is_err());
        }
        let mut trailing = wire;
        trailing.push(0);
        assert!(codec.decode(&trailing).is_err());
    }
}

#[test]
fn normalized_rtp_snapshot_preserves_descriptor_and_extended_counters() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
        RawDependencyDescriptor,
    };
    let codec = ShardCodec {
        origin: Instant::now(),
    };
    let mut packet = RtpPacket {
        payload: Vec::from([1, 2, 3]).into(),
        seq_no: (u64::from(u16::MAX) + 17).into(),
        rtp_ts: crate::rtp::MediaTime::new(u64::from(u32::MAX) + 19, crate::rtp::VIDEO_FREQUENCY),
        nal: crate::rtp::h264::NalFlags::from_components(true, true, true),
        ..Default::default()
    };
    packet
        .ext_vals
        .set_raw_descriptor(RawDependencyDescriptor([0xc0, 0, 1].into_iter().collect()));
    packet.ext_vals.set_descriptor(DependencyDescriptor {
        start_of_frame: true,
        end_of_frame: true,
        frame_number: 1,
        frame_dependencies: FrameDependencyTemplate {
            dtis: [DecodeTargetIndication::Required].into_iter().collect(),
            ..Default::default()
        },
        ..Default::default()
    });
    let expected = packet.clone();
    let wire = encoded(&codec, &media(TrackPacket::Rtp(packet), TrackKind::Video));
    let ShardFrame::Media { payload, .. } = codec.decode(&wire).unwrap() else {
        panic!("media expected");
    };
    let TrackPacket::Rtp(packet) = payload.packet else {
        panic!("RTP expected");
    };
    assert_eq!(packet.payload, expected.payload);
    assert_eq!(packet.seq_no, expected.seq_no);
    assert_eq!(packet.rtp_ts, expected.rtp_ts);
    assert_eq!(packet.ext_vals, expected.ext_vals);
    assert_eq!(packet.nal, expected.nal);
    assert_eq!(
        encoded(&codec, &media(TrackPacket::Rtp(packet), TrackKind::Video)),
        wire
    );
    for length in 0..wire.len() {
        assert!(codec.decode(&wire[..length]).is_err());
    }
}

#[test]
fn descriptor_structure_rejects_invalid_chain_ownership_before_admission_and_on_decode() {
    use pulsebeam_core::dd::{
        DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
        TemplateDependencyStructure,
    };
    let codec = ShardCodec {
        origin: Instant::now(),
    };
    let mut structure = TemplateDependencyStructure {
        decode_target_count: 1,
        chain_count: 1,
        decode_target_protected_by: [0].into_iter().collect(),
        templates: [FrameDependencyTemplate {
            dtis: [DecodeTargetIndication::Required].into_iter().collect(),
            chain_diffs: [0].into_iter().collect(),
            ..Default::default()
        }]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let snapshot = |structure| {
        let mut packet = RtpPacket {
            payload: Vec::from([7]).into(),
            ..Default::default()
        };
        packet.ext_vals.set_descriptor(DependencyDescriptor {
            attached_structure: Some(Box::new(structure)),
            ..Default::default()
        });
        media(TrackPacket::Rtp(packet), TrackKind::Video)
    };
    let wire = encoded(&codec, &snapshot(structure.clone()));
    assert!(codec.decode(&wire).is_ok());
    structure.decode_target_protected_by[0] = 1;
    assert!(matches!(
        codec.encoded_len(&snapshot(structure)),
        Err(pulsebeam_link::CodecError::Unsupported)
    ));
    // Locate the complete bounded structure encoding, then corrupt only the
    // chain owner so the public decoder must reject the hostile snapshot.
    let valid = [0, 1, 1, 1, 0, 1, 0, 0, 1, 3, 0, 1, 0, 0];
    let offset = wire
        .windows(valid.len())
        .position(|bytes| bytes == valid)
        .unwrap();
    let mut invalid = wire;
    invalid[offset + 4] = 1;
    assert!(matches!(
        codec.decode(&invalid),
        Err(pulsebeam_link::CodecError::Malformed)
    ));
}

#[test]
fn oversize_is_rejected_before_encoding_or_admission() {
    let codec = ShardCodec {
        origin: Instant::now(),
    };
    let frame = media(
        TrackPacket::Data {
            lane: DataLane::Lossy,
            bytes: bytes::Bytes::from(vec![0; 1200]),
        },
        TrackKind::Data,
    );
    assert!(matches!(
        codec.encoded_len(&frame),
        Err(pulsebeam_link::CodecError::TooLarge)
    ));
}
