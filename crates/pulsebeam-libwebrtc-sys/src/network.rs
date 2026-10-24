use std::{
    fmt,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use crate::{ControlledPeerDriver, ManualClock, ffi};

/// A validated, fully resolved IP socket address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NetworkAddress {
    ip: IpAddr,
    port: u16,
}

impl NetworkAddress {
    pub fn new(ip: IpAddr, port: u16) -> Result<Self, NetworkError> {
        validate_ip(ip)?;
        if port == 0 {
            return Err(NetworkError::InvalidPort);
        }
        Ok(Self { ip, port })
    }

    pub fn ip(self) -> IpAddr {
        self.ip
    }

    pub fn port(self) -> u16 {
        self.port
    }
}

/// What an outbound network decision controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundKind {
    Udp,
    TcpConnect,
    TcpData,
}

/// A packet or connection attempt waiting for an explicit caller decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundPacket {
    pub id: u64,
    pub kind: OutboundKind,
    pub source: NetworkAddress,
    pub destination: NetworkAddress,
    pub payload: Vec<u8>,
    pub deadline: Duration,
}

/// A packet copied out of an adapter socket's receive callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedPacket {
    pub source: NetworkAddress,
    pub payload: Vec<u8>,
}

/// A deterministic packet network with no delay, loss, or ordering policy.
#[derive(Clone)]
pub struct SimulatedNetwork(Arc<NetworkInner>);

struct NetworkInner {
    native: cxx::UniquePtr<ffi::NativeSimulatedNetwork>,
    _clock: ManualClock,
}

/// A packet network on the caller-pumped peer thread. Unlike
/// `SimulatedNetwork`, it never starts an OS thread and is sequence-bound.
/// UDP delivery, client-side TCP connections and DNS answers are driven by
/// caller decisions. TURN/TLS uses the same simulated TCP byte stream;
/// TCP listeners are unsupported.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::ControlledSimulatedNetwork>();
/// ```
#[derive(Clone)]
pub struct ControlledSimulatedNetwork(Rc<ControlledNetworkInner>);

struct ControlledNetworkInner {
    native: cxx::UniquePtr<ffi::NativeSimulatedNetwork>,
    _clock: ManualClock,
    driver: ControlledPeerDriver,
}

#[derive(Clone)]
enum NetworkRoot {
    Threaded { _owner: SimulatedNetwork },
    Controlled(ControlledSimulatedNetwork),
}

impl NetworkRoot {
    fn is_controlled(&self) -> bool {
        matches!(self, Self::Controlled(_))
    }

    fn is_controlled_by(&self, driver: &ControlledPeerDriver) -> bool {
        matches!(self, Self::Controlled(network) if network.0.driver.same_identity(driver))
    }
}

// SAFETY: the native root protects shared packet state with a mutex and
// marshals every socket operation onto its owned WebRTC network thread.
unsafe impl Send for NetworkInner {}
// SAFETY: see the `Send` rationale; all exposed native methods are serialized.
unsafe impl Sync for NetworkInner {}

impl SimulatedNetwork {
    pub fn new(clock: &ManualClock) -> Result<Self, NetworkError> {
        let native = ffi::new_simulated_network(clock.native());
        if native.is_null() {
            Err(NetworkError::NativeConstructionFailed)
        } else {
            Ok(Self(Arc::new(NetworkInner {
                native,
                _clock: clock.clone(),
            })))
        }
    }

    pub fn register_endpoint(&self, ip: IpAddr) -> Result<NetworkEndpoint, NetworkError> {
        validate_ip(ip)?;
        let mut error = 0;
        let native = ffi::register_network_endpoint(self.native(), &ip_bytes(ip), &mut error);
        if native.is_null() {
            Err(native_error(error))
        } else {
            Ok(NetworkEndpoint {
                native,
                network: NetworkRoot::Threaded {
                    _owner: self.clone(),
                },
                identity: Rc::new(()),
                _creator_sequence: PhantomData,
            })
        }
    }

    pub fn next_packet(&self) -> Option<OutboundPacket> {
        take_packet(self.native())
    }

    pub fn deliver(&self, packet_id: u64) -> Result<(), NetworkError> {
        deliver(self.native(), packet_id, false)
    }

    /// Delivers one copy while retaining the packet for another decision.
    pub fn deliver_copy(&self, packet_id: u64) -> Result<(), NetworkError> {
        deliver(self.native(), packet_id, true)
    }

    pub fn drop_packet(&self, packet_id: u64) -> Result<(), NetworkError> {
        let mut error = 0;
        ffi::drop_outbound_packet(self.native(), packet_id, &mut error)
            .then_some(())
            .ok_or_else(|| native_error(error))
    }

