use std::{collections::BTreeMap, num::NonZeroU64};

use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::time::Instant;

use super::ClaimRank;
use crate::entity::EndpointKey;

/// A locally allocated handle for one authenticated control session. A wire
/// message can never choose its own session or origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    node: NodeId,
    serial: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Capacity,
    Closed,
    Stale,
    NeedsSnapshot,
    Invalid,
}

#[derive(Default)]
struct Origin {
    incarnation: Option<NonZeroU64>,
    session: Option<NonZeroU64>,
    revision: Option<NonZeroU64>,
    resync_at_least: Option<NonZeroU64>,
    fresh_until: Option<Instant>,
    claims: BTreeMap<EndpointKey, ClaimRank>,
}

/// Current remote claims, never a historical winner floor. This is downstream
/// of peer authentication and does not establish receipt issuer eligibility.
/// Incarnation/revision watermarks are kept within the origin budget even when
/// the origin has no current claims.
pub(crate) struct Observations {
    origins: BTreeMap<NodeId, Origin>,
    last_session: u64,
    max_origins: usize,
    max_claims_per_origin: usize,
}

impl Observations {
    pub(crate) fn new(max_origins: usize, max_claims_per_origin: usize) -> Self {
        Self {
            origins: BTreeMap::new(),
            last_session: 0,
            max_origins,
            max_claims_per_origin,
        }
    }

    /// Only the authenticated peer adapter may open a session for an origin.
    /// Reopening an incarnation requires a new complete snapshot, not deltas.
    pub(crate) fn open(&mut self, node: NodeId, incarnation: NonZeroU64) -> Result<Session, Error> {
        if !self.origins.contains_key(&node) && self.origins.len() >= self.max_origins {
            return Err(Error::Capacity);
        }
        let origin = self.origins.entry(node).or_default();
        if origin
            .incarnation
            .is_some_and(|current| incarnation < current)
        {
            return Err(Error::Stale);
        }
        let serial = self
            .last_session
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(Error::Capacity)?;
        self.last_session = serial.get();
        if origin.incarnation != Some(incarnation) {
            origin.incarnation = Some(incarnation);
            origin.revision = None;
            origin.resync_at_least = None;
        }
        origin.session = Some(serial);
        origin.withdraw();
        Ok(Session { node, serial })
    }

    pub(crate) fn close(&mut self, session: Session) -> Result<(), Error> {
        let origin = self.origin(session)?;
        origin.session = None;
        origin.withdraw();
        Ok(())
    }

    /// Replaces the complete claim view for this origin incarnation. An empty
    /// snapshot withdraws all claims without forgetting the origin watermark.
    pub(crate) fn snapshot(
        &mut self,
        session: Session,
        revision: NonZeroU64,
        claims: &[(EndpointKey, ClaimRank)],
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let limit = self.max_claims_per_origin;
        let origin = self.origin(session)?;
        origin.check_revision(revision, now, fresh_until)?;
        if origin
            .resync_at_least
            .is_some_and(|minimum| revision < minimum)
        {
            return Err(Error::Stale);
        }
        if claims.len() > limit {
            origin.require_snapshot(revision);
            return Err(Error::Capacity);
        }
        let snapshot: BTreeMap<_, _> = claims.iter().copied().collect();
        if snapshot.len() != claims.len() {
            origin.require_snapshot(revision);
            return Err(Error::Invalid);
        }
        origin.resync_at_least = None;
        origin.claims = snapshot;
        origin.revision = Some(revision);
        origin.fresh_until = Some(fresh_until);
        Ok(())
    }

    /// The base watermark makes a missing withdrawal equivalent to loss of
    /// synchronization. No later delta restores authority until a full view.
    pub(crate) fn delta(
        &mut self,
        session: Session,
        base: NonZeroU64,
        revision: NonZeroU64,
        endpoint: EndpointKey,
        claim: Option<ClaimRank>,
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let limit = self.max_claims_per_origin;
        let origin = self.origin(session)?;
        origin.check_revision(revision, now, fresh_until)?;
        if !origin.is_fresh(now) || origin.revision != Some(base) {
            origin.require_snapshot(revision);
            return Err(Error::NeedsSnapshot);
        }
        if let Some(claim) = claim {
            if origin.claims.len() >= limit && !origin.claims.contains_key(&endpoint) {
                origin.require_snapshot(revision);
                return Err(Error::Capacity);
            }
            origin.claims.insert(endpoint, claim);
        } else {
            origin.claims.remove(&endpoint);
        }
        origin.revision = Some(revision);
        origin.fresh_until = Some(fresh_until);
        Ok(())
    }

