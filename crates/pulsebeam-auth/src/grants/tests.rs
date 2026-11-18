#![allow(clippy::unwrap_used)]

use super::*;

fn grant(rooms: &[&str], allow: &[Action]) -> Grant {
    Grant::new(
        rooms.iter().map(|room| (*room).to_owned()).collect(),
        allow.to_vec(),
        None,
        None,
        None,
    )
    .unwrap()
}

#[test]
fn exact_prefix_and_project_wildcard_are_literal_and_case_sensitive() {
    for (pattern, accepted, denied) in [
        ("main", vec!["main"], vec!["Main", "main/child", "mainland"]),
        (
            "meeting/123/*",
            vec!["meeting/123/", "meeting/123/a", "meeting/123/a/b"],
            vec!["meeting/123", "meeting/1234/a", "Meeting/123/a"],
        ),
        ("/*", vec!["/", "/a"], vec!["a", "a/"]),
        ("*", vec!["main", "robots", "a/b"], vec![]),
    ] {
        let scopes = GrantSet::new(vec![grant(&[pattern], &[Action::Join])]).unwrap();
        for room in accepted {
            assert!(scopes.allows(Action::Join, room, None, None, None));
        }
        for room in denied {
            assert!(!scopes.allows(Action::Join, room, None, None, None));
        }
        for invalid in ["", "*", "main/*"] {
            assert!(!scopes.allows(Action::Join, invalid, None, None, None));
        }
    }
    for pattern in ["", "main*", "main/*/child", "**", "a/**", "a*b/*"] {
        assert_eq!(
            Grant::new(vec![pattern.into()], vec![Action::Join], None, None, None),
            Err(GrantError::InvalidRoomPattern)
        );
    }
}

