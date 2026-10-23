use pulsebeam_rtc_simulation::{
    NetworkAddress, NetworkError, OutboundKind, PacketBridge, PacketBridgeError, SimulationWorld,
};
use std::{net::IpAddr, rc::Rc};

#[test]
fn native_packet_bridge_preserves_tuples_bytes_and_owned_decisions() {
    let world = SimulationWorld::acquire();
    let mut bridge = PacketBridge::new(world.clone()).unwrap();
    let client_ip: IpAddr = "10.0.0.2".parse().unwrap();
    let server_ip: IpAddr = "10.0.0.1".parse().unwrap();
    let endpoint = bridge.endpoint(client_ip).unwrap();
    assert!(Rc::ptr_eq(&endpoint, &bridge.endpoint(client_ip).unwrap()));
    let mut client = endpoint.bind_udp(50000).unwrap();
    let local = client.local_address();
    let server = NetworkAddress::new(server_ip, 3478).unwrap();

    client.send_to(server, vec![1, 2, 3]).unwrap();
    let mut turn = bridge.poll(1).unwrap();
    assert!(turn.budget_exhausted);
    assert_eq!(turn.outgoing.len(), 1);
    let outgoing = turn.outgoing.pop().unwrap();
    assert_eq!(outgoing.packet().source, local);
    assert_eq!(outgoing.packet().destination, server);
    assert_eq!(outgoing.packet().kind, OutboundKind::Udp);
    assert_eq!(outgoing.packet().payload, [1, 2, 3]);
    outgoing.accept().unwrap();
    assert!(bridge.poll(8).unwrap().outgoing.is_empty());

    bridge.receive_udp(server, local, vec![4, 5, 6]).unwrap();
    let turn = bridge.poll(8).unwrap();
    assert!(!turn.budget_exhausted);
    assert!(turn.outgoing.is_empty());
    assert_eq!(turn.closed_destinations, 0);
    world.pump();
    let incoming = client.try_receive().unwrap();
    assert_eq!(incoming.source, server);
    assert_eq!(incoming.payload, [4, 5, 6]);
    assert!(client.try_receive().is_none());

    // A cancelled external decision is rejected rather than retained by the
    // native network. Only a subsequent independently submitted datagram exits.
    client.send_to(server, vec![7]).unwrap();
    drop(bridge.poll(8).unwrap());
    client.send_to(server, vec![8]).unwrap();
    let turn = bridge.poll(8).unwrap();
    assert_eq!(turn.outgoing.len(), 1);
    assert_eq!(turn.outgoing[0].packet().payload, [8]);
    drop(turn);

    // Ingress racing native socket closure is a real closed destination, not a
    // frame injected into another receiver or an invented generation binding.
    client.close();
    bridge.receive_udp(server, local, vec![9]).unwrap();
    let turn = bridge.poll(8).unwrap();
    assert_eq!(turn.closed_destinations, 1);
    assert!(turn.outgoing.is_empty());

    // External adapter socket admission is bounded and reports its own limit,
    // never a fabricated native address collision or a media profile refusal.
    for port in 3479..3542 {
        bridge
            .receive_udp(
                NetworkAddress::new(server_ip, port).unwrap(),
                local,
                vec![10],
            )
            .unwrap();
    }
    assert!(matches!(
        bridge.receive_udp(
            NetworkAddress::new(server_ip, 3542).unwrap(),
            local,
            vec![11]
        ),
        Err(PacketBridgeError::ExternalSocketLimit)
    ));
    assert_eq!(bridge.poll(128).unwrap().closed_destinations, 63);
    let unknown = NetworkAddress::new("10.0.0.9".parse().unwrap(), 50000).unwrap();
    assert!(matches!(
        bridge.receive_udp(server, unknown, vec![12]),
        Err(PacketBridgeError::Network(
            NetworkError::DestinationUnavailable
        ))
    ));
}
