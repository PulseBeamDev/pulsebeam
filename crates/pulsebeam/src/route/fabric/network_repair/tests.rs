use std::time::Duration;

use pulsebeam_link::{
    LinkControl, LinkTime, NetworkClass, NetworkPeer, NodeRouteKey, PeerRole, SecretCredential,
};

use super::*;
use crate::{entity::ConnectionId, shard::worker::ShardFrame};

#[test]
fn repair_ingress_requires_exact_acknowledged_demand_until_handoff() {
    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let now = Instant::now();
    let mut link = LinkControl::<ShardFrame>::new(|| LinkTime::from_nanos(0)).unwrap();
    let mut association = |port| {
        let credential = SecretCredential::generate().unwrap();
        link.open_network(
            NetworkPeer {
                endpoint: ([127, 0, 0, 1], port).into(),
                expected_peer: credential.fingerprint(),
                credential,
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            },
            crate::link::ReverseCodec,
        )
        .unwrap()
    };
    let route = RepairRoute {
        ingress: FabricIngress {
            association: association(17001),
            key: NodeRouteKey::from_bytes([1, 2, 0, 3]),
            purpose: pulsebeam_link::FabricPurpose::RepairOnly,
        },
        publication: ConnectionId::from_bytes([7; 16])
            .fixture_publication(TrackKind::Data, "repair"),
        source: NonZeroU64::MIN,
        branch: NonZeroU64::MIN,
        major: pulsebeam_core::fabric::MAJOR,
        revision: 0,
        emission: Emission::new(Profile::Data, &[]).unwrap(),
    };
    let other = RepairRoute {
        ingress: FabricIngress {
            association: association(17002),
            ..route.ingress
        },
        ..route
    };
    let other_branch = RepairRoute {
        branch: NonZeroU64::MAX,
        ..route
    };
    let expires = now + Duration::from_secs(1);
    let fragment = RepairFragment {
        unit_seq: 12,
        index: 1,
    };
    let demand = Demand {
        request: RepairRequest {
            fragment,
            id: NonZeroU64::MIN,
        },
        sequence: u64::from(u32::MAX) + 2,
        expires,
    };
    let mut ingress = RepairIngress::new(route, expires).unwrap();
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        None
    );
    assert_eq!(
        ingress.install(other_branch, demand, now),
        Admission::Refused
    );
    assert_eq!(ingress.install(route, demand, now), Admission::Installed);
    assert_eq!(ingress.install(route, demand, now), Admission::Replay);
    assert!(!ingress.acknowledge(other, demand, now));
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        None
    );
    assert!(ingress.acknowledge(route, demand, now));
    assert_eq!(
        ingress.claim(other.ingress, fragment, demand.sequence, now),
        None
    );
    assert_eq!(ingress.claim(route.ingress, fragment, 1, now), None);
    assert_eq!(
        ingress.claim(
            route.ingress,
            RepairFragment {
                index: 0,
                ..fragment
            },
            demand.sequence,
            now
        ),
        None
    );
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        Some(demand)
    );
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        None
    );
    assert!(!ingress.handoff(route, demand, now, || false));
    assert!(ingress.release(route, demand, now));
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        Some(demand)
    );
    assert!(ingress.handoff(route, demand, now, || true));
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        None
    );
    assert_eq!(ingress.install(route, demand, now), Admission::Refused);

    let replacement = Demand {
        request: RepairRequest {
            id: NonZeroU64::new(2).unwrap(),
            ..demand.request
        },
        ..demand
    };
    assert_eq!(
        ingress.install(route, replacement, now),
        Admission::Installed
    );
    assert!(!ingress.cancel(route, demand, now));
    assert!(!ingress.acknowledge(route, demand, now));
    assert!(ingress.acknowledge(route, replacement, now));
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        Some(replacement)
    );
    assert!(!ingress.cancel(other, replacement, now));
    assert!(ingress.cancel(route, replacement, now));
    assert!(!ingress.handoff(route, replacement, now, || panic!("cancelled claim")));
    assert_eq!(ingress.install(route, replacement, now), Admission::Refused);
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        None
    );

    let expiring = Demand {
        request: RepairRequest {
            id: NonZeroU64::new(3).unwrap(),
            ..demand.request
        },
        ..demand
    };
    assert_eq!(ingress.install(route, expiring, now), Admission::Installed);
    assert!(ingress.acknowledge(route, expiring, now));
    assert_eq!(
        ingress.claim(route.ingress, fragment, demand.sequence, now),
        Some(expiring)
    );
    assert!(!ingress.handoff(route, expiring, expires, || panic!("expired demand")));
}
