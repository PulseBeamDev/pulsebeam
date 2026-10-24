#![cfg(feature = "native")]

use pulsebeam_webrtc_sys::{ControlledWorld, PeerError, PeerErrorKind};
use std::{net::Ipv4Addr, time::Duration};

fn invalid_selection<T>(result: Result<T, PeerError>) {
    let error = result.err().expect("invalid capture selection accepted");
    assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
    assert!(
        !error
            .message
            .contains("unavailable in controlled execution")
    );
}

#[test]
fn explicit_capture_selection_reaches_normal_validation_without_a_mode_gate() {
    let world = ControlledWorld::acquire(404, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 4, 0, 1).into())
        .unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let before = (world.now(), world.next_deadline());

    // These methods explicitly request a physical backend. Invalid selections
    // are rejected by their ordinary validation, not by the surrounding runtime.
    // Do not enumerate/open real devices here: that opt-out has external effects
    // and is not evidence of deterministic managed-core execution.
    invalid_selection(factory.camera_formats(""));
    invalid_selection(factory.camera_formats("invalid\0identifier"));
    invalid_selection(factory.open_camera("", 0, 0, 0));
    invalid_selection(factory.open_camera("invalid\0identifier", 640, 480, 30));
    invalid_selection(factory.open_camera("not-opened", 0, 480, 30));

    assert_eq!((world.now(), world.next_deadline()), before);
    assert!(network.next_packet().is_none());
}
