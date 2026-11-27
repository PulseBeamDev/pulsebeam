use super::*;
use crate::entity::{ParticipantId, ProjectId, RoomId};
use pulsebeam_proto::{catalog as limits, prelude::Message, state as wire};
use pulsebeam_runtime::{rand::RngCore, time::Instant};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::{
    num::NonZeroU64,
    ops::Bound::{Excluded, Unbounded},
};

use neighbor::client;
use responders::RoomRead as _;

pub(super) mod neighbor;
pub(super) mod owner;
pub(super) mod responders;

#[cfg(test)]
mod tests;

const WATCH_WORK: usize = 16;
const REGIONAL_WATCHES: usize = 2 * crate::cluster::declarations::MAX_INTERESTS;
const CANDIDATE_WORK: usize = 64;
const CURSOR_LIFETIME: Duration = Duration::from_secs(60);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const ORDERING_VERSION: u8 = 1;
const ENDPOINT_CURSORS: usize = 64;
const SERVER_CURSORS: usize = 4096;
type WatchKey = (ConnectionId, u64);
type Token = [u8; 32];
type Endpoint = (ProjectId, ParticipantId);

#[derive(Clone, PartialEq, Eq)]
struct CatalogCut {
    connection: ConnectionId,
    authorization: u64,
    coverage: responders::Cut,
}

#[derive(Clone, PartialEq, Eq)]
struct Scope {
    project: ProjectId,
    endpoint: ParticipantId,
    room: String,
    limit: u32,
    ordering_version: u8,
}

struct Continuation {
    scope: Scope,
    cut: CatalogCut,
    after: String,
    expires: Instant,
    sequence: u64,
}

#[derive(Default)]
struct Cursors {
    values: HashMap<Token, Continuation>,
    expiry: BTreeSet<(Instant, u64, Token)>,
    endpoints: HashMap<Endpoint, BTreeSet<(Instant, u64, Token)>>,
    sequence: u64,
}

impl Cursors {
    fn remove(&mut self, token: Token) {
        if let Some(value) = self.values.remove(&token) {
            self.expiry.remove(&(value.expires, value.sequence, token));
            let endpoint = (value.scope.project, value.scope.endpoint);
            if let Some(tokens) = self.endpoints.get_mut(&endpoint) {
                tokens.remove(&(value.expires, value.sequence, token));
                if tokens.is_empty() {
                    self.endpoints.remove(&endpoint);
                }
            }
        }
    }

    fn lookup(
        &mut self,
        scope: &Scope,
        token: &[u8],
        now: Instant,
    ) -> Result<&Continuation, wire::FailureCode> {
        let token: Token = token
            .try_into()
            .map_err(|_| wire::FailureCode::InvalidConfiguration)?;
        let value = self
            .values
            .get(&token)
            .ok_or(wire::FailureCode::InvalidConfiguration)?;
        if value.expires <= now {
            self.remove(token);
            return Err(wire::FailureCode::InvalidConfiguration);
        }
        if &value.scope != scope {
            return Err(wire::FailureCode::InvalidConfiguration);
        }
        // Returning a borrow requires looking up again after the expiry removal.
        self.values
            .get(&token)
            .ok_or(wire::FailureCode::InvalidConfiguration)
    }

    #[cfg(test)]
    fn resolve(
        &mut self,
        scope: &Scope,
        cut: &CatalogCut,
        token: &[u8],
        now: Instant,
    ) -> Result<String, wire::FailureCode> {
        let value = self.lookup(scope, token, now)?;
        if &value.cut != cut {
            return Err(wire::FailureCode::ResourceGone);
        }
        Ok(value.after.clone())
    }