    fn native(&self) -> &ffi::NativeSimulatedNetwork {
        self.0.native.as_ref().expect("validated simulated network")
    }
}

impl ControlledSimulatedNetwork {
    pub fn new(clock: &ManualClock, driver: &ControlledPeerDriver) -> Result<Self, NetworkError> {
        let native = ffi::new_controlled_simulated_network(clock.native(), driver.native());
        if native.is_null() {
            Err(NetworkError::NativeConstructionFailed)
        } else {
            Ok(Self(Rc::new(ControlledNetworkInner {
                native,
                _clock: clock.clone(),
                driver: driver.clone(),
            })))
        }
    }

    pub fn register_endpoint(&self, ip: IpAddr) -> Result<NetworkEndpoint, NetworkError> {
        validate_ip(ip)?;
        let mut error = 0;
        let native = ffi::register_network_endpoint(self.native(), &ip_bytes(ip), &mut error);
        if native.is_null() {
            Err(native_error(error))
        } else {
            Ok(NetworkEndpoint {
                native,
                network: NetworkRoot::Controlled(self.clone()),
                identity: Rc::new(()),
                _creator_sequence: PhantomData,
            })
        }
    }

    /// Add a DNS answer visible to all controlled socket factories on this
    /// network. Answers are delivered on the driver sequence when pumped.
    /// Unknown names and unsupported address families resolve to an error.
    pub fn add_dns_record(&self, hostname: &str, ip: IpAddr) -> Result<(), NetworkError> {
        validate_ip(ip)?;
        if hostname.is_empty() || hostname.contains(char::is_whitespace) {
            return Err(NetworkError::InvalidAddress);
        }
        ffi::add_simulated_dns_record(self.native(), hostname, &ip_bytes(ip))
            .then_some(())
            .ok_or(NetworkError::InvalidAddress)
    }

    pub fn next_packet(&self) -> Option<OutboundPacket> {
        take_packet(self.native())
    }

    pub fn deliver(&self, packet_id: u64) -> Result<(), NetworkError> {
        deliver(self.native(), packet_id, false)
    }

    pub fn deliver_copy(&self, packet_id: u64) -> Result<(), NetworkError> {
        deliver(self.native(), packet_id, true)
    }

    pub fn drop_packet(&self, packet_id: u64) -> Result<(), NetworkError> {
        let mut error = 0;
        ffi::drop_outbound_packet(self.native(), packet_id, &mut error)
            .then_some(())
            .ok_or_else(|| native_error(error))
    }

    /// Inject bytes from an external TCP peer into a connected client socket.
    /// The addresses must exactly match that client's remote and local pair.
    pub fn inject_tcp_data(
        &self,
        source: NetworkAddress,
        destination: NetworkAddress,
        data: &[u8],
    ) -> Result<(), NetworkError> {
        let mut error = 0;
        ffi::inject_simulated_tcp_data(
            self.native(),
            &ip_bytes(source.ip),
            source.port,
            &ip_bytes(destination.ip),
            destination.port,
            data,
            &mut error,
        )
        .then_some(())
        .ok_or_else(|| native_error(error))
    }

    fn native(&self) -> &ffi::NativeSimulatedNetwork {
        self.0
            .native
            .as_ref()
            .expect("validated controlled network")
    }
}

fn take_packet(network: &ffi::NativeSimulatedNetwork) -> Option<OutboundPacket> {
    let packet = ffi::take_outbound_packet(network);
    let packet = packet.as_ref()?;
    Some(OutboundPacket {
        id: ffi::outbound_packet_id(packet),
        kind: match ffi::outbound_packet_kind(packet) {
            0 => OutboundKind::Udp,
            1 => OutboundKind::TcpConnect,
            2 => OutboundKind::TcpData,
            _ => unreachable!("native adapter returned an invalid packet kind"),
        },
        source: address_from_native(
            ffi::outbound_packet_source_ip(packet),
            ffi::outbound_packet_source_port(packet),
        ),
        destination: address_from_native(
            ffi::outbound_packet_destination_ip(packet),
            ffi::outbound_packet_destination_port(packet),
        ),
        payload: ffi::outbound_packet_payload(packet),
        deadline: Duration::from_micros(ffi::outbound_packet_deadline_us(packet) as u64),
    })
}

