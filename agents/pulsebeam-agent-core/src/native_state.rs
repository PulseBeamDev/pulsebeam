use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    string::String,
    vec::Vec,
};
use pulsebeam_proto::{
    prelude::Message,
    state::{self as wire, codec, validation},
};

use crate::{MediaSlot, SignalingError};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Resource {
    Publication {
        kind: wire::TrackKind,
        label: String,
    },
    Reception(MediaSlot),
    DataSubscription(String),
}

impl Resource {
    fn matches_spec(&self, spec: &wire::ResourceSpec) -> bool {
        match (self, spec.declaration.as_ref()) {
            (
                Self::Publication { kind, label },
                Some(wire::resource_spec::Declaration::Publication(publication)),
            ) => {
                label == &publication.label
                    && match publication.kind.as_ref() {
                        Some(wire::publication_spec::Kind::Audio(_)) => {
                            *kind == wire::TrackKind::Audio
                        }
                        Some(wire::publication_spec::Kind::Video(_)) => {
                            *kind == wire::TrackKind::Video
                        }
                        Some(wire::publication_spec::Kind::Data(_)) => {
                            *kind == wire::TrackKind::Data
                        }
                        None => false,
                    }
            }
            (
                Self::Reception(slot),
                Some(wire::resource_spec::Declaration::Subscription(subscription)),
            ) => match slot {
                MediaSlot::RemoteAudio(_) => subscription.kind == wire::TrackKind::Audio as i32,
                MediaSlot::RemoteVideo(_) => subscription.kind == wire::TrackKind::Video as i32,
                _ => false,
            },
            (
                Self::DataSubscription(track),
                Some(wire::resource_spec::Declaration::Subscription(subscription)),
            ) => {
                track == &subscription.track_id && subscription.kind == wire::TrackKind::Data as i32
            }
            _ => false,
        }
    }

    fn retained_bytes(&self) -> usize {
        let text = match self {
            Self::Publication { label, .. } => label.len(),
            Self::Reception(MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name)) => {
                name.len()
            }
            Self::Reception(_) => 0,
            Self::DataSubscription(track) => track.len(),
        };
        size_of::<Self>().saturating_add(text)
    }
}

struct Target {
    spec: wire::ResourceSpec,
    reference: Option<u64>,
    waiting_for: Option<u64>,
    blocked: bool,
}

impl Target {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.spec.encoded_len())
    }
}

// Reserve keyed pending-work and two bounded canonical-identity indexes per live row.
const ROW_INDEX_BYTES: usize = 1024;

struct Owned {
    resource: Resource,
    sent_revision: u64,
    sent: wire::ResourceSpec,
    confirmed: Option<wire::ConfirmedState>,
    execution: Option<Box<wire::ExecutionReport>>,
    retiring: bool,
}