    fn issue(
        &mut self,
        scope: Scope,
        cut: CatalogCut,
        after: String,
        now: Instant,
        entropy: &mut (impl RngCore + rand::CryptoRng),
    ) -> Result<Token, wire::FailureCode> {
        let endpoint = (scope.project, scope.endpoint.clone());
        if let Some(tokens) = self.endpoints.get(&endpoint)
            && tokens.len() >= ENDPOINT_CURSORS
            && let Some((_, _, token)) = tokens.first().copied()
        {
            self.remove(token);
        }
        if self.values.len() >= SERVER_CURSORS
            && let Some((_, _, token)) = self.expiry.first().copied()
        {
            self.remove(token);
        }
        let expires = now
            .checked_add(CURSOR_LIFETIME)
            .ok_or(wire::FailureCode::CapacityExceeded)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(wire::FailureCode::CapacityExceeded)?;
        let sequence = self.sequence;
        for _ in 0..4 {
            let mut token = [0; 32];
            entropy.fill_bytes(&mut token);
            if self.values.contains_key(&token) {
                continue;
            }
            self.values.insert(
                token,
                Continuation {
                    scope,
                    cut,
                    after,
                    expires,
                    sequence,
                },
            );
            self.expiry.insert((expires, sequence, token));
            self.endpoints
                .entry(endpoint)
                .or_default()
                .insert((expires, sequence, token));
            return Ok(token);
        }
        Err(wire::FailureCode::CapacityExceeded)
    }
}

struct Build {
    scanned: Option<String>,
    selected: Option<String>,
    participants: u32,
    bytes: usize,
    records: Vec<wire::CatalogPageRecord>,
}

/// Request to keep current and future declarations for one authorized watch.
/// This is a subscription key, not an authorization credential.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct CurrentFuture {
    pub(super) watch_id: u64,
    pub(super) scope: responders::ReadScope,
}

#[derive(Clone, Copy)]
pub(super) enum CatalogChange {
    Changed,
    Gap,
}

struct Watch {
    participant: ParticipantId,
    request: wire::CatalogPageRequest,
    scope: Option<Scope>,
    after: Option<String>,
    revision: u64,
    cut: Option<CatalogCut>,
    refreshed_at: Option<Instant>,
    published_until: Option<Instant>,
    build: Option<Build>,
    interest: Option<responders::ReadScope>,
    gap: bool,
    prior: Option<CatalogCut>,
    remote: Option<RemoteScan>,
}

struct RemoteScan {
    round: NonZeroU64,
    after: Option<ParticipantId>,
    started: Instant,
    missing_since: Option<Instant>,
    revision_changed: bool,
    waiting: bool,
    refresh: bool,
}

impl Watch {
    fn demand(&self, watch_id: u64, peer: responders::Peer) -> Option<client::Demand> {
        let remote = self.remote.as_ref()?;
        let pinned = self.prior.as_ref().or_else(|| {
            self.build.as_ref()?;
            self.cut.as_ref().filter(|cut| !cut.coverage.partial())
        });
        let revision = pinned.and_then(|cut| {
            cut.coverage.peers.iter().find_map(|(id, read)| {
                if *id == peer
                    && let Some(responders::ReadyRevision::Catalog { catalog, .. }) = read
                {
                    return Some(*catalog);
                }
                None
            })
        });
        Some(client::Demand {
            watch: CurrentFuture {
                watch_id,
                scope: self.interest?,
            },
            round: remote.round,
            after: remote.after,
            revision,
        })
    }

    fn page_start(&self) -> Result<Option<ParticipantId>, wire::FailureCode> {
        self.after
            .as_deref()
            .map(ParticipantId::new)
            .transpose()
            .map_err(|_| wire::FailureCode::InvalidConfiguration)
    }
}

#[derive(Default)]
pub(super) struct CatalogRuntime {
    watches: BTreeMap<WatchKey, Watch>,
    cursors: Cursors,
    stamp: (u64, u64),
    reconcile_at: Option<Instant>,
    cursor: Option<WatchKey>,
    active: bool,
    again: bool,
    incomplete: bool,
    remote_serial: u64,
    regional: BTreeSet<WatchKey>,
}

