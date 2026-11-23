use pulsebeam_core::fabric::Profile;
use pulsebeam_link::{
    FabricPath, LinkControl, NetworkClass, NodeRouteKey, PeerRole, SecretCredential,
};
use pulsebeam_runtime::time::Duration;

use crate::link::{fabric_codec::FabricWireCodec, host};

use super::*;

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn provisioned_legacy_and_wan_sockets_cannot_activate_either_carrier() {
    for (wan_role, legacy_role) in [
        (PeerRole::Initiator, PeerRole::Responder),
        (PeerRole::Responder, PeerRole::Initiator),
    ] {
        let now = Instant::now();
        let mut links = LinkControl::<ShardFrame>::new(|| LinkTime::from_nanos(0)).unwrap();
        let address = "127.0.0.1:0".parse().unwrap();
        let (mut legacy, control) = host::Host::bind(address, now).await.unwrap();
        let (service, provisioner) = Service::bind(address, now).await.unwrap();
        let legacy_pin = SecretCredential::generate().unwrap();
        let wan_pin = SecretCredential::generate().unwrap();
        let old = NetworkPeer {
            endpoint: service.local_addr(),
            expected_peer: wan_pin.fingerprint(),
            credential: legacy_pin.clone(),
            role: legacy_role,
            class: NetworkClass::Wan,
        };
        let new = NetworkPeer {
            endpoint: legacy.test_socket().local_addr(),
            expected_peer: legacy_pin.fingerprint(),
            credential: wan_pin,
            role: wan_role,
            class: NetworkClass::Wan,
        };
        let legacy_endpoint = old.endpoint;
        let (old_id, mut legacy_lane) = links
            .open_fabric_network_with_control(
                old.with_fabric_path(FabricPath {
                    udp_payload_limit: 1200,
                    additional_overhead: 0,
                }),
                FabricWireCodec,
            )
            .unwrap();
        assert!(
            control
                .try_send(host::Command::Install(Box::new(host::Install {
                    id: old_id,
                    endpoint: legacy_endpoint,
                    driver: links.take_network_driver(old_id).unwrap(),
                    node: None,
                    peer_node: None,
                })))
                .is_ok()
        );
        let peer = links.reserve_network_id().unwrap();
        let (_commands, mut lane) = provisioner.open(new, peer, NonZeroU64::MIN, now).unwrap();
        let Service {
            socket,
            mut opens,
            origin,
        } = service;
        let Open {
            driver,
            commands,
            media,
            ..
        } = opens.try_recv().ok().unwrap();
        let mut wan = WanHost::attach(socket, peer, origin, driver, LIMITS.active_windows);
        wan.commands = Some(commands);
        wan.egress.inbox = Some(media);

        for _ in 0..64 {
            legacy.poll_at(now).unwrap();
            if wan.poll(now).is_err() {
                break;
            }
            assert!(!legacy_lane.is_ready() && !lane.is_ready());
            pulsebeam_runtime::select! {
                ready = wan.socket.readable() => ready.unwrap(),
                ready = legacy.test_socket().readable() => ready.unwrap(),
                () = pulsebeam_runtime::time::sleep(Duration::from_millis(1)) => {},
            }
        }
        let later = now + Duration::from_secs(3);
        let _ = wan.poll(later);
        legacy.poll_at(later).unwrap();
        assert!(!legacy_lane.is_ready() && !lane.is_ready());
        assert!(legacy_lane.try_recv().is_none() && lane.try_recv().is_none());
        let label = NonZeroU32::new(0x0506_0708).unwrap();
        assert!(
            links
                .bind_fabric_send(old_id, NodeRouteKey::from_bytes(label.get().to_be_bytes()))
                .is_err()
        );
        let window = Window::new(Profile::Opus, 1 << 32, 0, label, 0).unwrap();
        assert!(
            wan.stage_send(
                later,
                window,
                WindowSpec {
                    authority: NonZeroU64::MIN,
                    direction: Direction::Send,
                    start: window.start(),
                    ssrc: label,
                    purpose: Purpose::Live,
                    expires: (later + Duration::from_secs(1)).into_std(),
                }
            )
            .is_err()
        );
    }
}
