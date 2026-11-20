pub(super) mod transfer;

use std::{collections::BTreeSet, num::NonZeroU64, time::Duration};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::NetworkId;
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    ConnectionEnd, ControllerActor, ControllerCommand, ControllerSender, SystemTime, ownership,
};
use crate::entity::EndpointKey;
use ownership::{
    ClaimRank,
    observations::{EndpointFact, Error, Observations, PublicationFact, Session, Snapshot},
};

const MAX_ORIGINS: usize = 64;
pub(super) const MAX_CLAIMS_PER_ORIGIN: usize = 4096;
pub(super) const MAX_PUBLICATIONS_PER_ORIGIN: usize = 4096;
const MAX_REMOTE_FRESHNESS: Duration = Duration::from_secs(30);
type Reply<T> = oneshot::Sender<Result<T, Error>>;

#[derive(Debug)]
pub struct Command(Box<Action>);

#[derive(Debug)]
enum Action {
    Open(NodeId, NonZeroU64, Option<NetworkId>, Reply<Session>),
    Close(Session, Reply<()>),
    Read(Reply<Snapshot>),
    Snapshot(Session, NonZeroU64, Snapshot, Instant, Reply<()>),
    RejectOversized(Session, NonZeroU64, Instant, Reply<()>),
    Delta(
        Session,
        NonZeroU64,
        NonZeroU64,
        (EndpointKey, Option<EndpointFact>),
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
        self.exchange(|reply| Action::Open(node, incarnation, None, reply))
            .await
    }

    pub(crate) async fn open_linked(
        &self,
        node: NodeId,
        incarnation: NonZeroU64,
        network: NetworkId,
    ) -> Result<Session, Error> {
        self.exchange(|reply| Action::Open(node, incarnation, Some(network), reply))
            .await
    }

    pub(crate) async fn close(&self, session: Session) -> Result<(), Error> {
        self.exchange(|reply| Action::Close(session, reply)).await
    }

    pub(crate) async fn read(&self) -> Result<Snapshot, Error> {
        self.exchange(Action::Read).await
    }

    pub(crate) async fn snapshot(
        &self,
        session: Session,
        revision: NonZeroU64,
        endpoints: Snapshot,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        if endpoints.len() > MAX_CLAIMS_PER_ORIGIN
            || endpoints.iter().fold(0usize, |count, (_, fact)| {
                count.saturating_add(fact.publications.len())
            }) > MAX_PUBLICATIONS_PER_ORIGIN
        {
            return self
                .exchange(|reply| Action::RejectOversized(session, revision, fresh_until, reply))
                .await;
        }
        self.exchange(|reply| Action::Snapshot(session, revision, endpoints, fresh_until, reply))
            .await
    }

    pub(crate) async fn delta(
        &self,
        session: Session,
        base: NonZeroU64,
        revision: NonZeroU64,
        change: (EndpointKey, Option<EndpointFact>),
        fresh_until: Instant,
    ) -> Result<(), Error> {
        if change
            .1
            .as_ref()
            .is_some_and(|fact| fact.publications.len() > MAX_PUBLICATIONS_PER_ORIGIN)
        {
            return self
                .exchange(|reply| Action::RejectOversized(session, revision, fresh_until, reply))
                .await;
        }
        self.exchange(|reply| Action::Delta(session, base, revision, change, fresh_until, reply))
            .await
    }
}

fn locally_bounded_deadline(now: Instant, fresh_until: Instant) -> Instant {
    now.checked_add(MAX_REMOTE_FRESHNESS)
        .map_or(fresh_until, |limit| fresh_until.min(limit))
}

pub(super) fn new_observations() -> Observations {
    Observations::new(
        MAX_ORIGINS,
        MAX_CLAIMS_PER_ORIGIN,
        MAX_PUBLICATIONS_PER_ORIGIN,
    )
}

