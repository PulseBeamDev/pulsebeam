use pulsebeam_webrtc_sys::*;
use std::net::{Ipv4Addr, Ipv6Addr};

#[test]
fn native_network_identity_clone_and_ip_replacement_preserve_sdk_semantics() {
    let prefix = Ipv4Addr::new(10, 20, 30, 41);
    let mut network = Network::with_adapter(
        b"eth\0\xff",
        b"Ethernet detail\xff",
        prefix,
        23,
        NetworkAdapterType::Ethernet,
    );
    assert_eq!(network.name(), b"eth\0\xff");
    assert_eq!(network.description(), b"Ethernet detail\xff");
    assert_eq!(network.prefix(), prefix.into());
    assert_eq!(network.prefix_length(), 23);
    assert_eq!(network.family(), AddressFamily::Ipv4);
    assert_eq!(network.key(), Network::make_key(b"eth\0\xff", prefix, 23));
    assert_eq!(network.best_ip(), IpAddress::Unspecified);
    assert!(network.active());
    assert!(!network.ignored());
    assert_eq!(network.id(), 0);
    assert_eq!(network.scope_id(), 0);
    let a = InterfaceAddress::new(Ipv4Addr::new(10, 20, 30, 50), -7);
    let b = InterfaceAddress::new(Ipv4Addr::new(10, 20, 30, 60), 0x40);
    network.add_interface_address(a);
    network.add_interface_address(b);
    let shared = network.share();
    assert!(network.is_same_network(&shared));
    assert_eq!(network.best_ip(), a.address);
    // Native SetIPs detects membership, not ordering, but stores the new order.
    assert!(!network.set_ips(&[b, a], false));
    assert_eq!(shared.best_ip(), b.address);
    assert_eq!(network.ips(), vec![b, a]);
    // Its duplicate check is not a multiset comparison. Keep that native result.
    assert!(!network.set_ips(&[a, a], false));
    assert_eq!(network.ips(), vec![a, a]);
    assert!(network.set_ips(&[a, a], true));
    network.set_scope_id(i32::MIN);
    network.set_id(u16::MAX);
    network.set_preference(i32::MAX);
    network.set_active(false);
    network.set_ignored(true);
    network.set_adapter_type(NetworkAdapterType::Vpn).unwrap();
    network
        .set_underlying_type_for_vpn(NetworkAdapterType::Cellular5G)
        .unwrap();
    network
        .set_network_preference(NetworkPreference::NotPreferred)
        .unwrap();
    network
        .set_network_slice(NetworkSlice::UnifiedCommunications)
        .unwrap();
    let copied = network.clone();
    assert!(!network.is_same_network(&copied));
    drop(network);
    drop(shared);
    assert_eq!(copied.ips(), vec![a, a]);
    assert_eq!(copied.scope_id(), i32::MIN);
    assert_eq!(copied.id(), u16::MAX);
    assert_eq!(copied.preference(), i32::MAX);
    assert!(!copied.active());
    assert!(copied.ignored());
    assert!(copied.is_vpn());
    assert_eq!(
        copied.underlying_type_for_vpn(),
        NetworkAdapterType::Cellular5G
    );
    assert_eq!(copied.network_preference(), NetworkPreference::NotPreferred);
    assert_eq!(copied.network_slice(), NetworkSlice::UnifiedCommunications);
    let text = copied.to_native_debug_text();
    assert!(
        text.windows(b"VPN/Cellular5G".len())
            .any(|window| window == b"VPN/Cellular5G")
    );
    let mut copied = copied;
    copied.set_adapter_type(NetworkAdapterType::Wifi).unwrap();
    assert_eq!(
        copied.underlying_type_for_vpn(),
        NetworkAdapterType::Unknown
    );
    copied.clear_ips();
    assert_eq!(copied.best_ip(), IpAddress::Unspecified);
}

