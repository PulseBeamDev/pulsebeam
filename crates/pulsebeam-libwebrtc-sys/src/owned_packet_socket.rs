//! Sequence-local owning native packet socket products.
use crate::{
    AsyncSocketPacketOptions, ControlledPeerDriver, Environment, PacketSocket, PacketSocketContext,
    PacketSocketEvent, PacketSocketEventError, PacketSocketOption, PacketSocketOptionError,
    PacketSocketState, PacketSocketSubscription, ReceivedIpPacket, SocketAddress, ffi,
};
use std::{marker::PhantomData, num::NonZeroI32, rc::Rc};

/// Owns an actual AsyncPacketSocket, its private retirement scope and the actual
/// selected controlled driver or production runtime. This is not a transport
/// implementation or a metadata view.
/// It cannot migrate between OS threads. Callback-time Drop retires publication
/// immediately and keeps the native facade alive through its last active frame.
/// Production construction runs on an explicit owning PacketSocketRole.
pub struct OwnedPacketSocket {
    native: Option<cxx::UniquePtr<ffi::NativeOwnedPacketSocket>>,
    context: PacketSocketContext,
    sequence: PhantomData<Rc<()>>,
}
/// Retained inside the native product core, including active callback frames.
/// Neither a borrowed NativeThread wrapper nor a resource-name join is an owner.
pub(crate) struct RustSocketRole {
    pub(crate) _environment: Environment,
    pub(crate) owner: SocketRoleOwner,
}
pub(crate) enum SocketRoleOwner {
    Controlled(ControlledPeerDriver),
    Production(crate::PacketSocketRuntime),
}
impl Drop for OwnedPacketSocket {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl OwnedPacketSocket {
    pub(crate) fn create_controlled(
        factory: crate::PacketSocketFactory,
        driver: &ControlledPeerDriver,
        environment: &Environment,
        creator: impl FnOnce(PacketSocketContext) -> Option<Box<dyn PacketSocket>>,
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
            RustSocketRole {
                _environment: environment.clone(),
                owner: SocketRoleOwner::Controlled(driver.clone()),
            },
            creator,
        )
    }
    pub(crate) fn create_production(
        factory: crate::PacketSocketFactory,
        role: &crate::PacketSocketRole,
        creator: impl FnOnce(PacketSocketContext) -> Option<Box<dyn PacketSocket>>,
    ) -> Result<Option<Self>, PacketSocketEventError> {
        if let Err(error) = role.check_current() {
            let _ = crate::provider_panic::catch_provider(|| drop(creator));
            return Err(error);
        }
        Self::create(
            factory,
            RustSocketRole {
                _environment: role.environment().clone(),
                owner: SocketRoleOwner::Production(role.runtime.clone()),
            },
            creator,
        )
    }
    fn create(
        factory: crate::PacketSocketFactory,
        role: RustSocketRole,
        creator: impl FnOnce(PacketSocketContext) -> Option<Box<dyn PacketSocket>>,
    ) -> Result<Option<Self>, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let environment = &role._environment;
        struct Creator<F>(Option<F>);
        impl<F> Drop for Creator<F> {
            fn drop(&mut self) {
                let capture = self.0.take();
                let _ = crate::provider_panic::catch_provider(|| drop(capture));
            }
        }
        let mut creator = Creator(Some(creator));
        let scope = ffi::packet_socket_scope_new();
        let Some(scope_ref) = scope.as_ref() else {
            return Err(PacketSocketEventError::WrongSequence);
        };
        let resources = ffi::packet_socket_scope_resources(scope_ref);
        let events = ffi::packet_socket_events_in_scope(resources.clone());
        if events.is_null() {
            return Err(PacketSocketEventError::Retired);
        }
        let context = PacketSocketContext::new(environment.clone(), resources, events.clone())
            .with_factory(Some(factory.clone()));
        let product = crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            creator.0.take().unwrap()(context.clone())
        }))
        .map_err(|_| PacketSocketEventError::ProviderFailed)?;
        let Some(product) = product else {
            return Ok(None);
        };
        let source = crate::packet_socket::RustPacketSocket::new(
            product,
            context.clone(),
            factory.failure_latch(),
        );
        let native = ffi::owned_packet_socket_new(source, events, scope, Box::new(role));
        if native.is_null() {
            return Err(PacketSocketEventError::Retired);
        }
        Ok(Some(Self {
            native: Some(native),
            context,
            sequence: PhantomData,
        }))
    }
    pub(crate) fn from_adoption(
        native: cxx::UniquePtr<ffi::NativeOwnedPacketSocket>,
        source: &crate::packet_socket::RustPacketSocket,
    ) -> Self {
        Self {
            native: Some(native),
            context: source.context(),
            sequence: PhantomData,
        }
    }
    pub(crate) fn into_accepted(
        mut self,
        resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
    ) -> Result<crate::AcceptedPacketSocket, PacketSocketEventError> {
        let native = ffi::owned_socket_into_accepted(self.native.take().unwrap(), resources);
        if native.is_null() {
            return Err(PacketSocketEventError::Retired);
        }
        Ok(crate::listen_socket::AcceptedPacketSocket::from_native(
            native,
        ))
    }
    pub fn context(&self) -> &PacketSocketContext {
        &self.context
    }
    pub fn has_failed(&self) -> bool {
        self.context.has_failed()
    }
    /// Own the SDK's one received-packet callback, not a passive observer.
    /// A second live receiver returns ReceiverAlreadyRegistered. Cancellation
    /// inside receive disables delivery now and deregisters only after invocation
    /// returns, before capture Drop. SDK-managed peer sockets cannot use this API.
    pub fn register_received(
        &self,
        callback: impl Fn(ReceivedIpPacket) + 'static,
    ) -> Result<PacketSocketSubscription, PacketSocketEventError> {
        self.context.subscribe_received(move |event| {
            if let PacketSocketEvent::Received(packet) = event {
                callback(packet);
            }
        })
    }
}
impl std::ops::Deref for OwnedPacketSocket {
    type Target = PacketSocketContext;
    fn deref(&self) -> &Self::Target {
        &self.context
    }
}
// These methods borrow the actual facade through its creation context. A
// retained context does not own the facade: retirement returns Retired, never a
// default address/state reported as a successful native query.
impl PacketSocketContext {
    fn call(
        &self,
        operation: u8,
        payload: &[u8],
        address: Option<&SocketAddress>,
        options: &AsyncSocketPacketOptions,
        option: i32,
        value: i32,
    ) -> Result<ffi::FfiOwnedSocketResult, PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let result = ffi::socket_member_call(
            self.observation_native(),
            operation,
            payload,
            address.map_or_else(cxx::UniquePtr::null, |a| {
                ffi::socket_address_clone(a.native())
            }),
            options.native(),
            option,
            value,
        );
        crate::packet_socket::event_result(result.status)?;
        Ok(result)
    }
    pub fn local_address(&self) -> Result<SocketAddress, PacketSocketEventError> {
        Ok(SocketAddress::from_native(
            self.call(0, &[], None, &Default::default(), 0, 0)?.address,
        ))
    }
    pub fn remote_address(&self) -> Result<SocketAddress, PacketSocketEventError> {
        Ok(SocketAddress::from_native(
            self.call(1, &[], None, &Default::default(), 0, 0)?.address,
        ))
    }
    pub fn state(&self) -> Result<PacketSocketState, PacketSocketEventError> {
        Ok(PacketSocketState::from_native_value(
            self.call(2, &[], None, &Default::default(), 0, 0)?.code,
        )
        .expect("native state representation"))
    }
    pub fn send(
        &self,
        payload: &[u8],
        options: &AsyncSocketPacketOptions,
    ) -> Result<i32, PacketSocketEventError> {
        Ok(self.call(3, payload, None, options, 0, 0)?.code)
    }
    pub fn send_to(
        &self,
        payload: &[u8],
        address: &SocketAddress,
        options: &AsyncSocketPacketOptions,
    ) -> Result<i32, PacketSocketEventError> {
        Ok(self.call(4, payload, Some(address), options, 0, 0)?.code)
    }
    pub fn close(&self) -> Result<i32, PacketSocketEventError> {
        Ok(self.call(5, &[], None, &Default::default(), 0, 0)?.code)
    }
    pub fn get_option(
        &self,
        option: PacketSocketOption,
    ) -> Result<Result<i32, PacketSocketOptionError>, PacketSocketEventError> {
        let result = self.call(6, &[], None, &Default::default(), option.native_value(), 0)?;
        Ok(match NonZeroI32::new(result.code) {
            None => Ok(result.value),
            Some(code) => Err(PacketSocketOptionError(code)),
        })
    }
    pub fn set_option(
        &self,
        option: PacketSocketOption,
        value: i32,
    ) -> Result<i32, PacketSocketEventError> {
        Ok(self
            .call(
                7,
                &[],
                None,
                &Default::default(),
                option.native_value(),
                value,
            )?
            .code)
    }
    pub fn error(&self) -> Result<i32, PacketSocketEventError> {
        Ok(self.call(8, &[], None, &Default::default(), 0, 0)?.code)
    }
    pub fn set_error(&self, error: i32) -> Result<(), PacketSocketEventError> {
        self.call(9, &[], None, &Default::default(), 0, error)?;
        Ok(())
    }
}
