use super::*;
use crate::participant::ParticipantEffect;

const NATIVE_BUDGET: usize = 32;

impl ControllerActor {
    pub(super) fn service_native_connections(&mut self) {
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

    fn service_native_connection(&mut self, connection: ConnectionId) {
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
            return;
        }
        let room = native.room;
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match native.service(unix_now, |track| {
            self.lifecycle
                .source(room, participant, track, &self.core.registry)
        }) {
            Ok(wake) => {
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

    pub(super) fn retire_native_connection(&mut self, connection: ConnectionId) {
        if let Some(native) = self.native_connections.remove(&connection)
            && let Some(deadline) = native.deadline
        {
            self.native_deadlines.remove(&(deadline, connection));
        }
    }

    pub(super) fn observe_native_effect(
        &mut self,
        participant: ParticipantId,
        effect: ParticipantEffect,
    ) -> Option<ParticipantEffect> {
        let Some(connection) = self
            .core
            .registry
            .get_participant(&participant)
            .map(|meta| meta.connection_id)
        else {
            return Some(effect);
        };
        let Some(NativeService {
            handler: native, ..
        }) = self.native_connections.get_mut(&connection)
        else {
            return Some(effect);
        };
        let (result, effect) = match effect {
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                (native.model.apply_participants(added, removed), None)
            }
            effect => return Some(effect),
        };
        self.finish_native_observation(connection, result);
        effect
    }

    fn finish_native_observation(
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

    pub(super) fn seed_native_sources(&mut self, connection: ConnectionId) {
        let Some(NativeService {
            handler: native, ..
        }) = self.native_connections.get_mut(&connection)
        else {
            return;
        };
        let mut result = Ok(());
        for track in self.lifecycle.sources_in_room(native.room) {
            if self
                .lifecycle
                .source(
                    native.room,
                    native.participant,
                    track.id(),
                    &self.core.registry,
                )
                .is_none()
            {
                continue;
            }
            result = native.model.observe_candidate(track);
            if result.is_err() {
                break;
            }
        }
        self.finish_native_observation(connection, result);
    }

    pub(super) fn observe_native_source(
        &mut self,
        meta: &crate::track::TrackMeta,
        track: Option<&crate::track::Track>,
    ) {
        for participant in self
            .core
            .registry
            .local_participant_ids_in_room(&meta.room_id)
        {
            if participant == meta.origin {
                continue;
            }
            if track.is_some()
                && self
                    .lifecycle
                    .source(meta.room_id, participant, meta.id, &self.core.registry)
                    .is_none()
            {
                continue;
            }
            let Some(connection) = self
                .core
                .registry
                .get_participant(&participant)
                .map(|peer| peer.connection_id)
            else {
                continue;
            };
            let Some(NativeService {
                handler: native, ..
            }) = self.native_connections.get_mut(&connection)
            else {
                continue;
            };
            let result = match track {
                Some(track) => native.model.observe_candidate(track),
                None => native.model.remove_track(meta.id.as_str()),
            };
            self.finish_native_observation(connection, result);
        }
    }

    pub(super) fn observe_native_authorization(
        &mut self,
        participant: ParticipantId,
        lease: AuthorizationLease,
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
            Ok(expiry) => native.model.authorization(expiry, lease.revision),
            Err(_) => Err(crate::control::native::OutputError::InvalidObservation),
        };
        self.finish_native_observation(connection, result);
    }
}
