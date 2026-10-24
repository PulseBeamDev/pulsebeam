//! Role-local SDK listening products and move-only accepted socket ownership.
use crate::{
    Environment, PacketSocket, PacketSocketContext, PacketSocketEventError, SocketAddress, ffi,
    packet_socket::event_result,
};
use std::{
    fmt,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Open scoped AsyncListenSocket::State values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListenSocketState(pub i32);
impl ListenSocketState {
    pub const CLOSED: Self = Self(0);
    pub const BOUND: Self = Self(1);
}
pub trait ListenSocket: 'static {
    fn state(&self) -> ListenSocketState;
    fn local_address(&self) -> SocketAddress;
}

/// A not-yet-transferred socket. Drop releases the actual native product on its
/// creation sequence; successful submission moves it into listener-owned pending
/// storage, then into the SDK accepting consumer. It cannot be cloned or sent.
pub struct AcceptedPacketSocket {
    native: Option<cxx::UniquePtr<ffi::NativeAcceptedPacketSocket>>,
    sequence: PhantomData<Rc<()>>,
}
impl AcceptedPacketSocket {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeAcceptedPacketSocket>) -> Self {
        Self {
            native: Some(native),
            sequence: PhantomData,
        }
    }
}
impl Drop for AcceptedPacketSocket {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
/// Rejected submission returns the same owned socket, allowing retry or drop.
pub struct PacketSocketAcceptError {
    pub error: PacketSocketEventError,
    pub socket: AcceptedPacketSocket,
}
impl fmt::Debug for PacketSocketAcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketSocketAcceptError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for PacketSocketAcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}
impl std::error::Error for PacketSocketAcceptError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSocketAcceptStatus {
    Pending,
    Delivering,
    Delivered,
    Cancelled,
    Retired,
    ProviderFailed,
    NativeConsumerUnavailable,
}
struct AcceptInner(
    cxx::SharedPtr<ffi::NativePacketSocketAccept>,
    Option<Arc<AtomicBool>>,
);
// This alias owns atomic completion metadata only, never a product or role loan.
unsafe impl Send for AcceptInner {}
unsafe impl Sync for AcceptInner {}
/// Transfer status for queued or synchronous publication. Cancellation wins
/// only before delivery starts. It does not run SDK work; native progress or
/// scope retirement releases the pending socket on its role. Dropping this observation does not cancel it.
#[derive(Clone)]
pub struct PacketSocketAcceptTicket(Arc<AcceptInner>);
impl PacketSocketAcceptTicket {
    pub fn status(&self) -> PacketSocketAcceptStatus {
        match ffi::listen_accept_status(self.0.0.as_ref().unwrap()) {
            0 => PacketSocketAcceptStatus::Pending,
            1 => PacketSocketAcceptStatus::Delivering,
            2 => PacketSocketAcceptStatus::Delivered,
            3 => PacketSocketAcceptStatus::Cancelled,
            4 => PacketSocketAcceptStatus::Retired,
            6 => PacketSocketAcceptStatus::NativeConsumerUnavailable,
            _ => PacketSocketAcceptStatus::ProviderFailed,
        }
    }
    pub fn cancel(&self) -> bool {
        ffi::listen_accept_cancel(self.0.0.as_ref().unwrap())
    }
    /// Creation/provider failure or asynchronous creator-capture Drop panic.
    /// A capture failure does not overwrite an already Cancelled/Retired status.
    pub fn has_failed(&self) -> bool {
        self.status() == PacketSocketAcceptStatus::ProviderFailed
            || self
                .0
                .1
                .as_ref()
                .is_some_and(|failed| failed.load(Ordering::Acquire))
    }
}

struct ListenEventsInner(cxx::SharedPtr<ffi::NativeListenSocketEvents>);
// Synchronized post/retire metadata. Scope retirement clears role-local source
// and callback captures before a foreign alias can be the last native owner.
unsafe impl Send for ListenEventsInner {}
unsafe impl Sync for ListenEventsInner {}
/// Cross-thread asynchronous accepted-product creation and transfer. A creator
/// runs on the actual listener role, never on the submitting thread. Admission
/// is not creation/delivery success; cancellation can race a running creator.
#[derive(Clone)]
pub struct ListenSocketEvents(Arc<ListenEventsInner>);
impl ListenSocketEvents {
    pub fn post_accept(
        &self,
        creator: impl FnOnce(PacketSocketContext) -> Box<dyn PacketSocket> + Send + 'static,
    ) -> PacketSocketAcceptTicket {
        let failed = Arc::new(AtomicBool::new(false));
        let ticket = ffi::listen_post_creator(
            self.0.0.as_ref().unwrap(),
            Box::new(RustAcceptCreator(Some(Box::new(creator)), failed.clone())),
        );
        PacketSocketAcceptTicket(Arc::new(AcceptInner(ticket, Some(failed))))
    }
}
type AcceptCreator = Box<dyn FnOnce(PacketSocketContext) -> Box<dyn PacketSocket> + Send>;

