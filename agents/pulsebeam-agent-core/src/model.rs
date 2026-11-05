use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::{Generation, TopicNotification, TopicRegistrations, TopicSnapshot};

pub const MAX_MID_BYTES: usize = 16;

#[derive(Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub endpoint: String,
    pub token: String,
    pub retry: RetryPolicy,
    pub log_level: LogLevel,
}

impl core::fmt::Debug for AgentConfig {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("AgentConfig")
            .field("endpoint", &self.endpoint)
            .field("token", &"[REDACTED]")
            .field("retry", &self.retry)
            .field("log_level", &self.log_level)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Off,
    Error,
    #[default]
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub(crate) fn allows(self, level: log::Level) -> bool {
        match level {
            log::Level::Error => self >= Self::Error,
            log::Level::Warn => self >= Self::Warn,
            log::Level::Info => self >= Self::Info,
            log::Level::Debug => self >= Self::Debug,
            log::Level::Trace => self >= Self::Trace,
        }
    }
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
pub struct DesiredState {
    pub revision: u64,
    pub connected: bool,
    pub publications: Vec<PublicationIntent>,
    pub video: Vec<VideoSubscription>,
    pub audio: AudioSubscription,
    pub topics: TopicRegistrations,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationIntent {
    pub slot: String,
    pub kind: MediaKind,
    pub label: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSubscription {
    pub slot: u32,
    pub track_id: String,
    pub selector: Option<TrackSelector>,
    pub height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
    pub playout_delay: PlayoutDelay,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioSubscription {
    pub pinned: Vec<String>,
    pub selected: Vec<TrackSelector>,
    pub automatic: bool,
    pub playout_delays: BTreeMap<String, PlayoutDelay>,
    pub selector_delays: BTreeMap<TrackSelector, PlayoutDelay>,
}

impl AudioSubscription {
    pub(crate) fn has_demand(&self) -> bool {
        self.automatic || !self.pinned.is_empty() || !self.selected.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackSelector {
    pub participant_external_id: String,
    pub label: String,
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
    RemoteVideo(u32),
    RemoteAudio(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotBinding {
    pub slot: MediaSlot,
    pub mid: String,
    pub media_index: u32,
    pub kind: MediaKind,
    pub direction: MediaDirection,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaDirection {
    SendOnly,
    ReceiveOnly,
}

impl MediaSlot {
    pub const fn kind(&self) -> MediaKind {
        match self {
            Self::LocalVideo(_) | Self::RemoteVideo(_) => MediaKind::Video,
            Self::LocalAudio(_) | Self::RemoteAudio(_) => MediaKind::Audio,
        }
    }

    pub const fn direction(&self) -> MediaDirection {
        match self {
            Self::LocalVideo(_) | Self::LocalAudio(_) => MediaDirection::SendOnly,
            Self::RemoteVideo(_) | Self::RemoteAudio(_) => MediaDirection::ReceiveOnly,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
    pub external_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub id: String,
    pub participant_id: String,
    pub kind: MediaKind,
    pub label: String,
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
    pub participant_external_id: Option<String>,
    pub room_external_id: Option<String>,
    pub authorization_expires_at: Option<i64>,
    pub catalog_revision: u64,
    pub accepted_intent_revision: u64,
    pub video_mapping: BTreeMap<u32, String>,
    pub audio_mapping: BTreeMap<u32, String>,
    pub participants: BTreeMap<String, Participant>,
    pub publications: BTreeMap<String, Publication>,
    pub remote_tracks: BTreeMap<String, crate::RemoteTrack>,
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
            participant_external_id: None,
            room_external_id: None,
            authorization_expires_at: None,
            catalog_revision: 0,
            accepted_intent_revision: 0,
            video_mapping: BTreeMap::new(),
            audio_mapping: BTreeMap::new(),
            participants: BTreeMap::new(),
            publications: BTreeMap::new(),
            remote_tracks: BTreeMap::new(),
            topics: TopicSnapshot::default(),
            terminal_failure: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    InvalidConfiguration,
    Authorization,
    /// This Agent lost participant ownership. Start a new Agent to reclaim it.
    ParticipantSuperseded,
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
    SnapshotChanged,
    DataReady {
        owners: Vec<u64>,
    },
    DataSendFinished {
        send: crate::OperationId,
        error: Option<crate::DataSendFailure>,
    },
    Topic(TopicNotification),
    Failure(Failure),
    ServerError(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("endpoint must be an absolute HTTP(S) URL")]
    Endpoint,
    #[error("token must be a non-empty HTTP bearer value")]
    Token,
    #[error("{field} is invalid")]
    Identifier { field: &'static str },
    #[error("duplicate {field}: {value}")]
    Duplicate { field: &'static str, value: String },
    #[error("local publication binding conflicts with a reserved slot or label: {0}")]
    PublicationBinding(String),
    #[error("audio playout policy refers to an unpinned track: {0}")]
    UnknownAudioTrack(String),
    #[error("retry policy is invalid")]
    RetryPolicy,
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
        if self.token.is_empty()
            || self
                .token
                .bytes()
                .any(|byte| !(b'!'..=b'~').contains(&byte))
        {
            return Err(ValidationError::Token);
        }
        if self.retry.maximum_attempts == 0
            || self.retry.initial_delay > self.retry.maximum_delay
            || self.retry.maximum_delay == Duration::ZERO
        {
            return Err(ValidationError::RetryPolicy);
        }
        Ok(())
    }
}

impl PublicationIntent {
    pub(crate) fn media_slot(&self) -> MediaSlot {
        match self.kind {
            MediaKind::Video => MediaSlot::LocalVideo(self.slot.clone()),
            MediaKind::Audio => MediaSlot::LocalAudio(self.slot.clone()),
        }
    }
}

impl DesiredState {
    pub(crate) fn normalize(&mut self) {
        self.topics.normalize();
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        let mut publications = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for publication in &self.publications {
            validate_identifier("publication slot", &publication.slot, 256, false)?;
            if publication.label.is_empty() || publication.label.len() > 256 {
                return Err(ValidationError::Identifier {
                    field: "publication label",
                });
            }
            if !publications.insert(publication.media_slot()) {
                return Err(ValidationError::Duplicate {
                    field: "publication slot",
                    value: publication.slot.clone(),
                });
            }
            if !labels.insert((publication.kind, publication.label.clone())) {
                return Err(ValidationError::Duplicate {
                    field: "publication label",
                    value: publication.label.clone(),
                });
            }
        }
        let mut video_slots = BTreeSet::new();
        let mut video_tracks = BTreeSet::new();
        for video in &self.video {
            if let Some(selector) = &video.selector {
                if !video.track_id.is_empty() {
                    return Err(ValidationError::Identifier {
                        field: "video track_id with selector",
                    });
                }
                selector.validate()?;
            } else {
                validate_identifier("video track_id", &video.track_id, 256, true)?;
            }
            if !video_slots.insert(video.slot) {
                return Err(ValidationError::Duplicate {
                    field: "video slot",
                    value: video.slot.to_string(),
                });
            }
            if !video_tracks.insert((video.selector.as_ref(), video.track_id.as_str())) {
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
        let mut selectors = BTreeSet::new();
        for selector in &self.audio.selected {
            selector.validate()?;
            if !selectors.insert(selector) {
                return Err(ValidationError::Duplicate {
                    field: "audio selector",
                    value: alloc::format!(
                        "{}:{}",
                        selector.participant_external_id,
                        selector.label
                    ),
                });
            }
        }
        for track_id in self.audio.playout_delays.keys() {
            if !pins.contains(track_id) {
                return Err(ValidationError::UnknownAudioTrack(track_id.clone()));
            }
        }
        for selector in self.audio.selector_delays.keys() {
            if !selectors.contains(selector) {
                return Err(ValidationError::Identifier {
                    field: "audio selector playout policy",
                });
            }
        }
        self.topics.validate()?;
        Ok(())
    }
}

impl TrackSelector {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_participant_id(&self.participant_external_id)?;
        validate_identifier("track label", &self.label, 64, true)
    }
}

pub fn validate_participant_id(id: &str) -> Result<(), ValidationError> {
    validate_identifier("participant external id", id, 256, true)
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
