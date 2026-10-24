//! Owned native IP/socket values. Text results are bytes so hostnames need not be UTF-8.
use crate::ffi;
use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AddressFamily {
    Unspecified,
    Ipv4,
    Ipv6,
    Other(i32),
}
impl AddressFamily {
    pub fn from_native_value(value: i32) -> Self {
        if value == ffi::address_family(0) {
            Self::Unspecified
        } else if value == ffi::address_family(1) {
            Self::Ipv4
        } else if value == ffi::address_family(2) {
            Self::Ipv6
        } else {
            Self::Other(value)
        }
    }
    pub fn native_value(self) -> i32 {
        match self {
            Self::Unspecified => ffi::address_family(0),
            Self::Ipv4 => ffi::address_family(1),
            Self::Ipv6 => ffi::address_family(2),
            Self::Other(value) => value,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IpAddress {
    #[default]
    Unspecified,
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}
impl From<IpAddr> for IpAddress {
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(v) => Self::V4(v),
            IpAddr::V6(v) => Self::V6(v),
        }
    }
}
impl From<Ipv4Addr> for IpAddress {
    fn from(value: Ipv4Addr) -> Self {
        Self::V4(value)
    }
}
impl From<Ipv6Addr> for IpAddress {
    fn from(value: Ipv6Addr) -> Self {
        Self::V6(value)
    }
}
impl IpAddress {
    pub(crate) fn bytes(self) -> Vec<u8> {
        match self {
            Self::Unspecified => Vec::new(),
            Self::V4(v) => v.octets().to_vec(),
            Self::V6(v) => v.octets().to_vec(),
        }
    }
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        match bytes.len() {
            0 => Self::Unspecified,
            4 => Self::V4(Ipv4Addr::from(
                <[u8; 4]>::try_from(bytes.as_slice()).unwrap(),
            )),
            16 => Self::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(bytes.as_slice()).unwrap(),
            )),
            _ => panic!("invalid native IP byte length"),
        }
    }
    pub fn from_host_order_integer(value: u32) -> Self {
        Self::V4(Ipv4Addr::from(value))
    }
    pub fn parse(text: impl AsRef<[u8]>) -> Option<Self> {
        let bytes = ffi::ip_address_parse(text.as_ref());
        (!bytes.is_empty()).then(|| Self::from_bytes(bytes))
    }
    pub fn any(family: AddressFamily) -> Self {
        Self::from_bytes(ffi::ip_address_special(family.native_value(), false))
    }
    pub fn loopback(family: AddressFamily) -> Self {
        Self::from_bytes(ffi::ip_address_special(family.native_value(), true))
    }
    pub fn family(self) -> AddressFamily {
        match self {
            Self::Unspecified => AddressFamily::Unspecified,
            Self::V4(_) => AddressFamily::Ipv4,
            Self::V6(_) => AddressFamily::Ipv6,
        }
    }
    pub fn ipv4_address(self) -> Option<Ipv4Addr> {
        if let Self::V4(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn ipv6_address(self) -> Option<Ipv6Addr> {
        if let Self::V6(value) = self {
            Some(value)
        } else {
            None
        }
    }
    fn info(self, kind: u8) -> i64 {
        ffi::ip_address_info(&self.bytes(), kind)
    }
    pub fn size(self) -> usize {
        self.info(1) as usize
    }
    pub fn overhead(self) -> i32 {
        self.info(2) as i32
    }
    pub fn host_order_integer(self) -> u32 {
        self.info(3) as u32
    }
    pub fn precedence(self) -> i32 {
        self.info(4) as i32
    }
    pub fn count_mask_bits(self) -> i32 {
        self.info(5) as i32
    }
    pub fn native_hash(self) -> u64 {
        ffi::ip_address_hash(&self.bytes())
    }
    pub fn to_text(self) -> Vec<u8> {
        ffi::ip_address_text(&self.bytes(), 0, 0)
    }
    pub fn to_sensitive_text(self) -> Vec<u8> {
        ffi::ip_address_text(&self.bytes(), 1, 0)
    }
    pub fn to_native_debug_text(self) -> Vec<u8> {
        ffi::ip_address_text(&self.bytes(), 3, 0)
    }
    fn transform(self, operation: u8, length: i32) -> Self {
        Self::from_bytes(ffi::ip_address_transform(&self.bytes(), operation, length))
    }
    pub fn normalized(self) -> Self {
        self.transform(0, 0)
    }
    pub fn normalize_embedded_ipv4(self) -> Self {
        self.transform(1, 0)
    }
    pub fn as_ipv6_address(self) -> Self {
        self.transform(2, 0)
    }
    pub fn truncate(self, bits: i32) -> Self {
        self.transform(3, bits)
    }
    fn predicate(self, bit: u32) -> bool {
        ffi::ip_address_predicates(&self.bytes()) & (1 << bit) != 0
    }
    pub fn is_nil(self) -> bool {
        self.predicate(0)
    }
    pub fn is_any(self) -> bool {
        self.predicate(1)
    }
    pub fn is_loopback(self) -> bool {
        self.predicate(2)
    }
    pub fn is_link_local(self) -> bool {
        self.predicate(3)
    }
    pub fn is_private_network(self) -> bool {
        self.predicate(4)
    }
    pub fn is_shared_network(self) -> bool {
        self.predicate(5)
    }
    pub fn is_private(self) -> bool {
        self.predicate(6)
    }
    pub fn is_unspecified(self) -> bool {
        self.predicate(7)
    }
    pub fn is_6bone(self) -> bool {
        self.predicate(8)
    }
    pub fn is_6to4(self) -> bool {
        self.predicate(9)
    }
    pub fn is_isatap(self) -> bool {
        self.predicate(10)
    }
    pub fn is_mac_based(self) -> bool {
        self.predicate(11)
    }
    pub fn is_nat64(self) -> bool {
        self.predicate(12)
    }
    pub fn is_site_local(self) -> bool {
        self.predicate(13)
    }
    pub fn is_teredo(self) -> bool {
        self.predicate(14)
    }
    pub fn is_ula(self) -> bool {
        self.predicate(15)
    }
    pub fn is_v4_compatibility(self) -> bool {
        self.predicate(16)
    }
    pub fn is_v4_mapped(self) -> bool {
        self.predicate(17)
    }
    pub fn from_address_info(address: Option<&NativeSockAddr>) -> Option<Self> {
        let bytes = ffi::ip_address_from_storage(&address?.bytes);
        (!bytes.is_empty()).then(|| Self::from_bytes(bytes))
    }
}
impl Ord for IpAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        ffi::ip_address_compare(&self.bytes(), &other.bytes()).cmp(&0)
    }
}
impl PartialOrd for IpAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Hash for IpAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.native_hash().hash(state);
    }
}

