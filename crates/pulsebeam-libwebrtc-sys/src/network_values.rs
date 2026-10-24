//! Owning SDK network values. These are not network-manager selections.
use crate::{
    AddressFamily, FieldTrialsSnapshot, InterfaceAddress, IpAddress, NetworkAdapterType,
    NetworkPreference, NetworkSlice, ffi,
};
use std::{cell::Cell, fmt, marker::PhantomData, rc::Rc};

/// The native callback list is currently sending on this same event channel.
/// Recursive Send and adding a receiver during Send are SDK precondition errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkChangeInProgress;
impl fmt::Display for NetworkChangeInProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("network event delivery is in progress")
    }
}
impl std::error::Error for NetworkChangeInProgress {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkChangeKind {
    AdapterType,
    Preference,
    Slice,
}

/// Tagged native subscription. Cancel and Drop unsubscribe the actual native
/// receiver; a callback may cancel itself. Final closure drop is deferred until
/// the SDK finishes receiver cleanup, and no unwinding crosses its callback ABI.
pub struct NetworkChangeSubscription {
    native: cxx::UniquePtr<ffi::NativeNetworkSubscription>,
    sequence: PhantomData<Rc<()>>,
}
impl NetworkChangeSubscription {
    pub fn cancel(&self) -> bool {
        ffi::network_subscription_cancel(self.native.as_ref().unwrap())
    }
    pub fn has_failed(&self) -> bool {
        ffi::network_subscription_failed(self.native.as_ref().unwrap())
    }
}
pub(crate) struct RustNetworkCallback {
    callback: Option<Box<dyn Fn(Network)>>,
    failed: Cell<bool>,
}
impl Drop for RustNetworkCallback {
    fn drop(&mut self) {
        let value = self.callback.take();
        let _ = crate::provider_panic::catch_provider(|| drop(value));
    }
}
pub(crate) fn network_callback_failed(source: &RustNetworkCallback) -> bool {
    source.failed.get()
}
pub(crate) fn network_callback_fire(
    source: &RustNetworkCallback,
    network: cxx::SharedPtr<ffi::NativeNetwork>,
) {
    if source.failed.get() {
        return;
    }
    if crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
        source.callback.as_ref().unwrap()(Network::from_native(network))
    }))
    .is_err()
    {
        source.failed.set(true);
    }
}

