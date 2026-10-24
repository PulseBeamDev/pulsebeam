//! Sequence-local packet socket products and their native notification context.
use crate::{
    AsyncSocketPacketOptions, Environment, PacketInfo, PacketSocketOption, PacketSocketState,
    ReceivedIpPacket, SentPacketInfo, SocketAddress, ffi,
};
use std::{
    fmt,
    marker::PhantomData,
    num::NonZeroI32,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// A native nonzero GetOption result. The socket's separate error() operation
/// retains the provider's native socket error, rather than translating it here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSocketOptionError(pub NonZeroI32);
impl fmt::Display for PacketSocketOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native socket option result {}", self.0)
    }
}
impl std::error::Error for PacketSocketOptionError {}

/// Implemented and destroyed on the selected network role. Shared receivers
/// permit synchronous SDK read/control reentry; interior state must not hold an
/// exclusive loan while notifying consumers. Signed operation/error results
/// are native results, not Rust byte counts or synthesized I/O success.
pub trait PacketSocket: 'static {
    fn local_address(&self) -> SocketAddress;
    fn remote_address(&self) -> SocketAddress;
    fn send(&self, payload: &[u8], options: &AsyncSocketPacketOptions) -> i32;
    fn send_to(
        &self,
        payload: &[u8],
        address: SocketAddress,
        options: &AsyncSocketPacketOptions,
    ) -> i32;
    fn close(&self) -> i32;
    fn state(&self) -> PacketSocketState;
    fn get_option(&self, option: PacketSocketOption) -> Result<i32, PacketSocketOptionError>;
    fn set_option(&self, option: PacketSocketOption, value: i32) -> i32;
    fn error(&self) -> i32;
    fn set_error(&self, error: i32);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSocketEventError {
    Retired,
    NotReady,
    WrongSequence,
    InProgress,
    ProviderFailed,
    InvalidDecryption,
    ScopeMismatch,
    IncompatibleExecution,
    ReceiverAlreadyRegistered,
    AcceptOwnerAlreadyRegistered,
    NativeConsumerUnavailable,
}
impl fmt::Display for PacketSocketEventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retired => "packet socket has retired",
            Self::NotReady => "packet socket has not been bound to its native product",
            Self::WrongSequence => "packet notification requires its native network role",
            Self::InProgress => "this packet notification channel is already active",
            Self::ProviderFailed => "packet socket provider has failed",
            Self::InvalidDecryption => {
                "packet delivery requires a defined native decryption status"
            }
            Self::ScopeMismatch => "accepted socket belongs to a different native factory scope",
            Self::IncompatibleExecution => {
                "standalone socket requires its selected controlled environment and role"
            }
            Self::AcceptOwnerAlreadyRegistered => {
                "the listener already has an ownership-taking accept receiver"
            }
            Self::NativeConsumerUnavailable => {
                "pinned ICE TCP consumer has no safe accepted-address admission interface"
            }
            Self::ReceiverAlreadyRegistered => {
                "the native socket already has an exclusive receive callback"
            }
        })
    }
}
impl std::error::Error for PacketSocketEventError {}
pub(crate) fn event_result(value: u8) -> Result<(), PacketSocketEventError> {
    match value {
        0 => Ok(()),
        1 => Err(PacketSocketEventError::Retired),
        2 => Err(PacketSocketEventError::NotReady),
        3 => Err(PacketSocketEventError::WrongSequence),
        4 => Err(PacketSocketEventError::InProgress),
        6 => Err(PacketSocketEventError::InvalidDecryption),
        9 => Err(PacketSocketEventError::ScopeMismatch),
        10 => Err(PacketSocketEventError::IncompatibleExecution),
        11 => Err(PacketSocketEventError::ReceiverAlreadyRegistered),
        12 => Err(PacketSocketEventError::AcceptOwnerAlreadyRegistered),
        13 => Err(PacketSocketEventError::NativeConsumerUnavailable),
        _ => Err(PacketSocketEventError::ProviderFailed),
    }
}
struct EventsInner(cxx::SharedPtr<ffi::NativePacketSocketEvents>);
// Native submission synchronizes with retirement before touching the borrowed
// role pointer. Queued messages own only thread-neutral packet values, not Rust
// products or native consumer closures. Native product destruction retires it.
unsafe impl Send for EventsInner {}
unsafe impl Sync for EventsInner {}

