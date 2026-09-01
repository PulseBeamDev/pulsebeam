use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use pulsebeam_rtc::{
    ChannelId, DatagramProtocol, EgressSlot, IngressDatagram, IngressStream, MaxMessageSize,
    MediaPacket, RtcConfiguration, RtcConnectionState, RtcEvent, RtcPeer, RtcPeerError,
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

assert_not_impl!(MediaPacket, Sync);

#[test]
fn facade_types_compile_for_external_consumers() {
    assert_send::<RtcPeer>();
    assert_send::<MediaPacket>();
}

#[test]
fn checked_inputs_reject_invalid_values() {
    assert!(IngressStream::new(0).is_none());
    assert!(EgressSlot::new(0).is_none());
    assert_eq!(ChannelId::new(0).expect("stream zero is valid").get(), 0);
    assert_eq!(
        pulsebeam_rtc::DataChannel::new(0)
            .expect("stream zero is valid")
            .get(),
        0
    );
    assert_eq!(
        IngressStream::new(7)
            .expect("nonzero stream IDs are valid")
            .get(),
        7
    );
    assert_eq!(
        EgressSlot::new(9)
            .expect("nonzero slot IDs are valid")
            .get(),
        9
    );
    assert_eq!(
        ChannelId::new(11)
            .expect("nonzero channel IDs are valid")
            .get(),
        11
    );

    let configuration = RtcConfiguration::new(1, 1, 1, 1).expect("positive bounds are valid");
    assert_eq!(configuration.max_ingress_streams(), 1);
    assert_eq!(configuration.max_egress_slots(), 1);
    assert_eq!(configuration.max_events(), 1);
    assert_eq!(configuration.max_transmissions(), 1);
    assert!(RtcConfiguration::new(1, 1, 1, 0).is_none());
    assert!(RtcConfiguration::new(513, 1, 1, 1).is_none());
    assert!(RtcConfiguration::new(1, 513, 1, 1).is_none());

    let endpoint = SocketAddr::from(([127, 0, 0, 1], 4000));
    assert!(IngressDatagram::new(DatagramProtocol::Udp, endpoint, endpoint, Vec::new()).is_none());
}

#[test]
fn data_channel_maximum_is_explicit() {
    assert_eq!(MaxMessageSize::default(), MaxMessageSize::Default);
    assert_eq!(
        MaxMessageSize::finite(65536),
        Some(MaxMessageSize::finite(65536).expect("finite size"))
    );
    assert_eq!(MaxMessageSize::finite(0), None);
    assert!(MaxMessageSize::Unlimited.is_unlimited());
}

#[test]
fn close_preserves_bounds_and_reports_terminal_facts() {
    let configuration = RtcConfiguration::new(1, 1, 1, 1).expect("positive bounds are valid");
    let mut peer = RtcPeer::new(configuration);
    let now = Instant::now();

    assert_eq!(peer.state(), RtcConnectionState::Configured);
    assert_eq!(peer.next_deadline(), None);
    assert!(peer.poll_transmit().is_none());
    assert_eq!(
        peer.close(now, pulsebeam_rtc::CloseReason::Application),
        Err(RtcPeerError::QueueFull)
    );
    assert_eq!(peer.state(), RtcConnectionState::Configured);
    assert_eq!(peer.close_reason(), None);
    assert!(matches!(
        peer.poll_event(),
        Some(RtcEvent::ConnectionStateChanged(
            RtcConnectionState::Configured
        ))
    ));

    peer.close(now, pulsebeam_rtc::CloseReason::Application)
        .expect("space is available after polling");
    assert_eq!(peer.state(), RtcConnectionState::Closed);
    assert_eq!(
        peer.close_reason(),
        Some(pulsebeam_rtc::CloseReason::Application)
    );
    assert!(matches!(
        peer.poll_event(),
        Some(RtcEvent::Closed(pulsebeam_rtc::CloseReason::Application))
    ));
    assert!(peer.poll_event().is_none());
    assert_eq!(
        peer.close(now, pulsebeam_rtc::CloseReason::Timeout),
        Err(RtcPeerError::Closed)
    );
}

#[test]
fn close_rejects_backward_time() {
    let mut peer = RtcPeer::new(RtcConfiguration::default());
    let now = Instant::now();
    assert_eq!(
        peer.handle_timeout(now + Duration::from_secs(1)),
        Err(RtcPeerError::NotNegotiated)
    );
    assert_eq!(
        peer.close(now, pulsebeam_rtc::CloseReason::Timeout),
        Err(RtcPeerError::InvalidInput)
    );
    assert_eq!(peer.state(), RtcConnectionState::Configured);
}
