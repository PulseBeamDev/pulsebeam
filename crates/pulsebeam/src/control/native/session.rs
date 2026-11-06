use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::state::{self as wire, codec, validation};

use crate::entity::TrackId;
use crate::media::ChannelId;
use crate::participant::signaling::{SignalingOutput, SignalingProgress};
use crate::track::Track;

use super::{ExecutionDisposition, ResourceExecution};

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
    #[error("invalid ClientStateBatch message: {0:?}")]
    Decode(codec::DecodeError),
    #[error("invalid connection revision order")]
    Revision,
    #[error("retained control work exhausted")]
    Capacity,
    #[error("native execution command failed: {0}")]
    Output(#[from] OutputError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OutputError {
    #[error("unrepresentable ServerStateBatch value: {0:?}")]
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

#[derive(Default)]
pub(crate) struct NativeInput {
    pub(crate) cutoffs: Vec<u64>,
    pub(crate) executions: Vec<(u64, wire::ExecutionReport)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Preparation {
    pub(crate) reference: u64,
    pub(crate) revision: u64,
    grant_revision: u64,
    sequence: u64,
    started_at: pulsebeam_runtime::time::Instant,
}

impl Preparation {
    pub(crate) fn scope(self) -> crate::participant::policy::ExecutionScope {
        crate::participant::policy::ExecutionScope {
            binding: self.reference,
            sequence: self.sequence,
            authorization: self.grant_revision,
        }
    }

    fn stamp(self) -> (u64, u64, u64) {
        (self.revision, self.grant_revision, self.sequence)
    }

    fn bytes() -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(crate::participant::Connection::native_execution_metadata_bytes())
    }
}

pub(crate) struct ExecutionCommand(ExecutionAction);

pub(crate) enum ExecutionAction {
    Prepare {
        work: Preparation,
        resource: crate::control::native::PreparedResource,
    },
    Retire(crate::participant::policy::ExecutionScope),
    FailReceiver(crate::participant::policy::ExecutionScope),
}

impl ExecutionCommand {
    pub(crate) fn into_action(self) -> ExecutionAction {
        self.0
    }
}

struct ProjectedExecution {
    work: Preparation,
    live: wire::LiveResource,
}

struct ReceiverSource {
    participant: wire::Participant,
    track: wire::Track,
}

impl ReceiverSource {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.participant.encoded_len())
            .saturating_add(self.track.encoded_len())
    }
}

struct Owned {
    receiver_source: Option<Box<ReceiverSource>>,
    receiver_binding: Option<crate::participant::policy::ReceiverBinding>,
    receiver_failure: Option<Box<wire::Failure>>,
    declaration: wire::ResourceSpec,
    target: wire::DesiredState,
    active: Option<(u64, u64, u64)>,
    queued: bool,
    retired: bool,
    cutoff_pending: bool,
    cutoff_reason: wire::AbsenceReason,
    execution_bytes: usize,
    // Paid command ownership, not an installed or applied revision.
    storage_sequence: u64,
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

mod source_wait;
use source_wait::SourceWait;

struct Pending {
    output: Option<SignalingOutput>,
    sequence: u64,
    cid: ChannelId,
    selected: Vec<(Key, u64)>,
    initial_cursor: u64,
    initial_complete: bool,
    selected_at: pulsebeam_runtime::time::Instant,
    deadline: pulsebeam_runtime::time::Instant,
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
    source_waits: BTreeMap<u64, SourceWait>,
    source_deadlines: BTreeSet<(pulsebeam_runtime::time::Instant, u64)>,
    source_scan: bool,
    source_rescan: bool,
    source_cursor: Option<u64>,
    pending: Option<Pending>,
    output_sequence: u64,
    execution_sequence: u64,
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
    initial_deadline: Option<pulsebeam_runtime::time::Instant>,
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
        let record = wire::ServerStateRecord {
            key: Some(key.wire()),
            confirmed: Some(value.clone()),
        };
        codec::encode_server(&wire::ServerStateBatch {
            records: vec![record],
            initial_complete: false,
        })
        .map_err(OutputError::Encode)?;
        if self.rows.get(&key).is_some_and(|row| row.value == value) {
            return Ok(());
        }
        if matches!(&key, Key::Track(_) | Key::Participant(_) | Key::Session) {
            self.source_wait_changed();
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

    pub(crate) fn set_cid(&mut self, cid: ChannelId, at: pulsebeam_runtime::time::Instant) {
        self.cid = Some(cid);
        self.initial_end.get_or_insert(self.ordinal);
        self.initial_deadline
            .get_or_insert_with(|| progress_deadline(at));
    }

    pub(crate) fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = crate::participant::effect::RoomPeer>,
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
        self.observe_execution_result(reference, execution, None)
    }

    pub(crate) fn observe_execution_result(
        &mut self,
        reference: u64,
        execution: wire::ExecutionStatus,
        failure: Option<wire::Failure>,
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
        live.execution_failure = failure;
        self.put(key, value)
    }

