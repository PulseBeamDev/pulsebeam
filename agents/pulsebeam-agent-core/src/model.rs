use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::{Generation, HttpHeader, TopicNotification, TopicRegistrations, TopicSnapshot};

pub const MAX_LOCAL_VIDEO_SLOTS: usize = 2;
pub const MAX_LOCAL_AUDIO_SLOTS: usize = 2;
pub const MAX_REMOTE_VIDEO_SLOTS: u8 = 7;
pub const MAX_REMOTE_AUDIO_SLOTS: u8 = 3;
pub const MAX_MID_BYTES: usize = 16;
pub const MAX_PLAYOUT_DELAY_MS: u32 = 40_950;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfig {
    pub endpoint: String,
    pub room_id: String,
    pub request_headers: Vec<HttpHeader>,
    pub topology: MediaTopology,
    pub manual_subscriptions: bool,
    pub retry: RetryPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
    pub maximum_attempts: u8,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            maximum_delay: Duration::from_secs(5),
            maximum_attempts: 10,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaTopology {
    pub local_video: Vec<String>,
    pub local_audio: Vec<String>,
    pub remote_video: u8,
    pub remote_audio: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredState {
    pub revision: u64,
    pub connected: bool,
    pub publications: Vec<PublicationIntent>,
    pub video: Vec<VideoSubscription>,
    pub audio: AudioSubscription,
    pub playout_delay: PlayoutDelay,
    pub topics: TopicRegistrations,
}

impl Default for DesiredState {
    fn default() -> Self {
        Self {
            revision: 0,
            connected: false,
            publications: Vec::new(),
            video: Vec::new(),
            audio: AudioSubscription::default(),
            playout_delay: PlayoutDelay::Adaptive,
            topics: TopicRegistrations::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationIntent {
    pub slot: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSubscription {
    pub slot: u8,
    pub track_id: String,
    pub height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSubscription {
    pub pinned: Vec<String>,
    pub automatic: bool,
}

impl Default for AudioSubscription {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            automatic: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayoutDelay {
    #[default]
    Adaptive,
    Fixed {
        min_ms: u32,
        max_ms: u32,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaSlot {
    LocalVideo(String),
    LocalAudio(String),
    RemoteVideo(u8),
    RemoteAudio(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotBinding {
    pub slot: MediaSlot,
    pub mid: String,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub id: String,
    pub participant_id: String,
    pub kind: MediaKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoBinding {
    pub track_id: String,
    pub mid: String,
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioBinding {
    pub track_id: String,
    pub mid: String,
    pub level_dbov: i8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    CreatingOffer,
    Joining,
    ApplyingAnswer,
    WaitingForTransport,
    WaitingForSignaling,
    Connected,
    Reconnecting,
    RetryWaiting { attempt: u8, after: Duration },
    Closing,
    TerminalFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u64,
    pub desired_revision: u64,
    pub connection: ConnectionState,
    pub generation: Option<Generation>,
    pub participant_id: Option<String>,
    pub participants: BTreeMap<String, Participant>,
    pub publications: BTreeMap<String, Publication>,
    pub video: BTreeMap<String, VideoBinding>,
    pub audio: Vec<AudioBinding>,
    pub topics: TopicSnapshot,
    pub terminal_failure: Option<Failure>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 0,
            desired_revision: 0,
            connection: ConnectionState::Disconnected,
            generation: None,
            participant_id: None,
            participants: BTreeMap::new(),
            publications: BTreeMap::new(),
            video: BTreeMap::new(),
            audio: Vec::new(),
            topics: TopicSnapshot::default(),
            terminal_failure: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    InvalidConfiguration,
    Authorization,
    Protocol,
    Transient,
    ResourceExpired,
    RetryExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub class: FailureClass,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    ConnectionStateChanged {
        from: ConnectionState,
        to: ConnectionState,
    },
    ParticipantAdded(Participant),
    ParticipantRemoved(String),
    PublicationAdded(Publication),
    PublicationRemoved(String),
    VideoBindingChanged {
        mid: String,
        binding: Option<VideoBinding>,
    },
    AudioBindingsChanged(Vec<AudioBinding>),
    Topic(TopicNotification),
    Failure(Failure),
    ServerError(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("endpoint must be an absolute HTTP(S) URL")]
    Endpoint,
    #[error("{field} is invalid")]
    Identifier { field: &'static str },
    #[error("duplicate {field}: {value}")]
    Duplicate { field: &'static str, value: String },
    #[error("{kind} slot count {actual} exceeds {maximum}")]
    SlotLimit {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("unknown local publication slot: {0}")]
    UnknownPublicationSlot(String),
    #[error("remote video slot {slot} is outside topology capacity {capacity}")]
    UnknownVideoSlot { slot: u8, capacity: u8 },
    #[error("minimum video height exceeds target height")]
    VideoHeight,
    #[error("playout delay bounds are invalid")]
    PlayoutDelay,
    #[error("retry policy is invalid")]
    RetryPolicy,
    #[error("request header is invalid or protocol-owned: {0}")]
    RequestHeader(String),
    #[error("topic name is invalid: {0}")]
    Topic(String),
    #[error("topic publisher scope is invalid: {0}")]
    TopicScope(String),
    #[error("topic registrations exceed the {maximum}-channel limit: {actual}")]
    TopicChannelLimit { actual: usize, maximum: usize },
}

impl AgentConfig {
    pub(crate) fn validate(&mut self) -> Result<(), ValidationError> {
        while self.endpoint.ends_with('/') {
            let _ = self.endpoint.pop();
        }
        let authority = self
            .endpoint
            .strip_prefix("http://")
            .or_else(|| self.endpoint.strip_prefix("https://"))
            .and_then(|rest| rest.split('/').next());
        if authority.is_none_or(str::is_empty)
            || self.endpoint.chars().any(char::is_whitespace)
            || self.endpoint.contains('?')
            || self.endpoint.contains('#')
        {
            return Err(ValidationError::Endpoint);
        }
        validate_identifier("room_id", &self.room_id, 256, false)?;
        for header in &self.request_headers {
            validate_request_header(header)?;
        }
        self.topology.validate()?;
        if self.retry.maximum_attempts == 0
            || self.retry.initial_delay > self.retry.maximum_delay
            || self.retry.maximum_delay == Duration::ZERO
        {
            return Err(ValidationError::RetryPolicy);
        }
        Ok(())
    }
}

fn validate_request_header(header: &HttpHeader) -> Result<(), ValidationError> {
    let name = header.name.as_str();
    let protocol_owned = ["content-type", "content-length", "host", "if-match"];
    let valid_name = !name.is_empty()
        && name.is_ascii()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        });
    let valid_value = header
        .value
        .bytes()
        .all(|byte| byte == b'\t' || (b' '..=b'~').contains(&byte));
    if !valid_name
        || !valid_value
        || protocol_owned
            .iter()
            .any(|owned| name.eq_ignore_ascii_case(owned))
    {
        return Err(ValidationError::RequestHeader(header.name.clone()));
    }
    Ok(())
}

impl MediaTopology {
    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        validate_limit("local video", self.local_video.len(), MAX_LOCAL_VIDEO_SLOTS)?;
        validate_limit("local audio", self.local_audio.len(), MAX_LOCAL_AUDIO_SLOTS)?;
        validate_limit(
            "remote video",
            usize::from(self.remote_video),
            usize::from(MAX_REMOTE_VIDEO_SLOTS),
        )?;
        validate_limit(
            "remote audio",
            usize::from(self.remote_audio),
            usize::from(MAX_REMOTE_AUDIO_SLOTS),
        )?;
        let mut names = BTreeSet::new();
        for name in self.local_video.iter().chain(&self.local_audio) {
            validate_identifier("slot name", name, 64, false)?;
            if !names.insert(name.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "slot name",
                    value: name.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn slots(&self) -> Vec<MediaSlot> {
        let mut slots = Vec::with_capacity(
            self.local_video
                .len()
                .saturating_add(self.local_audio.len())
                .saturating_add(usize::from(self.remote_video))
                .saturating_add(usize::from(self.remote_audio)),
        );
        slots.extend(self.local_video.iter().cloned().map(MediaSlot::LocalVideo));
        slots.extend(self.local_audio.iter().cloned().map(MediaSlot::LocalAudio));
        slots.extend((0..self.remote_video).map(MediaSlot::RemoteVideo));
        slots.extend((0..self.remote_audio).map(MediaSlot::RemoteAudio));
        slots
    }
}

impl DesiredState {
    pub(crate) fn normalize(&mut self) {
        self.topics.normalize();
    }

    pub(crate) fn validate(&self, topology: &MediaTopology) -> Result<(), ValidationError> {
        let local: BTreeSet<&str> = topology
            .local_video
            .iter()
            .chain(&topology.local_audio)
            .map(String::as_str)
            .collect();
        let mut publications = BTreeSet::new();
        for publication in &self.publications {
            if !local.contains(publication.slot.as_str()) {
                return Err(ValidationError::UnknownPublicationSlot(
                    publication.slot.clone(),
                ));
            }
            if !publications.insert(publication.slot.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "publication slot",
                    value: publication.slot.clone(),
                });
            }
        }
        let mut video_slots = BTreeSet::new();
        let mut video_tracks = BTreeSet::new();
        for video in &self.video {
            validate_identifier("video track_id", &video.track_id, 256, true)?;
            if video.slot >= topology.remote_video {
                return Err(ValidationError::UnknownVideoSlot {
                    slot: video.slot,
                    capacity: topology.remote_video,
                });
            }
            if video.min_height > video.height || (video.height == 0 && video.min_height != 0) {
                return Err(ValidationError::VideoHeight);
            }
            if !video_slots.insert(video.slot) {
                return Err(ValidationError::Duplicate {
                    field: "video slot",
                    value: video.slot.to_string(),
                });
            }
            if !video_tracks.insert(video.track_id.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "video track",
                    value: video.track_id.clone(),
                });
            }
        }
        let mut pins = BTreeSet::new();
        for track_id in &self.audio.pinned {
            validate_identifier("audio track_id", track_id, 256, true)?;
            if !pins.insert(track_id.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "audio pin",
                    value: track_id.clone(),
                });
            }
        }
        if let PlayoutDelay::Fixed { min_ms, max_ms } = self.playout_delay
            && (min_ms > max_ms || max_ms > MAX_PLAYOUT_DELAY_MS)
        {
            return Err(ValidationError::PlayoutDelay);
        }
        self.topics.validate()?;
        Ok(())
    }
}

fn validate_limit(
    kind: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), ValidationError> {
    if actual > maximum {
        return Err(ValidationError::SlotLimit {
            kind,
            actual,
            maximum,
        });
    }
    Ok(())
}

pub(crate) fn validate_identifier(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_slash: bool,
) -> Result<(), ValidationError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
        || (!allow_slash && value.contains('/'))
    {
        return Err(ValidationError::Identifier { field });
    }
    Ok(())
}
