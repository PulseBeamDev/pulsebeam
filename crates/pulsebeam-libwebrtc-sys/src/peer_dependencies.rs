use std::sync::Arc;

/// Owned executable dependencies supplied for one peer, separate from its
/// RtcConfiguration values. Providers are retained by native peer/environment
/// owners, not borrowed from the caller or factory builder.
///
/// Exposes per-peer field trials, TLS verification, local-network permissions,
/// asynchronous DNS, asynchronous certificate generation, and video bitrate
/// allocator factories. Other pinned native
/// dependency interfaces remain to be bridged; this is not a complete inventory.
#[derive(Clone, Default)]
pub struct PeerConnectionDependencies {
    /// Overrides the factory environment's trials for this peer. Does not
    /// change the factory environment or subsequently created default peers.
    pub field_trials: Option<Arc<dyn crate::FieldTrialsView>>,
    /// Custom TURN TLS verification. Cannot be combined with the configuration's
    /// selected TURN CA verifier, since both occupy the same native dependency.
    pub tls_certificate_verifier: Option<Arc<dyn crate::TlsCertificateVerifier>>,
    /// Creates asynchronous permission products on the native network sequence.
    pub local_network_permission_factory: Option<Arc<dyn crate::LocalNetworkPermissionFactory>>,
    /// Asynchronous certificate generation, independently retained by this peer.
    pub certificate_generator: Option<crate::CertificateGenerator>,
    /// Owned factory/products with callable native DNS result views.
    pub async_dns_resolver_factory: Option<crate::AsyncDnsResolverFactory>,
    /// Independently owned callable video bitrate allocator factory/products.
    pub video_bitrate_allocator_factory: Option<crate::VideoBitrateAllocatorFactory>,
}
impl PeerConnectionDependencies {
    pub fn video_bitrate_allocator_factory(
        mut self,
        factory: &crate::VideoBitrateAllocatorFactory,
    ) -> Self {
        self.video_bitrate_allocator_factory = Some(factory.clone());
        self
    }
    pub fn video_bitrate_allocator_provider(
        self,
        provider: impl crate::VideoBitrateAllocatorFactoryProvider,
    ) -> Self {
        self.video_bitrate_allocator_factory(&crate::VideoBitrateAllocatorFactory::from_provider(
            provider,
        ))
    }
    pub fn async_dns_resolver_factory(mut self, factory: &crate::AsyncDnsResolverFactory) -> Self {
        self.async_dns_resolver_factory = Some(factory.clone());
        self
    }
    pub fn async_dns_resolver_provider(
        self,
        provider: impl crate::AsyncDnsResolverFactoryProvider,
    ) -> Self {
        self.async_dns_resolver_factory(&crate::AsyncDnsResolverFactory::from_provider(provider))
    }
    pub fn certificate_generator(mut self, generator: &crate::CertificateGenerator) -> Self {
        self.certificate_generator = Some(generator.clone());
        self
    }
    pub fn certificate_generator_provider(
        self,
        provider: impl crate::CertificateGeneratorProvider,
    ) -> Self {
        self.certificate_generator(&crate::CertificateGenerator::from_provider(provider))
    }
    pub fn local_network_permission_factory(
        mut self,
        factory: impl crate::LocalNetworkPermissionFactory,
    ) -> Self {
        self.local_network_permission_factory = Some(Arc::new(factory));
        self
    }
    pub fn tls_certificate_verifier(
        mut self,
        verifier: impl crate::TlsCertificateVerifier,
    ) -> Self {
        self.tls_certificate_verifier = Some(Arc::new(verifier));
        self
    }
    pub fn field_trials(mut self, view: impl crate::FieldTrialsView) -> Self {
        self.field_trials = Some(Arc::new(view));
        self
    }
}
