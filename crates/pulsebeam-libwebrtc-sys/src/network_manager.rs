//! General caller-owned manager products, created and retired on the SDK network role.
use crate::{
    AddressFamily, Environment, FieldTrialsSnapshot, IpAddress, IpNetworkMask, Network,
    NetworkChangeNotifier, ffi,
};
use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkEnumerationPermission {
    Allowed,
    Blocked,
}

/// The SDK's count value. This is not a live manager enumeration or query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkManagerStats {
    pub ipv4_network_count: i32,
    pub ipv6_network_count: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkManagerEvent {
    NetworksChanged,
    Error,
}
impl NetworkManagerEvent {
    fn tag(self) -> u8 {
        match self {
            Self::NetworksChanged => 0,
            Self::Error => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkManagerObservationError {
    Retired,
    /// The factory has not finished constructing the actual SDK facade yet.
    NotReady,
    /// The same native CallbackList is sending. Reentrant publication and new
    /// receiver addition on that channel would enter an SDK precondition check.
    DeliveryInProgress,
}
impl fmt::Display for NetworkManagerObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retired => "network manager scope is retired",
            Self::NotReady => "network manager facade is not constructed yet",
            Self::DeliveryInProgress => "network manager event delivery is in progress",
        })
    }
}
impl std::error::Error for NetworkManagerObservationError {}

/// Sequence-bound access to a manager's actual SDK notification lists. A
/// retained alias does not keep the peer, facade or role alive. Registration
/// during factory creation binds when that facade is constructed; failed
/// creation cancels it without delivering a fabricated event.
#[derive(Clone)]
pub struct NetworkManagerObservationSource {
    resources: cxx::SharedPtr<ffi::NativeNetworkResources>,
    sequence: PhantomData<Rc<()>>,
}
impl NetworkManagerObservationSource {
    pub fn is_retired(&self) -> bool {
        ffi::manager_observation_retired(self.resources.as_ref().unwrap())
    }
    pub fn is_ready(&self) -> bool {
        ffi::manager_observation_ready(self.resources.as_ref().unwrap())
    }
    pub fn subscribe(
        &self,
        event: NetworkManagerEvent,
        callback: impl Fn() + 'static,
    ) -> Result<NetworkManagerSubscription, NetworkManagerObservationError> {
        let failed = Rc::new(Cell::new(false));
        let native = ffi::manager_observe(
            self.resources.clone(),
            event.tag(),
            Box::new(RustManagerCallback {
                callback: Some(Box::new(callback)),
                failed: failed.clone(),
            }),
        );
        if native.is_null() {
            Err(if self.is_retired() {
                NetworkManagerObservationError::Retired
            } else {
                NetworkManagerObservationError::DeliveryInProgress
            })
        } else {
            Ok(NetworkManagerSubscription {
                native,
                failed,
                sequence: PhantomData,
            })
        }
    }
    /// Synchronous native Notify on this sequence, independently of updating
    /// client count. For cross-thread posted delivery use the context's signals.
    pub fn notify(&self, event: NetworkManagerEvent) -> Result<(), NetworkManagerObservationError> {
        let _turn = crate::execution::controlled_owner_turn();
        match ffi::manager_observation_notify(self.resources.as_ref().unwrap(), event.tag()) {
            0 => Ok(()),
            1 => Err(NetworkManagerObservationError::Retired),
            2 => Err(NetworkManagerObservationError::DeliveryInProgress),
            _ => Err(NetworkManagerObservationError::NotReady),
        }
    }
}

