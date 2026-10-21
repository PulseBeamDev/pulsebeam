#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "boundary fixtures intentionally construct exact limit-plus-one values"
)]

mod support;

use bytes::Bytes;
use pulsebeam_rtc::{AcceptError, ConnectionLimits, DataMessage, WriteError};
use support::{FixtureDataEvent, PeerFixture, rtp_write};

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
            max_unsignaled_encodings: hard.max_unsignaled_encodings + 1,
            ..hard
        },
        ConnectionLimits {
            max_data_channels: hard.max_data_channels + 1,
            ..hard
        },
        ConnectionLimits {
            max_inbound_data_message_bytes: hard.max_inbound_data_message_bytes + 1,
            ..hard
        },
        ConnectionLimits {
            max_buffered_data_bytes: hard.max_buffered_data_bytes + 1,
            ..hard
        },
        ConnectionLimits {
            max_queued_media_bytes: hard.max_queued_media_bytes + 1,
            ..hard
        },
        ConnectionLimits {
            max_retransmission_bytes: hard.max_retransmission_bytes + 1,
            ..hard
        },
    ] {
        assert_eq!(invalid.validate(), Err(AcceptError::InvalidConfiguration));
    }
}

#[test]
fn media_queue_admission_is_exact_and_preserves_ownership_and_index_on_retry() {
    let mut source = PeerFixture::connected();
    let mut fixture = PeerFixture::connected_with_media_limit(2);
    let packet = source.send_source(&[0xf8]);
    let mid = fixture.sender_mid.clone();
    for id in 1..=2 {
        fixture
            .connection
            .write_rtp(fixture.at(), &mid, rtp_write(packet.clone(), id))
            .expect("one-byte queue slot");
    }
    let write = rtp_write(packet, 3);
    let storage = write.payload.as_ptr();
    let rejected = fixture
        .connection
        .write_rtp(fixture.at(), &mid, write.clone())
        .expect_err("full queue");
    assert_eq!(rejected.reason, WriteError::WouldBlock);
    assert_eq!(rejected.packet, write);
    assert_eq!(rejected.packet.payload.as_ptr(), storage);
    assert_eq!(
        fixture.connection.stats().senders[0].queued_payload_bytes,
        2
    );
    fixture.drive_for(std::time::Duration::from_millis(50));
    fixture
        .connection
        .write_rtp(fixture.at(), &mid, rejected.packet)
        .expect("retry the original owned index");
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
    fixture
        .connection
        .send_data(
            fixture.at(),
            channel,
            DataMessage::Binary(Bytes::from_static(b"12345678")),
        )
        .expect("exact buffer capacity");
    assert_eq!(fixture.connection.stats().connection.buffered_data_bytes, 8);
    let one_more = Bytes::from(vec![b'x']);
    let storage = one_more.as_ptr();
    let rejected = fixture
        .connection
        .send_data(fixture.at(), channel, DataMessage::Binary(one_more))
        .expect_err("data buffer full");
    assert_eq!(rejected.reason, WriteError::WouldBlock);
    let DataMessage::Binary(payload) = &rejected.message else {
        panic!("message kind preserved");
    };
    assert_eq!(payload.as_ref(), b"x");
    assert_eq!(payload.as_ptr(), storage);
    let config = pulsebeam_rtc::DataChannelConfig {
        id: None,
        label: "second".into(),
        protocol: "test".into(),
        ordered: true,
        reliability: pulsebeam_rtc::DataReliability::Reliable,
        priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
        negotiated: false,
    };
    let label = config.label.as_ptr();
    let (error, config) = fixture
        .connection
        .open_data_channel(fixture.at(), config)
        .expect_err("channel limit");
    assert_eq!(error, WriteError::WouldBlock);
    assert_eq!(config.label.as_ref(), "second");
    assert_eq!(config.label.as_ptr(), label);
    fixture.connection.abort();
    let rejected = fixture
        .connection
        .send_data(fixture.at(), channel, rejected.message)
        .expect_err("closed connection");
    assert_eq!(rejected.reason, WriteError::Closed);
    let DataMessage::Binary(payload) = rejected.message else {
        panic!("message kind preserved");
    };
    assert_eq!(payload.as_ref(), b"x");
    assert_eq!(payload.as_ptr(), storage);
}
