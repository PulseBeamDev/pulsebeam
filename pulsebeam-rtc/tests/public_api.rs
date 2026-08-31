use std::time::Instant;

use pulsebeam_rtc::{
    DataChannel, DataPayload, EgressSlot, IngressDatagram, IngressStream, MediaPacket,
    MediaRewrite, RtcEvent, RtcNegotiation, RtcPeer, RtcPeerError, TransitMediaPacket, Transmit,
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

fn consume_facade(
    peer: &mut RtcPeer,
    now: Instant,
    datagram: IngressDatagram,
    packet: &MediaPacket,
    transit: &TransitMediaPacket,
    slot: EgressSlot,
    stream: IngressStream,
    rewrite: MediaRewrite,
    channel: DataChannel,
    payload: DataPayload,
) {
    let _ = peer.state();
    let _ = peer.handle_datagram(now, datagram);
    let _ = peer.handle_timeout(now);
    let _ = peer.next_deadline();
    let _ = peer.poll_event();
    let _ = peer.forward(now, slot, packet, rewrite.clone());
    let _ = peer.forward_transit(now, slot, transit, rewrite);
    let _ = peer.request_keyframe(now, stream);
    let _ = peer.set_desired_bitrate(now, 1);
    let _ = peer.set_current_bitrate(now, 1);
    let _ = peer.send_data(now, channel, payload);
    let _ = peer.poll_transmit(now);
}

#[test]
fn facade_types_compile_for_external_consumers() {
    let _: fn(
        Instant,
        u64,
        &str,
        String,
        String,
        Box<[String]>,
    ) -> Result<(RtcPeer, RtcNegotiation), RtcPeerError> = RtcPeer::accept;
    assert_send::<RtcPeer>();
    assert_send::<MediaPacket>();
    assert_send::<RtcEvent>();
    assert_send::<Transmit>();
    let _ = consume_facade;
}
