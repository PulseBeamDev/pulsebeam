use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::{Generation, TopicNotification, TopicRegistrations, TopicSnapshot};

pub const MAX_MID_BYTES: usize = 16;

const MAX_AUTHORIZATION_TOKEN_BYTES: usize = 8_192;

/// Preflight credential input before a host retains or queues it.
/// Agent commands revalidate when they apply the credential intent.
pub fn validate_authorization_token(token: &str) -> Result<(), ValidationError> {
    if token.is_empty()
        || token.len() > MAX_AUTHORIZATION_TOKEN_BYTES
        || token.bytes().any(|byte| !(b'!'..=b'~').contains(&byte))
    {
        return Err(ValidationError::Token);
    }
    Ok(())
}

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
    pub track_id: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSubscription {
    pub slot: u32,
    pub publication_id: String,
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
    pub playout_delays: BTreeMap<String, PlayoutDelay>,
    pub selector_delays: BTreeMap<TrackSelector, PlayoutDelay>,
}

impl AudioSubscription {
    pub(crate) fn has_demand(&self) -> bool {
        !self.pinned.is_empty() || !self.selected.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackSelector {
    pub room: String,
    pub participant_id: String,
    pub track_id: String,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub id: String,
    pub participant_id: String,
    pub kind: MediaKind,
    pub track_id: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStatus {
    Accepted,
    Rejected,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialOutcome {
    pub revision: u64,
    pub status: CredentialStatus,
    pub error: Option<Failure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u64,
    pub desired_revision: u64,
    pub join_revision: u64,
    pub evaluated_join_revision: u64,
    pub accepted_join_revision: Option<u64>,
    pub credential_revision: u64,
    pub accepted_credential_revision: Option<u64>,
    pub credential_outcome: Option<CredentialOutcome>,
    pub desired_room: Option<String>,
    pub connection: ConnectionState,
    pub generation: Option<Generation>,
    pub connection_id: Option<crate::ConnectionId>,
    pub participant_id: Option<String>,
    pub room_id: Option<String>,
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
            join_revision: 0,
            evaluated_join_revision: 0,
            accepted_join_revision: None,
            credential_revision: 0,
            accepted_credential_revision: None,
            credential_outcome: None,
            desired_room: None,
            connection: ConnectionState::Disconnected,
            generation: None,
            connection_id: None,
            participant_id: None,
            room_id: None,
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
    CatalogWatchChanged {
        owner: u64,
    },
    CatalogWatchTerminated {
        owner: u64,
        state: crate::ffi::WatchState,
    },
    DataReady {
        owners: Vec<u64>,
    },
    DataSendFinished {
        send: crate::OperationId,
        error: Option<crate::DataSendFailure>,
    },
    ReliableData {
        track_id: String,
        generation: crate::Generation,
        source: u64,
        event: crate::ffi::ReliablePublishEvent,
    },
    Topic(TopicNotification),
    Failure(Failure),
    ServerError(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("endpoint must be an absolute HTTP(S) URL")]
    Endpoint,
    #[error("token must be an HTTP bearer value of at most 8192 bytes")]
    Token,
    #[error("{field} is invalid")]
    Identifier { field: &'static str },
    #[error("duplicate {field}: {value}")]
    Duplicate { field: &'static str, value: String },
    #[error("local publication binding conflicts with a reserved slot or track ID: {0}")]
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
        validate_authorization_token(&self.token)?;
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
        let mut track_ids = BTreeSet::new();
        for publication in &self.publications {
            validate_identifier("publication slot", &publication.slot, 256, false)?;
            validate_track_id(&publication.track_id)?;
            if !publications.insert(publication.media_slot()) {
                return Err(ValidationError::Duplicate {
                    field: "publication slot",
                    value: publication.slot.clone(),
                });
            }
            if !track_ids.insert((publication.kind, publication.track_id.clone())) {
                return Err(ValidationError::Duplicate {
                    field: "publication track ID",
                    value: publication.track_id.clone(),
                });
            }
        }
        let mut video_slots = BTreeSet::new();
        let mut video_tracks = BTreeSet::new();
        for video in &self.video {
            if let Some(selector) = &video.selector {
                if !video.publication_id.is_empty() {
                    return Err(ValidationError::Identifier {
                        field: "video publication_id with selector",
                    });
                }
                selector.validate()?;
            } else {
                validate_identifier("video publication_id", &video.publication_id, 256, true)?;
            }
            if !video_slots.insert(video.slot) {
                return Err(ValidationError::Duplicate {
                    field: "video slot",
                    value: video.slot.to_string(),
                });
            }
            if !video_tracks.insert((video.selector.as_ref(), video.publication_id.as_str())) {
                return Err(ValidationError::Duplicate {
                    field: "video track",
                    value: video.publication_id.clone(),
                });
            }
        }
        let mut pins = BTreeSet::new();
        for publication_id in &self.audio.pinned {
            validate_identifier("audio publication_id", publication_id, 256, true)?;
            if !pins.insert(publication_id.clone()) {
                return Err(ValidationError::Duplicate {
                    field: "audio pin",
                    value: publication_id.clone(),
                });
            }
        }
        let mut selectors = BTreeSet::new();
        for selector in &self.audio.selected {
            selector.validate()?;
            if !selectors.insert(selector) {
                return Err(ValidationError::Duplicate {
                    field: "audio selector",
                    value: alloc::format!("{}:{}", selector.participant_id, selector.track_id),
                });
            }
        }
        for publication_id in self.audio.playout_delays.keys() {
            if !pins.contains(publication_id) {
                return Err(ValidationError::UnknownAudioTrack(publication_id.clone()));
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
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_room_name(&self.room)?;
        validate_participant_id(&self.participant_id)?;
        validate_track_id(&self.track_id)
    }
}

pub fn validate_room_name(room: &str) -> Result<(), ValidationError> {
    validate_application_name("room ID", room)
}

pub fn validate_participant_id(id: &str) -> Result<(), ValidationError> {
    validate_application_name("participant ID", id)
}

pub(crate) fn validate_track_id(id: &str) -> Result<(), ValidationError> {
    validate_application_name("track ID", id)
}

fn validate_application_name(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if !pulsebeam_proto::state::validation::is_application_name(value) {
        return Err(ValidationError::Identifier { field });
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
