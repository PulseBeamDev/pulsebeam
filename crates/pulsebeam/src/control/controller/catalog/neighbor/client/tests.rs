use super::*;
use crate::control::controller::ConnectionId;
use crate::entity::{ProjectId, ProjectRoomKey, RoomId};

fn order() -> Order {
    Order {
        demand: Demand {
            watch: CurrentFuture {
                watch_id: 1,
                scope: ReadScope {
                    viewer: ConnectionId::new(),
                    authorization: 1,
                    room: ProjectRoomKey {
                        project: ProjectId::new(),
                        room: RoomId::new("watched").unwrap(),
                    },
                },
            },
            round: NonZeroU64::MIN,
            after: None,
            revision: None,
        },
        declaration: DeclarationId {
            stream: NonZeroU64::MIN,
            interest: NonZeroU64::MIN,
            changed_at: NonZeroU64::MIN,
        },
    }
}

const REVISION: owner::Revision = owner::Revision {
    presence: 2,
    publications: 3,
};

#[test]
fn exact_live_watch_and_declaration_are_required_and_reply_is_single_use() {
    let start = Instant::now();
    let initial = order();
    let other = NonZeroU64::new(2).unwrap();
    let exhausted = || Ok(owner::NeighborWindow::Exhausted { revision: REVISION });
    let mut replacement = initial;
    replacement.demand.watch.scope.authorization = 2;
    let mut reconnect = initial;
    reconnect.declaration.stream = other;
    let mut new_cut = initial;
    new_cut.declaration.changed_at = other;
    let mut advanced = initial;
    advanced.demand.after = Some(ParticipantId::new("next").unwrap());
    for current in [replacement, reconnect, new_cut, advanced] {
        let mut reads = Reads::default();
        reads.send(&[initial], start, |id, _, _, _| {
            assert_eq!(id, NonZeroU64::MIN);
            true
        });
        assert!(
            reads
                .receive(other, exhausted(), start, &[initial])
                .is_none()
        );
        assert!(
            reads
                .receive(NonZeroU64::MIN, exhausted(), start, &[current])
                .is_none()
        );
        assert!(reads.window(initial, start).is_none());
        assert!(reads.window(current, start).is_none());
        reads.send(&[current], start, |id, _, _, _| {
            assert_eq!(id, other);
            true
        });
        assert!(matches!(
            reads.receive(other, exhausted(), start, &[current]),
            Some((_, Ok(())))
        ));
        assert!(
            reads
                .receive(other, exhausted(), start, &[current])
                .is_none()
        );
        assert!(reads.window(current, start).is_some());
        reads.prune(&[current], start + WINDOW_TTL, |watch| {
            assert!(watch == current.demand.watch);
        });
        assert!(reads.window(current, start).is_none());
    }
}

#[test]
fn scan_progress_is_not_a_row_or_exhaustion_and_refill_keeps_the_owner_revision() {
    let now = Instant::now();
    let order = order();
    let mut reads = Reads::default();
    reads.send(&[order], now, |_, _, after, revision| {
        assert!(after.is_none() && revision.is_none());
        true
    });
    let result = owner::NeighborWindow::ScanMore {
        revision: REVISION,
        scanned_through: "hidden".into(),
    };
    assert!(matches!(
        reads.receive(NonZeroU64::MIN, Ok(result), now, &[order]),
        Some((_, Ok(())))
    ));
    let window = reads.window(order, now).unwrap();
    assert!(matches!(window.next(None), Ok(ReadStep::Refill)));
    assert!(window.fresh_until() > now);
    let second = NonZeroU64::new(2).unwrap();
    reads.send(&[order], now, |id, _, after, revision| {
        assert_eq!(id, second);
        assert_eq!(after.as_ref().map(ParticipantId::as_str), Some("hidden"));
        assert_eq!(revision, Some(REVISION));
        true
    });
    let row = wire::CatalogParticipantEntry {
        participant: Some(wire::Participant {
            room_id: "watched".into(),
            participant_id: "visible".into(),
        }),
        ..Default::default()
    };
    let result = owner::NeighborWindow::Row {
        revision: REVISION,
        candidate: owner::Candidate {
            canonical: "visible".into(),
            entry: Some(row),
        },
    };
    assert!(matches!(
        reads.receive(second, Ok(result), now, &[order]),
        Some((_, Ok(())))
    ));
    let window = reads.window(order, now).unwrap();
    assert!(matches!(window.next(None), Ok(ReadStep::Row(row)) if row.canonical == "visible"));
    let mut order = order;
    order.demand.after = Some(ParticipantId::new("visible").unwrap());
    let third = NonZeroU64::new(3).unwrap();
    reads.send(&[order], now, |id, _, _, revision| {
        assert_eq!((id, revision), (third, Some(REVISION)));
        true
    });
    let changed = owner::NeighborWindow::Exhausted {
        revision: owner::Revision {
            publications: 4,
            ..REVISION
        },
    };
    assert!(matches!(
        reads.receive(third, Ok(changed), now, &[order]),
        Some((_, Err(wire::FailureCode::ResourceGone)))
    ));
    assert!(reads.window(order, now).is_none());
}
