use super::*;
use std::cell::Cell;

fn owner() -> Owner {
    Owner {
        domain: [1; 16],
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
        connection: ConnectionId::new(),
    }
}

fn initial() -> anyhow::Result<Handoff> {
    Ok(Handoff {
        scope: Scope {
            project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room: RoomId::new("studio")?,
            participant: ParticipantId::new("publisher")?,
        },
        sequence: NonZeroU64::MIN,
        predecessor: owner(),
        successor: owner(),
    })
}

#[test]
fn reopened_checkpoint_rejects_replay_conflicts_and_forks() {
    let directory = tempfile::tempdir().unwrap();
    let first = initial().unwrap();
    let second = Handoff {
        sequence: NonZeroU64::new(2).unwrap(),
        predecessor: first.successor,
        successor: owner(),
        ..first
    };
    {
        let mut store = Store::open(directory.path()).unwrap();
        assert!(Store::open(directory.path()).is_err());
        assert!(store.commit(second).is_err());
        store.commit(first).unwrap();
        store.commit(first).unwrap();
        assert!(
            store
                .commit(Handoff {
                    successor: owner(),
                    ..first
                })
                .is_err()
        );
    }
    let mut store = Store::open(directory.path()).unwrap();
    assert!(store.current(first.scope).unwrap() == Some(first));
    assert!(
        store
            .commit(Handoff {
                predecessor: first.predecessor,
                ..second
            })
            .is_err()
    );
    store.commit(second).unwrap();
    drop(store);
    let mut store = Store::open(directory.path()).unwrap();
    assert!(store.current(first.scope).unwrap() == Some(second));
    assert!(store.commit(first).is_err());
    store.commit(second).unwrap();
}

#[test]
fn a_failed_durability_barrier_cannot_acknowledge_a_transfer() {
    let directory = tempfile::tempdir().unwrap();
    let first = initial().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    assert!(
        store
            .commit_with_sync(first, |_| Err(std::io::Error::other("fixture sync error")))
            .is_err()
    );
    assert!(store.current(first.scope).is_err());
    assert!(store.commit(first).is_err());
    drop(store);
    let mut store = Store::open(directory.path()).unwrap();
    assert!(store.current(first.scope).unwrap().is_none());

    let syncs = Cell::new(0);
    assert!(
        store
            .commit_with_sync(first, |file| {
                let count = syncs.get();
                syncs.set(count + 1);
                if count == 0 {
                    file.sync_all()
                } else {
                    Err(std::io::Error::other("fixture directory sync error"))
                }
            })
            .is_err()
    );
    assert_eq!(syncs.get(), 2);
    assert!(store.current(first.scope).is_err());
    drop(store);
    let store = Store::open(directory.path()).unwrap();
    assert!(store.current(first.scope).unwrap() == Some(first));
    drop(store);
    std::fs::write(directory.path().join(STATE), b"invalid checkpoint").unwrap();
    assert!(Store::open(directory.path()).is_err());
}
