pub(in crate::control) mod fabric_data;
pub(super) mod media_repair;
pub(crate) mod provisioned;
pub(in crate::control) mod relay;
pub(super) mod target_demand;
pub(super) mod transfer;

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    time::Duration,
};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::NetworkId;
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    ConnectionEnd, ControllerActor, ControllerCommand, ControllerSender, SystemTime, ownership,
};
use crate::entity::{EndpointKey, ParticipantId, TrackId, TrackKind};
use ownership::{
    ClaimRank,
    observations::{EndpointFact, Error, Observations, PublicationFact, Session, Snapshot},
    receipt::{
        Authority as ReceiptAuthority, Error as ReceiptError, Issuer, epoch::Checkpoint, standby,
    },
};

const MAX_ORIGINS: usize = 64;
pub(super) const MAX_CLAIMS_PER_ORIGIN: usize = 4096;
pub(super) const MAX_PUBLICATIONS_PER_ORIGIN: usize = 4096;
const MAX_REMOTE_FRESHNESS: Duration = Duration::from_secs(30);
type Reply<T> = oneshot::Sender<Result<T, Error>>;

type Logical = (TrackKind, TrackId);

struct Selection {
    owner: standby::Owner,
    authority: Session,
    revision: u64,
    source: standby::Source,
    primary: bool,
    deadline: Option<Instant>,
}

type EncoderKey = (EndpointKey, TrackKind, [u8; 16]);

struct Encoder {
    owner: standby::Owner,
    source: standby::Source,
    session: Session,
    publisher: EndpointKey,
    deadline: Instant,
}

fn encoder_key(owner: standby::Owner, source: standby::Source) -> EncoderKey {
    (
        owner.endpoint,
        source.publication.kind(),
        *source.publication.as_bytes(),
    )
}

#[derive(Default)]
pub(super) struct Selections {
    entries: BTreeMap<EndpointKey, BTreeMap<Logical, Selection>>,
    deadlines: BTreeSet<(Instant, EndpointKey, Logical)>,
    count: usize,
    encoders: BTreeMap<EncoderKey, Encoder>,
    encoder_deadlines: BTreeSet<(Instant, EncoderKey)>,
}

