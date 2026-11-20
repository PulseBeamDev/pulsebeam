use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use pulsebeam_core::identity::{NodeId, PublicationId, TrackKind};
use pulsebeam_link::NetworkId;
use pulsebeam_proto::state::{self as wire, Track, track, validation};
use pulsebeam_runtime::time::Instant;

use super::ClaimRank;
use crate::{
    entity::{ConnectionId, EndpointKey, PublicationKey, TrackId},
    media::Rid,
    track::{DataLane, LayerQuality, SourceEncoding, Track as MediaTrack, TrackLayer, TrackMeta},
};

#[derive(Clone, PartialEq)]
pub(crate) struct PublicationFact {
    pub(crate) descriptor: Track,
    pub(super) layers: Box<[(SourceEncoding, Option<Rid>, LayerQuality)]>,
}

impl PublicationFact {
    pub(crate) fn from_track(track: &MediaTrack) -> Option<Self> {
        Some(Self {
            descriptor: crate::control::native::track_descriptor(track)?,
            layers: track
                .layers()
                .iter()
                .map(|layer| (layer.encoding, layer.rid, layer.quality))
                .collect(),
        })
    }

    pub(crate) fn media_track(
        &self,
        endpoint: EndpointKey,
        connection: ConnectionId,
    ) -> Option<MediaTrack> {
        let descriptor = &self.descriptor;
        let kind = match descriptor.kind.as_ref()? {
            track::Kind::Audio(_) => TrackKind::Audio,
            track::Kind::Video(_) => TrackKind::Video,
            track::Kind::Data(_) => TrackKind::Data,
        };
        let meta = TrackMeta {
            room: endpoint.room,
            origin: endpoint.participant,
            connection_id: connection,
            id: PublicationKey::new(kind, descriptor.publication_id.parse().ok()?),
            track_id: Some(TrackId::new(&descriptor.track_id).ok()?),
        };
        Some(match descriptor.kind.as_ref()? {
            track::Kind::Audio(audio) => MediaTrack::Audio(crate::track::AudioTrack {
                meta,
                format: Some(audio.format.clone()?),
                reverse: None,
            }),
            track::Kind::Video(video) => {
                let layers = self
                    .layers
                    .iter()
                    .map(|&(encoding, rid, quality)| TrackLayer {
                        meta: meta.clone(),
                        rid,
                        encoding,
                        quality,
                    })
                    .collect();
                let MediaTrack::Video(mut track) = MediaTrack::video(meta, layers, None) else {
                    return None;
                };
                track.format = Some(video.format.clone()?);
                MediaTrack::Video(track)
            }
            track::Kind::Data(data) => {
                let lane = match wire::DataDelivery::try_from(data.delivery).ok()? {
                    wire::DataDelivery::Ordered => DataLane::Ordered,
                    wire::DataDelivery::Latest => DataLane::Latest,
                    wire::DataDelivery::Lossy => DataLane::Lossy,
                    wire::DataDelivery::Unspecified => return None,
                };
                MediaTrack::data_publication(meta, lane)
            }
        })
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct EndpointFact {
    pub(crate) claim: ClaimRank,
    pub(crate) publications: Box<[PublicationFact]>,
}

impl std::fmt::Debug for EndpointFact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EndpointFact")
            .field("claim", &self.claim)
            .field("publication_count", &self.publications.len())
            .finish()
    }
}

pub(crate) type Snapshot = Box<[(EndpointKey, EndpointFact)]>;

