use super::*;

fn member() -> Member {
    Member {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
    }
}

fn report(origin: Member, role: Role, neighbors: &[(Member, u16)]) -> Advertisement {
    Advertisement {
        origin,
        role,
        neighbors: neighbors
            .iter()
            .map(|&(peer, cost)| (peer, NonZeroU16::new(cost).unwrap()))
            .collect(),
    }
}

#[test]
fn bilateral_max_and_hold_select_alternate_route_without_holding_a_dead_path() {
    let (publisher, relay, viewer) = (member(), member(), member());
    let p = report(
        publisher,
        Role::Router,
        &[(viewer, default_link_cost().get()), (relay, 20)],
    );
    let a = report(viewer, Role::Router, &[(publisher, 90), (relay, 40)]);
    let slow = report(relay, Role::Router, &[(publisher, 80), (viewer, 20)]);
    let fast = report(relay, Role::Router, &[(publisher, 30), (viewer, 20)]);
    let now = Instant::now();
    let mut routes = Routes::default();
    routes.reconcile(
        publisher,
        &Graph::from_reports([&p, &a, &slow]).unwrap(),
        now,
    );
    assert_eq!(routes.path(viewer), Some([viewer].as_slice()));
    let graph = Graph::from_reports([&p, &a, &fast]).unwrap();
    assert_eq!(
        graph.standby_for(publisher, &[viewer]),
        Some(vec![relay, viewer])
    );
    routes.reconcile(publisher, &graph, now);
    assert_eq!(routes.path(viewer), Some([viewer].as_slice()));
    routes.reconcile(publisher, &graph, now + HOLD);
    assert_eq!(routes.path(viewer), Some([relay, viewer].as_slice()));
    routes.reconcile(
        publisher,
        &Graph::from_reports([&p, &a]).unwrap(),
        now + HOLD,
    );
    assert_eq!(routes.path(viewer), Some([viewer].as_slice()));
}

#[test]
fn diamond_ties_are_stable_and_edges_cannot_forward_between_routers() {
    let (p, r, s, a, edge) = (member(), member(), member(), member(), member());
    let reports = [
        report(p, Role::Router, &[(s, 10), (r, 10), (edge, 1)]),
        report(r, Role::Router, &[(p, 10), (a, 10)]),
        report(s, Role::Router, &[(p, 10), (a, 10)]),
        report(a, Role::Router, &[(r, 10), (s, 10), (edge, 1)]),
        report(edge, Role::Edge, &[(p, 1), (a, 1)]),
    ];
    let mut routes = Routes::default();
    let graph = Graph::from_reports(reports.iter().rev()).unwrap();
    routes.reconcile(p, &graph, Instant::now());
    assert_eq!(routes.path(a), Some([r.min(s), a].as_slice()));
    assert_eq!(routes.path(edge), Some([edge].as_slice()));
    routes.reconcile(edge, &graph, Instant::now());
    assert_eq!(routes.path(a), Some([a].as_slice()));
}

#[test]
fn protection_skips_cheaper_routes_through_the_same_relay() {
    let (p, shared, detour, standby, a) = (member(), member(), member(), member(), member());
    let reports = [
        report(p, Role::Router, &[(shared, 5), (detour, 10), (standby, 40)]),
        report(shared, Role::Router, &[(p, 5), (detour, 10), (a, 5)]),
        report(detour, Role::Router, &[(p, 10), (shared, 10)]),
        report(a, Role::Edge, &[(shared, 5), (standby, 40)]),
        report(standby, Role::Router, &[(p, 40), (a, 40)]),
    ];
    let active = [shared, a];
    let graph = Graph::from_reports(&reports).unwrap();
    assert_eq!(
        graph.shortest_paths(p).get(&a).map(Vec::as_slice),
        Some(active.as_slice())
    );
    assert_eq!(graph.standby_for(p, &active), Some(vec![standby, a]));

    let no_standby = Graph::from_reports(&reports[..4]).unwrap();
    assert_eq!(no_standby.standby_for(p, &active), None);
    let no_primary = Graph::from_reports(reports.iter().filter(|r| r.origin != shared)).unwrap();
    assert_eq!(no_primary.standby_for(p, &active), None);
}

#[test]
fn incident_links_require_mutual_current_generations() {
    let (p, a) = (member(), member());
    let p0 = report(p, Role::Router, &[(a, 10)]);
    let a0 = report(a, Role::Edge, &[(p, 10)]);
    let replacement = Member {
        generation: NonZeroU64::new(2).unwrap(),
        ..a
    };
    let a1 = report(replacement, Role::Edge, &[(p, 10)]);
    assert!(Graph::from_reports([&p0, &a0, &a1]).is_none());
    let mut routes = Routes::default();
    let now = Instant::now();
    routes.reconcile(p, &Graph::from_reports([&p0, &a0]).unwrap(), now);
    assert!(routes.path(a).is_some());
    routes.reconcile(p, &Graph::from_reports([&p0, &a1]).unwrap(), now);
    assert!(routes.path(a).is_none());
    assert!(routes.path(replacement).is_none());
    let p1 = report(p, Role::Router, &[(replacement, 10)]);
    routes.reconcile(p, &Graph::from_reports([&p1, &a1]).unwrap(), now);
    assert!(routes.path(replacement).is_some());
}
