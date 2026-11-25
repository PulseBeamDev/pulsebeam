use std::{num::NonZeroUsize, time::Duration};

use super::*;
use crate::media::{H264, PacketBudget, jitter};

#[allow(
    clippy::disallowed_methods,
    reason = "all elapsed time is virtual from a single test origin"
)]
fn origin() -> Instant {
    Instant::now()
}

fn ordered<'a>(
    at: Instant,
    sequence: u64,
    timestamp: u64,
    marker: bool,
    payload: &'a [u8],
) -> OrderedPacket<'a> {
    let [.., a, b, c, d] = timestamp.to_be_bytes();
    OrderedPacket {
        sequence,
        timestamp: u32::from_be_bytes([a, b, c, d]),
        rtp_time: Some(timestamp),
        arrived_at: at,
        marker,
        payload,
        descriptor: None,
    }
}

#[test]
fn vp8_assembles_partitions_and_rejects_gap_and_dd_conflicts() {
    let at = origin();
    let mut decoder =
        Depacketizer::new(1, Codec::Vp8).with_video_limit(NonZeroUsize::new(5).unwrap());
    let first = [0x10, 0x11, 0x00, 0x00];
    let second = [0x11, 0xab, 0xcd];
    let mut packet = ordered(at, 65_535, u64::from(u32::MAX), false, &first);
    packet.descriptor = Some(&[0x80, 0, 7]);
    assert!(decoder.push(1, packet).unwrap().is_none());
    let mut packet = ordered(at, 65_536, u64::from(u32::MAX), true, &second);
    packet.descriptor = Some(&[0x40, 0, 7]);
    let Some(Event::Unit(unit)) = decoder.push(1, packet).unwrap() else {
        panic!("complete partitioned VP8 frame");
    };
    assert_eq!(unit.data, [0x11, 0, 0, 0xab, 0xcd]);
    assert_eq!(unit.access, Access::DependentVideo);
    assert!(matches!(unit.framing, Framing::Vp8 { partitions, .. } if partitions == [0..3, 3..5]));

    let timestamp = u64::from(u32::MAX) + 1;
    let mut packet = ordered(at, 65_537, timestamp, true, &first);
    packet.descriptor = Some(&[0x80, 0, 8]);
    assert!(
        matches!(decoder.push(1, packet), Ok(Some(Event::Discontinuity(gap))) if gap.cause == Cause::MissingBoundary)
    );
    assert!(
        decoder
            .push(1, ordered(at, 65_538, timestamp, false, &first))
            .unwrap()
            .is_none()
    );
    decoder
        .gap(1, 65_539..65_540, GapReason::ReorderLimit)
        .unwrap();
    assert!(
        matches!(decoder.push(1, ordered(at, 65_540, timestamp, true, &second)), Ok(Some(Event::Discontinuity(gap))) if gap.cause == Cause::MissingBoundary)
    );
    assert!(
        decoder
            .push(1, ordered(at, 65_541, timestamp, false, &first))
            .unwrap()
            .is_none()
    );
    let overflow = [0, 1, 2, 3];
    assert!(
        matches!(decoder.push(1, ordered(at, 65_542, timestamp, true, &overflow)), Ok(Some(Event::Discontinuity(gap))) if gap.cause == Cause::UnitLimit)
    );
}

#[test]
fn vp9_delivers_distinct_spatial_frames_at_one_timestamp() {
    let at = origin();
    let mut decoder = Depacketizer::new(1, Codec::Vp9);
    let mut push = |seq, marker, payload, dd| {
        let mut packet = ordered(at, seq, 9000, marker, payload);
        packet.descriptor = Some(dd);
        decoder.push(1, packet).unwrap()
    };
    assert!(push(30, false, &[0xa8, 7, 0x10, 4, 0x82], &[0x80, 0, 9]).is_none());
    let Some(Event::Unit(base)) = push(31, false, &[0xa4, 7, 0x10, 4, 0x49], &[0x40, 0, 9]) else {
        panic!("spatial frame must finish before picture marker");
    };
    let Some(Event::Unit(upper)) = push(32, true, &[0xac, 7, 0x13, 4, 0x83], &[0xc0, 0, 10]) else {
        panic!("upper frame shares the base RTP timestamp");
    };
    assert_eq!(base.rtp_time, upper.rtp_time);
    assert_eq!(base.data, [0x82, 0x49]);
    assert_eq!(upper.data, [0x83]);
    assert_eq!(base.access, Access::IndependentVideo);
    assert_eq!(upper.access, Access::DependentVideo);
    assert!(
        matches!(base.framing, Framing::Vp9 { descriptor, end_of_picture: false }
        if descriptor.layer.unwrap().spatial_id == 0)
    );
    assert!(
        matches!(upper.framing, Framing::Vp9 { descriptor, end_of_picture: true }
        if descriptor.layer.unwrap().spatial_id == 1)
    );
    assert!(push(33, false, &[0xa8, 8, 0x10, 5, 0x82], &[0x80, 0, 11]).is_none());
    assert!(
        matches!(push(34, true, &[0xa4, 8, 0x13, 5, 0x49], &[0x40, 0, 11]),
        Some(Event::Discontinuity(gap)) if gap.cause == Cause::MissingBoundary && gap.sequence == (33..35))
    );
}

