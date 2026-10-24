use crate::{FieldTrialsSnapshot, ffi};
use std::{
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(i32)]
pub enum NetworkAdapterType {
    #[default]
    Unknown = 0,
    Ethernet = 1,
    Wifi = 2,
    Cellular = 4,
    Vpn = 8,
    Loopback = 16,
    Any = 32,
    Cellular2G = 64,
    Cellular3G = 128,
    Cellular4G = 256,
    Cellular5G = 512,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(i32)]
pub enum NetworkPreference {
    #[default]
    Neutral = 0,
    NotPreferred = -1,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(i32)]
pub enum NetworkSlice {
    #[default]
    None = 0,
    UnifiedCommunications = 1,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum NetworkBindingResult {
    Success = 0,
    Failure = -1,
    NotImplemented = -2,
    AddressNotFound = -3,
    NetworkChanged = -4,
}
/// Native interface metadata. The default represents an unknown, unavailable
/// interface, not permission to use an unidentified interface.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkInterfaceInfo {
    pub adapter_type: NetworkAdapterType,
    pub underlying_type_for_vpn: NetworkAdapterType,
    pub preference: NetworkPreference,
    pub available: bool,
    pub slice: NetworkSlice,
}

/// Socket borrowed from the native bind callback. It must not be closed or
/// retained as an owned descriptor by the provider.
pub struct NetworkSocket<'a> {
    handle: i32,
    _borrow: PhantomData<&'a ()>,
}
impl NetworkSocket<'_> {
    pub fn raw_handle(&self) -> i32 {
        self.handle
    }
    #[cfg(unix)]
    pub fn borrowed_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: the native socket remains open throughout its synchronous
        // binding callback. The return borrow cannot outlive this wrapper.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.handle) }
    }
}

/// Thread-safe notification handle, independent of the monitor's lifetime.
/// Notifications are posted to the native network thread, never delivered
/// inline. Stop and destruction cancel previously posted notifications.
#[derive(Clone)]
pub struct NetworkChangeNotifier {
    native: cxx::SharedPtr<ffi::NativeNetworkChange>,
}
// SAFETY: the native handle protects its dispatch target and active generation
// with a mutex. Retirement clears the target before its thread can be dropped.
unsafe impl Send for NetworkChangeNotifier {}
unsafe impl Sync for NetworkChangeNotifier {}
impl NetworkChangeNotifier {
    pub(crate) fn from_native(native: cxx::SharedPtr<ffi::NativeNetworkChange>) -> Self {
        Self { native }
    }
    /// False when the monitor is not running or has been destroyed. True means
    /// accepted for posting, not guaranteed delivery before a subsequent stop.
    pub fn notify(&self) -> bool {
        self.native.as_ref().is_some_and(ffi::network_change_notify)
    }
}

/// Native network-thread monitor. All operations on an individual instance are
/// serialized on that thread. Multiple instances and shared application state
/// may be used concurrently. Stop must quiesce the provider's event source.
/// Interface names are bytes because OS interface names need not be UTF-8.
/// A callback panic disables subsequent operations (unavailable interfaces and
/// failed binds); Stop is still attempted for cleanup. Final destruction can
/// occur on another native role thread. Non-panicking destructors are recommended;
/// adapter-owned destruction contains a single unwind.
pub trait NetworkMonitor: Send + Sync + 'static {
    fn start(&self);
    fn stop(&self);
    fn interface_info(&self, interface_name: &[u8]) -> NetworkInterfaceInfo;
    fn supports_socket_binding(&self) -> bool {
        false
    }
    fn bind_socket(
        &self,
        _socket: NetworkSocket<'_>,
        _address: IpAddr,
        _interface_name: &[u8],
    ) -> NetworkBindingResult {
        NetworkBindingResult::NotImplemented
    }
}
impl<T: NetworkMonitor + ?Sized> NetworkMonitor for Arc<T> {
    fn start(&self) {
        (**self).start();
    }
    fn stop(&self) {
        (**self).stop();
    }
    fn interface_info(&self, name: &[u8]) -> NetworkInterfaceInfo {
        (**self).interface_info(name)
    }
    fn supports_socket_binding(&self) -> bool {
        (**self).supports_socket_binding()
    }
    fn bind_socket(
        &self,
        socket: NetworkSocket<'_>,
        address: IpAddr,
        name: &[u8],
    ) -> NetworkBindingResult {
        (**self).bind_socket(socket, address, name)
    }
}
/// Factory retained by the native peer factory. Creation runs on its network
/// thread; factory destruction may run on another native role thread. Returning
/// None selects the native no-monitor fallback for that manager. A creation
/// panic also selects that fallback; provider destructors must not panic.
pub trait NetworkMonitorFactory: Send + Sync + 'static {
    fn create(
        &self,
        trials: FieldTrialsSnapshot,
        changes: NetworkChangeNotifier,
    ) -> Option<Box<dyn NetworkMonitor>>;
}
impl<T: NetworkMonitorFactory + ?Sized> NetworkMonitorFactory for Arc<T> {
    fn create(
        &self,
        trials: FieldTrialsSnapshot,
        changes: NetworkChangeNotifier,
    ) -> Option<Box<dyn NetworkMonitor>> {
        (**self).create(trials, changes)
    }
}

