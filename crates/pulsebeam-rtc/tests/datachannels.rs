#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    reason = "integration fixtures exercise the Arc/Bytes public contract and stop at the first violated protocol invariant"
)]

mod support;

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use pulsebeam_rtc::{
    CloseReason, Command, DataChannelConfig, DataChannelPriority, DataMessage, DataReliability,
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata, IceTcpFlowId,
    MediaPayloadBitrate, MediaPriority, Output, PlayoutDelay, SenderPolicy,
};
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
fn tcp_path_loss_holds_user_data_without_closing_or_reusing_protected_output() {
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
    let packet = fixture.send_source(b"committed-before-close");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: support::forwarded(packet, 1),
    });
    assert_eq!(fixture.receive_egress().1, b"committed-before-close");
    let packet = fixture.send_source(b"not-submitted-on-closed-stream");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: support::forwarded(packet, 2),
    });
    fixture.command(Command::SendData {
        channel: channel.expect("channel opened"),
        message: DataMessage::Binary(Bytes::from_static(b"queued-during-path-loss")),
    });
    fixture.command(Command::RetireIceTcpFlow {
        flow: IceTcpFlowId::from_value(1),
    });
    let committed = fixture.connection.stats().connection.transmitted_rtp_bytes;
    for _ in 0..20 {
        fixture.advance_clock_without_poll(Duration::from_millis(100));
        let at = fixture.at();
        let mut idle = false;
        for _ in 0..512 {
            match fixture.connection.poll(at) {
                Output::Idle { next_wakeup } => {
                    assert!(
                        next_wakeup.is_none_or(|deadline| deadline > at.monotonic),
                        "unavailable path must not spin on a transmit deadline"
                    );
                    idle = true;
                    break;
                }
                Output::Transmit(_) => panic!("output escaped to retired stream"),
                Output::Closed(reason) => panic!("temporary path loss closed RTC: {reason:?}"),
                Output::Event(_) => {}
                _ => panic!("unexpected RTC output"),
            }
        }
        assert!(idle, "RTC must reach its scheduled boundary");
    }
    assert_eq!(
        fixture.connection.stats().connection.transmitted_rtp_bytes,
        committed
    );
    fixture.command(Command::CloseGracefully {
        deadline: fixture.at().monotonic,
    });
    for _ in 0..512 {
        match fixture.connection.poll(fixture.at()) {
            Output::Closed(CloseReason::Graceful) => return,
            Output::Event(_) => {}
            output => panic!("unexpected graceful-close output: {output:?}"),
        }
    }
    panic!("graceful close did not reach its terminal boundary");
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
    fixture.command(Command::SendData {
        channel,
        message: DataMessage::Binary(Bytes::from_static(b"terminal-control-reply")),
    });
    let deadline = fixture.at().monotonic + Duration::from_secs(1);
    fixture.command(Command::CloseGracefully { deadline });
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

    fixture.command(Command::OpenDataChannel(config(
        DataReliability::MaxRetransmits(2),
        false,
    )));
    let local = loop {
        if let FixtureDataEvent::ConnectionOpened(channel) = fixture.next_data_event()
            && channel != remote
        {
            break channel;
        }
    };
    fixture.command(Command::SendData {
        channel: local,
        message: DataMessage::Binary(Bytes::from(vec![7; 8_000])),
    });
    loop {
        if let FixtureDataEvent::PeerMessage { binary, payload } = fixture.next_data_event() {
            assert!(binary);
            assert_eq!(payload, vec![7; 8_000]);
            break;
        }
    }

    fixture.command(Command::CloseDataChannel { channel: local });
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
        fixture.command(Command::OpenDataChannel(config(reliability, ordered)));
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
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy: SenderPolicy {
            playout_delay: Some(PlayoutDelay::from_ticks(0, 400).expect("playout policy")),
            priority: MediaPriority::HIGH,
            desired_bitrate: MediaPayloadBitrate::from_bps(1_000_000),
        },
    });
    const COUNT: u64 = 16;
    for index in 0..COUNT {
        fixture.command(Command::SendMedia {
            sender: fixture.sender,
            media: ForwardedMedia {
                packet: packet.clone(),
                frame: FrameMetadata {
                    decode_target_mask: None,
                    id: FrameId::from_value(index + 1),
                    boundary: FrameBoundary::Complete,
                    random_access: true,
                    discardable: false,
                    dependencies: FrameDependencies::Known(Arc::from([])),
                },
            },
        });
        fixture.command(Command::SendData {
            channel,
            message: DataMessage::Binary(Bytes::from(vec![
                u8::try_from(index).unwrap_or(u8::MAX);
                4_000
            ])),
        });
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
    assert!(stats.media_budget.as_bps() <= stats.target_media_bitrate.as_bps());
}
