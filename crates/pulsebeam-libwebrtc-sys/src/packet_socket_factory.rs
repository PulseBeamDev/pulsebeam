//! Owning general SDK packet socket factory selection and creation dispatch.
use crate::{
    AsyncDnsResolver, DnsResolutionCompletion, DnsResolverResult, Environment, ListenSocket,
    ListenSocketContext, PacketSocket, PacketSocketContext, PacketSocketFlags, SocketAddress,
    SocketTlsVerifier, ffi, listen_socket::RustListenSocket, packet_socket::RustPacketSocket,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Exact owning TCP/DTLS creation options. Native std::string values are bytes,
/// including embedded NUL/non-UTF-8. No protocol/curve normalization is applied.
/// The optional verifier owns the actual selected peer verifier backing, not a
/// factory-scoped raw loan; retained aliases can outlive the peer and socket.
#[derive(Clone, Default)]
pub struct PacketSocketTcpOptions {
    pub options: PacketSocketFlags,
    pub tls_alpn_protocols: Vec<Vec<u8>>,
    pub tls_elliptic_curves: Vec<Vec<u8>>,
    pub tls_cert_verifier: Option<SocketTlsVerifier>,
}
impl PacketSocketTcpOptions {
    fn from_native(value: ffi::FfiSocketTcpOptions) -> Self {
        Self {
            options: PacketSocketFlags(value.options),
            tls_alpn_protocols: value.alpn.into_iter().map(|item| item.bytes).collect(),
            tls_elliptic_curves: value.curves.into_iter().map(|item| item.bytes).collect(),
            tls_cert_verifier: SocketTlsVerifier::from_native(value.verifier),
        }
    }
}

/// Factory recipes are shareable; products are created, used and destroyed on
/// the SDK network role. None rejects construction, never selects stock sockets.
/// Native algorithms and socket I/O remain the product/SDK's responsibility.
/// Reentrant creation must not hold an exclusive user-state loan. All methods
/// are explicit, including native's default-but-unimplemented client UDP member.
pub trait PacketSocketFactoryImplementation: Send + Sync + 'static {
    fn create_udp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
    ) -> Option<Box<dyn PacketSocket>>;
    fn create_server_tcp_socket(
        &self,
        context: ListenSocketContext,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>>;
    fn create_client_tcp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        remote: SocketAddress,
        options: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>>;
    fn create_client_udp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        remote: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>>;
    /// This legacy SDK member has no environment argument. Receive the owning
    /// installed factory environment. SDK consumers require a non-null product;
    /// operational DNS failure belongs to its result, not null construction.
    fn create_async_dns_resolver(&self, environment: Environment) -> Box<dyn AsyncDnsResolver>;
    /// Controlled implementations must use only caller-owned progress and I/O.
    fn is_controlled(&self) -> bool {
        false
    }
}
impl<T: PacketSocketFactoryImplementation + ?Sized> PacketSocketFactoryImplementation for Arc<T> {
    fn create_udp_socket(
        &self,
        c: PacketSocketContext,
        l: SocketAddress,
        min: u16,
        max: u16,
    ) -> Option<Box<dyn PacketSocket>> {
        (**self).create_udp_socket(c, l, min, max)
    }
    fn create_server_tcp_socket(
        &self,
        c: ListenSocketContext,
        l: SocketAddress,
        min: u16,
        max: u16,
        o: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>> {
        (**self).create_server_tcp_socket(c, l, min, max, o)
    }
    fn create_client_tcp_socket(
        &self,
        c: PacketSocketContext,
        l: SocketAddress,
        r: SocketAddress,
        o: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        (**self).create_client_tcp_socket(c, l, r, o)
    }
    fn create_client_udp_socket(
        &self,
        c: PacketSocketContext,
        l: SocketAddress,
        r: SocketAddress,
        min: u16,
        max: u16,
        o: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        (**self).create_client_udp_socket(c, l, r, min, max, o)
    }
    fn create_async_dns_resolver(&self, e: Environment) -> Box<dyn AsyncDnsResolver> {
        (**self).create_async_dns_resolver(e)
    }
    fn is_controlled(&self) -> bool {
        (**self).is_controlled()
    }
}
struct Registration {
    provider: Option<Arc<dyn PacketSocketFactoryImplementation>>,
    failed: Arc<AtomicBool>,
    disabled: AtomicBool,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let value = self.provider.take();
        if crate::provider_panic::catch_provider(|| drop(value)).is_err() {
            self.failed.store(true, Ordering::Release);
        }
    }
}
/// Reusable selection. Clones retain the same recipe and failure diagnostics;
/// each installed native factory owns a separate role-local resource scope.
#[derive(Clone)]
pub struct PacketSocketFactory(Arc<Registration>);
impl PacketSocketFactory {
    pub fn from_provider(provider: impl PacketSocketFactoryImplementation) -> Self {
        Self(Arc::new(Registration {
            provider: Some(Arc::new(provider)),
            failed: Arc::new(AtomicBool::new(false)),
            disabled: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    pub(crate) fn failure_latch(&self) -> Arc<AtomicBool> {
        self.0.failed.clone()
    }
    fn standalone_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
        create: impl FnOnce(PacketSocketContext) -> Option<Box<dyn PacketSocket>>,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        if !self.is_controlled() {
            return Err(if self.creation_failed() {
                crate::PacketSocketEventError::ProviderFailed
            } else {
                crate::PacketSocketEventError::IncompatibleExecution
            });
        }
        let result =
            crate::OwnedPacketSocket::create_controlled(self.clone(), driver, environment, create);
        if self.creation_failed() {
            return Err(crate::PacketSocketEventError::ProviderFailed);
        }
        result
    }
    /// Construct an owning standalone facade on a caller-owned controlled role.
    /// The exact environment/recipe/driver remain owned through final native Drop.
    /// None is provider rejection, not selection of a stock transport. These
    /// standalone products are independent of peer installations of this recipe.
    pub fn create_server_tcp_socket_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketFlags,
    ) -> Result<Option<crate::OwnedListenSocket>, crate::PacketSocketEventError> {
        if !self.is_controlled() {
            return Err(if self.creation_failed() {
                crate::PacketSocketEventError::ProviderFailed
            } else {
                crate::PacketSocketEventError::IncompatibleExecution
            });
        }
        let result = crate::OwnedListenSocket::create_controlled(
            self.clone(),
            driver,
            environment,
            |context| {
                self.call(|p| {
                    p.create_server_tcp_socket(context, local, min_port, max_port, options)
                })
                .flatten()
            },
        );
        if self.creation_failed() {
            return Err(crate::PacketSocketEventError::ProviderFailed);
        }
        result
    }
    pub fn create_udp_socket_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone_controlled(driver, environment, |context| {
            self.call(|p| p.create_udp_socket(context, local, min_port, max_port))
                .flatten()
        })
    }
    pub fn create_client_tcp_socket_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
        local: SocketAddress,
        remote: SocketAddress,
        options: PacketSocketTcpOptions,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone_controlled(driver, environment, |context| {
            self.call(|p| p.create_client_tcp_socket(context, local, remote, options))
                .flatten()
        })
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Preserve the existing native boundary parameter contract."
    )]
    pub fn create_client_udp_socket_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
        local: SocketAddress,
        remote: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketTcpOptions,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone_controlled(driver, environment, |context| {
            self.call(|p| {
                p.create_client_udp_socket(context, local, remote, min_port, max_port, options)
            })
            .flatten()
        })
    }
    fn standalone(
        &self,
        role: &crate::PacketSocketRole,
        create: impl FnOnce(PacketSocketContext) -> Option<Box<dyn PacketSocket>>,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        let result = crate::OwnedPacketSocket::create_production(self.clone(), role, create);
        if self.creation_failed() {
            return Err(crate::PacketSocketEventError::ProviderFailed);
        }
        result
    }
    /// Create on an explicit owning production SDK network role. Products are
    /// sequence-local; use PacketSocketRuntime::post to obtain this capability.
    pub fn create_udp_socket(
        &self,
        role: &crate::PacketSocketRole,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone(role, |context| {
            self.call(|p| p.create_udp_socket(context, local, min_port, max_port))
                .flatten()
        })
    }
    pub fn create_client_tcp_socket(
        &self,
        role: &crate::PacketSocketRole,
        local: SocketAddress,
        remote: SocketAddress,
        options: PacketSocketTcpOptions,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone(role, |context| {
            self.call(|p| p.create_client_tcp_socket(context, local, remote, options))
                .flatten()
        })
    }
    pub fn create_client_udp_socket(
        &self,
        role: &crate::PacketSocketRole,
        local: SocketAddress,
        remote: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketTcpOptions,
    ) -> Result<Option<crate::OwnedPacketSocket>, crate::PacketSocketEventError> {
        self.standalone(role, |context| {
            self.call(|p| {
                p.create_client_udp_socket(context, local, remote, min_port, max_port, options)
            })
            .flatten()
        })
    }
    pub fn create_server_tcp_socket(
        &self,
        role: &crate::PacketSocketRole,
        local: SocketAddress,
        min_port: u16,
        max_port: u16,
        options: PacketSocketFlags,
    ) -> Result<Option<crate::OwnedListenSocket>, crate::PacketSocketEventError> {
        let result = crate::OwnedListenSocket::create_production(self.clone(), role, |context| {
            self.call(|p| p.create_server_tcp_socket(context, local, min_port, max_port, options))
                .flatten()
        });
        if self.creation_failed() {
            return Err(crate::PacketSocketEventError::ProviderFailed);
        }
        result
    }
    /// Own the legacy native DNS product on the selected production role. The
    /// effective factory environment is retained because native has no env arg.
    pub fn create_async_dns_resolver(
        &self,
        role: &crate::PacketSocketRole,
    ) -> Result<crate::OwnedDnsResolver, crate::PacketSocketEventError> {
        role.check_current()?;
        self.owned_dns(crate::owned_packet_socket::RustSocketRole {
            _environment: role.environment().clone(),
            owner: crate::owned_packet_socket::SocketRoleOwner::Production(role.runtime.clone()),
        })
    }
    pub fn create_async_dns_resolver_controlled(
        &self,
        driver: &crate::ControlledPeerDriver,
        environment: &Environment,
    ) -> Result<crate::OwnedDnsResolver, crate::PacketSocketEventError> {
        if !self.is_controlled() {
            return Err(if self.creation_failed() {
                crate::PacketSocketEventError::ProviderFailed
            } else {
                crate::PacketSocketEventError::IncompatibleExecution
            });
        }
        if !driver.is_current() {
            return Err(crate::PacketSocketEventError::WrongSequence);
        }
        if !environment.uses_controlled_clock(driver.clock()) {
            return Err(crate::PacketSocketEventError::IncompatibleExecution);
        }
        self.owned_dns(crate::owned_packet_socket::RustSocketRole {
            _environment: environment.clone(),
            owner: crate::owned_packet_socket::SocketRoleOwner::Controlled(driver.clone()),
        })
    }
    fn owned_dns(
        &self,
        role: crate::owned_packet_socket::RustSocketRole,
    ) -> Result<crate::OwnedDnsResolver, crate::PacketSocketEventError> {
        let _turn = crate::execution::controlled_owner_turn();
        let product = self
            .call(|provider| -> Box<dyn AsyncDnsResolver> {
                Box::new(RetainedResolver {
                    product: provider.create_async_dns_resolver(role._environment.clone()),
                    _factory: self.clone(),
                    _environment: role._environment.clone(),
                })
            })
            .ok_or(crate::PacketSocketEventError::ProviderFailed)?;
        crate::OwnedDnsResolver::new(
            self.clone(),
            role,
            crate::async_dns::socket_factory_resolver(Some(product), self.failure_latch()),
        )
    }
    pub(crate) fn creation_failed(&self) -> bool {
        self.0.disabled.load(Ordering::Acquire)
    }
    fn call<T>(
        &self,
        action: impl FnOnce(&dyn PacketSocketFactoryImplementation) -> T,
    ) -> Option<T> {
        if self.0.disabled.load(Ordering::Acquire) {
            return None;
        }
        match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            action(self.0.provider.as_deref().unwrap())
        })) {
            Ok(value) => Some(value),
            Err(_) => {
                self.0.failed.store(true, Ordering::Release);
                self.0.disabled.store(true, Ordering::Release);
                None
            }
        }
    }
    pub fn is_controlled(&self) -> bool {
        self.call(|provider| provider.is_controlled())
            .unwrap_or(false)
    }
}