    pub(crate) fn highest(&self, endpoint: EndpointKey, now: Instant) -> Option<ClaimRank> {
        self.origins
            .values()
            .filter(|origin| origin.is_fresh(now))
            .filter_map(|origin| origin.claims.get(&endpoint).copied())
            .max()
    }

    fn origin(&mut self, session: Session) -> Result<&mut Origin, Error> {
        self.origins
            .get_mut(&session.node)
            .filter(|origin| origin.session == Some(session.serial))
            .ok_or(Error::Stale)
    }
}

impl Origin {
    fn check_revision(
        &self,
        revision: NonZeroU64,
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        if self.revision.is_some_and(|current| revision <= current) || now >= fresh_until {
            return Err(Error::Stale);
        }
        Ok(())
    }

    fn is_fresh(&self, now: Instant) -> bool {
        self.fresh_until.is_some_and(|deadline| now < deadline)
    }

    fn require_snapshot(&mut self, revision: NonZeroU64) {
        self.resync_at_least = self.resync_at_least.max(Some(revision));
        self.withdraw();
    }

    fn withdraw(&mut self) {
        self.fresh_until = None;
        self.claims.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::entity::{ConnectionId, ParticipantId, ProjectRoomKey, RoomId};

    fn revision(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn endpoint(name: &str) -> EndpointKey {
        EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("stage").unwrap()),
            participant: ParticipantId::new(name).unwrap(),
        }
    }

    fn rank(value: u8) -> ClaimRank {
        ClaimRank::root(ConnectionId::from_bytes([
            0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, value,
        ]))
    }

    #[test]
    fn current_claims_survive_a_departed_winner_without_a_historical_rank_floor() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut view = Observations::new(2, 1);
        let survivor = view.open(NodeId::new(), revision(1)).unwrap();
        let other = NodeId::new();
        let stale = view.open(other, revision(1)).unwrap();
        let endpoint = endpoint("alice");
        view.snapshot(survivor, revision(1), &[(endpoint, rank(1))], now, deadline)
            .unwrap();
        view.snapshot(stale, revision(1), &[(endpoint, rank(2))], now, deadline)
            .unwrap();
        assert_eq!(view.highest(endpoint, now), Some(rank(2)));
        view.close(stale).unwrap();
        assert_eq!(view.highest(endpoint, now), Some(rank(1)));
        let returning = view.open(other, revision(2)).unwrap();
        assert_eq!(view.open(other, revision(1)), Err(Error::Stale));
        assert_eq!(
            view.snapshot(stale, revision(5), &[(endpoint, rank(2))], now, deadline),
            Err(Error::Stale)
        );
        view.snapshot(returning, revision(1), &[], now, deadline)
            .unwrap();
        assert_eq!(view.highest(endpoint, now), Some(rank(1)));
        assert_eq!(view.highest(endpoint, deadline), None);
    }

    #[test]
    fn gaps_and_reconnect_require_a_current_complete_snapshot_without_evicting_watermarks() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut view = Observations::new(1, 1);
        let node = NodeId::new();
        let session = view.open(node, revision(1)).unwrap();
        let alice = endpoint("alice");
        let bob = endpoint("bob");
        view.snapshot(session, revision(1), &[(alice, rank(1))], now, deadline)
            .unwrap();
        assert_eq!(
            view.delta(
                session,
                revision(2),
                revision(3),
                bob,
                Some(rank(2)),
                now,
                deadline
            ),
            Err(Error::NeedsSnapshot)
        );
        assert_eq!(view.highest(alice, now), None);
        assert_eq!(
            view.snapshot(session, revision(2), &[(alice, rank(1))], now, deadline),
            Err(Error::Stale)
        );
        view.snapshot(session, revision(3), &[(bob, rank(2))], now, deadline)
            .unwrap();
        let resumed = view.open(node, revision(1)).unwrap();
        assert_eq!(view.close(session), Err(Error::Stale));
        assert_eq!(view.open(NodeId::new(), revision(1)), Err(Error::Capacity));
        assert_eq!(
            view.delta(resumed, revision(3), revision(4), bob, None, now, deadline),
            Err(Error::NeedsSnapshot)
        );
        assert_eq!(
            view.snapshot(
                resumed,
                revision(4),
                &[(alice, rank(1)), (bob, rank(2))],
                now,
                deadline
            ),
            Err(Error::Capacity)
        );
        view.snapshot(resumed, revision(4), &[(bob, rank(2))], now, deadline)
            .unwrap();
        view.delta(resumed, revision(4), revision(5), bob, None, now, deadline)
            .unwrap();
        assert_eq!(view.highest(bob, now), None);
        assert_eq!(
            view.snapshot(resumed, revision(4), &[(bob, rank(2))], now, deadline),
            Err(Error::Stale)
        );
    }
}
