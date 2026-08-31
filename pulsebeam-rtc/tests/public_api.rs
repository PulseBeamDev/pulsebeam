use pulsebeam_rtc::{MediaPacket, RtcPeer};

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

fn consume_facade(peer: RtcPeer, packet: &MediaPacket) -> (RtcPeer, u64) {
    let _ = packet.payload();
    (peer, packet.packet_id())
}

#[test]
fn facade_types_compile_for_external_consumers() {
    assert_send::<RtcPeer>();
    let _ = consume_facade;
}