impl Selections {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadlines
            .first()
            .map(|entry| entry.0)
            .into_iter()
            .chain(self.encoder_deadlines.first().map(|entry| entry.0))
            .min()
    }

    fn encoder(&self, entry: &Selection) -> Option<&Encoder> {
        self.encoders
            .get(&encoder_key(entry.owner, entry.source))
            .filter(|encoder| {
                encoder.owner.same_epoch(entry.owner) && encoder.source == entry.source
            })
    }

    fn record_encoder(&mut self, encoder: Encoder) -> Result<(), ReceiptError> {
        let key = encoder_key(encoder.owner, encoder.source);
        if let Some(previous) = self.encoders.get(&key) {
            if !previous.owner.same_epoch(encoder.owner)
                || previous.source != encoder.source
                || previous.publisher != encoder.publisher
            {
                return Err(ReceiptError::Scope);
            }
            if previous.deadline > encoder.deadline {
                return Err(ReceiptError::Invalid);
            }
            self.encoder_deadlines.remove(&(previous.deadline, key));
        } else if self.encoders.len() >= MAX_PUBLICATIONS_PER_ORIGIN {
            return Err(ReceiptError::NoAuthority);
        }
        self.encoder_deadlines.insert((encoder.deadline, key));
        self.encoders.insert(key, encoder);
        Ok(())
    }

    fn record(&mut self, logical: Logical, next: Selection) -> Result<(), ReceiptError> {
        let endpoint = next.owner.endpoint;
        let current = self
            .entries
            .get(&endpoint)
            .and_then(|entries| entries.get(&logical));
        if let Some(current) = current {
            if next.owner.rank < current.owner.rank
                || (next.owner.rank == current.owner.rank
                    && (next.owner.issuer != current.owner.issuer
                        || (next.owner.authorization, next.revision)
                            < (current.owner.authorization, current.revision)
                        || ((next.owner.authorization, next.revision)
                            == (current.owner.authorization, current.revision)
                            && next.source != current.source)))
            {
                return Err(ReceiptError::Invalid);
            }
            if let Some(old) = current.deadline {
                self.deadlines.remove(&(old, endpoint, logical));
            }
        } else {
            if self.count >= MAX_PUBLICATIONS_PER_ORIGIN {
                return Err(ReceiptError::NoAuthority);
            }
            self.count = self.count.saturating_add(1);
        }
        if let Some(deadline) = next.deadline {
            self.deadlines.insert((deadline, endpoint, logical));
        }
        self.entries
            .entry(endpoint)
            .or_default()
            .insert(logical, next);
        Ok(())
    }

    fn expire(&mut self, now: Instant) -> BTreeSet<EndpointKey> {
        let mut changed = BTreeSet::new();
        while let Some(&(deadline, endpoint, logical)) = self.deadlines.first() {
            if deadline > now {
                break;
            }
            self.deadlines.pop_first();
            if let Some(entry) = self
                .entries
                .get_mut(&endpoint)
                .and_then(|entries| entries.get_mut(&logical))
            {
                // Keep the selection watermark; expiry must not fall back to primary.
                entry.deadline = None;
                changed.insert(endpoint);
            }
        }
        while let Some(&(deadline, key)) = self.encoder_deadlines.first() {
            if deadline > now {
                break;
            }
            self.encoder_deadlines.pop_first();
            self.encoders.remove(&key);
            changed.insert(key.0);
        }
        changed
    }

    fn retire(&mut self, endpoint: EndpointKey, lineage: crate::entity::ConnectionId) {
        let Some(entries) = self.entries.get_mut(&endpoint) else {
            return;
        };
        entries.retain(|logical, entry| {
            if entry.owner.rank.lineage() != lineage {
                return true;
            }
            if let Some(deadline) = entry.deadline {
                self.deadlines.remove(&(deadline, endpoint, *logical));
            }
            self.count = self.count.saturating_sub(1);
            false
        });
        if entries.is_empty() {
            self.entries.remove(&endpoint);
        }
    }

    fn affected<'a>(
        &'a self,
        predicate: impl Fn(&Selection) -> bool + 'a,
    ) -> impl Iterator<Item = EndpointKey> + 'a {
        self.entries.iter().filter_map(move |(endpoint, entries)| {
            entries.values().any(&predicate).then_some(*endpoint)
        })
    }
}