#[test]
fn native_ipv6_selection_uses_flags_and_link_local_before_ula() {
    let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
    let mut network = Network::new(b"v6", b"IPv6", prefix, 64);
    let ula = InterfaceAddress::parse(b"fd00::1", 0).unwrap();
    let link = InterfaceAddress::parse(b"fe80::1", 0).unwrap();
    let deprecated =
        InterfaceAddress::parse(b"2001:db8::2", InterfaceAddress::FLAG_DEPRECATED).unwrap();
    let global = InterfaceAddress::parse(b"2001:db8::3", 0).unwrap();
    let temporary =
        InterfaceAddress::parse(b"2001:db8::4", InterfaceAddress::FLAG_TEMPORARY).unwrap();
    network.set_ips(&[ula, link, deprecated], false);
    assert_eq!(network.best_ip(), link.address);
    network.add_interface_address(global);
    assert_eq!(network.best_ip(), global.address);
    network.add_interface_address(temporary);
    assert_eq!(network.best_ip(), temporary.address);
    assert_eq!(network.ips()[2], deprecated);
}

#[test]
fn native_cost_uses_retained_selected_trials_and_rejects_only_reached_vpn_abort() {
    let mut enabled = Environment::builder().field_trials(FieldTrials::parse(
        "WebRTC-UseDifferentiatedCellularCosts/Enabled/WebRTC-AddNetworkCostToVpn/Enabled/WebRTC-UnifiedCommunications/Enabled/").unwrap()).build().unwrap();
    let selected = enabled.field_trials();
    enabled.close();
    let disabled = Environment::builder()
        .field_trials(FieldTrials::default())
        .build()
        .unwrap()
        .field_trials();
    let mut network = Network::with_adapter(
        b"vpn",
        b"tunnel detail",
        Ipv4Addr::UNSPECIFIED,
        0,
        NetworkAdapterType::Vpn,
    );
    network
        .set_underlying_type_for_vpn(NetworkAdapterType::Cellular5G)
        .unwrap();
    network
        .set_network_slice(NetworkSlice::UnifiedCommunications)
        .unwrap();
    assert_eq!(network.cost(&selected), Ok(249));
    assert_eq!(network.cost(&disabled), Ok(900));
    assert_eq!(
        Network::guess_adapter_from_cost(249),
        NetworkCostGuess {
            adapter: NetworkAdapterType::Cellular5G,
            vpn: true,
            slice: NetworkSlice::UnifiedCommunications
        }
    );
    assert_eq!(
        Network::guess_adapter_from_cost(i32::MIN),
        NetworkCostGuess {
            adapter: NetworkAdapterType::Unknown,
            vpn: false,
            slice: NetworkSlice::None
        }
    );
    network
        .set_underlying_type_for_vpn(NetworkAdapterType::Vpn)
        .unwrap();
    assert_eq!(network.underlying_type_for_vpn(), NetworkAdapterType::Vpn);
    assert_eq!(
        network.cost(&selected),
        Err(NetworkValueError::VpnUnderlyingVpn)
    );
    network
        .set_adapter_type(NetworkAdapterType::Cellular4G)
        .unwrap();
    assert!(network.is_cellular());
    assert_eq!(network.cost(&selected), Ok(500));
    assert!(NetworkAdapterType::Cellular2G.is_cellular());
    assert!(!NetworkAdapterType::Vpn.is_cellular());
}

#[test]
fn native_mask_key_and_adapter_helpers_keep_unnormalized_values() {
    let address = Ipv4Addr::new(10, 2, 3, 4);
    let mask = IpNetworkMask::new(address, -1);
    assert_eq!(mask.address(), address.into());
    assert_eq!(mask.prefix_length(), -1);
    assert_eq!(mask, mask.clone());
    assert_ne!(mask, IpNetworkMask::new(address, 24));
    std::thread::spawn(move || {
        assert_eq!(mask.address(), address.into());
    })
    .join()
    .unwrap();
    let network = Network::new(b"odd", b"odd", IpAddress::Unspecified, i32::MIN);
    assert_eq!(network.prefix_length(), i32::MIN);
    assert_eq!(network.family(), AddressFamily::Unspecified);
    assert_eq!(
        network.key(),
        Network::make_key(b"odd", IpAddress::Unspecified, i32::MIN)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"wlan12"),
        Ok(NetworkAdapterType::Wifi)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"tun1"),
        Ok(NetworkAdapterType::Vpn)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"wlan\xff"),
        Ok(NetworkAdapterType::Unknown)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"wlan12\x80"),
        Err(NetworkValueError::InvalidClassifierSuffix)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"wlanx\x80"),
        Ok(NetworkAdapterType::Unknown)
    );
    assert_eq!(
        NetworkAdapterType::from_interface_name(b"\x80wlan12"),
        Ok(NetworkAdapterType::Unknown)
    );
}
