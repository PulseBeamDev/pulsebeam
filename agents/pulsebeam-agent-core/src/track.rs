use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    vec::Vec,
};

use crate::{
    MediaKind, PublicationIntent, TopicChannel, TopicMode, TopicNotification, TopicPublisher,
    TopicRegistrations, TopicSubscriber, TrackSelector, ValidationError, VideoSubscription,
    ffi::{PublicationConfig, VideoCodec, VideoPublishOptions},
    model::validate_identifier,
};

mod data_set;
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
    #[error("selector observation capacity exceeded")]
    SelectorObservationCapacityExceeded,
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
        track_id: String,
        publication_id: String,
        delivery: crate::ffi::DataDelivery,
        sequence: u64,
        data: Vec<u8>,
        #[uniffi(default = None)]
        captured_at: Option<crate::ffi::CaptureTimestamp>,
        #[uniffi(default = None)]
        fragment_descriptors: Option<Vec<crate::data_v1::DataFragmentDescriptor>>,
    },
    SourceFailed {
        participant_id: String,
        track_id: String,
        publication_id: String,
        delivery: crate::ffi::DataDelivery,
        error: crate::ffi::ErrorInfo,
    },
    Discontinuity {
        participant_id: String,
        previous_publication_id: String,
        publication_id: String,
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
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Message {
                participant_id,
                track_id,
                publication_id,
                data,
                captured_at,
                fragment_descriptors,
                ..
            } => {
                let descriptors = fragment_descriptors.as_ref().map_or(0, |items| {
                    items
                        .len()
                        .saturating_mul(
                            core::mem::size_of::<crate::data_v1::DataFragmentDescriptor>(),
                        )
                        .saturating_add(
                            items
                                .iter()
                                .map(|item| item.bytes.len())
                                .fold(0_usize, usize::saturating_add),
                        )
                });
                participant_id
                    .len()
                    .saturating_add(track_id.len())
                    .saturating_add(publication_id.len())
                    .saturating_add(data.len())
                    .saturating_add(captured_at.as_ref().map_or(0, |stamp| stamp.clock.id.len()))
                    .saturating_add(descriptors)
            }
            Self::SourceFailed { .. } => 0,
            Self::Discontinuity {
                participant_id,
                previous_publication_id,
                publication_id,
            } => participant_id
                .len()
                .saturating_add(previous_publication_id.len())
                .saturating_add(publication_id.len()),
            Self::Gap {
                participant_id,
                publication_id,
                ..
            } => participant_id.len().saturating_add(publication_id.len()),
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
}

struct VideoView {
    selector: TrackSelector,
    observation: VideoObservation,
}

enum AudioView {
    Named { selector: TrackSelector },
    Policy(crate::AudioSubscription),
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
    video_codec: Option<VideoCodecRequirement>,
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

#[derive(Default)]
struct DataReadQueue {
    events: VecDeque<(usize, DataEvent)>,
    bytes: usize,
}

impl DataReadQueue {
    const BYTE_LIMIT: usize = 1024 * 1024;