/// Native IPv6 flags remain signed and open to unknown bits on every family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceAddress {
    pub address: IpAddress,
    pub ipv6_flags: i32,
}
impl InterfaceAddress {
    pub const FLAG_NONE: i32 = 0;
    pub const FLAG_TEMPORARY: i32 = 1;
    pub const FLAG_DEPRECATED: i32 = 2;
    pub fn new(address: impl Into<IpAddress>, ipv6_flags: i32) -> Self {
        Self {
            address: address.into(),
            ipv6_flags,
        }
    }
    pub fn parse(text: impl AsRef<[u8]>, flags: i32) -> Option<Self> {
        Some(Self::new(IpAddress::parse(text)?, flags))
    }
    pub fn to_text(self) -> Vec<u8> {
        ffi::ip_address_text(&self.address.bytes(), 2, self.ipv6_flags)
    }
}
impl Default for InterfaceAddress {
    fn default() -> Self {
        Self::new(IpAddress::default(), 0)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressParseError {
    EmptyInput,
    PortOutOfRange,
}
impl std::fmt::Display for AddressParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyInput => "native socket parser requires nonempty input",
            Self::PortOutOfRange => "parsed port is outside the native u16 domain",
        })
    }
}
impl std::error::Error for AddressParseError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IpAddressType {
    Unknown,
    Any,
    Loopback,
    Private,
    Public,
    Other(i32),
}

