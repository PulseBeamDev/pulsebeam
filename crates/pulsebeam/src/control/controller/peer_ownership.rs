use std::{num::NonZeroU64, time::Duration};

use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    CloseReason, ControllerActor, ControllerCommand, ControllerSender, SystemTime, ownership,
};
use crate::entity::EndpointKey;
use ownership::{
    ClaimRank,
    observations::{Error, Observations, Session},
};

const MAX_ORIGINS: usize = 64;
const MAX_CLAIMS_PER_ORIGIN: usize = 4096;
const MAX_REMOTE_FRESHNESS: Duration = Duration::from_secs(30);
type Claims = Box<[(EndpointKey, ClaimRank)]>;
type Reply<T> = oneshot::Sender<Result<T, Error>>;

#[derive(Debug)]
pub struct Command(Box<Action>);

#[derive(Debug)]
enum Action {
    Open(NodeId, NonZeroU64, Reply<Session>),
    Close(Session, Reply<()>),
    Read(Reply<Claims>),
    Snapshot(Session, NonZeroU64, Claims, Instant, Reply<()>),
    Delta(
        Session,
        NonZeroU64,
        NonZeroU64,
        (EndpointKey, Option<ClaimRank>),
        Instant,
        Reply<()>,
    ),
}

/// Controller input downstream of authenticated peer sessions. This is not a
/// peer directory, and never interprets unauthenticated gossip as ownership.
#[derive(Clone)]
pub(crate) struct Control(ControllerSender);

impl Control {
    pub(crate) fn new(sender: ControllerSender) -> Self {
        Self(sender)
    }

    async fn exchange<T>(&self, action: impl FnOnce(Reply<T>) -> Action) -> Result<T, Error> {
        let (reply, result) = oneshot::channel();
        self.0
            .try_send(ControllerCommand::PeerOwnership(Command(Box::new(action(
                reply,
            )))))
            .map_err(|error| match error {
                pulsebeam_queue::TrySendError::Full(_) => Error::Capacity,
                pulsebeam_queue::TrySendError::Closed(_) => Error::Closed,
            })?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub(crate) async fn open(
        &self,
        node: NodeId,
        incarnation: NonZeroU64,
    ) -> Result<Session, Error> {
        self.exchange(|reply| Action::Open(node, incarnation, reply))
            .await
    }

    pub(crate) async fn close(&self, session: Session) -> Result<(), Error> {
        self.exchange(|reply| Action::Close(session, reply)).await
    }

    pub(crate) async fn read(&self) -> Result<Claims, Error> {
        self.exchange(Action::Read).await
    }

    pub(crate) async fn snapshot(
        &self,
        session: Session,
        revision: NonZeroU64,
        claims: Claims,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        if claims.len() > MAX_CLAIMS_PER_ORIGIN {
            return Err(Error::Capacity);
        }
        self.exchange(|reply| Action::Snapshot(session, revision, claims, fresh_until, reply))
            .await
    }

    pub(crate) async fn delta(
        &self,
        session: Session,
        base: NonZeroU64,
        revision: NonZeroU64,
        change: (EndpointKey, Option<ClaimRank>),
        fresh_until: Instant,
    ) -> Result<(), Error> {
        self.exchange(|reply| Action::Delta(session, base, revision, change, fresh_until, reply))
            .await
    }
}

fn locally_bounded_deadline(now: Instant, fresh_until: Instant) -> Instant {
    now.checked_add(MAX_REMOTE_FRESHNESS)
        .map_or(fresh_until, |limit| fresh_until.min(limit))
}

pub(super) fn new_observations() -> Observations {
    Observations::new(MAX_ORIGINS, MAX_CLAIMS_PER_ORIGIN)
}

impl ControllerActor {
    pub(super) fn process_peer_ownership(&mut self, Command(action): Command) {
        match *action {
            Action::Open(node, incarnation, reply) => {
                let _ = reply.send(self.open_ownership_peer(node, incarnation));
            }
            Action::Close(session, reply) => {
                let _ = reply.send(self.close_ownership_peer(session));
            }
            Action::Read(reply) => {
                let _ = reply.send(self.local_ownership_snapshot().map(Vec::into_boxed_slice));
            }
            Action::Snapshot(session, revision, claims, fresh_until, reply) => {
                let _ = reply.send(self.apply_ownership_snapshot(
                    session,
                    revision,
                    &claims,
                    fresh_until,
                ));
            }
            Action::Delta(session, base, revision, change, fresh_until, reply) => {
                let _ = reply.send(self.apply_ownership_delta(
                    session,
                    base,
                    revision,
                    change,
                    fresh_until,
                ));
            }
        }
    }

    fn open_ownership_peer(
        &mut self,
        node: NodeId,
        incarnation: NonZeroU64,
    ) -> Result<Session, Error> {
        self.peer_ownership.open(node, incarnation)
    }

    fn close_ownership_peer(&mut self, session: Session) -> Result<(), Error> {
        self.peer_ownership.close(session)
    }

    fn apply_ownership_snapshot(
        &mut self,
        session: Session,
        revision: NonZeroU64,
        claims: &[(EndpointKey, ClaimRank)],
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let now = Instant::now();
        self.peer_ownership.snapshot(
            session,
            revision,
            claims,
            now,
            locally_bounded_deadline(now, fresh_until),
        )?;
        for &(endpoint, claim) in claims {
            self.apply_remote_defeat(endpoint, claim);
        }
        Ok(())
    }

    fn apply_ownership_delta(
        &mut self,
        session: Session,
        base: NonZeroU64,
        revision: NonZeroU64,
        change: (EndpointKey, Option<ClaimRank>),
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let (endpoint, claim) = change;
        let now = Instant::now();
        self.peer_ownership.delta(
            session,
            base,
            revision,
            endpoint,
            claim,
            now,
            locally_bounded_deadline(now, fresh_until),
        )?;
        if let Some(claim) = claim {
            self.apply_remote_defeat(endpoint, claim);
        }
        Ok(())
    }

    fn local_ownership_snapshot(&self) -> Result<Vec<(EndpointKey, ClaimRank)>, Error> {
        let wall_now = SystemTime::now();
        let mut claims = Vec::new();
        for meta in self.core.registry.connections() {
            let (Some(endpoint), Some(rank), Some(lease)) =
                (meta.endpoint(), meta.claim, meta.authorization.as_ref())
            else {
                continue;
            };
            if lease.is_expired_at(wall_now)
                || !lease.allows_current_room()
                || self
                    .core
                    .registry
                    .get_endpoint(endpoint)
                    .is_none_or(|current| current.connection_id != meta.connection_id)
            {
                continue;
            }
            if claims.len() == MAX_CLAIMS_PER_ORIGIN {
                return Err(Error::Capacity);
            }
            claims.push((endpoint, rank));
        }
        claims.sort_unstable_by_key(|&(endpoint, _)| endpoint);
        Ok(claims)
    }

    fn apply_remote_defeat(&mut self, endpoint: EndpointKey, winner: ClaimRank) {
        let Some(previous) = self
            .core
            .registry
            .get_endpoint(endpoint)
            .filter(|meta| meta.claim.is_some_and(|local| local < winner))
            .cloned()
        else {
            return;
        };
        self.retire_defeated_lineage(&previous, winner);
        self.terminate_incarnation(
            endpoint.participant,
            previous,
            true,
            Some(CloseReason::ParticipantReplaced),
        );
    }
}
