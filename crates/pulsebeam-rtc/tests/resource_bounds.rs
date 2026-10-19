#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "boundary tests intentionally construct exact limit-plus-one values"
)]

mod support;

use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptError, Command, CommandError, ConnectionLimits, DataMessage, FrameDependencies, FrameId,
};
use support::{FixtureDataEvent, PeerFixture, forwarded};

#[test]
fn every_configured_limit_accepts_its_hard_maximum_and_rejects_limit_plus_one() {
    let hard = ConnectionLimits {
        max_unsignaled_encodings: ConnectionLimits::HARD_MAX_UNSIGNALED_ENCODINGS,
        max_data_channels: ConnectionLimits::HARD_MAX_DATA_CHANNELS,
        max_inbound_data_message_bytes: ConnectionLimits::HARD_MAX_INBOUND_DATA_MESSAGE_BYTES,
        max_buffered_data_bytes: ConnectionLimits::HARD_MAX_BUFFERED_DATA_BYTES,
        max_queued_media_bytes: ConnectionLimits::HARD_MAX_QUEUED_MEDIA_BYTES,
        max_retransmission_bytes: ConnectionLimits::HARD_MAX_RETRANSMISSION_BYTES,
    };
    assert_eq!(hard.validate(), Ok(hard));

    for invalid in [
        ConnectionLimits {
            max_unsignaled_encodings: ConnectionLimits::HARD_MAX_UNSIGNALED_ENCODINGS + 1,
            ..hard
        },
        ConnectionLimits {
            max_data_channels: ConnectionLimits::HARD_MAX_DATA_CHANNELS + 1,
            ..hard
        },
        ConnectionLimits {
            max_inbound_data_message_bytes: ConnectionLimits::HARD_MAX_INBOUND_DATA_MESSAGE_BYTES
                + 1,
            ..hard
        },
        ConnectionLimits {
            max_buffered_data_bytes: ConnectionLimits::HARD_MAX_BUFFERED_DATA_BYTES + 1,
            ..hard
        },
        ConnectionLimits {
            max_queued_media_bytes: ConnectionLimits::HARD_MAX_QUEUED_MEDIA_BYTES + 1,
            ..hard
        },
        ConnectionLimits {
            max_retransmission_bytes: ConnectionLimits::HARD_MAX_RETRANSMISSION_BYTES + 1,
            ..hard
        },
    ] {
        assert_eq!(invalid.validate(), Err(AcceptError::InvalidConfiguration));
    }
}

#[test]
fn media_queue_admission_is_exact_and_preserves_ownership_on_would_block() {
    let mut source = PeerFixture::connected();
    let mut fixture = PeerFixture::connected_with_media_limit(2);
    source.drive_for(
        fixture
            .at()
            .monotonic
            .saturating_duration_since(source.at().monotonic),
    );
    let packet = source.send_source(&[0xf8]);

    for id in 1..=2 {
        assert_eq!(
            fixture
                .connection
                .command(
                    fixture.at(),
                    Command::SendMedia {
                        sender: fixture.sender,
                        media: forwarded(packet.clone(), id),
                    },
                )
                .map_err(|rejected| rejected.reason),
            Ok(())
        );
    }
    let rejected = forwarded(packet, 3);
    let rejected_global = rejected.packet.global_media_at();
    let packet_storage = rejected.packet.bytes().as_ptr();
    let rejected = fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: rejected,
            },
        )
        .expect_err("queue is full");
    assert_eq!(rejected.reason, CommandError::WouldBlock);
    let Command::SendMedia { media, .. } = &rejected.command else {
        panic!("rejected command retains its variant");
    };
    assert_eq!(media.packet.global_media_at(), rejected_global);
    assert_eq!(media.packet.bytes().as_ptr(), packet_storage);
    assert_eq!(
        fixture.connection.stats().senders[0].queued_payload_bytes,
        2
    );
    fixture.drive_for(std::time::Duration::from_millis(50));
    fixture
        .connection
        .command(fixture.at(), rejected.command)
        .expect("retry the original owned command");
}

