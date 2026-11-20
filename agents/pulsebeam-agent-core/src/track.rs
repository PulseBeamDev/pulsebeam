use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    vec::Vec,
};

use crate::{
    MediaKind, PlayoutDelay, PublicationIntent, TopicChannel, TopicMode, TopicNotification,
    TopicPublisher, TopicRegistrations, TopicSubscriber, TrackSelector, ValidationError,
    VideoSubscription, model::validate_identifier,
};

mod observation;
pub use observation::{MediaObservationControl, MediaObservationId, MediaObservationTarget};

pub const DATA_RECEIVE_CAPACITY: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrackSpec {
    Video {
        track_id: String,
    },
    Audio {
        track_id: String,
    },
    Data {
        track_id: String,
        delivery: TopicMode,
    },
}

impl TrackSpec {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let track_id = match self {
            Self::Audio { track_id } | Self::Video { track_id } | Self::Data { track_id, .. } => {
                track_id
            }
        };
        crate::model::validate_track_id(track_id)
    }

    pub(crate) fn from_state(track: &pulsebeam_proto::state::Track) -> Option<Self> {
        use pulsebeam_proto::state::track::Kind;
        let track_id = track.track_id.clone();
        Some(match track.kind.as_ref()? {
            Kind::Audio(_) => Self::Audio { track_id },
            Kind::Video(_) => Self::Video { track_id },
            Kind::Data(format) => Self::Data {
                track_id,
                delivery: TopicMode::from_delivery(format.delivery)?,
            },
        })
    }

    pub fn track_id(&self) -> &str {
        match self {
            Self::Video { track_id } | Self::Audio { track_id } | Self::Data { track_id, .. } => {
                track_id
            }
        }
    }

    pub const fn media_kind(&self) -> Option<MediaKind> {
        match self {
            Self::Video { .. } => Some(MediaKind::Video),
            Self::Audio { .. } => Some(MediaKind::Audio),
            Self::Data { .. } => None,
        }
    }

    pub(crate) fn publisher(&self) -> Result<TopicPublisher, TrackResourceError> {
        self.validate()?;
        match self {
            Self::Data { track_id, delivery } => Ok(TopicPublisher {
                topic: track_id.clone(),
                mode: *delivery,
            }),
            _ => Err(TrackResourceError::WrongKind),
        }
    }

    pub(crate) fn subscriber(
        &self,
        source: &TrackSelector,
    ) -> Result<TopicSubscriber, TrackResourceError> {
        source.validate()?;
        let publisher = self.publisher()?;
        if publisher.topic != source.track_id {
            return Err(ValidationError::Identifier {
                field: "data source track_id",
            }
            .into());
        }
        Ok(TopicSubscriber {
            room: source.room.clone(),
            participant_id: source.participant_id.clone(),
            topic: publisher.topic,
            mode: publisher.mode,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTrack {
    pub id: String,
    pub participant_id: String,
    pub spec: TrackSpec,
}

/// One declaration or stream lifetime, including a fresh token for Strict Mode replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackOwner(pub(crate) u64);

impl TrackOwner {
    pub const fn id(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrackResourceError {
    #[error(transparent)]
    InvalidSpec(#[from] ValidationError),
    #[error("track kind does not support this operation")]
    WrongKind,
    #[error("data send requires connected intent")]
    NotConnected,
    #[error("resource already has an exclusive owner")]
    AlreadyOwned,
    #[error("owned publication capacity exceeded")]
    PublicationCapacityExceeded,
    #[error("owner already owns another resource")]
    OwnerInUse,
    #[error("resource owner identity space exhausted")]
    OwnerSpaceExhausted,
    #[error("publication input operation identity space exhausted")]
    InputOperationSpaceExhausted,
    #[error("publication configuration revision space exhausted")]
    PublicationConfigurationSpaceExhausted,
    #[error("{kind:?} physical coordinate space exhausted")]
    SlotSpaceExhausted { kind: MediaKind },
    #[error("owner no longer owns this resource")]
    Released,
    #[error("playout delay minimum exceeds maximum")]
    InvalidPlayoutDelay,
    #[error("data receive buffer cannot represent all publisher tracks")]
    ReceiveOverflow,
    #[error("data receive channel failed: {0}")]
    ReceiveChannelFailed(String),
    #[error("data reception failed: {0:?}")]
    ReceiveFailed(crate::ffi::ErrorInfo),
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DataEvent {
    Message {
        participant_id: String,
        publication_id: String,
        sequence: u64,
        data: Vec<u8>,
    },
    Gap {
        participant_id: String,
        publication_id: String,
        from: u64,
        to: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DataUpdate {
    Event { event: DataEvent },
    Closed,
}

impl DataEvent {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Message {
                participant_id,
                publication_id,
                ..
            }
            | Self::Gap {
                participant_id,
                publication_id,
                ..
            } => (participant_id, publication_id),
        }
    }

    fn from(&self) -> u64 {
        match self {
            Self::Message { sequence, .. } => *sequence,
            Self::Gap { from, .. } => *from,
        }
    }

    fn end(&self) -> Option<u64> {
        match self {
            Self::Message { sequence, .. } => sequence.checked_add(1),
            Self::Gap { to, .. } => Some(*to),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoObservation {
    pub visible: bool,
    pub height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
    pub playout_delay: PlayoutDelay,
}

struct VideoView {
    selector: TrackSelector,
    observation: VideoObservation,
}

enum AudioView {
    Named {
        selector: TrackSelector,
        playout_delay: PlayoutDelay,
    },
    Policy(crate::AudioSubscription),
}

impl AudioView {
    fn has_fixed_delay(&self) -> bool {
        match self {
            Self::Named { playout_delay, .. } => *playout_delay != PlayoutDelay::Adaptive,
            Self::Policy(policy) => policy
                .playout_delays
                .values()
                .chain(policy.selector_delays.values())
                .any(|delay| *delay != PlayoutDelay::Adaptive),
        }
    }
}

#[derive(Default)]
pub struct ReceiveUpdate {
    pub ready: Vec<TrackOwner>,
    pub registrations_changed: bool,
}

/// A logical input lifetime, independent of registration work and RTP bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationInputId {
    owner: TrackOwner,
    id: TrackOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationInputIntent {
    Replace,
    ConfigureCurrent,
    Retain(PublicationInputId),
}

/// Core-issued registration work. Reconfiguration preserves the input lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationInputOperation {
    owner: TrackOwner,
    input: Option<PublicationInputId>,
    revision: u64,
    slot: String,
    present: bool,
}

impl PublicationInputOperation {
    pub const fn owner(&self) -> u64 {
        self.owner.id()
    }

    pub fn slot(&self) -> &str {
        &self.slot
    }

    pub const fn input(&self) -> Option<PublicationInputId> {
        self.input
    }
}

/// Apply to the owner's registered input, not to a borrowed capture device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationInputState {
    pub owner: u64,
    pub input: Option<PublicationInputId>,
    pub slot: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublicationConfigurationState {
    pub desired_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: u64,
    pub desired_enabled: bool,
    pub accepted_enabled: bool,
    pub rejected_revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationInputTarget {
    pub generation: crate::Generation,
    pub binding: crate::SlotBinding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationInputControl {
    pub(crate) effect: Option<crate::OperationId>,
    pub operation: PublicationInputOperation,
    pub enabled: bool,
    pub target: Option<PublicationInputTarget>,
}

impl PublicationInputControl {
    pub fn state(&self) -> PublicationInputState {
        PublicationInputState {
            owner: self.operation.owner(),
            input: self.operation.input(),
            slot: self.operation.slot().into(),
            enabled: self.enabled,
        }
    }
}

struct PublicationOwner {
    owner: TrackOwner,
    slot: String,
    input: Option<PublicationInputId>,
    input_revision: u64,
    pending_input: bool,
    input_error: Option<crate::ffi::ErrorInfo>,
    muted: bool,
    last_control: Option<PublicationInputControl>,
    control_completed: bool,
    desired_revision: u64,
    evaluated_revision: u64,
    accepted_revision: u64,
    accepted_enabled: bool,
    rejected_revision: Option<u64>,
    last_control_configuration_revision: Option<u64>,
    last_control_accepted_revision: u64,
    last_control_accepted_enabled: bool,
    // Publication declaration is independent of current input availability.
    // Input loss disables the stream; only owner release ends its lifetime.
    present: Option<bool>,
}

impl PublicationOwner {
    fn input_enabled(&self) -> bool {
        let enabled = if self.desired_revision > self.evaluated_revision {
            !self.muted
        } else {
            self.accepted_enabled
        };
        self.present == Some(true) && enabled && !self.pending_input
    }

    fn input_state(&self) -> PublicationInputState {
        PublicationInputState {
            owner: self.owner.id(),
            input: self.input,
            slot: self.slot.clone(),
            enabled: self.input_enabled(),
        }
    }

    fn settle_configuration_without_input(&mut self) {
        if self.input.is_none() {
            self.evaluated_revision = self.desired_revision;
            self.accepted_revision = self.desired_revision;
            self.accepted_enabled = !self.muted;
            self.rejected_revision = None;
        }
    }
}

struct DataReceiver {
    subscriber: TopicSubscriber,
    queue: VecDeque<DataEvent>,
    failure: Option<TrackResourceError>,
}

impl DataReceiver {
    fn push(&mut self, event: DataEvent) {
        if self.failure.is_some() || self.subscriber.participant_id != event.identity().0 {
            return;
        }
        match self.subscriber.mode {
            TopicMode::Latest => {
                self.queue
                    .retain(|queued| queued.identity() != event.identity());
            }
            TopicMode::Lossy if self.queue.len() == DATA_RECEIVE_CAPACITY => {
                self.queue.pop_front();
            }
            TopicMode::Lossy | TopicMode::Ordered => {}
        }
        self.queue.push_back(event);
        if self.queue.len() <= DATA_RECEIVE_CAPACITY {
            return;
        }
        if self.subscriber.mode != TopicMode::Ordered || !self.compact_ordered() {
            self.fail(TrackResourceError::ReceiveOverflow);
        }
    }

    fn compact_ordered(&mut self) -> bool {
        let mut first_by_track = BTreeMap::new();
        for (second, later) in self.queue.iter().enumerate() {
            let identity = later.identity();
            let Some(first) = first_by_track.get(&identity).copied() else {
                first_by_track.insert(identity, second);
                continue;
            };
            let Some(earlier) = self.queue.get(first) else {
                return false;
            };
            let Some(earlier_end) = earlier.end() else {
                return false;
            };
            let Some(later_end) = later.end() else {
                return false;
            };
            let gap = DataEvent::Gap {
                participant_id: identity.0.into(),
                publication_id: identity.1.into(),
                from: earlier.from(),
                to: earlier_end.max(later_end),
            };
            let Some(earlier) = self.queue.get_mut(first) else {
                return false;
            };
            *earlier = gap;
            self.queue.remove(second);
            return true;
        }
        false
    }

    fn fail(&mut self, error: TrackResourceError) {
        self.queue.clear();
        self.failure = Some(error);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceCommand {
    ClaimPublication {
        spec: TrackSpec,
    },
    DeclareDataPublication {
        spec: TrackSpec,
    },
    UpdatePublication {
        owner: u64,
        present: bool,
    },
    AttachPublicationInput {
        owner: u64,
    },
    DetachPublicationInput {
        input: PublicationInputId,
    },
    BeginPublicationInput {
        owner: u64,
        input: PublicationInputIntent,
        present: bool,
        muted: Option<bool>,
    },
    AuthorizePublicationInput {
        operation: PublicationInputOperation,
    },
    CompletePublicationInput {
        operation: PublicationInputOperation,
        result: Result<(), crate::ffi::ErrorCode>,
    },
    CompletePublicationInputTarget {
        control: PublicationInputControl,
        result: Result<(), crate::ffi::ErrorCode>,
    },
    SetPublicationMuted {
        owner: u64,
        muted: bool,
    },
    ClaimVideo {
        selector: TrackSelector,
    },
    ObserveVideo {
        owner: u64,
        observation: VideoObservation,
    },
    ClaimAudio,
    ClaimAudioSubscription {
        selector: TrackSelector,
    },
    ConfigureAudio {
        owner: u64,
        playout_delay: PlayoutDelay,
    },
    ClaimAudioPolicy {
        policy: crate::AudioSubscription,
    },
    ConfigureAudioPolicy {
        owner: u64,
        policy: crate::AudioSubscription,
    },
    BeginMediaObservation {
        owner: u64,
    },
    BeginAudioSourceObservation {
        owner: u64,
        source: String,
    },
    AuthorizeMediaObservation {
        control: MediaObservationControl,
    },
    CompleteMediaObservation {
        control: MediaObservationControl,
        succeeded: bool,
    },
    EndMediaObservation {
        control: MediaObservationControl,
    },
    RetainProducer {
        spec: TrackSpec,
    },
    ReceiveData {
        spec: TrackSpec,
        source: TrackSelector,
    },
    ReadData {
        owner: u64,
    },
    Release {
        owner: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceResult {
    Publication {
        owner: u64,
        slot: String,
        /// Earlier sessions retain an incompatible wire binding for this slot.
        after_generation: Option<crate::Generation>,
    },
    PublicationInput {
        operation: PublicationInputOperation,
        state: PublicationInputState,
    },
    PublicationInputState(PublicationInputState),
    MediaObservation(MediaObservationId),
    Superseded,
    Owned {
        owner: u64,
    },
    Event {
        event: Option<DataEvent>,
    },
    Updated,
}

/// Portable resource policy. Browser objects and pending promises remain in the host.
#[derive(Default)]
pub struct TrackResources {
    next_owner: u64,
    publications: BTreeMap<TrackSpec, PublicationOwner>,
    data_publications: BTreeMap<String, (TrackOwner, TopicMode)>,
    audio: Option<TrackOwner>,
    audio_views: BTreeMap<TrackOwner, AudioView>,
    video: BTreeMap<TrackOwner, VideoView>,
    video_slots: BTreeMap<TrackSelector, u32>,
    observations: BTreeMap<MediaObservationId, observation::MediaObservation>,
    producers: BTreeSet<TopicPublisher>,
    receivers: BTreeMap<TrackOwner, DataReceiver>,
    intent_revisions: BTreeMap<TrackOwner, u64>,
}

impl TrackResources {
    fn begin_intent(&mut self, owner: TrackOwner) -> Result<u64, TrackResourceError> {
        let revision = self
            .intent_revisions
            .get(&owner)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(TrackResourceError::PublicationConfigurationSpaceExhausted)?;
        self.intent_revisions.insert(owner, revision);
        Ok(revision)
    }

    pub(crate) fn intent_revision(&self, owner: TrackOwner) -> Option<u64> {
        self.intent_revisions.get(&owner).copied()
    }

    pub(crate) fn video_view(
        &self,
        owner: TrackOwner,
    ) -> Option<(&TrackSelector, &VideoObservation)> {
        let view = self.video.get(&owner)?;
        Some((&view.selector, &view.observation))
    }

    pub(crate) fn audio_view(&self, owner: TrackOwner) -> Option<(&TrackSelector, PlayoutDelay)> {
        match self.audio_views.get(&owner)? {
            AudioView::Named {
                selector,
                playout_delay,
            } => Some((selector, *playout_delay)),
            AudioView::Policy(_) => None,
        }
    }

    pub(crate) fn data_publication(&self, owner: TrackOwner) -> Option<(&str, TopicMode)> {
        self.data_publications
            .iter()
            .find_map(|(track_id, (current, mode))| {
                (*current == owner).then_some((track_id.as_str(), *mode))
            })
    }

    pub(crate) fn data_receiver(
        &self,
        owner: TrackOwner,
    ) -> Option<(&TopicSubscriber, Option<&TrackResourceError>)> {
        let receiver = self.receivers.get(&owner)?;
        Some((&receiver.subscriber, receiver.failure.as_ref()))
    }

    pub fn new_owner(&mut self) -> Result<TrackOwner, TrackResourceError> {
        self.next_owner = self
            .next_owner
            .checked_add(1)
            .ok_or(TrackResourceError::OwnerSpaceExhausted)?;
        Ok(TrackOwner(self.next_owner))
    }

    pub fn claim_publication(
        &mut self,
        owner: TrackOwner,
        spec: TrackSpec,
        occupied: &BTreeSet<String>,
    ) -> Result<String, TrackResourceError> {
        spec.validate()?;
        let kind = spec.media_kind().ok_or(TrackResourceError::WrongKind)?;
        if let Some(existing) = self.publications.get(&spec) {
            return if existing.owner == owner {
                Ok(existing.slot.clone())
            } else {
                Err(TrackResourceError::AlreadyOwned)
            };
        }
        if self.publications.values().any(|entry| entry.owner == owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        if self.owned_publication_capacity_exceeded() {
            return Err(TrackResourceError::PublicationCapacityExceeded);
        }
        let prefix = match kind {
            MediaKind::Video => 'v',
            MediaKind::Audio => 'a',
        };
        let slot = (0..=u64::MAX)
            .map(|index| format!("{prefix}{index}"))
            .find(|slot| {
                !occupied.contains(slot)
                    && !self.publications.values().any(|entry| &entry.slot == slot)
            })
            .ok_or(TrackResourceError::SlotSpaceExhausted { kind })?;
        self.publications.insert(
            spec,
            PublicationOwner {
                owner,
                slot: slot.clone(),
                input: None,
                input_revision: 0,
                pending_input: false,
                input_error: None,
                last_control: None,
                control_completed: false,
                muted: false,
                desired_revision: 1,
                evaluated_revision: 1,
                accepted_revision: 1,
                accepted_enabled: true,
                rejected_revision: None,
                last_control_configuration_revision: None,
                last_control_accepted_revision: 1,
                last_control_accepted_enabled: true,
                present: None,
            },
        );
        Ok(slot)
    }

    pub(crate) fn owned_publication_capacity_exceeded(&self) -> bool {
        self.publications
            .len()
            .checked_add(self.data_publications.len())
            .is_none_or(|count| count >= 64)
    }

    pub(crate) fn validate_data_publication_claim(
        &self,
        spec: &TrackSpec,
    ) -> Result<(), TrackResourceError> {
        spec.validate()?;
        let TrackSpec::Data { track_id, .. } = spec else {
            return Err(TrackResourceError::WrongKind);
        };
        if self.data_publications.contains_key(track_id) {
            return Err(TrackResourceError::AlreadyOwned);
        }
        if self.owned_publication_capacity_exceeded() {
            return Err(TrackResourceError::PublicationCapacityExceeded);
        }
        Ok(())
    }

    pub(crate) fn declare_data_publication(
        &mut self,
        owner: TrackOwner,
        spec: &TrackSpec,
    ) -> Result<(), TrackResourceError> {
        spec.validate()?;
        let TrackSpec::Data { track_id, delivery } = spec else {
            return Err(TrackResourceError::WrongKind);
        };
        if let Some((current, mode)) = self.data_publications.get(track_id) {
            return if *current == owner && *mode == *delivery {
                Ok(())
            } else {
                Err(TrackResourceError::AlreadyOwned)
            };
        }
        self.validate_data_publication_claim(spec)?;
        if self.publications.values().any(|entry| entry.owner == owner)
            || self
                .data_publications
                .values()
                .any(|(current, _)| *current == owner)
        {
            return Err(TrackResourceError::OwnerInUse);
        }
        self.begin_intent(owner)?;
        self.data_publications
            .insert(track_id.clone(), (owner, *delivery));
        Ok(())
    }

    pub fn update_publication(
        &mut self,
        owner: TrackOwner,
        spec: &TrackSpec,
        present: bool,
    ) -> Result<(), TrackResourceError> {
        let entry = self
            .publications
            .get_mut(spec)
            .filter(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        entry.input_revision = entry
            .input_revision
            .checked_add(1)
            .ok_or(TrackResourceError::InputOperationSpaceExhausted)?;
        entry.input = None;
        entry.pending_input = false;
        entry.settle_configuration_without_input();
        if present || entry.present.is_some() {
            entry.present = Some(present);
        }
        Ok(())
    }

    pub(crate) fn attach_publication_input(
        &mut self,
        owner: TrackOwner,
    ) -> Result<(PublicationInputOperation, PublicationInputState), TrackResourceError> {
        let entry = self
            .publications
            .values()
            .find(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        if entry.input.is_some() {
            return Err(TrackResourceError::AlreadyOwned);
        }
        let muted = entry.muted;
        self.begin_publication_input(owner, PublicationInputIntent::Replace, true, muted)
    }

    pub(crate) fn publication_input_state(
        &self,
        input: PublicationInputId,
    ) -> Option<PublicationInputState> {
        self.publications
            .values()
            .find(|entry| entry.input == Some(input))
            .map(PublicationOwner::input_state)
    }

    pub(crate) fn pipe_state(&self, input: PublicationInputId) -> Option<crate::ffi::PipeState> {
        let entry = self
            .publications
            .values()
            .find(|entry| entry.input == Some(input))?;
        Some(crate::ffi::PipeState {
            observation_revision: 0,
            status: if entry.pending_input {
                crate::ffi::PipeStatus::Pending
            } else if entry.present == Some(true) {
                crate::ffi::PipeStatus::Available
            } else {
                crate::ffi::PipeStatus::Unavailable
            },
            error: entry.input_error.clone(),
        })
    }

    pub(crate) fn detach_publication_input(
        &mut self,
        input: PublicationInputId,
    ) -> Option<(PublicationInputOperation, PublicationInputState)> {
        let entry = self
            .publications
            .values_mut()
            .find(|entry| entry.input == Some(input))?;
        // Removing the lifetime fences earlier work without allocating another revision.
        // Cleanup must remain possible when registration counters are exhausted.
        entry.input = None;
        entry.pending_input = true;
        entry.present = entry.present.map(|_| false);
        entry.settle_configuration_without_input();
        let operation = PublicationInputOperation {
            owner: entry.owner,
            input: None,
            revision: entry.input_revision,
            slot: entry.slot.clone(),
            present: false,
        };
        Some((operation, entry.input_state()))
    }

    pub(crate) fn begin_publication_input(
        &mut self,
        owner: TrackOwner,
        input: PublicationInputIntent,
        present: bool,
        muted: bool,
    ) -> Result<(PublicationInputOperation, PublicationInputState), TrackResourceError> {
        let entry = self
            .publications
            .values()
            .find(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        let revision = entry
            .input_revision
            .checked_add(1)
            .ok_or(TrackResourceError::InputOperationSpaceExhausted)?;
        let configuration_revision = if entry.muted != muted {
            Some(
                entry
                    .desired_revision
                    .checked_add(1)
                    .ok_or(TrackResourceError::PublicationConfigurationSpaceExhausted)?,
            )
        } else {
            None
        };
        let retained = match input {
            PublicationInputIntent::Replace => None,
            PublicationInputIntent::ConfigureCurrent => entry.input,
            PublicationInputIntent::Retain(input) => {
                if input.owner != owner || entry.input != Some(input) {
                    return Err(TrackResourceError::Released);
                }
                Some(input)
            }
        };
        let input = if present {
            Some(match retained {
                Some(input) => input,
                None => PublicationInputId {
                    owner,
                    id: self.new_owner()?,
                },
            })
        } else {
            None
        };
        let entry = self
            .publications
            .values_mut()
            .find(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        if entry.input != input {
            entry.input_error = None;
        }
        entry.input = input;
        entry.input_revision = revision;
        entry.pending_input = true;
        if let Some(configuration_revision) = configuration_revision {
            entry.muted = muted;
            entry.desired_revision = configuration_revision;
        }
        entry.settle_configuration_without_input();
        entry.present = entry.present.map(|_| false);
        let operation = PublicationInputOperation {
            owner,
            input,
            revision,
            slot: entry.slot.clone(),
            present,
        };
        Ok((operation, entry.input_state()))
    }

    pub(crate) fn publication_configuration(
        &self,
        owner: TrackOwner,
    ) -> Result<PublicationConfigurationState, TrackResourceError> {
        let entry = self
            .publications
            .values()
            .find(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        Ok(PublicationConfigurationState {
            desired_revision: entry.desired_revision,
            evaluated_revision: entry.evaluated_revision,
            accepted_revision: entry.accepted_revision,
            desired_enabled: !entry.muted,
            accepted_enabled: entry.accepted_enabled,
            rejected_revision: entry.rejected_revision,
        })
    }

    pub(crate) fn publication_muted(&self, owner: TrackOwner) -> Result<bool, TrackResourceError> {
        self.publications
            .values()
            .find(|entry| entry.owner == owner)
            .map(|entry| entry.muted)
            .ok_or(TrackResourceError::Released)
    }

    pub(crate) fn publication_input_current(&self, operation: &PublicationInputOperation) -> bool {
        self.publications.values().any(|entry| {
            entry.owner == operation.owner
                && entry.input == operation.input
                && entry.input_revision == operation.revision
        })
    }

    pub(crate) fn complete_publication_input(
        &mut self,
        operation: &PublicationInputOperation,
        result: Result<(), crate::ffi::ErrorCode>,
    ) -> Option<PublicationInputState> {
        let entry = self.publications.values_mut().find(|entry| {
            entry.owner == operation.owner
                && entry.input == operation.input
                && entry.input_revision == operation.revision
                && entry.pending_input
        })?;
        entry.pending_input = false;
        let present = result.is_ok() && operation.present;
        entry.input_error = result
            .err()
            .map(|code| crate::ffi::ErrorInfo::new(code, "Input registration failed"));
        if present || entry.present.is_some() {
            entry.present = Some(present);
        }
        Some(entry.input_state())
    }

    pub(crate) fn publication_inputs(&self) -> impl Iterator<Item = PublicationInputControl> + '_ {
        self.publications
            .values()
            .filter(|entry| {
                entry.input.is_some() || entry.pending_input || entry.last_control.is_some()
            })
            .map(|entry| PublicationInputControl {
                effect: entry
                    .last_control
                    .as_ref()
                    .and_then(|control| control.effect),
                operation: PublicationInputOperation {
                    owner: entry.owner,
                    input: entry.input,
                    revision: entry.input_revision,
                    slot: entry.slot.clone(),
                    present: entry.input.is_some(),
                },
                enabled: entry.input_enabled(),
                target: None,
            })
    }

    pub(crate) fn input_control_current(&self, control: &PublicationInputControl) -> bool {
        self.publications
            .values()
            .find(|entry| entry.owner.id() == control.operation.owner())
            .map_or_else(
                || {
                    control.operation.input().is_none()
                        && !control.enabled
                        && control.target.is_none()
                },
                |entry| entry.last_control.as_ref() == Some(control),
            )
    }

    pub(crate) fn input_control_changed(&self, control: &PublicationInputControl) -> bool {
        self.publications
            .values()
            .find(|entry| entry.owner.id() == control.operation.owner())
            .is_some_and(|entry| entry.last_control.as_ref() != Some(control))
    }

    pub(crate) fn update_input_control(&mut self, control: &PublicationInputControl) {
        if let Some(entry) = self
            .publications
            .values_mut()
            .find(|entry| entry.owner.id() == control.operation.owner())
        {
            entry.last_control = Some(control.clone());
            entry.last_control_configuration_revision = (entry.input.is_some()
                && entry.desired_revision > entry.evaluated_revision
                && control.enabled == !entry.muted)
                .then_some(entry.desired_revision);
            entry.last_control_accepted_revision = entry.accepted_revision;
            entry.last_control_accepted_enabled = entry.accepted_enabled;
            entry.control_completed = false;
        }
    }

    pub(crate) fn complete_input_control(&mut self, control: &PublicationInputControl) -> bool {
        let Some(entry) = self
            .publications
            .values_mut()
            .find(|entry| entry.last_control.as_ref() == Some(control) && !entry.control_completed)
        else {
            return false;
        };
        entry.control_completed = true;
        if let Some(revision) = entry.last_control_configuration_revision
            && revision == entry.desired_revision
            && revision > entry.evaluated_revision
        {
            entry.evaluated_revision = revision;
            if control.enabled == !entry.muted {
                entry.accepted_revision = revision;
                entry.accepted_enabled = !entry.muted;
                entry.rejected_revision = None;
            } else {
                entry.rejected_revision = Some(revision);
            }
        }
        true
    }

    pub(crate) fn release_input_control(
        &self,
        owner: TrackOwner,
    ) -> Option<PublicationInputControl> {
        let mut control = self
            .publication_inputs()
            .find(|control| control.operation.owner == owner)?;
        control.operation.input = None;
        control.operation.present = false;
        control.enabled = false;
        Some(control)
    }

    pub(crate) fn fail_publication_input(
        &mut self,
        operation: &PublicationInputOperation,
        code: crate::ffi::ErrorCode,
    ) -> Option<PublicationInputState> {
        let entry = self.publications.values_mut().find(|entry| {
            entry.owner == operation.owner
                && entry.input == operation.input
                && entry.input_revision == operation.revision
        })?;
        if entry
            .last_control
            .as_ref()
            .is_some_and(|control| control.operation == *operation)
        {
            if let Some(revision) = entry.last_control_configuration_revision
                && revision == entry.desired_revision
                && (revision > entry.evaluated_revision || entry.accepted_revision == revision)
            {
                entry.evaluated_revision = revision;
                entry.accepted_revision = entry.last_control_accepted_revision;
                entry.accepted_enabled = entry.last_control_accepted_enabled;
                entry.rejected_revision = Some(revision);
            }
        }
        entry.present = Some(false);
        entry.pending_input = false;
        entry.input_error = Some(crate::ffi::ErrorInfo::new(code, "Input operation failed"));
        Some(entry.input_state())
    }

    pub(crate) fn set_publication_muted(
        &mut self,
        owner: TrackOwner,
        muted: bool,
    ) -> Result<PublicationInputState, TrackResourceError> {
        let entry = self
            .publications
            .values_mut()
            .find(|entry| entry.owner == owner)
            .ok_or(TrackResourceError::Released)?;
        if entry.muted != muted {
            let revision = entry
                .desired_revision
                .checked_add(1)
                .ok_or(TrackResourceError::PublicationConfigurationSpaceExhausted)?;
            entry.muted = muted;
            entry.desired_revision = revision;
            entry.settle_configuration_without_input();
        }
        Ok(entry.input_state())
    }

    pub fn release_publication(&mut self, owner: TrackOwner, spec: &TrackSpec) -> Option<String> {
        if !self
            .publications
            .get(spec)
            .is_some_and(|entry| entry.owner == owner)
        {
            return None;
        }
        self.publications.remove(spec).map(|entry| entry.slot)
    }

    pub(crate) fn publication_spec(&self, owner: TrackOwner) -> Option<TrackSpec> {
        self.publications
            .iter()
            .find(|(_, entry)| entry.owner == owner)
            .map(|(spec, _)| spec.clone())
    }

    pub(crate) fn has_publication(&self, spec: &TrackSpec) -> bool {
        self.publications.contains_key(spec)
    }

    pub(crate) fn has_video(&self) -> bool {
        !self.video.is_empty()
    }

    pub(crate) fn has_audio_subscriptions(&self) -> bool {
        !self.audio_views.is_empty()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.publications.is_empty()
            && self.data_publications.is_empty()
            && self.audio_views.is_empty()
            && self.audio.is_none()
            && self.video.is_empty()
            && self.producers.is_empty()
            && self.receivers.is_empty()
    }

    pub(crate) fn publication_slots(&self) -> BTreeSet<String> {
        self.publications
            .values()
            .map(|entry| entry.slot.clone())
            .collect()
    }

    pub(crate) fn release(&mut self, owner: TrackOwner) {
        self.publications.retain(|_, entry| entry.owner != owner);
        self.intent_revisions.remove(&owner);
        self.data_publications
            .retain(|_, (current, _)| *current != owner);
        self.release_audio(owner);
        self.audio_views.remove(&owner);
        self.release_video(owner);
        self.release_receiver(owner);
    }

    pub fn publications(&self) -> Vec<PublicationIntent> {
        self.publications
            .iter()
            .filter_map(|(spec, entry)| {
                let kind = spec.media_kind()?;
                Some(PublicationIntent {
                    slot: entry.slot.clone(),
                    kind,
                    track_id: spec.track_id().into(),
                    active: true,
                })
            })
            .collect()
    }

    pub fn claim_audio(&mut self, owner: TrackOwner) -> Result<(), TrackResourceError> {
        if self.audio.is_some_and(|current| current != owner) {
            return Err(TrackResourceError::AlreadyOwned);
        }
        self.audio = Some(owner);
        Ok(())
    }

    pub fn release_audio(&mut self, owner: TrackOwner) -> bool {
        if self.audio != Some(owner) {
            return false;
        }
        self.audio = None;
        true
    }

    pub(crate) fn claim_audio_subscription(
        &mut self,
        owner: TrackOwner,
        selector: TrackSelector,
    ) -> Result<(), TrackResourceError> {
        validate_identifier(
            "participant external id",
            &selector.participant_id,
            256,
            true,
        )?;
        TrackSpec::Audio {
            track_id: selector.track_id.clone(),
        }
        .validate()?;
        if self.audio_views.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        if self.audio_views.values().any(|view| match view {
            AudioView::Named {
                selector: other,
                playout_delay,
            } => *other == selector && *playout_delay != PlayoutDelay::Adaptive,
            AudioView::Policy(_) => view.has_fixed_delay(),
        }) {
            return Err(TrackResourceError::AlreadyOwned);
        }
        self.begin_intent(owner)?;
        self.audio_views.insert(
            owner,
            AudioView::Named {
                selector,
                playout_delay: PlayoutDelay::Adaptive,
            },
        );
        Ok(())
    }

    pub(crate) fn configure_audio(
        &mut self,
        owner: TrackOwner,
        playout_delay: PlayoutDelay,
    ) -> Result<(), TrackResourceError> {
        if self.video.contains_key(&owner) {
            return Err(TrackResourceError::WrongKind);
        }
        if matches!(playout_delay, PlayoutDelay::Fixed { min_ms, max_ms } if min_ms > max_ms) {
            return Err(TrackResourceError::InvalidPlayoutDelay);
        }
        let AudioView::Named { selector, .. } = self
            .audio_views
            .get(&owner)
            .ok_or(TrackResourceError::Released)?
        else {
            return Err(TrackResourceError::WrongKind);
        };
        if self.audio_views.iter().any(|(other, view)| {
            *other != owner
                && match view {
                    AudioView::Named {
                        selector: candidate,
                        playout_delay: delay,
                    } => candidate == selector && *delay != playout_delay,
                    AudioView::Policy(_) => {
                        playout_delay != PlayoutDelay::Adaptive || view.has_fixed_delay()
                    }
                }
        }) {
            return Err(TrackResourceError::AlreadyOwned);
        }
        self.begin_intent(owner)?;
        if let Some(AudioView::Named {
            playout_delay: delay,
            ..
        }) = self.audio_views.get_mut(&owner)
        {
            *delay = playout_delay;
        }
        Ok(())
    }

    pub(crate) fn claim_audio_policy(
        &mut self,
        owner: TrackOwner,
        policy: crate::AudioSubscription,
    ) -> Result<(), TrackResourceError> {
        if self.audio_views.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        self.validate_audio_policy(owner, &policy)?;
        self.begin_intent(owner)?;
        self.audio_views.insert(owner, AudioView::Policy(policy));
        Ok(())
    }

    pub(crate) fn configure_audio_policy(
        &mut self,
        owner: TrackOwner,
        policy: crate::AudioSubscription,
    ) -> Result<(), TrackResourceError> {
        match self.audio_views.get(&owner) {
            Some(AudioView::Policy(_)) => {}
            Some(AudioView::Named { .. }) => return Err(TrackResourceError::WrongKind),
            None => return Err(TrackResourceError::Released),
        }
        self.validate_audio_policy(owner, &policy)?;
        self.begin_intent(owner)?;
        self.audio_views.insert(owner, AudioView::Policy(policy));
        Ok(())
    }

    fn validate_audio_policy(
        &self,
        owner: TrackOwner,
        policy: &crate::AudioSubscription,
    ) -> Result<(), TrackResourceError> {
        crate::DesiredState {
            audio: policy.clone(),
            ..Default::default()
        }
        .validate()?;
        if policy
            .playout_delays
            .values()
            .chain(policy.selector_delays.values())
            .any(|delay| matches!(delay, PlayoutDelay::Fixed { min_ms, max_ms } if min_ms > max_ms))
        {
            return Err(TrackResourceError::InvalidPlayoutDelay);
        }
        let fixed = policy
            .playout_delays
            .values()
            .chain(policy.selector_delays.values())
            .any(|delay| *delay != PlayoutDelay::Adaptive);
        // Policy matching can overlap named or opaque-ID demand after discovery.
        // Fixed policy delay therefore requires exclusive audio ownership.
        if self
            .audio_views
            .iter()
            .any(|(other, view)| *other != owner && (fixed || view.has_fixed_delay()))
        {
            return Err(TrackResourceError::AlreadyOwned);
        }
        Ok(())
    }

    pub(crate) fn audio_source_authorized(
        &self,
        owner: TrackOwner,
        source: &str,
        selector: Option<&TrackSelector>,
    ) -> bool {
        match self.audio_views.get(&owner) {
            Some(AudioView::Named {
                selector: selected, ..
            }) => selector == Some(selected),
            Some(AudioView::Policy(policy)) => {
                policy.pinned.iter().any(|id| id == source)
                    || selector.is_some_and(|selector| policy.selected.contains(selector))
            }
            None => false,
        }
    }

    pub(crate) fn audio_projection(&self) -> crate::AudioSubscription {
        let mut projection = crate::AudioSubscription::default();
        for view in self.audio_views.values() {
            match view {
                AudioView::Named {
                    selector,
                    playout_delay,
                } => {
                    projection.selected.push(selector.clone());
                    projection
                        .selector_delays
                        .insert(selector.clone(), *playout_delay);
                }
                AudioView::Policy(policy) => {
                    projection.pinned.extend(policy.pinned.iter().cloned());
                    projection.selected.extend(policy.selected.iter().cloned());
                    projection
                        .playout_delays
                        .extend(policy.playout_delays.clone());
                    projection
                        .selector_delays
                        .extend(policy.selector_delays.clone());
                }
            }
        }
        projection.pinned.sort();
        projection.pinned.dedup();
        projection.selected.sort();
        projection.selected.dedup();
        projection
    }

    pub fn claim_video(
        &mut self,
        owner: TrackOwner,
        selector: TrackSelector,
    ) -> Result<(), TrackResourceError> {
        validate_identifier(
            "participant external id",
            &selector.participant_id,
            256,
            true,
        )?;
        TrackSpec::Video {
            track_id: selector.track_id.clone(),
        }
        .validate()?;
        if self.video.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        self.begin_intent(owner)?;
        self.video.insert(
            owner,
            VideoView {
                selector,
                observation: VideoObservation::default(),
            },
        );
        Ok(())
    }

    pub fn observe_video(
        &mut self,
        owner: TrackOwner,
        observation: VideoObservation,
    ) -> Result<(), TrackResourceError> {
        if self.audio_views.contains_key(&owner) {
            return Err(TrackResourceError::WrongKind);
        }
        if matches!(observation.playout_delay, PlayoutDelay::Fixed { min_ms, max_ms } if min_ms > max_ms)
        {
            return Err(TrackResourceError::InvalidPlayoutDelay);
        }
        if !self.video.contains_key(&owner) {
            return Err(TrackResourceError::Released);
        }
        self.begin_intent(owner)?;
        let view = self
            .video
            .get_mut(&owner)
            .ok_or(TrackResourceError::Released)?;
        view.observation = observation;
        Ok(())
    }

    pub fn release_video(&mut self, owner: TrackOwner) -> bool {
        self.video.remove(&owner).is_some()
    }

    pub fn video_projection(&mut self) -> Result<Vec<VideoSubscription>, TrackResourceError> {
        let mut grouped: BTreeMap<TrackSelector, VideoObservation> = BTreeMap::new();
        for view in self.video.values().filter(|view| view.observation.visible) {
            let observation = &view.observation;
            let aggregate = grouped
                .entry(view.selector.clone())
                .or_insert_with(|| observation.clone());
            aggregate.height = if aggregate.height == 0 || observation.height == 0 {
                0
            } else {
                aggregate.height.max(observation.height)
            };
            aggregate.min_height = aggregate.min_height.max(observation.min_height);
            aggregate.min_fps = aggregate.min_fps.max(observation.min_fps);
            aggregate.priority = aggregate.priority.max(observation.priority);
            aggregate.playout_delay = match (aggregate.playout_delay, observation.playout_delay) {
                (
                    PlayoutDelay::Fixed {
                        min_ms: a_min,
                        max_ms: a_max,
                    },
                    PlayoutDelay::Fixed { min_ms, max_ms },
                ) => PlayoutDelay::Fixed {
                    min_ms: a_min.min(min_ms),
                    max_ms: a_max.max(max_ms),
                },
                (PlayoutDelay::Adaptive, delay) | (delay, PlayoutDelay::Adaptive) => delay,
            };
        }
        self.video_slots
            .retain(|selector, _| self.video.values().any(|view| view.selector == *selector));
        let mut occupied: BTreeSet<_> = self.video_slots.values().copied().collect();
        let mut projection = Vec::new();
        for (selector, observation) in grouped {
            let slot = self
                .video_slots
                .get(&selector)
                .copied()
                .or_else(|| (0..=u32::MAX).find(|slot| !occupied.contains(slot)))
                .ok_or(TrackResourceError::SlotSpaceExhausted {
                    kind: MediaKind::Video,
                })?;
            occupied.insert(slot);
            self.video_slots.insert(selector.clone(), slot);
            projection.push(VideoSubscription {
                slot,
                publication_id: String::new(),
                selector: Some(selector),
                height: observation.height,
                min_height: observation.min_height,
                min_fps: observation.min_fps,
                priority: observation.priority,
                playout_delay: observation.playout_delay,
            });
        }
        projection.sort_by_key(|subscription| subscription.slot);
        Ok(projection)
    }

    pub fn retain_producer(&mut self, spec: &TrackSpec) -> Result<(), TrackResourceError> {
        let publisher = spec.publisher()?;
        if let Some((_, mode)) = self.data_publications.get(spec.track_id()) {
            if *mode != publisher.mode {
                return Err(TrackResourceError::AlreadyOwned);
            }
            return Ok(());
        }
        if self
            .producers
            .iter()
            .any(|current| current.topic == publisher.topic && current.mode != publisher.mode)
        {
            return Err(TrackResourceError::AlreadyOwned);
        }
        let mut registrations = self.registrations();
        registrations.publishers.push(publisher.clone());
        registrations.normalize();
        registrations.publishers.dedup();
        registrations.validate()?;
        self.producers.insert(publisher);
        Ok(())
    }

    pub fn receive(
        &mut self,
        owner: TrackOwner,
        spec: &TrackSpec,
        source: TrackSelector,
    ) -> Result<(), TrackResourceError> {
        let subscriber = spec.subscriber(&source)?;
        if self.receivers.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        let mut registrations = self.registrations();
        registrations.subscribers.push(subscriber.clone());
        registrations.normalize();
        registrations.subscribers.dedup();
        registrations.validate()?;
        self.begin_intent(owner)?;
        self.receivers.insert(
            owner,
            DataReceiver {
                subscriber,
                queue: VecDeque::new(),
                failure: None,
            },
        );
        Ok(())
    }

    pub fn release_receiver(&mut self, owner: TrackOwner) -> bool {
        self.receivers.remove(&owner).is_some()
    }

    pub fn registrations(&self) -> TopicRegistrations {
        let mut registrations = TopicRegistrations {
            publishers: self
                .producers
                .iter()
                .cloned()
                .chain(
                    self.data_publications
                        .iter()
                        .map(|(topic, (_, mode))| TopicPublisher {
                            topic: topic.clone(),
                            mode: *mode,
                        }),
                )
                .collect(),
            subscribers: self
                .receivers
                .values()
                .filter(|receiver| receiver.failure.is_none())
                .map(|receiver| receiver.subscriber.clone())
                .collect(),
        };
        registrations.normalize();
        registrations.subscribers.dedup();
        registrations
    }

    pub fn read(&mut self, owner: TrackOwner) -> Result<Option<DataEvent>, TrackResourceError> {
        let receiver = self
            .receivers
            .get_mut(&owner)
            .ok_or(TrackResourceError::Released)?;
        if let Some(failure) = &receiver.failure {
            return Err(failure.clone());
        }
        Ok(receiver.queue.pop_front())
    }

    pub(crate) fn fail_receivers_for_source(
        &mut self,
        source: &TrackSelector,
        mode: Option<TopicMode>,
        error: &crate::ffi::ErrorInfo,
    ) -> ReceiveUpdate {
        let mut update = ReceiveUpdate::default();
        for (owner, receiver) in &mut self.receivers {
            let subscriber = &receiver.subscriber;
            if receiver.failure.is_some()
                || subscriber.room != source.room
                || subscriber.participant_id != source.participant_id
                || subscriber.topic != source.track_id
                || mode.is_some_and(|mode| subscriber.mode != mode)
            {
                continue;
            }
            receiver.fail(TrackResourceError::ReceiveFailed(error.clone()));
            update.ready.push(*owner);
            update.registrations_changed = true;
        }
        update
    }

    /// Feed only notifications already validated and deduplicated by the topic transport.
    pub fn notify(&mut self, notification: &TopicNotification) -> ReceiveUpdate {
        let mut update = ReceiveUpdate::default();
        for (owner, receiver) in &mut self.receivers {
            if receiver.failure.is_some() {
                continue;
            }
            match notification {
                TopicNotification::Message(message)
                    if receiver.subscriber.room == message.room
                        && receiver.subscriber.topic == message.topic
                        && receiver.subscriber.mode == message.mode =>
                {
                    receiver.push(DataEvent::Message {
                        participant_id: message.participant_id.clone(),
                        publication_id: message.publication_id.clone(),
                        sequence: message.sequence,
                        data: message.payload.clone(),
                    });
                }
                TopicNotification::RecoveryGap {
                    subscriber,
                    publication_id,
                    participant_id,
                    expected_sequence,
                    available_sequence,
                } if &receiver.subscriber == subscriber
                    && subscriber.mode == TopicMode::Ordered =>
                {
                    receiver.push(DataEvent::Gap {
                        participant_id: participant_id.clone(),
                        publication_id: publication_id.clone(),
                        from: *expected_sequence,
                        to: *available_sequence,
                    });
                }
                TopicNotification::ChannelFailed {
                    channel: TopicChannel::Subscriber(subscriber),
                    message,
                } if &receiver.subscriber == subscriber => {
                    receiver.fail(TrackResourceError::ReceiveChannelFailed(message.clone()));
                }
                _ => continue,
            }
            if !receiver.queue.is_empty() || receiver.failure.is_some() {
                update.ready.push(*owner);
            }
            update.registrations_changed |= receiver.failure.is_some();
        }
        update
    }
}

#[cfg(test)]
mod tests;
