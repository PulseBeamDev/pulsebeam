//! Owning listeners and linear accepted-product handoff on actual SDK lists.
use crate::{
    ControlledPeerDriver, Environment, ListenSocket, ListenSocketContext, ListenSocketState,
    OwnedPacketSocket, PacketSocketContext, PacketSocketEventError, ffi,
};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

/// Owns the actual native listener and its selected controlled/production role. Accepted sockets
/// have independent native scopes and keep their own role/environment alive.
/// Dropping the listener cancels unclaimed products, not adopted sockets.
pub struct OwnedListenSocket {
    native: Option<cxx::UniquePtr<ffi::NativeOwnedListenSocket>>,
    context: ListenSocketContext,
    sequence: PhantomData<Rc<()>>,
}
impl Drop for OwnedListenSocket {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl std::ops::Deref for OwnedListenSocket {
    type Target = ListenSocketContext;
    fn deref(&self) -> &Self::Target {
        &self.context
    }
}
impl OwnedListenSocket {
    pub(crate) fn create_controlled(
        factory: crate::PacketSocketFactory,
        driver: &ControlledPeerDriver,
        environment: &Environment,
        creator: impl FnOnce(ListenSocketContext) -> Option<Box<dyn ListenSocket>>,
    ) -> Result<Option<Self>, PacketSocketEventError> {
        if !driver.is_current() {
            let _ = crate::provider_panic::catch_provider(|| drop(creator));
            return Err(PacketSocketEventError::WrongSequence);
        }
        if !environment.uses_controlled_clock(driver.clock()) {
            let _ = crate::provider_panic::catch_provider(|| drop(creator));
            return Err(PacketSocketEventError::IncompatibleExecution);
        }
        Self::create(
            factory,
            crate::owned_packet_socket::RustSocketRole {
                _environment: environment.clone(),
                owner: crate::owned_packet_socket::SocketRoleOwner::Controlled(driver.clone()),
            },
            creator,
        )
    }
    pub(crate) fn create_production(
        factory: crate::PacketSocketFactory,
        role: &crate::PacketSocketRole,
        creator: impl FnOnce(ListenSocketContext) -> Option<Box<dyn ListenSocket>>,
    ) -> Result<Option<Self>, PacketSocketEventError> {
        if let Err(error) = role.check_current() {
            let _ = crate::provider_panic::catch_provider(|| drop(creator));
            return Err(error);
        }
        Self::create(
            factory,
            crate::owned_packet_socket::RustSocketRole {
                _environment: role.environment().clone(),
                owner: crate::owned_packet_socket::SocketRoleOwner::Production(
                    role.runtime.clone(),
                ),
            },
            creator,
        )
    }
    fn create(
        factory: crate::PacketSocketFactory,
        role: crate::owned_packet_socket::RustSocketRole,
        creator: impl FnOnce(ListenSocketContext) -> Option<Box<dyn ListenSocket>>,
    ) -> Result<Option<Self>, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let environment = &role._environment;
        struct Creator<F>(Option<F>);
        impl<F> Drop for Creator<F> {
            fn drop(&mut self) {
                let value = self.0.take();
                let _ = crate::provider_panic::catch_provider(|| drop(value));
            }
        }
        let mut creator = Creator(Some(creator));
        let scope = ffi::packet_socket_scope_new();
        let Some(scope_ref) = scope.as_ref() else {
            return Err(PacketSocketEventError::WrongSequence);
        };
        let resources = ffi::packet_socket_scope_resources(scope_ref);
        let events = ffi::listen_socket_events_in_scope(resources.clone(), true);
        if events.is_null() {
            return Err(PacketSocketEventError::Retired);
        }
        let context = ListenSocketContext::new(
            environment.clone(),
            resources,
            events.clone(),
            factory.failure_latch(),
        )
        .with_factory(Some(factory))
        .with_role(&role);
        let product = crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            creator.0.take().unwrap()(context.clone())
        }))
        .map_err(|_| PacketSocketEventError::ProviderFailed)?;
        let Some(product) = product else {
            return Ok(None);
        };
        let source =
            crate::listen_socket::RustListenSocket::from_creation(Some(product), context.clone());
        let native = ffi::owned_listen_socket_new(source, events, scope, Box::new(role));
        if native.is_null() {
            return Err(PacketSocketEventError::Retired);
        }
        Ok(Some(Self {
            native: Some(native),
            context,
            sequence: PhantomData,
        }))
    }
    pub fn context(&self) -> &ListenSocketContext {
        &self.context
    }
    /// The single ownership-taking receiver. The SDK callback receives and
    /// transfers an actual product once, never a second owner of the raw pointer.
    /// Install before publication. No hidden dispatch or first-turn fence is
    /// needed for standalone listeners with an explicitly installed receiver.
    pub fn register_accept(
        &self,
        callback: impl Fn(OwnedPacketSocket) + 'static,
    ) -> Result<ListenSocketSubscription, PacketSocketEventError> {
        self.observe(true, ListenCallback::Owner(Box::new(callback)))
    }
    /// Observe a borrowed accepted facade without owning it. The context may be
    /// retained, but cannot revive native access after the socket is retired.
    /// Observer order follows the actual SDK list, including before adoption.
    pub fn observe_accept(
        &self,
        callback: impl Fn(PacketSocketContext) + 'static,
    ) -> Result<ListenSocketSubscription, PacketSocketEventError> {
        self.observe(false, ListenCallback::Observer(Box::new(callback)))
    }
    fn observe(
        &self,
        ownership: bool,
        callback: ListenCallback,
    ) -> Result<ListenSocketSubscription, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let failed = Rc::new(Cell::new(false));
        let result = ffi::listen_observe(
            self.context.native(),
            ownership,
            Box::new(RustListenCallback {
                callback: Some(callback),
                failed: failed.clone(),
            }),
        );
        crate::packet_socket::event_result(result.status)?;
        Ok(ListenSocketSubscription {
            native: Some(result.subscription),
            failed,
            sequence: PhantomData,
        })
    }
}
impl ListenSocketContext {
    /// Query the actual native virtual state, not a creation snapshot/default.
    pub fn state(&self) -> Result<ListenSocketState, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let result = ffi::listen_member_call(self.native(), false);
        crate::packet_socket::event_result(result.status)?;
        Ok(ListenSocketState(result.code))
    }
    pub fn local_address(&self) -> Result<crate::SocketAddress, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let result = ffi::listen_member_call(self.native(), true);
        crate::packet_socket::event_result(result.status)?;
        Ok(crate::SocketAddress::from_native(result.address))
    }
}
/// Actual tagged native registration ownership. Logical cancellation is
/// immediate; capture Drop waits for native Send/tombstone cleanup to return.
pub struct ListenSocketSubscription {
    native: Option<cxx::UniquePtr<ffi::NativeListenSocketSubscription>>,
    failed: Rc<Cell<bool>>,
    sequence: PhantomData<Rc<()>>,
}
impl Drop for ListenSocketSubscription {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl ListenSocketSubscription {
    pub fn cancel(&self) -> bool {
        let _turn = crate::execution::controlled_owner_turn();
        ffi::listen_subscription_cancel(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn is_active(&self) -> bool {
        ffi::listen_subscription_active(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn has_failed(&self) -> bool {
        self.failed.get()
    }
}
enum ListenCallback {
    Owner(Box<dyn Fn(OwnedPacketSocket)>),
    Observer(Box<dyn Fn(PacketSocketContext)>),
}
pub(crate) struct RustListenCallback {
    callback: Option<ListenCallback>,
    failed: Rc<Cell<bool>>,
}
impl Drop for RustListenCallback {
    fn drop(&mut self) {
        let capture = self.callback.take();
        if crate::provider_panic::catch_provider(|| drop(capture)).is_err() {
            self.failed.set(true);
        }
    }
}
pub(crate) fn listen_callback_fire(
    callback: &RustListenCallback,
    source: &crate::packet_socket::RustPacketSocket,
    owned: cxx::UniquePtr<ffi::NativeOwnedPacketSocket>,
) -> bool {
    let _turn = crate::execution::controlled_owner_turn();
    if callback.failed.get() {
        return false;
    }
    if crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
        match callback.callback.as_ref().unwrap() {
            ListenCallback::Owner(f) => f(OwnedPacketSocket::from_adoption(owned, source)),
            ListenCallback::Observer(f) => f(source.context()),
        }
    }))
    .is_err()
    {
        callback.failed.set(true);
    }
    !callback.failed.get()
}
