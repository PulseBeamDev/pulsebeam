use super::*;
use crate::entity::{ParticipantId, ProjectId, RoomId};
use pulsebeam_core::auth::grants::Action;
use pulsebeam_proto::{catalog as limits, prelude::Message, state as wire};
use pulsebeam_runtime::{rand::RngCore, time::Instant};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound::{Excluded, Unbounded};

#[cfg(test)]
mod tests;

const WATCH_WORK: usize = 16;
const CANDIDATE_WORK: usize = 64;
const CURSOR_LIFETIME: Duration = Duration::from_secs(60);
const ORDERING_VERSION: u8 = 1;
const ENDPOINT_CURSORS: usize = 64;
const SERVER_CURSORS: usize = 4096;
type WatchKey = (ConnectionId, u64);
type Token = [u8; 32];
type Endpoint = (ProjectId, ParticipantId);

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

    fn resolve(
        &mut self,
        scope: &Scope,
        token: &[u8],
        now: Instant,
    ) -> Result<String, wire::FailureCode> {
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
        Ok(value.after.clone())
    }

    fn issue(
        &mut self,
        scope: Scope,
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

struct Watch {
    participant: ParticipantId,
    request: wire::CatalogPageRequest,
    scope: Option<Scope>,
    after: Option<String>,
    revision: u64,
    stamp: Option<(u64, u64, u64)>,
    build: Option<Build>,
}

#[derive(Default)]
pub(super) struct CatalogRuntime {
    watches: BTreeMap<WatchKey, Watch>,
    cursors: Cursors,
    stamp: (u64, u64),
    cursor: Option<WatchKey>,
    active: bool,
    again: bool,
    incomplete: bool,
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
                    stamp: None,
                    build: None,
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
        }
    }

    pub(super) fn has_work(&self) -> bool {
        self.active
    }

    pub(super) fn service(&mut self, controller: &mut ControllerActor) {
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
                continue;
            }
            match self.advance(controller, key, &mut watch, &mut candidates) {
                Ok(()) => {
                    self.incomplete |= watch.build.is_some();
                    self.watches.insert(key, watch);
                }
                Err(code) => {
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
        let allowed = match lease.permissions() {
            super::super::permissions::ResourcePermissions::Grants { grants, .. } => {
                grants.may_discover(&watch.request.room)
            }
            #[cfg(test)]
            super::super::permissions::ResourcePermissions::RoomBound => true,
            _ => false,
        };
        if !allowed {
            return Err(wire::FailureCode::PermissionDenied);
        }
        let scope = Scope {
            project: lease.authority.project_id,
            endpoint: watch.participant,
            room: watch.request.room.clone(),
            limit: watch.request.participant_limit,
            ordering_version: ORDERING_VERSION,
        };
        if watch.scope.is_none() {
            watch.after = if watch.request.cursor.is_empty() {
                None
            } else {
                Some(
                    self.cursors
                        .resolve(&scope, &watch.request.cursor, Instant::now())?,
                )
            };
            watch.scope = Some(scope.clone());
        } else if watch.scope.as_ref() != Some(&scope) {
            return Err(wire::FailureCode::InvalidConfiguration);
        }
        let room = RoomId::new(&scope.room).map_err(|_| wire::FailureCode::InvalidConfiguration)?;
        let room_scope = ProjectRoomKey {
            project: scope.project,
            room,
        };
        let stamp = (
            registry.room_catalog_version(room_scope),
            controller.lifecycle.room_catalog_version(room_scope),
            lease.revision,
        );
        if watch.stamp != Some(stamp) {
            watch.revision = watch
                .revision
                .checked_add(1)
                .ok_or(wire::FailureCode::CapacityExceeded)?;
            watch.stamp = Some(stamp);
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
        while *candidates != 0 {
            let candidate = registry
                .ordered_participants_in_room(&room_scope, build.scanned.as_deref())
                .next();
            let Some((canonical, participant, publisher)) = candidate else {
                return self.finish(controller, key, watch, false);
            };
            *candidates = candidates.saturating_sub(1);
            let canonical = canonical.to_owned();
            let entry = catalog_entry(
                controller,
                lease.permissions(),
                room_scope,
                &scope.room,
                participant,
                publisher,
            )?;
            build.scanned = Some(canonical.clone());
            let Some(entry) = entry else {
                continue;
            };
            if build.participants == scope.limit {
                return self.finish(controller, key, watch, true);
            }
            let metadata = limits::CatalogPageMetadata {
                watch_id: key.1,
                room: scope.room.clone(),
                revision: watch.revision,
                complete: true,
                has_more: true,
                next_cursor: vec![0; 32],
                cursor_expires_at_unix_seconds: i64::MAX,
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
        let (cursor, expiry) = if more {
            let scope = watch
                .scope
                .clone()
                .ok_or(wire::FailureCode::InternalFailure)?;
            let after = build.selected.ok_or(wire::FailureCode::CapacityExceeded)?;
            let token = self
                .cursors
                .issue(scope, after, now, &mut controller.ownership_entropy)?;
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
        Ok(())
    }
}

fn catalog_entry(
    controller: &ControllerActor,
    permissions: &super::super::permissions::ResourcePermissions,
    room: ProjectRoomKey,
    literal: &str,
    participant: ParticipantId,
    publisher: &super::super::registry::ParticipantMeta,
) -> Result<Option<wire::CatalogParticipantEntry>, wire::FailureCode> {
    if !publisher.materialized {
        return Ok(None);
    }

    let presence = permissions.allows_at_room(
        Action::Discover,
        literal,
        Some(participant.as_str()),
        None,
        None,
    );
    let mut tracks = Vec::new();
    for (index, track) in controller
        .lifecycle
        .publisher_sources(room, participant, publisher.connection_id)
        .enumerate()
    {
        if index >= limits::MAX_TRACKS_PER_PARTICIPANT {
            return Err(wire::FailureCode::CapacityExceeded);
        }
        if !publisher.authorization.as_ref().is_some_and(|lease| {
            !lease.is_expired_at(SystemTime::now())
                && lease
                    .permissions()
                    .allows_source(Action::Publish, None, track)
        }) {
            continue;
        }
        if permissions.allows_source_at_room(
            Action::Discover,
            literal,
            Some(participant.as_str()),
            track,
        ) {
            tracks.push(
                super::super::native::track_descriptor(track)
                    .ok_or(wire::FailureCode::InternalFailure)?,
            );
        }
    }
    if !presence && tracks.is_empty() {
        return Ok(None);
    }
    tracks.sort_by(|left, right| left.publication_id.cmp(&right.publication_id));
    Ok(Some(wire::CatalogParticipantEntry {
        data_recipient_id: Some(pulsebeam_proto::topic::data_recipient_id(
            publisher.connection_id.as_bytes(),
        )),
        participant: Some(wire::Participant {
            participant_id: participant.as_str().to_owned(),
            room_id: literal.to_owned(),
        }),
        tracks,
    }))
}
