//! Runtime dependencies and ownership policy for the shared session graph.
use super::{PeerError, PeerErrorKind, Session, error, retirement::SessionRetirement};
use crate::{ControlledPeerDriver, Environment, PeerConnectionFactoryBuilder};

/// Runtime selection changes dependencies and final-owner policy, not session
/// operations or media selection. Implementations are sealed so a Send owner
/// cannot be constructed around an affine runtime.
pub trait SessionRuntime: sealed::Policy {
    /// Start a standalone SDK environment configuration with this runtime's
    /// native defaults. Setters retain their ordinary SDK semantics.
    /// Use `create_environment` after configuration to attach runtime ownership.
    fn environment_factory(
        &self,
    ) -> Result<crate::EnvironmentFactory, crate::BuildEnvironmentError> {
        sealed::Policy::default_environment_factory(self)
    }

    /// Invoke the actual SDK factory and attach this runtime's lifetime policy.
    /// This constructs no peer, device, owner worker or alternate media engine.
    /// Explicit factory selections are preserved, including external services
    /// outside managed deterministic execution. Release environments and their
    /// dependent products before closing the caller-owned runtime scope.
    fn create_environment(
        &self,
        factory: &crate::EnvironmentFactory,
    ) -> Result<Environment, crate::BuildEnvironmentError> {
        sealed::Policy::bind_environment(self, factory.create()?)
    }
}

/// Native threaded execution. A session with this runtime is move-only Send.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadedRuntime;

/// Caller-pumped execution. This owner and its sessions remain sequence-bound.
/// Create it with `ControlledWorld::runtime`; network delivery remains supplied
/// by the outer harness through the ordinary session builder registrations.
pub struct ManagedRuntime {
    // Release SDK dependencies before the affine driver.
    environment: Environment,
    network_manager: Option<crate::NetworkManagerProvider>,
    socket_factory: Option<crate::PacketSocketFactoryProvider>,
    driver: ControlledPeerDriver,
}
impl ManagedRuntime {
    pub(crate) fn new(environment: Environment, driver: ControlledPeerDriver) -> Self {
        Self {
            environment,
            driver,
            network_manager: None,
            socket_factory: None,
        }
    }

    /// Supply caller-controlled packet services in the outer harness. The
    /// application still uses the same Session::with_runtime construction.
    pub fn with_endpoint(
        mut self,
        endpoint: &crate::NetworkEndpoint,
    ) -> Result<Self, crate::NetworkError> {
        self.network_manager = Some(endpoint.network_manager()?);
        self.socket_factory = Some(endpoint.packet_socket_factory()?);
        Ok(self)
    }
}

impl SessionRuntime for ThreadedRuntime {}
impl SessionRuntime for ManagedRuntime {}

// This policy is not an application extension point. In particular, only the
// concrete ThreadedRuntime may hand a captive graph to a retirement worker.
#[allow(
    private_interfaces,
    reason = "The sealed policy carries private retirement tokens, not application extension points."
)]
mod sealed {
    use super::*;
    pub trait Policy: Sized {
        fn configure(
            &mut self,
            builder: PeerConnectionFactoryBuilder,
        ) -> PeerConnectionFactoryBuilder;
        fn reserve(&self) -> Result<Option<SessionRetirement>, PeerError>;
        fn retire(session: &mut Session<Self>) -> bool
        where
            Self: SessionRuntime;
        fn requires_threaded_queues(&self) -> bool;
        fn default_environment_factory(
            &self,
        ) -> Result<crate::EnvironmentFactory, crate::BuildEnvironmentError>;
        fn bind_environment(
            &self,
            environment: Environment,
        ) -> Result<Environment, crate::BuildEnvironmentError> {
            Ok(environment)
        }
    }
    impl Policy for ThreadedRuntime {
        fn configure(
            &mut self,
            builder: PeerConnectionFactoryBuilder,
        ) -> PeerConnectionFactoryBuilder {
            builder
        }
        fn reserve(&self) -> Result<Option<SessionRetirement>, PeerError> {
            SessionRetirement::new().map(Some)
        }
        fn retire(session: &mut Session<Self>) -> bool {
            if session.retirement.is_some()
                && (crate::provider_panic::callback_active()
                    || session.factory.requires_deferred_teardown())
            {
                let mut owner = session.take_for_cleanup();
                let retirement = owner.retirement.take().expect("admitted retirement owner");
                retirement.retire(owner);
                true
            } else {
                false
            }
        }
        fn requires_threaded_queues(&self) -> bool {
            true
        }
        fn default_environment_factory(
            &self,
        ) -> Result<crate::EnvironmentFactory, crate::BuildEnvironmentError> {
            Ok(crate::EnvironmentFactory::default())
        }
    }
    impl Policy for ManagedRuntime {
        fn configure(
            &mut self,
            builder: PeerConnectionFactoryBuilder,
        ) -> PeerConnectionFactoryBuilder {
            let mut builder = builder
                .environment(self.environment.clone())
                .controlled_driver(&self.driver);
            if let Some(manager) = self.network_manager.take() {
                builder = builder.network_manager(manager);
            }
            if let Some(sockets) = self.socket_factory.take() {
                builder = builder.packet_socket_factory(sockets);
            }
            builder
        }
        fn reserve(&self) -> Result<Option<SessionRetirement>, PeerError> {
            Ok(None)
        }
        fn retire(_session: &mut Session<Self>) -> bool {
            false
        }
        fn requires_threaded_queues(&self) -> bool {
            false
        }
        fn default_environment_factory(
            &self,
        ) -> Result<crate::EnvironmentFactory, crate::BuildEnvironmentError> {
            crate::EnvironmentFactory::from_environment(&self.environment)
        }
        fn bind_environment(
            &self,
            environment: Environment,
        ) -> Result<Environment, crate::BuildEnvironmentError> {
            environment.bind_driver(&self.driver)
        }
    }
}

pub(super) fn configure<R: SessionRuntime>(
    runtime: &mut R,
    builder: PeerConnectionFactoryBuilder,
) -> PeerConnectionFactoryBuilder {
    runtime.configure(builder)
}
pub(super) fn reserve<R: SessionRuntime>(
    runtime: &R,
) -> Result<Option<SessionRetirement>, PeerError> {
    runtime.reserve()
}
pub(super) fn retire<R: SessionRuntime>(session: &mut Session<R>) -> bool {
    R::retire(session)
}
pub(super) fn environment_factory<R: SessionRuntime>(
    runtime: &R,
) -> Result<crate::EnvironmentFactory, PeerError> {
    runtime
        .environment_factory()
        .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))
}
pub(super) fn validate_queues<R: SessionRuntime>(
    runtime: &R,
    cooperative: Option<bool>,
) -> Result<(), PeerError> {
    if runtime.requires_threaded_queues() && cooperative != Some(false) {
        return Err(error(
            PeerErrorKind::UnsupportedParameter,
            "movable sessions require qualified non-cooperative task queues",
        ));
    }
    Ok(())
}
