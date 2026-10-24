use pulsebeam_webrtc_sys::*;
use std::net::{Ipv4Addr, Ipv6Addr};

#[test]
fn native_ip_value_families_and_interface_flags_are_lossless() {
    let nil = IpAddress::default();
    assert!(nil.is_nil());
    assert!(nil.is_unspecified());
    assert_eq!(nil.size(), 0);
    assert_eq!(nil.to_native_debug_text(), b"unspecified");
    assert_eq!(IpAddress::any(AddressFamily::Other(-1)), nil);
    assert!(IpAddress::parse(b"invalid").is_none());
    assert!(IpAddress::from_address_info(None).is_none());
    let v4 = IpAddress::from_host_order_integer(0xc0000201);
    assert_eq!(v4, IpAddress::from(Ipv4Addr::new(192, 0, 2, 1)));
    assert_eq!(v4.host_order_integer(), 0xc0000201);
    assert_eq!(v4.size(), 4);
    assert_eq!(v4.overhead(), 20);
    assert_eq!(v4.to_text(), b"192.0.2.1");
    assert_eq!(v4.to_sensitive_text(), b"192.0.2.x");
    assert_eq!(v4.to_native_debug_text(), b"ipv4:192.0.2.1");
    assert!(v4.ipv6_address().is_none());
    let mapped = v4.as_ipv6_address();
    assert_eq!(mapped.family(), AddressFamily::Ipv6);
    assert!(mapped.is_v4_mapped());
    assert_eq!(mapped.normalized(), v4);
    assert_eq!(mapped.normalize_embedded_ipv4(), v4);
    assert_ne!(mapped, v4);
    assert_eq!(mapped.native_hash(), v4.native_hash());
    assert_eq!(mapped.host_order_integer(), 0);
    assert_eq!(mapped.size(), 16);
    assert_eq!(mapped.overhead(), 40);
    assert_eq!(v4.truncate(-1), nil);
    assert_eq!(v4.truncate(0), IpAddress::from(Ipv4Addr::UNSPECIFIED));
    assert_eq!(
        v4.truncate(24),
        IpAddress::from(Ipv4Addr::new(192, 0, 2, 0))
    );
    assert_eq!(v4.truncate(i32::MAX), v4);
    let mask = IpAddress::parse("ffff:ffff:ffff:ffff::").unwrap();
    assert_eq!(mask.count_mask_bits(), 64);
    assert_eq!(mask.truncate(48).count_mask_bits(), 48);
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        assert!(IpAddress::any(family).is_any());
        assert!(IpAddress::loopback(family).is_loopback());
    }
    for flags in [0, 1, 2, 3, i32::MIN, i32::MAX, -1] {
        let interface = InterfaceAddress::parse("::1", flags).unwrap();
        assert_eq!(interface.address, IpAddress::from(Ipv6Addr::LOCALHOST));
        assert_eq!(interface.ipv6_flags, flags);
        assert!(interface.to_text().starts_with(b"::1|flags:0x"));
        assert_ne!(
            interface,
            InterfaceAddress::new(interface.address, flags ^ 1)
        );
    }
    let private = IpAddress::parse("192.168.1.2").unwrap();
    assert!(private.is_private_network());
    assert!(private.is_private());
    let shared = IpAddress::parse("100.64.1.2").unwrap();
    assert!(shared.is_shared_network());
    let link = IpAddress::parse("fe80::1").unwrap();
    assert!(link.is_link_local());
    let _ = (
        link.precedence(),
        link.is_6bone(),
        link.is_6to4(),
        link.is_isatap(),
        link.is_mac_based(),
        link.is_nat64(),
        link.is_site_local(),
        link.is_teredo(),
        link.is_ula(),
        link.is_v4_compatibility(),
    );
}

