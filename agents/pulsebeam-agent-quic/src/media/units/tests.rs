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
