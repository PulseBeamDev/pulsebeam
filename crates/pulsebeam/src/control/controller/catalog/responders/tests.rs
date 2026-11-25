use super::*;
use crate::entity::{ProjectId, RoomId};
use std::time::Duration;

struct View {
    scope: ReadScope,
    revision: owner::Revision,
    participants: &'static [&'static str],
}

impl RoomRead for View {
    fn scope(&self) -> ReadScope {
        self.scope
    }

    fn revision(&self) -> owner::Revision {
        self.revision
    }

    fn next(&self, after: Option<&str>) -> Result<ReadStep, wire::FailureCode> {
        Ok(self
            .participants
            .iter()
            .find(|&&id| after.is_none_or(|after| id > after))
            .map_or(ReadStep::Exhausted, |&id| {
                ReadStep::Row(owner::Candidate {
                    canonical: id.into(),
                    entry: Some(wire::CatalogParticipantEntry::default()),
                })
            }))
    }
}

fn declaration(changed_at: u64) -> DeclarationId {
    DeclarationId {
        stream: NonZeroU64::MIN,
        interest: NonZeroU64::MIN,
        changed_at: NonZeroU64::new(changed_at).unwrap(),
    }
}

fn view(participants: &'static [&'static str]) -> View {
    View {
        scope: ReadScope {
            room: ProjectRoomKey {
                project: ProjectId::from_bytes([1; 16]),
                room: RoomId::new("room").unwrap(),
            },
            viewer: ConnectionId::new(),
            authorization: 1,
        },
        revision: owner::Revision {
            presence: 5,
            publications: 7,
        },
        participants,
    }
}

#[test]
fn coverage_counts_only_fresh_distinct_expected_incarnations_and_merges_in_order() {
    let now = Instant::now();
    let fresh = now.checked_add(Duration::from_secs(1)).unwrap();
    let local = view(&["b"]);
    let remote = View {
        participants: &["a", "c"],
        ..local
    };
    let empty = View {
        participants: &[],
        ..local
    };
    let unrelated = View {
        scope: ReadScope {
            authorization: 2,
            ..local.scope
        },
        ..local
    };
    let a = (NodeId::new(), NonZeroU64::MIN);
    let b = (NodeId::new(), NonZeroU64::MIN);
    let restarted = (b.0, NonZeroU64::new(2).unwrap());
    let reply = |peer, fresh_until, view| Reply {
        peer,
        declaration: declaration(1),
        fresh_until,
        view,
    };
    let ready_reply = |peer, fresh_until, view| Observation::Ready(reply(peer, fresh_until, view));
    let missing = Responders::at_cut(
        &local,
        [a, b],
        [
            ready_reply(a, fresh, &remote),
            ready_reply(b, now, &empty),
            ready_reply(b, fresh, &unrelated),
            ready_reply(restarted, fresh, &empty),
            Observation::Gap {
                peer: b,
                scope: unrelated.scope,
            },
        ],
        now,
    )
    .unwrap();
    assert!(missing.cut().partial());
    assert_eq!(missing.scan_cost(), 2);
    let ready = Responders::at_cut(
        &local,
        [a, b],
        [
            ready_reply(b, fresh, &empty),
            ready_reply(a, fresh, &remote),
        ],
        now,
    )
    .unwrap();
    assert!(!ready.cut().partial());
    assert_eq!(ready.scan_cost(), 3);
    assert_ne!(missing.cut(), ready.cut());
    for (after, expected) in [
        (None, Some("a")),
        (Some("a"), Some("b")),
        (Some("b"), Some("c")),
        (Some("c"), None),
    ] {
        let next = match ready.next(after).unwrap() {
            ReadStep::Row(row) => Some(row.canonical),
            ReadStep::Exhausted => None,
            ReadStep::Refill => panic!("unexpected refill"),
        };
        assert_eq!(next.as_deref(), expected);
    }
    let new_cut = Responders::at_cut(
        &local,
        [a, restarted],
        [
            ready_reply(a, fresh, &remote),
            ready_reply(restarted, fresh, &empty),
        ],
        now,
    )
    .unwrap();
    assert!(!new_cut.cut().partial());
    assert_ne!(ready.cut(), new_cut.cut());
    let resynced = Responders::at_cut(
        &local,
        [a, b],
        [
            ready_reply(a, fresh, &remote),
            Observation::Ready(Reply {
                declaration: declaration(2),
                ..reply(b, fresh, &empty)
            }),
        ],
        now,
    )
    .unwrap();
    assert!(!resynced.cut().partial());
    assert_ne!(ready.cut(), resynced.cut());
    assert!(matches!(
        Responders::at_cut(
            &local,
            [a, b],
            [
                ready_reply(a, fresh, &remote),
                ready_reply(a, fresh, &remote)
            ],
            now
        ),
        Err(wire::FailureCode::ResourceGone)
    ));
}

#[test]
fn finalized_empty_room_counts_for_only_its_live_scope_and_stream() {
    let now = Instant::now();
    let fresh = now.checked_add(Duration::from_secs(1)).unwrap();
    let local = view(&["a"]);
    let peer = (NodeId::new(), NonZeroU64::MIN);
    let old_generation = (peer.0, NonZeroU64::new(2).unwrap());
    let empty = |peer, scope, stream, fresh_until| Observation::Empty {
        peer,
        scope,
        stream,
        interest: NonZeroU64::MIN,
        changed_at: NonZeroU64::MIN,
        fresh_until,
    };
    let cut = |observations| Responders::at_cut(&local, [peer], observations, now).unwrap();
    assert!(
        cut([empty(old_generation, local.scope, NonZeroU64::MIN, fresh)])
            .cut()
            .partial()
    );
    assert!(
        cut([empty(peer, local.scope, NonZeroU64::MIN, now)])
            .cut()
            .partial()
    );
    let other_scope = ReadScope {
        authorization: 2,
        ..local.scope
    };
    assert!(
        cut([empty(peer, other_scope, NonZeroU64::MIN, fresh)])
            .cut()
            .partial()
    );
    let ready = cut([empty(peer, local.scope, NonZeroU64::MIN, fresh)]);
    assert!(!ready.cut().partial());
    assert_eq!(ready.scan_cost(), 1);
    assert!(matches!(ready.next(None).unwrap(), ReadStep::Row(row) if row.canonical == "a"));
    let restarted = cut([empty(peer, local.scope, NonZeroU64::new(2).unwrap(), fresh)]);
    assert_ne!(ready.cut(), restarted.cut());
}

#[test]
fn maximum_configured_adjacencies_without_coverage_still_serve_local_rows() {
    let local = view(&["local"]);
    let cut = Responders::at_cut(
        &local,
        (0..MAX_CATALOG_MEMBERS).map(|_| (NodeId::new(), NonZeroU64::MIN)),
        [],
        Instant::now(),
    )
    .unwrap();
    assert!(cut.cut().partial());
    assert!(matches!(cut.next(None).unwrap(), ReadStep::Row(row) if row.canonical == "local"));
}

#[test]
fn conflicting_visible_owners_fail_closed() {
    let now = Instant::now();
    let local = view(&["a"]);
    let peer = (NodeId::new(), NonZeroU64::MIN);
    let responders = Responders::at_cut(
        &local,
        [peer],
        [Observation::Ready(Reply {
            peer,
            declaration: declaration(1),
            fresh_until: now.checked_add(Duration::from_secs(1)).unwrap(),
            view: &local,
        })],
        now,
    )
    .unwrap();
    assert!(matches!(
        responders.next(None),
        Err(wire::FailureCode::ResourceGone)
    ));
}
