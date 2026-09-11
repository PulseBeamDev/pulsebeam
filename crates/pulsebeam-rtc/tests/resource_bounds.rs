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
    let packet = source.send_source(b"x");
    let mut fixture = PeerFixture::connected_with_media_limit(2);

    for id in 1..=2 {
        assert_eq!(
            fixture.connection.command(
                fixture.at(),
                Command::SendMedia {
                    sender: fixture.sender,
                    media: forwarded(packet.clone(), id),
                },
            ),
            Ok(())
        );
    }
    let rejected = forwarded(packet, 3);
    let rejected_global = rejected.packet.global_media_at();
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: rejected.clone(),
            },
        ),
        Err(CommandError::WouldBlock)
    );
    assert_eq!(rejected.packet.global_media_at(), rejected_global);
    assert_eq!(
        fixture.connection.stats().senders[0].queued_payload_bytes,
        2
    );
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
        fixture.connection.command(
            fixture.at(),
            Command::SendData {
                channel,
                message: DataMessage::Binary(Bytes::from_static(b"12345678")),
            },
        ),
        Ok(())
    );
    assert_eq!(fixture.connection.stats().connection.buffered_data_bytes, 8);
    let one_more = Bytes::from_static(b"x");
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendData {
                channel,
                message: DataMessage::Binary(one_more.clone()),
            },
        ),
        Err(CommandError::WouldBlock)
    );
    assert_eq!(one_more.as_ref(), b"x");

    let config = pulsebeam_rtc::DataChannelConfig {
        id: None,
        label: "second".into(),
        protocol: "test".into(),
        ordered: true,
        reliability: pulsebeam_rtc::DataReliability::Reliable,
        priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
        negotiated: false,
    };
    assert_eq!(
        fixture
            .connection
            .command(fixture.at(), Command::OpenDataChannel(config)),
        Err(CommandError::WouldBlock)
    );
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
