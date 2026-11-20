use super::*;
use crate::entity::{TrackId, TrackKind};

const NATIVE_BUDGET: usize = 32;
const MAX_TERMINAL_HANDOFFS: usize = 256;
const TERMINAL_HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
const LOOKUP_WAKE_BUDGET: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceLookupKey {
    room: ProjectRoomKey,
    participant: ParticipantId,
    kind: TrackKind,
    track_id: TrackId,
}

impl SourceLookupKey {
    fn from_address(
        room: ProjectRoomKey,
        address: &pulsebeam_proto::state::LogicalSourceAddress,
    ) -> Option<Self> {
        Some(Self {
            room,
            participant: ParticipantId::new(&address.participant_id).ok()?,
            kind: crate::control::native::track_kind(address.kind)?,
            track_id: TrackId::new(&address.track_id).ok()?,
        })
    }
}

#[derive(Default)]
pub(super) struct SourceLookupIndex {
    by_source: std::collections::BTreeMap<
        SourceLookupKey,
        std::collections::BTreeSet<(ConnectionId, u64)>,
    >,
    by_owner: std::collections::BTreeMap<(ConnectionId, u64), SourceLookupKey>,
    wake_queue: std::collections::VecDeque<SourceLookupKey>,
    wake_queued: std::collections::BTreeSet<SourceLookupKey>,
    wake_cursor: std::collections::BTreeMap<SourceLookupKey, (ConnectionId, u64)>,
    wake_again: std::collections::BTreeSet<SourceLookupKey>,
}

impl SourceLookupIndex {
    fn change(
        &mut self,
        connection: ConnectionId,
        source_room: ProjectRoomKey,
        change: crate::control::native::LookupInterestChange,
    ) {
        let owner = (connection, change.reference);
        if let Some(previous) = self.by_owner.remove(&owner) {
            if let Some(owners) = self.by_source.get_mut(&previous) {
                owners.remove(&owner);
                if owners.is_empty() {
                    self.by_source.remove(&previous);
                }
            }
        }
        if change.active
            && let Some(key) = SourceLookupKey::from_address(source_room, &change.address)
        {
            self.by_source.entry(key).or_default().insert(owner);
            self.by_owner.insert(owner, key);
        }
    }

    pub(super) fn remove_connection(&mut self, connection: ConnectionId) {
        let owners = self
            .by_owner
            .range((connection, 0)..=(connection, u64::MAX))
            .map(|(owner, _)| *owner)
            .collect::<Vec<_>>();
        for owner in owners {
            if let Some(key) = self.by_owner.remove(&owner)
                && let Some(indexed) = self.by_source.get_mut(&key)
            {
                indexed.remove(&owner);
                if indexed.is_empty() {
                    self.by_source.remove(&key);
                }
            }
        }
    }

    fn source_changed(&mut self, key: SourceLookupKey) {
        if !self.by_source.contains_key(&key) {
            return;
        }
        if self.wake_queued.insert(key) {
            self.wake_queue.push_back(key);
        } else {
            self.wake_again.insert(key);
        }
    }
}

pub(super) struct RetiringNative {
    handler: StateProtocolHandler,
    meta: crate::control::registry::ParticipantMeta,
    deadline: pulsebeam_runtime::time::Instant,
}

impl ControllerActor {
    fn source_room(&self, connection: ConnectionId, room: &str) -> Option<ProjectRoomKey> {
        let external = crate::entity::RoomId::new(room).ok()?;
        let (_, meta) = self.core.registry.get_connection(connection)?;
        let project = meta.scope.project;
        let room = match &meta.authorization {
            Some(_) => external,
            #[cfg(test)]
            None => external,
            #[cfg(not(test))]
            None => return None,
        };
        Some(ProjectRoomKey { project, room })
    }

    pub(super) fn native_resource_lease(
        &self,
        connection: ConnectionId,
    ) -> Option<&AuthorizationLease> {
        if let Some(native) = self.native_connections.get(&connection)
            && let Some(meta) = self
                .core
                .registry
                .get_incarnation(native.handler.participant, connection)
            && meta.profile == SignalingProtocol::State
            && let Some(lease) = &meta.authorization
            && lease.matches_scope(meta.scope, native.handler.participant)
        {
            return Some(lease);
        }
        // Terminal metadata can authorize exact cleanup, never prove quiescence.
        if let Some(retiring) = self.retiring_native.get(&connection)
            && retiring.meta.connection_id == connection
            && retiring.meta.profile == SignalingProtocol::State
            && let Some(lease) = &retiring.meta.authorization
            && lease.matches_scope(retiring.meta.scope, retiring.handler.participant)
        {
            return Some(lease);
        }
        self.materializations.iter().find_map(|work| {
            let pending = &work.pending;
            let lease = pending.authorization.as_ref()?;
            (pending.connection_id == connection
                && pending.profile == SignalingProtocol::State
                && lease.matches_scope(pending.scope, pending.participant))
            .then_some(lease)
        })
    }