/// Tagged SDK receiver. Cancel releases its captures after any active native
/// Send finishes receiver cleanup. Retirement cancels it on the creation role;
/// retained aliases remain inert and preserve the observed failure latch.
pub struct NetworkManagerSubscription {
    native: cxx::UniquePtr<ffi::NativeManagerSubscription>,
    failed: Rc<Cell<bool>>,
    sequence: PhantomData<Rc<()>>,
}
impl NetworkManagerSubscription {
    pub fn cancel(&self) -> bool {
        ffi::manager_subscription_cancel(self.native.as_ref().unwrap())
    }
    pub fn is_active(&self) -> bool {
        ffi::manager_subscription_active(self.native.as_ref().unwrap())
    }
    pub fn has_failed(&self) -> bool {
        self.failed.get()
    }
}

pub(crate) struct RustManagerCallback {
    callback: Option<Box<dyn Fn()>>,
    failed: Rc<Cell<bool>>,
}
impl Drop for RustManagerCallback {
    fn drop(&mut self) {
        let value = self.callback.take();
        if crate::provider_panic::catch_provider(|| drop(value)).is_err() {
            self.failed.set(true);
        }
    }
}
pub(crate) fn manager_callback_fire(source: &RustManagerCallback) {
    if source.failed.get() {
        return;
    }
    if crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
        source.callback.as_ref().unwrap()()
    }))
    .is_err()
    {
        source.failed.set(true);
    }
}

/// Thread-safe event sources. Delivery is posted to the existing native network
/// role; Stop of the last updating client and retirement cancel queued events.
#[derive(Clone)]
pub struct NetworkManagerSignals {
    changes: NetworkChangeNotifier,
    errors: NetworkChangeNotifier,
}
impl NetworkManagerSignals {
    pub fn notify_networks_changed(&self) -> bool {
        self.changes.notify()
    }
    pub fn notify_error(&self) -> bool {
        self.errors.notify()
    }
}

/// Created on the selected native network role, with owning environment/trial
/// views. It cannot be transferred to another caller sequence.
pub struct NetworkManagerContext {
    environment: Environment,
    trials: FieldTrialsSnapshot,
    signals: NetworkManagerSignals,
    resources: cxx::SharedPtr<ffi::NativeNetworkResources>,
    factory: NetworkManagerFactory,
    failed: Rc<Cell<bool>>,
    sequence: PhantomData<Rc<()>>,
}
impl NetworkManagerContext {
    pub fn environment(&self) -> Environment {
        self.environment.clone()
    }
    pub fn field_trials(&self) -> FieldTrialsSnapshot {
        self.trials.clone()
    }
    pub fn signals(&self) -> NetworkManagerSignals {
        self.signals.clone()
    }
    pub fn observations(&self) -> NetworkManagerObservationSource {
        NetworkManagerObservationSource {
            resources: self.resources.clone(),
            sequence: PhantomData,
        }
    }
    /// Construct an owning mDNS product on this role. The manager scope retires
    /// pending callbacks and the provider before releasing its native threads.
    pub fn mdns_responder(
        &self,
        provider: impl crate::MdnsResponderProvider,
    ) -> Result<crate::MdnsResponder, crate::MdnsOperationError> {
        let source = crate::RustMdnsResponder::new(
            provider,
            self.factory.clone(),
            self.signals.clone(),
            self.failed.clone(),
        );
        let native = ffi::mdns_responder_new(self.resources.clone(), Box::new(source));
        if native.is_null() {
            Err(crate::MdnsOperationError::Retired)
        } else {
            Ok(crate::MdnsResponder::from_native(native))
        }
    }
}

/// Implementations may contain sequence-bound Network handles. All operations
/// and final destruction run on the native network role, including rejected
/// factory creation. Shared receivers permit synchronous read reentry; use
/// sequence-local interior state rather than holding exclusive loans over calls
/// that can notify native subscribers. Calls must not block on or reenter the owning peer/actor.
/// Return shared handles for repeat enumeration of the same network, not native
/// deep clones. The adapter retains every returned allocation until retirement.
/// Networks needing default-address or mDNS capabilities must carry their own
/// provider links; the SDK allocator does not attach them automatically.
pub trait NetworkManager: 'static {
    fn initialize(&self) {}
    fn start_updating(&self);
    fn stop_updating(&self);
    fn networks(&self) -> Vec<Network>;
    fn any_address_networks(&self) -> Vec<Network>;
    fn enumeration_permission(&self) -> NetworkEnumerationPermission {
        NetworkEnumerationPermission::Allowed
    }
    fn default_local_address(&self, _family: AddressFamily) -> Option<IpAddress> {
        None
    }
    fn mdns_responder(&self) -> Option<crate::MdnsResponder> {
        None
    }
    fn dump_networks(&self) {}
    fn set_vpn_list(&self, _masks: Vec<IpNetworkMask>) {}
}