#[test]
fn av1_uses_dd_for_frame_boundaries_and_requires_explicit_whole_source_mode() {
    let at = origin();
    let timestamp = 9000;
    let base = [0x28, 2, 0x08, 0xab, 0x30, 0xcd];
    let higher = [0x10, 0x34, 0x08, 0xef];
    let mut decoder = Depacketizer::new(1, Codec::Av1);
    assert!(
        matches!(decoder.push(1, ordered(at, 10, timestamp, true, &base)),
        Ok(Some(Event::Discontinuity(gap))) if gap.cause == Cause::MissingBoundary)
    );
    let mut first = ordered(at, 11, timestamp, false, &base);
    first.descriptor = Some(&[0xc0, 0, 1]);
    let Some(Event::Unit(first)) = decoder.push(1, first).unwrap() else {
        panic!("DD end without a picture marker");
    };
    let mut second = ordered(at, 12, timestamp, true, &higher);
    second.descriptor = Some(&[0xc0, 0, 2]);
    let Some(Event::Unit(second)) = decoder.push(1, second).unwrap() else {
        panic!("spatial frame at the same RTP time");
    };
    assert_eq!(first.data, [0x0a, 1, 0xab, 0x32, 1, 0xcd]);
    assert_eq!(first.access, Access::IndependentVideo);
    assert!(
        matches!(first.framing, Framing::Av1 { sequence_header: Some(range), end_of_picture: false, .. } if range == (0..3))
    );
    assert_eq!(second.rtp_time, first.rtp_time);
    assert!(matches!(
        second.framing,
        Framing::Av1 {
            layer: av1::Layer { spatial_id: 1, .. },
            end_of_picture: true,
            ..
        }
    ));

    let mut whole = Depacketizer::new(1, Codec::Av1).with_av1_whole_source();
    assert!(matches!(
        whole.push(1, ordered(at, 20, timestamp, true, &base)),
        Ok(Some(Event::Unit(_)))
    ));
    let continued = [0x50, 0x30, 0xcd];
    assert!(
        whole
            .push(1, ordered(at, 21, timestamp + 1, false, &continued))
            .unwrap()
            .is_none()
    );
    whole.gap(1, 22..23, GapReason::ReorderLimit).unwrap();
    assert!(
        matches!(whole.push(1, ordered(at, 23, timestamp + 1, true, &[0x90, 0xef])),
        Ok(Some(Event::Discontinuity(gap))) if gap.cause == Cause::MissingBoundary)
    );
}

#[test]
fn opus_keeps_rtp_time_and_gaps_separate_from_sender_report_time() {
    let at = origin();
    let mut opus = Depacketizer::new(1, Codec::Opus);
    let before = u64::from(u32::MAX).saturating_sub(480);
    let next = u64::from(u32::MAX).saturating_add(480);
    let encoded = [0xf8, 0xff];
    let Some(Event::Unit(unit)) = opus
        .push(1, ordered(at, 10, before, true, &encoded))
        .unwrap()
    else {
        panic!("Opus unit")
    };
    assert_eq!(unit.rtp_time, Some(before));
    assert_eq!(unit.sender_time, SenderTime::Unknown);
    assert_eq!(unit.codec.clock_rate(), 48_000);
    let gap = opus.gap(1, 11..12, GapReason::Deadline).unwrap();
    assert_eq!(
        (gap.sequence, gap.cause),
        (11..12, Cause::Gap(GapReason::Deadline))
    );
    let report = SenderReport {
        rtp_timestamp: u32::MAX,
        ntp: 0x1234_5678_9000_0000,
    };
    assert!(opus.observe_report(1, report));
    assert!(!opus.observe_report(1, report));
    assert!(!opus.observe_report(1, SenderReport { ntp: 1, ..report }));
    let Some(Event::Unit(unit)) = opus
        .push(1, ordered(at, 12, next, false, &encoded))
        .unwrap()
    else {
        panic!("Opus unit after a gap")
    };
    assert_eq!(unit.rtp_time, Some(next));
    assert_eq!(
        unit.sender_time,
        SenderTime::Report {
            rtp_anchor: u64::from(u32::MAX),
            ntp: report.ntp
        }
    );
    assert_eq!(unit.data, encoded);
    let mut successor = Depacketizer::new(2, Codec::Opus);
    assert!(!successor.observe_report(1, report));
    assert!(matches!(
        successor.push(1, ordered(at, 13, next, false, &encoded)),
        Err(Rejected::Retired)
    ));
    let Some(Event::Unit(unit)) = successor
        .push(2, ordered(at, 0, 12, false, &encoded))
        .unwrap()
    else {
        panic!("new Opus epoch")
    };
    assert_eq!(unit.sender_time, SenderTime::Unknown);
}

