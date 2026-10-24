//! Owning receivers on an actual packet socket's native callback lists.
use crate::{PacketSocketContext, PacketSocketEventError, SentPacketInfo, SocketAddress, ffi};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

/// Nonexclusive native notification lists. Received-packet registration is
/// exclusive and intentionally not represented as a passive observation here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PacketSocketEventKind {
    Closed,
    ReadyToSend,
    Connected,
    Sent,
    AddressReady,
}
impl PacketSocketEventKind {
    fn tag(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::ReadyToSend => 1,
            Self::Connected => 2,
            Self::Sent => 4,
            Self::AddressReady => 5,
        }
    }
}
/// Deeply owned callback data, independent of the native callback loan. No
/// native socket pointer or callback-scoped packet/address reference escapes.
#[derive(Clone, Debug)]
pub enum PacketSocketEvent {
    Closed {
        error: i32,
    },
    ReadyToSend,
    Connected,
    Sent(SentPacketInfo),
    AddressReady(SocketAddress),
    /// Only delivered by OwnedPacketSocket::register_received, not subscribe.
    Received(crate::ReceivedIpPacket),
}

/// Sequence-bound receiver ownership. Drop/cancel disables delivery immediately;
/// tagged native removal and capture release finish after any active Send (or
/// on the network-role queue after wrong-cooperative-queue cancellation). The
/// deprecated untagged overload has no physical native removal. Failure is
/// local to this receiver and remains observable after cancellation/retirement.
/// A receiver registered during product creation binds to that actual facade;
/// rejected construction cancels it without a synthesized notification.
pub struct PacketSocketSubscription {
    native: Option<cxx::UniquePtr<ffi::NativePacketSocketSubscription>>,
    failed: Rc<Cell<bool>>,
    sequence: PhantomData<Rc<()>>,
}
impl Drop for PacketSocketSubscription {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl PacketSocketSubscription {
    pub fn cancel(&self) -> bool {
        let _turn = crate::execution::controlled_owner_turn();
        ffi::socket_subscription_cancel(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn is_active(&self) -> bool {
        ffi::socket_subscription_active(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn is_bound(&self) -> bool {
        ffi::socket_subscription_bound(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn has_failed(&self) -> bool {
        self.failed.get()
    }
}
impl PacketSocketContext {
    pub fn subscribe(
        &self,
        event: PacketSocketEventKind,
        callback: impl Fn(PacketSocketEvent) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        self.subscribe_inner(event, false, callback)
    }
    /// Uses the SDK's deprecated untagged connect overload. Cancellation releases
    /// the Rust capture and disables delivery; SDK has no removal operation for
    /// this overload, so its empty weak receiver shell lasts until facade Drop.
    /// Prefer subscribe(Connected, ...) for repeated registration/removal.
    pub fn subscribe_connect_untagged(
        &self,
        callback: impl Fn(PacketSocketEvent) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        self.subscribe_inner(PacketSocketEventKind::Connected, true, callback)
    }
    pub(crate) fn subscribe_received(
        &self,
        callback: impl Fn(PacketSocketEvent) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        self.subscribe_tag(3, false, callback)
    }
    fn subscribe_inner(
        &self,
        event: PacketSocketEventKind,
        legacy: bool,
        callback: impl Fn(PacketSocketEvent) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        self.subscribe_tag(event.tag(), legacy, callback)
    }
    fn subscribe_tag(
        &self,
        tag: u8,
        legacy: bool,
        callback: impl Fn(PacketSocketEvent) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let failed = Rc::new(Cell::new(false));
        // The native entry takes ownership even on rejection. RustSocketCallback
        // Drop contains a rejected capture's destructor panic as well.
        let result = ffi::socket_observe(
            self.observation_native(),
            tag,
            legacy,
            Box::new(RustSocketCallback {
                callback: Some(Box::new(callback)),
                failed: failed.clone(),
            }),
        );
        crate::packet_socket::event_result(result.status)?;
        Ok(PacketSocketSubscription {
            native: Some(result.subscription),
            failed,
            sequence: PhantomData,
        })
    }
}
pub(crate) struct RustSocketCallback {
    callback: Option<Box<dyn Fn(PacketSocketEvent)>>,
    failed: Rc<Cell<bool>>,
}
impl Drop for RustSocketCallback {
    fn drop(&mut self) {
        let capture = self.callback.take();
        if crate::provider_panic::catch_provider(|| drop(capture)).is_err() {
            self.failed.set(true);
        }
    }
}
pub(crate) fn socket_callback_fire(
    source: &RustSocketCallback,
    value: ffi::FfiSocketObservation,
) -> bool {
    if source.failed.get() {
        return false;
    }
    let result = crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
        let event = match value.kind {
            0 => PacketSocketEvent::Closed { error: value.error },
            1 => PacketSocketEvent::ReadyToSend,
            2 => PacketSocketEvent::Connected,
            3 => PacketSocketEvent::Received(crate::ReceivedIpPacket::from_native(value.packet)),
            4 => PacketSocketEvent::Sent(SentPacketInfo::from_native(value.sent)),
            5 => PacketSocketEvent::AddressReady(SocketAddress::from_native(value.address)),
            _ => unreachable!("private socket callback channel"),
        };
        source.callback.as_ref().unwrap()(event);
    }));
    if result.is_err() {
        source.failed.set(true);
    }
    !source.failed.get()
}
