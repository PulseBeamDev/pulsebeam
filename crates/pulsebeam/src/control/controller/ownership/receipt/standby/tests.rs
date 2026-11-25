use super::*;
use crate::control::controller::ownership::receipt::{PeerTrust, StandaloneTrust};
use crate::entity::{ParticipantId, ProjectRoomKey, RoomId, TrackKind};

#[test]
fn parallel_encoders_do_not_replace_the_selected_source_without_a_switch() {
    let mut rng = pulsebeam_runtime::rand::seeded_rng(38);
    let trust = StandaloneTrust::new(&mut rng);
    let owner = Owner {
        endpoint: EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("main").unwrap()),
            participant: ParticipantId::new("publisher").unwrap(),
        },
        issuer: trust.issuer(),
        rank: ClaimRank::root(ConnectionId::new()),
        authorization: 1,
        expires_at: 130,
    };
    let primary = Source {
        node: owner.issuer.node,
        generation: owner.issuer.incarnation,
        connection: owner.rank.connection(),
        publication: owner
            .rank
            .connection()
            .fixture_publication(TrackKind::Video, "camera"),
        track: TrackId::new("camera").unwrap(),
    };
    let independent = || {
        let connection = ConnectionId::new();
        Source {
            node: NodeId::new(),
            connection,
            publication: connection.fixture_publication(TrackKind::Video, "camera"),
            ..primary
        }
    };
    let a = independent();
    let b = independent();
    let grant_a = owner.issue(&trust, a, 120, 100).unwrap();
    let grant_b = owner.issue(&trust, b, 125, 100).unwrap();
    let mut candidates = Candidates::new(owner, primary).unwrap();
    let revision = |value| NonZeroU64::new(value).unwrap();
    assert_eq!(
        candidates.admit(owner, &trust, b, grant_a, 100),
        Err(Error::Invalid)
    );
    assert_eq!(
        candidates.select(owner, &trust, revision(1), a, 100),
        Err(Error::NoAuthority)
    );
    candidates.admit(owner, &trust, a, grant_a, 100).unwrap();
    candidates.admit(owner, &trust, b, grant_b, 100).unwrap();
    assert_eq!(candidates.standbys.len(), 2);
    assert!(candidates.selected(owner, &trust, 100) == Some(primary));
    candidates
        .select(owner, &trust, revision(1), a, 101)
        .unwrap();
    assert!(candidates.selected(owner, &trust, 101) == Some(a));
    candidates
        .select(owner, &trust, revision(2), b, 102)
        .unwrap();
    assert_eq!(
        candidates.select(owner, &trust, revision(2), b, 103),
        Ok(())
    );
    assert_eq!(
        candidates.select(owner, &trust, revision(1), a, 103),
        Err(Error::Invalid)
    );
    assert_eq!(
        candidates.select(owner, &trust, revision(2), a, 103),
        Err(Error::Invalid)
    );
    assert!(candidates.selected(owner, &trust, 103) == Some(b));
    let restarted = StandaloneTrust::new(&mut rng);
    assert!(candidates.selected(owner, &restarted, 103).is_none());
    assert!(candidates.selected(owner, &trust, 125).is_none());
    assert!(
        candidates
            .selected(
                Owner {
                    authorization: 2,
                    ..owner
                },
                &trust,
                105
            )
            .is_none()
    );
    let renewal = owner.issue(&trust, b, 130, 125).unwrap();
    candidates.admit(owner, &trust, b, renewal, 125).unwrap();
    assert!(candidates.selected(owner, &trust, 125).is_none());
    assert_eq!(
        candidates.select(owner, &trust, revision(2), b, 125),
        Err(Error::Invalid)
    );
    candidates
        .select(owner, &trust, revision(3), primary, 125)
        .unwrap();
    assert!(candidates.selected(owner, &trust, 125) == Some(primary));
}

#[test]
fn grant_matches_only_the_live_owner_and_exact_encoder() {
    let mut rng = pulsebeam_runtime::rand::seeded_rng(37);
    let mut trust = StandaloneTrust::new(&mut rng);
    let owner = Owner {
        endpoint: EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("main").unwrap()),
            participant: ParticipantId::new("publisher").unwrap(),
        },
        issuer: trust.issuer(),
        rank: ClaimRank::root(ConnectionId::new()),
        authorization: 1,
        expires_at: 125,
    };
    let source = Source {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
        connection: ConnectionId::new(),
        publication: ConnectionId::new().fixture_publication(TrackKind::Video, "camera"),
        track: TrackId::new("camera").unwrap(),
    };
    let grant = owner.issue(&trust, source, 120, 100).unwrap();
    assert_eq!(owner.validate(&trust, source, grant, 119), Ok(120));
    assert_eq!(
        owner.validate(&trust, source, grant, 120),
        Err(Error::Expired)
    );
    assert!(matches!(
        owner.issue(&trust, source, 126, 100),
        Err(Error::Expired)
    ));
    for other_source in [
        Source {
            node: NodeId::new(),
            ..source
        },
        Source {
            generation: NonZeroU64::new(2).unwrap(),
            ..source
        },
        Source {
            connection: ConnectionId::new(),
            ..source
        },
        Source {
            publication: ConnectionId::new().fixture_publication(TrackKind::Video, "camera"),
            ..source
        },
        Source {
            track: TrackId::new("screen").unwrap(),
            ..source
        },
    ] {
        assert_eq!(
            owner.validate(&trust, other_source, grant, 119),
            Err(Error::Invalid)
        );
    }
    let successor = Owner {
        rank: owner.rank.successor(ConnectionId::new()).unwrap(),
        ..owner
    };
    assert_eq!(
        successor.validate(&trust, source, grant, 119),
        Err(Error::Invalid)
    );
    let other_room = Owner {
        endpoint: EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("other").unwrap()),
            ..owner.endpoint
        },
        ..owner
    };
    assert_eq!(
        other_room.validate(&trust, source, grant, 119),
        Err(Error::Invalid)
    );

    let reauthorized = Owner {
        authorization: 2,
        ..owner
    };
    assert_eq!(
        reauthorized.validate(&trust, source, grant, 119),
        Err(Error::Invalid)
    );

    let restart = StandaloneTrust::new(&mut rng);
    assert_eq!(
        owner.validate(&restart, source, grant, 119),
        Err(Error::NoAuthority)
    );
    assert!(matches!(
        owner.issue(&restart, source, 120, 119),
        Err(Error::NoAuthority)
    ));
    trust
        .record_issuance(owner.endpoint, owner.rank, 125, 100)
        .unwrap();
    trust.retire(owner.endpoint, owner.rank);
    assert_eq!(
        owner.validate(&trust, source, grant, 119),
        Err(Error::Retired)
    );
    assert!(matches!(
        owner.issue(&trust, source, 120, 119),
        Err(Error::Retired)
    ));
}
