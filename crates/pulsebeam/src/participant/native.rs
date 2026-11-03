use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::state::{self as wire, codec, validation};

use crate::media::ChannelId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Session,
    Owned(u64),
    Participant(String),
    Track(String),
}

impl Key {
    fn wire(&self) -> wire::ResourceKey {
        use wire::resource_key::Key as Value;
        wire::ResourceKey {
            key: Some(match self {
                Self::Session => Value::Session(true),
                Self::Owned(value) => Value::Owned(*value),
                Self::Participant(value) => Value::Participant(value.clone()),
                Self::Track(value) => Value::Track(value.clone()),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum InputError {
    #[error("invalid StateBatch frame: {0:?}")]
    Decode(codec::DecodeError),
    #[error("invalid connection revision order")]
    Revision,
    #[error("retained control work exhausted")]
    Capacity,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OutputError {
    #[error("unrepresentable StateBatch value: {0:?}")]
    Encode(codec::EncodeError),
    #[error("required source or execution facts are unavailable")]
    InvalidObservation,
    #[error("retained control work exhausted")]
    Capacity,
    #[error("control output made no required progress")]
    ProgressExpired,
    #[error("current grant has expired")]
    AuthorizationExpired,
}

pub(crate) struct NativeOutput {
    pub(crate) cid: ChannelId,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct Preparation {
    pub(crate) reference: u64,
    pub(crate) revision: u64,
    pub(crate) spec: wire::ResourceSpec,
    grant_revision: u64,
}

struct Owned {
    declaration: wire::ResourceSpec,
    target: wire::DesiredState,
    active: Option<(u64, u64)>,
    active_bytes: usize,
    queued: bool,
    retired: bool,
    cutoff_pending: bool,
    cutoff_reason: wire::AbsenceReason,
    execution_bytes: usize,
}

struct Row {
    value: wire::ConfirmedState,
    owned: Option<Owned>,
    ordinal: u64,
    version: u64,
    dirty: bool,
    ticket: Option<u64>,
    unavailable: bool,
    represented: bool,
}

struct Pending {
    output: NativeOutput,
    selected: Vec<(Key, u64)>,
    initial_cursor: u64,
    initial_complete: bool,
    deadline: tokio::time::Instant,
}

// Finite local retained-control work, not a stream-count or identity-history ceiling.
const STATE_BYTES: usize = 8 * 1024 * 1024;
const SELECT_WORK: usize = 32;

#[derive(Default)]
pub(crate) struct NativeSession {
    pub(crate) cid: Option<ChannelId>,
    rows: BTreeMap<Key, Row>,
    order: BTreeMap<u64, Key>,
    publications: BTreeMap<(i32, String), u64>,
    dirty: BTreeMap<u64, Key>,
    preparations: VecDeque<u64>,
    pending: Option<Pending>,
    last_revision: u64,
    version: u64,
    ordinal: u64,
    initial_end: Option<u64>,
    initial_cursor: u64,
    initial_complete: bool,
    grant_revision: u64,
    closed: bool,
    retained_bytes: usize,
    unavailable: usize,
    initial_deadline: Option<tokio::time::Instant>,
}

impl NativeSession {
    fn charge(&mut self, previous: usize, next: usize) -> Result<(), OutputError> {
        let bytes = self
            .retained_bytes
            .checked_sub(previous)
            .and_then(|bytes| bytes.checked_add(next))
            .ok_or(OutputError::Capacity)?;
        if bytes > STATE_BYTES {
            return Err(OutputError::Capacity);
        }
        self.retained_bytes = bytes;
        Ok(())
    }

    fn next_version(&mut self) -> Result<u64, OutputError> {
        self.version = self.version.checked_add(1).ok_or(OutputError::Capacity)?;
        Ok(self.version)
    }

    fn put(&mut self, key: Key, value: wire::ConfirmedState) -> Result<(), OutputError> {
        let record = wire::StateRecord {
            key: Some(key.wire()),
            state: Some(wire::state_record::State::Confirmed(value.clone())),
        };
        codec::encode(
            &wire::StateBatch {
                records: vec![record],
                initial_complete: false,
            },
            validation::Direction::Server,
        )
        .map_err(OutputError::Encode)?;
        if self.rows.get(&key).is_some_and(|row| row.value == value) {
            return Ok(());
        }
        let previous = self.rows.get(&key).map_or(0, |row| row.value.encoded_len());
        let overhead = if self.rows.contains_key(&key) {
            0
        } else {
            key_cost(&key)
        };
        self.charge(previous, value.encoded_len().saturating_add(overhead))?;
        let version = self.next_version()?;
        if let Some(row) = self.rows.get_mut(&key) {
            row.value = value;
            row.version = version;
            if row.unavailable {
                row.unavailable = false;
                self.unavailable = self.unavailable.saturating_sub(1);
            }
            row.dirty = true;
            if row.ticket.is_none() {
                row.ticket = Some(version);
                self.dirty.insert(version, key);
            }
            return Ok(());
        }
        self.ordinal = self.ordinal.checked_add(1).ok_or(OutputError::Capacity)?;
        self.order.insert(self.ordinal, key.clone());
        self.rows.insert(
            key.clone(),
            Row {
                value,
                owned: None,
                ordinal: self.ordinal,
                version,
                dirty: true,
                ticket: Some(version),
                unavailable: false,
                represented: false,
            },
        );
        self.dirty.insert(version, key);
        Ok(())
    }

    pub(crate) fn set_cid(&mut self, cid: ChannelId) {
        self.cid = Some(cid);
        self.initial_end.get_or_insert(self.ordinal);
        self.initial_deadline.get_or_insert_with(progress_deadline);
    }

    pub(crate) fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = super::effect::RoomPeer>,
        removed: impl IntoIterator<Item = crate::entity::ParticipantId>,
    ) -> Result<(), OutputError> {
        for peer in added {
            match peer.external_id {
                Some(external) => self.observe_participant(wire::Participant {
                    participant_id: peer.id.as_str(),
                    participant_external_id: external.as_str().to_owned(),
                })?,
                None => self.unavailable(Key::Participant(peer.id.as_str()))?,
            }
        }
        for id in removed {
            self.remove_participant(id.as_str())?;
        }
        Ok(())
    }

    fn unavailable(&mut self, key: Key) -> Result<(), OutputError> {
        self.put(key.clone(), absence(0, wire::AbsenceReason::Gone, None))?;
        if let Some(row) = self.rows.get_mut(&key)
            && !row.unavailable
        {
            row.unavailable = true;
            self.unavailable = self.unavailable.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) fn observe_candidate(
        &mut self,
        track: &crate::track::Track,
    ) -> Result<(), OutputError> {
        match track_descriptor(track) {
            Some(descriptor) => self.observe_track(descriptor),
            None => self.unavailable(Key::Track(track.id().as_str())),
        }
    }

    pub(crate) fn observe_participant(
        &mut self,
        participant: wire::Participant,
    ) -> Result<(), OutputError> {
        validation::validate_participant(&participant)
            .map_err(|_| OutputError::InvalidObservation)?;
        let key = Key::Participant(participant.participant_id.clone());
        if self.rows.get(&key).is_some_and(|row| matches!(&row.value.value, Some(wire::confirmed_state::Value::Participant(current)) if current != &participant)) {
            return Err(OutputError::InvalidObservation);
        }
        self.put(
            key,
            confirmed(wire::confirmed_state::Value::Participant(participant)),
        )
    }

    pub(crate) fn remove_participant(&mut self, id: String) -> Result<(), OutputError> {
        self.put(
            Key::Participant(id),
            absence(0, wire::AbsenceReason::Removed, None),
        )
    }

    pub(crate) fn observe_track(&mut self, track: wire::Track) -> Result<(), OutputError> {
        validation::validate_track(&track).map_err(|_| OutputError::InvalidObservation)?;
        let key = Key::Track(track.track_id.clone());
        if self.rows.get(&key).is_some_and(|row| matches!(&row.value.value, Some(wire::confirmed_state::Value::Track(current)) if current != &track)) {
            return Err(OutputError::InvalidObservation);
        }
        self.put(key, confirmed(wire::confirmed_state::Value::Track(track)))
    }

    pub(crate) fn source(&self, id: &str) -> Option<(wire::Participant, wire::Track)> {
        let row = self.rows.get(&Key::Track(id.to_owned()))?;
        let Some(wire::confirmed_state::Value::Track(track)) = &row.value.value else {
            return None;
        };
        let owner = self
            .rows
            .get(&Key::Participant(track.participant_id.clone()))?;
        let Some(wire::confirmed_state::Value::Participant(participant)) = &owner.value.value
        else {
            return None;
        };
        Some((participant.clone(), track.clone()))
    }

    pub(crate) fn observe_execution(
        &mut self,
        reference: u64,
        execution: wire::ExecutionStatus,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        if row.owned.as_ref().is_none_or(|owned| owned.retired) {
            return Ok(());
        }
        let mut value = row.value.clone();
        let Some(wire::confirmed_state::Value::Owned(live)) = &mut value.value else {
            return Ok(());
        };
        live.execution = execution as i32;
        live.execution_failure = None;
        self.put(key, value)
    }

    pub(crate) fn observe_activity(
        &mut self,
        reference: u64,
        active: bool,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        if row.owned.as_ref().is_none_or(|owned| owned.retired) {
            return Ok(());
        }
        let mut value = row.value.clone();
        let Some(wire::confirmed_state::Value::Owned(live)) = &mut value.value else {
            return Ok(());
        };
        if live.activity.is_none() {
            return Err(OutputError::InvalidObservation);
        }
        live.activity = Some(if active {
            wire::MediaActivity::Active
        } else {
            wire::MediaActivity::Idle
        } as i32);
        self.put(key, value)
    }

    pub(crate) fn remove_track(&mut self, id: String) -> Result<(), OutputError> {
        self.put(
            Key::Track(id),
            absence(0, wire::AbsenceReason::Removed, None),
        )
    }

    pub(crate) fn authorization(&mut self, expires: i64, revision: u64) -> Result<(), OutputError> {
        if self.closed || revision <= self.grant_revision {
            return Ok(());
        }
        self.grant_revision = revision;
        self.put(
            Key::Session,
            confirmed(wire::confirmed_state::Value::Session(wire::SessionState {
                authorization: Some(wire::Authorization {
                    expires_at_unix_seconds: expires,
                    revision,
                }),
                closed: None,
                diagnostic: String::new(),
            })),
        )
    }

    pub(crate) fn close(&mut self, reason: wire::CloseReason) -> Result<(), OutputError> {
        self.closed = true;
        let Some(row) = self.rows.get(&Key::Session) else {
            return Ok(());
        };
        let Some(wire::confirmed_state::Value::Session(mut session)) = row.value.value.clone()
        else {
            return Err(OutputError::InvalidObservation);
        };
        session.closed = Some(reason as i32);
        self.put(
            Key::Session,
            confirmed(wire::confirmed_state::Value::Session(session)),
        )
    }

    pub(crate) fn apply_input(&mut self, bytes: &[u8]) -> Result<Vec<u64>, InputError> {
        let batch =
            codec::decode(bytes, validation::Direction::Client).map_err(InputError::Decode)?;
        let mut cutoffs = Vec::new();
        for record in batch.records {
            let Some(wire::resource_key::Key::Owned(reference)) =
                record.key.and_then(|key| key.key)
            else {
                return Err(InputError::Revision);
            };
            let Some(wire::state_record::State::Desired(target)) = record.state else {
                return Err(InputError::Revision);
            };
            if self.closed || target.revision <= self.last_revision {
                return Err(InputError::Revision);
            }
            self.last_revision = target.revision;
            let key = Key::Owned(reference);
            if matches!(target.value, Some(wire::desired_state::Value::Absent(true))) {
                let previous = self
                    .rows
                    .get(&key)
                    .and_then(|row| row.owned.as_ref())
                    .map_or(0, |owned| owned.target.encoded_len());
                if previous != 0 {
                    self.charge(previous, target.encoded_len())
                        .map_err(|_| InputError::Capacity)?;
                }
                if let Some(row) = self.rows.get_mut(&key)
                    && let Some(owned) = &mut row.owned
                {
                    owned.target = target;
                    owned.retired = true;
                    owned.cutoff_pending = true;
                    owned.cutoff_reason = wire::AbsenceReason::Removed;
                    cutoffs.push(reference);
                } else {
                    self.put(
                        key,
                        absence(target.revision, wire::AbsenceReason::Gone, None),
                    )
                    .map_err(|_| InputError::Capacity)?;
                }
                continue;
            }
            let Some(wire::desired_state::Value::Present(spec)) = target.value.as_ref() else {
                return Err(InputError::Revision);
            };
            let existing = self.rows.get(&key).and_then(|row| row.owned.as_ref());
            if reference < target.revision && existing.is_none_or(|owned| owned.retired) {
                self.put(
                    key,
                    absence(
                        target.revision,
                        wire::AbsenceReason::Gone,
                        Some(failure(wire::FailureCode::ResourceGone)),
                    ),
                )
                .map_err(|_| InputError::Capacity)?;
                continue;
            }
            let invalid = validation::validate_spec(spec).is_err()
                || existing.is_some_and(|owned| {
                    !validation::declarations_equivalent(&owned.declaration, spec)
                });
            if invalid {
                self.reject(
                    reference,
                    target,
                    failure(wire::FailureCode::InvalidConfiguration),
                )?;
                continue;
            }
            if let Some(label) = publication_key(spec)
                && self
                    .publications
                    .get(&label)
                    .is_some_and(|owner| *owner != reference)
            {
                self.reject(
                    reference,
                    target,
                    failure(wire::FailureCode::InvalidConfiguration),
                )?;
                continue;
            }
            if !live_fits(spec) {
                self.reject(
                    reference,
                    target,
                    failure(wire::FailureCode::CapacityExceeded),
                )?;
                continue;
            }
            let previous = self
                .rows
                .get(&key)
                .and_then(|row| row.owned.as_ref())
                .map_or(0, |owned| owned.target.encoded_len());
            let next = target.encoded_len().saturating_add(if existing.is_none() {
                spec.encoded_len()
            } else {
                0
            });
            self.charge(previous, next)
                .map_err(|_| InputError::Capacity)?;
            if let Some(row) = self.rows.get_mut(&key)
                && let Some(owned) = &mut row.owned
            {
                owned.target = target;
            } else {
                // The pending creation exists before preparation and owns its immutable declaration.
                self.put(key.clone(), absence(0, wire::AbsenceReason::Gone, None))
                    .map_err(|_| InputError::Capacity)?;
                if let Some(label) = publication_key(spec) {
                    self.publications.insert(label, reference);
                }
                let declaration = spec.as_ref().clone();
                let row = self.rows.get_mut(&key).ok_or(InputError::Capacity)?;
                row.owned = Some(Owned {
                    declaration,
                    target,
                    active: None,
                    active_bytes: 0,
                    queued: false,
                    retired: false,
                    cutoff_pending: false,
                    cutoff_reason: wire::AbsenceReason::Removed,
                    execution_bytes: 0,
                });
                // Pending entries have no authoritative absence to send.
                row.dirty = false;
            }
            self.queue_preparation(reference);
        }
        Ok(cutoffs)
    }

    fn reject(
        &mut self,
        reference: u64,
        target: wire::DesiredState,
        rejection: wire::Failure,
    ) -> Result<(), InputError> {
        let key = Key::Owned(reference);
        let previous = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .map_or(0, |owned| owned.target.encoded_len());
        if previous != 0 {
            self.charge(previous, target.encoded_len())
                .map_err(|_| InputError::Capacity)?;
        }
        if let Some(row) = self.rows.get_mut(&key)
            && let Some(owned) = &mut row.owned
        {
            owned.target = target.clone();
            let mut value = row.value.clone();
            if !matches!(value.value, Some(wire::confirmed_state::Value::Owned(_))) {
                owned.retired = true;
                value.value = Some(wire::confirmed_state::Value::Absent(wire::AbsentState {
                    reason: wire::AbsenceReason::CreateRejected as i32,
                }));
            }
            value.evaluated_revision = target.revision;
            value.rejection = Some(rejection);
            self.put(key, value).map_err(|_| InputError::Capacity)?;
            self.release_label(reference);
            Ok(())
        } else {
            self.put(
                key,
                absence(
                    target.revision,
                    wire::AbsenceReason::CreateRejected,
                    Some(rejection),
                ),
            )
            .map_err(|_| InputError::Capacity)
        }
    }

    fn queue_preparation(&mut self, reference: u64) {
        if let Some(row) = self.rows.get_mut(&Key::Owned(reference))
            && let Some(owned) = &mut row.owned
            && !owned.retired
            && owned.active.is_none()
            && !owned.queued
            && row.value.evaluated_revision < owned.target.revision
        {
            owned.queued = true;
            self.preparations.push_back(reference);
        }
    }

    pub(crate) fn take_preparation(&mut self) -> Result<Option<Preparation>, OutputError> {
        if self.closed || self.grant_revision == 0 {
            return Ok(None);
        }
        for _ in 0..SELECT_WORK {
            let Some(reference) = self.preparations.pop_front() else {
                break;
            };
            let bytes = self
                .rows
                .get(&Key::Owned(reference))
                .and_then(|row| row.owned.as_ref())
                .map_or(0, |owned| owned.target.encoded_len());
            self.charge(0, bytes)?;
            let Some(row) = self.rows.get_mut(&Key::Owned(reference)) else {
                self.charge(bytes, 0)?;
                continue;
            };
            let Some(owned) = &mut row.owned else {
                self.charge(bytes, 0)?;
                continue;
            };
            owned.queued = false;
            if owned.retired
                || owned.active.is_some()
                || row.value.evaluated_revision >= owned.target.revision
            {
                self.charge(bytes, 0)?;
                continue;
            }
            let Some(wire::desired_state::Value::Present(spec)) = &owned.target.value else {
                self.charge(bytes, 0)?;
                continue;
            };
            owned.active = Some((owned.target.revision, self.grant_revision));
            owned.active_bytes = bytes;
            return Ok(Some(Preparation {
                reference,
                revision: owned.target.revision,
                spec: spec.as_ref().clone(),
                grant_revision: self.grant_revision,
            }));
        }
        Ok(None)
    }

    pub(crate) fn authorization_current(&self, unix_now: u64) -> bool {
        !self.closed
            && self
                .rows
                .get(&Key::Session)
                .and_then(|row| row.value.value.as_ref())
                .is_some_and(|value| {
                    let wire::confirmed_state::Value::Session(session) = value else {
                        return false;
                    };
                    session.authorization.as_ref().is_some_and(|grant| {
                        u64::try_from(grant.expires_at_unix_seconds)
                            .is_ok_and(|expiry| unix_now < expiry)
                    })
                })
    }

    pub(crate) fn defer_preparation(&mut self, work: Preparation) -> Result<(), OutputError> {
        let key = Key::Owned(work.reference);
        let Some(owned) = self.rows.get(&key).and_then(|row| row.owned.as_ref()) else {
            return Ok(());
        };
        if owned.active != Some((work.revision, work.grant_revision)) {
            return Ok(());
        }
        let bytes = owned.active_bytes;
        self.charge(bytes, 0)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.active = None;
            owned.active_bytes = 0;
        }
        self.queue_preparation(work.reference);
        self.collect(&key);
        Ok(())
    }

    pub(crate) fn preparation_current(
        &self,
        work: &Preparation,
        unix_now: u64,
    ) -> Result<bool, OutputError> {
        let Some(owned) = self
            .rows
            .get(&Key::Owned(work.reference))
            .and_then(|row| row.owned.as_ref())
        else {
            return Ok(false);
        };
        if self.closed
            || owned.retired
            || owned.active != Some((work.revision, work.grant_revision))
            || owned.target.revision != work.revision
            || self.grant_revision != work.grant_revision
        {
            return Ok(false);
        }
        let Some(wire::desired_state::Value::Present(target)) = owned.target.value.as_ref() else {
            return Ok(false);
        };
        if !validation::declarations_equivalent(target, &work.spec)
            || !matches!((&target.settings, &work.spec.settings), (Some(target), Some(work)) if validation::settings_equivalent(target, work))
        {
            return Ok(false);
        }
        if !self.authorization_current(unix_now) {
            return Err(OutputError::AuthorizationExpired);
        }
        Ok(true)
    }

    pub(crate) fn complete(
        &mut self,
        work: Preparation,
        result: Result<wire::LiveResource, wire::Failure>,
        unix_now: u64,
    ) -> Result<bool, OutputError> {
        let key = Key::Owned(work.reference);
        let Some(owned) = self.rows.get(&key).and_then(|row| row.owned.as_ref()) else {
            return Ok(false);
        };
        if owned.active != Some((work.revision, work.grant_revision)) {
            return Ok(false);
        }
        let active_bytes = owned.active_bytes;
        let current = self.preparation_current(&work, unix_now);
        if matches!(current, Ok(false) | Err(OutputError::AuthorizationExpired)) {
            self.defer_preparation(work)?;
            return Ok(false);
        }
        current?;
        self.charge(active_bytes, 0)?;
        let Some(row) = self.rows.get_mut(&key) else {
            return Ok(false);
        };
        let Some(owned) = &mut row.owned else {
            return Ok(false);
        };
        owned.active = None;
        owned.active_bytes = 0;
        let Some(row) = self.rows.get_mut(&key) else {
            return Ok(false);
        };
        let Some(owned) = &mut row.owned else {
            return Ok(false);
        };
        let value = match result {
            Ok(live) => {
                if live.spec.as_ref().is_none_or(|spec| {
                    !validation::declarations_equivalent(&work.spec, spec)
                        || !matches!((&work.spec.settings, &spec.settings), (Some(requested), Some(accepted)) if validation::settings_equivalent(requested, accepted))
                }) {
                    return Err(OutputError::InvalidObservation);
                }
                wire::ConfirmedState {
                    evaluated_revision: work.revision,
                    rejection: None,
                    value: Some(wire::confirmed_state::Value::Owned(Box::new(live))),
                }
            }
            Err(rejection) => {
                let mut value = row.value.clone();
                value.evaluated_revision = work.revision;
                value.rejection = Some(rejection);
                if !matches!(value.value, Some(wire::confirmed_state::Value::Owned(_))) {
                    owned.retired = true;
                    value.value = Some(wire::confirmed_state::Value::Absent(wire::AbsentState {
                        reason: wire::AbsenceReason::CreateRejected as i32,
                    }));
                }
                value
            }
        };
        self.put(key, value)?;
        self.release_label(work.reference);
        Ok(true)
    }

    pub(crate) fn owns_lifetime(&self, reference: u64) -> bool {
        !self.closed
            && self
                .rows
                .get(&Key::Owned(reference))
                .and_then(|row| row.owned.as_ref())
                .is_some_and(|owned| !owned.retired)
    }

    pub(crate) fn reserve_execution(
        &mut self,
        reference: u64,
        bytes: usize,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let previous = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .ok_or(OutputError::InvalidObservation)?
            .execution_bytes;
        self.charge(previous, bytes)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.execution_bytes = bytes;
        }
        self.collect(&key);
        Ok(())
    }

    pub(crate) fn source_ended(&mut self, reference: u64) -> Result<(), OutputError> {
        let Some(owned) = self
            .rows
            .get_mut(&Key::Owned(reference))
            .and_then(|row| row.owned.as_mut())
        else {
            return Err(OutputError::InvalidObservation);
        };
        if !owned.retired {
            owned.retired = true;
            owned.cutoff_pending = true;
            owned.cutoff_reason = wire::AbsenceReason::SourceEnded;
        }
        Ok(())
    }

    pub(crate) fn cutoff_complete(&mut self, reference: u64) -> Result<(), OutputError> {
        let Some(row) = self.rows.get_mut(&Key::Owned(reference)) else {
            return Ok(());
        };
        let Some(owned) = &mut row.owned else {
            return Ok(());
        };
        if !owned.retired {
            return Err(OutputError::InvalidObservation);
        }
        owned.cutoff_pending = false;
        let value = if owned.cutoff_reason != wire::AbsenceReason::Removed {
            absence(
                row.value.evaluated_revision,
                owned.cutoff_reason,
                row.value.rejection.clone(),
            )
        } else if row.value.evaluated_revision > owned.target.revision {
            row.value.clone()
        } else {
            absence(owned.target.revision, wire::AbsenceReason::Removed, None)
        };
        self.put(Key::Owned(reference), value)?;
        self.release_label(reference);
        Ok(())
    }

    fn release_label(&mut self, reference: u64) {
        let Some(owned) = self
            .rows
            .get(&Key::Owned(reference))
            .and_then(|row| row.owned.as_ref())
        else {
            return;
        };
        if owned.retired
            && let Some(label) = publication_key(&owned.declaration)
            && self.publications.get(&label) == Some(&reference)
        {
            self.publications.remove(&label);
        }
    }

    pub(crate) fn needs_poll(&self) -> bool {
        self.cid.is_some()
            && self.grant_revision != 0
            && (self.pending.is_some() || !self.dirty.is_empty() || !self.initial_complete)
    }

    pub(crate) fn enumeration_progress(&self) -> u64 {
        self.initial_cursor
    }

    pub(crate) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .as_ref()
            .map(|pending| pending.deadline)
            .into_iter()
            .chain(self.initial_deadline)
            .min()
    }

    pub(crate) fn poll(&mut self) -> Result<Option<&NativeOutput>, OutputError> {
        if self
            .deadline()
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(OutputError::ProgressExpired);
        }
        if self.pending.is_some() {
            return Ok(self.pending.as_ref().map(|pending| &pending.output));
        }
        let Some(cid) = self.cid else { return Ok(None) };
        if self.grant_revision == 0 {
            return Ok(None);
        }
        let session_dirty = self.rows.get(&Key::Session).is_some_and(|row| row.dirty);
        if self.unavailable != 0 && !session_dirty {
            return Err(OutputError::InvalidObservation);
        }
        let mut keys = Vec::new();
        let mut seen = BTreeSet::new();
        if self.rows.get(&Key::Session).is_some_and(|row| row.dirty) {
            keys.push(Key::Session);
            seen.insert(Key::Session);
        }
        // Reserve initial-key progress even under continuous live churn.
        let end = self.initial_end.unwrap_or(0);
        let mut cursor = self.initial_cursor;
        if !self.initial_complete && self.unavailable == 0 {
            for (ordinal, key) in self
                .order
                .range((
                    std::ops::Bound::Excluded(cursor),
                    std::ops::Bound::Included(end),
                ))
                .take(SELECT_WORK / 2)
            {
                cursor = *ordinal;
                if self.rows.get(key).is_some_and(|row| !row.represented)
                    && seen.insert(key.clone())
                {
                    keys.push(key.clone());
                }
            }
        }
        for _ in 0..if self.unavailable == 0 {
            SELECT_WORK / 2
        } else {
            0
        } {
            let Some((ticket, key)) = self.dirty.pop_first() else {
                break;
            };
            if let Some(row) = self.rows.get_mut(&key) {
                if row.ticket != Some(ticket) {
                    continue;
                }
                row.ticket = None;
                if row.dirty && seen.insert(key.clone()) {
                    keys.push(key);
                }
            }
        }
        let mut batch = wire::StateBatch::default();
        let mut selected = Vec::new();
        let mut selected_cursor = cursor;
        for key in keys {
            if batch.records.len() == codec::MAX_RECORDS {
                self.requeue(&key);
                continue;
            }
            let Some(row) = self.rows.get(&key) else {
                continue;
            };
            if row.owned.as_ref().is_some_and(|owned| {
                owned.cutoff_pending || !owned.retired && row.value.evaluated_revision == 0
            }) {
                self.requeue(&key);
                continue;
            }
            let record = wire::StateRecord {
                key: Some(key.wire()),
                state: Some(wire::state_record::State::Confirmed(row.value.clone())),
            };
            let candidate_len = batch
                .encoded_len()
                .saturating_add(record.encoded_len())
                .saturating_add(3);
            if candidate_len > codec::MAX_DECODED_SIZE {
                self.requeue(&key);
                continue;
            }
            if row.ordinal <= cursor && row.ordinal <= end {
                selected_cursor = selected_cursor.max(row.ordinal);
            }
            selected.push((key, row.version));
            batch.records.push(record);
        }
        // Do not pass an initial key that did not fit the selected batch.
        let remaining = self
            .order
            .range((
                std::ops::Bound::Excluded(self.initial_cursor),
                std::ops::Bound::Included(end),
            ))
            .take(SELECT_WORK)
            .find(|(_, key)| {
                self.rows.get(*key).is_some_and(|row| !row.represented)
                    && !selected.iter().any(|(selected, _)| selected == *key)
            });
        if let Some((first, _)) = remaining {
            selected_cursor = selected_cursor.min(first.saturating_sub(1));
        } else {
            selected_cursor = cursor;
        }
        if self
            .order
            .range((
                std::ops::Bound::Excluded(selected_cursor),
                std::ops::Bound::Included(end),
            ))
            .next()
            .is_none()
        {
            selected_cursor = end;
        }
        batch.initial_complete =
            self.unavailable == 0 && !self.initial_complete && selected_cursor == end;
        if batch.encoded_len() > codec::MAX_DECODED_SIZE {
            batch.initial_complete = false;
        }
        if batch.records.is_empty() && !batch.initial_complete {
            self.initial_cursor = selected_cursor;
            return Ok(None);
        }
        let bytes =
            codec::encode(&batch, validation::Direction::Server).map_err(OutputError::Encode)?;
        self.pending = Some(Pending {
            output: NativeOutput { cid, bytes },
            selected,
            initial_cursor: selected_cursor,
            initial_complete: batch.initial_complete,
            deadline: progress_deadline(),
        });
        Ok(self.pending.as_ref().map(|pending| &pending.output))
    }

    fn requeue(&mut self, key: &Key) {
        if let Some(row) = self.rows.get_mut(key)
            && row.ticket.is_none()
        {
            row.ticket = Some(row.version);
            self.dirty.insert(row.version, key.clone());
        }
    }

    pub(crate) fn commit_sent(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.initial_cursor = pending.initial_cursor;
        self.initial_complete |= pending.initial_complete;
        if self.initial_complete {
            self.initial_deadline = None;
        }
        let mut initial_progress = false;
        for (key, version) in pending.selected {
            if let Some(row) = self.rows.get_mut(&key) {
                initial_progress |=
                    !row.represented && self.initial_end.is_some_and(|end| row.ordinal <= end);
                row.represented = true;
                if row.version == version {
                    row.dirty = false;
                    if let Some(ticket) = row.ticket.take() {
                        self.dirty.remove(&ticket);
                    }
                }
            }
            self.collect(&key);
        }
        if initial_progress && !self.initial_complete {
            self.initial_deadline = Some(progress_deadline());
        }
    }

    fn collect(&mut self, key: &Key) {
        let remove = self.rows.get(key).is_some_and(|row| {
            !row.dirty
                && matches!(
                    row.value.value,
                    Some(wire::confirmed_state::Value::Absent(_))
                )
                && row.owned.as_ref().is_none_or(|owned| {
                    owned.retired
                        && !owned.cutoff_pending
                        && owned.active.is_none()
                        && owned.execution_bytes == 0
                })
        });
        if remove && let Some(row) = self.rows.remove(key) {
            self.order.remove(&row.ordinal);
            if let Some(ticket) = row.ticket {
                self.dirty.remove(&ticket);
            }
            let owned_bytes = row.owned.as_ref().map_or(0, |owned| {
                owned
                    .declaration
                    .encoded_len()
                    .saturating_add(owned.target.encoded_len())
                    .saturating_add(owned.active_bytes)
                    .saturating_add(owned.execution_bytes)
            });
            let bytes = key_cost(key)
                .saturating_add(row.value.encoded_len())
                .saturating_add(owned_bytes);
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
        }
    }
}

fn progress_deadline() -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(std::time::Duration::from_secs(30))
        .unwrap_or(now)
}

fn key_cost(key: &Key) -> usize {
    let text = match key {
        Key::Participant(id) | Key::Track(id) => id.len(),
        _ => 0,
    };
    // Track observations fund the indexed native source runtime and its bounded work queue.
    let source = if matches!(key, Key::Track(_)) {
        4096
    } else {
        0
    };
    std::mem::size_of::<Row>()
        .saturating_add(
            std::mem::size_of::<Key>()
                .saturating_add(text)
                .saturating_mul(4),
        )
        .saturating_add(source)
}

fn live_fits(spec: &wire::ResourceSpec) -> bool {
    // Sizing only: never published or used as transport/source facts. Reserve both failure fields.
    let identity = "x".repeat(128);
    let diagnostic = "x".repeat(256);
    let participant = wire::Participant {
        participant_id: identity.clone(),
        participant_external_id: diagnostic.clone(),
    };
    let format = wire::CodecFormat {
        codec: Some(wire::codec_format::Codec::Opus(wire::OpusParameters {
            stereo: Some(true),
            source_stereo: Some(true),
            in_band_fec: Some(true),
            dtx: Some(true),
            constant_bitrate: Some(true),
            max_average_bitrate_bps: Some(510000),
            max_playback_rate_hz: Some(48000),
            max_capture_rate_hz: Some(48000),
            min_packet_time_ms: Some(120),
        })),
    };
    let track = wire::Track {
        track_id: identity.clone(),
        participant_id: identity,
        label: diagnostic.clone(),
        kind: Some(wire::track::Kind::Audio(wire::AudioTrack {
            format: Some(format),
        })),
    };
    let failure = wire::Failure {
        code: wire::FailureCode::InternalFailure as i32,
        diagnostic,
    };
    let record = wire::StateRecord {
        key: Some(Key::Owned(u64::MAX).wire()),
        state: Some(wire::state_record::State::Confirmed(wire::ConfirmedState {
            evaluated_revision: u64::MAX,
            rejection: Some(failure.clone()),
            value: Some(wire::confirmed_state::Value::Owned(Box::new(
                wire::LiveResource {
                    spec: Some(spec.clone()),
                    participant: Some(participant),
                    track: Some(track),
                    execution: wire::ExecutionStatus::Failed as i32,
                    execution_failure: Some(failure),
                    activity: Some(wire::MediaActivity::Active as i32),
                },
            ))),
        })),
    };
    wire::StateBatch {
        records: vec![record],
        initial_complete: true,
    }
    .encoded_len()
        <= codec::MAX_DECODED_SIZE
}

fn confirmed(value: wire::confirmed_state::Value) -> wire::ConfirmedState {
    wire::ConfirmedState {
        evaluated_revision: 0,
        rejection: None,
        value: Some(value),
    }
}

fn absence(
    revision: u64,
    reason: wire::AbsenceReason,
    rejection: Option<wire::Failure>,
) -> wire::ConfirmedState {
    wire::ConfirmedState {
        evaluated_revision: revision,
        rejection,
        value: Some(wire::confirmed_state::Value::Absent(wire::AbsentState {
            reason: reason as i32,
        })),
    }
}

pub(crate) fn track_descriptor(track: &crate::track::Track) -> Option<wire::Track> {
    use crate::track::{DataLane, Track};
    let kind = match track {
        Track::Audio(audio) => wire::track::Kind::Audio(wire::AudioTrack {
            format: Some(audio.format?),
        }),
        Track::Video(video) => wire::track::Kind::Video(wire::VideoTrack {
            format: Some(video.format?),
        }),
        Track::Data(data) => wire::track::Kind::Data(wire::DataTrack {
            delivery: match data.lane {
                DataLane::Latest => wire::DataDelivery::Latest,
                DataLane::Lossy => wire::DataDelivery::Lossy,
                DataLane::Ordered => wire::DataDelivery::Ordered,
            } as i32,
        }),
    };
    Some(wire::Track {
        track_id: track.id().as_str(),
        participant_id: track.meta().origin.as_str(),
        label: track.meta().label.clone()?,
        kind: Some(kind),
    })
}

pub(crate) fn failure(code: wire::FailureCode) -> wire::Failure {
    wire::Failure {
        code: code as i32,
        diagnostic: String::new(),
    }
}

fn publication_key(spec: &wire::ResourceSpec) -> Option<(i32, String)> {
    let wire::resource_spec::Declaration::Publication(publication) = spec.declaration.as_ref()?
    else {
        return None;
    };
    let kind = match publication.kind.as_ref()? {
        wire::publication_spec::Kind::Audio(_) => wire::TrackKind::Audio,
        wire::publication_spec::Kind::Video(_) => wire::TrackKind::Video,
        wire::publication_spec::Kind::Data(_) => wire::TrackKind::Data,
    };
    Some((kind as i32, publication.label.clone()))
}

#[cfg(test)]
mod tests;