/// A stable native Network allocation, confined to its caller sequence.
/// Shared handles retain the same SDK object; Clone invokes the native deep
/// clone, which intentionally drops connected signals and provider links.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<pulsebeam_webrtc_sys::Network>();
/// ```
pub struct Network {
    native: cxx::SharedPtr<ffi::NativeNetwork>,
    sequence: PhantomData<Rc<()>>,
}
impl Network {
    pub fn new(
        name: impl AsRef<[u8]>,
        description: impl AsRef<[u8]>,
        prefix: impl Into<IpAddress>,
        prefix_length: i32,
    ) -> Self {
        Self::with_adapter(
            name,
            description,
            prefix,
            prefix_length,
            NetworkAdapterType::Unknown,
        )
    }
    pub fn with_adapter(
        name: impl AsRef<[u8]>,
        description: impl AsRef<[u8]>,
        prefix: impl Into<IpAddress>,
        prefix_length: i32,
        adapter: NetworkAdapterType,
    ) -> Self {
        Self::from_native(ffi::network_value_new(
            name.as_ref(),
            description.as_ref(),
            &prefix.into().bytes(),
            prefix_length,
            adapter as i32,
        ))
    }
    pub(crate) fn from_native(native: cxx::SharedPtr<ffi::NativeNetwork>) -> Self {
        assert!(!native.is_null());
        Self {
            native,
            sequence: PhantomData,
        }
    }
    pub(crate) fn shared_native(&self) -> cxx::SharedPtr<ffi::NativeNetwork> {
        self.native.clone()
    }
    pub(crate) fn native(&self) -> &ffi::NativeNetwork {
        self.native.as_ref().expect("owned native network")
    }
    /// Retain the same native identity. Mutations are visible to both handles.
    pub fn share(&self) -> Self {
        Self::from_native(self.native.clone())
    }
    /// Install or clear the SDK's default-local-address provider link. The
    /// network retains the provider, and returned aliases own it independently.
    pub fn set_default_local_address_provider(
        &mut self,
        provider: Option<&crate::DefaultLocalAddressProviderHandle>,
    ) {
        ffi::network_value_set_default_provider(
            self.native(),
            provider
                .map(|value| value.native.clone())
                .unwrap_or_else(cxx::SharedPtr::null),
        );
    }
    pub fn default_local_address_provider(
        &self,
    ) -> Option<crate::DefaultLocalAddressProviderHandle> {
        let native = ffi::network_value_default_provider(self.native());
        (!native.is_null()).then_some(crate::DefaultLocalAddressProviderHandle { native })
    }
    /// Install or clear the actual SDK mDNS provider link. It owns the selected
    /// facade; manager retirement cancels pending work and makes it unavailable.
    pub fn set_mdns_responder(&mut self, responder: Option<&crate::MdnsResponder>) {
        ffi::network_value_set_mdns(
            self.native(),
            responder
                .map(|value| value.native.clone())
                .unwrap_or_else(cxx::SharedPtr::null),
        );
    }
    pub fn mdns_responder(&self) -> Option<crate::MdnsResponder> {
        let native = ffi::network_value_mdns(self.native());
        (!native.is_null()).then(|| crate::MdnsResponder::from_native(native))
    }
    /// Publish the native event for this exact network without changing its
    /// fields. Recursive publication on the same SDK channel is rejected;
    /// callback payloads identify this actual native allocation.
    pub fn notify_change(&self, kind: NetworkChangeKind) -> Result<(), NetworkChangeInProgress> {
        let _turn = crate::execution::controlled_owner_turn();
        let kind = match kind {
            NetworkChangeKind::AdapterType => 0,
            NetworkChangeKind::Preference => 1,
            NetworkChangeKind::Slice => 2,
        };
        if ffi::network_value_notify(self.native(), kind) {
            Ok(())
        } else {
            Err(NetworkChangeInProgress)
        }
    }
    pub fn subscribe(
        &self,
        kind: NetworkChangeKind,
        callback: impl Fn(Network) + 'static,
    ) -> Result<NetworkChangeSubscription, NetworkChangeInProgress> {
        let kind = match kind {
            NetworkChangeKind::AdapterType => 0,
            NetworkChangeKind::Preference => 1,
            NetworkChangeKind::Slice => 2,
        };
        let native = ffi::network_value_subscribe(
            self.native.clone(),
            kind,
            Box::new(RustNetworkCallback {
                callback: Some(Box::new(callback)),
                failed: Cell::new(false),
            }),
        );
        if native.is_null() {
            Err(NetworkChangeInProgress)
        } else {
            Ok(NetworkChangeSubscription {
                native,
                sequence: PhantomData,
            })
        }
    }
    pub fn is_same_network(&self, other: &Self) -> bool {
        ffi::network_value_same(self.native(), other.native())
    }
    fn bytes(&self, field: u8) -> Vec<u8> {
        ffi::network_value_bytes(self.native(), field)
    }
    fn info(&self, field: u8) -> i32 {
        ffi::network_value_info(self.native(), field)
    }
    fn set(&mut self, field: u8, value: i32) -> Result<(), NetworkChangeInProgress> {
        let _turn = crate::execution::controlled_owner_turn();
        if ffi::network_value_set(self.native(), field, value) {
            Ok(())
        } else {
            Err(NetworkChangeInProgress)
        }
    }
    pub fn name(&self) -> Vec<u8> {
        self.bytes(0)
    }
    pub fn description(&self) -> Vec<u8> {
        self.bytes(1)
    }
    pub fn prefix(&self) -> IpAddress {
        IpAddress::from_bytes(self.bytes(2))
    }
    pub fn key(&self) -> Vec<u8> {
        self.bytes(3)
    }
    pub fn best_ip(&self) -> IpAddress {
        IpAddress::from_bytes(self.bytes(4))
    }
    pub fn to_native_debug_text(&self) -> Vec<u8> {
        self.bytes(5)
    }
    pub fn family(&self) -> AddressFamily {
        AddressFamily::from_native_value(self.info(11))
    }
    pub fn prefix_length(&self) -> i32 {
        self.info(12)
    }
    pub fn scope_id(&self) -> i32 {
        self.info(0)
    }
    pub fn set_scope_id(&mut self, value: i32) {
        let _ = self.set(0, value);
    }
    pub fn ignored(&self) -> bool {
        self.info(1) != 0
    }
    pub fn set_ignored(&mut self, value: bool) {
        let _ = self.set(1, value.into());
    }
    pub fn adapter_type(&self) -> NetworkAdapterType {
        adapter_from_native(self.info(2))
    }
    pub fn set_adapter_type(
        &mut self,
        value: NetworkAdapterType,
    ) -> Result<(), NetworkChangeInProgress> {
        self.set(2, value as i32)
    }
    pub fn underlying_type_for_vpn(&self) -> NetworkAdapterType {
        adapter_from_native(self.info(3))
    }
    pub fn set_underlying_type_for_vpn(
        &mut self,
        value: NetworkAdapterType,
    ) -> Result<(), NetworkChangeInProgress> {
        self.set(3, value as i32)
    }
    pub fn preference(&self) -> i32 {
        self.info(4)
    }
    pub fn set_preference(&mut self, value: i32) {
        let _ = self.set(4, value);
    }
    pub fn active(&self) -> bool {
        self.info(5) != 0
    }
    pub fn set_active(&mut self, value: bool) {
        let _ = self.set(5, value.into());
    }
    pub fn id(&self) -> u16 {
        self.info(6) as u16
    }
    pub fn set_id(&mut self, value: u16) {
        let _ = self.set(6, value.into());
    }
    pub fn network_preference(&self) -> NetworkPreference {
        match self.info(7) {
            0 => NetworkPreference::Neutral,
            -1 => NetworkPreference::NotPreferred,
            _ => unreachable!("known native preference"),
        }
    }
    pub fn set_network_preference(
        &mut self,
        value: NetworkPreference,
    ) -> Result<(), NetworkChangeInProgress> {
        self.set(7, value as i32)
    }
    pub fn network_slice(&self) -> NetworkSlice {
        slice_from_native(self.info(8))
    }
    pub fn set_network_slice(
        &mut self,
        value: NetworkSlice,
    ) -> Result<(), NetworkChangeInProgress> {
        self.set(8, value as i32)
    }
    pub fn is_vpn(&self) -> bool {
        self.info(9) != 0
    }
    pub fn is_cellular(&self) -> bool {
        self.info(10) != 0
    }
    pub fn add_ip(&mut self, address: impl Into<IpAddress>) {
        self.add_interface_address(InterfaceAddress::new(address, 0));
    }
    pub fn add_interface_address(&mut self, address: InterfaceAddress) {
        ffi::network_value_add_ip(self.native(), &address.address.bytes(), address.ipv6_flags);
    }
    pub fn ips(&self) -> Vec<InterfaceAddress> {
        ffi::network_value_ips(self.native())
            .into_iter()
            .map(|value| InterfaceAddress::new(IpAddress::from_bytes(value.ip), value.flags))
            .collect()
    }
    /// Uses SDK set-membership change detection, preserving list order and
    /// duplicates. `already_changed` only short-circuits that change check.
    pub fn set_ips(&mut self, ips: &[InterfaceAddress], already_changed: bool) -> bool {
        let values: Vec<_> = ips
            .iter()
            .map(|ip| ffi::FfiInterfaceAddress {
                ip: ip.address.bytes(),
                flags: ip.ipv6_flags,
            })
            .collect();
        ffi::network_value_set_ips(self.native(), &values, already_changed)
    }
    pub fn clear_ips(&mut self) {
        ffi::network_value_clear_ips(self.native());
    }
    pub fn cost(&self, trials: &FieldTrialsSnapshot) -> Result<u16, NetworkValueError> {
        let cost = ffi::network_value_cost(self.native(), trials.native());
        if cost < 0 {
            Err(NetworkValueError::VpnUnderlyingVpn)
        } else {
            Ok(cost as u16)
        }
    }
    pub fn guess_adapter_from_cost(cost: i32) -> NetworkCostGuess {
        let result = ffi::network_guess_cost(cost);
        NetworkCostGuess {
            adapter: adapter_from_native(result.adapter),
            vpn: result.vpn,
            slice: slice_from_native(result.slice),
        }
    }
    pub fn make_key(
        name: impl AsRef<[u8]>,
        prefix: impl Into<IpAddress>,
        prefix_length: i32,
    ) -> Vec<u8> {
        ffi::network_make_key(name.as_ref(), &prefix.into().bytes(), prefix_length)
    }
}
impl Clone for Network {
    fn clone(&self) -> Self {
        Self::from_native(ffi::network_value_clone(self.native()))
    }
}
impl fmt::Debug for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network")
            .field("native_text", &self.to_native_debug_text())
            .finish()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkCostGuess {
    pub adapter: NetworkAdapterType,
    pub vpn: bool,
    pub slice: NetworkSlice,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkValueError {
    VpnUnderlyingVpn,
    InvalidClassifierSuffix,
}
impl fmt::Display for NetworkValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::VpnUnderlyingVpn => {
                "the native cost computation cannot use VPN as its underlying adapter type"
            }
            Self::InvalidClassifierSuffix => {
                "the native interface classifier would pass a negative non-EOF byte to isdigit"
            }
        })
    }
}
impl std::error::Error for NetworkValueError {}
impl NetworkAdapterType {
    pub fn from_interface_name(name: impl AsRef<[u8]>) -> Result<Self, NetworkValueError> {
        let value = ffi::network_adapter_from_name(name.as_ref());
        if value < 0 {
            Err(NetworkValueError::InvalidClassifierSuffix)
        } else {
            Ok(adapter_from_native(value))
        }
    }
    pub fn is_cellular(self) -> bool {
        ffi::network_adapter_is_cellular(self as i32)
    }
}
fn adapter_from_native(value: i32) -> NetworkAdapterType {
    match value {
        0 => NetworkAdapterType::Unknown,
        1 => NetworkAdapterType::Ethernet,
        2 => NetworkAdapterType::Wifi,
        4 => NetworkAdapterType::Cellular,
        8 => NetworkAdapterType::Vpn,
        16 => NetworkAdapterType::Loopback,
        32 => NetworkAdapterType::Any,
        64 => NetworkAdapterType::Cellular2G,
        128 => NetworkAdapterType::Cellular3G,
        256 => NetworkAdapterType::Cellular4G,
        512 => NetworkAdapterType::Cellular5G,
        _ => unreachable!("known native adapter type"),
    }
}
fn slice_from_native(value: i32) -> NetworkSlice {
    match value {
        0 => NetworkSlice::None,
        1 => NetworkSlice::UnifiedCommunications,
        _ => unreachable!("known native slice"),
    }
}

