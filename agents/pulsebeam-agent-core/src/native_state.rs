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

use crate::{MediaSlot, SignalingError, TrackSelector};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Resource {
    Publication {
        kind: wire::TrackKind,
        track_id: String,
    },
    Reception(MediaSlot),
    DataSubscription(String),
    LogicalReception(MediaSlot),
    LogicalData(TrackSelector),
    SubscriptionSelector(u64),
}

impl Resource {
    pub(crate) fn diagnostic_kind(&self) -> &'static str {
        match self {
            Self::Publication { .. } => "publication",
            Self::Reception(_) | Self::LogicalReception(_) => "media_subscription",
            Self::DataSubscription(_) | Self::LogicalData(_) => "data_subscription",
            Self::SubscriptionSelector(_) => "subscription_selector",
        }
    }

    fn matches_spec(&self, spec: &wire::ResourceSpec) -> bool {
        match (self, spec.declaration.as_ref()) {
            (
                Self::Publication { kind, track_id },
                Some(wire::resource_spec::Declaration::Publication(publication)),
            ) => {
                track_id == &publication.track_id
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
                track == &subscription.publication_id
                    && subscription.kind == wire::TrackKind::Data as i32
            }
            (
                Self::LogicalReception(slot),
                Some(wire::resource_spec::Declaration::LogicalSubscription(subscription)),
            ) => subscription
                .source
                .as_ref()
                .is_some_and(|source| match slot {
                    MediaSlot::RemoteAudio(_) => source.kind == wire::TrackKind::Audio as i32,
                    MediaSlot::RemoteVideo(_) => source.kind == wire::TrackKind::Video as i32,
                    _ => false,
                }),
            (
                Self::LogicalData(selector),
                Some(wire::resource_spec::Declaration::LogicalSubscription(subscription)),
            ) => subscription.source.as_ref().is_some_and(|source| {
                source.kind == wire::TrackKind::Data as i32
                    && source.room == selector.room
                    && source.participant_id == selector.participant_id
                    && source.track_id == selector.track_id
            }),
            (
                Self::SubscriptionSelector(owner),
                Some(wire::resource_spec::Declaration::SubscriptionSelector(_)),
            ) => *owner != 0,
            _ => false,
        }
    }

    fn retained_bytes(&self) -> usize {
        let text = match self {
            Self::Publication { track_id, .. } => track_id.len(),
            Self::Reception(MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name))
            | Self::LogicalReception(MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name)) => {
                name.len()
            }
            Self::Reception(_) | Self::LogicalReception(_) | Self::SubscriptionSelector(_) => 0,
            Self::DataSubscription(track) => track.len(),
            Self::LogicalData(selector) => selector
                .room
                .len()
                .saturating_add(selector.participant_id.len())
                .saturating_add(selector.track_id.len()),
        };
        size_of::<Self>().saturating_add(text)
    }
}

struct Target {
    spec: wire::ResourceSpec,
    terminal_evaluation: Option<wire::ConfirmedState>,
    reference: Option<u64>,
    waiting_for: Option<u64>,
    blocked: bool,
}

impl Target {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.spec.encoded_len())
            .saturating_add(
                self.terminal_evaluation
                    .as_ref()
                    .map_or(0, Message::encoded_len),
            )
    }
}

// Reserve keyed pending-work and two bounded canonical-identity indexes per live row.
const ROW_INDEX_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    pub(crate) reference: u64,
    pub(crate) source_revision: u64,
}

struct Owned {
    resource: Resource,
    sent_revision: u64,
    sent: wire::ResourceSpec,
    confirmed: Option<wire::ConfirmedState>,
    selector_revision: u64,
    selector_assembly: Option<wire::ConfirmedState>,
    execution: Option<Box<wire::ExecutionReport>>,
    reported_offer: Option<(u64, bool)>,
    retiring: bool,
}

impl Owned {
    fn assembly_bytes(state: &wire::ConfirmedState) -> usize {
        let Some(wire::confirmed_state::Value::SubscriptionSelector(selector)) =
            state.value.as_ref()
        else {
            return 0;
        };
        let capacity = selector
            .membership
            .as_ref()
            .map_or(0, |span| span.total as usize);
        state
            .encoded_len()
            .saturating_add(capacity.saturating_mul(size_of::<wire::LogicalSourceAddress>()))
    }

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
            .saturating_add(
                self.selector_assembly
                    .as_ref()
                    .map_or(0, Self::assembly_bytes),
            )
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
        evaluates_current: bool,
    },
    Participant(ParticipantKey),
    Track(String),
    Session,
    InitialComplete,
    Catalog(Box<wire::CatalogServerRecord>),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ParticipantKey {
    pub(crate) room_id: String,
    pub(crate) participant_id: String,
}

