use std::time::Duration;

use super::*;
use crate::control::controller::ownership::receipt::{issue_accepted, validate_successor};

const LEASE: Duration = Duration::from_secs(10);

impl Trust {
    fn apply(
        &mut self,
        rev: NonZeroU64,
        checkpoint: Checkpoint,
        now: Instant,
        deadline: Instant,
    ) -> Result<Vec<Key>, Error> {
        let origin = (checkpoint.issuer.node, checkpoint.issuer.incarnation);
        let checkpoint = self.validate_checkpoint(origin, rev, checkpoint, now, deadline)?;
        self.install_checkpoint(checkpoint)
    }
}

fn pair() -> (Trust, Trust) {
    let a = Issuer {
        trust_domain: [23; 16],
        node: NodeId::new(),
        incarnation: NonZeroU64::MIN,
    };
    let b = Issuer {
        node: NodeId::new(),
        ..a
    };
    let a_key = ApiSigningKey::from_seed([31; 32]);
    let b_key = ApiSigningKey::from_seed([32; 32]);
    let a_pub = a_key.public_key();
    let b_pub = b_key.public_key();
    (
        Trust::new(a, a_key, b, b_pub).unwrap(),
        Trust::new(b, b_key, a, a_pub).unwrap(),
    )
}

fn endpoint() -> EndpointKey {
    EndpointKey {
        room: ProjectRoomKey::fixture(RoomId::new("stage").unwrap()),
        participant: ParticipantId::new("sender").unwrap(),
    }
}

fn rev(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

#[test]
fn expired_view_does_not_revoke_a_covered_receipt_or_authorize_admission() {
    let (mut a, mut b) = pair();
    let key = endpoint();
    let rank = ClaimRank::root(ConnectionId::new());
    let candidate = ConnectionId::new();
    let now = Instant::now();
    let deadline = now + LEASE;
    assert!(a.eligible_key(b.issuer()).is_some());
    assert_eq!(
        issue_accepted(&mut a, key, rank, 1000).err(),
        Some(Error::Unfresh)
    );
    a.apply(rev(1), b.checkpoint(), now, deadline).unwrap();
    let issued = issue_accepted(&mut a, key, rank, 1000).unwrap();
    assert_eq!(
        validate_successor(&b, &issued.proof, key, candidate, 1000),
        Err(Error::Unfresh)
    );
    b.apply(rev(1), a.checkpoint(), now, deadline).unwrap();
    assert_eq!(
        validate_successor(&b, &issued.proof, key, candidate, 1000),
        rank.successor(candidate).ok_or(Error::Invalid)
    );
    let lineage = (key, rank.lineage());
    assert_eq!(
        b.check_coverage(a.issuer(), lineage, 1301),
        Err(Error::Unfresh)
    );
    for verifier in [&a, &b] {
        assert!(!verifier.admission_ready(deadline));
        assert_eq!(
            validate_successor(verifier, &issued.proof, key, candidate, 1001),
            rank.successor(candidate).ok_or(Error::Invalid)
        );
    }
}

#[test]
fn retirement_cannot_be_forgotten_on_expiry_or_epoch_mismatch() {
    let (mut a, mut b) = pair();
    let endpoint = endpoint();
    let rank = ClaimRank::root(ConnectionId::new());
    let now = Instant::now();
    let deadline = now + LEASE;
    a.apply(rev(1), b.checkpoint(), now, deadline).unwrap();
    let issued = issue_accepted(&mut a, endpoint, rank, 1000).unwrap();
    let old = b.checkpoint();
    b.retire(endpoint, rank);
    assert_eq!(
        a.apply(rev(2), b.checkpoint(), deadline, deadline),
        Err(Error::Unfresh)
    );
    let learned = a.apply(rev(2), b.checkpoint(), now, deadline).unwrap();
    assert_eq!(learned, [(endpoint, rank.lineage())]);
    assert_eq!(a.apply(rev(3), old, now, deadline), Err(Error::Invalid));
    assert_eq!(
        a.check_coverage(a.issuer(), (endpoint, rank.lineage()), 1300),
        Err(Error::Retired)
    );
    assert_eq!(
        issue_accepted(&mut a, endpoint, rank, 1001).err(),
        Some(Error::Retired)
    );
    let (_, fresh_epoch) = pair();
    assert_eq!(
        validate_successor(
            &fresh_epoch,
            &issued.proof,
            endpoint,
            ConnectionId::new(),
            1001
        ),
        Err(Error::Invalid)
    );
}

#[test]
fn pinned_peers_have_independent_revisions_and_cannot_install_each_others_checkpoints() {
    let (mut a, mut b) = pair();
    let c_key = ApiSigningKey::from_seed([33; 32]);
    let c_id = Issuer {
        node: NodeId::new(),
        ..a.issuer()
    };
    a.pin_peer(c_id, c_key.public_key()).unwrap();
    let mut c = Trust::new(c_id, c_key, a.issuer(), a.key.public_key()).unwrap();
    let now = Instant::now();
    let deadline = now + LEASE;
    let wrong_origin = (b.issuer().node, b.issuer().incarnation);
    assert!(matches!(
        a.validate_checkpoint(wrong_origin, rev(2), c.checkpoint(), now, deadline),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        a.validate_checkpoint((c_id.node, rev(2)), rev(1), c.checkpoint(), now, deadline),
        Err(Error::Invalid)
    ));

    a.apply(rev(7), b.checkpoint(), now, deadline).unwrap();
    assert!(!a.admission_ready(now));
    a.apply(rev(1), c.checkpoint(), now, deadline).unwrap();
    assert!(a.admission_ready(now));
    let endpoint = endpoint();
    let rank = ClaimRank::root(ConnectionId::new());
    c.retire(endpoint, rank);
    assert_eq!(
        a.apply(rev(2), c.checkpoint(), now, deadline),
        Ok(vec![(endpoint, rank.lineage())])
    );
    b.records
        .entry((endpoint, rank.lineage()))
        .or_default()
        .issued_through = 100;
    a.apply(rev(8), b.checkpoint(), now, deadline).unwrap();
    assert!(a.is_retired(endpoint, rank));
    assert!(!a.admission_ready(deadline));
}

#[test]
fn saturation_reserves_existing_fences_and_blocks_if_a_new_terminal_fact_cannot_fit() {
    let (mut a, mut b) = pair();
    let now = Instant::now();
    let deadline = now + LEASE;
    let endpoint = endpoint();
    a.apply(rev(1), b.checkpoint(), now, deadline).unwrap();
    let issued_rank = ClaimRank::root(ConnectionId::new());
    issue_accepted(&mut a, endpoint, issued_rank, 1000).unwrap();
    for _ in 1..CAPACITY {
        a.retire(endpoint, ClaimRank::root(ConnectionId::new()));
    }
    let overflow = ClaimRank::root(ConnectionId::new());
    assert_eq!(
        issue_accepted(&mut a, endpoint, overflow, 1000).err(),
        Some(Error::NoAuthority)
    );
    a.retire(endpoint, issued_rank);
    assert!(a.is_retired(endpoint, issued_rank));
    assert!(!a.checkpoint().blocked);
    a.retire(endpoint, overflow);
    assert!(a.checkpoint().blocked);
    let retired = b.apply(rev(1), a.checkpoint(), now, deadline).unwrap();
    assert_eq!(retired.len(), CAPACITY);
    assert_eq!(
        issue_accepted(&mut b, endpoint, ClaimRank::root(ConnectionId::new()), 1000).err(),
        Some(Error::NoAuthority)
    );
    assert!(b.is_retired(endpoint, issued_rank));
}