/// A registered local IP and its provider recipes.
///
/// The endpoint is sequence-bound; close it before dropping creator-sequence
/// state when deterministic shutdown ordering matters.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::NetworkEndpoint>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::NetworkEndpoint>();
/// ```
pub struct NetworkEndpoint {
    native: cxx::UniquePtr<ffi::NativeNetworkEndpoint>,
    network: NetworkRoot,
    identity: Rc<()>,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl NetworkEndpoint {
    pub fn network_manager(&self) -> Result<NetworkManagerProvider, NetworkError> {
        let native = ffi::new_network_manager_provider(self.native());
        if native.is_null() {
            Err(NetworkError::EndpointClosed)
        } else {
            Ok(NetworkManagerProvider {
                native,
                endpoint_identity: self.identity.clone(),
                _network: self.network.clone(),
                _creator_sequence: PhantomData,
            })
        }
    }

    pub fn packet_socket_factory(&self) -> Result<PacketSocketFactoryProvider, NetworkError> {
        let native = ffi::new_packet_socket_factory_provider(self.native());
        if native.is_null() {
            Err(NetworkError::EndpointClosed)
        } else {
            Ok(PacketSocketFactoryProvider {
                native,
                endpoint_identity: self.identity.clone(),
                _network: self.network.clone(),
                _creator_sequence: PhantomData,
            })
        }
    }

    pub fn bind_udp(&self, port: u16) -> Result<SimulatedUdpSocket, NetworkError> {
        self.packet_socket_factory()?.bind_udp(port)
    }

    pub fn close(&mut self) {
        if let Some(native) = self.native.as_mut() {
            ffi::close_network_endpoint(native);
        }
    }

    fn native(&self) -> &ffi::NativeNetworkEndpoint {
        self.native.as_ref().expect("validated network endpoint")
    }
}

/// A recipe for constructing the native production `NetworkManager` adapter.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::NetworkManagerProvider>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::NetworkManagerProvider>();
/// ```
pub struct NetworkManagerProvider {
    native: cxx::UniquePtr<ffi::NativeNetworkManagerProvider>,
    endpoint_identity: Rc<()>,
    _network: NetworkRoot,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl NetworkManagerProvider {
    pub(crate) fn same_endpoint(&self, sockets: &PacketSocketFactoryProvider) -> bool {
        Rc::ptr_eq(&self.endpoint_identity, &sockets.endpoint_identity)
    }

    pub(crate) fn is_controlled(&self) -> bool {
        self._network.is_controlled()
    }

    pub(crate) fn is_controlled_by(&self, driver: &ControlledPeerDriver) -> bool {
        self._network.is_controlled_by(driver)
    }

    pub fn is_available(&self) -> bool {
        ffi::network_manager_provider_is_valid(self.native())
    }

    pub(crate) fn native(&self) -> &ffi::NativeNetworkManagerProvider {
        self.native
            .as_ref()
            .expect("validated network manager provider")
    }
}

/// A recipe for constructing the native production `PacketSocketFactory` adapter.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::PacketSocketFactoryProvider>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::PacketSocketFactoryProvider>();
/// ```
pub struct PacketSocketFactoryProvider {
    native: cxx::UniquePtr<ffi::NativePacketSocketFactoryProvider>,
    endpoint_identity: Rc<()>,
    _network: NetworkRoot,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl PacketSocketFactoryProvider {
    pub(crate) fn is_controlled(&self) -> bool {
        self._network.is_controlled()
    }

    pub(crate) fn is_controlled_by(&self, driver: &ControlledPeerDriver) -> bool {
        self._network.is_controlled_by(driver)
    }

    pub fn supports_udp(&self) -> bool {
        ffi::packet_socket_factory_supports_udp(self.native())
    }

    /// Whether client-side TCP connection attempts can be externally driven.
    /// TCP listening and TLS are not implied.
    pub fn supports_tcp(&self) -> bool {
        ffi::packet_socket_factory_supports_tcp(self.native())
    }

    pub fn supports_dns(&self) -> bool {
        ffi::packet_socket_factory_supports_dns(self.native())
    }

    pub fn bind_udp(&self, port: u16) -> Result<SimulatedUdpSocket, NetworkError> {
        let mut error = 0;
        let native = ffi::create_simulated_udp_socket(self.native(), port, &mut error);
        if native.is_null() {
            Err(native_error(error))
        } else {
            Ok(SimulatedUdpSocket {
                native,
                _network: self._network.clone(),
                _creator_sequence: PhantomData,
            })
        }
    }

    /// Require client-side TCP; this does not assert TLS or listener support.
    pub fn require_tcp(&self) -> Result<(), NetworkError> {
        self.supports_tcp()
            .then_some(())
            .ok_or(NetworkError::UnsupportedTransport)
    }

    pub fn require_dns(&self) -> Result<(), NetworkError> {
        self.supports_dns()
            .then_some(())
            .ok_or(NetworkError::UnsupportedDns)
    }

    pub(crate) fn native(&self) -> &ffi::NativePacketSocketFactoryProvider {
        self.native
            .as_ref()
            .expect("validated packet socket factory provider")
    }
}

/// A sequence-bound proxy around the native `AsyncPacketSocket` adapter.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::SimulatedUdpSocket>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::SimulatedUdpSocket>();
/// ```
pub struct SimulatedUdpSocket {
    native: cxx::UniquePtr<ffi::NativeSimulatedUdpSocket>,
    _network: NetworkRoot,
    _creator_sequence: PhantomData<Rc<()>>,
}

impl SimulatedUdpSocket {
    pub fn local_address(&self) -> NetworkAddress {
        address_from_native(
            ffi::simulated_udp_local_ip(self.native()),
            ffi::simulated_udp_local_port(self.native()),
        )
    }