impl ControllerActor {
    pub(super) fn process_peer_ownership(&mut self, Command(action): Command) {
        match *action {
            Action::Open(node, incarnation, network, reply) => {
                let _ = reply.send(self.open_ownership_peer(node, incarnation, network));
            }
            Action::Close(session, reply) => {
                let _ = reply.send(self.close_ownership_peer(session));
            }
            Action::Read(reply) => {
                let _ = reply.send(self.local_endpoint_snapshot());
            }
            Action::Snapshot(session, revision, endpoints, fresh_until, reply) => {
                let _ = reply.send(self.apply_ownership_snapshot(
                    session,
                    revision,
                    endpoints,
                    fresh_until,
                ));
            }
            Action::RejectOversized(session, revision, fresh_until, reply) => {
                let now = Instant::now();
                let affected: Vec<_> = self.peer_ownership.endpoints_for_session(session).collect();
                let result = self.peer_ownership.reject_oversized(
                    session,
                    revision,
                    now,
                    locally_bounded_deadline(now, fresh_until),
                );
                for endpoint in affected {
                    self.reconcile_peer_sources(endpoint, now);
                }
                let _ = reply.send(result);
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
        network: Option<NetworkId>,
    ) -> Result<Session, Error> {
        if network.is_some_and(|network| {
            self.links
                .as_ref()
                .is_none_or(|links| !links.has_peer(network))
        }) {
            return Err(Error::Closed);
        }
        let affected: Vec<_> = self.peer_ownership.endpoints_for_node(node).collect();
        let session = self.peer_ownership.open(node, incarnation, network)?;
        if let (Some(link), Some(links)) = (network, self.links.as_mut()) {
            links.identify_peer(link, node);
        }
        let now = Instant::now();
        for endpoint in affected {
            self.reconcile_peer_sources(endpoint, now);
        }
        Ok(session)
    }

    fn close_ownership_peer(&mut self, session: Session) -> Result<(), Error> {
        let affected: Vec<_> = self.peer_ownership.endpoints_for_session(session).collect();
        self.peer_ownership.close(session)?;
        let now = Instant::now();
        for endpoint in affected {
            self.reconcile_peer_sources(endpoint, now);
        }
        Ok(())
    }

    pub(super) fn expire_peer_sources(&mut self, now: Instant) {
        for endpoint in self
            .peer_ownership
            .expire(now)
            .into_iter()
            .collect::<BTreeSet<_>>()
        {
            self.reconcile_peer_sources(endpoint, now);
        }
    }

    pub(super) fn detach_peer_sources(&mut self, network: NetworkId) {
        let now = Instant::now();
        for endpoint in self.peer_ownership.detach_network(network) {
            self.reconcile_peer_sources(endpoint, now);
        }
    }

    pub(super) fn reconcile_peer_sources(&mut self, endpoint: EndpointKey, now: Instant) {
        let desired = self
            .core
            .registry
            .get_endpoint(endpoint)
            .is_none()
            .then(|| {
                self.peer_ownership.ingress(endpoint, now, |rank| {
                    !self.peer_lineage_retired(endpoint, rank)
                })
            })
            .flatten();
        let mut install = Vec::new();
        if let Some((network, fact)) = desired {
            let connection = fact.claim.connection();
            for publication in &fact.publications {
                if let Some(track) = publication.media_track(endpoint, connection) {
                    install.push((track, network));
                }
            }
        }
        let current: Vec<_> = self
            .lifecycle
            .ingress_sources(endpoint)
            .map(|(track, network)| (track.meta().clone(), network))
            .collect();
        let same = |meta: &crate::track::TrackMeta,
                    peer: NetworkId,
                    track: &crate::track::Track,
                    network: NetworkId| {
            meta.id == track.id()
                && meta.connection_id == track.meta().connection_id
                && peer == network
        };
        for (meta, peer) in &current {
            if !install
                .iter()
                .any(|(track, network)| same(meta, *peer, track, *network))
                && let Some(outcome) =
                    self.lifecycle
                        .unpublish(endpoint, meta.id, &self.core.registry, now)
            {
                self.publish_track_lifecycle(outcome);
                self.notify_scoped_source_changed(meta);
            }
        }
        for (track, network) in install {
            if current
                .iter()
                .any(|(meta, peer)| same(meta, *peer, &track, network))
            {
                continue;
            }
            let meta = track.meta().clone();
            if let Some(outcome) =
                self.lifecycle
                    .publish_ingress(track, network, &self.core.registry, now)
            {
                self.publish_track_lifecycle(outcome);
                self.notify_scoped_source_changed(&meta);
            }
        }
    }

    fn apply_ownership_snapshot(
        &mut self,
        session: Session,
        revision: NonZeroU64,
        endpoints: Snapshot,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let now = Instant::now();
        let claims: Vec<_> = endpoints
            .iter()
            .map(|(endpoint, fact)| (*endpoint, fact.claim))
            .collect();
        let mut affected: BTreeSet<_> =
            self.peer_ownership.endpoints_for_session(session).collect();
        let result = self.peer_ownership.snapshot(
            session,
            revision,
            endpoints,
            now,
            locally_bounded_deadline(now, fresh_until),
        );
        if result.is_ok() {
            for (endpoint, claim) in claims {
                affected.insert(endpoint);
                self.apply_remote_defeat(endpoint, claim);
            }
        }
        for endpoint in affected {
            self.reconcile_peer_sources(endpoint, now);
        }
        result
    }

    fn apply_ownership_delta(
        &mut self,
        session: Session,
        base: NonZeroU64,
        revision: NonZeroU64,
        change: (EndpointKey, Option<EndpointFact>),
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let (endpoint, fact) = change;
        let claim = fact.as_ref().map(|fact| fact.claim);
        let now = Instant::now();
        let before: Vec<_> = self.peer_ownership.endpoints_for_session(session).collect();
        let result = self.peer_ownership.delta(
            session,
            base,
            revision,
            endpoint,
            fact,
            now,
            locally_bounded_deadline(now, fresh_until),
        );
        if result.is_ok() {
            if let Some(claim) = claim {
                self.apply_remote_defeat(endpoint, claim);
            }
            self.reconcile_peer_sources(endpoint, now);
        } else if result != Err(Error::Stale) {
            for affected in before {
                self.reconcile_peer_sources(affected, now);
            }
        }
        result
    }

    fn local_endpoint_snapshot(&self) -> Result<Snapshot, Error> {
        let wall_now = SystemTime::now();
        let mut endpoints = Vec::new();
        let mut publication_count = 0;
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
            if endpoints.len() == MAX_CLAIMS_PER_ORIGIN {
                return Err(Error::Capacity);
            }
            let mut publications = Vec::new();
            if meta.materialized {
                for track in self.lifecycle.publisher_sources(
                    endpoint.room,
                    endpoint.participant,
                    meta.connection_id,
                ) {
                    if let Some(fact) = PublicationFact::from_track(track) {
                        if publication_count == MAX_PUBLICATIONS_PER_ORIGIN {
                            return Err(Error::Capacity);
                        }
                        publication_count += 1;
                        publications.push(fact);
                    }
                }
            }
            publications.sort_unstable_by(|a, b| {
                a.descriptor
                    .publication_id
                    .cmp(&b.descriptor.publication_id)
            });
            endpoints.push((
                endpoint,
                EndpointFact {
                    claim: rank,
                    publications: publications.into_boxed_slice(),
                },
            ));
        }
        endpoints.sort_unstable_by_key(|(endpoint, _)| *endpoint);
        Ok(endpoints.into_boxed_slice())
    }

    pub(super) fn peer_lineage_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
        self.receipt_trust
            .as_deref()
            .is_some_and(|trust| trust.is_retired(endpoint, rank))
    }

    fn apply_remote_defeat(&mut self, endpoint: EndpointKey, winner: ClaimRank) {
        if self.peer_lineage_retired(endpoint, winner) {
            return;
        }
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
            ConnectionEnd::Replaced(Some(winner.connection())),
        );
    }
}