    pub(super) fn service_native_connections(&mut self) {
        self.service_lookup_wakes();
        for _ in 0..NATIVE_BUDGET {
            let Some(&(deadline, connection)) = self.native_deadlines.first() else {
                break;
            };
            if deadline > pulsebeam_runtime::time::Instant::now() {
                break;
            }
            self.native_deadlines.remove(&(deadline, connection));
            self.service_native_connection(connection);
        }
        for _ in 0..NATIVE_BUDGET {
            let Ok(connection) = self.native_ready_rx.try_recv() else {
                break;
            };
            self.service_native_connection(connection);
        }
    }

    fn service_lookup_wakes(&mut self) {
        for _ in 0..LOOKUP_WAKE_BUDGET {
            let Some(key) = self.source_lookup_index.wake_queue.pop_front() else {
                break;
            };
            if !self.source_lookup_index.wake_queued.contains(&key) {
                continue;
            }
            let cursor = self.source_lookup_index.wake_cursor.get(&key).copied();
            let next = self
                .source_lookup_index
                .by_source
                .get(&key)
                .and_then(|owners| {
                    owners
                        .range((
                            cursor.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                        .copied()
                });
            let Some(owner) = next else {
                if self.source_lookup_index.wake_again.remove(&key) {
                    self.source_lookup_index.wake_cursor.remove(&key);
                    self.source_lookup_index.wake_queue.push_back(key);
                    continue;
                }
                self.source_lookup_index.wake_queued.remove(&key);
                self.source_lookup_index.wake_cursor.remove(&key);
                continue;
            };
            let (connection, _) = owner;
            let Some(service) = self.native_connections.get_mut(&connection) else {
                self.source_lookup_index.remove_connection(connection);
                self.source_lookup_index.wake_queue.push_back(key);
                continue;
            };
            service.handler.model.source_lookup_changed();
            service.handler.model.source_wait_changed();
            if !service.handler.handoff.schedule() {
                self.source_lookup_index.wake_queue.push_back(key);
                continue;
            }
            self.source_lookup_index.wake_cursor.insert(key, owner);
            let more = self
                .source_lookup_index
                .by_source
                .get(&key)
                .is_some_and(|owners| {
                    owners
                        .range((std::ops::Bound::Excluded(owner), std::ops::Bound::Unbounded))
                        .next()
                        .is_some()
                });
            if more {
                self.source_lookup_index.wake_queue.push_back(key);
            } else if self.source_lookup_index.wake_again.remove(&key) {
                self.source_lookup_index.wake_cursor.remove(&key);
                self.source_lookup_index.wake_queue.push_back(key);
            } else {
                self.source_lookup_index.wake_queued.remove(&key);
                self.source_lookup_index.wake_cursor.remove(&key);
            }
        }
    }

    fn service_native_connection(&mut self, connection: ConnectionId) {
        if let Some(retiring) = self.retiring_native.remove(&connection) {
            self.service_native_retirement(connection, retiring);
            return;
        }
        let Some(NativeService {
            handler: mut native,
            deadline,
        }) = self.native_connections.remove(&connection)
        else {
            return;
        };
        if let Some(deadline) = deadline {
            self.native_deadlines.remove(&(deadline, connection));
        }
        let participant = native.participant;
        let Some(meta) = self
            .core
            .registry
            .get_incarnation(participant, connection)
            .filter(|meta| meta.scope.room == native.room)
        else {
            self.catalog.remove_connection(connection);
            self.source_lookup_index.remove_connection(connection);
            return;
        };
        let room = meta.scope.joined_room();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match native.service(
            unix_now,
            |track| {
                let track = self.lifecycle.source_for_connection(
                    room?,
                    connection,
                    track,
                    &self.core.registry,
                )?;
                Some(crate::control::native::ResolvedSource {
                    track,
                    publisher: &track.meta().origin,
                })
            },
            |source_room, track| {
                let source_room = self.source_room(connection, source_room)?;
                let track = self.lifecycle.source_for_connection(
                    source_room,
                    connection,
                    track,
                    &self.core.registry,
                )?;
                Some(crate::control::native::ResolvedSource {
                    track,
                    publisher: &track.meta().origin,
                })
            },
            |address| {
                let source_room = self.source_room(connection, &address.room)?;
                let track = self.lifecycle.source_by_address(
                    source_room,
                    connection,
                    address,
                    &self.core.registry,
                )?;
                Some(crate::control::native::ResolvedSource {
                    track,
                    publisher: &track.meta().origin,
                })
            },
        ) {
            Ok(wake) => {
                self.catalog.sync(
                    connection,
                    participant,
                    native.model.catalog.requests().cloned().collect(),
                );
                for change in native.model.take_lookup_changes() {
                    if let Some(source_room) = self.source_room(connection, &change.address.room) {
                        self.source_lookup_index
                            .change(connection, source_room, change);
                    }
                }
                let deadline = native.model.deadline();
                if let Some(deadline) = deadline {
                    self.native_deadlines.insert((deadline, connection));
                }
                self.native_connections.insert(
                    connection,
                    NativeService {
                        handler: native,
                        deadline,
                    },
                );
                if wake {
                    self.wake_native_endpoint(connection);
                }
            }
            Err(error) => {
                if matches!(
                    error,
                    crate::control::native::InputError::Output(
                        crate::control::native::OutputError::ProgressExpired
                    )
                ) {
                    metrics::counter!("native_signaling_handoff_failure", "lane" => "output_deadline")
                        .increment(1);
                }
                tracing::warn!(?error, connection_id = %connection, "native controller connection failed");
                self.catalog.remove_connection(connection);
                self.source_lookup_index.remove_connection(connection);
                self.remove_incarnation(participant, connection);
            }
        }
    }

    fn wake_native_endpoint(&mut self, connection_id: ConnectionId) {
        if let Some((_, meta)) = self.core.registry.get_connection(connection_id) {
            self.command_backlog
                .push_back((meta.shard_id, ShardCommand::NativeReady { connection_id }));
        }
    }

    pub(super) fn retire_native_connection(
        &mut self,
        connection: ConnectionId,
    ) -> Option<NativeService> {
        let native = self.native_connections.remove(&connection)?;
        self.catalog.remove_connection(connection);
        self.source_lookup_index.remove_connection(connection);
        if let Some(deadline) = native.deadline {
            self.native_deadlines.remove(&(deadline, connection));
        }
        Some(native)
    }

    pub(super) fn begin_native_retirement(
        &mut self,
        participant: ParticipantId,
        meta: crate::control::registry::ParticipantMeta,
        mut handler: StateProtocolHandler,
        reason: CloseReason,
    ) -> bool {
        if self.retiring_native.len() >= MAX_TERMINAL_HANDOFFS
            || meta.transport.is_none()
            || handler.close(reason).is_err()
        {
            return false;
        }
        debug_assert_eq!(handler.participant, participant);
        let connection = meta.connection_id;
        let now = pulsebeam_runtime::time::Instant::now();
        let deadline = now.checked_add(TERMINAL_HANDOFF_TIMEOUT).unwrap_or(now);
        self.native_deadlines.insert((deadline, connection));
        self.service_native_retirement(
            connection,
            RetiringNative {
                handler,
                meta,
                deadline,
            },
        );
        true
    }

    fn service_native_retirement(
        &mut self,
        connection: ConnectionId,
        mut retiring: RetiringNative,
    ) {
        let now = pulsebeam_runtime::time::Instant::now();
        // Ownership and media plans have already been revoked. Only the old
        // session's bounded output handoff survives, never a retry/admission fence.
        let result = retiring.handler.service_terminal();
        // RTC admission is not a network flush. Keep the handoff alive for the
        // bounded grace period so dropping it cannot abort queued terminal bytes.
        let finished = result.is_err() || now >= retiring.deadline;
        if finished {
            self.native_deadlines
                .remove(&(retiring.deadline, connection));
            self.retire_participant_transport(retiring.handler.participant, &retiring.meta);
            return;
        }
        if matches!(result, Ok(true)) {
            // Wake terminal output by the retained location, even after registry removal.
            self.command_backlog.push_back((
                retiring.meta.shard_id,
                ShardCommand::NativeReady {
                    connection_id: connection,
                },
            ));
        }
        self.retiring_native.insert(connection, retiring);
    }

    pub(super) fn finish_native_observation(
        &mut self,
        connection: ConnectionId,
        result: Result<(), crate::control::native::OutputError>,
    ) {
        let Some(service) = self.native_connections.get_mut(&connection) else {
            return;
        };
        let native = &service.handler;
        if result.is_err() || !native.handoff.schedule() {
            tracing::warn!(?result, connection_id = %connection, "native controller observation failed");
            native.handoff.stop();
            let wake = native.handoff.wake_endpoint();
            if let Some(deadline) = service.deadline.take() {
                self.native_deadlines.remove(&(deadline, connection));
            }
            // A full readiness lane cannot carry its own failure notice.
            let deadline = pulsebeam_runtime::time::Instant::now();
            service.deadline = Some(deadline);
            self.native_deadlines.insert((deadline, connection));
            if wake {
                self.wake_native_endpoint(connection);
            }
        }
    }

    pub(super) fn observe_native_source(&mut self, meta: &crate::track::TrackMeta, appeared: bool) {
        let Some((participant, publisher)) = self.core.registry.get_connection(meta.connection_id)
        else {
            return;
        };
        if participant != meta.origin || publisher.scope.joined_room() != Some(meta.room) {
            return;
        }
        let room = meta.room;
        if let Some(track_id) = meta.track_id {
            self.source_lookup_index.source_changed(SourceLookupKey {
                room,
                participant,
                kind: meta.id.kind(),
                track_id,
            });
        }
        if !appeared {
            return;
        }
        let connections: Vec<_> = self
            .core
            .registry
            .ordered_participants_in_room(&room, None)
            .map(|(_, _, peer)| peer.connection_id)
            .collect();
        for connection in connections {
            let Some(service) = self.native_connections.get_mut(&connection) else {
                continue;
            };
            if service.handler.model.notify_waiting_source(meta.id) {
                self.finish_native_observation(connection, Ok(()));
            }
        }
    }

    pub(super) fn observe_native_authorization(
        &mut self,
        connection: ConnectionId,
        lease: &AuthorizationLease,
    ) {
        let Some(NativeService {
            handler: native, ..
        }) = self.native_connections.get_mut(&connection)
        else {
            return;
        };
        let result = match lease.native_expiry() {
            Ok(expiry) => {
                native
                    .model
                    .authorization(expiry, lease.revision, lease.permissions().clone())
            }
            Err(_) => Err(crate::control::native::OutputError::InvalidObservation),
        };
        self.finish_native_observation(connection, result.map(|_| ()));
    }
}

#[cfg(test)]
mod source_lookup_index_tests {
    use super::*;

    #[test]
    fn lookup_interest_wakes_coalesce_and_cleanup_with_owner_lifetimes() {
        let mut index = SourceLookupIndex::default();
        let first = ConnectionId::new();
        let second = ConnectionId::new();
        let address = pulsebeam_proto::state::LogicalSourceAddress {
            room: String::from("source-room"),
            participant_id: String::from("publisher"),
            kind: pulsebeam_proto::state::TrackKind::Data as i32,
            track_id: String::from("topic"),
        };
        let change = |reference, active| crate::control::native::LookupInterestChange {
            reference,
            address: address.clone(),
            active,
        };

        let room = ProjectRoomKey {
            project: crate::entity::ProjectId::new(),
            room: crate::entity::RoomId::new(&address.room).unwrap(),
        };
        let other_project_room = ProjectRoomKey {
            project: crate::entity::ProjectId::new(),
            ..room
        };
        let key = SourceLookupKey::from_address(room, &address).unwrap();
        index.change(first, room, change(1, true));
        index.change(second, room, change(2, true));
        index.source_changed(SourceLookupKey {
            room: other_project_room,
            ..key
        });
        index.source_changed(SourceLookupKey {
            kind: TrackKind::Video,
            ..key
        });
        assert!(index.wake_queue.is_empty());
        for _ in 0..2 {
            index.source_changed(key);
        }
        assert_eq!(index.wake_queue.len(), 1);
        assert!(index.wake_again.contains(index.wake_queue.front().unwrap()));
        assert_eq!(index.by_source.values().next().unwrap().len(), 2);

        index.change(first, room, change(1, false));
        assert_eq!(index.by_source.values().next().unwrap().len(), 1);
        index.remove_connection(second);
        assert!(index.by_source.is_empty());
        assert!(index.by_owner.is_empty());
    }
}