fn expected_responders(controller: &ControllerActor) -> Vec<responders::Peer> {
    if let Some(routing) = &controller.regional_routing {
        return routing
            .catalog_coverage_members()
            .map(|member| (member.node, member.generation))
            .collect();
    }
    controller
        .provisioned_peers
        .iter()
        .map(super::peer_ownership::provisioned::Peer::catalog_responder)
        .collect()
}

fn regional_observations<'a>(
    controller: &'a ControllerActor,
    watch_id: u64,
    watch: &Watch,
    scope: responders::ReadScope,
    expected: &[responders::Peer],
    now: Instant,
) -> Vec<responders::Observation<'a>> {
    if let Some(routing) = &controller.regional_routing {
        let declaration = super::regional_routing::ROUTED;
        let observations: Vec<_> = routing
            .catalog_members()
            .filter_map(|member| {
                let peer = (member.node, member.generation);
                let demand = watch.demand(watch_id, peer)?;
                let view = routing.window(
                    member,
                    client::Order {
                        demand,
                        declaration,
                    },
                    now,
                )?;
                Some(responders::Observation::Ready(responders::Reply {
                    peer,
                    declaration,
                    fresh_until: view.fresh_until(),
                    view,
                }))
            })
            .collect();
        // A local partial page must not consume a suffix whose predecessor could
        // still arrive from a missing owner.
        return if observations.len() == expected.len() {
            observations
        } else {
            Vec::new()
        };
    }
    let mut observations: Vec<_> = controller
        .provisioned_peers
        .iter()
        .filter_map(|peer| {
            let peer_id = peer.catalog_responder();
            if !expected.contains(&peer_id) {
                return None;
            }
            let (declaration, cut) = peer.catalog_declaration(scope.room, now)?;
            if !cut.present {
                return Some(responders::Observation::Empty {
                    peer: peer_id,
                    scope,
                    stream: declaration.stream,
                    interest: declaration.interest,
                    changed_at: declaration.changed_at,
                    fresh_until: cut.fresh_until,
                });
            }
            let demand = watch.demand(watch_id, peer_id)?;
            let view = peer.catalog_window(
                client::Order {
                    demand,
                    declaration,
                },
                now,
            )?;
            Some(responders::Observation::Ready(responders::Reply {
                peer: peer_id,
                declaration,
                fresh_until: cut.fresh_until.min(view.fresh_until()),
                view,
            }))
        })
        .collect();
    // Serve a local partial page while the remote heads are being primed. Do not
    // consume any remote suffix until we can merge against every responder.
    if observations.len() != expected.len() {
        observations
            .retain(|observation| matches!(observation, responders::Observation::Empty { .. }));
    }
    observations
}

fn declarations_match(
    controller: &ControllerActor,
    previous: &CatalogCut,
    room: ProjectRoomKey,
    now: Instant,
) -> bool {
    let expected = expected_responders(controller);
    if expected.len() != previous.coverage.peers.len() {
        return false;
    }
    if controller.regional_routing.is_some() {
        return previous.coverage.peers.iter().all(|(peer, old)| {
            expected.contains(peer) && matches!(old, Some(responders::ReadyRevision::Catalog { declaration, .. }) if *declaration == super::regional_routing::ROUTED)
        });
    }
    previous.coverage.peers.iter().all(|&(peer, ref old)| {
        let Some((id, cut)) = controller
            .provisioned_peers
            .iter()
            .find(|configured| configured.catalog_responder() == peer)
            .and_then(|configured| configured.catalog_declaration(room, now))
        else {
            return false;
        };
        match old {
            Some(responders::ReadyRevision::Empty {
                stream,
                interest,
                changed_at,
            }) => {
                !cut.present
                    && (id.stream, id.interest, id.changed_at) == (*stream, *interest, *changed_at)
            }
            Some(responders::ReadyRevision::Catalog { declaration, .. }) => {
                cut.present && *declaration == id
            }
            None => false,
        }
    })
}