pub(super) fn validate_publications<'a>(
    endpoints: impl IntoIterator<Item = (EndpointKey, &'a EndpointFact)>,
    limit: usize,
) -> Result<(), Error> {
    let mut publications = BTreeSet::new();
    for (endpoint, fact) in endpoints {
        let mut labels = BTreeSet::new();
        for fact in &fact.publications {
            let publication = &fact.descriptor;
            if publications.len() >= limit {
                return Err(Error::Capacity);
            }
            let id = publication
                .publication_id
                .parse::<PublicationId>()
                .map_err(|_| Error::Invalid)?;
            let kind = match publication.kind {
                Some(track::Kind::Audio(_)) => TrackKind::Audio,
                Some(track::Kind::Video(_)) => TrackKind::Video,
                Some(track::Kind::Data(_)) => TrackKind::Data,
                None => return Err(Error::Invalid),
            };
            if fact.layers.len() > 3
                || (kind == TrackKind::Video) == fact.layers.is_empty()
                || validation::validate_track(publication).is_err()
                || publication.room_id != endpoint.room.room.as_str()
                || publication.participant_id != endpoint.participant.as_str()
                || !labels.insert((kind, publication.track_id.as_str()))
                || !publications.insert(id)
            {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}

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
    network: Option<NetworkId>,
    revision: Option<NonZeroU64>,
    resync_at_least: Option<NonZeroU64>,
    fresh_until: Option<Instant>,
    endpoints: BTreeMap<EndpointKey, EndpointFact>,
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
    max_publications_per_origin: usize,
}

impl Observations {
    pub(crate) fn new(
        max_origins: usize,
        max_claims_per_origin: usize,
        max_publications_per_origin: usize,
    ) -> Self {
        Self {
            origins: BTreeMap::new(),
            last_session: 0,
            max_origins,
            max_claims_per_origin,
            max_publications_per_origin,
        }
    }

    /// Only the authenticated peer adapter may open a session for an origin.
    /// Reopening an incarnation requires a new complete snapshot, not deltas.
    pub(crate) fn open(
        &mut self,
        node: NodeId,
        incarnation: NonZeroU64,
        network: Option<NetworkId>,
    ) -> Result<Session, Error> {
        if let Some(network) = network
            && self
                .origins
                .iter()
                .any(|(&other, origin)| other != node && origin.network == Some(network))
        {
            return Err(Error::Invalid);
        }
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
        origin.network = network;
        origin.withdraw();
        Ok(Session { node, serial })
    }

    pub(crate) fn close(&mut self, session: Session) -> Result<(), Error> {
        let origin = self.origin(session)?;
        origin.session = None;
        origin.network = None;
        origin.withdraw();
        Ok(())
    }

    /// Replaces the complete view for this origin incarnation. Omission
    /// withdraws facts without forgetting the origin watermark.
    pub(crate) fn snapshot(
        &mut self,
        session: Session,
        revision: NonZeroU64,
        endpoints: Snapshot,
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let limit = self.max_claims_per_origin;
        let publication_limit = self.max_publications_per_origin;
        let origin = self.origin(session)?;
        origin.check_revision(revision, now, fresh_until)?;
        if origin
            .resync_at_least
            .is_some_and(|minimum| revision < minimum)
        {
            return Err(Error::Stale);
        }
        let len = endpoints.len();
        if len > limit {
            origin.require_snapshot(revision);
            return Err(Error::Capacity);
        }
        if let Err(error) = validate_publications(
            endpoints.iter().map(|(endpoint, fact)| (*endpoint, fact)),
            publication_limit,
        ) {
            origin.require_snapshot(revision);
            return Err(error);
        }
        let snapshot: BTreeMap<_, _> = endpoints.into_vec().into_iter().collect();
        if snapshot.len() != len {
            origin.require_snapshot(revision);
            return Err(Error::Invalid);
        }
        origin.resync_at_least = None;
        origin.endpoints = snapshot;
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
        fact: Option<EndpointFact>,
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let limit = self.max_claims_per_origin;
        let publication_limit = self.max_publications_per_origin;
        let origin = self.origin(session)?;
        origin.check_revision(revision, now, fresh_until)?;
        if !origin.is_fresh(now) || origin.revision != Some(base) {
            origin.require_snapshot(revision);
            return Err(Error::NeedsSnapshot);
        }
        if let Some(fact) = fact {
            if origin.endpoints.len() >= limit && !origin.endpoints.contains_key(&endpoint) {
                origin.require_snapshot(revision);
                return Err(Error::Capacity);
            }
            if let Err(error) = validate_publications(
                origin
                    .endpoints
                    .iter()
                    .filter(|(key, _)| **key != endpoint)
                    .map(|(key, value)| (*key, value))
                    .chain(std::iter::once((endpoint, &fact))),
                publication_limit,
            ) {
                origin.require_snapshot(revision);
                return Err(error);
            }
            origin.endpoints.insert(endpoint, fact);
        } else {
            origin.endpoints.remove(&endpoint);
        }
        origin.revision = Some(revision);
        origin.fresh_until = Some(fresh_until);
        Ok(())
    }

    pub(crate) fn reject_oversized(
        &mut self,
        session: Session,
        revision: NonZeroU64,
        now: Instant,
        fresh_until: Instant,
    ) -> Result<(), Error> {
        let origin = self.origin(session)?;
        origin.check_revision(revision, now, fresh_until)?;
        origin.require_snapshot(revision);
        Err(Error::Capacity)
    }

    pub(crate) fn highest(
        &self,
        endpoint: EndpointKey,
        now: Instant,
        eligible: impl Fn(ClaimRank) -> bool,
    ) -> Option<ClaimRank> {
        self.origins
            .values()
            .filter(|origin| origin.is_fresh(now))
            .filter_map(|origin| origin.endpoints.get(&endpoint).map(|fact| fact.claim))
            .filter(|rank| eligible(*rank))
            .max()
    }

    pub(crate) fn endpoints_for_node(
        &self,
        node: NodeId,
    ) -> impl Iterator<Item = EndpointKey> + '_ {
        self.origins
            .get(&node)
            .into_iter()
            .flat_map(|origin| origin.endpoints.keys().copied())
    }

    pub(crate) fn endpoints_for_session(
        &self,
        session: Session,
    ) -> impl Iterator<Item = EndpointKey> + '_ {
        self.origins
            .get(&session.node)
            .filter(move |origin| origin.session == Some(session.serial))
            .into_iter()
            .flat_map(|origin| origin.endpoints.keys().copied())
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.origins
            .values()
            .filter_map(|origin| origin.fresh_until)
            .min()
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Vec<EndpointKey> {
        let mut endpoints = Vec::new();
        for origin in self.origins.values_mut() {
            if origin.fresh_until.is_some_and(|deadline| deadline <= now) {
                endpoints.extend(origin.endpoints.keys().copied());
                origin.withdraw();
            }
        }
        endpoints
    }

    pub(crate) fn detach_network(&mut self, network: NetworkId) -> Vec<EndpointKey> {
        let Some(origin) = self
            .origins
            .values_mut()
            .find(|origin| origin.network == Some(network))
        else {
            return Vec::new();
        };
        origin.network = None;
        origin.endpoints.keys().copied().collect()
    }

    pub(crate) fn ingress(
        &self,
        endpoint: EndpointKey,
        now: Instant,
        eligible: impl Fn(ClaimRank) -> bool,
    ) -> Option<(NetworkId, &EndpointFact)> {
        let mut winner: Option<(Option<NetworkId>, &EndpointFact)> = None;
        let mut tied = false;
        for origin in self.origins.values().filter(|origin| origin.is_fresh(now)) {
            let Some(fact) = origin.endpoints.get(&endpoint) else {
                continue;
            };
            if !eligible(fact.claim) {
                continue;
            }
            match winner {
                Some((_, current)) if current.claim > fact.claim => continue,
                Some((_, current)) if current.claim == fact.claim => tied = true,
                _ => {
                    winner = Some((origin.network, fact));
                    tied = false;
                }
            }
        }
        let (network, fact) = winner?;
        (!tied).then_some((network?, fact))
    }

    #[cfg(test)]
    pub(crate) fn endpoint(
        &self,
        session: Session,
        endpoint: EndpointKey,
        now: Instant,
    ) -> Option<&EndpointFact> {
        self.origins
            .get(&session.node)
            .filter(|origin| origin.session == Some(session.serial) && origin.is_fresh(now))?
            .endpoints
            .get(&endpoint)
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
        self.endpoints.clear();
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

    fn fact(claim: ClaimRank, publications: impl Into<Box<[PublicationFact]>>) -> EndpointFact {
        EndpointFact {
            claim,
            publications: publications.into(),
        }
    }

    fn claims(claims: &[(EndpointKey, ClaimRank)]) -> Snapshot {
        claims
            .iter()
            .map(|(endpoint, claim)| (*endpoint, fact(*claim, [])))
            .collect()
    }

    #[test]
    fn publications_are_scoped_to_the_current_peer_snapshot_and_withdraw_atomically() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut view = Observations::new(1, 2, 1);
        let node = NodeId::new();
        let session = view.open(node, revision(1), None).unwrap();
        let alice = endpoint("alice");
        let publication = Track {
            publication_id: PublicationId::mint().to_string(),
            room_id: alice.room.room.to_string(),
            participant_id: alice.participant.to_string(),
            track_id: "events".into(),
            kind: Some(track::Kind::Data(pulsebeam_proto::state::DataTrack {
                delivery: pulsebeam_proto::state::DataDelivery::Lossy as i32,
            })),
        };
        let publication_fact = |descriptor| PublicationFact {
            descriptor,
            layers: Box::new([]),
        };
        let snapshot = |endpoint, descriptor| {
            Box::new([(endpoint, fact(rank(1), [publication_fact(descriptor)]))])
        };
        view.snapshot(
            session,
            revision(1),
            snapshot(alice, publication.clone()),
            now,
            deadline,
        )
        .unwrap();
        assert_eq!(
            view.endpoint(session, alice, now).unwrap().publications[0].descriptor,
            publication
        );
        assert!(view.endpoint(session, alice, deadline).is_none());
        view.delta(
            session,
            revision(1),
            revision(2),
            alice,
            Some(fact(rank(1), [])),
            now,
            deadline,
        )
        .unwrap();
        assert!(
            view.endpoint(session, alice, now)
                .unwrap()
                .publications
                .is_empty()
        );
        assert_eq!(
            view.highest(alice, now, |_| true),
            Some(rank(1)),
            "unpublish does not withdraw ownership"
        );

        let replacement = Track {
            publication_id: PublicationId::mint().to_string(),
            ..publication.clone()
        };
        view.snapshot(
            session,
            revision(3),
            snapshot(alice, replacement.clone()),
            now,
            deadline,
        )
        .unwrap();
        let renewed = view.open(node, revision(2), None).unwrap();
        assert!(view.endpoint(session, alice, now).is_none());
        assert!(view.endpoint(renewed, alice, now).is_none());
        assert_eq!(
            view.snapshot(
                renewed,
                revision(1),
                snapshot(endpoint("bob"), replacement.clone()),
                now,
                deadline
            ),
            Err(Error::Invalid),
        );
        assert_eq!(view.highest(alice, now, |_| true), None);
        assert_eq!(
            view.snapshot(
                renewed,
                revision(1),
                Box::new([(
                    alice,
                    fact(
                        rank(1),
                        [
                            publication_fact(publication),
                            publication_fact(replacement.clone())
                        ]
                    )
                )]),
                now,
                deadline
            ),
            Err(Error::Capacity),
        );
        view.snapshot(
            renewed,
            revision(1),
            snapshot(alice, replacement.clone()),
            now,
            deadline,
        )
        .unwrap();
        assert_eq!(
            view.reject_oversized(renewed, revision(3), now, deadline),
            Err(Error::Capacity)
        );
        assert!(view.endpoint(renewed, alice, now).is_none());
        assert_eq!(
            view.snapshot(
                renewed,
                revision(2),
                snapshot(alice, replacement.clone()),
                now,
                deadline
            ),
            Err(Error::Stale)
        );
        view.snapshot(
            renewed,
            revision(3),
            snapshot(alice, replacement),
            now,
            deadline,
        )
        .unwrap();
        view.close(renewed).unwrap();
        assert!(view.endpoint(renewed, alice, now).is_none());
    }

    #[test]
    fn current_claims_survive_a_departed_winner_without_a_historical_rank_floor() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut view = Observations::new(2, 1, 2);
        let survivor = view.open(NodeId::new(), revision(1), None).unwrap();
        let other = NodeId::new();
        let stale = view.open(other, revision(1), None).unwrap();
        let endpoint = endpoint("alice");
        view.snapshot(
            survivor,
            revision(1),
            claims(&[(endpoint, rank(1))]),
            now,
            deadline,
        )
        .unwrap();
        view.snapshot(
            stale,
            revision(1),
            claims(&[(endpoint, rank(2))]),
            now,
            deadline,
        )
        .unwrap();
        assert_eq!(view.highest(endpoint, now, |_| true), Some(rank(2)));
        view.close(stale).unwrap();
        assert_eq!(
            view.highest(endpoint, now, |claim| claim.lineage() != rank(1).lineage()),
            None
        );
        assert_eq!(view.highest(endpoint, now, |_| true), Some(rank(1)));
        let returning = view.open(other, revision(2), None).unwrap();
        assert_eq!(view.open(other, revision(1), None), Err(Error::Stale));
        assert_eq!(
            view.snapshot(
                stale,
                revision(5),
                claims(&[(endpoint, rank(2))]),
                now,
                deadline
            ),
            Err(Error::Stale)
        );
        view.snapshot(returning, revision(1), claims(&[]), now, deadline)
            .unwrap();
        assert_eq!(view.highest(endpoint, now, |_| true), Some(rank(1)));
        assert_eq!(view.highest(endpoint, deadline, |_| true), None);
    }

    #[test]
    fn gaps_and_reconnect_require_a_current_complete_snapshot_without_evicting_watermarks() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut view = Observations::new(1, 1, 2);
        let node = NodeId::new();
        let session = view.open(node, revision(1), None).unwrap();
        let alice = endpoint("alice");
        let bob = endpoint("bob");
        view.snapshot(
            session,
            revision(1),
            claims(&[(alice, rank(1))]),
            now,
            deadline,
        )
        .unwrap();
        assert_eq!(
            view.delta(
                session,
                revision(2),
                revision(3),
                bob,
                Some(fact(rank(2), [])),
                now,
                deadline
            ),
            Err(Error::NeedsSnapshot)
        );
        assert_eq!(view.highest(alice, now, |_| true), None);
        assert_eq!(
            view.snapshot(
                session,
                revision(2),
                claims(&[(alice, rank(1))]),
                now,
                deadline
            ),
            Err(Error::Stale)
        );
        view.snapshot(
            session,
            revision(3),
            claims(&[(bob, rank(2))]),
            now,
            deadline,
        )
        .unwrap();
        let resumed = view.open(node, revision(1), None).unwrap();
        assert_eq!(view.close(session), Err(Error::Stale));
        assert_eq!(
            view.open(NodeId::new(), revision(1), None),
            Err(Error::Capacity)
        );
        assert_eq!(
            view.delta(resumed, revision(3), revision(4), bob, None, now, deadline),
            Err(Error::NeedsSnapshot)
        );
        assert_eq!(
            view.snapshot(
                resumed,
                revision(4),
                claims(&[(alice, rank(1)), (bob, rank(2))]),
                now,
                deadline
            ),
            Err(Error::Capacity)
        );
        view.snapshot(
            resumed,
            revision(4),
            claims(&[(bob, rank(2))]),
            now,
            deadline,
        )
        .unwrap();
        view.delta(resumed, revision(4), revision(5), bob, None, now, deadline)
            .unwrap();
        assert_eq!(view.highest(bob, now, |_| true), None);
        assert_eq!(
            view.snapshot(
                resumed,
                revision(4),
                claims(&[(bob, rank(2))]),
                now,
                deadline
            ),
            Err(Error::Stale)
        );
    }
}
