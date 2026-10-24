use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use pulsebeam_webrtc_sys::{ManualClock, NetworkAddress, NetworkError, SimulatedNetwork};

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 42, 0, last))
}

#[test]
fn packets_move_only_after_explicit_ordered_delivery() {
    let clock = ManualClock::new(Duration::from_secs(7)).unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let alice = network.register_endpoint(ip(1)).unwrap();
    let bob = network.register_endpoint(ip(2)).unwrap();
    assert!(alice.network_manager().unwrap().is_available());
    let factory = alice.packet_socket_factory().unwrap();
    assert!(factory.supports_udp());
    assert!(!factory.supports_tcp());
    assert!(!factory.supports_dns());
    assert_eq!(
        factory.require_tcp(),
        Err(NetworkError::UnsupportedTransport)
    );
    assert_eq!(factory.require_dns(), Err(NetworkError::UnsupportedDns));

    let alice_socket = alice.bind_udp(4000).unwrap();
    let bob_socket = bob.bind_udp(5000).unwrap();
    alice_socket
        .send_to(bob_socket.local_address(), b"first".to_vec())
        .unwrap();
    clock.advance(Duration::from_millis(3)).unwrap();
    alice_socket
        .send_to(bob_socket.local_address(), b"second".to_vec())
        .unwrap();

    let first = network.next_packet().unwrap();
    let second = network.next_packet().unwrap();
    assert!(first.id < second.id);
    assert_eq!(first.deadline, Duration::from_secs(7));
    assert_eq!(second.deadline, Duration::from_millis(7003));
    assert_eq!(first.source, alice_socket.local_address());
    assert_eq!(first.destination, bob_socket.local_address());
    assert_eq!(first.payload, b"first");
    assert!(bob_socket.try_receive().is_none());

    network.deliver_copy(second.id).unwrap();
    network.deliver(second.id).unwrap();
    network.deliver(first.id).unwrap();
    assert_eq!(bob_socket.try_receive().unwrap().payload, b"second");
    assert_eq!(bob_socket.try_receive().unwrap().payload, b"second");
    assert_eq!(bob_socket.try_receive().unwrap().payload, b"first");
    assert!(bob_socket.try_receive().is_none());
}

#[test]
fn drops_conflicts_and_closed_destinations_are_explicit() {
    let clock = ManualClock::new(Duration::ZERO).unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let alice = network.register_endpoint(ip(1)).unwrap();
    assert_eq!(
        network.register_endpoint(ip(1)).err(),
        Some(NetworkError::AddressInUse)
    );
    let mut bob = network.register_endpoint(ip(2)).unwrap();
    let alice_socket = alice.bind_udp(4000).unwrap();
    assert_eq!(alice.bind_udp(4000).err(), Some(NetworkError::AddressInUse));
    let bob_socket = bob.bind_udp(5000).unwrap();

    alice_socket
        .send_to(bob_socket.local_address(), vec![1])
        .unwrap();
    let dropped = network.next_packet().unwrap();
    network.drop_packet(dropped.id).unwrap();
    assert_eq!(
        network.deliver(dropped.id),
        Err(NetworkError::PacketNotFound)
    );
    assert!(bob_socket.try_receive().is_none());

    alice_socket
        .send_to(bob_socket.local_address(), vec![2])
        .unwrap();
    let queued = network.next_packet().unwrap();
    bob.close();
    bob.close();
    assert_eq!(
        network.deliver(queued.id),
        Err(NetworkError::PacketNotFound)
    );
    assert!(bob_socket.try_receive().is_none());
    assert_eq!(
        alice_socket.send_to(NetworkAddress::new(ip(2), 5000).unwrap(), vec![3]),
        Ok(())
    );
    let unavailable = network.next_packet().unwrap();
    assert_eq!(
        network.deliver(unavailable.id),
        Err(NetworkError::DestinationUnavailable)
    );
}

#[test]
fn repeated_create_close_and_drop_quiesces_callbacks() {
    for iteration in 1..=128_u16 {
        let clock = ManualClock::new(Duration::ZERO).unwrap();
        let network = SimulatedNetwork::new(&clock).unwrap();
        let alice = network.register_endpoint(ip(1)).unwrap();
        let mut bob = network.register_endpoint(ip(2)).unwrap();
        let alice_socket = alice.bind_udp(iteration).unwrap();
        let mut bob_socket = bob.bind_udp(iteration).unwrap();
        alice_socket
            .send_to(bob_socket.local_address(), vec![iteration as u8])
            .unwrap();
        let packet = network.next_packet().unwrap();
        bob_socket.close();
        bob_socket.close();
        assert_eq!(
            network.deliver(packet.id),
            Err(NetworkError::DestinationUnavailable)
        );
        bob.close();
    }
}