impl CatalogRuntime {
    pub(super) fn sync(
        &mut self,
        connection: ConnectionId,
        participant: ParticipantId,
        requests: Vec<wire::CatalogPageRequest>,
    ) {
        let removed: Vec<_> = self
            .watches
            .range((connection, 0)..=(connection, u64::MAX))
            .filter(|(_, watch)| {
                !requests
                    .iter()
                    .any(|request| request.watch_id == watch.request.watch_id)
            })
            .map(|(key, _)| *key)
            .collect();
        for key in removed {
            self.watches.remove(&key);
            self.regional.remove(&key);
        }
        for request in requests {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.watches.entry((connection, request.watch_id))
            {
                entry.insert(Watch {
                    participant,
                    request,
                    scope: None,
                    after: None,
                    revision: 0,
                    cut: None,
                    refreshed_at: None,
                    published_until: None,
                    build: None,
                    interest: None,
                    gap: false,
                    prior: None,
                    remote: None,
                });
                self.active = true;
                self.again = true;
            }
        }
    }

    pub(super) fn remove_connection(&mut self, connection: ConnectionId) {
        let keys: Vec<_> = self
            .watches
            .range((connection, 0)..=(connection, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            self.watches.remove(&key);
            self.regional.remove(&key);
        }
    }

    #[cfg(test)]
    pub(super) fn current_future_interests(&self) -> impl Iterator<Item = CurrentFuture> + '_ {
        self.watches.iter().filter_map(|(&(_, watch_id), watch)| {
            watch
                .interest
                .map(|scope| CurrentFuture { watch_id, scope })
        })
    }

    pub(super) fn declaration_rooms(
        &self,
        controller: &ControllerActor,
    ) -> BTreeSet<ProjectRoomKey> {
        let mut rooms = BTreeSet::new();
        for &(connection, watch_id) in &self.regional {
            let Some(scope) = self
                .watches
                .get(&(connection, watch_id))
                .and_then(|watch| watch.interest)
            else {
                continue;
            };
            if self
                .live_lease(controller, CurrentFuture { watch_id, scope })
                .is_some()
            {
                rooms.insert(scope.room);
                if rooms.len() == crate::cluster::declarations::MAX_INTERESTS {
                    break;
                }
            }
        }
        rooms
    }

    fn begin_remote(
        &mut self,
        watch: &mut Watch,
        now: Instant,
        refresh: bool,
    ) -> Result<(), wire::FailureCode> {
        let round = self
            .remote_serial
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(wire::FailureCode::CapacityExceeded)?;
        self.remote_serial = round.get();
        watch.remote = Some(RemoteScan {
            round,
            after: watch.page_start()?,
            started: now,
            missing_since: None,
            revision_changed: false,
            waiting: false,
            refresh,
        });
        Ok(())
    }

