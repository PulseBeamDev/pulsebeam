#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    reason = "integration fixtures exercise the Arc/Bytes public contract and stop at the first violated protocol invariant"
)]

mod support;

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use pulsebeam_rtc::{DataChannelConfig, DataChannelPriority, DataMessage, DataReliability};
use support::{FixtureCoexistenceEvent, FixtureDataEvent, PeerFixture};

fn config(reliability: DataReliability, ordered: bool) -> DataChannelConfig {
    DataChannelConfig {
        id: None,
        label: Arc::from("pulsebeam"),
        protocol: Arc::from("events"),
        ordered,
        reliability,
        priority: DataChannelPriority::HIGH,
        negotiated: false,
    }
}

#[test]
fn passive_tcp_peer_exchanges_authenticated_rtp_and_channels() {
    let mut fixture = PeerFixture::connected_tcp_datachannels();
    let mut channel = None;
    let mut peer_opened = false;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(opened) => channel = Some(opened),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    let packet = fixture.send_source(b"tcp-media");
    fixture
        .write_rtp(support::rtp_write(packet, 1))
        .expect("write RTP");
    assert_eq!(fixture.receive_egress().1, b"tcp-media");
    fixture
        .connection
        .send_data(
            fixture.at(),
            channel.expect("opened channel"),
            DataMessage::Binary(Bytes::from_static(b"tcp-channel")),
        )
        .expect("send TCP data");
    for _ in 0..32 {
        if let FixtureDataEvent::PeerMessage { binary, payload } = fixture.next_data_event() {
            assert!(binary);
            assert_eq!(payload, b"tcp-channel");
            return;
        }
    }
    panic!("native TCP peer did not receive its channel message");
}

#[test]
fn queued_control_reply_reaches_peer_during_bounded_graceful_close() {
    let mut fixture = PeerFixture::connected_datachannels();
    let mut channel = None;
    let mut peer_opened = false;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(opened) => channel = Some(opened),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    let channel = channel.expect("control channel opened");
    fixture
        .connection
        .send_data(
            fixture.at(),
            channel,
            DataMessage::Binary(Bytes::from_static(b"terminal-control-reply")),
        )
        .expect("send channel message");
    let deadline = fixture.at() + Duration::from_secs(1);
    fixture
        .connection
        .close(fixture.at(), deadline)
        .expect("close gracefully");
    for _ in 0..32 {
        if let FixtureDataEvent::PeerMessage { binary, payload } = fixture.next_data_event() {
            assert!(binary);
            assert_eq!(payload, b"terminal-control-reply");
            return;
        }
    }
    panic!("queued control reply was not drained to the connected peer");
}

#[test]
fn local_and_remote_channels_preserve_messages_and_close() {
    let mut fixture = PeerFixture::connected_datachannels();

    let mut remote = None;
    let mut peer_opened = false;
    while remote.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(channel) => remote = Some(channel),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    let remote = remote.expect("remote channel opened");
    fixture.peer_send(false, b"from peer");
    assert!(matches!(
        fixture.next_data_event(),
        FixtureDataEvent::ConnectionMessage {
            channel,
            message: DataMessage::Text(ref bytes),
        } if channel == remote && bytes.as_ref() == b"from peer"
    ));

    fixture
        .connection
        .open_data_channel(
            fixture.at(),
            config(DataReliability::MaxRetransmits(2), false),
        )
        .expect("open channel");
    let local = loop {
        if let FixtureDataEvent::ConnectionOpened(channel) = fixture.next_data_event()
            && channel != remote
        {
            break channel;
        }
    };
    fixture
        .connection
        .send_data(
            fixture.at(),
            local,
            DataMessage::Binary(Bytes::from(vec![7; 8_000])),
        )
        .expect("send channel message");
    loop {
        if let FixtureDataEvent::PeerMessage { binary, payload } = fixture.next_data_event() {
            assert!(binary);
            assert_eq!(payload, vec![7; 8_000]);
            break;
        }
    }

    fixture
        .connection
        .close_data_channel(fixture.at(), local)
        .expect("close channel");
    let mut local_closed = 0;
    let mut peer_closed = 0;
    while local_closed == 0 || peer_closed == 0 {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionClosed(channel) if channel == local => local_closed += 1,
            FixtureDataEvent::PeerClosed => peer_closed += 1,
            _ => {}
        }
    }
    assert_eq!(local_closed, 1);
    assert_eq!(peer_closed, 1);
}

#[test]
fn all_reliability_modes_open_over_the_runtime_transport_seam() {
    let mut fixture = PeerFixture::connected_datachannels();
    let mut initial_connection_opened = false;
    let mut initial_peer_opened = false;
    while !initial_connection_opened || !initial_peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(_) => initial_connection_opened = true,
            FixtureDataEvent::PeerOpened => initial_peer_opened = true,
            _ => {}
        }
    }
    for (reliability, ordered) in [
        (DataReliability::Reliable, true),
        (DataReliability::MaxRetransmits(0), false),
        (
            DataReliability::MaxLifetime(Duration::from_millis(50)),
            true,
        ),
    ] {
        fixture
            .connection
            .open_data_channel(fixture.at(), config(reliability, ordered))
            .expect("open channel");
        let mut connection_opened = false;
        let mut peer_opened = false;
        while !connection_opened || !peer_opened {
            match fixture.next_data_event() {
                FixtureDataEvent::ConnectionOpened(_) => connection_opened = true,
                FixtureDataEvent::PeerOpened => peer_opened = true,
                _ => {}
            }
        }
    }
}

#[test]
fn fragmented_data_and_rtp_share_service_without_cross_accounting_starvation() {
    let mut fixture = PeerFixture::connected_datachannels();
    let mut channel = None;
    let mut peer_opened = false;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(id) => channel = Some(id),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    let channel = channel.expect("channel");
    fixture.configure_bottleneck(500_000);
    let packet = fixture.send_source(b"coexistence");
    const COUNT: u64 = 16;
    for index in 0..COUNT {
        let mut write = support::rtp_write(packet.clone(), index + 1);
        write.timestamp = write
            .timestamp
            .wrapping_add(u32::try_from(index).expect("short trace") * 960);
        fixture.write_rtp(write).expect("queue original RTP");
        fixture
            .connection
            .send_data(
                fixture.at(),
                channel,
                DataMessage::Binary(Bytes::from(vec![
                    u8::try_from(index).expect("message index");
                    4_000
                ])),
            )
            .expect("queue fragmented data");
    }
    let mut rtp = 0;
    let mut data = 0;
    while rtp < COUNT || data < COUNT {
        match fixture.next_coexistence_event() {
            FixtureCoexistenceEvent::Rtp(payload) => {
                assert_eq!(payload, b"coexistence");
                rtp += 1;
            }
            FixtureCoexistenceEvent::Data { binary, payload } => {
                assert!(binary);
                assert_eq!(
                    payload,
                    vec![u8::try_from(data).expect("message index"); 4_000]
                );
                data += 1;
            }
        }
    }
    assert_eq!((rtp, data), (COUNT, COUNT));
    let stats = fixture.connection.stats().connection;
    assert!(stats.transmitted_sctp_bytes > stats.transmitted_rtp_bytes);
    assert!(
        stats.rtp_bytes_in_flight
            <= stats
                .transmitted_rtp_bytes
                .saturating_add(stats.transmitted_padding_bytes)
    );
}
