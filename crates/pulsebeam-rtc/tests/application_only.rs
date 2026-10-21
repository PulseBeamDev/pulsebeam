#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration fixtures stop at violated protocol invariants"
)]

mod support;

use pulsebeam_rtc::DataMessage;
use support::{FixtureDataEvent, PeerFixture};

#[test]
fn application_only_peer_exchanges_messages_without_rtp_accounting() {
    let mut fixture = PeerFixture::connected_application_only();
    assert!(fixture.senders.is_empty());
    let mut channel = None;
    let mut peer_opened = false;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(opened) => channel = Some(opened),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    fixture.peer_send(true, b"from peer");
    fixture
        .connection
        .send_data(
            fixture.at(),
            channel.expect("application-only channel"),
            DataMessage::Binary(b"from connection".to_vec().into()),
        )
        .expect("send application-only message");
    let mut peer_received = false;
    let mut connection_received = false;
    for _ in 0..32 {
        match fixture.next_data_event() {
            FixtureDataEvent::PeerMessage { binary, payload } => {
                assert!(binary);
                assert_eq!(payload, b"from connection");
                peer_received = true;
            }
            FixtureDataEvent::ConnectionMessage { message, .. } => {
                assert_eq!(message, DataMessage::Binary(b"from peer".to_vec().into()));
                connection_received = true;
            }
            _ => {}
        }
        let stats = fixture.connection.stats().connection;
        assert_eq!(stats.feedback, None);
        assert_eq!(stats.transmitted_rtp_bytes, 0);
        assert_eq!(stats.transmitted_padding_bytes, 0);
        if peer_received && connection_received {
            return;
        }
    }
    panic!("SCTP-only messages did not arrive in both directions");
}