    pub(super) fn demands<'a>(
        &'a self,
        controller: &'a ControllerActor,
        peer: responders::Peer,
    ) -> impl Iterator<Item = client::Demand> + 'a {
        self.regional.iter().filter_map(move |key| {
            let demand = self.watches.get(key)?.demand(key.1, peer)?;
            self.live_lease(controller, demand.watch)?;
            Some(demand)
        })
    }

    pub(super) fn discover_for(
        &self,
        controller: &ControllerActor,
        interest: CurrentFuture,
    ) -> Option<pulsebeam_core::auth::grants::RoomDiscover> {
        let lease = self.live_lease(controller, interest)?;
        match lease.permissions() {
            crate::control::permissions::ResourcePermissions::Grants { grants, .. } => {
                grants.room_discover(interest.scope.room.room.as_str())
            }
            _ => None,
        }
    }

    fn live_lease<'a>(
        &self,
        controller: &'a ControllerActor,
        interest: CurrentFuture,
    ) -> Option<&'a AuthorizationLease> {
        let watch = self
            .watches
            .get(&(interest.scope.viewer, interest.watch_id))?;
        if watch.interest != Some(interest.scope)
            || watch.gap
            || !controller
                .native_connections
                .get(&interest.scope.viewer)
                .is_some_and(|native| native.handler.model.catalog.contains(interest.watch_id))
        {
            return None;
        }
        let lease = controller
            .core
            .registry
            .get_incarnation(watch.participant, interest.scope.viewer)?
            .authorization
            .as_ref()?;
        (lease.revision == interest.scope.authorization
            && lease.authority.project_id == interest.scope.room.project
            && !lease.is_expired_at(SystemTime::now()))
        .then_some(lease)
    }

    pub(super) fn receive_remote(
        &mut self,
        interest: CurrentFuture,
        result: Result<(), wire::FailureCode>,
    ) {
        if result == Err(wire::FailureCode::ResourceGone)
            && let Some(watch) = self
                .watches
                .get_mut(&(interest.scope.viewer, interest.watch_id))
            && watch.interest == Some(interest.scope)
        {
            watch.gap |= watch.prior.is_some();
            if let Some(read) = &mut watch.remote {
                read.revision_changed = true;
            }
        }
        self.notify(interest, CatalogChange::Changed);
    }

    /// Coalesce hints in the bounded watch table. Losing a hint is harmless
    /// because periodic reconciliation rereads the owners; losing a gap is not.
    pub(super) fn notify(&mut self, interest: CurrentFuture, change: CatalogChange) -> bool {
        let key = (interest.scope.viewer, interest.watch_id);
        let Some(watch) = self.watches.get_mut(&key) else {
            return false;
        };
        if watch.interest != Some(interest.scope) {
            return false;
        }
        watch.gap |= matches!(change, CatalogChange::Gap);
        self.active = true;
        self.again = true;
        true
    }

    pub(super) fn has_work(&self) -> bool {
        self.active
    }

    #[cfg(test)]
    pub(super) fn skip_change_wake(&mut self, presence: u64, publications: u64) {
        assert!(!self.active);
        self.stamp = (presence, publications);
    }

    pub(super) fn service(&mut self, controller: &mut ControllerActor) {
        self.service_with_coverage(controller, &expected_responders(controller), &[]);
    }

    pub(super) fn service_with_coverage(
        &mut self,
        controller: &mut ControllerActor,
        expected: &[responders::Peer],
        observations: &[responders::Observation<'_>],
    ) {
        let now = Instant::now();
        if self.watches.is_empty() {
            self.reconcile_at = None;
        } else if self.reconcile_at.is_none_or(|deadline| now >= deadline) {
            self.reconcile_at = now.checked_add(RECONCILE_INTERVAL);
            self.active = true;
            self.again = true;
        }
        let stamp = (
            controller.core.registry.catalog_version(),
            controller.lifecycle.catalog_version(),
        );
        if stamp != self.stamp {
            self.stamp = stamp;
            self.active = true;
            self.again = true;
        }
        let mut candidates = CANDIDATE_WORK;
        for _ in 0..WATCH_WORK {
            if !self.active {
                break;
            }
            let next = match self.cursor {
                Some(cursor) => self.watches.range((Excluded(cursor), Unbounded)).next(),
                None => self.watches.first_key_value(),
            }
            .map(|(key, _)| *key);
            let Some(key) = next else {
                self.cursor = None;
                self.active = self.again || self.incomplete;
                self.again = false;
                self.incomplete = false;
                continue;
            };
            self.cursor = Some(key);
            let Some(mut watch) = self.watches.remove(&key) else {
                continue;
            };
            if !controller
                .native_connections
                .get(&key.0)
                .is_some_and(|native| native.handler.model.catalog.contains(key.1))
            {
                self.regional.remove(&key);
                continue;
            }
            match self.advance(
                controller,
                key,
                &mut watch,
                &mut candidates,
                expected,
                observations,
            ) {
                Ok(()) => {
                    self.incomplete |= watch.build.is_some()
                        && !watch.remote.as_ref().is_some_and(|remote| remote.waiting);
                    self.watches.insert(key, watch);
                }
                Err(code) => {
                    self.regional.remove(&key);
                    if let Some(native) = controller.native_connections.get_mut(&key.0) {
                        let result = native.handler.model.fail_catalog(key.1, code);
                        if result.is_err() || !native.handler.handoff.schedule() {
                            controller.remove_incarnation(watch.participant, key.0);
                        }
                    }
                }
            }
        }
    }

    fn advance(
        &mut self,
        controller: &mut ControllerActor,
        key: WatchKey,
        watch: &mut Watch,
        candidates: &mut usize,
        expected: &[responders::Peer],
        observations: &[responders::Observation<'_>],
    ) -> Result<(), wire::FailureCode> {
        let registry = &controller.core.registry;
        let meta = registry
            .get_incarnation(watch.participant, key.0)
            .ok_or(wire::FailureCode::ResourceGone)?;
        let lease = meta
            .authorization
            .as_ref()
            .filter(|lease| !lease.is_expired_at(SystemTime::now()))
            .ok_or(wire::FailureCode::PermissionDenied)?;
        let scope = Scope {
            project: lease.authority.project_id,
            endpoint: watch.participant,
            room: watch.request.room.clone(),
            limit: watch.request.participant_limit,
            ordering_version: ORDERING_VERSION,
        };
        let room = RoomId::new(&scope.room).map_err(|_| wire::FailureCode::InvalidConfiguration)?;
        let room_scope = ProjectRoomKey {
            project: scope.project,
            room,
        };
        let read_scope = responders::ReadScope {
            room: room_scope,
            viewer: key.0,
            authorization: lease.revision,
        };
        let local = owner::RoomQuery::authorized(controller, read_scope, lease.permissions())?;
        let now = Instant::now();
        if watch.scope.is_none() {
            if !watch.request.cursor.is_empty() {
                let value = self.cursors.lookup(&scope, &watch.request.cursor, now)?;
                watch.after = Some(value.after.clone());
                watch.prior = Some(value.cut.clone());
            }
            watch.scope = Some(scope.clone());
        } else if watch.scope.as_ref() != Some(&scope) {
            return Err(wire::FailureCode::InvalidConfiguration);
        }
        if watch.prior.as_ref().is_some_and(|prior| {
            prior.connection != key.0
                || prior.authorization != lease.revision
                || prior.coverage.local != local.revision()
        }) {
            return Err(wire::FailureCode::ResourceGone);
        }
        if watch.interest != Some(read_scope) {
            watch.interest = Some(read_scope);
            watch.gap = false;
            if self.regional.contains(&key) {
                self.begin_remote(watch, now, false)?;
            }
        }
        if !expected.is_empty() && watch.remote.is_none() && self.regional.len() < REGIONAL_WATCHES
        {
            self.begin_remote(watch, now, false)?;
            self.regional.insert(key);
        }
        if watch.gap {
            return Err(wire::FailureCode::ResourceGone);
        }
        let timed_refresh = watch.after.is_none()
            && watch.build.is_none()
            && watch
                .refreshed_at
                .is_some_and(|last| now.duration_since(last) >= RECONCILE_INTERVAL);
        if watch
            .remote
            .as_ref()
            .is_some_and(|read| read.revision_changed)
        {
            watch.build = None;
            watch.published_until = None;
            self.begin_remote(watch, now, false)?;
        } else if timed_refresh && watch.remote.as_ref().is_some_and(|remote| !remote.refresh) {
            self.begin_remote(watch, now, true)?;
        }
        let remote = regional_observations(controller, key.1, watch, read_scope, expected, now);
        let responders = responders::Responders::at_cut(
            &local,
            expected.iter().copied(),
            observations.iter().copied().chain(remote),
            now,
        )?;
        let cut = CatalogCut {
            connection: key.0,
            authorization: lease.revision,
            coverage: responders.cut().clone(),
        };
        let partial = cut.coverage.partial();
        let assembling = watch
            .remote
            .as_ref()
            .is_some_and(|read| now.duration_since(read.started) < RECONCILE_INTERVAL);
        if partial
            && let Some(build) = &watch.build
            && let Some(old) = &watch.cut
            && !old.coverage.partial()
            && old.connection == cut.connection
            && old.authorization == cut.authorization
            && old.coverage.local == cut.coverage.local
            && declarations_match(controller, old, room_scope, now)
            && let Some(read) = &mut watch.remote
            && now.duration_since(*read.missing_since.get_or_insert(now)) < RECONCILE_INTERVAL
        {
            // Preserve the prefix only while revalidating every owner at the same cut.
            read.after = build
                .scanned
                .as_deref()
                .map(ParticipantId::new)
                .transpose()
                .map_err(|_| wire::FailureCode::InternalFailure)?;
            read.waiting = true;
            return Ok(());
        }
        if let Some(prior) = &watch.prior
            && *prior != cut
        {
            if watch.cut.is_none()
                && partial
                && assembling
                && declarations_match(controller, prior, room_scope, now)
            {
                return Ok(());
            }
            return Err(wire::FailureCode::ResourceGone);
        }
        let refresh = match watch.remote.as_ref() {
            Some(read) => read.refresh,
            None => timed_refresh,
        };
        if refresh
            && partial
            && assembling
            && watch.published_until.is_some_and(|deadline| now < deadline)
            && watch.cut.as_ref().is_some_and(|old| {
                old.coverage.local == cut.coverage.local
                    && !old.coverage.partial()
                    && declarations_match(controller, old, room_scope, now)
            })
        {
            // Declarations alone cannot extend a published page's freshness.
            return Ok(());
        }
        let changed = watch.cut.as_ref() != Some(&cut);
        if changed
            && watch
                .remote
                .as_ref()
                .is_some_and(|read| Some(read.after) != watch.page_start().ok())
        {
            // A buffered suffix of the old build cannot serve a new page cut.
            self.begin_remote(watch, now, false)?;
            watch.build = None;
            self.again = true;
            return Ok(());
        }
        if let Some(read) = &mut watch.remote {
            read.waiting = false;
            read.missing_since = None;
            read.refresh = false;
        }
        if changed || refresh {
            watch.revision = watch
                .revision
                .checked_add(1)
                .ok_or(wire::FailureCode::CapacityExceeded)?;
            watch.cut = Some(cut);
            watch.build = Some(Build {
                scanned: watch.after.clone(),
                selected: None,
                participants: 0,
                bytes: 0,
                records: Vec::new(),
            });
        }
        let Some(build) = watch.build.as_mut() else {
            return Ok(());
        };
        let fresh_until = responders.fresh_until();
        let scan_cost = responders.scan_cost();
        while *candidates >= scan_cost {
            *candidates = candidates.saturating_sub(scan_cost);
            let owner::Candidate { canonical, entry } =
                match responders.next(build.scanned.as_deref())? {
                    responders::ReadStep::Row(candidate) => candidate,
                    responders::ReadStep::Exhausted => {
                        watch.published_until = fresh_until;
                        return self.finish(controller, key, watch, false);
                    }
                    responders::ReadStep::Refill => {
                        let read = watch
                            .remote
                            .as_mut()
                            .ok_or(wire::FailureCode::InternalFailure)?;
                        read.after = build
                            .scanned
                            .as_deref()
                            .map(ParticipantId::new)
                            .transpose()
                            .map_err(|_| wire::FailureCode::InternalFailure)?;
                        read.waiting = true;
                        return Ok(());
                    }
                };
            build.scanned = Some(canonical.clone());
            let Some(entry) = entry else {
                continue;
            };
            if build.participants == scope.limit {
                watch.published_until = fresh_until;
                return self.finish(controller, key, watch, true);
            }
            let metadata = limits::CatalogPageMetadata {
                watch_id: key.1,
                room: scope.room.clone(),
                revision: watch.revision,
                complete: true,
                partial,
                has_more: !partial,
                next_cursor: if partial { Vec::new() } else { vec![0; 32] },
                cursor_expires_at_unix_seconds: if partial { 0 } else { i64::MAX },
            };
            let records = limits::fragment_entry(&metadata, build.participants, &entry)
                .map_err(|_| wire::FailureCode::CapacityExceeded)?;
            let bytes = records.iter().fold(0usize, |total, record| {
                total.saturating_add(record.encoded_len())
            });
            if build.bytes.saturating_add(bytes) > limits::MAX_CATALOG_PAGE_BYTES
                || build.records.len().saturating_add(records.len())
                    > limits::MAX_CATALOG_PAGE_RECORDS
            {
                if build.records.is_empty() {
                    return Err(wire::FailureCode::CapacityExceeded);
                }
                watch.published_until = fresh_until;
                return self.finish(controller, key, watch, true);
            }
            build.bytes = build.bytes.saturating_add(bytes);
            build.records.extend(records);
            build.selected = Some(canonical);
            build.participants = build.participants.saturating_add(1);
        }
        Ok(())
    }

    fn finish(
        &mut self,
        controller: &mut ControllerActor,
        key: WatchKey,
        watch: &mut Watch,
        more: bool,
    ) -> Result<(), wire::FailureCode> {
        let mut build = watch
            .build
            .take()
            .ok_or(wire::FailureCode::InternalFailure)?;
        let now = Instant::now();
        let cut = watch
            .cut
            .as_ref()
            .ok_or(wire::FailureCode::InternalFailure)?;
        let partial = cut.coverage.partial();
        let more = more && !partial;
        let (cursor, expiry) = if more {
            let scope = watch
                .scope
                .clone()
                .ok_or(wire::FailureCode::InternalFailure)?;
            let after = build.selected.ok_or(wire::FailureCode::CapacityExceeded)?;
            let token = self.cursors.issue(
                scope,
                cut.clone(),
                after,
                now,
                &mut controller.ownership_entropy,
            )?;
            let expiry = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .checked_add(60)
                .and_then(|value| i64::try_from(value).ok())
                .ok_or(wire::FailureCode::CapacityExceeded)?;
            (token.to_vec(), expiry)
        } else {
            (Vec::new(), 0)
        };
        if build.records.is_empty() {
            build.records.push(wire::CatalogPageRecord {
                watch_id: key.1,
                room: watch.request.room.clone(),
                revision: watch.revision,
                complete: false,
                partial,
                has_more: false,
                next_cursor: Vec::new(),
                cursor_expires_at_unix_seconds: 0,
                fragments: Vec::new(),
            });
        }
        for record in &mut build.records {
            record.complete = false;
            record.has_more = more;
            record.next_cursor.clone_from(&cursor);
            record.cursor_expires_at_unix_seconds = expiry;
        }
        if let Some(record) = build.records.last_mut() {
            record.complete = true;
        }
        let native = controller
            .native_connections
            .get_mut(&key.0)
            .ok_or(wire::FailureCode::ResourceGone)?;
        native
            .handler
            .model
            .replace_catalog(key.1, watch.revision, build.records)
            .map_err(|_| wire::FailureCode::CapacityExceeded)?;
        if !native.handler.handoff.schedule() {
            return Err(wire::FailureCode::CapacityExceeded);
        }
        watch.refreshed_at = Some(now);
        Ok(())
    }
}
