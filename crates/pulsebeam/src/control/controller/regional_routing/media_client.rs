//! Poll active selector interests only. Owner replies are not an index: a row
//! survives only while its permit and at least one local interest are current.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU64,
    time::{Duration, SystemTime},
};

use pulsebeam_proto::state::{Track, track::Kind};
use pulsebeam_runtime::time::Instant;

use crate::{
    control::{
        controller::{
            ControllerActor,
            native_service::{MediaInterest, MediaInterestId},
            peer_ownership::relay::{self, wan_source::WanSourceWindow},
        },
        permissions::grant_kind,
    },
    entity::{EndpointKey, TrackId, TrackKind},
};

use super::{
    Member, RegionTree,
    forwarding::{Forwarding, Links, ROUTED_READ_TTL, Router},
    media_permit,
    media_read::{Get, Reply, Row},
};

const MAX_FLIGHTS: usize = 8;
const MAX_ATTACHMENTS: usize = 32;
const DELAY: Duration = Duration::from_millis(250);

struct Query {
    interests: Vec<MediaInterest>,
    target: usize,
    after: Option<NonZeroU64>,
    retry: u8,
    empty: bool,
}

impl Query {
    fn advance(&mut self, next: Option<NonZeroU64>) {
        self.after = next;
        self.retry = 0;
        if next.is_none() {
            self.target = self.target.saturating_add(1);
        }
    }

    fn observe_page(&mut self, empty_confirmed: bool, next: Option<NonZeroU64>) {
        let valid = next.is_none_or(|cursor| self.after.is_none_or(|after| cursor > after));
        self.empty &= empty_confirmed && valid;
        self.advance(next.filter(|_| valid));
    }

    fn failed(&mut self) {
        if self.retry == 0 {
            self.retry = 1;
        } else {
            self.observe_page(false, None);
        }
    }

    fn refresh(&mut self, actor: &ControllerActor, wall: SystemTime) {
        self.interests
            .retain(|interest| actor.media_interest_current(interest, wall));
    }
}

struct Flight {
    query: Query,
    target: Member,
    attempt: u8,
    expires: Instant,
}

pub(super) struct Demand {
    pub(super) source: WanSourceWindow,
    pub(super) descriptor: Track,
    pub(super) permit: Box<[u8]>,
    pub(super) issued_ms: u64,
    wall_expiry_ms: u64,
    pub(super) expires: Instant,
    pub(super) endpoint: EndpointKey,
    track: TrackId,
    owners: BTreeMap<MediaInterestId, u64>,
}

impl Demand {
    pub(super) fn verify(
        tree: &RegionTree,
        origin: Member,
        interests: &[MediaInterest],
        row: Row,
        now: Instant,
        wall: SystemTime,
    ) -> Option<Self> {
        let first = interests.first()?;
        let descriptor = row.descriptor()?;
        let (endpoint, track) = media_permit::source_name(first.project, &descriptor)?;
        let kind = descriptor_kind(&descriptor)?;
        let owners: BTreeMap<_, _> = interests
            .iter()
            .filter(|interest| allows(interest, endpoint, kind, track))
            .map(|interest| (interest.owner, interest.authorization))
            .collect();
        if owners.is_empty() {
            return None;
        }
        let permit = media_permit::verify(
            tree,
            origin,
            endpoint,
            track,
            &descriptor,
            &row.permit,
            wall,
        )?;
        let ttl = Duration::from_millis(
            permit
                .expires_ms
                .checked_sub(media_permit::unix_millis(wall)?)?,
        );
        Some(Self {
            source: permit.source,
            descriptor,
            permit: row.permit,
            issued_ms: permit.issued_ms,
            wall_expiry_ms: permit.expires_ms,
            expires: now.checked_add(ttl)?,
            endpoint,
            track,
            owners,
        })
    }

    pub(super) fn receivers(&self) -> impl Iterator<Item = crate::entity::ConnectionId> + '_ {
        self.owners.keys().map(|&(connection, _)| connection)
    }

    pub(super) fn authorized(
        &self,
        actor: &ControllerActor,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        now < self.expires
            && media_permit::unix_millis(wall).is_some_and(|wall| wall < self.wall_expiry_ms)
            && self.owners.iter().any(|(&owner, &revision)| {
                actor.media_interest_matches(
                    owner,
                    revision,
                    self.endpoint,
                    self.source.publisher.publication.kind(),
                    self.track,
                    wall,
                )
            })
    }

    fn refresh(&mut self, actor: &ControllerActor, now: Instant, wall: SystemTime) -> bool {
        if now >= self.expires
            || media_permit::unix_millis(wall).is_none_or(|now| now >= self.wall_expiry_ms)
        {
            return false;
        }
        self.owners.retain(|&owner, revision| {
            actor.media_interest_matches(
                owner,
                *revision,
                self.endpoint,
                self.source.publisher.publication.kind(),
                self.track,
                wall,
            )
        });
        !self.owners.is_empty()
    }
}