pub(crate) struct RustNetworkMonitorFactory(pub Option<Arc<dyn NetworkMonitorFactory>>);
impl Drop for RustNetworkMonitorFactory {
    fn drop(&mut self) {
        let provider = self.0.take();
        let _ = crate::provider_panic::catch_provider(|| drop(provider));
    }
}
pub(crate) struct RustNetworkMonitor {
    monitor: Option<Box<dyn NetworkMonitor>>,
    failed: AtomicBool,
}
impl Drop for RustNetworkMonitor {
    fn drop(&mut self) {
        let monitor = self.monitor.take();
        let _ = crate::provider_panic::catch_provider(|| drop(monitor));
    }
}
impl RustNetworkMonitor {
    fn call<T>(&self, fallback: T, operation: impl FnOnce(&dyn NetworkMonitor) -> T) -> T {
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let Some(monitor) = &self.monitor else {
            return fallback;
        };
        match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            operation(monitor.as_ref())
        })) {
            Ok(value) => value,
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                fallback
            }
        }
    }
}
pub(crate) fn network_monitor_factory_present(factory: &RustNetworkMonitorFactory) -> bool {
    factory.0.is_some()
}
pub(crate) fn network_monitor_create(
    factory: &RustNetworkMonitorFactory,
    trials: cxx::UniquePtr<ffi::NativeFieldTrials>,
    changes: cxx::SharedPtr<ffi::NativeNetworkChange>,
) -> Box<RustNetworkMonitor> {
    let monitor = factory.0.as_ref().and_then(|factory| {
        crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            factory.create(
                FieldTrialsSnapshot::from_native(trials),
                NetworkChangeNotifier { native: changes },
            )
        }))
        .ok()
        .flatten()
    });
    Box::new(RustNetworkMonitor {
        monitor,
        failed: AtomicBool::new(false),
    })
}
pub(crate) fn network_monitor_present(monitor: &RustNetworkMonitor) -> bool {
    monitor.monitor.is_some()
}
pub(crate) fn network_monitor_start(monitor: &RustNetworkMonitor) -> bool {
    monitor.call(false, |value| {
        value.start();
        true
    })
}
pub(crate) fn network_monitor_stop(monitor: &RustNetworkMonitor) {
    // Cleanup remains attempted even after a preceding callback panic.
    if let Some(value) = &monitor.monitor
        && crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| value.stop()))
            .is_err()
    {
        monitor.failed.store(true, Ordering::Release);
    }
}
pub(crate) fn network_monitor_info(
    monitor: &RustNetworkMonitor,
    name: &[u8],
) -> ffi::FfiNetworkInterfaceInfo {
    let value = monitor.call(NetworkInterfaceInfo::default(), |monitor| {
        monitor.interface_info(name)
    });
    ffi::FfiNetworkInterfaceInfo {
        adapter_type: value.adapter_type as i32,
        underlying_type_for_vpn: value.underlying_type_for_vpn as i32,
        preference: value.preference as i32,
        available: value.available,
        slice: value.slice as i32,
    }
}
pub(crate) fn network_monitor_supports_binding(monitor: &RustNetworkMonitor) -> bool {
    monitor.call(false, |monitor| monitor.supports_socket_binding())
}
pub(crate) fn network_monitor_bind(
    monitor: &RustNetworkMonitor,
    socket: i32,
    address: &[u8],
    name: &[u8],
) -> i32 {
    if socket < 0 {
        return NetworkBindingResult::Failure as i32;
    }
    let address = match address.len() {
        4 => IpAddr::V4(Ipv4Addr::new(
            address[0], address[1], address[2], address[3],
        )),
        16 => {
            let mut octets = [0; 16];
            octets.copy_from_slice(address);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return NetworkBindingResult::AddressNotFound as i32,
    };
    monitor.call(NetworkBindingResult::Failure, |monitor| {
        monitor.bind_socket(
            NetworkSocket {
                handle: socket,
                _borrow: PhantomData,
            },
            address,
            name,
        )
    }) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Info;
    impl NetworkMonitor for Info {
        fn start(&self) {}
        fn stop(&self) {}
        fn interface_info(&self, name: &[u8]) -> NetworkInterfaceInfo {
            assert_eq!(name, b"\xff\0if");
            NetworkInterfaceInfo {
                adapter_type: NetworkAdapterType::Vpn,
                underlying_type_for_vpn: NetworkAdapterType::Cellular5G,
                preference: NetworkPreference::NotPreferred,
                available: true,
                slice: NetworkSlice::UnifiedCommunications,
            }
        }
        fn supports_socket_binding(&self) -> bool {
            true
        }
        fn bind_socket(
            &self,
            socket: NetworkSocket<'_>,
            address: IpAddr,
            name: &[u8],
        ) -> NetworkBindingResult {
            assert_eq!(name, b"\xff\0if");
            assert_eq!(address, IpAddr::V6(Ipv6Addr::LOCALHOST));
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                assert_eq!(socket.raw_handle(), socket.borrowed_fd().as_raw_fd());
            }
            NetworkBindingResult::NetworkChanged
        }
    }
    fn wrap(monitor: impl NetworkMonitor) -> RustNetworkMonitor {
        RustNetworkMonitor {
            monitor: Some(Box::new(monitor)),
            failed: AtomicBool::new(false),
        }
    }
    #[test]
    fn metadata_preserves_all_fields_and_non_utf8_interface_names() {
        let monitor = wrap(Info);
        let info = network_monitor_info(&monitor, b"\xff\0if");
        assert_eq!(info.adapter_type, NetworkAdapterType::Vpn as i32);
        assert_eq!(
            info.underlying_type_for_vpn,
            NetworkAdapterType::Cellular5G as i32
        );
        assert_eq!(info.preference, -1);
        assert!(info.available);
        assert_eq!(info.slice, 1);
        assert!(network_monitor_supports_binding(&monitor));
        assert_eq!(network_monitor_bind(&monitor, -1, &[127, 0, 0, 1], b""), -1);
        assert_eq!(network_monitor_bind(&monitor, 0, &[1, 2], b""), -3);
    }
    #[cfg(unix)]
    #[test]
    fn binding_borrows_native_socket_and_preserves_ipv6_and_result() {
        use std::os::fd::AsRawFd;
        let socket = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert_eq!(
            network_monitor_bind(
                &wrap(Info),
                socket.as_raw_fd(),
                &Ipv6Addr::LOCALHOST.octets(),
                b"\xff\0if"
            ),
            -4
        );
        assert!(
            socket.local_addr().is_ok(),
            "callback must not close its borrowed socket"
        );
    }
    struct Panic(Arc<std::sync::atomic::AtomicUsize>);
    impl NetworkMonitor for Panic {
        fn start(&self) {
            panic!("provider failure");
        }
        fn stop(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn interface_info(&self, _name: &[u8]) -> NetworkInterfaceInfo {
            panic!("must not be called after failure");
        }
    }
    #[test]
    fn callback_failure_is_contained_without_suppressing_stop_cleanup() {
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let monitor = wrap(Panic(stops.clone()));
        assert!(!network_monitor_start(&monitor));
        assert!(!network_monitor_info(&monitor, b"if").available);
        network_monitor_stop(&monitor);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }
}