/// Exact SDK NetworkMask value. Construction stores the supplied prefix and
/// signed length; it does not truncate the address or normalize the length.
pub struct IpNetworkMask {
    native: cxx::UniquePtr<ffi::NativeNetworkMask>,
}
// SAFETY: this immutable native mask contains only an IP value and an integer.
unsafe impl Send for IpNetworkMask {}
// SAFETY: all exposed native methods are const and retain no borrowed storage.
unsafe impl Sync for IpNetworkMask {}
impl IpNetworkMask {
    pub fn new(address: impl Into<IpAddress>, prefix_length: i32) -> Self {
        Self {
            native: ffi::network_mask_new(&address.into().bytes(), prefix_length),
        }
    }
    fn native(&self) -> &ffi::NativeNetworkMask {
        self.native.as_ref().expect("owned native network mask")
    }
    pub fn address(&self) -> IpAddress {
        IpAddress::from_bytes(ffi::network_mask_address(self.native()))
    }
    pub fn prefix_length(&self) -> i32 {
        ffi::network_mask_prefix_length(self.native())
    }
}
impl Clone for IpNetworkMask {
    fn clone(&self) -> Self {
        Self::new(self.address(), self.prefix_length())
    }
}
impl PartialEq for IpNetworkMask {
    fn eq(&self, other: &Self) -> bool {
        ffi::network_mask_equal(self.native(), other.native())
    }
}
impl Eq for IpNetworkMask {}
impl fmt::Debug for IpNetworkMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpNetworkMask")
            .field("address", &self.address())
            .field("prefix_length", &self.prefix_length())
            .finish()
    }
}
