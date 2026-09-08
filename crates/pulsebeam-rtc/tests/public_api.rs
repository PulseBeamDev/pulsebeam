use std::{any::TypeId, time::Duration};

use pulsebeam_rtc::{
    Connection, ConnectionConfig, ConnectionLimits, DataChannelId, FrameDependencies, FrameId,
    GlobalMediaTime, IceTcpFlowId, MediaPacket, MediaPriority, PlayoutDelay, PolicyError,
};

fn assert_send<T: Send>() {}

macro_rules! assert_not_impl {
    ($type:ty, $trait:path) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn marker() {}
            }

            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}

            struct Invalid;

            impl<T: ?Sized + $trait> AmbiguousIfImpl<Invalid> for T {}

            let _ = <$type as AmbiguousIfImpl<_>>::marker;
        };
    };
}

assert_not_impl!(Connection, Sync);
assert_not_impl!(MediaPacket, Sync);

#[test]
fn facade_values_are_send_but_connection_owned_values_are_not_sync() {
    assert_send::<Connection>();
    assert_send::<MediaPacket>();
}

#[test]
fn stable_identifiers_are_nominally_distinct() {
    assert_ne!(TypeId::of::<DataChannelId>(), TypeId::of::<FrameId>());
    assert_ne!(TypeId::of::<FrameId>(), TypeId::of::<IceTcpFlowId>());
    assert_eq!(DataChannelId::from_value(7).value(), 7);
    assert_eq!(FrameId::from_value(9).value(), 9);
    assert_eq!(IceTcpFlowId::from_value(11).value(), 11);
}

#[test]
fn global_media_time_has_exact_big_endian_wire_representation() {
    assert_eq!(GlobalMediaTime::from_micros(0).to_be_bytes(), [0; 8]);
    assert_eq!(
        GlobalMediaTime::from_micros(1).to_be_bytes(),
        [0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(
        GlobalMediaTime::from_micros(u64::MAX).to_be_bytes(),
        [u8::MAX; 8]
    );

    let encoded = 0x0102_0304_0506_0708_u64.to_be_bytes();
    assert_eq!(
        GlobalMediaTime::from_be_bytes(encoded).as_micros(),
        0x0102_0304_0506_0708
    );
    assert_eq!(
        GlobalMediaTime::from_micros(u64::MAX).checked_add(Duration::from_micros(1)),
        None
    );
    assert_eq!(
        GlobalMediaTime::from_micros(0).checked_sub(Duration::from_micros(1)),
        None
    );
}

#[test]
fn configured_hard_limits_reject_invalid_values() {
    let defaults = ConnectionLimits::default();
    assert_eq!(defaults.validate(), Ok(defaults));

    let invalid = ConnectionLimits {
        max_unsignaled_encodings: ConnectionLimits::HARD_MAX_UNSIGNALED_ENCODINGS + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_data_channels: ConnectionLimits::HARD_MAX_DATA_CHANNELS + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_inbound_data_message_bytes: ConnectionLimits::HARD_MAX_INBOUND_DATA_MESSAGE_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_buffered_data_bytes: ConnectionLimits::HARD_MAX_BUFFERED_DATA_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_queued_media_bytes: ConnectionLimits::HARD_MAX_QUEUED_MEDIA_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_retransmission_bytes: ConnectionLimits::HARD_MAX_RETRANSMISSION_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    assert!(ConnectionConfig::default().validate().is_ok());
}

#[test]
fn policy_values_reject_unrepresentable_construction() {
    assert_eq!(MediaPriority::new(0), Err(PolicyError::PriorityOutOfRange));
    assert_eq!(
        MediaPriority::new(257),
        Err(PolicyError::PriorityOutOfRange)
    );
    assert_eq!(MediaPriority::new(1).map(MediaPriority::weight), Ok(1));
    assert_eq!(
        PlayoutDelay::from_millis_exact(1, 10),
        Err(PolicyError::PlayoutNotExactlyRepresentable)
    );
    assert_eq!(
        PlayoutDelay::from_ticks(2, 1),
        Err(PolicyError::PlayoutRange)
    );
    assert_eq!(FrameDependencies::known([FrameId::from_value(1); 9]), None);
}