impl Owned {
    fn awaiting_evaluation(&self) -> bool {
        self.confirmed
            .as_ref()
            .is_none_or(|state| state.evaluated_revision < self.sent_revision)
    }

    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(ROW_INDEX_BYTES)
            .saturating_add(self.resource.retained_bytes())
            .saturating_add(self.sent.encoded_len())
            .saturating_add(self.confirmed.as_ref().map_or(0, Message::encoded_len))
            .saturating_add(self.execution.as_ref().map_or(0, |report| {
                size_of::<wire::ExecutionReport>().saturating_add(report.encoded_len())
            }))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Change {
    Owned {
        resource: Resource,
        reference: u64,
        retired_binding: Option<Box<wire::TrackBinding>>,
        state: Box<wire::ConfirmedState>,
        new_evaluation: bool,
    },
    Participant(String),
    Track(String),
    Session,
    InitialComplete,
}

pub(crate) struct NativeState {
    revision: u64,
    targets: BTreeMap<Resource, Target>,
    owned: BTreeMap<u64, Owned>,
    dirty: BTreeSet<Resource>,
    retiring: BTreeSet<u64>,
    executions: BTreeSet<u64>,
    prefer_execution: bool,
    retiring_resources: BTreeMap<Resource, u64>,
    cursor: Option<Resource>,
    prefer_retirement: bool,
    participants: BTreeMap<String, wire::Participant>,
    tracks: BTreeMap<String, wire::Track>,
    source_references: BTreeMap<String, BTreeSet<u64>>,
    participant_references: BTreeMap<String, BTreeSet<u64>>,
    self_participant: wire::Participant,
    session: Option<wire::SessionState>,
    initial_complete: bool,
    terminal: bool,
    retained_bytes: usize,
    memory_limit: usize,
}

impl NativeState {
    pub(crate) fn new(
        memory_limit: usize,
        self_participant: wire::Participant,
    ) -> Result<Self, SignalingError> {
        validation::validate_participant(&self_participant)
            .map_err(|_| SignalingError::Invalid("bootstrap participant"))?;
        Ok(Self {
            revision: 0,
            targets: BTreeMap::new(),
            owned: BTreeMap::new(),
            dirty: BTreeSet::new(),
            retiring: BTreeSet::new(),
            executions: BTreeSet::new(),
            prefer_execution: true,
            retiring_resources: BTreeMap::new(),
            cursor: None,
            prefer_retirement: true,
            participants: BTreeMap::new(),
            tracks: BTreeMap::new(),
            source_references: BTreeMap::new(),
            participant_references: BTreeMap::new(),
            self_participant,
            session: None,
            initial_complete: false,
            terminal: false,
            retained_bytes: 0,
            memory_limit,
        })
    }

    pub(crate) fn stage(
        &mut self,
        resource: Resource,
        spec: Option<wire::ResourceSpec>,
    ) -> Result<(), SignalingError> {
        if self.terminal {
            return Err(SignalingError::Invalid("terminal Session"));
        }
        if let Some(spec) = &spec {
            validation::validate_spec(spec)
                .map_err(|_| SignalingError::Invalid("resource spec"))?;
            if !resource.matches_spec(spec) {
                return Err(SignalingError::Invalid("resource correlation"));
            }
            // Reserve representability with the largest revision/ref encodings.
            codec::encode_client(&wire::ClientStateBatch {
                records: alloc::vec![desired_record(u64::MAX, u64::MAX, Some(spec.clone()))],
            })
            .map_err(|_| SignalingError::Encode)?;
        }
        let old = self.targets.get(&resource);
        if old.is_some_and(|old| {
            spec.as_ref()
                .is_some_and(|spec| specs_equal(&old.spec, spec))
        }) {
            return Ok(());
        }
        let previous_reference = old.and_then(|old| old.reference);
        let replace = old.is_some_and(|old| {
            spec.as_ref()
                .is_none_or(|spec| !validation::declarations_equivalent(&old.spec, spec))
        });
        let waiting_for = if replace {
            previous_reference.or_else(|| self.retiring_resources.get(&resource).copied())
        } else {
            self.retiring_resources.get(&resource).copied()
        };
        let mut next = spec.map(|spec| Target {
            spec,
            reference: (!replace).then_some(previous_reference).flatten(),
            waiting_for,
            blocked: false,
        });
        let already_sent = next
            .as_ref()
            .and_then(|target| {
                target
                    .reference
                    .and_then(|reference| self.owned.get(&reference))
            })
            .filter(|owned| {
                next.as_ref()
                    .is_some_and(|target| specs_equal(&owned.sent, &target.spec))
            });
        if let (Some(next), Some(owned)) = (&mut next, already_sent) {
            next.blocked = owned.confirmed.as_ref().is_some_and(|state| {
                state.rejection.is_some() && state.evaluated_revision == owned.sent_revision
            });
        }
        let needs_send = already_sent.is_none();
        let old_bytes = old.map_or(0, |old| {
            resource
                .retained_bytes()
                .saturating_add(old.retained_bytes())
        });
        let new_bytes = next.as_ref().map_or(0, |next| {
            resource
                .retained_bytes()
                .saturating_add(next.retained_bytes())
        });
        self.account(old_bytes, new_bytes)?;
        if replace && let Some(reference) = previous_reference {
            if let Some(owned) = self.owned.get_mut(&reference) {
                owned.retiring = true;
            }
            self.retiring.insert(reference);
            self.retiring_resources.insert(resource.clone(), reference);
        }
        if let Some(next) = next {
            if next.waiting_for.is_none() && needs_send {
                self.dirty.insert(resource.clone());
            } else {
                self.dirty.remove(&resource);
            }
            self.targets.insert(resource, next);
        } else {
            self.targets.remove(&resource);
            self.dirty.remove(&resource);
        }
        Ok(())
    }

