use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    vec::Vec,
};

use crate::{
    MediaKind, MediaTopology, PlayoutDelay, PublicationIntent, TopicChannel, TopicMode,
    TopicNotification, TopicPublisher, TopicRegistrations, TopicSubscriber, TrackSelector,
    ValidationError, VideoSubscription, model::validate_identifier,
};

pub const DATA_RECEIVE_CAPACITY: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrackSpec {
    Video { label: String },
    Audio { label: String },
    Data { label: String, delivery: TopicMode },
}

impl TrackSpec {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Video { label } | Self::Audio { label } => {
                validate_identifier("publication label", label, 64, false)
            }
            Self::Data { label, delivery } => TopicRegistrations {
                publishers: vec![TopicPublisher {
                    topic: label.clone(),
                    mode: *delivery,
                }],
                subscribers: Vec::new(),
            }
            .validate(),
        }
    }

    pub(crate) fn from_catalog(track: &pulsebeam_proto::signaling_v1::RemoteTrack) -> Option<Self> {
        use pulsebeam_proto::signaling_v1::{TopicMode as WireMode, TrackKind};
        let label = track.label.clone();
        Some(match TrackKind::try_from(track.kind).ok()? {
            TrackKind::Video => Self::Video { label },
            TrackKind::Audio => Self::Audio { label },
            TrackKind::Data => Self::Data {
                label,
                delivery: match WireMode::try_from(track.mode).ok()? {
                    WireMode::Latest => TopicMode::Latest,
                    WireMode::Lossy => TopicMode::Lossy,
                    WireMode::Ordered => TopicMode::Ordered,
                    WireMode::Unspecified => return None,
                },
            },
            TrackKind::Unspecified => return None,
        })
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Video { label } | Self::Audio { label } | Self::Data { label, .. } => label,
        }
    }

    pub const fn media_kind(&self) -> Option<MediaKind> {
        match self {
            Self::Video { .. } => Some(MediaKind::Video),
            Self::Audio { .. } => Some(MediaKind::Audio),
            Self::Data { .. } => None,
        }
    }

    pub(crate) fn subscriber(&self) -> Result<TopicSubscriber, TrackResourceError> {
        self.validate()?;
        match self {
            Self::Data { label, delivery } => Ok(TopicSubscriber {
                topic: label.clone(),
                mode: *delivery,
            }),
            _ => Err(TrackResourceError::WrongKind),
        }
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
    #[error("owner already owns another resource")]
    OwnerInUse,
    #[error("resource owner identity space exhausted")]
    OwnerSpaceExhausted,
    #[error("local {kind:?} capacity exhausted")]
    Capacity { kind: MediaKind },
    #[error("owner no longer owns this resource")]
    Released,
    #[error("playout delay minimum exceeds maximum")]
    InvalidPlayoutDelay,
    #[error("data receive buffer cannot represent all publisher tracks")]
    ReceiveOverflow,
    #[error("data receive channel failed: {0}")]
    ReceiveChannelFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataEvent {
    Message {
        participant_id: String,
        track_id: String,
        sequence: u64,
        data: Vec<u8>,
    },
    Gap {
        participant_id: String,
        track_id: String,
        from: u64,
        to: u64,
    },
}

impl DataEvent {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Message {
                participant_id,
                track_id,
                ..
            }
            | Self::Gap {
                participant_id,
                track_id,
                ..
            } => (participant_id, track_id),
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoProjection {
    pub subscriptions: Vec<VideoSubscription>,
    pub unmapped: Vec<TrackSelector>,
}

#[derive(Default)]
pub struct ReceiveUpdate {
    pub ready: Vec<TrackOwner>,
    pub registrations_changed: bool,
}

struct PublicationOwner {
    owner: TrackOwner,
    slot: String,
    present: bool,
}

struct DataReceiver {
    subscriber: TopicSubscriber,
    participant_id: Option<String>,
    queue: VecDeque<DataEvent>,
    failure: Option<TrackResourceError>,
}

impl DataReceiver {
    fn push(&mut self, event: DataEvent) {
        if self.failure.is_some()
            || self
                .participant_id
                .as_deref()
                .is_some_and(|id| id != event.identity().0)
        {
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
                track_id: identity.1.into(),
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
    UpdatePublication {
        owner: u64,
        present: bool,
    },
    ClaimVideo {
        selector: TrackSelector,
    },
    ObserveVideo {
        owner: u64,
        observation: VideoObservation,
    },
    ClaimAudio,
    RetainProducer {
        spec: TrackSpec,
    },
    ReceiveData {
        spec: TrackSpec,
        participant_id: Option<String>,
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
    Publication { owner: u64, slot: String },
    Owned { owner: u64 },
    Event { event: Option<DataEvent> },
    Updated,
}

/// Portable resource policy. Browser objects and pending promises remain in the host.
#[derive(Default)]
pub struct TrackResources {
    next_owner: u64,
    publications: BTreeMap<TrackSpec, PublicationOwner>,
    audio: Option<TrackOwner>,
    video: BTreeMap<TrackOwner, VideoView>,
    video_slots: BTreeMap<TrackSelector, u8>,
    producers: BTreeSet<TopicPublisher>,
    receivers: BTreeMap<TrackOwner, DataReceiver>,
}

impl TrackResources {
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
        topology: &MediaTopology,
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
        let (prefix, capacity) = match kind {
            MediaKind::Video => ('v', topology.local_video),
            MediaKind::Audio => ('a', topology.local_audio),
        };
        let slot = (0..capacity)
            .map(|index| format!("{prefix}{index}"))
            .find(|slot| {
                !occupied.contains(slot)
                    && !self.publications.values().any(|entry| &entry.slot == slot)
            })
            .ok_or(TrackResourceError::Capacity { kind })?;
        self.publications.insert(
            spec,
            PublicationOwner {
                owner,
                slot: slot.clone(),
                present: false,
            },
        );
        Ok(slot)
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
        entry.present = present;
        Ok(())
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

    pub(crate) fn is_empty(&self) -> bool {
        self.publications.is_empty()
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
        self.release_audio(owner);
        self.release_video(owner);
        self.release_receiver(owner);
    }

    pub fn publications(&self) -> Vec<PublicationIntent> {
        self.publications
            .iter()
            .filter(|(_, entry)| entry.present)
            .map(|(spec, entry)| PublicationIntent {
                slot: entry.slot.clone(),
                label: spec.label().into(),
                active: true,
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

    pub fn claim_video(
        &mut self,
        owner: TrackOwner,
        selector: TrackSelector,
    ) -> Result<(), TrackResourceError> {
        validate_identifier(
            "participant external id",
            &selector.participant_external_id,
            256,
            true,
        )?;
        TrackSpec::Video {
            label: selector.label.clone(),
        }
        .validate()?;
        if self.video.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
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
        if matches!(observation.playout_delay, PlayoutDelay::Fixed { min_ms, max_ms } if min_ms > max_ms)
        {
            return Err(TrackResourceError::InvalidPlayoutDelay);
        }
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

    pub fn video_projection(
        &mut self,
        tracks: &BTreeMap<String, RemoteTrack>,
        capacity: u8,
    ) -> VideoProjection {
        let mut grouped: BTreeMap<TrackSelector, VideoObservation> = BTreeMap::new();
        for view in self.video.values().filter(|view| view.observation.visible) {
            let observation = &view.observation;
            let aggregate = grouped.entry(view.selector.clone()).or_default();
            aggregate.height = aggregate.height.max(observation.height);
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
        grouped.retain(|selector, observation| {
            observation.height > 0
                && tracks.values().any(|track| {
                    track.participant_id == selector.participant_external_id
                        && track.spec
                            == (TrackSpec::Video {
                                label: selector.label.clone(),
                            })
                })
        });
        self.video_slots
            .retain(|selector, slot| grouped.contains_key(selector) && *slot < capacity);
        let mut occupied: BTreeSet<_> = self.video_slots.values().copied().collect();
        let mut projection = VideoProjection::default();
        for (selector, observation) in grouped {
            let slot = self
                .video_slots
                .get(&selector)
                .copied()
                .or_else(|| (0..capacity).find(|slot| !occupied.contains(slot)));
            let Some(slot) = slot else {
                projection.unmapped.push(selector);
                continue;
            };
            occupied.insert(slot);
            self.video_slots.insert(selector.clone(), slot);
            projection.subscriptions.push(VideoSubscription {
                slot,
                track_id: String::new(),
                selector: Some(selector),
                height: observation.height,
                min_height: observation.min_height,
                min_fps: observation.min_fps,
                priority: observation.priority,
                playout_delay: observation.playout_delay,
            });
        }
        projection
            .subscriptions
            .sort_by_key(|subscription| subscription.slot);
        projection
    }

    pub fn retain_producer(&mut self, spec: &TrackSpec) -> Result<(), TrackResourceError> {
        let subscriber = spec.subscriber()?;
        let publisher = TopicPublisher {
            topic: subscriber.topic,
            mode: subscriber.mode,
        };
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
        participant_id: Option<String>,
    ) -> Result<(), TrackResourceError> {
        let subscriber = spec.subscriber()?;
        if self.receivers.contains_key(&owner) {
            return Err(TrackResourceError::OwnerInUse);
        }
        if let Some(id) = &participant_id {
            validate_identifier("participant external id", id, 256, true)?;
        }
        let mut registrations = self.registrations();
        registrations.subscribers.push(subscriber.clone());
        registrations.normalize();
        registrations.subscribers.dedup();
        registrations.validate()?;
        self.receivers.insert(
            owner,
            DataReceiver {
                subscriber,
                participant_id,
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
            publishers: self.producers.iter().cloned().collect(),
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

    /// Feed only notifications already validated and deduplicated by the topic transport.
    pub fn notify(&mut self, notification: &TopicNotification) -> ReceiveUpdate {
        let mut update = ReceiveUpdate::default();
        for (owner, receiver) in &mut self.receivers {
            if receiver.failure.is_some() {
                continue;
            }
            match notification {
                TopicNotification::Message(message)
                    if receiver.subscriber.topic == message.topic
                        && receiver.subscriber.mode == message.mode =>
                {
                    receiver.push(DataEvent::Message {
                        participant_id: message.participant_id.clone(),
                        track_id: message.data_track_id.clone(),
                        sequence: message.sequence,
                        data: message.payload.clone(),
                    });
                }
                TopicNotification::RecoveryGap {
                    subscriber,
                    data_track_id,
                    participant_id,
                    expected_sequence,
                    available_sequence,
                } if &receiver.subscriber == subscriber
                    && subscriber.mode == TopicMode::Ordered =>
                {
                    receiver.push(DataEvent::Gap {
                        participant_id: participant_id.clone(),
                        track_id: data_track_id.clone(),
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