fn descriptor_kind(descriptor: &Track) -> Option<TrackKind> {
    Some(match descriptor.kind.as_ref()? {
        Kind::Audio(_) => TrackKind::Audio,
        Kind::Video(_) => TrackKind::Video,
        Kind::Data(_) => TrackKind::Data,
    })
}

fn allows(
    interest: &MediaInterest,
    endpoint: EndpointKey,
    kind: TrackKind,
    track: TrackId,
) -> bool {
    interest.project == endpoint.room.project
        && interest.scope.allows(
            endpoint.room.room.as_str(),
            endpoint.participant.as_str(),
            grant_kind(kind),
            track.as_str(),
        )
}

#[derive(Default)]
pub(super) struct Client {
    after: Option<MediaInterestId>,
    ready: VecDeque<Query>,
    flights: BTreeMap<NonZeroU64, Flight>,
    demand: BTreeMap<(Member, relay::Key), Demand>,
    serial: u64,
    attempts: BTreeMap<Member, u8>,
    next_sweep: Option<Instant>,
    empty: BTreeMap<MediaInterestId, (MediaInterest, Instant)>,
    changed: BTreeSet<MediaInterestId>,
}

impl Client {
    pub(super) fn take_coverage_changes(&mut self) -> BTreeSet<MediaInterestId> {
        std::mem::take(&mut self.changed)
    }

    pub(super) fn empty_confirmed(
        &self,
        owner: MediaInterestId,
        revision: u64,
        now: Instant,
    ) -> bool {
        self.empty
            .get(&owner)
            .is_some_and(|(interest, expires)| revision == interest.authorization && now < *expires)
    }

    fn finish(&mut self, actor: &ControllerActor, query: Query, now: Instant) {
        let Some(expires) = now.checked_add(ROUTED_READ_TTL) else {
            return;
        };
        for interest in query.interests {
            let owner = interest.owner;
            if !actor.is_regional_selector_interest(owner) {
                continue;
            }
            if query.empty {
                if let Some(current) = self.empty.get_mut(&owner) {
                    if current.0.authorization != interest.authorization {
                        self.changed.insert(owner);
                    }
                    *current = (interest, expires);
                } else if self.empty.len() < relay::LIMIT {
                    self.empty.insert(owner, (interest, expires));
                    self.changed.insert(owner);
                }
            } else if self.empty.remove(&owner).is_some() {
                self.changed.insert(owner);
            }
        }
    }

    pub(super) fn demands(&self) -> impl Iterator<Item = &Demand> {
        self.demand.values()
    }

    pub(super) fn attempt_for(&self, target: Member) -> u8 {
        self.attempts.get(&target).copied().unwrap_or_default()
    }

    pub(super) fn receive(
        &mut self,
        actor: &ControllerActor,
        tree: &RegionTree,
        target: Member,
        reply: Reply,
        now: Instant,
        wall: SystemTime,
    ) {
        let Some(current) = self.flights.get(&reply.id) else {
            return;
        };
        if current.target != target || now >= current.expires {
            return;
        }
        let Some(Flight {
            mut query, attempt, ..
        }) = self.flights.remove(&reply.id)
        else {
            return;
        };
        query.refresh(actor, wall);
        if query.interests.is_empty() {
            return;
        }
        if let Some(page) = reply.page {
            query.observe_page(page.row.as_ref().is_some_and(Row::proves_empty), page.next);
            if let Some(demand) = page
                .row
                .filter(|row| !row.proves_empty())
                .and_then(|row| Demand::verify(tree, target, &query.interests, row, now, wall))
            {
                tracing::debug!(
                    ?target,
                    attempt,
                    issued_ms = demand.issued_ms,
                    valid_ms = demand.expires.saturating_duration_since(now).as_millis(),
                    "regional media permit received"
                );
                self.attempts.insert(target, attempt);
                self.record(target, demand);
            }
        } else {
            // A rejected continuation is not evidence of an empty owner.
            query.observe_page(false, None);
        }
        self.ready.push_back(query);
    }

    fn record(&mut self, origin: Member, mut demand: Demand) {
        let key = (origin, demand.source.scope());
        if let Some(current) = self.demand.get_mut(&key) {
            // An identical signed reply is not a new local lease.
            if demand.issued_ms > current.issued_ms
                || (demand.issued_ms == current.issued_ms
                    && demand.source.sequence_anchor > current.source.sequence_anchor
                    && demand.source.publisher == current.source.publisher)
            {
                std::mem::swap(current, &mut demand);
            }
            for (owner, revision) in demand.owners {
                if current.owners.contains_key(&owner) || current.owners.len() < MAX_ATTACHMENTS {
                    current
                        .owners
                        .entry(owner)
                        .and_modify(|old| *old = (*old).max(revision))
                        .or_insert(revision);
                }
            }
        } else if self.demand.len() < relay::LIMIT {
            self.demand.insert(key, demand);
        }
    }

