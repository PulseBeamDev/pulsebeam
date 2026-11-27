use super::*;

fn configured(role: Role) -> ConfiguredMember {
    ConfiguredMember {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
        role,
        catalog: false,
        media_key: None,
        boot_key: None,
    }
}

fn fixture() -> Manifest {
    let gateway = || configured(Role::Gateway);
    let [p, q, a, b, c] = std::array::from_fn(|_| gateway());
    let region = |name: &str, parent: Option<&str>, mut members: Vec<ConfiguredMember>| {
        let mut authority = configured(Role::Router);
        authority.catalog = true;
        members.push(authority);
        RegionConfig {
            name: name.into(),
            parent: parent.map(str::to_owned),
            members,
        }
    };
    Manifest {
        regions: vec![
            region("hub", None, vec![p, q]),
            region("west", Some("hub"), vec![a, b]),
            region("east", Some("hub"), vec![c]),
        ],
        borders: vec![
            Border {
                child: a.node,
                parent: p.node,
            },
            Border {
                child: b.node,
                parent: q.node,
            },
            Border {
                child: c.node,
                parent: p.node,
            },
        ],
    }
}

#[test]
fn sibling_routes_use_the_parent_and_only_pinned_gateway_pairs() {
    let config = fixture();
    let p = config.regions[0].members[0].identity();
    let a = config.regions[1].members[0].identity();
    let b = config.regions[1].members[1].identity();
    let router = config.regions[1].members[2].identity();
    let c = config.regions[2].members[0].identity();
    let tree = RegionTree::try_from(config).unwrap();
    assert_eq!(tree.next_region("west", "east"), Some("hub"));
    assert_eq!(tree.next_region("hub", "east"), Some("east"));
    assert_eq!(tree.next_region("east", "east"), None);
    assert_eq!(tree.next_region("west", "not-configured"), None);
    assert_eq!(tree.border_pairs("west", "hub").count(), 2);
    assert_eq!(tree.border_pairs("west", "east").count(), 0);
    assert_eq!(tree.adjacency(a, p).unwrap().peer_region, "hub");
    assert_eq!(tree.adjacency(p, a).unwrap().peer_region, "west");
    assert_eq!(tree.adjacency(a, router).unwrap().local_region, "west");
    assert_eq!(tree.adjacency(a, c), None);
    assert_eq!(tree.adjacency(b, p), None);
    assert_eq!(
        tree.adjacency(
            Member {
                generation: NonZeroU64::MAX,
                ..a
            },
            p
        ),
        None
    );

    let coverage: Vec<_> = tree.catalog_members().collect();
    assert_eq!(coverage.len(), 3);
    assert!(coverage.contains(&router));
    assert!(
        coverage
            .iter()
            .any(|&member| tree.region(member) == Some("east"))
    );
    assert!(!coverage.contains(&a));
}

#[test]
fn malformed_hierarchies_are_rejected_before_opening_links() {
    let mut cyclic = fixture();
    cyclic.regions[1].parent = Some("east".into());
    cyclic.regions[2].parent = Some("west".into());
    let mut second_root = fixture();
    second_root.regions[1].parent = None;
    let mut unknown_parent = fixture();
    unknown_parent.regions[1].parent = Some("unconfigured".into());
    let mut repeated_member = fixture();
    let duplicate = repeated_member.regions[0].members[0];
    repeated_member.regions[1].members.push(duplicate);
    let mut sibling_border = fixture();
    sibling_border.borders[0].parent = sibling_border.regions[2].members[0].node;
    let mut nongateway_border = fixture();
    nongateway_border.regions[0].members[0].role = Role::Router;
    let mut missing_border = fixture();
    missing_border.borders.pop();
    let mut reused_key = fixture();
    let mut cross_use = fixture();
    let key = pulsebeam_core::auth::ApiSigningKey::from_seed([7; 32]).public_key();
    reused_key.regions[1].members[0].media_key = Some(key);
    reused_key.regions[2].members[0].media_key = Some(key);
    cross_use.regions[1].members[0].media_key = Some(key);
    cross_use.regions[2].members[0].boot_key = Some(key);
    for config in [
        cyclic,
        second_root,
        unknown_parent,
        repeated_member,
        sibling_border,
        nongateway_border,
        missing_border,
        reused_key,
        cross_use,
    ] {
        assert!(RegionTree::try_from(config).is_err());
    }
}