/// Cross-thread event submission, always queued to the creating network role.
/// Success admits a message, not proof that a receiver consumed it. Retirement
/// discards admitted but undelivered messages without invoking stale consumers.
#[derive(Clone)]
pub struct PacketSocketEvents(Arc<EventsInner>);
impl PacketSocketEvents {
    fn native(&self) -> &ffi::NativePacketSocketEvents {
        self.0.0.as_ref().unwrap()
    }
    pub fn post_closed(&self, error: i32) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_simple(self.native(), 0, error, true))
    }
    pub fn post_ready_to_send(&self) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_simple(self.native(), 1, 0, true))
    }
    pub fn post_connected(&self) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_simple(self.native(), 2, 0, true))
    }
    pub fn post_received(&self, packet: ReceivedIpPacket) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_received(
            self.native(),
            packet.native(),
            true,
        ))
    }
    pub fn post_sent(&self, packet: SentPacketInfo) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_sent(self.native(), packet.native(), true))
    }
    pub fn post_address_ready(&self, address: SocketAddress) -> Result<(), PacketSocketEventError> {
        event_result(ffi::socket_event_address(
            self.native(),
            ffi::socket_address_clone(address.native()),
            true,
        ))
    }
}

/// Actual creation environment and sequence-bound SDK notification operations.
/// Set provider state before publishing its corresponding native notification.
/// Unlike post_* methods, notify_* methods dispatch synchronously and may reenter
/// the product. Same-channel recursive notification returns InProgress.
#[derive(Clone)]
pub struct PacketSocketContext {
    environment: Environment,
    _resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
    events: PacketSocketEvents,
    failed: Arc<AtomicBool>,
    factory: Option<crate::PacketSocketFactory>,
    sequence: PhantomData<Rc<()>>,
}
impl PacketSocketContext {
    pub fn environment(&self) -> &Environment {
        &self.environment
    }
    pub(crate) fn observation_native(&self) -> &ffi::NativePacketSocketEvents {
        self.events.native()
    }
    pub fn events(&self) -> PacketSocketEvents {
        self.events.clone()
    }
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    pub(crate) fn new(
        environment: Environment,
        resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
        native: cxx::SharedPtr<ffi::NativePacketSocketEvents>,
    ) -> Self {
        assert!(!native.is_null());
        Self {
            environment,
            _resources: resources,
            events: PacketSocketEvents(Arc::new(EventsInner(native))),
            failed: Arc::new(AtomicBool::new(false)),
            factory: None,
            sequence: PhantomData,
        }
    }
    pub(crate) fn with_factory(mut self, factory: Option<crate::PacketSocketFactory>) -> Self {
        self.factory = factory;
        self
    }
    pub fn notify_closed(&self, error: i32) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_simple(
            self.events.native(),
            0,
            error,
            false,
        ))
    }
    pub fn notify_ready_to_send(&self) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_simple(self.events.native(), 1, 0, false))
    }
    pub fn notify_connected(&self) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_simple(self.events.native(), 2, 0, false))
    }
    pub fn notify_received(&self, packet: &ReceivedIpPacket) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_received(
            self.events.native(),
            packet.native(),
            false,
        ))
    }
    pub fn notify_sent(&self, packet: &SentPacketInfo) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_sent(
            self.events.native(),
            packet.native(),
            false,
        ))
    }
    pub fn notify_address_ready(
        &self,
        address: &SocketAddress,
    ) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        event_result(ffi::socket_event_address(
            self.events.native(),
            ffi::socket_address_clone(address.native()),
            false,
        ))
    }
    /// The SDK helper copies packet size and IP overhead from the actual live
    /// socket. This is not computed from cached creation-address metadata.
    pub fn copy_socket_information(
        &self,
        packet_size: usize,
        info: &mut PacketInfo,
    ) -> Result<(), PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let mut native = info.native();
        event_result(ffi::socket_copy_information(
            self.events.native(),
            packet_size,
            &mut native,
        ))?;
        *info = PacketInfo::from_native(native);
        Ok(())
    }
}