#[test]
fn native_socket_values_preserve_hostname_bytes_scope_and_clone_state() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<SocketAddress>();
    let nil = SocketAddress::default();
    assert!(nil.is_nil());
    let host = b"host\0with\xffbytes";
    let mut address = SocketAddress::from_host(host, u16::MAX);
    assert_eq!(address.hostname(), host);
    assert_eq!(address.host_as_uri(), host);
    assert!(address.is_unresolved_ip());
    assert_eq!(address.ip_address_type(), IpAddressType::Unknown);
    address.set_resolved_ip(Ipv6Addr::LOCALHOST);
    address.set_scope_id(-17);
    assert_eq!(address.hostname(), host);
    assert_eq!(address.host_as_uri(), host);
    assert_eq!(address.scope_id(), -17);
    assert_eq!(address.ip_address_type(), IpAddressType::Loopback);
    assert!(address.is_complete());
    let copy = address.clone();
    address.set_host("192.0.2.9");
    assert_eq!(address.scope_id(), 0);
    assert_eq!(address.host_as_uri(), b"192.0.2.9");
    let ip = copy.ip_address();
    let port = copy.port();
    let equal = SocketAddress::new(ip, port);
    assert!(copy.equal_ips(&equal));
    assert!(copy.equal_ports(&equal));
    assert_eq!(copy, equal); // Native equality omits hostname and scope here.
    assert_eq!(copy.native_hash(), equal.native_hash());
    std::thread::spawn(move || {
        assert_eq!(copy.hostname(), host);
        assert_eq!(copy.scope_id(), -17);
        assert_eq!(copy.port_as_text(), b"65535");
        assert!(!copy.to_sensitive_name_and_address().is_empty());
        assert!(!copy.to_sensitive_text().is_empty());
        assert_eq!(copy.host_as_sensitive_uri(), host);
        assert!(copy.is_loopback_ip());
        assert!(copy.is_private_ip());
    })
    .join()
    .unwrap();
    address.clear();
    assert_eq!(address, nil);
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        let empty = SocketAddress::empty(family);
        assert_eq!(empty.family(), family);
        assert!(empty.is_any_ip());
        assert_eq!(empty.ip_address_type(), IpAddressType::Any);
    }
}

#[test]
fn native_socket_parse_checks_and_sockaddr_roundtrips_preserve_native_outcomes() {
    let mut address = SocketAddress::new(Ipv4Addr::new(192, 0, 2, 1), 443);
    assert_eq!(address.parse_into(""), Err(AddressParseError::EmptyInput));
    assert_eq!(
        address.parse_into("example:65536"),
        Err(AddressParseError::PortOutOfRange)
    );
    assert_eq!(address.port(), 443);
    assert!(!address.parse_into("no-colon").unwrap());
    assert!(address.parse_into("[missing-bracket").unwrap());
    assert_eq!(address.port(), 443); // Pinned native parser succeeds without mutation.
    let quirky = SocketAddress::parse("example:12trailing").unwrap().unwrap();
    assert_eq!(quirky.port(), 12);
    let invalid_numeric = SocketAddress::parse("example:junk").unwrap().unwrap();
    assert_eq!(invalid_numeric.port(), 0);
    let v6 = SocketAddress::parse("[fe80::1234]:123").unwrap().unwrap();
    assert_eq!(v6.to_text(), b"[fe80::1234]:123");
    for mut value in [address, v6] {
        value.set_scope_id(-7);
        let storage = value.to_sockaddr_storage();
        assert!(storage.written_size() > 0);
        let roundtrip = SocketAddress::from_sockaddr(&storage).unwrap();
        assert_eq!(value, roundtrip);
        assert_eq!(
            roundtrip.scope_id(),
            if value.family() == AddressFamily::Ipv6 {
                -7
            } else {
                0
            }
        );
        assert_eq!(
            IpAddress::from_address_info(Some(&storage)),
            Some(value.ip_address())
        );
        let raw = NativeSockAddr::from_native_bytes(storage.as_bytes().to_vec(), false).unwrap();
        assert_eq!(SocketAddress::from_sockaddr(&raw).unwrap(), value);
        let dual = SocketAddress::from_sockaddr(&value.to_dual_stack_sockaddr_storage()).unwrap();
        assert_eq!(dual.family(), AddressFamily::Ipv6);
        assert_eq!(
            dual.ip_address().normalized(),
            value.ip_address().normalized()
        );
        let v4 = value.to_sockaddr_in();
        if value.family() == AddressFamily::Ipv4 {
            assert_eq!(SocketAddress::from_sockaddr(&v4).unwrap(), value);
        } else {
            assert!(SocketAddress::from_sockaddr(&v4).is_none());
        }
    }
    let storage = SocketAddress::default().to_sockaddr_storage();
    assert_eq!(storage.written_size(), 0);
    assert!(SocketAddress::from_sockaddr(&storage).is_none());
    let mut value = SocketAddress::from_host("retained", 7);
    assert!(!value.read_sockaddr(&storage));
    assert_eq!(value.hostname(), b"retained");
    assert!(NativeSockAddr::from_native_bytes(vec![], false).is_none());
}