    pub(crate) fn retry(&mut self, resource: &Resource) {
        if let Some(target) = self.targets.get_mut(resource) {
            target.blocked = false;
            if target.waiting_for.is_none() {
                self.dirty.insert(resource.clone());
            }
        }
    }

    pub(crate) fn report_execution(
        &mut self,
        reference: u64,
        failure: wire::Failure,
    ) -> Result<(), SignalingError> {
        let Some(owned) = self
            .owned
            .get(&reference)
            .filter(|owned| !owned.retiring && !self.terminal)
        else {
            return Ok(());
        };
        let Some(wire::confirmed_state::Value::Owned(live)) = owned
            .confirmed
            .as_ref()
            .and_then(|state| state.value.as_ref())
        else {
            return Err(SignalingError::Invalid("failure without admitted receiver"));
        };
        if !matches!(owned.resource, Resource::Reception(_))
            || !matches!(live.spec.as_ref().and_then(|spec| spec.declaration.as_ref()),
                Some(wire::resource_spec::Declaration::Subscription(subscription))
                    if matches!(subscription.binding.as_ref().and_then(|binding| binding.transport.as_ref()), Some(wire::track_binding::Transport::Rtp(_))))
        {
            return Err(SignalingError::Invalid("failure without admitted receiver"));
        }
        let report = Box::new(wire::ExecutionReport {
            revision: u64::MAX,
            failure: Some(failure),
        });
        validation::validate_execution_report(&report)
            .map_err(|_| SignalingError::Invalid("execution report"))?;
        let old = owned.execution.as_ref().map_or(0, |report| {
            size_of::<wire::ExecutionReport>().saturating_add(report.encoded_len())
        });
        let next = size_of::<wire::ExecutionReport>().saturating_add(report.encoded_len());
        self.account(old, next)?;
        if let Some(owned) = self.owned.get_mut(&reference) {
            owned.execution = Some(report);
        }
        self.executions.insert(reference);
        Ok(())
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Vec<u8>>, SignalingError> {
        if self.terminal {
            return Ok(None);
        }
        let mut batch = wire::ClientStateBatch::default();
        while batch.records.len() < codec::MAX_RECORDS {
            let resource = self
                .cursor
                .as_ref()
                .and_then(|cursor| {
                    self.dirty
                        .range((
                            core::ops::Bound::Excluded(cursor),
                            core::ops::Bound::Unbounded,
                        ))
                        .next()
                })
                .or_else(|| self.dirty.first())
                .cloned();
            let retirement = if self.prefer_retirement || resource.is_none() {
                self.retiring.first().copied()
            } else {
                None
            };
            let execution = retirement
                .filter(|reference| self.executions.contains(reference))
                .or_else(|| self.executions.first().copied());
            if let Some(reference) = execution
                && (self.prefer_execution
                    || (resource.is_none() && retirement.is_none())
                    || retirement == Some(reference))
            {
                let Some(report) = self
                    .owned
                    .get(&reference)
                    .and_then(|owned| owned.execution.as_ref())
                else {
                    self.executions.remove(&reference);
                    continue;
                };
                let bytes = size_of::<wire::ExecutionReport>().saturating_add(report.encoded_len());
                let revision = self.next_revision()?;
                let mut report = report.as_ref().clone();
                report.revision = revision;
                batch.records.push(wire::ClientStateRecord {
                    owned: reference,
                    desired: None,
                    execution: Some(report),
                });
                if batch.encoded_len() > codec::MAX_FRAME_SIZE {
                    batch.records.pop();
                    if batch.records.is_empty() {
                        return Err(SignalingError::Encode);
                    }
                    break;
                }
                self.account(bytes, 0)?;
                if let Some(owned) = self.owned.get_mut(&reference) {
                    owned.execution = None;
                }
                self.executions.remove(&reference);
                self.revision = revision;
                self.prefer_execution = false;
                continue;
            }
            let (reference, resource, spec) = if let Some(reference) = retirement {
                let Some(owned) = self.owned.get(&reference) else {
                    self.retiring.remove(&reference);
                    continue;
                };
                (reference, owned.resource.clone(), None)
            } else if let Some(resource) = resource {
                let Some(target) = self.targets.get(&resource) else {
                    self.dirty.remove(&resource);
                    continue;
                };
                if target.blocked
                    || target
                        .reference
                        .and_then(|reference| self.owned.get(&reference))
                        .is_some_and(Owned::awaiting_evaluation)
                {
                    self.dirty.remove(&resource);
                    continue;
                }
                (
                    target.reference.unwrap_or(self.next_revision()?),
                    resource,
                    Some(target.spec.clone()),
                )
            } else {
                break;
            };
            let revision = self.next_revision()?;
            batch
                .records
                .push(desired_record(reference, revision, spec.clone()));
            if batch.encoded_len() > codec::MAX_FRAME_SIZE {
                batch.records.pop();
                if batch.records.is_empty() {
                    return Err(SignalingError::Encode);
                }
                break;
            }
            let previous = self.owned.get(&reference);
            let sent = spec
                .or_else(|| previous.map(|owned| owned.sent.clone()))
                .ok_or(SignalingError::Invalid("missing retirement declaration"))?;
            let next = Owned {
                resource: resource.clone(),
                sent_revision: revision,
                sent,
                confirmed: previous.and_then(|owned| owned.confirmed.clone()),
                execution: previous.and_then(|owned| owned.execution.clone()),
                retiring: retirement.is_some(),
            };
            let old_bytes = previous.map_or(0, Owned::retained_bytes);
            if let Err(error) = self.account(old_bytes, next.retained_bytes()) {
                batch.records.pop();
                if batch.records.is_empty() {
                    return Err(error);
                }
                break;
            }
            self.owned.insert(reference, next);
            self.revision = revision;
            self.prefer_retirement = retirement.is_none();
            self.prefer_execution = true;
            if retirement.is_some() {
                self.retiring.remove(&reference);
            } else {
                self.cursor = Some(resource.clone());
                if let Some(target) = self.targets.get_mut(&resource) {
                    target.reference = Some(reference);
                }
                self.dirty.remove(&resource);
            }
        }
        if batch.records.is_empty() {
            return Ok(None);
        }
        codec::encode_client(&batch)
            .map(Some)
            .map_err(|_| SignalingError::Encode)
    }

    pub(crate) fn apply(&mut self, frame: &[u8]) -> Result<Vec<Change>, SignalingError> {
        if self.terminal {
            return Ok(Vec::new());
        }
        let batch = codec::decode_server(frame).map_err(|_| SignalingError::Malformed)?;
        let mut changes = Vec::new();
        for record in batch.records {
            if self.terminal {
                return Ok(changes);
            }
            let key = record
                .key
                .and_then(|key| key.key)
                .ok_or(SignalingError::MissingPayload)?;
            let state = record.confirmed.ok_or(SignalingError::MissingPayload)?;
            match key {
                wire::resource_key::Key::Owned(reference) => {
                    if let Some(change) = self.apply_owned(reference, state)? {
                        changes.push(change);
                    }
                }
                wire::resource_key::Key::Session(_) => {
                    let Some(wire::confirmed_state::Value::Session(session)) = state.value else {
                        return Err(SignalingError::Invalid("Session value"));
                    };
                    if let (Some(previous), Some(next)) = (
                        self.session
                            .as_ref()
                            .and_then(|session| session.authorization.as_ref()),
                        session.authorization.as_ref(),
                    ) && (next.revision < previous.revision
                        || next.revision == previous.revision && next != previous)
                    {
                        return Err(SignalingError::Invalid("authorization revision"));
                    }
                    let old_bytes = self.session.as_ref().map_or(0, Message::encoded_len);
                    self.account(old_bytes, session.encoded_len())?;
                    self.terminal = session.closed.is_some();
                    self.session = Some(session);
                    changes.push(Change::Session);
                }
                wire::resource_key::Key::Participant(id) => {
                    let old = self.participants.get(&id);
                    let next = match state.value {
                        Some(wire::confirmed_state::Value::Participant(participant)) => {
                            if old.is_some_and(|old| old != &participant)
                                || participant.participant_id
                                    == self.self_participant.participant_id
                                    && participant != self.self_participant
                                || self
                                    .participant_references
                                    .get(&id)
                                    .and_then(|references| references.first())
                                    .and_then(|reference| self.live(*reference))
                                    .and_then(|live| live.participant.as_ref())
                                    .is_some_and(|previous| previous != &participant)
                            {
                                return Err(SignalingError::Invalid("participant identity"));
                            }
                            Some(participant)
                        }
                        Some(wire::confirmed_state::Value::Absent(_)) => None,
                        _ => return Err(SignalingError::Invalid("participant value")),
                    };
                    let old_bytes = old.map_or(0, |value| entry_bytes(&id, value));
                    let new_bytes = next.as_ref().map_or(0, |value| entry_bytes(&id, value));
                    self.account(old_bytes, new_bytes)?;
                    if let Some(next) = next {
                        self.participants.insert(id.clone(), next);
                    } else {
                        self.participants.remove(&id);
                    }
                    changes.push(Change::Participant(id));
                }
                wire::resource_key::Key::Track(id) => {
                    let old = self.tracks.get(&id);
                    let next = match state.value {
                        Some(wire::confirmed_state::Value::Track(track)) => {
                            if old.is_some_and(|old| !tracks_equal(old, &track))
                                || self
                                    .source_references
                                    .get(&id)
                                    .and_then(|references| references.first())
                                    .and_then(|reference| self.live(*reference))
                                    .and_then(|live| live.track.as_ref())
                                    .is_some_and(|previous| !tracks_equal(previous, &track))
                            {
                                return Err(SignalingError::Invalid("track identity"));
                            }
                            Some(track)
                        }
                        Some(wire::confirmed_state::Value::Absent(_)) => None,
                        _ => return Err(SignalingError::Invalid("track value")),
                    };
                    let old_bytes = old.map_or(0, |value| entry_bytes(&id, value));
                    let new_bytes = next.as_ref().map_or(0, |value| entry_bytes(&id, value));
                    self.account(old_bytes, new_bytes)?;
                    if let Some(next) = next {
                        self.tracks.insert(id.clone(), next);
                    } else {
                        self.tracks.remove(&id);
                    }
                    changes.push(Change::Track(id));
                }
            }
        }
        if batch.initial_complete && !self.terminal {
            if self.initial_complete {
                return Err(SignalingError::Invalid("repeated initial completion"));
            }
            self.initial_complete = true;
            changes.push(Change::InitialComplete);
        }
        Ok(changes)
    }

    fn apply_owned(
        &mut self,
        reference: u64,
        state: wire::ConfirmedState,
    ) -> Result<Option<Change>, SignalingError> {
        let Some(owned) = self.owned.get(&reference) else {
            if reference <= self.revision
                && matches!(state.value, Some(wire::confirmed_state::Value::Absent(_)))
            {
                return Ok(None);
            }
            return Err(SignalingError::Invalid("unknown owned reference"));
        };
        if state.evaluated_revision > owned.sent_revision
            || owned
                .confirmed
                .as_ref()
                .is_some_and(|old| state.evaluated_revision < old.evaluated_revision)
        {
            return Err(SignalingError::Invalid("evaluated revision"));
        }
        if let Some(wire::confirmed_state::Value::Owned(live)) = &state.value {
            self.validate_source(live)?;
            let spec = live.spec.as_ref().ok_or(SignalingError::MissingPayload)?;
            if !validation::declarations_equivalent(&owned.sent, spec) {
                return Err(SignalingError::Invalid("accepted declaration"));
            }
            if let Some(wire::confirmed_state::Value::Owned(previous)) = owned
                .confirmed
                .as_ref()
                .and_then(|state| state.value.as_ref())
                && (previous.participant != live.participant
                    || !matches!((&previous.track, &live.track), (Some(previous), Some(next)) if tracks_equal(previous, next)))
            {
                return Err(SignalingError::Invalid("owned source identity"));
            }
        }
        let new_evaluation = owned
            .confirmed
            .as_ref()
            .is_none_or(|previous| previous.evaluated_revision != state.evaluated_revision);
        let resource = owned.resource.clone();
        let terminal = matches!(state.value, Some(wire::confirmed_state::Value::Absent(_)));
        let retired_binding = if terminal {
            owned
                .confirmed
                .as_ref()
                .and_then(|state| match state.value.as_ref() {
                    Some(wire::confirmed_state::Value::Owned(live))
                        if matches!(resource, Resource::Reception(_)) =>
                    {
                        live.spec
                            .as_ref()
                            .and_then(|spec| match spec.declaration.as_ref() {
                                Some(wire::resource_spec::Declaration::Subscription(
                                    subscription,
                                )) => subscription.binding.clone().map(Box::new),
                                _ => None,
                            })
                    }
                    _ => None,
                })
        } else {
            None
        };
        let old_bytes = owned.retained_bytes();
        let new_bytes = if terminal {
            0
        } else {
            old_bytes
                .saturating_sub(owned.confirmed.as_ref().map_or(0, Message::encoded_len))
                .saturating_add(state.encoded_len())
        };
        let previous_source = owned
            .confirmed
            .as_ref()
            .and_then(|state| match &state.value {
                Some(wire::confirmed_state::Value::Owned(live)) => Some((
                    live.track.as_ref()?.track_id.clone(),
                    live.participant.as_ref()?.participant_id.clone(),
                )),
                _ => None,
            });
        let next_source = match &state.value {
            Some(wire::confirmed_state::Value::Owned(live)) if previous_source.is_none() => Some((
                live.track
                    .as_ref()
                    .ok_or(SignalingError::MissingPayload)?
                    .track_id
                    .clone(),
                live.participant
                    .as_ref()
                    .ok_or(SignalingError::MissingPayload)?
                    .participant_id
                    .clone(),
            )),
            _ => None,
        };
        self.account(old_bytes, new_bytes)?;
        if terminal && let Some((track, participant)) = previous_source {
            remove_reference(&mut self.source_references, &track, reference);
            remove_reference(&mut self.participant_references, &participant, reference);
        } else if let Some((track, participant)) = next_source {
            self.source_references
                .entry(track)
                .or_default()
                .insert(reference);
            self.participant_references
                .entry(participant)
                .or_default()
                .insert(reference);
        }
        if let Some(target) = self.targets.get_mut(&resource) {
            if terminal && target.waiting_for == Some(reference) {
                target.waiting_for = None;
                self.dirty.insert(resource.clone());
            } else if target.reference == Some(reference) {
                if terminal {
                    target.reference = None;
                    target.blocked = true;
                    self.dirty.remove(&resource);
                } else if state.rejection.is_some()
                    && self.owned.get(&reference).is_some_and(|owned| {
                        state.evaluated_revision == owned.sent_revision
                            && specs_equal(&owned.sent, &target.spec)
                    })
                {
                    target.blocked = true;
                    self.dirty.remove(&resource);
                }
            }
        }
        if terminal {
            self.owned.remove(&reference);
            self.retiring.remove(&reference);
            self.executions.remove(&reference);
            if self.retiring_resources.get(&resource) == Some(&reference) {
                self.retiring_resources.remove(&resource);
            }
        } else if let Some(owned) = self.owned.get_mut(&reference) {
            owned.confirmed = Some(state.clone());
            if !owned.retiring
                && !owned.awaiting_evaluation()
                && self.targets.get(&resource).is_some_and(|target| {
                    target.reference == Some(reference)
                        && target.waiting_for.is_none()
                        && !target.blocked
                        && !specs_equal(&owned.sent, &target.spec)
                })
            {
                self.dirty.insert(resource.clone());
            }
        }
        Ok(Some(Change::Owned {
            resource,
            reference,
            retired_binding,
            state: Box::new(state),
            new_evaluation,
        }))
    }

    fn live(&self, reference: u64) -> Option<&wire::LiveResource> {
        match self
            .owned
            .get(&reference)?
            .confirmed
            .as_ref()?
            .value
            .as_ref()?
        {
            wire::confirmed_state::Value::Owned(live) => Some(live),
            _ => None,
        }
    }

    fn validate_source(&self, live: &wire::LiveResource) -> Result<(), SignalingError> {
        let participant = live
            .participant
            .as_ref()
            .ok_or(SignalingError::MissingPayload)?;
        let track = live.track.as_ref().ok_or(SignalingError::MissingPayload)?;
        let publication = matches!(
            live.spec
                .as_ref()
                .and_then(|spec| spec.declaration.as_ref()),
            Some(wire::resource_spec::Declaration::Publication(_))
        );
        if publication && participant != &self.self_participant
            || self
                .participants
                .get(&participant.participant_id)
                .is_some_and(|known| known != participant)
            || self
                .tracks
                .get(&track.track_id)
                .is_some_and(|known| !tracks_equal(known, track))
            || self
                .participant_references
                .get(&participant.participant_id)
                .and_then(|references| references.first())
                .and_then(|reference| self.live(*reference))
                .and_then(|live| live.participant.as_ref())
                .is_some_and(|known| known != participant)
            || self
                .source_references
                .get(&track.track_id)
                .and_then(|references| references.first())
                .and_then(|reference| self.live(*reference))
                .and_then(|live| live.track.as_ref())
                .is_some_and(|known| !tracks_equal(known, track))
        {
            return Err(SignalingError::Invalid("source identity"));
        }
        Ok(())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn target_spec(&self, resource: &Resource) -> Option<&wire::ResourceSpec> {
        self.targets.get(resource).map(|target| &target.spec)
    }

    pub(crate) fn retaining(&self, resource: &Resource) -> bool {
        self.retiring_resources.contains_key(resource)
            || self
                .targets
                .get(resource)
                .is_some_and(|target| target.reference.is_some())
    }

    pub(crate) fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.targets.keys()
    }

    pub(crate) fn owned(&self) -> impl Iterator<Item = (&Resource, &wire::LiveResource)> {
        self.targets
            .iter()
            .filter_map(|(resource, target)| Some((resource, self.live(target.reference?)?)))
    }

    pub(crate) fn source(&self, id: &str) -> Option<&wire::Track> {
        self.tracks.get(id).or_else(|| {
            let reference = self.source_references.get(id)?.first()?;
            self.live(*reference)?.track.as_ref()
        })
    }

    pub(crate) fn source_participant(&self, id: &str) -> Option<&wire::Participant> {
        if id == self.self_participant.participant_id {
            return Some(&self.self_participant);
        }
        self.participants.get(id).or_else(|| {
            let reference = self.participant_references.get(id)?.first()?;
            self.live(*reference)?.participant.as_ref()
        })
    }

    pub(crate) fn session(&self) -> Option<&wire::SessionState> {
        self.session.as_ref()
    }

    pub(crate) fn participants(&self) -> &BTreeMap<String, wire::Participant> {
        &self.participants
    }

    pub(crate) fn tracks(&self) -> &BTreeMap<String, wire::Track> {
        &self.tracks
    }

    pub(crate) fn initial_complete(&self) -> bool {
        self.initial_complete
    }

    pub(crate) fn terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn reference(&self, resource: &Resource) -> Option<u64> {
        self.targets.get(resource)?.reference
    }

    pub(crate) fn confirmed(&self, resource: &Resource) -> Option<&wire::ConfirmedState> {
        let reference = self.targets.get(resource)?.reference?;
        self.owned.get(&reference)?.confirmed.as_ref()
    }

    pub(crate) fn eligible(&self, resource: &Resource) -> Option<&wire::LiveResource> {
        if self.terminal || self.session.as_ref()?.authorization.is_none() {
            return None;
        }
        let target = self.targets.get(resource)?;
        if target.spec.settings.as_ref()?.enabled != Some(true) {
            return None;
        }
        let owned = self.owned.get(&target.reference?)?;
        let state = owned.confirmed.as_ref()?;
        if owned.retiring || state.evaluated_revision < owned.sent_revision {
            return None;
        }
        match state.value.as_ref()? {
            wire::confirmed_state::Value::Owned(live)
                if live.execution == wire::ExecutionStatus::Installed as i32
                    && live.spec.as_ref()?.settings.as_ref()?.enabled == Some(true) =>
            {
                Some(live)
            }
            _ => None,
        }
    }

    pub(crate) fn converged(&self, resource: &Resource) -> bool {
        let Some(target) = self.targets.get(resource) else {
            return false;
        };
        let Some(owned) = target
            .reference
            .and_then(|reference| self.owned.get(&reference))
        else {
            return false;
        };
        owned.confirmed.as_ref().is_some_and(|state| {
            state.evaluated_revision >= owned.sent_revision
                && state.rejection.is_none()
                && matches!(&state.value, Some(wire::confirmed_state::Value::Owned(live)) if live.spec.as_ref().is_some_and(|spec| specs_equal(spec, &target.spec)))
        })
    }

    fn next_revision(&self) -> Result<u64, SignalingError> {
        self.revision
            .checked_add(1)
            .ok_or(SignalingError::Invalid("revision exhaustion"))
    }

    fn account(&mut self, old: usize, new: usize) -> Result<(), SignalingError> {
        let next = self
            .retained_bytes
            .checked_sub(old)
            .and_then(|bytes| bytes.checked_add(new))
            .filter(|bytes| *bytes <= self.memory_limit)
            .ok_or(SignalingError::Invalid("control memory budget"))?;
        self.retained_bytes = next;
        Ok(())
    }
}

fn desired_record(
    reference: u64,
    revision: u64,
    spec: Option<wire::ResourceSpec>,
) -> wire::ClientStateRecord {
    wire::ClientStateRecord {
        owned: reference,
        execution: None,
        desired: Some(wire::DesiredState {
            revision,
            value: Some(match spec {
                Some(spec) => wire::desired_state::Value::Present(Box::new(spec)),
                None => wire::desired_state::Value::Absent(true),
            }),
        }),
    }
}

fn specs_equal(left: &wire::ResourceSpec, right: &wire::ResourceSpec) -> bool {
    validation::declarations_equivalent(left, right)
        && matches!((&left.settings, &right.settings), (Some(left), Some(right)) if validation::settings_equivalent(left, right))
}

fn tracks_equal(left: &wire::Track, right: &wire::Track) -> bool {
    left.track_id == right.track_id
        && left.participant_id == right.participant_id
        && left.label == right.label
        && match (&left.kind, &right.kind) {
            (Some(wire::track::Kind::Audio(left)), Some(wire::track::Kind::Audio(right))) => {
                matches!((&left.format, &right.format), (Some(left), Some(right)) if validation::formats_equivalent(left, right))
            }
            (Some(wire::track::Kind::Video(left)), Some(wire::track::Kind::Video(right))) => {
                matches!((&left.format, &right.format), (Some(left), Some(right)) if validation::formats_equivalent(left, right))
            }
            (Some(wire::track::Kind::Data(left)), Some(wire::track::Kind::Data(right))) => {
                left == right
            }
            _ => false,
        }
}

fn remove_reference(index: &mut BTreeMap<String, BTreeSet<u64>>, key: &str, reference: u64) {
    if let Some(references) = index.get_mut(key) {
        references.remove(&reference);
        if references.is_empty() {
            index.remove(key);
        }
    }
}

fn entry_bytes<T: Message>(key: &str, value: &T) -> usize {
    size_of::<(String, T)>()
        .saturating_add(key.len())
        .saturating_add(value.encoded_len())
}

#[cfg(test)]
mod tests;