#[test]
fn data_channel_count_and_buffered_payload_stop_at_configured_limits() {
    let limits = ConnectionLimits {
        max_data_channels: 1,
        max_buffered_data_bytes: 8,
        ..ConnectionLimits::default()
    };
    let mut fixture = PeerFixture::connected_with_limits(limits, true);
    let channel = loop {
        if let FixtureDataEvent::ConnectionOpened(channel) = fixture.next_data_event() {
            break channel;
        }
    };

    assert_eq!(
        fixture
            .connection
            .command(
                fixture.at(),
                Command::SendData {
                    channel,
                    message: DataMessage::Binary(Bytes::from_static(b"12345678")),
                },
            )
            .map_err(|rejected| rejected.reason),
        Ok(())
    );
    assert_eq!(fixture.connection.stats().connection.buffered_data_bytes, 8);
    let one_more = Bytes::from(vec![b'x']);
    let payload_storage = one_more.as_ptr();
    let rejected = fixture
        .connection
        .command(
            fixture.at(),
            Command::SendData {
                channel,
                message: DataMessage::Binary(one_more),
            },
        )
        .expect_err("data buffer is full");
    assert_eq!(rejected.reason, CommandError::WouldBlock);
    let Command::SendData {
        message: DataMessage::Binary(payload),
        ..
    } = &rejected.command
    else {
        panic!("rejected data keeps its channel and message");
    };
    assert_eq!(payload.as_ref(), b"x");
    assert_eq!(payload.as_ptr(), payload_storage);

    let config = pulsebeam_rtc::DataChannelConfig {
        id: None,
        label: "second".into(),
        protocol: "test".into(),
        ordered: true,
        reliability: pulsebeam_rtc::DataReliability::Reliable,
        priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
        negotiated: false,
    };
    let label_storage = config.label.as_ptr();
    let rejected_open = fixture
        .connection
        .command(fixture.at(), Command::OpenDataChannel(config))
        .expect_err("channel limit");
    assert_eq!(rejected_open.reason, CommandError::WouldBlock);
    let Command::OpenDataChannel(config) = rejected_open.command else {
        panic!("rejected channel retains its configuration");
    };
    assert_eq!(config.label.as_ref(), "second");
    assert_eq!(config.label.as_ptr(), label_storage);
    fixture.command(Command::Abort);
    let rejected = fixture
        .connection
        .command(fixture.at(), rejected.command)
        .expect_err("closed connection");
    assert_eq!(rejected.reason, CommandError::Closed);
    let Command::SendData {
        message: DataMessage::Binary(payload),
        ..
    } = rejected.command
    else {
        panic!("closed-state rejection retains data");
    };
    assert_eq!(payload.as_ref(), b"x");
    assert_eq!(payload.as_ptr(), payload_storage);
}

#[test]
fn dependency_and_packet_store_bounds_are_constructible_only_through_public_values() {
    let at_limit = (0..FrameDependencies::MAX_DIRECT_DEPENDENCIES)
        .map(|id| FrameId::from_value(u64::try_from(id).expect("small dependency id") + 1))
        .collect::<Vec<_>>();
    assert!(FrameDependencies::known(at_limit).is_some());

    let over_limit = (0..=FrameDependencies::MAX_DIRECT_DEPENDENCIES)
        .map(|id| FrameId::from_value(u64::try_from(id).expect("small dependency id") + 1))
        .collect::<Vec<_>>();
    assert!(FrameDependencies::known(over_limit).is_none());

    assert_eq!(
        ConnectionLimits::DEFAULT_MAX_QUEUED_MEDIA_BYTES,
        8 * 1024 * 1024
    );
    assert_eq!(
        ConnectionLimits::DEFAULT_MAX_RETRANSMISSION_BYTES,
        16 * 1024 * 1024
    );
}