pub(super) struct EncoderAdvertisement {
    pub(super) session: Session,
    pub(super) source: standby::Source,
    pub(super) publisher: ParticipantId,
    pub(super) grant: standby::Grant,
}

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
        if matches!(self.receipt_trust, Some(ReceiptAuthority::Provisioned(_))) {
            // A provisioned peer has only one input: its authenticated Link.
            return;
        }
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
                    None,
                ));
            }
            Action::RejectOversized(session, revision, fresh_until, reply) => {
                let now = Instant::now();
                let affected = self.peer_ownership.endpoints_for_session(session).collect();
                let result = self.peer_ownership.reject_oversized(
                    session,
                    revision,
                    now,
                    locally_bounded_deadline(now, fresh_until),
                );
                self.reconcile_peer_source_changes(affected, now);
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
                .is_none_or(|links| !links.accepts_ownership(network))
        }) {
            return Err(Error::Closed);
        }
        let affected: BTreeSet<_> = self
            .peer_ownership
            .endpoints_for_node(node)
            .chain(
                self.peer_selections
                    .affected(|entry| entry.source.node == node || entry.owner.issuer.node == node),
            )
            .collect();
        let session = self.peer_ownership.open(node, incarnation, network)?;
        if let (Some(link), Some(links)) = (network, self.links.as_mut()) {
            links.identify_peer(link, node);
        }
        self.reconcile_peer_source_changes(affected, Instant::now());
        Ok(session)
    }

    fn close_ownership_peer(&mut self, session: Session) -> Result<(), Error> {
        let affected: BTreeSet<_> = self
            .peer_ownership
            .endpoints_for_session(session)
            .chain(self.peer_selections.affected(|entry| {
                entry.authority == session
                    || self
                        .peer_selections
                        .encoder(entry)
                        .is_some_and(|encoder| encoder.session == session)
            }))
            .collect();
        self.peer_ownership.close(session)?;
        self.reconcile_peer_source_changes(affected, Instant::now());
        Ok(())
    }

    pub(super) fn expire_peer_sources(&mut self, now: Instant) {
        let mut affected = self.peer_ownership.expire(now);
        affected.extend(self.peer_selections.expire(now));
        self.reconcile_peer_source_changes(affected, now);
    }

    pub(super) fn close_link_peer(&mut self, network: NetworkId) {
        let plans = self
            .links
            .as_mut()
            .map(|links| links.close_peer(network))
            .unwrap_or_default();
        let now = Instant::now();
        let mut affected: BTreeSet<_> = self
            .peer_selections
            .affected(|entry| {
                self.peer_selections.encoder(entry).is_some_and(|encoder| {
                    self.peer_ownership.media_route(encoder.session) == Some(network)
                })
            })
            .collect();
        affected.extend(self.peer_ownership.detach_media(network));
        self.reconcile_peer_source_changes(affected, now);
        if !plans.is_empty() {
            let generation = self.lifecycle.next_generation();
            for (shard, plan) in plans {
                self.stage_plans_at(shard, generation, vec![plan]);
            }
            self.publish_staged();
        }
    }

    pub(super) fn reconcile_peer_source_changes(
        &mut self,
        affected: BTreeSet<EndpointKey>,
        now: Instant,
    ) {
        let dependents: BTreeSet<_> = self
            .peer_selections
            .affected(|entry| {
                self.peer_selections
                    .encoder(entry)
                    .is_some_and(|encoder| affected.contains(&encoder.publisher))
            })
            .collect();
        for endpoint in dependents.iter().chain(affected.difference(&dependents)) {
            self.reconcile_peer_sources(*endpoint, now);
        }
    }

    pub(super) fn reconcile_peer_sources(&mut self, endpoint: EndpointKey, now: Instant) {
        let desired = self
            .core
            .registry
            .get_endpoint(endpoint)
            .is_none()
            .then(|| {
                self.peer_ownership.ingress(
                    endpoint,
                    now,
                    |rank| !self.peer_lineage_retired(endpoint, rank),
                    |peer| {
                        self.links
                            .as_ref()
                            .is_some_and(|links| links.is_wan_peer(peer))
                    },
                )
            })
            .flatten();
        let selections = self.peer_selections.entries.get(&endpoint);
        let mut install = Vec::new();
        if let Some((network, fact)) = desired {
            let connection = fact.claim.connection();
            for publication in &fact.publications {
                if let Some(track) = publication.media_track(endpoint, connection)
                    && !track.meta().track_id.is_some_and(|id| {
                        selections
                            .and_then(|entries| entries.get(&(track.kind(), id)))
                            .is_some_and(|entry| fact.claim <= entry.owner.rank)
                    })
                {
                    install.push((track, network));
                }
            }
        }
        let mut rehome = Vec::new();
        if self.core.registry.get_endpoint(endpoint).is_none()
            && let Some(entries) = selections
        {
            let highest = self.peer_ownership.highest(endpoint, now, |rank| {
                !self.peer_lineage_retired(endpoint, rank)
            });
            for entry in entries.values() {
                if highest == Some(entry.owner.rank)
                    && entry.deadline.is_some_and(|deadline| now < deadline)
                    && self.handoff_permits(
                        endpoint,
                        entry.owner.issuer,
                        entry.owner.rank.connection(),
                    )
                    && self
                        .peer_ownership
                        .endpoint(entry.authority, endpoint, now)
                        .is_some_and(|fact| fact.claim == entry.owner.rank)
                    && let Some((track, network, publisher)) = self.selected_encoder(entry, now)
                {
                    if let Some(publisher) = publisher.filter(|publisher| *publisher != endpoint)
                        && self.lifecycle.ingress_peer(endpoint.room, track.id()) == Some(network)
                        && let Some(raw) = self.lifecycle.source_exact(endpoint.room, track.id())
                        && raw.meta().origin == publisher.participant
                        && raw.meta().connection_id == track.meta().connection_id
                    {
                        rehome.push((publisher, raw.meta().clone()));
                    }
                    install.push((track, network));
                }
            }
        }
        // A publication key is globally unique. Remove precisely the observed
        // raw ingress before presenting that encoder under the delegated label.
        for (publisher, raw) in rehome {
            if let Some(outcome) =
                self.lifecycle
                    .unpublish(publisher, raw.id, &self.core.registry, now)
            {
                self.publish_track_lifecycle(outcome);
                self.notify_scoped_source_changed(&raw);
            }
        }
        self.apply_peer_source_image(endpoint, install, now);
    }

    fn selected_encoder(
        &self,
        entry: &Selection,
        now: Instant,
    ) -> Option<(crate::track::Track, NetworkId, Option<EndpointKey>)> {
        let endpoint = entry.owner.endpoint;
        if entry.primary {
            let origin = self.peer_ownership.session_origin(entry.authority).ok()?;
            let network = self.peer_ownership.media_route(entry.authority)?;
            let fact = self
                .peer_ownership
                .endpoint(entry.authority, endpoint, now)?;
            return fact.publications.iter().find_map(|publication| {
                entry
                    .source
                    .materialize(endpoint, origin, fact.claim.connection(), publication)
                    .map(|track| (track, network, None))
            });
        }
        let encoder = self.peer_selections.encoder(entry)?;
        if now >= encoder.deadline {
            return None;
        }
        let network = self.peer_ownership.media_route(encoder.session)?;
        let track = self.observed_encoder(encoder, now)?;
        Some((track, network, Some(encoder.publisher)))
    }

    fn observed_encoder(&self, encoder: &Encoder, now: Instant) -> Option<crate::track::Track> {
        let origin = self.peer_ownership.session_origin(encoder.session).ok()?;
        let fact = self
            .peer_ownership
            .endpoint(encoder.session, encoder.publisher, now)?;
        if self.core.registry.get_endpoint(encoder.publisher).is_some()
            || self.peer_ownership.highest(encoder.publisher, now, |rank| {
                !self.peer_lineage_retired(encoder.publisher, rank)
            }) != Some(fact.claim)
            || !self.handoff_permits(
                encoder.publisher,
                Issuer {
                    trust_domain: encoder.owner.issuer.trust_domain,
                    node: origin.0,
                    incarnation: origin.1,
                },
                fact.claim.connection(),
            )
        {
            return None;
        }
        fact.publications.iter().find_map(|publication| {
            encoder.source.materialize(
                encoder.owner.endpoint,
                origin,
                fact.claim.connection(),
                publication,
            )
        })
    }

    pub(super) fn observe_standby_encoder(
        &mut self,
        authority_session: Session,
        owner: standby::Owner,
        advertisement: EncoderAdvertisement,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<(), ReceiptError> {
        let trust = self.standby_authority(authority_session, owner, now)?;
        let elapsed = wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ReceiptError::Invalid)?;
        let EncoderAdvertisement {
            session,
            source,
            publisher,
            grant,
        } = advertisement;
        let expires_at = owner.validate(trust, source, grant, elapsed.as_secs())?;
        let remaining = Duration::from_secs(expires_at)
            .checked_sub(elapsed)
            .ok_or(ReceiptError::Invalid)?;
        let deadline = now.checked_add(remaining).ok_or(ReceiptError::Invalid)?;
        let origin = self
            .peer_ownership
            .session_origin(session)
            .map_err(|_| ReceiptError::NoAuthority)?;
        if origin != (source.node, source.generation) {
            return Err(ReceiptError::Scope);
        }
        let encoder = Encoder {
            owner,
            source,
            session,
            publisher: EndpointKey {
                room: owner.endpoint.room,
                participant: publisher,
            },
            deadline,
        };
        self.observed_encoder(&encoder, now)
            .ok_or(ReceiptError::Scope)?;
        self.peer_selections.record_encoder(encoder)?;
        self.reconcile_peer_sources(owner.endpoint, now);
        Ok(())
    }

    // Selection comes from the authority session. The selected encoder's
    // description and route come from observations, never from that command.
    pub(super) fn reconcile_selected_peer_source(
        &mut self,
        authority_session: Session,
        owner: standby::Owner,
        candidates: &standby::Candidates,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<(), ReceiptError> {
        let trust = self.standby_authority(authority_session, owner, now)?;
        let elapsed = wall_now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ReceiptError::Invalid)?;
        let (revision, selected, expires_at) = candidates
            .selection(owner, trust, elapsed.as_secs())
            .ok_or(ReceiptError::NoAuthority)?;
        let remaining = Duration::from_secs(expires_at)
            .checked_sub(elapsed)
            .ok_or(ReceiptError::Invalid)?;
        let deadline = now.checked_add(remaining).ok_or(ReceiptError::Invalid)?;
        let primary = candidates.primary();
        let logical = (primary.publication.kind(), primary.track);
        let previous_publisher = self
            .peer_selections
            .entries
            .get(&owner.endpoint)
            .and_then(|entries| entries.get(&logical))
            .filter(|entry| entry.source != selected)
            .and_then(|entry| self.peer_selections.encoder(entry))
            .map(|encoder| encoder.publisher)
            .filter(|publisher| *publisher != owner.endpoint);
        self.peer_selections.record(
            logical,
            Selection {
                owner,
                authority: authority_session,
                revision,
                source: selected,
                primary: selected == primary,
                deadline: Some(deadline),
            },
        )?;
        self.reconcile_peer_sources(owner.endpoint, now);
        if let Some(publisher) = previous_publisher {
            self.reconcile_peer_sources(publisher, now);
        }
        Ok(())
    }

    pub(super) fn standby_authority(
        &self,
        authority_session: Session,
        owner: standby::Owner,
        now: Instant,
    ) -> Result<&dyn ownership::receipt::LineageTrust, ReceiptError> {
        let endpoint = owner.endpoint;
        let origin = self
            .peer_ownership
            .session_origin(authority_session)
            .map_err(|_| ReceiptError::NoAuthority)?;
        if origin != (owner.issuer.node, owner.issuer.incarnation)
            || self.core.registry.get_endpoint(endpoint).is_some()
        {
            return Err(ReceiptError::Scope);
        }
        if !self.handoff_permits(endpoint, owner.issuer, owner.rank.connection())
            || self
                .peer_ownership
                .endpoint(authority_session, endpoint, now)
                .is_none_or(|fact| fact.claim != owner.rank)
            || self.peer_ownership.highest(endpoint, now, |rank| {
                !self.peer_lineage_retired(endpoint, rank)
            }) != Some(owner.rank)
        {
            return Err(ReceiptError::NoAuthority);
        }
        self.receipt_trust
            .as_deref()
            .ok_or(ReceiptError::NoAuthority)
    }

    fn apply_peer_source_image(
        &mut self,
        endpoint: EndpointKey,
        install: Vec<(crate::track::Track, NetworkId)>,
        now: Instant,
    ) {
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
        mut endpoints: Snapshot,
        fresh_until: Instant,
        checkpoint: Option<Checkpoint>,
    ) -> Result<(), Error> {
        let now = Instant::now();
        let deadline = locally_bounded_deadline(now, fresh_until);
        let checkpoint = match self.receipt_trust.as_mut() {
            Some(ReceiptAuthority::Provisioned(trust)) => {
                let image = checkpoint.ok_or(Error::Invalid)?;
                let origin = self.peer_ownership.session_origin(session)?;
                let validated = trust
                    .validate_checkpoint(origin, revision, image, now, deadline)
                    .map_err(|_| Error::Invalid)?;
                if validated.blocked() {
                    endpoints = Box::new([]);
                }
                Some((trust, validated))
            }
            _ if checkpoint.is_some() => return Err(Error::Invalid),
            _ => None,
        };
        let claims: Vec<_> = endpoints
            .iter()
            .map(|(endpoint, fact)| (*endpoint, fact.claim))
            .collect();
        let mut affected: BTreeSet<_> =
            self.peer_ownership.endpoints_for_session(session).collect();
        let result = self
            .peer_ownership
            .snapshot(session, revision, endpoints, now, deadline);
        if result.is_ok() {
            if let Some((trust, checkpoint)) = checkpoint {
                for (endpoint, lineage) in trust
                    .install_checkpoint(checkpoint)
                    .map_err(|_| Error::Invalid)?
                {
                    affected.insert(endpoint);
                    self.apply_remote_retirement(endpoint, lineage);
                }
            }
            for (endpoint, claim) in claims {
                affected.insert(endpoint);
                self.apply_remote_defeat(endpoint, claim);
            }
        }
        self.reconcile_peer_source_changes(affected, now);
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
        let endpoint = change.0;
        let claim = change.1.as_ref().map(|fact| fact.claim);
        let now = Instant::now();
        let before = self.peer_ownership.endpoints_for_session(session).collect();
        let result = self.peer_ownership.delta(
            session,
            base,
            revision,
            change,
            now,
            locally_bounded_deadline(now, fresh_until),
        );
        if result.is_ok() {
            if let Some(claim) = claim {
                self.apply_remote_defeat(endpoint, claim);
            }
            self.reconcile_peer_source_changes(BTreeSet::from([endpoint]), now);
        } else if result != Err(Error::Stale) {
            self.reconcile_peer_source_changes(before, now);
        }
        result
    }

    pub(super) fn local_endpoint_snapshot(&self) -> Result<Snapshot, Error> {
        let wall_now = SystemTime::now();
        let mut endpoints: BTreeMap<_, EndpointFact> = BTreeMap::new();
        let provisioned = matches!(self.receipt_trust, Some(ReceiptAuthority::Provisioned(_)));
        if provisioned {
            // Logical withdrawal removes media immediately, but an endpoint
            // remains occupied until every shard has applied the cutoff.
            for (endpoint, claim) in self
                .ownership
                .retiring_claims()
                .chain(self.publisher_handoffs.retiring_claim())
            {
                let fact = endpoints.entry(endpoint).or_insert_with(|| EndpointFact {
                    claim,
                    publications: Box::new([]),
                });
                fact.claim = fact.claim.max(claim);
                if endpoints.len() > MAX_CLAIMS_PER_ORIGIN {
                    return Err(Error::Capacity);
                }
            }
        }
        let mut publication_count = 0usize;
        for meta in self.core.registry.connections() {
            let (Some(endpoint), Some(rank)) = (meta.endpoint(), meta.claim) else {
                continue;
            };
            if self
                .core
                .registry
                .get_endpoint(endpoint)
                .is_none_or(|current| current.connection_id != meta.connection_id)
            {
                continue;
            }
            let publishes =
                meta.authorization.as_ref().is_some_and(|lease| {
                    !lease.is_expired_at(wall_now) && lease.allows_current_room()
                }) && !self.peer_lineage_retired(endpoint, rank);
            if !provisioned && !publishes {
                continue;
            }
            let mut publications = Vec::new();
            if publishes && meta.materialized {
                for track in self.lifecycle.publisher_sources(
                    endpoint.room,
                    endpoint.participant,
                    meta.connection_id,
                ) {
                    if let Some(fact) = PublicationFact::from_track(track) {
                        if publication_count == MAX_PUBLICATIONS_PER_ORIGIN {
                            return Err(Error::Capacity);
                        }
                        publication_count = publication_count.saturating_add(1);
                        publications.push(fact);
                    }
                }
            }
            publications.sort_unstable_by(|a, b| {
                a.descriptor
                    .publication_id
                    .cmp(&b.descriptor.publication_id)
            });
            endpoints.insert(
                endpoint,
                EndpointFact {
                    claim: rank,
                    publications: publications.into_boxed_slice(),
                },
            );
            if endpoints.len() > MAX_CLAIMS_PER_ORIGIN {
                return Err(Error::Capacity);
            }
        }
        Ok(endpoints.into_iter().collect())
    }

    pub(super) fn peer_lineage_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
        self.receipt_trust
            .as_deref()
            .is_some_and(|trust| trust.is_retired(endpoint, rank))
    }

    fn apply_remote_retirement(
        &mut self,
        endpoint: EndpointKey,
        lineage: crate::entity::ConnectionId,
    ) {
        self.peer_selections.retire(endpoint, lineage);
        for work in &mut self.materializations {
            if work.pending.endpoint() == Some(endpoint) && work.pending.claim.lineage() == lineage
            {
                work.superseded = true;
            }
        }
        if let Some(previous) = self
            .core
            .registry
            .get_endpoint(endpoint)
            .filter(|meta| meta.claim.is_some_and(|claim| claim.lineage() == lineage))
            .cloned()
        {
            self.terminate_incarnation(
                endpoint.participant,
                previous,
                true,
                ConnectionEnd::Replaced(None),
            );
        }
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