    fn observe_receiver_failure(
        &mut self,
        reference: u64,
        error: Option<wire::Failure>,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        if row.owned.as_ref().is_none_or(|owned| owned.retired)
            || self.receiver_failure(reference).is_some()
        {
            return Ok(());
        }
        let error = error.unwrap_or_else(|| failure(wire::FailureCode::InvalidConfiguration));
        if matches!(
            row.value.value,
            Some(wire::confirmed_state::Value::Owned(_))
        ) {
            return self.observe_execution_result(
                reference,
                wire::ExecutionStatus::Failed,
                Some(error),
            );
        }
        self.charge(0, receiver_failure_bytes(&error))?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.receiver_failure = Some(Box::new(error));
        }
        Ok(())
    }

    fn receiver_failure(&self, reference: u64) -> Option<&wire::Failure> {
        let row = self.rows.get(&Key::Owned(reference))?;
        row.owned
            .as_ref()?
            .receiver_failure
            .as_deref()
            .or_else(|| match &row.value.value {
                Some(wire::confirmed_state::Value::Owned(live)) => live.execution_failure.as_ref(),
                _ => None,
            })
    }

    fn receiver_result(
        &self,
        reference: u64,
        execution: &crate::participant::policy::ReceiverExecution,
    ) -> Result<(wire::ExecutionStatus, Option<wire::Failure>), OutputError> {
        let error = if execution.failed {
            Some(
                self.receiver_failure(reference)
                    .ok_or(OutputError::InvalidObservation)?
                    .clone(),
            )
        } else {
            None
        };
        let status = if execution.failed {
            wire::ExecutionStatus::Failed
        } else if !execution.enabled {
            wire::ExecutionStatus::Disabled
        } else if execution.ready {
            wire::ExecutionStatus::Installed
        } else {
            wire::ExecutionStatus::WaitingForSource
        };
        Ok((status, error))
    }

    fn observe_receiver_execution(
        &mut self,
        reference: u64,
        execution: &crate::participant::policy::ReceiverExecution,
    ) -> Result<(), OutputError> {
        if self
            .rows
            .get(&Key::Owned(reference))
            .and_then(|row| row.owned.as_ref())
            .is_none_or(|owned| owned.retired)
        {
            return Ok(());
        }
        let (status, error) = self.receiver_result(reference, execution)?;
        self.observe_execution_result(reference, status, error)
    }

    pub(crate) fn observe_resource(
        &mut self,
        observation: crate::participant::policy::ResourceObservation,
    ) -> Result<Option<ExecutionCommand>, OutputError> {
        use crate::participant::policy::ResourceObservation;
        match observation {
            ResourceObservation::Activity { binding, active } => {
                self.observe_activity(binding, active)?;
            }
            ResourceObservation::Receiver { binding, execution } => {
                self.observe_receiver_execution(binding, &execution)?;
            }
            ResourceObservation::ReceiverBinding { binding, state } => {
                if let Some(owned) = self
                    .rows
                    .get_mut(&Key::Owned(binding))
                    .and_then(|row| row.owned.as_mut())
                {
                    owned.receiver_binding = if owned.retired { None } else { state };
                }
            }
            ResourceObservation::Ended { binding } => {
                self.source_ended(binding)?;
                return self.retirement_command(binding).map(Some);
            }
            ResourceObservation::Released { binding } => {
                self.reserve_execution(binding, 0)?;
                self.cutoff_complete(binding)?;
            }
            ResourceObservation::StorageReleased { binding, sequence } => {
                if sequence != 0
                    && self
                        .rows
                        .get(&Key::Owned(binding))
                        .and_then(|row| row.owned.as_ref())
                        .is_some_and(|owned| owned.storage_sequence == sequence)
                {
                    self.reserve_execution(binding, 0)?;
                }
            }
        }
        Ok(None)
    }

