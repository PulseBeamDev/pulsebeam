#![allow(
    clippy::disallowed_types,
    reason = "NetworkInput's public contract is Bytes-backed"
)]

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    Connection, ConnectionConfig, ConnectionEntropy, LocalCandidate, NetworkInput, Output, SdpOffer,
};

mod support;

#[test]
fn mixed_direction_native_publisher_recovers_loss_as_primary_media() {
    let mut fixture = support::PeerFixture::connected_video_publisher_audio_subscriber();
    fixture.drop_source_packet_after(1);
    let packets = fixture.send_fragmented_video_source(None);
    let (ssrc, sequence) = fixture.lost_source_packet();
    let recovered = packets
        .iter()
        .find(|packet| packet.header.sequence == sequence)
        .cloned()
        .unwrap_or_else(|| fixture.receive_source_sequence(sequence));
    let view = recovered.header;
    assert_eq!(view.ssrc, ssrc);
    assert_eq!(view.sequence, sequence);
    assert!(!recovered.payload.is_empty());
    assert_eq!(recovered.header.timestamp, packets[0].header.timestamp);
    assert!(
        fixture.source_repair_packets > 0,
        "native publisher must send real RTX"
    );
    assert!(
        fixture.publisher_nacks > 0,
        "NACK must cross the authenticated protocol boundary"
    );
    fixture.drive_for(Duration::from_millis(1_100));
    assert!(
        fixture.publisher_reports > 0,
        "publisher must receive an RTCP receiver report"
    );
    assert!(
        fixture.publisher_feedback > 0,
        "publisher must receive packet-level congestion feedback"
    );
    let stats = fixture.connection.stats();
    assert_eq!(stats.senders.len(), 1);
    assert_eq!(stats.encodings.len(), 1);
    assert_eq!(stats.encodings[0].kind, pulsebeam_rtc::MediaKind::Video);
    assert!(stats.encodings[0].received_packets > 0);
}

#[test]
fn unauthenticated_rtp_never_creates_a_semantic_media_event() {
    let start = Instant::now();
    let mut connection = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(include_str!("fixtures/chrome-representative.sdp")),
        start,
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([9; 32]),
    )
    .expect("connection")
    .connection;

    assert_eq!(
        connection.receive(
            start,
            start + Duration::from_millis(1),
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::new(),
            },
        ),
        Err(pulsebeam_rtc::ReceiveError::InvalidObservation)
    );

    connection
        .receive(
            start,
            start,
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::from_static(&[0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7]),
            },
        )
        .expect("network input is dropped, not an API error");

    for _ in 0..16 {
        match connection.poll(start) {
            Output::Idle { .. } => return,
            Output::Event(pulsebeam_rtc::Event::BandwidthEstimate(_)) => {}
            output => panic!("unauthenticated input produced output: {output:?}"),
        }
    }
    panic!("bounded unauthenticated input did not become idle");
}
