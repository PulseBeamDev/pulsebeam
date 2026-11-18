use super::*;

const NATIVE_BUDGET: usize = 32;
const MAX_TERMINAL_HANDOFFS: usize = 256;
const TERMINAL_HANDOFF_TIMEOUT: Duration = Duration::from_millis(100);
const LOOKUP_WAKE_BUDGET: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SourceLookupKey {
    room: RoomId,
    participant: String,
    label: String,
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
    pub(super) fn change(
        &mut self,
        connection: ConnectionId,
        source_room: RoomId,
        change: crate::control::native::LookupInterestChange,
    ) {
        let owner = (connection, change.reference);
        let key = SourceLookupKey {
            room: source_room,
            participant: change.address.participant_external_id,
            label: change.address.label,
        };
        if let Some(previous) = self.by_owner.remove(&owner) {
            if let Some(owners) = self.by_source.get_mut(&previous) {
                owners.remove(&owner);
                if owners.is_empty() {
                    self.by_source.remove(&previous);
                }
            }
        }
        if change.active {
            self.by_source.entry(key.clone()).or_default().insert(owner);
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

    pub(super) fn source_changed(&mut self, room: RoomId, participant: String, label: String) {
        self.changed(SourceLookupKey {
            room,
            participant,
            label,
        });
    }

    fn changed(&mut self, key: SourceLookupKey) {
        if !self.by_source.contains_key(&key) {
            return;
        }
        if self.wake_queued.insert(key.clone()) {
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
    fn source_room_id(
        &self,
        subscriber: ParticipantId,
        room: &str,
    ) -> Option<crate::entity::RoomId> {
        let external = crate::entity::RoomExternalId::new(room).ok()?;
        let meta = self.core.registry.get_participant(&subscriber)?;
        if let Some(lease) = &meta.authorization {
            return Some(crate::entity::RoomId::derive(
                &lease.authority.project_id,
                &external,
            ));
        }

        // Synthetic controller fixtures can register participants without an admission lease.
        #[cfg(test)]
        let fixture_room = Some(crate::entity::RoomId::from_external(&external));
        #[cfg(not(test))]
        let fixture_room = None;
        fixture_room
    }

    pub(super) fn native_resource_lease(
        &self,
        connection: ConnectionId,
    ) -> Option<&AuthorizationLease> {
        if let Some(native) = self.native_connections.get(&connection)
            && let Some(meta) = self
                .core
                .registry
                .get_participant(&native.handler.participant)
            && meta.connection_id == connection
            && meta.profile == SignalingProtocol::State
            && let Some(lease) = &meta.authorization
            && let Some(external) = &meta.participant_external_id
            && lease.matches_participant(meta.room_id, native.handler.participant, external)
        {
            return Some(lease);
        }
        // Terminal metadata can authorize exact cleanup, never prove quiescence.
        if let Some(retiring) = self.retiring_native.get(&connection)
            && retiring.meta.connection_id == connection
            && retiring.meta.profile == SignalingProtocol::State
            && let Some(lease) = &retiring.meta.authorization
            && let Some(external) = &retiring.meta.participant_external_id
            && lease.matches_participant(
                retiring.meta.room_id,
                retiring.handler.participant,
                external,
            )
        {
            return Some(lease);
        }
        self.materializations.iter().find_map(|work| {
            let pending = &work.pending;
            let lease = pending.authorization.as_ref()?;
            (pending.connection_id == connection
                && pending.profile == SignalingProtocol::State
                && lease.matches_participant(
                    pending.room_id,
                    pending.participant,
                    &pending.participant_external_id,
                ))
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
            self.source_lookup_index
                .wake_cursor
                .insert(key.clone(), owner);
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
        if !self.current_incarnation(participant, connection) {
            self.catalog.remove_connection(connection);
            self.source_lookup_index.remove_connection(connection);
            return;
        }
        let room = native.room;
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match native.service(
            unix_now,
            |track| {
                let track =
                    self.lifecycle
                        .source(room, room, participant, track, &self.core.registry)?;
                let publisher = self.core.registry.get_participant(&track.meta().origin)?;
                Some(crate::control::native::ResolvedSource {
                    track,
                    publisher: publisher.participant_external_id.as_ref()?,
                })
            },
            |source_room, track| {
                let source_room = self.source_room_id(participant, source_room)?;
                let track = self.lifecycle.source(
                    room,
                    source_room,
                    participant,
                    track,
                    &self.core.registry,
                )?;
                let publisher = self.core.registry.get_participant(&track.meta().origin)?;
                Some(crate::control::native::ResolvedSource {
                    track,
                    publisher: publisher.participant_external_id.as_ref()?,
                })
            },
            |address| {
                let source_room = self.source_room_id(participant, &address.room)?;
                let (track, publisher) = self.lifecycle.source_by_address(
                    room,
                    source_room,
                    participant,
                    address,
                    &self.core.registry,
                )?;
                Some(crate::control::native::ResolvedSource { track, publisher })
            },
        ) {
            Ok(wake) => {
                self.catalog.sync(
                    connection,
                    participant,
                    native.model.catalog.requests().cloned().collect(),
                );
                for change in native.model.take_lookup_changes() {
                    if let Some(source_room) =
                        self.source_room_id(participant, &change.address.room)
                    {
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
                    self.wake_native_endpoint(participant, connection);
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
                tracing::warn!(?error, participant_id = %participant, %connection, "native controller connection failed");
                self.catalog.remove_connection(connection);
                self.source_lookup_index.remove_connection(connection);
                self.remove_incarnation(participant, connection);
            }
        }
    }

    fn wake_native_endpoint(&mut self, participant: ParticipantId, connection_id: ConnectionId) {
        if let Some(meta) = self.core.registry.get_participant(&participant)
            && meta.connection_id == connection_id
        {
            self.command_backlog.push_back((
                meta.shard_id,
                ShardCommand::NativeReady {
                    participant,
                    connection_id,
                },
            ));
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
                    participant: retiring.handler.participant,
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
            tracing::warn!(?result, participant_id = %native.participant, "native controller observation failed");
            native.handoff.stop();
            let participant = native.participant;
            let wake = native.handoff.wake_endpoint();
            if let Some(deadline) = service.deadline.take() {
                self.native_deadlines.remove(&(deadline, connection));
            }
            // A full readiness lane cannot carry its own failure notice.
            let deadline = pulsebeam_runtime::time::Instant::now();
            service.deadline = Some(deadline);
            self.native_deadlines.insert((deadline, connection));
            if wake {
                self.wake_native_endpoint(participant, connection);
            }
        }
    }

    pub(super) fn observe_native_source(&mut self, meta: &crate::track::TrackMeta, appeared: bool) {
        if let Some(label) = meta.label.as_ref()
            && let Some(publisher) = self.core.registry.get_participant(&meta.origin)
            && publisher.connection_id == meta.connection_id
            && let Some(external) = publisher.participant_external_id.as_ref()
        {
            self.source_lookup_index.source_changed(
                meta.room_id,
                external.as_str().to_owned(),
                label.clone(),
            );
        }
        if !appeared {
            return;
        }
        for participant in self
            .core
            .registry
            .local_participant_ids_in_room(&meta.room_id)
        {
            let Some(connection) = self
                .core
                .registry
                .get_participant(&participant)
                .map(|peer| peer.connection_id)
            else {
                continue;
            };
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
        participant: ParticipantId,
        lease: &AuthorizationLease,
    ) {
        let Some(meta) = self.core.registry.get_participant(&participant) else {
            return;
        };
        let connection = meta.connection_id;
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
            participant_external_id: String::from("publisher"),
            kind: pulsebeam_proto::state::TrackKind::Data as i32,
            label: String::from("topic"),
        };
        let change = |reference, active| crate::control::native::LookupInterestChange {
            reference,
            address: address.clone(),
            active,
        };

        let room = RoomId::derive(
            &crate::entity::ProjectId::new(),
            &crate::entity::RoomExternalId::new(&address.room).unwrap(),
        );
        let other_project_room = RoomId::derive(
            &crate::entity::ProjectId::new(),
            &crate::entity::RoomExternalId::new(&address.room).unwrap(),
        );
        index.change(first, room, change(1, true));
        index.change(second, room, change(2, true));
        index.source_changed(
            other_project_room,
            address.participant_external_id.clone(),
            address.label.clone(),
        );
        assert!(index.wake_queue.is_empty());
        for _ in 0..2 {
            index.source_changed(
                room,
                address.participant_external_id.clone(),
                address.label.clone(),
            );
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