pub(crate) struct RustPacketSocket {
    product: Option<Box<dyn PacketSocket>>,
    _context: PacketSocketContext,
    failed: Arc<AtomicBool>,
    aggregate: Arc<AtomicBool>,
}
impl RustPacketSocket {
    pub(crate) fn context(&self) -> PacketSocketContext {
        self._context.clone()
    }
    pub(crate) fn new(
        product: Box<dyn PacketSocket>,
        context: PacketSocketContext,
        aggregate: Arc<AtomicBool>,
    ) -> Box<Self> {
        Self::from_creation(Some(product), context, aggregate)
    }
    pub(crate) fn from_creation(
        product: Option<Box<dyn PacketSocket>>,
        context: PacketSocketContext,
        aggregate: Arc<AtomicBool>,
    ) -> Box<Self> {
        let failed = context.failed.clone();
        Box::new(Self {
            product,
            _context: context,
            failed,
            aggregate,
        })
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.aggregate.store(true, Ordering::Release);
    }
    fn call<T>(&self, failure: T, operation: impl FnOnce(&dyn PacketSocket) -> T) -> T {
        if packet_socket_source_failed(self) {
            return failure;
        }
        let Some(product) = &self.product else {
            return failure;
        };
        match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            operation(product.as_ref())
        })) {
            Ok(value) => value,
            Err(_) => {
                self.fail();
                failure
            }
        }
    }
}
impl Drop for RustPacketSocket {
    fn drop(&mut self) {
        let product = self.product.take();
        if crate::provider_panic::catch_provider(|| drop(product)).is_err() {
            self.fail();
        }
    }
}
pub(crate) fn packet_socket_present(source: &RustPacketSocket) -> bool {
    source.product.is_some()
}
pub(crate) fn packet_socket_source_failed(source: &RustPacketSocket) -> bool {
    source.failed.load(Ordering::Acquire)
}
pub(crate) fn packet_socket_local(
    source: &RustPacketSocket,
    remote: bool,
) -> cxx::UniquePtr<ffi::NativeSocketAddress> {
    let address = source.call(SocketAddress::default(), |product| {
        if remote {
            product.remote_address()
        } else {
            product.local_address()
        }
    });
    ffi::socket_address_clone(address.native())
}
pub(crate) fn packet_socket_send(
    source: &RustPacketSocket,
    payload: Vec<u8>,
    options: ffi::FfiSocketPacketOptions,
) -> i32 {
    source.call(-1, |product| {
        product.send(&payload, &AsyncSocketPacketOptions::from_native(options))
    })
}
pub(crate) fn packet_socket_send_to(
    source: &RustPacketSocket,
    payload: Vec<u8>,
    address: cxx::UniquePtr<ffi::NativeSocketAddress>,
    options: ffi::FfiSocketPacketOptions,
) -> i32 {
    source.call(-1, |product| {
        product.send_to(
            &payload,
            SocketAddress::from_native(address),
            &AsyncSocketPacketOptions::from_native(options),
        )
    })
}
pub(crate) fn packet_socket_close(source: &RustPacketSocket) -> i32 {
    // Cleanup remains callable after operational failure.
    let Some(product) = &source.product else {
        return -1;
    };
    match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| product.close())) {
        Ok(value) => value,
        Err(_) => {
            source.fail();
            -1
        }
    }
}
pub(crate) fn packet_socket_state(source: &RustPacketSocket) -> i32 {
    source
        .call(PacketSocketState::CLOSED, |product| product.state())
        .native_value()
}
pub(crate) fn packet_socket_get_option(
    source: &RustPacketSocket,
    option: i32,
) -> ffi::FfiSocketOptionResult {
    source.call(
        ffi::FfiSocketOptionResult { code: -1, value: 0 },
        |product| match product.get_option(
            PacketSocketOption::from_native_value(option)
                .expect("native Socket::Option representation"),
        ) {
            Ok(value) => ffi::FfiSocketOptionResult { code: 0, value },
            Err(error) => ffi::FfiSocketOptionResult {
                code: error.0.get(),
                value: 0,
            },
        },
    )
}
pub(crate) fn packet_socket_set_option(source: &RustPacketSocket, option: i32, value: i32) -> i32 {
    source.call(-1, |product| {
        product.set_option(
            PacketSocketOption::from_native_value(option)
                .expect("native Socket::Option representation"),
            value,
        )
    })
}
pub(crate) fn packet_socket_error(source: &RustPacketSocket) -> i32 {
    source.call(ffi::socket_provider_io_error(), |product| product.error())
}
pub(crate) fn packet_socket_set_error(source: &RustPacketSocket, error: i32) {
    source.call((), |product| product.set_error(error));
}