impl core::fmt::Debug for ParticipantKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParticipantKey").finish_non_exhaustive()
    }
}

impl From<&wire::Participant> for ParticipantKey {
    fn from(participant: &wire::Participant) -> Self {
        Self {
            room_id: participant.room_id.clone(),
            participant_id: participant.participant_id.clone(),
        }
    }
}

impl From<&wire::Track> for ParticipantKey {
    fn from(track: &wire::Track) -> Self {
        Self {
            room_id: track.room_id.clone(),
            participant_id: track.participant_id.clone(),
        }
    }
}

impl ParticipantKey {
    fn retained_bytes(&self) -> usize {
        self.room_id.len().saturating_add(self.participant_id.len())
    }
}

pub(crate) struct NativeState {
    profile: validation::BindingProfile,
    revision: u64,
    targets: BTreeMap<Resource, Target>,
    owned: BTreeMap<u64, Owned>,
    dirty: BTreeSet<Resource>,
    retiring: BTreeSet<u64>,
    executions: BTreeSet<u64>,
    prefer_execution: bool,
    catalog_pending: BTreeMap<u64, wire::CatalogClientRecord>,
    catalog_first: bool,
    retiring_resources: BTreeMap<Resource, u64>,
    cursor: Option<Resource>,
    prefer_retirement: bool,
    participants: BTreeMap<ParticipantKey, wire::Participant>,
    tracks: BTreeMap<String, wire::Track>,
    source_references: BTreeMap<String, BTreeSet<u64>>,
    participant_references: BTreeMap<ParticipantKey, BTreeSet<u64>>,
    self_participant: Option<wire::Participant>,
    session: Option<wire::SessionState>,
    initial_complete: bool,
    terminal: bool,
    retained_bytes: usize,
    memory_limit: usize,
}

impl NativeState {
    pub(crate) fn new(
        memory_limit: usize,
        self_participant: impl Into<Option<wire::Participant>>,
    ) -> Result<Self, SignalingError> {
        Self::with_profile(
            memory_limit,
            self_participant,
            validation::BindingProfile::Rtc,
        )
    }

    pub(crate) fn binding_profile(&self) -> validation::BindingProfile {
        self.profile
    }

