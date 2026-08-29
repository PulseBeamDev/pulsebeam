use alloc::{collections::BTreeSet, string::String, vec::Vec};

use crate::id::{DataChannelId, Generation};

pub const MAX_UPSTREAM_SLOTS: usize = 2;
pub const MAX_VIDEO_RECEIVE_SLOTS: u8 = 7;
pub const MAX_AUDIO_RECEIVE_SLOTS: u8 = 3;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct AgentConfig {
    endpoint: String,
    topology: Topology,
}

impl AgentConfig {
    pub fn new(endpoint: impl Into<String>, topology: Topology) -> Result<Self, ConfigError> {
        let endpoint = endpoint.into();
        if endpoint.is_empty() {
            return Err(ConfigError::EmptyEndpoint);
        }
        Ok(Self { endpoint, topology })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn topology(&self) -> &Topology {
        &self.topology
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Topology {
    upstream_slots: Vec<UpstreamSlot>,
    video_receive_slots: u8,
    audio_receive_slots: u8,
}

impl Topology {
    pub fn new(
        upstream_slots: Vec<UpstreamSlot>,
        video_receive_slots: u8,
        audio_receive_slots: u8,
    ) -> Result<Self, ConfigError> {
        if upstream_slots.len() > MAX_UPSTREAM_SLOTS {
            return Err(ConfigError::TooManyUpstreamSlots {
                maximum: MAX_UPSTREAM_SLOTS,
            });
        }
        if video_receive_slots > MAX_VIDEO_RECEIVE_SLOTS {
            return Err(ConfigError::TooManyVideoReceiveSlots {
                maximum: MAX_VIDEO_RECEIVE_SLOTS,
            });
        }
        if audio_receive_slots > MAX_AUDIO_RECEIVE_SLOTS {
            return Err(ConfigError::TooManyAudioReceiveSlots {
                maximum: MAX_AUDIO_RECEIVE_SLOTS,
            });
        }

        let mut names = BTreeSet::new();
        for slot in &upstream_slots {
            if slot.name.is_empty() {
                return Err(ConfigError::EmptyUpstreamSlotName);
            }
            if !names.insert(slot.name.clone()) {
                return Err(ConfigError::DuplicateUpstreamSlot(slot.name.clone()));
            }
        }

        Ok(Self {
            upstream_slots,
            video_receive_slots,
            audio_receive_slots,
        })
    }

    pub fn upstream_slots(&self) -> &[UpstreamSlot] {
        &self.upstream_slots
    }

    pub const fn video_receive_slots(&self) -> u8 {
        self.video_receive_slots
    }

    pub const fn audio_receive_slots(&self) -> u8 {
        self.audio_receive_slots
    }

    pub fn has_upstream_slot(&self, name: &str) -> bool {
        self.upstream_slots.iter().any(|slot| slot.name == name)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UpstreamSlot {
    name: String,
}

impl UpstreamSlot {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(thiserror::Error, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ConfigError {
    #[error("endpoint must not be empty")]
    EmptyEndpoint,
    #[error("upstream slot name must not be empty")]
    EmptyUpstreamSlotName,
    #[error("duplicate upstream slot {0}")]
    DuplicateUpstreamSlot(String),
    #[error("at most {maximum} upstream slots are supported")]
    TooManyUpstreamSlots { maximum: usize },
    #[error("at most {maximum} video receive slots are supported")]
    TooManyVideoReceiveSlots { maximum: u8 },
    #[error("at most {maximum} audio receive slots are supported")]
    TooManyAudioReceiveSlots { maximum: u8 },
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct ClientState {
    pub connection: ClientConnectionState,
    pub identity: Option<ConnectionIdentity>,
    pub local_slots: Vec<LocalSlotIntent>,
    pub subscriptions: SubscriptionIntent,
    pub latency: LatencyIntent,
    pub topics: Vec<TopicRegistration>,
}

impl ClientState {
    pub fn validate(&self, topology: &Topology) -> Result<(), StateError> {
        if self.connection == ClientConnectionState::Connected && self.identity.is_none() {
            return Err(StateError::ConnectedWithoutIdentity);
        }
        if let Some(identity) = &self.identity {
            identity.validate()?;
        }

        let mut slots = BTreeSet::new();
        for slot in &self.local_slots {
            if !topology.has_upstream_slot(&slot.slot) {
                return Err(StateError::UnknownUpstreamSlot(slot.slot.clone()));
            }
            if !slots.insert(slot.slot.clone()) {
                return Err(StateError::DuplicateLocalSlot(slot.slot.clone()));
            }
        }

        self.subscriptions.validate()?;
        self.latency.validate()?;

        let mut topics = BTreeSet::new();
        for topic in &self.topics {
            topic.validate()?;
            if !topics.insert(topic.clone()) {
                return Err(StateError::DuplicateTopicRegistration(topic.topic.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum ClientConnectionState {
    Connected,
    #[default]
    Disconnected,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ConnectionIdentity {
    pub room: String,
    pub token: Option<String>,
    pub metadata: Vec<MetadataEntry>,
}

impl ConnectionIdentity {
    fn validate(&self) -> Result<(), StateError> {
        if self.room.is_empty() {
            return Err(StateError::EmptyRoom);
        }
        let mut names = BTreeSet::new();
        for entry in &self.metadata {
            if entry.name.is_empty() {
                return Err(StateError::EmptyMetadataName);
            }
            if !names.insert(entry.name.clone()) {
                return Err(StateError::DuplicateMetadataName(entry.name.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MetadataEntry {
    pub name: String,
    pub value: String,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct LocalSlotIntent {
    pub slot: String,
    pub audio: LocalAudioIntent,
    pub video: LocalVideoIntent,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct LocalAudioIntent {
    pub attached: bool,
    pub muted: bool,
    pub preset: AudioPreset,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct LocalVideoIntent {
    pub attached: bool,
    pub muted: bool,
    pub preset: VideoPreset,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum AudioPreset {
    #[default]
    Speech,
    Music,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum VideoPreset {
    #[default]
    Motion,
    Detail,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct SubscriptionIntent {
    pub video: Vec<VideoRequest>,
    pub audio: AudioIntent,
}

impl SubscriptionIntent {
    fn validate(&self) -> Result<(), StateError> {
        let mut tracks = BTreeSet::new();
        for request in &self.video {
            if request.track_id.is_empty() {
                return Err(StateError::EmptyVideoTrackId);
            }
            if request.min_height > request.target_height {
                return Err(StateError::VideoFloorAboveTarget(request.track_id.clone()));
            }
            if !tracks.insert(request.track_id.clone()) {
                return Err(StateError::DuplicateVideoRequest(request.track_id.clone()));
            }
        }
        if self.audio.pinned.iter().any(String::is_empty) {
            return Err(StateError::EmptyPinnedAudioTrack);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct VideoRequest {
    pub track_id: String,
    pub target_height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct AudioIntent {
    pub pinned: Vec<String>,
    pub auto: bool,
}

impl Default for AudioIntent {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            auto: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum LatencyIntent {
    #[default]
    Adaptive,
    Fixed {
        min_ms: u32,
        max_ms: u32,
    },
}

impl LatencyIntent {
    fn validate(self) -> Result<(), StateError> {
        let Self::Fixed { min_ms, max_ms } = self else {
            return Ok(());
        };
        if min_ms > max_ms {
            return Err(StateError::LatencyMinimumAboveMaximum);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TopicRegistration {
    pub topic: String,
    pub kind: TopicKind,
    pub direction: TopicDirection,
    pub publisher_id: Option<String>,
}

impl TopicRegistration {
    fn validate(&self) -> Result<(), StateError> {
        if self.topic.is_empty() {
            return Err(StateError::EmptyTopic);
        }
        let scoped = self.publisher_id.is_some();
        if scoped && (self.kind != TopicKind::Latest || self.direction != TopicDirection::Subscribe)
        {
            return Err(StateError::InvalidTopicScope);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TopicKind {
    Latest,
    Ordered,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TopicDirection {
    Publish,
    Subscribe,
}

#[derive(thiserror::Error, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum StateError {
    #[error("a connected state requires connection identity")]
    ConnectedWithoutIdentity,
    #[error("room must not be empty")]
    EmptyRoom,
    #[error("metadata name must not be empty")]
    EmptyMetadataName,
    #[error("duplicate metadata name {0}")]
    DuplicateMetadataName(String),
    #[error("unknown upstream slot {0}")]
    UnknownUpstreamSlot(String),
    #[error("duplicate local slot {0}")]
    DuplicateLocalSlot(String),
    #[error("video track id must not be empty")]
    EmptyVideoTrackId,
    #[error("video floor exceeds target for {0}")]
    VideoFloorAboveTarget(String),
    #[error("duplicate video request for {0}")]
    DuplicateVideoRequest(String),
    #[error("pinned audio track id must not be empty")]
    EmptyPinnedAudioTrack,
    #[error("latency minimum exceeds maximum")]
    LatencyMinimumAboveMaximum,
    #[error("fixed latency cannot return to adaptive during a session")]
    LatencyCannotReturnAdaptive,
    #[error("topic must not be empty")]
    EmptyTopic,
    #[error("topic scope is valid only for latest subscribers")]
    InvalidTopicScope,
    #[error("duplicate topic registration for {0}")]
    DuplicateTopicRegistration(String),
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AgentSnapshot {
    pub connection: ConnectionPhase,
    pub participant_id: Option<String>,
    pub publications: Vec<Publication>,
    pub video_bindings: Vec<VideoBinding>,
    pub audio_bindings: Vec<AudioBinding>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum ConnectionPhase {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Publication {
    pub track_id: String,
    pub participant_id: String,
    pub kind: MediaKind,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct VideoBinding {
    pub mid: String,
    pub track_id: String,
    pub paused: bool,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct AudioBinding {
    pub mid: String,
    pub track_id: String,
    pub level_dbov: i32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AgentNotification {
    Connection(ConnectionPhase),
    PublicationAdded(Publication),
    PublicationRemoved { track_id: String },
    VideoBindingsChanged,
    AudioBindingsChanged,
    Error(AgentError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AgentError {
    Protocol(String),
    Terminal(String),
    Topic(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventDisposition {
    Accepted,
    IgnoredStale,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransportDescription {
    pub generation: Generation,
    pub topology: Topology,
    pub signaling_channel: DataChannelId,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NegotiatedTopology {
    pub upstream_slots: Vec<NegotiatedUpstreamSlot>,
    pub video_receive_mids: Vec<String>,
    pub audio_receive_mids: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NegotiatedUpstreamSlot {
    pub slot: String,
    pub audio_mid: String,
    pub video_mid: String,
}

impl NegotiatedTopology {
    pub fn validate(&self, topology: &Topology) -> Result<(), AgentError> {
        if self.upstream_slots.len() != topology.upstream_slots().len()
            || self.video_receive_mids.len() != usize::from(topology.video_receive_slots())
            || self.audio_receive_mids.len() != usize::from(topology.audio_receive_slots())
        {
            return Err(AgentError::Protocol(String::from(
                "negotiated topology shape changed",
            )));
        }
        for (expected, actual) in topology.upstream_slots().iter().zip(&self.upstream_slots) {
            if expected.name() != actual.slot
                || actual.audio_mid.is_empty()
                || actual.video_mid.is_empty()
            {
                return Err(AgentError::Protocol(String::from(
                    "invalid negotiated upstream slot",
                )));
            }
        }
        if self.video_receive_mids.iter().any(String::is_empty)
            || self.audio_receive_mids.iter().any(String::is_empty)
        {
            return Err(AgentError::Protocol(String::from(
                "empty negotiated receive mid",
            )));
        }
        Ok(())
    }
}
