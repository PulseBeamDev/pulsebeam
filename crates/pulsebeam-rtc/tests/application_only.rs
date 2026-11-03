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
    let mut connection_received = None;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(opened) => {
                assert_eq!(opened.value(), 0);
                assert!(channel.replace(opened).is_none());
            }
            FixtureDataEvent::PeerOpened => {
                assert!(!peer_opened);
                peer_opened = true;
                fixture.peer_send(true, b"from peer");
            }
            FixtureDataEvent::ConnectionMessage {
                channel: received,
                message,
            } => {
                assert_eq!(channel, Some(received), "Opened must precede first DATA");
                assert!(connection_received.replace(message).is_none());
            }
            _ => {}
        }
    }
    fixture
        .connection
        .send_data(
            fixture.at(),
            channel.expect("application-only channel"),
            DataMessage::Binary(b"from connection".to_vec().into()),
        )
        .expect("send application-only message");
    let mut peer_received = false;
    for _ in 0..32 {
        match fixture.next_data_event() {
            FixtureDataEvent::PeerMessage { binary, payload } => {
                assert!(binary);
                assert_eq!(payload, b"from connection");
                peer_received = true;
            }
            FixtureDataEvent::ConnectionMessage {
                channel: received,
                message,
            } => {
                assert_eq!(channel, Some(received));
                assert!(connection_received.replace(message).is_none());
            }
            _ => {}
        }
        let stats = fixture.connection.stats().connection;
        assert_eq!(stats.feedback, None);
        assert_eq!(stats.transmitted_rtp_bytes, 0);
        assert_eq!(stats.transmitted_padding_bytes, 0);
        if peer_received && connection_received.is_some() {
            assert_eq!(
                connection_received,
                Some(DataMessage::Binary(b"from peer".to_vec().into()))
            );
            return;
        }
    }
    panic!("SCTP-only messages did not arrive in both directions");
}