    fn observe_activity(&mut self, reference: u64, active: bool) -> Result<(), OutputError> {
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

    pub(crate) fn receive_input(
        &mut self,
        bytes: &[u8],
        unix_now: u64,
    ) -> Result<Vec<ExecutionCommand>, InputError> {
        let input = self.apply_input(bytes)?;
        let mut commands =
            Vec::with_capacity(input.cutoffs.len().saturating_add(input.executions.len()));
        for reference in input.cutoffs {
            commands.push(self.retirement_command(reference)?);
        }
        for (reference, report) in input.executions {
            if !self.authorization_current(unix_now) || self.receiver_failure(reference).is_some() {
                continue;
            }
            let eligible = self.rows.get(&Key::Owned(reference)).is_some_and(|row| {
                let Some(owned) = &row.owned else {
                    return false;
                };
                let Some(wire::resource_spec::Declaration::Subscription(subscription)) =
                    &owned.declaration.declaration
                else {
                    return false;
                };
                !owned.retired
                    && matches!(
                        row.value.value,
                        Some(wire::confirmed_state::Value::Owned(_))
                    )
                    && matches!(
                        subscription
                            .binding
                            .as_ref()
                            .and_then(|binding| binding.transport.as_ref()),
                        Some(wire::track_binding::Transport::Rtp(_))
                    )
            });
            if !eligible {
                continue;
            }
            self.observe_receiver_failure(reference, report.failure)?;
            commands.push(ExecutionCommand(ExecutionAction::FailReceiver(
                self.execution_scope(reference)?,
            )));
        }
        Ok(commands)
    }

    pub(crate) fn apply_input(&mut self, bytes: &[u8]) -> Result<NativeInput, InputError> {
        self.source_wait_changed();
        let batch = codec::decode_client(bytes).map_err(InputError::Decode)?;
        let mut effects = NativeInput::default();
        for record in batch.records {
            let reference = record.owned;
            let revision = record
                .desired
                .as_ref()
                .map(|desired| desired.revision)
                .or_else(|| record.execution.as_ref().map(|report| report.revision))
                .ok_or(InputError::Revision)?;
            if self.closed || revision <= self.last_revision {
                return Err(InputError::Revision);
            }
            self.last_revision = revision;
            if let Some(report) = record.execution {
                effects.executions.push((reference, report));
                continue;
            }
            let target = record.desired.ok_or(InputError::Revision)?;
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
                    effects.cutoffs.push(reference);
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
            if self.rows.get(&key).is_some_and(|row| {
                row.owned.as_ref().is_some_and(|owned| {
                    !owned.retired && row.value.evaluated_revision < owned.target.revision
                })
            }) {
                return Err(InputError::Revision);
            }
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
                    receiver_source: None,
                    receiver_binding: None,
                    receiver_failure: None,
                    declaration,
                    target,
                    active: None,
                    queued: false,
                    retired: false,
                    cutoff_pending: false,
                    cutoff_reason: wire::AbsenceReason::Removed,
                    execution_bytes: 0,
                    storage_sequence: 0,
                });
                // Pending entries have no authoritative absence to send.
                row.dirty = false;
            }
            self.queue_preparation(reference);
        }
        Ok(effects)
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

    fn next_execution_sequence(&self) -> Result<u64, OutputError> {
        self.execution_sequence
            .checked_add(1)
            .ok_or(OutputError::Capacity)
    }

    fn execution_scope(
        &mut self,
        reference: u64,
    ) -> Result<crate::participant::policy::ExecutionScope, OutputError> {
        let sequence = self.next_execution_sequence()?;
        self.execution_sequence = sequence;
        Ok(crate::participant::policy::ExecutionScope {
            binding: reference,
            sequence,
            authorization: self.grant_revision,
        })
    }

    pub(crate) fn retirement_command(
        &mut self,
        reference: u64,
    ) -> Result<ExecutionCommand, OutputError> {
        Ok(ExecutionCommand(ExecutionAction::Retire(
            self.execution_scope(reference)?,
        )))
    }

    pub(crate) fn preparation_pending(&self, unix_now: u64) -> bool {
        self.source_scan || (!self.preparations.is_empty() && self.authorization_current(unix_now))
    }

    pub(crate) fn take_preparation(&mut self) -> Result<Option<Preparation>, OutputError> {
        if self.closed || self.grant_revision == 0 {
            return Ok(None);
        }
        for _ in 0..SELECT_WORK {
            let Some(reference) = self.preparations.pop_front() else {
                break;
            };
            let key = Key::Owned(reference);
            let Some(row) = self.rows.get_mut(&key) else {
                continue;
            };
            let Some(owned) = &mut row.owned else {
                continue;
            };
            owned.queued = false;
            if owned.retired
                || owned.active.is_some()
                || row.value.evaluated_revision >= owned.target.revision
                || !matches!(
                    &owned.target.value,
                    Some(wire::desired_state::Value::Present(_))
                )
            {
                continue;
            }
            let revision = owned.target.revision;
            let sequence = self.next_execution_sequence()?;
            let work = Preparation {
                reference,
                revision,
                grant_revision: self.grant_revision,
                sequence,
                started_at: pulsebeam_runtime::time::Instant::now(),
            };
            self.charge(0, Preparation::bytes())?;
            let owned = self
                .rows
                .get_mut(&key)
                .and_then(|row| row.owned.as_mut())
                .ok_or(OutputError::InvalidObservation)?;
            owned.active = Some(work.stamp());
            self.execution_sequence = sequence;
            return Ok(Some(work));
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

    fn defer_preparation(&mut self, work: Preparation) -> Result<(), OutputError> {
        let key = Key::Owned(work.reference);
        let Some(owned) = self.rows.get(&key).and_then(|row| row.owned.as_ref()) else {
            return Ok(());
        };
        if owned.active != Some(work.stamp()) {
            return Ok(());
        }
        self.charge(Preparation::bytes(), 0)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.active = None;
        }
        self.queue_preparation(work.reference);
        self.collect(&key);
        Ok(())
    }

    pub(crate) fn preparation_spec(&self, work: &Preparation) -> Option<&wire::ResourceSpec> {
        let owned = self.rows.get(&Key::Owned(work.reference))?.owned.as_ref()?;
        if self.closed
            || owned.retired
            || owned.active != Some(work.stamp())
            || owned.target.revision != work.revision
            || self.grant_revision != work.grant_revision
        {
            return None;
        }
        let wire::desired_state::Value::Present(spec) = owned.target.value.as_ref()? else {
            return None;
        };
        Some(spec)
    }

    pub(crate) fn prepare_execution<'a>(
        &mut self,
        work: &Preparation,
        unix_now: u64,
        capabilities: Option<&wire::RtpCapabilities>,
        source: impl FnOnce(TrackId) -> Option<&'a Track>,
    ) -> Result<Option<ExecutionCommand>, OutputError> {
        match self.compile_execution(work, capabilities, source) {
            Ok(ExecutionCommand(ExecutionAction::Prepare {
                work,
                resource:
                    super::PreparedResource::Receiver(super::ReceiverPreparation::Waiting(source)),
            })) => {
                self.retain_source_wait(work, source)?;
                Ok(None)
            }
            Ok(command) => Ok(Some(command)),
            Err(error) => {
                self.complete_projected(*work, Err(error), unix_now)?;
                Ok(None)
            }
        }
    }

    fn compile_execution<'a>(
        &mut self,
        work: &Preparation,
        capabilities: Option<&wire::RtpCapabilities>,
        source: impl FnOnce(TrackId) -> Option<&'a Track>,
    ) -> Result<ExecutionCommand, wire::Failure> {
        use crate::control::native::PreparedResource;
        let resource = if let Some(publication) = self.prepare_publication(work, capabilities)? {
            PreparedResource::Publication(publication)
        } else {
            PreparedResource::Receiver(self.prepare_receiver(work, capabilities, source)?)
        };
        if let Some(owned) = self
            .rows
            .get_mut(&Key::Owned(work.reference))
            .and_then(|row| row.owned.as_mut())
            && owned.execution_bytes != 0
        {
            owned.storage_sequence = work.scope().sequence;
        }
        Ok(ExecutionCommand(ExecutionAction::Prepare {
            work: *work,
            resource,
        }))
    }

    fn prepare_publication(
        &mut self,
        work: &Preparation,
        capabilities: Option<&wire::RtpCapabilities>,
    ) -> Result<Option<crate::control::native::Publication>, wire::Failure> {
        let spec = self
            .preparation_spec(work)
            .ok_or_else(|| failure(wire::FailureCode::ResourceGone))?;
        let Some((plan, bytes)) =
            crate::control::native::publication(spec, capabilities).map_err(failure)?
        else {
            return Ok(None);
        };
        let reserved = self
            .rows
            .get(&Key::Owned(work.reference))
            .and_then(|row| row.owned.as_ref())
            .is_some_and(|owned| owned.execution_bytes != 0);
        if !reserved {
            self.reserve_execution(work.reference, bytes)
                .map_err(|_| failure(wire::FailureCode::CapacityExceeded))?;
        }
        Ok(Some(plan))
    }

    fn prepare_receiver<'a>(
        &mut self,
        work: &Preparation,
        capabilities: Option<&wire::RtpCapabilities>,
        source: impl FnOnce(TrackId) -> Option<&'a Track>,
    ) -> Result<crate::control::native::ReceiverPreparation, wire::Failure> {
        use crate::control::native::{ReceiveTransport, Receiver, ReceiverPreparation};
        let spec = self
            .preparation_spec(work)
            .ok_or_else(|| failure(wire::FailureCode::ResourceGone))?;
        let target = crate::control::native::receiver_target(spec).map_err(failure)?;
        if let Some(binding) = self
            .rows
            .get(&Key::Owned(work.reference))
            .and_then(|row| row.owned.as_ref())
            .and_then(|owned| owned.receiver_binding)
        {
            if !binding.available {
                return Err(failure(wire::FailureCode::Incompatible));
            }
            let policy =
                crate::control::native::receiver_policy(spec, binding.rtp).map_err(failure)?;
            let preview = crate::participant::policy::ReceiverExecution {
                source: target.source,
                enabled: policy.enabled,
                ready: false,
                failed: false,
            };
            self.project_receiver(work, &preview)?;
            return Ok(ReceiverPreparation::Policy(policy));
        }
        let Some((participant, track)) = self.source(target.key) else {
            if !target.wait_for_source {
                return Err(failure(wire::FailureCode::ResourceGone));
            }
            let source = target.source;
            self.reserve_execution(work.reference, 4096)
                .map_err(|_| failure(wire::FailureCode::CapacityExceeded))?;
            return Ok(ReceiverPreparation::Waiting(source));
        };
        let (source, (transport, policy)) = source(target.source)
            .ok_or(wire::FailureCode::Incompatible)
            .and_then(|source| {
                crate::control::native::receiver(spec, source, &track, capabilities)
                    .map(|prepared| (source, prepared))
            })
            .map_err(|code| wire::Failure {
                diagnostic: "native receiver transport preparation failed".to_owned(),
                ..failure(code)
            })?;
        let plan = Receiver {
            source: source.clone(),
            source_id: track.track_id.clone(),
            transport,
            policy,
        };
        let bytes = crate::participant::Connection::native_receiver_execution_bytes(
            &track.label,
            &plan.source_id,
            match &plan.transport {
                ReceiveTransport::Rtp(rtp) => Some(rtp.kind),
                ReceiveTransport::Sctp { .. } => None,
            },
        );
        self.reserve_receiver_source(work, bytes, participant, track)
            .map_err(|_| failure(wire::FailureCode::CapacityExceeded))?;
        Ok(ReceiverPreparation::Ready(Box::new(plan)))
    }

    pub(crate) fn preparation_current(
        &self,
        work: &Preparation,
        unix_now: u64,
    ) -> Result<bool, OutputError> {
        if self.preparation_spec(work).is_none() {
            return Ok(false);
        }
        if !self.authorization_current(unix_now) {
            return Err(OutputError::AuthorizationExpired);
        }
        Ok(true)
    }

    // Disposal belongs to the paid attempt, not merely the binding.
    fn release_preparation_storage(
        &mut self,
        work: Preparation,
        released: bool,
    ) -> Result<(), OutputError> {
        if released
            && self
                .rows
                .get(&Key::Owned(work.reference))
                .and_then(|row| row.owned.as_ref())
                .is_some_and(|owned| {
                    owned.active == Some(work.stamp())
                        || owned.storage_sequence == work.scope().sequence
                })
        {
            self.reserve_execution(work.reference, 0)?;
        }
        Ok(())
    }

    pub(crate) fn observe_deferral(
        &mut self,
        deferral: super::ExecutionDeferral,
    ) -> Result<(), OutputError> {
        metrics::histogram!("native_signaling_execution_observation_us").record(
            pulsebeam_runtime::time::Instant::now()
                .saturating_duration_since(deferral.work.started_at)
                .as_micros() as f64,
        );
        if let Some(sequence) = deferral.retained_sequence {
            if let Some(owned) = self
                .rows
                .get_mut(&Key::Owned(deferral.work.reference))
                .and_then(|row| row.owned.as_mut())
                && owned.active == Some(deferral.work.stamp())
                && owned.execution_bytes != 0
            {
                // Rejected admission leaves the surviving object's storage with its previous owner.
                owned.storage_sequence = sequence;
            }
        } else {
            self.release_preparation_storage(deferral.work, true)?;
        }
        self.defer_preparation(deferral.work)
    }

    pub(crate) fn observe_completion(
        &mut self,
        completion: super::ExecutionCompletion,
        external_id: &crate::entity::ParticipantExternalId,
    ) -> Result<(), OutputError> {
        let super::ExecutionCompletion {
            work,
            result,
            released,
            unix_now,
        } = completion;
        let executed = result.is_ok();
        metrics::histogram!("native_signaling_execution_observation_us").record(
            pulsebeam_runtime::time::Instant::now()
                .saturating_duration_since(work.started_at)
                .as_micros() as f64,
        );
        if executed && released {
            return Err(OutputError::InvalidObservation);
        }
        self.release_preparation_storage(work, released)?;
        let rejected = matches!(
            self.complete_execution(work, result, external_id, unix_now)?,
            ExecutionDisposition::Rejected
        );
        if executed && rejected {
            // A prepaid, scoped execution fact cannot be rejected as an ordinary proposal.
            return Err(OutputError::InvalidObservation);
        }
        Ok(())
    }

    fn complete_execution(
        &mut self,
        work: Preparation,
        result: Result<ResourceExecution, crate::participant::policy::ExecutionFailure>,
        external_id: &crate::entity::ParticipantExternalId,
        unix_now: u64,
    ) -> Result<ExecutionDisposition, OutputError> {
        let projected = result
            .map_err(super::execution_failure)
            .and_then(|execution| match execution {
                ResourceExecution::Publication(execution) => {
                    self.project_publication(&work, &execution, external_id)
                }
                ResourceExecution::Receiver(execution) => self.project_receiver(&work, &execution),
            });
        let rejected = projected.is_err();
        if !self.complete_projected(work, projected, unix_now)? {
            return Ok(ExecutionDisposition::Deferred);
        }
        Ok(if rejected {
            ExecutionDisposition::Rejected
        } else {
            ExecutionDisposition::Accepted
        })
    }

    fn complete_projected(
        &mut self,
        work: Preparation,
        result: Result<ProjectedExecution, wire::Failure>,
        now: u64,
    ) -> Result<bool, OutputError> {
        let result = match result {
            Ok(projected) => {
                if projected.work != work {
                    return Err(OutputError::InvalidObservation);
                }
                Ok(projected.live)
            }
            Err(error) => Err(error),
        };
        self.complete(work, result, now)
    }

    fn complete(
        &mut self,
        work: Preparation,
        result: Result<wire::LiveResource, wire::Failure>,
        unix_now: u64,
    ) -> Result<bool, OutputError> {
        let key = Key::Owned(work.reference);
        let Some(owned) = self.rows.get(&key).and_then(|row| row.owned.as_ref()) else {
            return Ok(false);
        };
        if owned.active != Some(work.stamp()) {
            return Ok(false);
        }
        let current = self.preparation_current(&work, unix_now);
        if matches!(current, Ok(false) | Err(OutputError::AuthorizationExpired)) {
            self.defer_preparation(work)?;
            return Ok(false);
        }
        current?;
        if let Ok(live) = &result {
            let target = self
                .preparation_spec(&work)
                .ok_or(OutputError::InvalidObservation)?;
            if live.spec.as_ref().is_none_or(|spec| {
                !validation::declarations_equivalent(target, spec)
                    || !matches!((&target.settings, &spec.settings), (Some(requested), Some(accepted)) if validation::settings_equivalent(requested, accepted))
            }) {
                return Err(OutputError::InvalidObservation);
            }
        }
        self.charge(Preparation::bytes(), 0)?;
        let Some(row) = self.rows.get_mut(&key) else {
            return Ok(false);
        };
        let Some(owned) = &mut row.owned else {
            return Ok(false);
        };
        owned.active = None;
        let Some(row) = self.rows.get_mut(&key) else {
            return Ok(false);
        };
        let Some(owned) = &mut row.owned else {
            return Ok(false);
        };
        let value = match result {
            Ok(live) => wire::ConfirmedState {
                evaluated_revision: work.revision,
                rejection: None,
                value: Some(wire::confirmed_state::Value::Owned(Box::new(live))),
            },
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
        self.release_receiver_source(work.reference)?;
        self.release_receiver_failure(work.reference)?;
        self.release_label(work.reference);
        Ok(true)
    }

    fn reserve_execution(&mut self, reference: u64, bytes: usize) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let owned = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .ok_or(OutputError::InvalidObservation)?;
        let source_bytes = if bytes == 0 {
            owned
                .receiver_source
                .as_ref()
                .map_or(0, |source| source.bytes())
        } else {
            0
        };
        let failure_bytes = if bytes == 0 {
            owned
                .receiver_failure
                .as_deref()
                .map_or(0, receiver_failure_bytes)
        } else {
            0
        };
        let previous = owned
            .execution_bytes
            .saturating_add(source_bytes)
            .saturating_add(failure_bytes);
        self.charge(previous, bytes)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.execution_bytes = bytes;
            if bytes == 0 {
                owned.storage_sequence = 0;
                owned.receiver_source = None;
                owned.receiver_binding = None;
                owned.receiver_failure = None;
            }
        }
        self.collect(&key);
        Ok(())
    }

    fn reserve_receiver_source(
        &mut self,
        work: &Preparation,
        bytes: usize,
        participant: wire::Participant,
        track: wire::Track,
    ) -> Result<(), OutputError> {
        let preview = wire::LiveResource {
            spec: Some(
                self.preparation_spec(work)
                    .ok_or(OutputError::InvalidObservation)?
                    .clone(),
            ),
            participant: Some(participant.clone()),
            track: Some(track.clone()),
            ..Default::default()
        };
        if !execution_fits(&preview) {
            return Err(OutputError::Capacity);
        }
        let key = Key::Owned(work.reference);
        let owned = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .ok_or(OutputError::InvalidObservation)?;
        let previous = owned.execution_bytes.saturating_add(
            owned
                .receiver_source
                .as_ref()
                .map_or(0, |source| source.bytes()),
        );
        let source = ReceiverSource { participant, track };
        self.charge(previous, bytes.saturating_add(source.bytes()))?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.execution_bytes = bytes;
            owned.receiver_source = Some(Box::new(source));
        }
        Ok(())
    }

    fn release_receiver_source(&mut self, reference: u64) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let bytes = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .and_then(|owned| owned.receiver_source.as_ref())
            .map_or(0, |source| source.bytes());
        self.charge(bytes, 0)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.receiver_source = None;
        }
        Ok(())
    }

    fn release_receiver_failure(&mut self, reference: u64) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let bytes = self
            .rows
            .get(&key)
            .and_then(|row| row.owned.as_ref())
            .and_then(|owned| owned.receiver_failure.as_deref())
            .map_or(0, receiver_failure_bytes);
        self.charge(bytes, 0)?;
        if let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) {
            owned.receiver_failure = None;
        }
        Ok(())
    }

    fn project_publication(
        &self,
        work: &Preparation,
        execution: &crate::participant::policy::PublicationExecution,
        external_id: &crate::entity::ParticipantExternalId,
    ) -> Result<ProjectedExecution, wire::Failure> {
        let spec = self
            .preparation_spec(work)
            .ok_or_else(|| failure(wire::FailureCode::ResourceGone))?;
        if !matches!(
            spec.declaration,
            Some(wire::resource_spec::Declaration::Publication(_))
        ) {
            return Err(failure(wire::FailureCode::InvalidConfiguration));
        }
        let live = wire::LiveResource {
            spec: Some(spec.clone()),
            participant: Some(wire::Participant {
                participant_id: execution.track.meta().origin.as_str(),
                participant_external_id: external_id.as_str().to_owned(),
            }),
            track: track_descriptor(&execution.track),
            execution: if execution.enabled {
                wire::ExecutionStatus::Installed
            } else {
                wire::ExecutionStatus::Disabled
            } as i32,
            execution_failure: None,
            activity: if matches!(execution.track, Track::Data(_)) {
                None
            } else {
                Some(if execution.active {
                    wire::MediaActivity::Active
                } else {
                    wire::MediaActivity::Idle
                } as i32)
            },
        };
        Ok(ProjectedExecution { work: *work, live })
    }

    fn project_receiver(
        &self,
        work: &Preparation,
        execution: &crate::participant::policy::ReceiverExecution,
    ) -> Result<ProjectedExecution, wire::Failure> {
        let (participant, track) = self
            .rows
            .get(&Key::Owned(work.reference))
            .and_then(|row| {
                row.owned
                    .as_ref()
                    .and_then(|owned| owned.receiver_source.as_ref())
                    .map(|source| (&source.participant, &source.track))
                    .or_else(|| match &row.value.value {
                        Some(wire::confirmed_state::Value::Owned(live)) => {
                            live.participant.as_ref().zip(live.track.as_ref())
                        }
                        _ => None,
                    })
            })
            .filter(|(_, track)| track.track_id == execution.source.as_str())
            .ok_or_else(|| failure(wire::FailureCode::ResourceGone))?;
        let (status, execution_failure) = self
            .receiver_result(work.reference, execution)
            .map_err(|_| failure(wire::FailureCode::InternalFailure))?;
        let live = wire::LiveResource {
            spec: Some(
                self.preparation_spec(work)
                    .ok_or_else(|| failure(wire::FailureCode::ResourceGone))?
                    .clone(),
            ),
            participant: Some(participant.clone()),
            track: Some(track.clone()),
            execution: status as i32,
            execution_failure,
            activity: None,
        };
        if !execution_fits(&live) {
            return Err(failure(wire::FailureCode::CapacityExceeded));
        }
        Ok(ProjectedExecution { work: *work, live })
    }

    fn source_ended(&mut self, reference: u64) -> Result<(), OutputError> {
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

    fn cutoff_complete(&mut self, reference: u64) -> Result<(), OutputError> {
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
            && self.pending.as_ref().map_or_else(
                || !self.dirty.is_empty() || !self.initial_complete,
                |pending| {
                    pending.output.is_some()
                        || self.deadline().is_some_and(|deadline| {
                            pulsebeam_runtime::time::Instant::now() >= deadline
                        })
                },
            )
    }

    pub(crate) fn enumeration_progress(&self) -> u64 {
        self.initial_cursor
    }

    pub(crate) fn deadline(&self) -> Option<pulsebeam_runtime::time::Instant> {
        self.progress_deadline()
            .into_iter()
            .chain(self.source_deadlines.first().map(|(deadline, _)| *deadline))
            .min()
    }

    fn progress_deadline(&self) -> Option<pulsebeam_runtime::time::Instant> {
        self.pending
            .as_ref()
            .map(|pending| pending.deadline)
            .into_iter()
            .chain(self.initial_deadline)
            .min()
    }

    pub(crate) fn poll(&mut self) -> Result<Option<&SignalingOutput>, OutputError> {
        if self
            .progress_deadline()
            .is_some_and(|deadline| pulsebeam_runtime::time::Instant::now() >= deadline)
        {
            return Err(OutputError::ProgressExpired);
        }
        if self.pending.is_some() {
            return Ok(self
                .pending
                .as_ref()
                .and_then(|pending| pending.output.as_ref()));
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
        let mut batch = wire::ServerStateBatch::default();
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
            let record = wire::ServerStateRecord {
                key: Some(key.wire()),
                confirmed: Some(row.value.clone()),
            };
            let candidate_len = batch
                .encoded_len()
                .saturating_add(record.encoded_len())
                .saturating_add(3);
            if candidate_len > codec::MAX_FRAME_SIZE {
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
        if batch.encoded_len() > codec::MAX_FRAME_SIZE {
            batch.initial_complete = false;
        }
        if batch.records.is_empty() && !batch.initial_complete {
            self.initial_cursor = selected_cursor;
            return Ok(None);
        }
        let bytes = codec::encode_server(&batch).map_err(OutputError::Encode)?;
        let sequence = self
            .output_sequence
            .checked_add(1)
            .ok_or(OutputError::Capacity)?;
        let selected_at = pulsebeam_runtime::time::Instant::now();
        let deadline = progress_deadline(selected_at);
        let deadline = self
            .initial_deadline
            .map_or(deadline, |initial| initial.min(deadline));
        self.output_sequence = sequence;
        self.pending = Some(Pending {
            output: Some(SignalingOutput {
                sequence,
                cid,
                bytes,
                deadline,
            }),
            sequence,
            cid,
            selected,
            initial_cursor: selected_cursor,
            initial_complete: batch.initial_complete,
            selected_at,
            deadline,
        });
        Ok(self
            .pending
            .as_ref()
            .and_then(|pending| pending.output.as_ref()))
    }

    fn requeue(&mut self, key: &Key) {
        if let Some(row) = self.rows.get_mut(key)
            && row.ticket.is_none()
        {
            row.ticket = Some(row.version);
            self.dirty.insert(row.version, key.clone());
        }
    }

    pub(crate) fn take_output(&mut self) -> Option<SignalingOutput> {
        self.pending.as_mut()?.output.take()
    }

    pub(crate) fn restore_output(&mut self, output: SignalingOutput) -> Result<(), OutputError> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(OutputError::InvalidObservation);
        };
        if pending.sequence != output.sequence
            || pending.cid != output.cid
            || pending.deadline != output.deadline
            || pending.output.is_some()
        {
            return Err(OutputError::InvalidObservation);
        }
        pending.output = Some(output);
        Ok(())
    }

    pub(crate) fn commit_sent(&mut self, progress: SignalingProgress) -> Result<bool, OutputError> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(false);
        };
        if pending.sequence != progress.sequence
            || pending.cid != progress.cid
            || pending.output.is_some()
        {
            return Ok(false);
        }
        if progress.at >= pending.deadline {
            return Err(OutputError::ProgressExpired);
        }
        let Some(pending) = self.pending.take() else {
            return Ok(false);
        };
        metrics::histogram!("native_signaling_output_admission_us").record(
            progress
                .at
                .saturating_duration_since(pending.selected_at)
                .as_micros() as f64,
        );
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
            self.initial_deadline = Some(progress_deadline(progress.at));
        }
        Ok(true)
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
                    .saturating_add(owned.execution_bytes)
                    .saturating_add(
                        owned
                            .receiver_source
                            .as_ref()
                            .map_or(0, |source| source.bytes()),
                    )
            });
            let bytes = key_cost(key)
                .saturating_add(row.value.encoded_len())
                .saturating_add(owned_bytes);
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
        }
    }
}

