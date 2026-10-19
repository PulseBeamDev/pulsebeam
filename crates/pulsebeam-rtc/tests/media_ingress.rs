#![allow(
    clippy::disallowed_types,
    reason = "NetworkInput's public contract is Bytes-backed"
)]

use std::{net::SocketAddr, time::Instant};

use bytes::Bytes;
use pulsebeam_rtc::{
    Connection, ConnectionConfig, ConnectionEntropy, GlobalMediaTime, LocalCandidate, NetworkInput,
    Output, SdpOffer, TimePoint,
};

mod support;

#[test]
fn mixed_direction_native_peer_publishes_video_while_subscribing_to_audio() {
    let mut fixture = support::PeerFixture::connected_video_publisher_audio_subscriber();
    let packet = fixture.send_source(&[0x65, 0x88, 0x44]);
    assert!(
        !packet
            .view()
            .expect("normalized source RTP")
            .payload
            .is_empty()
    );
    let stats = fixture.connection.stats();
    assert_eq!(stats.senders.len(), 1);
    assert_eq!(stats.encodings.len(), 1);
    assert_eq!(stats.encodings[0].kind, pulsebeam_rtc::MediaKind::Video);
    assert!(stats.encodings[0].received_packets > 0);
}

fn at(monotonic: Instant, global: u64) -> TimePoint {
    TimePoint {
        monotonic,
        global: GlobalMediaTime::from_micros(global),
    }
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
        at(start, 1_000),
        ConnectionEntropy::new([9; 32]),
    )
    .expect("connection")
    .connection;

    connection
        .receive(
            at(start, 1_000),
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::from_static(&[0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7]),
            },
        )
        .expect("network input is dropped, not an API error");

    assert!(!matches!(
        connection.poll(at(start, 9_000_000)),
        Output::Event(_)
    ));
}