pub(crate) struct RustAcceptCreator(Option<AcceptCreator>, Arc<AtomicBool>);
impl Drop for RustAcceptCreator {
    fn drop(&mut self) {
        let capture = self.0.take();
        if crate::provider_panic::catch_provider(|| drop(capture)).is_err() {
            self.1.store(true, Ordering::Release);
        }
    }
}
#[allow(
    clippy::boxed_local,
    reason = "CXX transfers ownership through this fixed Box ABI."
)]
pub(crate) fn listen_create_accepted(
    source: &RustListenSocket,
    mut creator: Box<RustAcceptCreator>,
    ticket: cxx::SharedPtr<ffi::NativePacketSocketAccept>,
) -> bool {
    let _turn = crate::execution::controlled_owner_turn();
    if ffi::listen_accept_status(ticket.as_ref().unwrap()) != 0 {
        return false;
    }
    let created = source
        .context
        .create_accepted(|context| creator.0.take().unwrap()(context));
    let mut socket = match created {
        Ok(value) => value,
        Err(error) => {
            ffi::listen_accept_creation_failed(
                ticket.as_ref().unwrap(),
                error == PacketSocketEventError::Retired,
            );
            return false;
        }
    };
    let result = ffi::listen_accept_prepared(
        source.context.native(),
        socket.native.take().unwrap(),
        ticket.clone(),
    );
    if result.status != 0 {
        // Rejected actual products still retire on this role. Keep the ticket's
        // existing Cancelled state if cancellation won during creation/admission.
        drop(AcceptedPacketSocket::from_native(result.socket));
        ffi::listen_accept_creation_failed(ticket.as_ref().unwrap(), result.status == 1);
        return false;
    }
    true
}