pub(crate) struct RustPacketSocketFactory(pub Option<PacketSocketFactory>, pub Environment);
impl Drop for RustPacketSocketFactory {
    fn drop(&mut self) {
        let value = self.0.take();
        let _ = crate::provider_panic::catch_provider(|| drop(value));
    }
}
pub(crate) fn socket_factory_present(source: &RustPacketSocketFactory) -> bool {
    source.0.is_some()
}

pub(crate) fn socket_factory_create_packet(
    source: &RustPacketSocketFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
    events: cxx::SharedPtr<ffi::NativePacketSocketEvents>,
    request: ffi::FfiSocketCreateRequest,
) -> Box<RustPacketSocket> {
    let factory = source.0.as_ref().expect("selected socket factory");
    let context = PacketSocketContext::new(
        Environment::from_sdk(environment, Some(&source.1)),
        resources,
        events,
    )
    .with_factory(Some(factory.clone()));
    let product = factory
        .call(|provider| {
            let local = SocketAddress::from_native(request.local);
            match request.kind {
                0 => provider.create_udp_socket(
                    context.clone(),
                    local,
                    request.min_port,
                    request.max_port,
                ),
                1 => provider.create_client_tcp_socket(
                    context.clone(),
                    local,
                    SocketAddress::from_native(request.remote),
                    PacketSocketTcpOptions::from_native(request.tcp),
                ),
                2 => provider.create_client_udp_socket(
                    context.clone(),
                    local,
                    SocketAddress::from_native(request.remote),
                    request.min_port,
                    request.max_port,
                    PacketSocketTcpOptions::from_native(request.tcp),
                ),
                _ => unreachable!("private SDK creation operation"),
            }
        })
        .flatten();
    RustPacketSocket::from_creation(product, context, factory.0.failed.clone())
}
pub(crate) fn socket_factory_create_listener(
    source: &RustPacketSocketFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    resources: cxx::SharedPtr<ffi::NativePacketSocketResources>,
    events: cxx::SharedPtr<ffi::NativeListenSocketEvents>,
    request: ffi::FfiSocketCreateRequest,
) -> Box<RustListenSocket> {
    let factory = source.0.as_ref().expect("selected socket factory");
    let context = ListenSocketContext::new(
        Environment::from_sdk(environment, Some(&source.1)),
        resources,
        events,
        factory.0.failed.clone(),
    )
    .with_factory(Some(factory.clone()));
    let product = factory
        .call(|provider| {
            provider.create_server_tcp_socket(
                context.clone(),
                SocketAddress::from_native(request.local),
                request.min_port,
                request.max_port,
                PacketSocketFlags(request.tcp.options),
            )
        })
        .flatten();
    RustListenSocket::from_creation(product, context)
}
struct RetainedResolver {
    product: Box<dyn AsyncDnsResolver>,
    _factory: PacketSocketFactory,
    _environment: Environment,
}
impl AsyncDnsResolver for RetainedResolver {
    fn start(&mut self, a: SocketAddress, c: DnsResolutionCompletion) {
        self.product.start(a, c);
    }
    fn start_with_family(
        &mut self,
        a: SocketAddress,
        f: crate::AddressFamily,
        c: DnsResolutionCompletion,
    ) {
        self.product.start_with_family(a, f, c);
    }
    fn result(&self) -> DnsResolverResult {
        self.product.result()
    }
}
pub(crate) fn socket_factory_create_dns(
    source: &RustPacketSocketFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    admitted: bool,
) -> Box<crate::RustAsyncDnsResolver> {
    let factory = source.0.as_ref().expect("selected socket factory");
    let environment = Environment::from_sdk(environment, Some(&source.1));
    let product = admitted
        .then(|| {
            factory.call(|provider| -> Box<dyn AsyncDnsResolver> {
                Box::new(RetainedResolver {
                    product: provider.create_async_dns_resolver(environment.clone()),
                    _factory: factory.clone(),
                    _environment: environment,
                })
            })
        })
        .flatten();
    crate::async_dns::socket_factory_resolver(product, factory.0.failed.clone())
}