pub trait NetworkManagerFactoryProvider: Send + Sync + 'static {
    /// None is construction rejection, never permission to select a stock manager.
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>>;
    /// Controlled implementations must use only caller-owned progress/resources.
    fn is_controlled(&self) -> bool {
        false
    }
}
impl<T: NetworkManagerFactoryProvider + ?Sized> NetworkManagerFactoryProvider for Arc<T> {
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>> {
        (**self).create(context)
    }
    fn is_controlled(&self) -> bool {
        (**self).is_controlled()
    }
}

struct Registration {
    provider: Option<Arc<dyn NetworkManagerFactoryProvider>>,
    failed: AtomicBool,
    disabled: AtomicBool,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let value = self.provider.take();
        let _ = crate::provider_panic::catch_provider(|| drop(value));
    }
}
#[derive(Clone)]
pub struct NetworkManagerFactory(Arc<Registration>);
impl NetworkManagerFactory {
    pub fn from_provider(provider: impl NetworkManagerFactoryProvider) -> Self {
        Self(Arc::new(Registration {
            provider: Some(Arc::new(provider)),
            failed: AtomicBool::new(false),
            disabled: AtomicBool::new(false),
        }))
    }
    pub(crate) fn record_failure(&self) {
        self.0.failed.store(true, Ordering::Release);
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    pub fn is_controlled(&self) -> bool {
        if self.0.disabled.load(Ordering::Acquire) {
            return false;
        }
        match crate::provider_panic::catch_provider(|| {
            self.0.provider.as_ref().unwrap().is_controlled()
        }) {
            Ok(value) => value,
            Err(_) => {
                self.0.failed.store(true, Ordering::Release);
                self.0.disabled.store(true, Ordering::Release);
                false
            }
        }
    }
}

pub(crate) struct RustNetworkManagerFactory(pub Option<NetworkManagerFactory>, pub Environment);
impl Drop for RustNetworkManagerFactory {
    fn drop(&mut self) {
        let value = self.0.take();
        let _ = crate::provider_panic::catch_provider(|| drop(value));
    }
}
pub(crate) struct RustNetworkManager {
    product: Option<Box<dyn NetworkManager>>,
    factory: Option<NetworkManagerFactory>,
    failed: Rc<Cell<bool>>,
}
impl RustNetworkManager {
    fn fail(&self) {
        self.failed.set(true);
        if let Some(factory) = &self.factory {
            factory.0.failed.store(true, Ordering::Release);
        }
    }
    fn call<T>(&self, failure: T, operation: impl FnOnce(&dyn NetworkManager) -> T) -> T {
        if network_manager_failed(self) {
            return failure;
        }
        let Some(product) = self.product.as_ref() else {
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
impl Drop for RustNetworkManager {
    fn drop(&mut self) {
        let value = self.product.take();
        if crate::provider_panic::catch_provider(|| drop(value)).is_err() {
            self.fail();
        }
    }
}
pub(crate) fn network_manager_factory_present(source: &RustNetworkManagerFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn network_manager_create(
    source: &RustNetworkManagerFactory,
    trials: cxx::UniquePtr<ffi::NativeFieldTrials>,
    changes: cxx::SharedPtr<ffi::NativeNetworkChange>,
    errors: cxx::SharedPtr<ffi::NativeNetworkChange>,
    resources: cxx::SharedPtr<ffi::NativeNetworkResources>,
) -> Box<RustNetworkManager> {
    let factory = source.0.clone();
    let failed = Rc::new(Cell::new(false));
    let product = factory
        .as_ref()
        .filter(|factory| !factory.0.disabled.load(Ordering::Acquire))
        .and_then(|factory| {
            let context = NetworkManagerContext {
                environment: source.1.clone(),
                trials: FieldTrialsSnapshot::from_native(trials),
                signals: NetworkManagerSignals {
                    changes: NetworkChangeNotifier::from_native(changes),
                    errors: NetworkChangeNotifier::from_native(errors),
                },
                resources,
                factory: factory.clone(),
                failed: failed.clone(),
                sequence: PhantomData,
            };
            match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
                factory.0.provider.as_ref().unwrap().create(context)
            })) {
                Ok(value) => value,
                Err(_) => {
                    factory.0.failed.store(true, Ordering::Release);
                    factory.0.disabled.store(true, Ordering::Release);
                    None
                }
            }
        });
    Box::new(RustNetworkManager {
        product,
        factory,
        failed,
    })
}
pub(crate) fn network_manager_present(source: &RustNetworkManager) -> bool {
    source.product.is_some()
}
pub(crate) fn network_manager_failed(source: &RustNetworkManager) -> bool {
    source.failed.get()
}
pub(crate) fn network_manager_initialize(source: &RustNetworkManager) -> bool {
    source.call(false, |product| {
        product.initialize();
        true
    })
}
pub(crate) fn network_manager_start(source: &RustNetworkManager) {
    source.call((), |product| product.start_updating());
}
pub(crate) fn network_manager_stop(source: &RustNetworkManager) {
    // Cleanup is attempted even after an operational failure. The SDK and
    // adapter serialize this with every other product entry on the network role.
    if let Some(product) = source.product.as_ref()
        && crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            product.stop_updating()
        }))
        .is_err()
    {
        source.fail();
    }
}
pub(crate) fn network_manager_networks(
    source: &RustNetworkManager,
    any: bool,
) -> Vec<ffi::FfiNetworkHandle> {
    source.call(Vec::new(), |product| {
        let values = if any {
            product.any_address_networks()
        } else {
            product.networks()
        };
        values
            .iter()
            .map(|value| ffi::FfiNetworkHandle {
                network: value.shared_native(),
            })
            .collect()
    })
}
pub(crate) fn network_manager_permission(source: &RustNetworkManager) -> bool {
    source.call(false, |product| {
        product.enumeration_permission() == NetworkEnumerationPermission::Blocked
    })
}
pub(crate) fn network_manager_default_address(
    source: &RustNetworkManager,
    family: i32,
) -> ffi::FfiDefaultLocalAddress {
    let value = source.call(None, |product| {
        product.default_local_address(AddressFamily::from_native_value(family))
    });
    ffi::FfiDefaultLocalAddress {
        present: value.is_some(),
        ip: value.unwrap_or_default().bytes(),
    }
}
pub(crate) fn network_manager_mdns(
    source: &RustNetworkManager,
) -> cxx::SharedPtr<ffi::NativeMdnsResponder> {
    source.call(cxx::SharedPtr::null(), |product| {
        product
            .mdns_responder()
            .map(|value| value.native.clone())
            .unwrap_or_else(cxx::SharedPtr::null)
    })
}
pub(crate) fn network_manager_dump(source: &RustNetworkManager) {
    source.call((), |product| product.dump_networks());
}
pub(crate) fn network_manager_set_vpn(
    source: &RustNetworkManager,
    masks: Vec<ffi::FfiIpNetworkMask>,
) {
    source.call((), |product| {
        product.set_vpn_list(
            masks
                .into_iter()
                .map(|mask| IpNetworkMask::new(IpAddress::from_bytes(mask.ip), mask.prefix_length))
                .collect(),
        )
    });
}
