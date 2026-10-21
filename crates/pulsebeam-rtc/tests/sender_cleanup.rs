#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "bounded native repair fixture stops at violated wire invariants"
)]

mod support;

use pulsebeam_rtc::{MediaPayloadBitrate, TransmitKind};
use std::time::Duration;
use support::{NetworkPolicy, PeerFixture, RtpImpairment, rtp_write};

fn write_frame(fixture: &mut PeerFixture, packets: &[pulsebeam_rtc::RtpPacket], first: u64) {
    for (index, packet) in packets.iter().enumerate() {
        fixture
            .write_rtp(rtp_write(
                packet.clone(),
                first + u64::try_from(index).expect("test packet index fits u64"),
            ))
            .expect("write original fragment");
    }
}

#[test]
fn discarding_uncommitted_work_preserves_native_repairs_and_rtx_numbering() {
    let mut fixture = PeerFixture::connected_video();
    fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(1_000_000)));
    fixture.drive_for(Duration::from_millis(200));
    let padding = fixture
        .take_video_wire_packets()
        .into_iter()
        .filter(|packet| packet.padding)
        .collect::<Vec<_>>();
    assert!(!padding.is_empty(), "source-free RTX bootstrap");
    fixture.set_desired_bitrate(None);
    let warmup = fixture.send_fragmented_video_source(None);
    write_frame(&mut fixture, &warmup, 1_000);
    fixture.receive_video_frame();
    fixture.drive_for(Duration::from_millis(200));
    let committed = fixture.connection.stats().senders[0].transmitted_packets;
    let sources = fixture.send_fragmented_video_source(None);
    let first = 1_000 + u64::try_from(warmup.len()).unwrap();
    fixture.drive_for(Duration::from_millis(1));
    fixture.take_video_wire_packets();
    fixture.configure_network(
        7,
        NetworkPolicy {
            delay: Duration::from_millis(100),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_impairment(RtpImpairment::Burst {
        every: 3,
        length: 1,
    });
    fixture.record_transmits();
    write_frame(&mut fixture, &sources, first);
    let deadline = fixture.at() + Duration::from_millis(150);
    while fixture.connection.stats().senders[0].queued_packets > 0 {
        assert!(fixture.at() < deadline, "bounded frame commit");
        fixture.drive_for(Duration::from_millis(1));
    }
    assert_eq!(
        fixture.connection.stats().senders[0].transmitted_packets,
        committed + u64::try_from(sources.len()).unwrap()
    );
    let primary_ssrc = fixture.receive_video_wire_header().ssrc;
    assert!(fixture.impairment_counts().1 > 0);
    let discarded_index = first + u64::try_from(sources.len()).unwrap();
    let mid = fixture.sender_mid.clone();
    fixture
        .connection
        .write_rtp(
            fixture.at(),
            &mid,
            rtp_write(sources[0].clone(), discarded_index),
        )
        .expect("queue uncommitted work");
    fixture
        .connection
        .discard_pending_rtp(&mid)
        .expect("discard only pending originals");
    fixture.configure_impairment(RtpImpairment::Random { per_mille: 0 });
    fixture.drive_for(Duration::from_millis(700));
    let repairs = fixture
        .take_video_wire_packets()
        .into_iter()
        .filter(|header| !header.padding && header.ssrc != primary_ssrc)
        .collect::<Vec<_>>();
    assert!(
        !repairs.is_empty(),
        "authenticated native NACK produces RTX after queue clearing"
    );
    let mut previous = padding.last().unwrap();
    for packet in &repairs {
        assert_eq!(packet.ssrc, previous.ssrc);
        assert!(
            (1..32_768).contains(
                &packet
                    .sequence_number
                    .wrapping_sub(previous.sequence_number)
            ),
            "padding and repairs share continuous RTX numbering"
        );
        previous = packet;
    }
    assert!(
        fixture
            .take_transmit_observations()
            .iter()
            .any(|(_, kind)| matches!(kind, TransmitKind::Repair { .. }))
    );
    fixture.receive_video_frame();
    let next = fixture.send_fragmented_video_source(None);
    write_frame(&mut fixture, &next, discarded_index + 1);
    let header = fixture.receive_video_wire_header();
    assert_eq!(header.ssrc, primary_ssrc);
    assert_eq!(
        u64::from(header.sequence_number),
        (discarded_index + 1) & 0xffff
    );
    fixture.receive_video_frame();
}