#[derive(Clone)]
pub struct ListenSocketContext {
    environment: Environment,
    resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
    native: cxx::SharedPtr<ffi::NativeListenSocketEvents>,
    failed: Arc<AtomicBool>,
    aggregate: Arc<AtomicBool>,
    factory: Option<crate::PacketSocketFactory>,
    controlled: Option<crate::execution::WeakControlledPeerDriver>,
    production: Option<crate::packet_socket_runtime::WeakPacketSocketRuntime>,
    sequence: PhantomData<Rc<()>>,
}
impl ListenSocketContext {
    pub(crate) fn new(
        environment: Environment,
        resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
        native: cxx::SharedPtr<ffi::NativeListenSocketEvents>,
        aggregate: Arc<AtomicBool>,
    ) -> Self {
        assert!(!resources.is_null() && !native.is_null());
        Self {
            environment,
            resources,
            native,
            failed: Arc::new(AtomicBool::new(false)),
            aggregate,
            factory: None,
            controlled: None,
            production: None,
            sequence: PhantomData,
        }
    }
    pub(crate) fn with_factory(mut self, factory: Option<crate::PacketSocketFactory>) -> Self {
        self.factory = factory;
        self
    }
    pub(crate) fn with_role(mut self, role: &crate::owned_packet_socket::RustSocketRole) -> Self {
        match &role.owner {
            crate::owned_packet_socket::SocketRoleOwner::Controlled(driver) => {
                self.controlled = Some(driver.downgrade())
            }
            crate::owned_packet_socket::SocketRoleOwner::Production(runtime) => {
                self.production = Some(runtime.downgrade())
            }
        }
        self
    }
    pub(crate) fn native(&self) -> &ffi::NativeListenSocketEvents {
        self.native.as_ref().unwrap()
    }
    pub fn environment(&self) -> &Environment {
        &self.environment
    }
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    pub fn events(&self) -> ListenSocketEvents {
        ListenSocketEvents(Arc::new(ListenEventsInner(self.native.clone())))
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.aggregate.store(true, Ordering::Release);
    }
    /// Construct the actual accepted native product on the listener's network
    /// role with the same owning SDK environment. Standalone children have
    /// independent retirement scopes; transfer is bound to this listener scope.
    /// No source or native provider is substituted on rejection/panic.
    /// Private ICE TCP contexts return NativeConsumerUnavailable without invoking
    /// the creator: this SDK pin exposes no safe network-membership admission.
    pub fn create_accepted(
        &self,
        creator: impl FnOnce(PacketSocketContext) -> Box<dyn PacketSocket>,
    ) -> Result<AcceptedPacketSocket, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        struct CreatorGuard<'a, F>(Option<F>, &'a ListenSocketContext);
        impl<F> Drop for CreatorGuard<'_, F> {
            fn drop(&mut self) {
                let capture = self.0.take();
                if crate::provider_panic::catch_provider(|| drop(capture)).is_err() {
                    self.1.fail();
                }
            }
        }
        let mut creator = CreatorGuard(Some(creator), self);
        event_result(ffi::packet_socket_resources_status(
            self.resources.as_ref().unwrap(),
        ))?;
        if self.has_failed() {
            return Err(PacketSocketEventError::ProviderFailed);
        }
        if let Some(weak) = &self.production {
            let role = weak.upgrade().ok_or(PacketSocketEventError::Retired)?;
            let result = crate::OwnedPacketSocket::create_production(
                self.factory
                    .as_ref()
                    .expect("standalone listener recipe")
                    .clone(),
                &role,
                |context| Some(creator.0.take().unwrap()(context)),
            );
            let owned = match result {
                Ok(Some(value)) => value,
                Ok(None) => unreachable!("accepted creator returns a product"),
                Err(error) => {
                    if error == PacketSocketEventError::ProviderFailed {
                        self.fail();
                    }
                    return Err(error);
                }
            };
            return owned.into_accepted(self.resources.clone());
        }
        if let Some(weak) = &self.controlled {
            let driver = weak.upgrade().ok_or(PacketSocketEventError::Retired)?;
            let factory = self
                .factory
                .as_ref()
                .expect("standalone listener recipe")
                .clone();
            let result = crate::OwnedPacketSocket::create_controlled(
                factory,
                &driver,
                &self.environment,
                |context| Some(creator.0.take().unwrap()(context)),
            );
            let owned = match result {
                Ok(Some(value)) => value,
                Ok(None) => unreachable!("accepted creator returns a product"),
                Err(error) => {
                    if error == PacketSocketEventError::ProviderFailed {
                        self.fail();
                    }
                    return Err(error);
                }
            };
            return owned.into_accepted(self.resources.clone());
        }
        // Only standalone ownership transfer has a complete consumer boundary.
        // The pinned ICE TCPPort does not expose its actual network membership
        // before the later TCPConnection getter DCHECK. Reject without calling
        // the creator; do not guess a bind address or freeze live getter values.
        Err(PacketSocketEventError::NativeConsumerUnavailable)
    }
    /// Queue a single ownership transfer. It never calls accepting consumers on
    /// a provider/drop stack. Retiring the listener cancels unclaimed transfers;
    /// sockets already delivered remain owned by their accepting SDK consumer.
    pub fn post_accept(
        &self,
        socket: AcceptedPacketSocket,
    ) -> Result<PacketSocketAcceptTicket, PacketSocketAcceptError> {
        self.submit_accept(socket, true)
    }
    /// Publish through the actual SDK listener on this stack. This SDK-managed
    /// facade becomes ready for inline publication only after its first queued
    /// network-role turn, when TCPPort's ownership-taking receiver is installed.
    /// Earlier calls return NotReady with the same socket; they are not queued.
    /// Recursive publication returns InProgress with the socket. Ok means
    /// accepted ownership, not successful transport: inspect the terminal ticket
    /// status, since cancellation pruning may retire/fail the listener mid-call.
    /// Standalone owned listeners instead require an explicit ownership-taking
    /// registration before publication, without the SDK-managed first-turn fence.
    pub fn notify_accept(
        &self,
        socket: AcceptedPacketSocket,
    ) -> Result<PacketSocketAcceptTicket, PacketSocketAcceptError> {
        self.submit_accept(socket, false)
    }
    fn submit_accept(
        &self,
        mut socket: AcceptedPacketSocket,
        post: bool,
    ) -> Result<PacketSocketAcceptTicket, PacketSocketAcceptError> {
        let _turn = crate::execution::controlled_owner_turn();
        let result = ffi::listen_accept(
            self.native.as_ref().unwrap(),
            socket.native.take().unwrap(),
            post,
        );
        if let Err(error) = event_result(result.status) {
            return Err(PacketSocketAcceptError {
                error,
                socket: AcceptedPacketSocket {
                    native: Some(result.socket),
                    sequence: PhantomData,
                },
            });
        }
        Ok(PacketSocketAcceptTicket(Arc::new(AcceptInner(
            result.ticket,
            None,
        ))))
    }
}

pub(crate) struct RustListenSocket {
    product: Option<Box<dyn ListenSocket>>,
    context: ListenSocketContext,
}
impl RustListenSocket {
    pub(crate) fn from_creation(
        product: Option<Box<dyn ListenSocket>>,
        context: ListenSocketContext,
    ) -> Box<Self> {
        Box::new(Self { product, context })
    }
    fn call<T>(&self, failure: T, operation: impl FnOnce(&dyn ListenSocket) -> T) -> T {
        if self.context.has_failed() {
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
                self.context.fail();
                failure
            }
        }
    }
}
impl Drop for RustListenSocket {
    fn drop(&mut self) {
        let product = self.product.take();
        if crate::provider_panic::catch_provider(|| drop(product)).is_err() {
            self.context.fail();
        }
    }
}
pub(crate) fn listen_socket_present(source: &RustListenSocket) -> bool {
    source.product.is_some()
}
pub(crate) fn listen_socket_failed(source: &RustListenSocket) -> bool {
    source.context.has_failed()
}
pub(crate) fn listen_socket_state(source: &RustListenSocket) -> i32 {
    source
        .call(ListenSocketState::CLOSED, |product| product.state())
        .0
}
pub(crate) fn listen_socket_local(
    source: &RustListenSocket,
) -> cxx::UniquePtr<ffi::NativeSocketAddress> {
    let address = source.call(SocketAddress::default(), |product| product.local_address());
    ffi::socket_address_clone(address.native())
}
