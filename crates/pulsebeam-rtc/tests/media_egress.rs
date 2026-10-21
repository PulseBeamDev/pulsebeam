#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "controlled native fixtures stop at violated wire invariants"
)]

mod support;

use pulsebeam_rtc::{Output, WriteError};
use std::time::Duration;
use support::{PeerFixture, rtp_write};

#[test]
fn caller_numbering_timestamps_markers_and_payload_cross_the_native_boundary() {
    let mut fixture = PeerFixture::connected();
    let source = fixture.send_source(b"direct-rtp");
    let original = source.clone();
    let mut ssrc = None;
    for (index, sequence) in [65_534_u64, 65_535, 65_537, 65_538].into_iter().enumerate() {
        let mut packet = rtp_write(source.clone(), sequence);
        packet.timestamp = (u32::MAX - 959).wrapping_add(u32::try_from(index).unwrap() * 960);
        packet.marker = index == 2;
        let timestamp = packet.timestamp;
        fixture
            .write_rtp(packet)
            .expect("direct write without a demand hint");
        let (header, payload) = fixture.receive_egress();
        assert_eq!(
            header.sequence_number,
            u16::try_from(sequence & 0xffff).unwrap()
        );
        assert_eq!(header.timestamp, timestamp);
        assert_eq!(header.marker, index == 2);
        assert_eq!(
            header.ext_vals.mid.as_deref(),
            Some(fixture.sender_mid.as_str())
        );
        assert!(header.ext_vals.transport_cc.is_some());
        assert_eq!(payload, b"direct-rtp");
        assert_eq!(*ssrc.get_or_insert(header.ssrc), header.ssrc);
        fixture.drive_for(Duration::from_millis(20));
    }
    assert_eq!(source, original, "a write does not mutate received media");
    assert_eq!(fixture.connection.stats().senders[0].transmitted_packets, 4);
    assert_eq!(
        fixture
            .connection
            .stats()
            .connection
            .transmitted_padding_bytes,
        0
    );
}

#[test]
fn authenticated_feedback_keeps_unassigned_primary_media_live() {
    let mut fixture = PeerFixture::connected();
    for sequence in 1..=80 {
        let source = fixture.send_source(b"feedback");
        fixture
            .write_rtp(rtp_write(source, sequence))
            .expect("queue primary");
        assert_eq!(fixture.receive_egress().1, b"feedback");
        fixture.drive_for(Duration::from_millis(20));
    }
    fixture.drive_for(Duration::from_millis(100));
    let stats = fixture.connection.stats().connection;
    assert!(
        fixture.twcc_sent() > 0,
        "real native TWCC crossed SRTP/SRTCP"
    );
    assert!(stats.bandwidth_estimate.as_bps() > 0);
    assert_eq!(stats.transmitted_padding_bytes, 0);
    assert_eq!(stats.queued_media_bytes, 0);
}

#[test]
fn direct_writes_return_owned_work_for_invalid_and_closed_states() {
    let mut fixture = PeerFixture::unconnected();
    let packet = support::sample(fixture.at(), 1, 32, 48_000);
    let mid = fixture.sender_mid.clone();
    let rejected = fixture
        .connection
        .write_rtp(fixture.at(), &mid, packet.clone())
        .expect_err("not connected");
    assert_eq!(rejected.reason, WriteError::InvalidState);
    assert_eq!(rejected.packet, packet);
    fixture.connection.abort();
    let rejected = fixture
        .connection
        .write_rtp(fixture.at(), &mid, packet.clone())
        .expect_err("closed");
    assert_eq!(rejected.reason, WriteError::Closed);
    assert_eq!(rejected.packet, packet);
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Closed(_)
    ));
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Idle { next_wakeup: None }
    ));
}

#[test]
fn invalid_stream_and_index_rejections_do_not_consume_primary_work() {
    let mut fixture = PeerFixture::connected();
    let source = fixture.send_source(b"retry-original");
    let packet = rtp_write(source, 1);
    let rejected = fixture
        .connection
        .write_rtp(fixture.at(), "unknown-mid", packet.clone())
        .expect_err("unknown stream");
    assert_eq!(rejected.reason, WriteError::UnknownStream);
    assert_eq!(rejected.packet, packet);
    let mut exhausted = packet.clone();
    exhausted.sequence = 1 << 48;
    let mid = fixture.sender_mid.clone();
    assert_eq!(
        fixture
            .connection
            .write_rtp(fixture.at(), &mid, exhausted)
            .expect_err("SRTP lifetime")
            .reason,
        WriteError::IndexExhausted
    );
    let mut oversized = packet;
    oversized.payload = vec![0; usize::from(u16::MAX)].into();
    assert_eq!(
        fixture
            .connection
            .write_rtp(fixture.at(), &mid, oversized)
            .expect_err("one RTP datagram must fit its transport envelope")
            .reason,
        WriteError::InvalidRtp
    );
    fixture
        .write_rtp(rejected.packet)
        .expect("retry original index");
    let (header, payload) = fixture.receive_egress();
    assert_eq!(header.sequence_number, 1);
    assert_eq!(payload, b"retry-original");
}
