//! Owning, sequence-bound SDK first-packet observer slots with explicit polling.
use crate::{MediaType, ffi, peer::PeerInner};
use std::{cell::Cell, fmt, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpObserverError {
    ZeroCapacity,
    AlreadyRegistered,
    NativeRejected,
}
impl fmt::Display for RtpObserverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RtpObserverError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpPacketEvent {
    FirstPacketSent(MediaType),
    /// SDK notification for this observer slot, not proof of delivery to this
    /// receiver's SSRC. The pinned SDK can notify every receiver of the same
    /// media type simultaneously regardless of which receiver got the packet.
    FirstPacketReceived(MediaType),
    FirstPacketReceivedAfterReceptiveChange(MediaType),
}
fn event(value: ffi::FfiRtpPacketEvent) -> Option<RtpPacketEvent> {
    if !value.present {
        return None;
    }
    let kind = MediaType::from_native(value.media_type);
    Some(match value.event_type {
        0 => RtpPacketEvent::FirstPacketSent(kind),
        1 => RtpPacketEvent::FirstPacketReceived(kind),
        2 => RtpPacketEvent::FirstPacketReceivedAfterReceptiveChange(kind),
        _ => unreachable!("private native event discriminator"),
    })
}
pub(crate) struct SenderObserverInner {
    // Native retires its slot and destroys the observer before the peer lease.
    native: cxx::UniquePtr<ffi::NativeRtpSenderObserver>,
    _peer: Rc<PeerInner>,
    registered: Cell<bool>,
}
/// One shared native observer slot/queue per actual SDK interface identity.
/// Sequence-bound; aliases cannot independently replace or retire each other.
/// Explicit polling only: callbacks enqueue records without executing Rust or
/// scheduling application work. The queue evicts oldest on overflow.
#[derive(Clone)]
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::RtpSenderObserver>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::RtpSenderObserver>();
/// ```
pub struct RtpSenderObserver(pub(crate) Rc<SenderObserverInner>);
impl RtpSenderObserver {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeRtpSenderObserver>,
        peer: Rc<PeerInner>,
    ) -> Self {
        assert!(!native.is_null());
        let identity = ffi::rtp_sender_observer_identity(native.as_ref().unwrap());
        let mut handles = peer.sender_observers.borrow_mut();
        handles.retain(|_, weak| weak.strong_count() != 0);
        if let Some(inner) = handles.get(&identity).and_then(std::rc::Weak::upgrade) {
            return Self(inner);
        }
        let inner = Rc::new(SenderObserverInner {
            native,
            _peer: peer.clone(),
            registered: Cell::new(false),
        });
        handles.insert(identity, Rc::downgrade(&inner));
        Self(inner)
    }
    fn native(&self) -> &ffi::NativeRtpSenderObserver {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    /// Starts a fresh queue epoch and preserves SDK synchronous replay behavior.
    pub fn register_observer(&self, capacity: usize) -> Result<(), RtpObserverError> {
        if capacity == 0 {
            return Err(RtpObserverError::ZeroCapacity);
        }
        if self.0.registered.get() {
            return Err(RtpObserverError::AlreadyRegistered);
        }
        if !ffi::rtp_sender_observer_register(self.native(), capacity) {
            return Err(RtpObserverError::NativeRejected);
        }
        self.0.registered.set(true);
        Ok(())
    }
    /// Serializes with native callbacks; queued records remain available.
    pub fn unregister_observer(&self) {
        ffi::rtp_sender_observer_unregister(self.native());
        self.0.registered.set(false);
    }
    pub fn take_event(&self) -> Option<RtpPacketEvent> {
        event(ffi::rtp_sender_observer_take(self.native()))
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::rtp_sender_observer_take_dropped(self.native())
    }
}
pub(crate) struct ReceiverObserverInner {
    // Native retires its slot and destroys the observer before the peer lease.
    native: cxx::UniquePtr<ffi::NativeRtpReceiverObserver>,
    _peer: Rc<PeerInner>,
    registered: Cell<bool>,
}
/// One shared native observer slot/queue per actual SDK interface identity.
/// Sequence-bound; aliases cannot independently replace or retire each other.
/// Explicit polling only: callbacks enqueue records without executing Rust or
/// scheduling application work. The queue evicts oldest on overflow.
#[derive(Clone)]
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::RtpReceiverObserver>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::RtpReceiverObserver>();
/// ```
pub struct RtpReceiverObserver(pub(crate) Rc<ReceiverObserverInner>);
impl RtpReceiverObserver {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeRtpReceiverObserver>,
        peer: Rc<PeerInner>,
    ) -> Self {
        assert!(!native.is_null());
        let identity = ffi::rtp_receiver_observer_identity(native.as_ref().unwrap());
        let mut handles = peer.receiver_observers.borrow_mut();
        handles.retain(|_, weak| weak.strong_count() != 0);
        if let Some(inner) = handles.get(&identity).and_then(std::rc::Weak::upgrade) {
            return Self(inner);
        }
        let inner = Rc::new(ReceiverObserverInner {
            native,
            _peer: peer.clone(),
            registered: Cell::new(false),
        });
        handles.insert(identity, Rc::downgrade(&inner));
        Self(inner)
    }
    fn native(&self) -> &ffi::NativeRtpReceiverObserver {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    /// Starts a fresh queue epoch and preserves SDK synchronous replay behavior.
    pub fn register_observer(&self, capacity: usize) -> Result<(), RtpObserverError> {
        if capacity == 0 {
            return Err(RtpObserverError::ZeroCapacity);
        }
        if self.0.registered.get() {
            return Err(RtpObserverError::AlreadyRegistered);
        }
        if !ffi::rtp_receiver_observer_register(self.native(), capacity) {
            return Err(RtpObserverError::NativeRejected);
        }
        self.0.registered.set(true);
        Ok(())
    }
    /// Serializes with native callbacks; queued records remain available.
    pub fn unregister_observer(&self) {
        ffi::rtp_receiver_observer_unregister(self.native());
        self.0.registered.set(false);
    }
    pub fn take_event(&self) -> Option<RtpPacketEvent> {
        event(ffi::rtp_receiver_observer_take(self.native()))
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::rtp_receiver_observer_take_dropped(self.native())
    }
}
