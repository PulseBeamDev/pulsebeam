use pulsebeam_link::StreamControl;
use std::num::NonZeroUsize;

use super::*;

fn member() -> Member {
    Member {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
    }
}

async fn send(exchange: &Exchange, peer: &mut Outbound, now: Instant) -> Box<[u8]> {
    let (lane, mut stream) =
        StreamControl::new_authenticated(NonZeroUsize::new(MAX_FRAME).unwrap());
    exchange.flush(peer, &lane, LinkTime::from_nanos(u64::MAX), now);
    stream.directions().0.recv().await.unwrap().0
}

#[test]
fn protection_does_not_use_an_expired_standby_between_reconciliations() {
    let (publisher, relay, edge) = (member(), member(), member());
    let now = Instant::now();
    let cost = default_link_cost();
    let links = BTreeMap::from([(relay, cost), (edge, cost)]);
    let mut exchange = Exchange::new(publisher, Role::Router, now);
    for (origin, role, fresh_ms, links) in [
        (
            relay,
            Role::Router,
            50,
            vec![(publisher, cost), (edge, cost)],
        ),
        (
            edge,
            Role::Edge,
            100,
            vec![(publisher, cost), (relay, cost)],
        ),
    ] {
        let mut frame = TAG.to_vec();
        serde_json::to_writer(
            &mut frame,
            &[Report {
                origin,
                role,
                revision: NonZeroU64::MIN,
                fresh_ms,
                links,
            }],
        )
        .unwrap();
        assert!(exchange.receive(&Outbound::new(origin, role), &frame, now));
    }
    exchange.reconcile(links, now);
    assert_eq!(
        exchange.protection_paths(edge, now),
        Some(([edge].as_slice(), vec![relay, edge]))
    );
    assert_eq!(
        exchange.protection_paths(edge, now + Duration::from_millis(50)),
        None
    );
    assert_eq!(exchange.path(edge), Some([edge].as_slice()));
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn router_relays_fresh_bilateral_links_but_a_replay_cannot_extend_them() {
    let (p, r, a) = (member(), member(), member());
    let now = Instant::now();
    let cost = default_link_cost();
    let mut publisher = Exchange::new(p, Role::Router, now);
    let mut router = Exchange::new(r, Role::Router, now);
    let mut edge = Exchange::new(a, Role::Edge, now);
    publisher.reconcile(BTreeMap::from([(r, cost)]), now);
    router.reconcile(BTreeMap::from([(p, cost), (a, cost)]), now);
    edge.reconcile(BTreeMap::from([(r, cost)]), now);

    let mut edge_to_router = Outbound::new(r, Role::Router);
    let router_from_edge = Outbound::new(a, Role::Edge);
    assert!(router.receive(
        &router_from_edge,
        &send(&edge, &mut edge_to_router, now).await,
        now
    ));

    let mut router_to_publisher = Outbound::new(p, Role::Router);
    let frame = send(&router, &mut router_to_publisher, now).await;
    let publisher_from_router = Outbound::new(r, Role::Router);
    assert!(publisher.receive(&publisher_from_router, &frame, now));
    publisher.reconcile(BTreeMap::from([(r, cost)]), now);
    assert_eq!(publisher.path(a), Some([r, a].as_slice()));
    assert_eq!(publisher.protection_paths(a, now), None);

    let nearly_expired = now + FRESH - RECHECK;
    assert!(publisher.receive(&publisher_from_router, &frame, nearly_expired));
    publisher.reconcile(BTreeMap::from([(r, cost)]), now + FRESH);
    assert!(publisher.path(a).is_none());
    assert!(publisher.path(r).is_none());

    let mut forged = TAG.to_vec();
    serde_json::to_writer(
        &mut forged,
        &[Report {
            origin: p,
            role: Role::Router,
            revision: NonZeroU64::MAX,
            fresh_ms: 1000,
            links: vec![(r, cost)],
        }],
    )
    .unwrap();
    assert!(router.receive(&router_from_edge, &forged, now));
    router.reconcile(BTreeMap::from([(p, cost), (a, cost)]), now);
    assert!(
        router.path(p).is_none(),
        "an edge cannot attest another origin"
    );
}