    fn push(&mut self, mode: TopicMode, event: DataEvent) -> bool {
        let bytes = event.retained_bytes();
        if bytes > Self::BYTE_LIMIT {
            return false;
        }
        if mode == TopicMode::Latest {
            self.clear();
        }
        while self.events.len() == DATA_RECEIVE_CAPACITY
            || self.bytes.saturating_add(bytes) > Self::BYTE_LIMIT
        {
            if mode != TopicMode::Lossy {
                return false;
            }
            let _ = self.pop();
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.events.push_back((bytes, event));
        true
    }

    fn pop(&mut self) -> Option<DataEvent> {
        let (bytes, event) = self.events.pop_front()?;
        self.bytes = self.bytes.saturating_sub(bytes);
        Some(event)
    }

    fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

enum DataReceiveIntent {
    Explicit(TopicSubscriber),
    Inherit(TrackSelector),
}

impl DataReceiveIntent {
    fn matches(&self, room: &str, participant: &str, track: &str, mode: Option<TopicMode>) -> bool {
        match self {
            Self::Explicit(subscriber) => {
                subscriber.room == room
                    && subscriber.participant_id == participant
                    && subscriber.topic == track
                    && mode.is_none_or(|mode| subscriber.mode == mode)
            }
            Self::Inherit(selector) => {
                selector.room == room
                    && selector.participant_id == participant
                    && selector.track_id == track
            }
        }
    }
}

struct DataReceiver {
    intent: DataReceiveIntent,
    source: Option<(String, TopicMode)>,
    discontinuity: Option<DataEvent>,
    queue: DataReadQueue,
    failure: Option<TrackResourceError>,
}

impl DataReceiver {
    fn push(&mut self, event: DataEvent) {
        if self.failure.is_some() {
            return;
        }
        let DataEvent::Message {
            participant_id: participant,
            publication_id: publication,
            delivery,
            ..
        } = &event
        else {
            return;
        };
        let mode = (*delivery).into();
        if self.source.as_ref().map(|(id, _)| id.as_str()) != Some(publication) {
            if let Some((previous, previous_mode)) = self.source.take() {
                if previous_mode == TopicMode::Ordered {
                    self.lose_continuity("Reliable source was replaced");
                    return;
                }
                let previous_publication_id = match self.discontinuity.take() {
                    Some(DataEvent::Discontinuity {
                        previous_publication_id,
                        ..
                    }) => previous_publication_id,
                    _ => previous,
                };
                self.queue.clear();
                self.discontinuity = Some(DataEvent::Discontinuity {
                    participant_id: participant.clone(),
                    previous_publication_id,
                    publication_id: publication.clone(),
                });
            }
            self.source = Some((publication.clone(), mode));
        }
        if !self.queue.push(mode, event) {
            if mode == TopicMode::Ordered {
                self.lose_continuity("Reliable receive buffer was exhausted");
            } else {
                self.fail(TrackResourceError::ReceiveOverflow);
            }
        }
    }

    fn lose_continuity(&mut self, message: &str) {
        self.fail(TrackResourceError::ReceiveFailed(
            crate::ffi::ErrorInfo::new(crate::ffi::ErrorCode::ContinuityLost, message),
        ));
    }

    fn fail(&mut self, error: TrackResourceError) {
        self.queue.clear();
        self.failure = Some(error);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodecRequirement {
    H264,
    Av1,
}

impl VideoCodecRequirement {
    pub(crate) fn matches(self, format: &pulsebeam_proto::state::CodecFormat) -> bool {
        use pulsebeam_proto::state::{codec_format::Codec, sdp};
        match self {
            Self::H264 => matches!(format.known_codec(), Some(Codec::H264(_))),
            Self::Av1 => sdp::codec_parameters(format).is_ok_and(|parameters| {
                self.matches_name(&parameters.name) && parameters.clock_rate == 90_000
            }),
        }
    }

    pub(crate) fn matches_name(self, name: &str) -> bool {
        name.eq_ignore_ascii_case(match self {
            Self::H264 => "h264",
            Self::Av1 => "av1",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaPublishOptions {
    Video(VideoPublishOptions),
    #[doc(hidden)]
    Audio(PublicationConfig),
}

impl MediaPublishOptions {
    pub(crate) fn default_for(spec: &TrackSpec) -> Self {
        match spec {
            TrackSpec::Video { .. } => Self::Video(VideoPublishOptions::default()),
            _ => Self::Audio(PublicationConfig::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceCommand {
    ClaimPublication {
        spec: TrackSpec,
        options: MediaPublishOptions,
    },
    DeclareDataPublication {
        spec: TrackSpec,
        capture_clock: Option<crate::ffi::CaptureClock>,
        record_dd: bool,
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
    ConfigureMediaPublication {
        owner: u64,
        options: MediaPublishOptions,
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
    OpenMediaCollection {
        room: String,
        kind: crate::SelectorTrackKind,
        filter: crate::TrackFilter,
    },
    BeginAdmittedMediaObservation {
        admission: crate::AdmittedMedia,
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
    ReceiveInheritedData {
        source: TrackSelector,
    },
    OpenDataSet {
        room: String,
        filter: crate::TrackFilter,
    },
    ReadData {
        owner: u64,
    },
    Release {
        owner: u64,
    },
}

impl ResourceCommand {
    pub fn claim_publication(spec: TrackSpec) -> Self {
        Self::ClaimPublication {
            options: MediaPublishOptions::default_for(&spec),
            spec,
        }
    }
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
    data_publications: BTreeMap<String, OwnedDataPublication>,
    audio: Option<TrackOwner>,
    audio_views: BTreeMap<TrackOwner, AudioView>,
    video: BTreeMap<TrackOwner, VideoView>,
    video_slots: BTreeMap<TrackSelector, u32>,
    observations: BTreeMap<MediaObservationId, observation::MediaObservation>,
    producers: BTreeSet<TopicPublisher>,
    receivers: BTreeMap<TrackOwner, DataReceiver>,
    data_sets: BTreeMap<TrackOwner, Result<data_set::DataSetQueue, crate::ffi::ErrorInfo>>,
    intent_revisions: BTreeMap<TrackOwner, u64>,
}

struct OwnedDataPublication {
    owner: TrackOwner,
    delivery: TopicMode,
    capture_clock: Option<pulsebeam_proto::state::DataV1CaptureClock>,
    record_dd: bool,
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

    pub(crate) fn audio_view(&self, owner: TrackOwner) -> Option<&TrackSelector> {
        match self.audio_views.get(&owner)? {
            AudioView::Named { selector } => Some(selector),
            AudioView::Policy(_) => None,
        }
    }

    pub(crate) fn data_publication(&self, owner: TrackOwner) -> Option<(&str, TopicMode)> {
        self.data_publications
            .iter()
            .find_map(|(track_id, current)| {
                (current.owner == owner).then_some((track_id.as_str(), current.delivery))
            })
    }

    pub(crate) fn data_capture_clock(
        &self,
        track_id: &str,
    ) -> Option<&pulsebeam_proto::state::DataV1CaptureClock> {
        self.data_publications.get(track_id)?.capture_clock.as_ref()
    }

    pub(crate) fn data_record_dd(&self, track_id: &str) -> bool {
        self.data_publications
            .get(track_id)
            .is_some_and(|publication| publication.record_dd)
    }

    pub(crate) fn data_receiver(
        &self,
        owner: TrackOwner,
    ) -> Option<(
        TrackSelector,
        Option<TopicMode>,
        Option<&TrackResourceError>,
    )> {
        let receiver = self.receivers.get(&owner)?;
        let (selector, mode) = match &receiver.intent {
            DataReceiveIntent::Explicit(subscriber) => (
                TrackSelector {
                    room: subscriber.room.clone(),
                    participant_id: subscriber.participant_id.clone(),
                    track_id: subscriber.topic.clone(),
                },
                Some(subscriber.mode),
            ),
            DataReceiveIntent::Inherit(selector) => (selector.clone(), None),
        };
        Some((selector, mode, receiver.failure.as_ref()))
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
        options: MediaPublishOptions,
        occupied: &BTreeSet<String>,
    ) -> Result<String, TrackResourceError> {
        spec.validate()?;
        let (kind, enabled, video_codec) = match (&spec, options) {
            (TrackSpec::Video { .. }, MediaPublishOptions::Video(options)) => (
                MediaKind::Video,
                options.enabled,
                Some(match options.codec.unwrap_or_default() {
                    VideoCodec::H264 => VideoCodecRequirement::H264,
                    VideoCodec::Av1 => VideoCodecRequirement::Av1,
                }),
            ),
            (TrackSpec::Audio { .. }, MediaPublishOptions::Audio(options)) => {
                (MediaKind::Audio, options.enabled, None)
            }
            _ => return Err(TrackResourceError::WrongKind),
        };
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
                video_codec,
                input: None,
                input_revision: 0,
                pending_input: false,
                input_error: None,
                last_control: None,
                control_completed: false,
                muted: !enabled,
                desired_revision: 1,
                evaluated_revision: 1,
                accepted_revision: 1,
                accepted_enabled: enabled,
                rejected_revision: None,
                last_control_configuration_revision: None,
                last_control_accepted_revision: 1,
                last_control_accepted_enabled: enabled,
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
        capture_clock: Option<crate::ffi::CaptureClock>,
        record_dd: bool,
    ) -> Result<(), TrackResourceError> {
        spec.validate()?;
        let TrackSpec::Data { track_id, delivery } = spec else {
            return Err(TrackResourceError::WrongKind);
        };
        let capture_clock = capture_clock.map(|clock| pulsebeam_proto::state::DataV1CaptureClock {
            id: clock.id,
            epoch: clock.epoch,
        });
        if capture_clock.as_ref().is_some_and(|clock| {
            !pulsebeam_proto::state::validation::valid_data_capture_clock(clock)
        }) {
            return Err(ValidationError::Identifier {
                field: "capture clock id",
            }
            .into());
        }
        if let Some(current) = self.data_publications.get(track_id) {
            return if current.owner == owner
                && current.delivery == *delivery
                && current.capture_clock == capture_clock
                && current.record_dd == record_dd
            {
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
                .any(|current| current.owner == owner)
        {
            return Err(TrackResourceError::OwnerInUse);
        }
        self.begin_intent(owner)?;
        self.data_publications.insert(
            track_id.clone(),
            OwnedDataPublication {
                owner,
                delivery: *delivery,
                capture_clock,
                record_dd,
            },
        );
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
            && self.data_sets.is_empty()
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
            .retain(|_, current| current.owner != owner);
        self.release_audio(owner);
        self.audio_views.remove(&owner);
        self.release_video(owner);
        self.release_receiver(owner);
    }

    pub(crate) fn publication_video_slot(&self, spec: &TrackSpec) -> Option<crate::MediaSlot> {
        let entry = self.publications.get(spec)?;
        entry
            .video_codec
            .map(|_| crate::MediaSlot::LocalVideo(entry.slot.clone()))
    }

    pub(crate) fn publication_video_codec(&self, track_id: &str) -> Option<VideoCodecRequirement> {
        self.publications
            .get(&TrackSpec::Video {
                track_id: track_id.into(),
            })?
            .video_codec
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
        self.begin_intent(owner)?;
        self.audio_views
            .insert(owner, AudioView::Named { selector });
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
        Self::validate_audio_policy(&policy)?;
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
        Self::validate_audio_policy(&policy)?;
        self.begin_intent(owner)?;
        self.audio_views.insert(owner, AudioView::Policy(policy));
        Ok(())
    }

    fn validate_audio_policy(policy: &crate::AudioSubscription) -> Result<(), TrackResourceError> {
        crate::DesiredState {
            audio: policy.clone(),
            ..Default::default()
        }
        .validate()?;
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
                AudioView::Named { selector } => projection.selected.push(selector.clone()),
                AudioView::Policy(policy) => {
                    projection.pinned.extend(policy.pinned.iter().cloned());
                    projection.selected.extend(policy.selected.iter().cloned());
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
                decode_target: None,
                slot,
                publication_id: String::new(),
                selector: Some(selector),
                height: observation.height,
                min_height: observation.min_height,
                min_fps: observation.min_fps,
                priority: observation.priority,
            });
        }
        projection.sort_by_key(|subscription| subscription.slot);
        Ok(projection)
    }

    pub fn retain_producer(&mut self, spec: &TrackSpec) -> Result<(), TrackResourceError> {
        let publisher = spec.publisher()?;
        if let Some(publication) = self.data_publications.get(spec.track_id()) {
            if publication.delivery != publisher.mode {
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
        self.insert_data_receiver(owner, DataReceiveIntent::Explicit(subscriber))
    }

    pub(crate) fn receive_inherited_data(
        &mut self,
        source: TrackSelector,
    ) -> Result<TrackOwner, TrackResourceError> {
        source.validate()?;
        if !self
            .inherited_data_demand()
            .any(|existing| existing == &source)
            && self.inherited_data_demand().collect::<BTreeSet<_>>().len()
                >= crate::MAX_TOPIC_CHANNELS
        {
            return Err(TrackResourceError::ReceiveFailed(
                crate::ffi::ErrorInfo::new(
                    crate::ffi::ErrorCode::CapacityExceeded,
                    "Exact data source capacity exceeded",
                ),
            ));
        }
        let owner = self.new_owner()?;
        self.insert_data_receiver(owner, DataReceiveIntent::Inherit(source))?;
        Ok(owner)
    }

    pub(crate) fn is_inherited_data_reader(&self, owner: TrackOwner) -> bool {
        self.receivers
            .get(&owner)
            .is_some_and(|receiver| matches!(receiver.intent, DataReceiveIntent::Inherit(_)))
    }

    pub(crate) fn inherited_data_demand(&self) -> impl Iterator<Item = &TrackSelector> {
        self.receivers
            .values()
            .filter_map(|receiver| match &receiver.intent {
                DataReceiveIntent::Inherit(source) if receiver.failure.is_none() => Some(source),
                _ => None,
            })
    }

    fn insert_data_receiver(
        &mut self,
        owner: TrackOwner,
        intent: DataReceiveIntent,
    ) -> Result<(), TrackResourceError> {
        self.begin_intent(owner)?;
        self.receivers.insert(
            owner,
            DataReceiver {
                intent,
                source: None,
                discontinuity: None,
                queue: DataReadQueue::default(),
                failure: None,
            },
        );
        Ok(())
    }

    pub fn release_receiver(&mut self, owner: TrackOwner) -> bool {
        self.receivers.remove(&owner).is_some() || self.data_sets.remove(&owner).is_some()
    }

    pub(crate) fn open_data_set(&mut self, owner: TrackOwner) -> Result<(), TrackResourceError> {
        self.begin_intent(owner)?;
        self.data_sets
            .insert(owner, Ok(data_set::DataSetQueue::default()));
        Ok(())
    }

    pub(crate) fn has_data_set(&self, owner: TrackOwner) -> bool {
        self.data_sets.contains_key(&owner)
    }

    pub(crate) fn data_set_error(&self, owner: TrackOwner) -> Option<&crate::ffi::ErrorInfo> {
        self.data_sets.get(&owner)?.as_ref().err()
    }

    pub(crate) fn failed_data_set_owners(&self) -> impl Iterator<Item = TrackOwner> + '_ {
        self.data_sets
            .iter()
            .filter_map(|(&owner, set)| set.is_err().then_some(owner))
    }

    pub(crate) fn terminate_data_set(
        &mut self,
        owner: TrackOwner,
        error: crate::ffi::ErrorInfo,
    ) -> bool {
        let Some(reader @ Ok(_)) = self.data_sets.get_mut(&owner) else {
            return false;
        };
        *reader = Err(error);
        true
    }

    pub fn registrations(&self) -> TopicRegistrations {
        let mut registrations =
            TopicRegistrations {
                publishers: self
                    .producers
                    .iter()
                    .cloned()
                    .chain(self.data_publications.iter().map(|(topic, publication)| {
                        TopicPublisher {
                            topic: topic.clone(),
                            mode: publication.delivery,
                        }
                    }))
                    .collect(),
                subscribers: self
                    .receivers
                    .values()
                    .filter(|receiver| receiver.failure.is_none())
                    .filter_map(|receiver| match &receiver.intent {
                        DataReceiveIntent::Explicit(subscriber) => Some(subscriber.clone()),
                        DataReceiveIntent::Inherit(_) => None,
                    })
                    .collect(),
            };
        registrations.normalize();
        registrations.subscribers.dedup();
        registrations
    }

    pub fn read(&mut self, owner: TrackOwner) -> Result<Option<DataEvent>, TrackResourceError> {
        if let Some(reader) = self.data_sets.get_mut(&owner) {
            return match reader {
                Ok(reader) => Ok(reader.pop()),
                Err(error) => Err(TrackResourceError::ReceiveFailed(error.clone())),
            };
        }
        let receiver = self
            .receivers
            .get_mut(&owner)
            .ok_or(TrackResourceError::Released)?;
        if let Some(failure) = &receiver.failure {
            return Err(failure.clone());
        }
        Ok(receiver
            .discontinuity
            .take()
            .or_else(|| receiver.queue.pop()))
    }

    pub(crate) fn fail_receivers_for_source(
        &mut self,
        source: &TrackSelector,
        mode: Option<TopicMode>,
        error: &crate::ffi::ErrorInfo,
    ) -> ReceiveUpdate {
        let mut update = ReceiveUpdate::default();
        for (owner, receiver) in &mut self.receivers {
            if receiver.failure.is_some()
                || (mode.is_some() && matches!(receiver.intent, DataReceiveIntent::Inherit(_)))
                || !receiver.intent.matches(
                    &source.room,
                    &source.participant_id,
                    &source.track_id,
                    mode,
                )
            {
                continue;
            }
            receiver.fail(TrackResourceError::ReceiveFailed(error.clone()));
            update.ready.push(*owner);
            update.registrations_changed = true;
        }
        update
    }

    pub(crate) fn retire_inherited_data_source(
        &mut self,
        source: &TrackSelector,
        replacement: Option<&str>,
    ) -> Vec<TrackOwner> {
        self.receivers.iter_mut().filter_map(|(&owner, reader)| {
            if reader.failure.is_some()
                || !matches!(&reader.intent, DataReceiveIntent::Inherit(selected) if selected == source)
                || !reader.source.as_ref().is_some_and(|(publication, mode)| {
                    *mode == TopicMode::Ordered && Some(publication.as_str()) != replacement
                })
            {
                return None;
            }
            reader.lose_continuity("Reliable source was retired");
            Some(owner)
        }).collect()
    }

    pub(crate) fn retire_data_sets_for_source(
        &mut self,
        source: &TrackSelector,
        replacement: Option<&str>,
    ) -> Vec<TrackOwner> {
        self.update_data_sets(|_, reader| reader.retire(source, replacement))
    }

    pub(crate) fn revoke_data_set_memberships(
        &mut self,
        owner: TrackOwner,
        current: impl FnMut(&TrackSelector) -> bool,
    ) -> bool {
        let Some(Ok(reader)) = self.data_sets.get_mut(&owner) else {
            return false;
        };
        match reader.revoke_unmatched(current) {
            Ok(ready) => ready,
            Err(error) => self.terminate_data_set(owner, error),
        }
    }

    fn update_data_sets(
        &mut self,
        mut update: impl FnMut(
            TrackOwner,
            &mut data_set::DataSetQueue,
        ) -> Result<bool, crate::ffi::ErrorInfo>,
    ) -> Vec<TrackOwner> {
        self.data_sets
            .iter_mut()
            .filter_map(|(&owner, entry)| {
                let Ok(reader) = entry else { return None };
                match update(owner, reader) {
                    Ok(ready) => ready.then_some(owner),
                    Err(error) => {
                        *entry = Err(error);
                        Some(owner)
                    }
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn receive_data_v1(
        &mut self,
        subscriber: &TopicSubscriber,
        publication_id: &str,
        unit: crate::data_v1::AssembledUnit,
    ) -> Vec<u64> {
        self.receive_admitted_data_v1(subscriber, publication_id, unit, |_| false)
    }

    pub(crate) fn receive_admitted_data_v1(
        &mut self,
        subscriber: &TopicSubscriber,
        publication_id: &str,
        unit: crate::data_v1::AssembledUnit,
        mut set_contains_source: impl FnMut(TrackOwner) -> bool,
    ) -> Vec<u64> {
        let event = DataEvent::Message {
            participant_id: subscriber.participant_id.clone(),
            track_id: subscriber.topic.clone(),
            publication_id: publication_id.to_owned(),
            delivery: subscriber.mode.into(),
            sequence: unit.unit_seq,
            data: unit.bytes,
            captured_at: unit.captured_at,
            fragment_descriptors: (!unit.fragment_descriptors.is_empty())
                .then_some(unit.fragment_descriptors),
        };
        let mut ready: Vec<_> = self
            .receivers
            .iter_mut()
            .filter(|(_, receiver)| {
                receiver.failure.is_none()
                    && receiver.intent.matches(
                        &subscriber.room,
                        &subscriber.participant_id,
                        &subscriber.topic,
                        Some(subscriber.mode),
                    )
            })
            .map(|(owner, receiver)| {
                receiver.push(event.clone());
                owner.id()
            })
            .collect();
        if !self.data_sets.is_empty() {
            let source = TrackSelector {
                room: subscriber.room.clone(),
                participant_id: subscriber.participant_id.clone(),
                track_id: subscriber.topic.clone(),
            };
            ready.extend(
                self.update_data_sets(|owner, reader| {
                    if !set_contains_source(owner) {
                        return Ok(false);
                    }
                    reader.push(&source, publication_id, subscriber.mode, event.clone())
                })
                .into_iter()
                .map(TrackOwner::id),
            );
        }
        ready
    }

    /// Feed only notifications already validated and deduplicated by the topic transport.
    pub fn notify(&mut self, notification: &TopicNotification) -> ReceiveUpdate {
        let mut update = ReceiveUpdate::default();
        for (owner, receiver) in &mut self.receivers {
            let DataReceiveIntent::Explicit(subscriber) = &receiver.intent else {
                continue;
            };
            if receiver.failure.is_some() {
                continue;
            }
            match notification {
                TopicNotification::Message(message)
                    if subscriber.room == message.room
                        && subscriber.participant_id == message.participant_id
                        && subscriber.topic == message.topic
                        && subscriber.mode == message.mode =>
                {
                    receiver.push(DataEvent::Message {
                        participant_id: message.participant_id.clone(),
                        track_id: message.topic.clone(),
                        publication_id: message.publication_id.clone(),
                        delivery: message.mode.into(),
                        sequence: message.sequence,
                        data: message.payload.clone(),
                        captured_at: None,
                        fragment_descriptors: None,
                    });
                }
                TopicNotification::RecoveryGap { subscriber, .. }
                    if receiver.intent.matches(
                        &subscriber.room,
                        &subscriber.participant_id,
                        &subscriber.topic,
                        Some(TopicMode::Ordered),
                    ) && subscriber.mode == TopicMode::Ordered =>
                {
                    receiver.lose_continuity("Reliable transport recovery was exhausted");
                }
                TopicNotification::ChannelFailed {
                    channel: TopicChannel::Subscriber(failed),
                    message,
                } if subscriber == failed => {
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