pub struct SocketAddress {
    inner: cxx::UniquePtr<ffi::NativeSocketAddress>,
}
// Owns a deep-copy std::string/IP value, never a socket, thread, or borrowed SDK
// pointer. Native reads are const; every mutation requires exclusive Rust access.
unsafe impl Send for SocketAddress {}
unsafe impl Sync for SocketAddress {}
impl SocketAddress {
    pub(crate) fn from_native(inner: cxx::UniquePtr<ffi::NativeSocketAddress>) -> Self {
        assert!(!inner.is_null());
        Self { inner }
    }
    pub(crate) fn native(&self) -> &ffi::NativeSocketAddress {
        self.inner.as_ref().unwrap()
    }
    pub fn new(ip: impl Into<IpAddress>, port: u16) -> Self {
        Self::from_native(ffi::socket_address_new(&ip.into().bytes(), port))
    }
    pub fn from_host(host: impl AsRef<[u8]>, port: u16) -> Self {
        Self::from_native(ffi::socket_address_host(host.as_ref(), port))
    }
    pub fn from_host_order_integer(ip: u32, port: u16) -> Self {
        Self::new(IpAddress::from_host_order_integer(ip), port)
    }
    pub fn empty(family: AddressFamily) -> Self {
        Self::from_native(ffi::socket_address_empty(family.native_value()))
    }
    pub fn clear(&mut self) {
        ffi::socket_address_clear(self.inner.pin_mut());
    }
    pub fn set_ip(&mut self, ip: impl Into<IpAddress>) {
        ffi::socket_address_set_ip(self.inner.pin_mut(), &ip.into().bytes(), false);
    }
    pub fn set_host(&mut self, host: impl AsRef<[u8]>) {
        ffi::socket_address_set_host(self.inner.pin_mut(), host.as_ref());
    }
    pub fn set_resolved_ip(&mut self, ip: impl Into<IpAddress>) {
        ffi::socket_address_set_ip(self.inner.pin_mut(), &ip.into().bytes(), true);
    }
    pub fn set_port(&mut self, port: u16) {
        ffi::socket_address_set_port(self.inner.pin_mut(), port);
    }
    pub fn set_scope_id(&mut self, scope: i32) {
        ffi::socket_address_set_scope(self.inner.pin_mut(), scope);
    }
    /// Preserves native parser quirks, including partial numeric port parses and
    /// a missing close bracket succeeding without mutation. Invalid CHECK inputs
    /// return an error before native mutation.
    pub fn parse_into(&mut self, text: impl AsRef<[u8]>) -> Result<bool, AddressParseError> {
        match ffi::socket_address_parse(self.inner.pin_mut(), text.as_ref()) {
            0 => Ok(false),
            1 => Ok(true),
            2 => Err(AddressParseError::EmptyInput),
            _ => Err(AddressParseError::PortOutOfRange),
        }
    }
    pub fn parse(text: impl AsRef<[u8]>) -> Result<Option<Self>, AddressParseError> {
        let mut value = Self::default();
        Ok(value.parse_into(text)?.then_some(value))
    }
    fn bytes(&self, kind: u8) -> Vec<u8> {
        ffi::socket_address_bytes(self.native(), kind)
    }
    fn info(&self, kind: u8) -> i64 {
        ffi::socket_address_info(self.native(), kind)
    }
    pub fn hostname(&self) -> Vec<u8> {
        self.bytes(0)
    }
    pub fn ip_address(&self) -> IpAddress {
        IpAddress::from_bytes(self.bytes(1))
    }
    pub fn family(&self) -> AddressFamily {
        self.ip_address().family()
    }
    pub fn host_order_integer(&self) -> u32 {
        self.info(1) as u32
    }
    pub fn port(&self) -> u16 {
        self.info(2) as u16
    }
    pub fn scope_id(&self) -> i32 {
        self.info(3) as i32
    }
    pub fn ip_address_type(&self) -> IpAddressType {
        match self.info(4) as i32 {
            0 => IpAddressType::Unknown,
            1 => IpAddressType::Any,
            2 => IpAddressType::Loopback,
            3 => IpAddressType::Private,
            4 => IpAddressType::Public,
            v => IpAddressType::Other(v),
        }
    }
    pub fn host_as_uri(&self) -> Vec<u8> {
        self.bytes(2)
    }
    pub fn host_as_sensitive_uri(&self) -> Vec<u8> {
        self.bytes(3)
    }
    pub fn port_as_text(&self) -> Vec<u8> {
        self.bytes(4)
    }
    pub fn to_text(&self) -> Vec<u8> {
        self.bytes(5)
    }
    pub fn to_sensitive_text(&self) -> Vec<u8> {
        self.bytes(6)
    }
    pub fn to_sensitive_name_and_address(&self) -> Vec<u8> {
        self.bytes(7)
    }
    fn predicate(&self, bit: u32) -> bool {
        ffi::socket_address_predicates(self.native()) & (1 << bit) != 0
    }
    pub fn is_nil(&self) -> bool {
        self.predicate(0)
    }
    pub fn is_complete(&self) -> bool {
        self.predicate(1)
    }
    pub fn is_any_ip(&self) -> bool {
        self.predicate(2)
    }
    pub fn is_loopback_ip(&self) -> bool {
        self.predicate(3)
    }
    pub fn is_private_ip(&self) -> bool {
        self.predicate(4)
    }
    pub fn is_unresolved_ip(&self) -> bool {
        self.predicate(5)
    }
    pub fn equal_ips(&self, other: &Self) -> bool {
        ffi::socket_address_equal_ip(self.native(), other.native())
    }
    pub fn equal_ports(&self, other: &Self) -> bool {
        ffi::socket_address_equal_port(self.native(), other.native())
    }
    pub fn native_hash(&self) -> u64 {
        ffi::socket_address_hash(self.native())
    }
    fn sockaddr(&self, kind: u8) -> NativeSockAddr {
        NativeSockAddr {
            bytes: ffi::socket_address_storage(self.native(), kind),
            ipv4: kind == 2,
            written: ffi::socket_address_storage_written(self.native(), kind),
        }
    }
    pub fn to_sockaddr_in(&self) -> NativeSockAddr {
        self.sockaddr(2)
    }
    pub fn to_sockaddr_storage(&self) -> NativeSockAddr {
        self.sockaddr(0)
    }
    pub fn to_dual_stack_sockaddr_storage(&self) -> NativeSockAddr {
        self.sockaddr(1)
    }
    pub fn from_sockaddr(address: &NativeSockAddr) -> Option<Self> {
        let value = ffi::socket_address_from_storage(&address.bytes, address.ipv4);
        (!value.is_null()).then(|| Self::from_native(value))
    }
    /// Leaves the existing value unchanged on native read failure.
    pub fn read_sockaddr(&mut self, address: &NativeSockAddr) -> bool {
        if let Some(value) = Self::from_sockaddr(address) {
            *self = value;
            true
        } else {
            false
        }
    }
}
impl Default for SocketAddress {
    fn default() -> Self {
        Self::new(IpAddress::Unspecified, 0)
    }
}
impl Clone for SocketAddress {
    fn clone(&self) -> Self {
        Self::from_native(ffi::socket_address_clone(self.native()))
    }
}
impl PartialEq for SocketAddress {
    fn eq(&self, other: &Self) -> bool {
        ffi::socket_address_compare(self.native(), other.native()) == 0
    }
}
impl Eq for SocketAddress {}
impl Ord for SocketAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        ffi::socket_address_compare(self.native(), other.native()).cmp(&0)
    }
}
impl PartialOrd for SocketAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Hash for SocketAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.native_hash().hash(state);
    }
}
impl std::fmt::Debug for SocketAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketAddress")
            .field("hostname", &self.hostname())
            .field("ip", &self.ip_address())
            .field("port", &self.port())
            .field("scope_id", &self.scope_id())
            .finish()
    }
}
/// Initialized native ABI bytes, not a borrowed sockaddr pointer. These bytes
/// are target-specific and are not a portable serialization format. Written size
/// is zero for failed native storage conversion; bytes still preserve its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSockAddr {
    bytes: Vec<u8>,
    ipv4: bool,
    written: usize,
}
impl NativeSockAddr {
    pub fn from_native_bytes(bytes: Vec<u8>, ipv4: bool) -> Option<Self> {
        (bytes.len() == ffi::socket_address_native_size(if ipv4 { 2 } else { 0 })).then_some(Self {
            written: bytes.len(),
            bytes,
            ipv4,
        })
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn is_sockaddr_in(&self) -> bool {
        self.ipv4
    }
    pub fn written_size(&self) -> usize {
        self.written
    }
}