fn progress_deadline(at: pulsebeam_runtime::time::Instant) -> pulsebeam_runtime::time::Instant {
    at.checked_add(std::time::Duration::from_secs(30))
        .unwrap_or(at)
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
    let format = wire::CodecFormat::from(wire::codec_format::Codec::Opus(wire::OpusParameters {
        stereo: Some(true),
        source_stereo: Some(true),
        in_band_fec: Some(true),
        dtx: Some(true),
        constant_bitrate: Some(true),
        max_average_bitrate_bps: Some(510000),
        max_playback_rate_hz: Some(48000),
        max_capture_rate_hz: Some(48000),
        min_packet_time_ms: Some(120),
    }));
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
    let record = wire::ServerStateRecord {
        key: Some(Key::Owned(u64::MAX).wire()),
        confirmed: Some(wire::ConfirmedState {
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
        }),
    };
    wire::ServerStateBatch {
        records: vec![record],
        initial_complete: true,
    }
    .encoded_len()
        <= codec::MAX_FRAME_SIZE
}

pub(super) fn execution_fits(live: &wire::LiveResource) -> bool {
    let failure = wire::Failure {
        code: wire::FailureCode::InternalFailure as i32,
        diagnostic: "e".repeat(256),
    };
    let mut live = live.clone();
    live.execution = wire::ExecutionStatus::Failed as i32;
    live.execution_failure = Some(failure.clone());
    wire::ServerStateBatch {
        initial_complete: true,
        records: vec![wire::ServerStateRecord {
            key: Some(Key::Owned(u64::MAX).wire()),
            confirmed: Some(wire::ConfirmedState {
                evaluated_revision: u64::MAX,
                rejection: Some(failure),
                value: Some(wire::confirmed_state::Value::Owned(Box::new(live))),
            }),
        }],
    }
    .encoded_len()
        <= codec::MAX_FRAME_SIZE
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
            format: Some(audio.format.clone()?),
        }),
        Track::Video(video) => wire::track::Kind::Video(wire::VideoTrack {
            format: Some(video.format.clone()?),
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

fn receiver_failure_bytes(error: &wire::Failure) -> usize {
    std::mem::size_of::<wire::Failure>().saturating_add(error.encoded_len())
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