#[test]
fn fragmented_video_reorders_then_lost_keyframe_waits_for_clean_random_access() {
    let at = origin();
    let idr = vec![
        0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3, 4, 5, 6, 7,
    ];
    let stage = H264::new(PacketBudget::new(16, 0).unwrap()).unwrap();
    let mut ordered_packets: Vec<_> = stage.packetize(0, 1500, idr.clone()).unwrap().collect();
    ordered_packets.swap(1, 2);
    let mut jitter = jitter::Buffer::new(1, jitter::Limits::default());
    let mut depacket = Depacketizer::new(1, Codec::H264);
    let mut recovery = VideoRecovery::new(1);
    let mut result = None;
    for packet in ordered_packets {
        let input = jitter::Input {
            source: 1,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            arrived_at: at,
            retained_bytes: NonZeroUsize::new(packet.payload.len()).unwrap(),
            packet,
        };
        assert!(matches!(jitter.push(at, input), jitter::Push::Queued));
        while let Some(output) = jitter.poll(at).unwrap() {
            let jitter::Output::Packet {
                sequence,
                timestamp,
                input,
            } = output
            else {
                panic!("no gap before deadline")
            };
            let mut packet = ordered(
                at,
                sequence,
                1500,
                input.packet.marker,
                &input.packet.payload,
            );
            packet.rtp_time = timestamp;
            if let Some(event) = depacket.push(input.source, packet).unwrap() {
                assert!(result.is_none());
                result = Some(recovery.apply(event));
            }
        }
    }
    let Some(Recovery::Deliver(Event::Unit(unit))) = result else {
        panic!("complete first IDR")
    };
    assert_eq!(unit.access, Access::IndependentVideo);
    assert_eq!(unit.codec.clock_rate(), 90_000);
    assert_eq!(unit.first_sequence, 0);
    assert!(recovery.take_refresh().is_none());
    let seq = unit.last_sequence.saturating_add(1);
    let mut seq = seq;
    for packet in stage.packetize(0, 3000, idr.clone()).unwrap().take(3) {
        assert!(
            depacket
                .push(1, ordered(at, seq, 3000, packet.marker, &packet.payload))
                .unwrap()
                .is_none()
        );
        seq = seq.saturating_add(1);
    }
    let now = at.checked_add(Duration::from_millis(60)).unwrap();
    let next = seq.saturating_add(1);
    let gap = depacket.gap(1, seq..next, GapReason::Deadline).unwrap();
    assert!(matches!(
        recovery.apply(Event::Discontinuity(gap)),
        Recovery::Deliver(Event::Discontinuity(_))
    ));
    assert_eq!(recovery.take_refresh(), Some(1));
    assert!(recovery.take_refresh().is_none());
    // The first post-loss packet is a FU continuation, not a usable start.
    let lost = depacket
        .push(1, ordered(now, next, 3000, true, &[0x7c, 0x45, 9]))
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovery.apply(lost),
        Recovery::Deliver(Event::Discontinuity(_))
    ));
    let delta = depacket
        .push(
            1,
            ordered(now, next.saturating_add(1), 4500, true, &[0x41, 1]),
        )
        .unwrap()
        .unwrap();
    assert!(
        matches!(recovery.apply(delta), Recovery::Withheld(unit) if unit.access == Access::DependentVideo)
    );
    assert!(recovery.take_refresh().is_none());
    let mut sequence = next.saturating_add(2);
    let mut access = None;
    for packet in stage.packetize(0, 6000, idr).unwrap() {
        if let Some(event) = depacket
            .push(
                1,
                ordered(now, sequence, 6000, packet.marker, &packet.payload),
            )
            .unwrap()
        {
            access = Some(recovery.apply(event));
        }
        sequence = sequence.saturating_add(1);
    }
    assert!(
        matches!(access, Some(Recovery::Deliver(Event::Unit(unit))) if unit.access == Access::IndependentVideo)
    );
    assert!(recovery.take_refresh().is_none());
}

#[test]
fn descriptor_without_template_is_explicit_and_does_not_invent_dependencies() {
    let at = origin();
    let mut decoder = Depacketizer::new(1, Codec::H264);
    let mut packet = ordered(at, 0, 500, true, &[0x41, 1]);
    packet.descriptor = Some(&[0xc0, 0, 10]);
    let Some(Event::Unit(unit)) = decoder.push(1, packet).unwrap() else {
        panic!("DD supplies a packet boundary without a template")
    };
    assert!(
        matches!(unit.dependency, Dependency::MissingContext(fields) if fields.frame_number == 10)
    );
    assert_eq!(unit.access, Access::DependentVideo);
}