    pub fn send_to(
        &self,
        destination: NetworkAddress,
        payload: Vec<u8>,
    ) -> Result<(), NetworkError> {
        let mut error = 0;
        if ffi::simulated_udp_send_to(
            self.native(),
            &ip_bytes(destination.ip),
            destination.port,
            payload,
            &mut error,
        ) {
            Ok(())
        } else {
            Err(native_error(error))
        }
    }

    pub fn try_receive(&self) -> Option<ReceivedPacket> {
        let packet = ffi::simulated_udp_take_received(self.native());
        let packet = packet.as_ref()?;
        Some(ReceivedPacket {
            source: address_from_native(
                ffi::received_packet_source_ip(packet),
                ffi::received_packet_source_port(packet),
            ),
            payload: ffi::received_packet_payload(packet),
        })
    }

    pub fn close(&mut self) {
        if let Some(native) = self.native.as_mut() {
            ffi::close_simulated_udp_socket(native);
        }
    }

    fn native(&self) -> &ffi::NativeSimulatedUdpSocket {
        self.native
            .as_ref()
            .expect("validated simulated UDP socket")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkError {
    InvalidAddress,
    InvalidPort,
    AddressInUse,
    EndpointClosed,
    SocketClosed,
    PacketNotFound,
    DestinationUnavailable,
    UnsupportedTransport,
    UnsupportedDns,
    NativeConstructionFailed,
}

impl fmt::Display for NetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidAddress => "the address must be a resolved unicast IP",
            Self::InvalidPort => "the port must be nonzero",
            Self::AddressInUse => "the network address is already in use",
            Self::EndpointClosed => "the network endpoint is closed",
            Self::SocketClosed => "the packet socket is closed",
            Self::PacketNotFound => "the outbound packet is not awaiting a decision",
            Self::DestinationUnavailable => "the packet destination is unavailable",
            Self::UnsupportedTransport => "TCP packet sockets are unsupported",
            Self::UnsupportedDns => "DNS resolution is unsupported",
            Self::NativeConstructionFailed => "native network construction failed",
        })
    }
}

impl std::error::Error for NetworkError {}

fn validate_ip(ip: IpAddr) -> Result<(), NetworkError> {
    if ip.is_unspecified() || ip.is_multicast() {
        Err(NetworkError::InvalidAddress)
    } else {
        Ok(())
    }
}

fn ip_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    }
}

fn ip_from_native(bytes: Vec<u8>) -> IpAddr {
    match bytes.as_slice() {
        [a, b, c, d] => IpAddr::V4(Ipv4Addr::new(*a, *b, *c, *d)),
        bytes if bytes.len() == 16 => {
            let mut octets = [0; 16];
            octets.copy_from_slice(bytes);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => unreachable!("native adapter returned an invalid IP address"),
    }
}

fn address_from_native(ip: Vec<u8>, port: u16) -> NetworkAddress {
    NetworkAddress {
        ip: ip_from_native(ip),
        port,
    }
}

fn deliver(
    network: &ffi::NativeSimulatedNetwork,
    packet_id: u64,
    keep_pending: bool,
) -> Result<(), NetworkError> {
    let mut error = 0;
    ffi::deliver_outbound_packet(network, packet_id, keep_pending, &mut error)
        .then_some(())
        .ok_or_else(|| native_error(error))
}

fn native_error(error: u8) -> NetworkError {
    match error {
        1 => NetworkError::InvalidAddress,
        2 => NetworkError::InvalidPort,
        3 => NetworkError::AddressInUse,
        4 => NetworkError::EndpointClosed,
        5 => NetworkError::SocketClosed,
        6 => NetworkError::PacketNotFound,
        7 => NetworkError::DestinationUnavailable,
        8 => NetworkError::UnsupportedTransport,
        9 => NetworkError::UnsupportedDns,
        _ => NetworkError::NativeConstructionFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn shared_network_has_only_justified_positive_traits() {
        assert_send_sync::<SimulatedNetwork>();
    }

    #[test]
    fn address_validation_is_explicit() {
        assert_eq!(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1),
            Err(NetworkError::InvalidAddress)
        );
        assert_eq!(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Err(NetworkError::InvalidPort)
        );
    }
}