    pub(super) fn step(
        &mut self,
        actor: &ControllerActor,
        router: &Router<'_, impl Links>,
        forwarding: &mut Forwarding,
        targets: &[Member],
        now: Instant,
        wall: SystemTime,
    ) {
        self.empty.retain(|owner, (interest, expires)| {
            let current = actor.media_interest_current(interest, wall);
            if current && now >= *expires {
                self.changed.insert(*owner);
            }
            current && now < *expires
        });
        self.demand
            .retain(|_, demand| demand.refresh(actor, now, wall));
        if targets.is_empty() {
            return;
        }
        self.flights.retain(|_, flight| {
            flight.query.refresh(actor, wall);
            if flight.query.interests.is_empty() {
                return false;
            }
            if now >= flight.expires {
                let mut query = Query {
                    interests: std::mem::take(&mut flight.query.interests),
                    ..flight.query
                };
                if query.retry != 0 {
                    self.attempts
                        .insert(flight.target, flight.attempt.wrapping_add(1));
                }
                query.failed();
                self.ready.push_back(query);
                return false;
            }
            true
        });
        if self.ready.is_empty()
            && self.flights.is_empty()
            && self.next_sweep.is_none_or(|next| next <= now)
        {
            let (page, next) = actor.media_interest_page(self.after, wall);
            self.after = next;
            if next.is_none() {
                self.next_sweep = now.checked_add(DELAY);
            }
            for interest in page {
                enqueue(&mut self.ready, interest);
            }
        }
        let count = self
            .ready
            .len()
            .min(MAX_FLIGHTS.saturating_sub(self.flights.len()));
        for _ in 0..count {
            let Some(mut query) = self.ready.pop_front() else {
                break;
            };
            query.refresh(actor, wall);
            let Some(interest) = query.interests.first() else {
                continue;
            };
            let Some(&target) = targets.get(query.target) else {
                self.finish(actor, query, now);
                continue;
            };
            let Some(id) = self.serial.checked_add(1).and_then(NonZeroU64::new) else {
                break;
            };
            self.serial = id.get();
            let Some(expires) = now.checked_add(ROUTED_READ_TTL) else {
                break;
            };
            let Some(expires_ms) = wall
                .checked_add(ROUTED_READ_TTL)
                .and_then(media_permit::unix_millis)
            else {
                continue;
            };
            let grant_expires_ms = query
                .interests
                .iter()
                .map(|interest| interest.expires_unix.saturating_mul(1_000))
                .max()
                .unwrap_or_default();
            let get = Get {
                id,
                project: interest.project,
                scope: interest.scope.clone(),
                after: query.after,
                expires_ms: expires_ms.min(grant_expires_ms),
                demand_expires_ms: query
                    .interests
                    .iter()
                    .filter(|interest| interest.wants_media)
                    .map(|interest| interest.expires_unix.saturating_mul(1_000).min(expires_ms))
                    .max(),
            };
            let attempt = self.attempt_for(target).wrapping_add(query.retry);
            if get.encode().is_some_and(|payload| {
                forwarding
                    .request(router, target, attempt, &payload, now)
                    .is_some()
            }) {
                self.flights.insert(
                    id,
                    Flight {
                        query,
                        target,
                        attempt,
                        expires,
                    },
                );
            } else {
                query.failed();
                self.ready.push_back(query);
            }
        }
    }
}

fn enqueue(ready: &mut VecDeque<Query>, interest: MediaInterest) {
    if let Some(query) = ready.iter_mut().find(|query| {
        query
            .interests
            .first()
            .is_some_and(|other: &MediaInterest| {
                other.project == interest.project && other.scope == interest.scope
            })
    }) {
        query.interests.push(interest);
    } else {
        ready.push_back(Query {
            interests: vec![interest],
            target: 0,
            after: None,
            retry: 0,
            empty: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_coverage_requires_every_page_and_target_to_confirm_absence() {
        let query = || Query {
            interests: Vec::new(),
            target: 0,
            after: None,
            retry: 0,
            empty: true,
        };
        let after = NonZeroU64::new(1);
        let mut complete = query();
        complete.observe_page(true, after);
        assert_eq!(complete.target, 0);
        complete.observe_page(true, None);
        complete.failed();
        assert_eq!(complete.target, 1);
        complete.observe_page(true, None);
        assert_eq!(complete.target, 2);
        assert!(complete.empty);

        let mut stalled = query();
        stalled.observe_page(true, after);
        stalled.observe_page(true, after);
        assert!(!stalled.empty);
        assert_eq!(stalled.target, 1);

        let mut unresolved = query();
        unresolved.observe_page(false, after);
        unresolved.observe_page(true, None);
        assert!(!unresolved.empty);

        let mut unreachable = query();
        unreachable.failed();
        unreachable.failed();
        unreachable.observe_page(true, None);
        assert!(!unreachable.empty);
    }
}