    pub(crate) fn with_profile(
        memory_limit: usize,
        self_participant: impl Into<Option<wire::Participant>>,
        profile: validation::BindingProfile,
    ) -> Result<Self, SignalingError> {
        let self_participant = self_participant.into();
        if let Some(participant) = &self_participant {
            validation::validate_participant(participant)
                .map_err(|_| SignalingError::Invalid("bootstrap participant"))?;
        }
        Ok(Self {
            profile,
            revision: 0,
            targets: BTreeMap::new(),
            owned: BTreeMap::new(),
            dirty: BTreeSet::new(),
            retiring: BTreeSet::new(),
            executions: BTreeSet::new(),
            prefer_execution: true,
            catalog_pending: BTreeMap::new(),
            catalog_first: true,
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

    pub(crate) fn decode_frame(
        &self,
        frame: &[u8],
    ) -> Result<codec::NativeFrame<wire::ServerStateBatch>, codec::DecodeError> {
        match self.profile {
            validation::BindingProfile::Rtc => codec::decode_server_frame(frame),
            validation::BindingProfile::Quic => {
                if let Some(control) = codec::decode_data_control_frame(frame) {
                    control.map(codec::NativeFrame::Data)
                } else {
                    codec::decode_quic_server(frame).map(codec::NativeFrame::State)
                }
            }
        }
    }

    fn encode_batch(&self, batch: &wire::ClientStateBatch) -> Result<Vec<u8>, codec::EncodeError> {
        match self.profile {
            validation::BindingProfile::Rtc => codec::encode_client(batch),
            validation::BindingProfile::Quic => codec::encode_quic_client(batch),
        }
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
            validation::validate_spec_for(spec, self.profile)
                .map_err(|_| SignalingError::Invalid("resource spec"))?;
            if !resource.matches_spec(spec) {
                return Err(SignalingError::Invalid("resource correlation"));
            }
            // Reserve representability with the largest revision/ref encodings.
            self.encode_batch(&wire::ClientStateBatch {
                records: alloc::vec![desired_record(u64::MAX, u64::MAX, Some(spec.clone()))],
                ..Default::default()
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
            terminal_evaluation: None,
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
            self.take_selector_assembly(reference)?;
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
            if self.cursor.as_ref() == Some(&resource) {
                self.cursor = None;
            }
        }
        Ok(())
    }

    pub(crate) fn retry_revoked_selectors(&mut self) {
        let revoked: Vec<_> = self.targets.iter().filter_map(|(resource, target)| {
            if !matches!(resource, Resource::SubscriptionSelector(_)) {
                return None;
            }
            let state = target.terminal_evaluation.as_ref()?;
            let revoked = matches!(&state.value, Some(wire::confirmed_state::Value::Absent(absent))
                if absent.reason() == wire::AbsenceReason::PermissionRevoked)
                || state.rejection.as_ref().is_some_and(|error| error.code() == wire::FailureCode::PermissionDenied);
            revoked.then(|| resource.clone())
        }).collect();
        for resource in revoked {
            self.retry(&resource);
        }
    }

    fn retry(&mut self, resource: &Resource) {
        if let Some(target) = self.targets.get_mut(resource) {
            if let Some(state) = target.terminal_evaluation.take() {
                self.retained_bytes = self.retained_bytes.saturating_sub(state.encoded_len());
            }
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
        self.queue_execution(reference, wire::execution_report::Outcome::Failure(failure))
    }

    pub(crate) fn logical_receiver_reported(&self, offer: Offer) -> bool {
        self.owned
            .get(&offer.reference)
            .and_then(|owned| owned.reported_offer)
            .is_some_and(|(revision, _)| revision == offer.source_revision)
    }

    pub(crate) fn report_logical_receiver(
        &mut self,
        resource: &Resource,
        offer: Offer,
        outcome: wire::logical_receiver_report::Outcome,
    ) -> Result<(), SignalingError> {
        let Some((current, _)) = self.logical_offer(resource) else {
            return Ok(());
        };
        if current != offer {
            return Ok(());
        }
        let failed = matches!(outcome, wire::logical_receiver_report::Outcome::Failure(_));
        if self
            .owned
            .get(&offer.reference)
            .and_then(|owned| owned.reported_offer)
            .is_some_and(|(revision, reported_failure)| {
                revision == offer.source_revision && (!failed || reported_failure)
            })
        {
            return Ok(());
        }
        self.queue_execution(
            offer.reference,
            wire::execution_report::Outcome::LogicalReceiver(wire::LogicalReceiverReport {
                source_revision: offer.source_revision,
                outcome: Some(outcome),
            }),
        )?;
        if let Some(owned) = self.owned.get_mut(&offer.reference) {
            owned.reported_offer = Some((offer.source_revision, failed));
        }
        Ok(())
    }

    fn queue_execution(
        &mut self,
        reference: u64,
        outcome: wire::execution_report::Outcome,
    ) -> Result<(), SignalingError> {
        let owned = self
            .owned
            .get(&reference)
            .ok_or(SignalingError::Invalid("owned report"))?;
        let report = Box::new(wire::ExecutionReport {
            revision: u64::MAX,
            outcome: Some(outcome),
        });
        validation::validate_execution_report_for(&report, self.profile)
            .map_err(|_| SignalingError::Invalid("execution report"))?;
        self.encode_batch(&wire::ClientStateBatch {
            records: alloc::vec![wire::ClientStateRecord {
                owned: reference,
                desired: None,
                execution: Some(report.as_ref().clone()),
            }],
            ..Default::default()
        })
        .map_err(|_| SignalingError::Encode)?;
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

    pub(crate) fn queue_catalog(
        &mut self,
        record: wire::CatalogClientRecord,
    ) -> Result<(), SignalingError> {
        self.encode_batch(&wire::ClientStateBatch {
            catalog_records: vec![record.clone()],
            ..Default::default()
        })
        .map_err(|_| SignalingError::Encode)?;
        let (id, close) = match record
            .command
            .as_ref()
            .ok_or(SignalingError::MissingPayload)?
        {
            wire::catalog_client_record::Command::Open(request) => (request.watch_id, false),
            wire::catalog_client_record::Command::Close(request) => (request.watch_id, true),
        };
        let old = self.catalog_pending.get(&id);
        let previous = old.map_or(0, Message::encoded_len);
        if close
            && old.is_some_and(|record| {
                matches!(
                    record.command,
                    Some(wire::catalog_client_record::Command::Open(_))
                )
            })
        {
            self.account(previous, 0)?;
            self.catalog_pending.remove(&id);
            return Ok(());
        }
        if self.catalog_pending.len() >= 16 && old.is_none() {
            return Err(SignalingError::Encode);
        }
        self.account(previous, record.encoded_len())?;
        self.catalog_pending.insert(id, record);
        Ok(())
    }

    fn append_catalog(&mut self, batch: &mut wire::ClientStateBatch) -> Result<(), SignalingError> {
        for _ in 0..codec::MAX_RECORDS / 2 {
            if batch
                .records
                .len()
                .saturating_add(batch.catalog_records.len())
                >= codec::MAX_RECORDS
            {
                break;
            }
            let Some((id, record)) = self.catalog_pending.pop_first() else {
                break;
            };
            let bytes = record.encoded_len();
            batch.catalog_records.push(record);
            if batch.encoded_len() > codec::MAX_FRAME_SIZE {
                if let Some(record) = batch.catalog_records.pop() {
                    self.catalog_pending.insert(id, record);
                }
                break;
            }
            self.account(bytes, 0)?;
        }
        Ok(())
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Vec<u8>>, SignalingError> {
        if self.terminal {
            return Ok(None);
        }
        let mut batch = wire::ClientStateBatch::default();
        if self.catalog_first {
            self.append_catalog(&mut batch)?;
        }
        while batch
            .records
            .len()
            .saturating_add(batch.catalog_records.len())
            < codec::MAX_RECORDS
        {
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
                let Some((owned, report)) = self
                    .owned
                    .get(&reference)
                    .and_then(|owned| Some((owned, owned.execution.as_ref()?)))
                else {
                    self.executions.remove(&reference);
                    continue;
                };
                let bytes = size_of::<wire::ExecutionReport>().saturating_add(report.encoded_len());
                if let Some(wire::execution_report::Outcome::LogicalReceiver(report)) =
                    &report.outcome
                    && self
                        .logical_offer(&owned.resource)
                        .is_none_or(|(offer, _)| {
                            offer.reference != reference
                                || offer.source_revision != report.source_revision
                        })
                {
                    self.account(bytes, 0)?;
                    if let Some(owned) = self.owned.get_mut(&reference) {
                        owned.execution = None;
                        owned.reported_offer = None;
                    }
                    self.executions.remove(&reference);
                    continue;
                }
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
                    if batch.records.is_empty() && batch.catalog_records.is_empty() {
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
                if batch.records.is_empty() && batch.catalog_records.is_empty() {
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
                selector_revision: previous.map_or(0, |owned| owned.selector_revision),
                selector_assembly: None,
                execution: previous.and_then(|owned| owned.execution.clone()),
                reported_offer: previous.and_then(|owned| owned.reported_offer),
                retiring: retirement.is_some(),
            };
            let old_bytes = previous.map_or(0, Owned::retained_bytes);
            if let Err(error) = self.account(old_bytes, next.retained_bytes()) {
                batch.records.pop();
                if batch.records.is_empty() && batch.catalog_records.is_empty() {
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
        if !self.catalog_first {
            self.append_catalog(&mut batch)?;
        }
        if batch.records.is_empty() && batch.catalog_records.is_empty() {
            return Ok(None);
        }
        self.catalog_first = !self.catalog_first;
        self.encode_batch(&batch)
            .map(Some)
            .map_err(|_| SignalingError::Encode)
    }

    #[cfg(test)]
    pub(crate) fn apply(&mut self, frame: &[u8]) -> Result<Vec<Change>, SignalingError> {
        if self.terminal {
            return Ok(Vec::new());
        }
        let codec::NativeFrame::State(batch) = self
            .decode_frame(frame)
            .map_err(|_| SignalingError::Malformed)?
        else {
            return Err(SignalingError::Malformed);
        };
        self.apply_batch(batch)
    }

    pub(crate) fn apply_batch(
        &mut self,
        batch: wire::ServerStateBatch,
    ) -> Result<Vec<Change>, SignalingError> {
        if self.terminal {
            return Ok(Vec::new());
        }
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
                    if let Some(state) = self.stage_selector_slice(reference, state)?
                        && let Some(change) = self.apply_owned(reference, state)?
                    {
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
                    if self.session.as_ref() == Some(&session) {
                        continue;
                    }
                    let old_bytes = self.session.as_ref().map_or(0, Message::encoded_len);
                    self.account(old_bytes, session.encoded_len())?;
                    self.terminal = session.closed.is_some();
                    self.session = Some(session);
                    changes.push(Change::Session);
                }
                wire::resource_key::Key::Participant(endpoint) => {
                    let id = ParticipantKey::from(&endpoint);
                    let old = self.participants.get(&id);
                    let next = match state.value {
                        Some(wire::confirmed_state::Value::Participant(participant)) => {
                            if old.is_some_and(|old| old != &participant)
                                || self
                                    .participant_references
                                    .get(&id)
                                    .and_then(|references| references.first())
                                    .and_then(|reference| self.description(*reference))
                                    .map(|(participant, _)| participant)
                                    .is_some_and(|previous| previous != &participant)
                            {
                                return Err(SignalingError::Invalid("participant identity"));
                            }
                            Some(participant)
                        }
                        Some(wire::confirmed_state::Value::Absent(_)) => None,
                        _ => return Err(SignalingError::Invalid("participant value")),
                    };
                    if old == next.as_ref() {
                        continue;
                    }
                    let bytes = |participant: &wire::Participant| {
                        id.retained_bytes()
                            .saturating_add(participant.encoded_len())
                    };
                    self.account(old.map_or(0, bytes), next.as_ref().map_or(0, bytes))?;
                    if let Some(next) = next {
                        self.participants.insert(id.clone(), next);
                    } else {
                        self.participants.remove(&id);
                    }
                    changes.push(Change::Participant(id));
                }
                wire::resource_key::Key::Publication(id) => {
                    let old = self.tracks.get(&id);
                    let next = match state.value {
                        Some(wire::confirmed_state::Value::Track(track)) => {
                            if old.is_some_and(|old| !tracks_equal(old, &track))
                                || self
                                    .source_references
                                    .get(&id)
                                    .and_then(|references| references.first())
                                    .and_then(|reference| self.source_track(*reference))
                                    .is_some_and(|previous| !tracks_equal(previous, &track))
                            {
                                return Err(SignalingError::Invalid("track identity"));
                            }
                            Some(track)
                        }
                        Some(wire::confirmed_state::Value::Absent(_)) => None,
                        _ => return Err(SignalingError::Invalid("track value")),
                    };
                    if old == next.as_ref() {
                        continue;
                    }
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
        if !self.terminal {
            changes.extend(
                batch
                    .catalog_records
                    .into_iter()
                    .map(|record| Change::Catalog(Box::new(record))),
            );
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

    fn take_selector_assembly(
        &mut self,
        reference: u64,
    ) -> Result<Option<wire::ConfirmedState>, SignalingError> {
        let assembly = self
            .owned
            .get_mut(&reference)
            .and_then(|owned| owned.selector_assembly.take());
        if let Some(assembly) = &assembly {
            self.account(Owned::assembly_bytes(assembly), 0)?;
        }
        Ok(assembly)
    }

    fn stage_selector_slice(
        &mut self,
        reference: u64,
        state: wire::ConfirmedState,
    ) -> Result<Option<wire::ConfirmedState>, SignalingError> {
        let Some(wire::confirmed_state::Value::SubscriptionSelector(slice)) = state.value.as_ref()
        else {
            self.take_selector_assembly(reference)?;
            return Ok(Some(state));
        };
        let Some(span) = slice.membership.as_ref() else {
            self.take_selector_assembly(reference)?;
            return Ok(Some(state));
        };
        let owned = self
            .owned
            .get(&reference)
            .ok_or(SignalingError::Invalid("unknown selector reference"))?;
        if owned.retiring || state.evaluated_revision < owned.sent_revision {
            return Ok(None);
        }
        if state.evaluated_revision > owned.sent_revision
            || !matches!(owned.resource, Resource::SubscriptionSelector(_))
            || !slice.spec.as_ref().is_some_and(|spec| {
                if state.rejection.is_some() {
                    validation::declarations_equivalent(spec, &owned.sent)
                } else {
                    specs_equal(spec, &owned.sent)
                }
            })
            || self
                .session
                .as_ref()
                .and_then(|session| session.authorization.as_ref())
                .is_none()
        {
            return Err(SignalingError::Invalid("selector membership authority"));
        }
        let offset = span.offset as usize;
        let total = span.total as usize;
        if total > validation::MAX_SELECTOR_MATCHES
            || offset.saturating_add(slice.matches.len()) > total
            || (total != 0 && slice.matches.is_empty())
        {
            return Err(SignalingError::Invalid("selector membership span"));
        }
        if span.revision < owned.selector_revision
            || (offset == 0 && span.revision == owned.selector_revision)
        {
            return Ok(None);
        }
        let mut assembly =
            if offset == 0 {
                self.take_selector_assembly(reference)?;
                self.owned
                    .get_mut(&reference)
                    .ok_or(SignalingError::Invalid("unknown selector reference"))?
                    .selector_revision = span.revision;
                state
            } else {
                if span.revision != owned.selector_revision {
                    self.take_selector_assembly(reference)?;
                    return Ok(None);
                }
                let Some(mut assembly) = self.take_selector_assembly(reference)? else {
                    return Ok(None);
                };
                let Some(wire::confirmed_state::Value::SubscriptionSelector(base)) =
                    &mut assembly.value
                else {
                    return Err(SignalingError::Invalid("selector assembly value"));
                };
                if base.membership.as_ref().map(|base| base.total) != Some(span.total)
                    || base.status != slice.status
                    || base.spec != slice.spec
                    || assembly.evaluated_revision != state.evaluated_revision
                    || assembly.rejection != state.rejection
                    || base.matches.len() != offset
                    || base.matches.last().zip(slice.matches.first()).is_some_and(
                        |(last, first)| selector_address_key(last) >= selector_address_key(first),
                    )
                {
                    return Ok(None);
                }
                let Some(wire::confirmed_state::Value::SubscriptionSelector(slice)) = state.value
                else {
                    return Err(SignalingError::Invalid("selector continuation value"));
                };
                base.matches.extend(slice.matches);
                assembly
            };
        let Some(wire::confirmed_state::Value::SubscriptionSelector(base)) = &mut assembly.value
        else {
            return Err(SignalingError::Invalid("selector assembly value"));
        };
        if base.matches.len() == total {
            // Framing geometry is not exposed to projection or evaluated as a new value.
            base.membership = None;
            return Ok(Some(assembly));
        }
        base.matches
            .reserve_exact(total.saturating_sub(base.matches.len()));
        self.account(0, Owned::assembly_bytes(&assembly))?;
        self.owned
            .get_mut(&reference)
            .ok_or(SignalingError::Invalid("unknown selector reference"))?
            .selector_assembly = Some(assembly);
        Ok(None)
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
        if accepted_spec(&state)
            .is_some_and(|spec| !validation::declarations_equivalent(&owned.sent, spec))
        {
            return Err(SignalingError::Invalid("accepted declaration"));
        }
        if let Some((participant, track)) = source_description(&state) {
            self.validate_description(participant, track)?;
        }
        if let Some(wire::confirmed_state::Value::LogicalSubscription(next)) = &state.value {
            if let Some(track) = &next.source {
                self.validate_source_identity(track)?;
            }
            if let Some(wire::confirmed_state::Value::LogicalSubscription(previous)) = owned
                .confirmed
                .as_ref()
                .and_then(|state| state.value.as_ref())
            {
                let aliased = previous.source.as_ref().map(|track| &track.publication_id)
                    != next.source.as_ref().map(|track| &track.publication_id)
                    || previous.data_v1 != next.data_v1;
                let revived = matches!(
                    previous.execution,
                    Some(wire::logical_subscription_state::Execution::Failure(_))
                ) && !matches!(
                    next.execution,
                    Some(wire::logical_subscription_state::Execution::Failure(_))
                );
                if next.source_revision < previous.source_revision
                    || (next.source_revision == previous.source_revision && (aliased || revived))
                {
                    return Err(SignalingError::Invalid("source offer revision"));
                }
            }
        }
        if let Some(wire::confirmed_state::Value::Owned(live)) = &state.value {
            if matches!(
                live.spec
                    .as_ref()
                    .and_then(|spec| spec.declaration.as_ref()),
                Some(wire::resource_spec::Declaration::Publication(_))
            ) && (self.self_participant.is_none() || live.participant != self.self_participant)
            {
                return Err(SignalingError::Invalid("source identity"));
            }
            if let Some(wire::confirmed_state::Value::Owned(previous)) = owned
                .confirmed
                .as_ref()
                .and_then(|state| state.value.as_ref())
                && (previous.participant != live.participant
                    || previous
                        .data_v1_source
                        .is_some_and(|source| live.data_v1_source != Some(source))
                    || !matches!((&previous.track, &live.track), (Some(previous), Some(next)) if tracks_equal(previous, next)))
            {
                return Err(SignalingError::Invalid("owned source identity"));
            }
        }
        if owned.confirmed.as_ref() == Some(&state) {
            return Ok(None);
        }
        let new_evaluation = owned
            .confirmed
            .as_ref()
            .is_none_or(|previous| previous.evaluated_revision != state.evaluated_revision);
        let evaluates_current = !owned.retiring
            && state.evaluated_revision == owned.sent_revision
            && self.targets.get(&owned.resource).is_some_and(|target| {
                target.reference == Some(reference)
                    && target.waiting_for.is_none()
                    && specs_equal(&owned.sent, &target.spec)
            });
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
        let previous_terminal_bytes = self
            .targets
            .get(&resource)
            .and_then(|target| target.terminal_evaluation.as_ref())
            .map_or(0, Message::encoded_len);
        let owned_bytes = owned.retained_bytes();
        let old_bytes = owned_bytes.saturating_add(previous_terminal_bytes);
        let new_bytes = if terminal && evaluates_current {
            state.encoded_len()
        } else if terminal {
            previous_terminal_bytes
        } else {
            old_bytes
                .saturating_sub(owned.confirmed.as_ref().map_or(0, Message::encoded_len))
                .saturating_add(state.encoded_len())
        };
        let identity = |(participant, track): (&wire::Participant, &wire::Track)| {
            (
                track.publication_id.clone(),
                ParticipantKey::from(participant),
            )
        };
        let previous_source = owned
            .confirmed
            .as_ref()
            .and_then(source_description)
            .map(identity);
        let next_source = source_description(&state).map(identity);
        let previous_offer = owned
            .confirmed
            .as_ref()
            .and_then(logical_state)
            .and_then(|state| state.source.as_ref())
            .map(|track| track.publication_id.clone());
        let next_offer = logical_state(&state)
            .and_then(|state| state.source.as_ref())
            .map(|track| track.publication_id.clone());
        self.account(old_bytes, new_bytes)?;
        if previous_source != next_source {
            if let Some((track, participant)) = previous_source {
                remove_reference(&mut self.source_references, &track, reference);
                remove_reference(&mut self.participant_references, &participant, reference);
            }
            if let Some((track, participant)) = next_source {
                self.source_references
                    .entry(track)
                    .or_default()
                    .insert(reference);
                self.participant_references
                    .entry(participant)
                    .or_default()
                    .insert(reference);
            }
        }
        if previous_offer != next_offer {
            if let Some(track) = previous_offer {
                remove_reference(&mut self.source_references, &track, reference);
            }
            if let Some(track) = next_offer {
                self.source_references
                    .entry(track)
                    .or_default()
                    .insert(reference);
            }
        }
        if let Some(target) = self.targets.get_mut(&resource) {
            if terminal && target.waiting_for == Some(reference) {
                target.waiting_for = None;
                self.dirty.insert(resource.clone());
            } else if target.reference == Some(reference) {
                if terminal {
                    if evaluates_current {
                        target.terminal_evaluation = Some(state.clone());
                    }
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
            evaluates_current,
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

    fn description(&self, reference: u64) -> Option<(&wire::Participant, &wire::Track)> {
        source_description(self.owned.get(&reference)?.confirmed.as_ref()?)
    }

    fn source_track(&self, reference: u64) -> Option<&wire::Track> {
        let state = self.owned.get(&reference)?.confirmed.as_ref()?;
        logical_state(state)
            .and_then(|state| state.source.as_ref())
            .or_else(|| source_description(state).map(|(_, track)| track))
    }

    fn validate_source_identity(&self, track: &wire::Track) -> Result<(), SignalingError> {
        if self
            .source(&track.publication_id)
            .is_some_and(|known| !tracks_equal(known, track))
        {
            return Err(SignalingError::Invalid("source identity"));
        }
        Ok(())
    }

    fn validate_description(
        &self,
        participant: &wire::Participant,
        track: &wire::Track,
    ) -> Result<(), SignalingError> {
        let endpoint = ParticipantKey::from(participant);
        if self
            .participants
            .get(&endpoint)
            .is_some_and(|known| known != participant)
            || self
                .participant_references
                .get(&endpoint)
                .and_then(|references| references.first())
                .and_then(|reference| self.description(*reference))
                .map(|(participant, _)| participant)
                .is_some_and(|known| known != participant)
        {
            return Err(SignalingError::Invalid("source identity"));
        }
        self.validate_source_identity(track)
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
            self.source_track(*reference)
        })
    }

    pub(crate) fn source_participant(&self, id: &ParticipantKey) -> Option<&wire::Participant> {
        if let Some(participant) = self.self_participant.as_ref()
            && id.room_id == participant.room_id
            && id.participant_id == participant.participant_id
        {
            return Some(participant);
        }
        self.participants.get(id).or_else(|| {
            let reference = self.participant_references.get(id)?.first()?;
            Some(self.description(*reference)?.0)
        })
    }

    pub(crate) fn session(&self) -> Option<&wire::SessionState> {
        self.session.as_ref()
    }

    pub(crate) fn participants(&self) -> &BTreeMap<ParticipantKey, wire::Participant> {
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

    pub(crate) fn resource_for_reference(&self, reference: u64) -> Option<&Resource> {
        let resource = &self.owned.get(&reference)?.resource;
        (self.reference(resource) == Some(reference)).then_some(resource)
    }

    pub(crate) fn confirmed(&self, resource: &Resource) -> Option<&wire::ConfirmedState> {
        let target = self.targets.get(resource)?;
        match target.reference {
            Some(reference) => self.owned.get(&reference)?.confirmed.as_ref(),
            None => target.terminal_evaluation.as_ref(),
        }
    }

    pub(crate) fn evaluation(&self, resource: &Resource) -> Option<&wire::ConfirmedState> {
        let target = self.targets.get(resource)?;
        let Some(reference) = target.reference else {
            return target.terminal_evaluation.as_ref();
        };
        let owned = self.owned.get(&reference)?;
        let state = owned.confirmed.as_ref()?;
        (specs_equal(&target.spec, &owned.sent) && state.evaluated_revision >= owned.sent_revision)
            .then_some(state)
    }

    pub(crate) fn selector_snapshot(&self, owner: u64) -> Option<&wire::SubscriptionSelectorState> {
        if self.terminal || self.session.as_ref()?.authorization.is_none() {
            return None;
        }
        let target = self.targets.get(&Resource::SubscriptionSelector(owner))?;
        if target.spec.settings.as_ref()?.enabled != Some(true) {
            return None;
        }
        let owned = self.owned.get(&target.reference?)?;
        if owned.retiring || owned.awaiting_evaluation() {
            return None;
        }
        let state = owned.confirmed.as_ref()?;
        match state.value.as_ref()? {
            wire::confirmed_state::Value::SubscriptionSelector(snapshot)
                if state.rejection.is_none()
                    && snapshot.spec.as_ref()?.settings.as_ref()?.enabled == Some(true) =>
            {
                Some(snapshot)
            }
            _ => None,
        }
    }

    pub(crate) fn logical_offer(
        &self,
        resource: &Resource,
    ) -> Option<(Offer, &wire::LogicalSubscriptionState)> {
        if self.terminal || self.session.as_ref()?.authorization.is_none() {
            return None;
        }
        let target = self.targets.get(resource)?;
        if target.spec.settings.as_ref()?.enabled != Some(true) {
            return None;
        }
        let reference = target.reference?;
        let owned = self.owned.get(&reference)?;
        let state = logical_state(owned.confirmed.as_ref()?)?;
        if owned.retiring
            || state.source.is_none()
            || matches!(
                state.execution,
                Some(wire::logical_subscription_state::Execution::Failure(_))
            )
        {
            return None;
        }
        Some((
            Offer {
                reference,
                source_revision: state.source_revision,
            },
            state,
        ))
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
                && accepted_spec(state).is_some_and(|spec| specs_equal(spec, &target.spec))
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

fn accepted_spec(state: &wire::ConfirmedState) -> Option<&wire::ResourceSpec> {
    match state.value.as_ref()? {
        wire::confirmed_state::Value::Owned(live) => live.spec.as_ref(),
        wire::confirmed_state::Value::SubscriptionSelector(selector) => selector.spec.as_ref(),
        wire::confirmed_state::Value::LogicalSubscription(subscription) => {
            subscription.spec.as_ref()
        }
        _ => None,
    }
}

pub(crate) fn logical_state(
    state: &wire::ConfirmedState,
) -> Option<&wire::LogicalSubscriptionState> {
    match state.value.as_ref()? {
        wire::confirmed_state::Value::LogicalSubscription(state) => Some(state),
        _ => None,
    }
}

fn source_description(state: &wire::ConfirmedState) -> Option<(&wire::Participant, &wire::Track)> {
    match state.value.as_ref()? {
        wire::confirmed_state::Value::Owned(live) => {
            Some((live.participant.as_ref()?, live.track.as_ref()?))
        }
        _ => None,
    }
}

fn selector_address_key(address: &wire::LogicalSourceAddress) -> (&str, &str, i32, &str) {
    (
        &address.room,
        &address.participant_id,
        address.kind,
        &address.track_id,
    )
}

fn specs_equal(left: &wire::ResourceSpec, right: &wire::ResourceSpec) -> bool {
    validation::declarations_equivalent(left, right)
        && matches!((&left.settings, &right.settings), (Some(left), Some(right)) if validation::settings_equivalent(left, right))
}

fn tracks_equal(left: &wire::Track, right: &wire::Track) -> bool {
    left.publication_id == right.publication_id
        && left.room_id == right.room_id
        && left.participant_id == right.participant_id
        && left.track_id == right.track_id
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

fn remove_reference<K: Ord>(index: &mut BTreeMap<K, BTreeSet<u64>>, key: &K, reference: u64) {
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