#[test]
fn grants_are_additive_and_filters_conjunctive_without_implied_actions() {
    let narrow = Grant::new(
        vec!["robots".into()],
        vec![Action::Subscribe],
        Some("robot-42".into()),
        Some(TrackKind::Video),
        Some("front".into()),
    )
    .unwrap();
    let scopes = GrantSet::new(vec![
        grant(&["meeting/123/*"], &[Action::Join, Action::Publish]),
        narrow.clone(),
    ])
    .unwrap();
    assert!(scopes.may_subscribe("robots", TrackKind::Video));
    assert!(!scopes.may_subscribe("robots", TrackKind::Audio));
    assert!(!scopes.may_subscribe("meeting/123/a", TrackKind::Video));
    assert!(!scopes.may_subscribe("*", TrackKind::Video));
    assert!(scopes.allows(
        Action::Subscribe,
        "robots",
        Some("robot-42"),
        Some(TrackKind::Video),
        Some("front"),
    ));
    for (participant, kind, label) in [
        (Some("robot-43"), Some(TrackKind::Video), Some("front")),
        (Some("robot-42"), Some(TrackKind::Audio), Some("front")),
        (Some("robot-42"), Some(TrackKind::Video), Some("back")),
        (None, Some(TrackKind::Video), Some("front")),
        (Some("robot-42"), None, Some("front")),
        (Some("robot-42"), Some(TrackKind::Video), None),
    ] {
        assert!(!scopes.allows(Action::Subscribe, "robots", participant, kind, label));
    }
    for action in [Action::Join, Action::Publish, Action::Discover] {
        assert!(!scopes.allows(
            action,
            "robots",
            Some("robot-42"),
            Some(TrackKind::Video),
            Some("front"),
        ));
    }
    assert!(!scopes.allows(Action::Join, "meeting/123", None, None, None));
    assert!(scopes.allows(Action::Join, "meeting/123/a", None, None, None));

    let broad = grant(&["*"], &[Action::Subscribe]);
    for grants in [vec![broad.clone(), narrow.clone()], vec![narrow, broad]] {
        assert!(GrantSet::new(grants).unwrap().allows(
            Action::Subscribe,
            "robots",
            Some("other"),
            Some(TrackKind::Data),
            Some("telemetry"),
        ));
    }
    let split_filters = GrantSet::new(
        [("alice", TrackKind::Video), ("bob", TrackKind::Audio)]
            .into_iter()
            .map(|(participant, kind)| {
                Grant::new(
                    vec!["robots".into()],
                    vec![Action::Subscribe],
                    Some(participant.into()),
                    Some(kind),
                    None,
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    assert!(!split_filters.allows(
        Action::Subscribe,
        "robots",
        Some("alice"),
        Some(TrackKind::Audio),
        Some("mic"),
    ));
    assert!(!GrantSet::default().allows(Action::Join, "main", None, None, None));
}

#[test]
fn publication_label_projection_matches_concrete_authorization() {
    let narrow = Grant::new(
        vec!["main/*".into()],
        vec![Action::Publish],
        None,
        Some(TrackKind::Data),
        Some("v2/latest/chat".into()),
    )
    .unwrap();
    let video = Grant::new(
        vec!["main/child".into()],
        vec![Action::Publish],
        None,
        Some(TrackKind::Video),
        None,
    )
    .unwrap();
    for grants in [
        vec![],
        vec![grant(&["*"], &[Action::Join, Action::Discover])],
        vec![grant(&["*"], &[Action::Subscribe])],
        vec![narrow.clone(), video.clone()],
        vec![video, narrow.clone(), grant(&["other"], &[Action::Publish])],
        vec![narrow, grant(&["*"], &[Action::Publish])],
    ] {
        let grants = GrantSet::new(grants).unwrap();
        for room in [
            "main",
            "main/child",
            "main/child/deep",
            "Main/child",
            "other",
            "*",
            "",
        ] {
            for kind in [TrackKind::Audio, TrackKind::Video, TrackKind::Data] {
                let labels = grants.publication_labels(room, kind).collect::<Vec<_>>();
                for label in ["v2/latest/chat", "v2/ordered/chat", "v2/latest/Chat", "*"] {
                    assert_eq!(
                        labels
                            .iter()
                            .any(|scope| scope.is_none_or(|scope| scope == label)),
                        grants.allows(Action::Publish, room, None, Some(kind), Some(label)),
                        "{room:?} {kind:?} {label:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn discovery_filters_do_not_expose_zero_track_or_unrelated_descriptions() {
    let filtered = GrantSet::new(vec![
        Grant::new(
            vec!["robots".into()],
            vec![Action::Discover],
            None,
            Some(TrackKind::Data),
            None,
        )
        .unwrap(),
    ])
    .unwrap();
    assert!(!filtered.allows(Action::Discover, "robots", Some("robot"), None, None));
    assert!(!filtered.allows(
        Action::Discover,
        "robots",
        Some("robot"),
        Some(TrackKind::Video),
        Some("camera"),
    ));
    assert!(filtered.allows(
        Action::Discover,
        "robots",
        Some("robot"),
        Some(TrackKind::Data),
        Some("telemetry"),
    ));
    assert!(
        GrantSet::new(vec![grant(&["robots"], &[Action::Discover])])
            .unwrap()
            .allows(Action::Discover, "robots", Some("robot"), None, None)
    );
}

#[test]
fn every_action_in_a_grant_must_allow_its_filters() {
    for allow in [vec![Action::Join], vec![Action::Subscribe, Action::Join]] {
        for (participant, kind, label) in [
            (Some("robot".into()), None, None),
            (None, Some(TrackKind::Audio), None),
            (None, None, Some("mic".into())),
        ] {
            assert_eq!(
                Grant::new(vec!["main".into()], allow.clone(), participant, kind, label),
                Err(GrantError::InvalidActionFilters)
            );
        }
    }
    assert_eq!(
        Grant::new(
            vec!["main".into()],
            vec![Action::Publish, Action::Subscribe],
            Some("robot".into()),
            None,
            None,
        ),
        Err(GrantError::InvalidActionFilters)
    );
    assert!(
        Grant::new(
            vec!["main".into()],
            vec![Action::Publish, Action::Discover, Action::Subscribe],
            None,
            Some(TrackKind::Audio),
            Some("mic".into()),
        )
        .is_ok()
    );
}

#[test]
fn enforce_received_counts_before_any_deduplication_and_use_utf8_bytes() {
    let unfiltered = grant(&["*"], &[Action::Join]);
    assert!(GrantSet::new(vec![unfiltered.clone(); MAX_GRANTS]).is_ok());
    assert_eq!(
        GrantSet::new(vec![unfiltered; MAX_GRANTS + 1]),
        Err(GrantError::TooManyGrants)
    );
    let many_rooms = Grant::new(
        vec!["main".into(); MAX_ROOM_PATTERNS],
        vec![Action::Join],
        None,
        None,
        None,
    )
    .unwrap();
    assert!(GrantSet::new(vec![many_rooms.clone()]).is_ok());
    assert_eq!(
        GrantSet::new(vec![many_rooms, grant(&["main"], &[Action::Join])]),
        Err(GrantError::TooManyRoomPatterns)
    );
    assert_eq!(
        Grant::new(
            vec!["main".into(); MAX_ROOM_PATTERNS + 1],
            vec![Action::Join],
            None,
            None,
            None,
        ),
        Err(GrantError::TooManyRoomPatterns)
    );
    assert_eq!(
        Grant::new(vec![], vec![Action::Join], None, None, None),
        Err(GrantError::EmptyRooms)
    );
    for actions in [vec![], vec![Action::Join, Action::Join]] {
        assert_eq!(
            Grant::new(vec!["main".into()], actions, None, None, None),
            Err(GrantError::InvalidActions)
        );
    }
    assert!(
        Grant::new(
            vec!["é".repeat(MAX_SCOPE_BYTES / 2)],
            vec![Action::Subscribe],
            Some("é".repeat(MAX_SCOPE_BYTES / 2)),
            None,
            Some("é".repeat(MAX_SCOPE_BYTES / 2)),
        )
        .is_ok()
    );
    let oversized = "é".repeat(MAX_SCOPE_BYTES / 2 + 1);
    assert_eq!(
        Grant::new(
            vec![oversized.clone()],
            vec![Action::Join],
            None,
            None,
            None
        ),
        Err(GrantError::InvalidRoomPattern)
    );
    for invalid in [
        String::new(),
        oversized,
        "\0".into(),
        "a\nb".into(),
        "\u{7f}".into(),
        "é\u{85}".into(),
    ] {
        assert_eq!(
            Grant::new(vec![invalid.clone()], vec![Action::Join], None, None, None),
            Err(GrantError::InvalidRoomPattern)
        );
        for (participant, label) in [(Some(invalid.clone()), None), (None, Some(invalid.clone()))] {
            assert_eq!(
                Grant::new(
                    vec!["main".into()],
                    vec![Action::Subscribe],
                    participant,
                    None,
                    label,
                ),
                Err(GrantError::InvalidFilter)
            );
        }
    }
}