#[test]
fn enrolled_roster_has_stable_policy_but_authorizes_only_admitted_boots() {
    let mut config = fixture();
    let local = config.regions[0].members[0].identity();
    let peer = &mut config.regions[1].members[0];
    peer.catalog = true;
    let pin = pulsebeam_core::auth::ApiSigningKey::from_seed([6; 32]).public_key();
    peer.media_key = Some(pin);
    peer.boot_key = Some(pin);
    let enrolled = peer.identity();
    let rebooted = Member {
        generation: NonZeroU64::new(7).unwrap(),
        ..enrolled
    };
    let other_gateway = config.regions[1].members[1].identity();

    let mut tree = RegionTree::try_from(config.clone()).unwrap();
    let legacy_fingerprint = *tree.fingerprint();
    let mut expected_coverage = tree.catalog_members().collect::<Vec<_>>();
    tree.begin_boot_admission(local).unwrap();
    let stable_fingerprint = *tree.fingerprint();
    assert_ne!(legacy_fingerprint, stable_fingerprint);
    assert!(tree.enrolled_adjacency(local.node, enrolled.node).is_some());
    assert!(
        tree.enrolled_adjacency(local.node, other_gateway.node)
            .is_none()
    );
    assert!(tree.adjacency(local, enrolled).is_none());
    assert!(tree.media_key(enrolled).is_none());
    assert_eq!(tree.discovery_key(enrolled), Some(pin));
    assert_eq!(tree.discovery_key(rebooted), Some(pin));
    assert_eq!(tree.catalog_members().count(), 0);
    assert_eq!(
        tree.catalog_coverage_members().collect::<Vec<_>>(),
        expected_coverage
    );
    assert!(tree.admit_boot(enrolled));
    assert_eq!(tree.media_key(enrolled), Some(pin));
    assert!(tree.admit_boot(rebooted));
    assert!(!tree.admit_boot(enrolled));
    assert!(!tree.admit_boot(Member {
        generation: NonZeroU64::MAX,
        ..local
    }));
    assert_eq!(tree.media_key(enrolled), None);
    assert_eq!(tree.media_key(rebooted), Some(pin));
    assert!(tree.adjacency(local, enrolled).is_none());
    assert!(tree.adjacency(local, rebooted).is_some());
    assert_eq!(tree.catalog_members().collect::<Vec<_>>(), [rebooted]);
    *expected_coverage
        .iter_mut()
        .find(|member| **member == enrolled)
        .unwrap() = rebooted;
    assert_eq!(
        tree.catalog_coverage_members().collect::<Vec<_>>(),
        expected_coverage
    );
    assert_eq!(
        tree.border_pairs("hub", "west").collect::<Vec<_>>(),
        [(local, rebooted)]
    );
    assert_eq!(*tree.fingerprint(), stable_fingerprint);

    config.regions[1].members[0].generation = rebooted.generation;
    let mut from_new_floor = RegionTree::try_from(config.clone()).unwrap();
    from_new_floor.begin_boot_admission(local).unwrap();
    assert_eq!(*from_new_floor.fingerprint(), stable_fingerprint);
    assert!(!from_new_floor.admit_boot(enrolled));
    config.regions[1].members[0].catalog = false;
    let mut changed_policy = RegionTree::try_from(config).unwrap();
    changed_policy.begin_boot_admission(local).unwrap();
    assert_ne!(*changed_policy.fingerprint(), stable_fingerprint);
}

#[test]
fn fingerprint_agrees_across_reordering_but_binds_exact_roster() {
    let mut config = fixture();
    let original = RegionTree::try_from(config.clone()).unwrap();
    config.regions.reverse();
    config.borders.reverse();
    for region in &mut config.regions {
        region.members.reverse();
    }
    let reordered: RegionTree =
        serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(reordered.fingerprint(), original.fingerprint());
    config.regions[0].members[0].generation = NonZeroU64::MAX;
    assert_ne!(
        RegionTree::try_from(config).unwrap().fingerprint(),
        original.fingerprint()
    );
}
