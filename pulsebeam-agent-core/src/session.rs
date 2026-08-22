use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::signaling::{self, server_message};

use crate::types::{MediaKind, ParticipantId, TrackId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationState {
    pub track_id: TrackId,
    pub participant_id: ParticipantId,
    pub kind: MediaKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoBindingState {
    pub mid: String,
    pub track_id: TrackId,
    pub paused: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioBindingState {
    pub mid: String,
    pub track_id: TrackId,
    pub level_dbov: i32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub participants: BTreeSet<ParticipantId>,
    pub publications: BTreeMap<TrackId, PublicationState>,
    pub video_bindings: BTreeMap<String, VideoBindingState>,
    pub audio_bindings: BTreeMap<String, AudioBindingState>,
    pub pending_video_bindings: BTreeMap<String, VideoBindingState>,
    pub pending_audio_bindings: BTreeMap<String, AudioBindingState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    ParticipantAdded(ParticipantId),
    ParticipantRemoved(ParticipantId),
    PublicationAdded(PublicationState),
    PublicationRemoved(TrackId),
    VideoGroupReplaced(Vec<VideoBindingState>),
    AudioGroupReplaced(Vec<AudioBindingState>),
    BindingActivated {
        mid: String,
        track_id: TrackId,
        kind: MediaKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    Decode(String),
    UnexpectedMessage,
    EmptyId(&'static str),
    DuplicateParticipant(ParticipantId),
    DuplicatePublication(TrackId),
    PublicationConflict(TrackId),
    DuplicateBinding(String),
    BindingKindMismatch {
        track_id: TrackId,
        expected: MediaKind,
    },
    UnknownTrackKind(i32),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(formatter, "invalid signaling message: {error}"),
            Self::UnexpectedMessage => formatter.write_str("signaling message has no state"),
            Self::EmptyId(kind) => write!(formatter, "{kind} id must not be empty"),
            Self::DuplicateParticipant(id) => write!(formatter, "duplicate participant {id}"),
            Self::DuplicatePublication(id) => write!(formatter, "duplicate publication {id}"),
            Self::PublicationConflict(id) => write!(formatter, "publication conflict for {id}"),
            Self::DuplicateBinding(mid) => write!(formatter, "duplicate binding for mid {mid}"),
            Self::BindingKindMismatch { track_id, expected } => {
                write!(
                    formatter,
                    "track {track_id} is not an {expected:?} publication"
                )
            }
            Self::UnknownTrackKind(kind) => write!(formatter, "unknown track kind {kind}"),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Clone)]
pub struct SessionReducer {
    participants: BTreeSet<ParticipantId>,
    publications: BTreeMap<TrackId, PublicationState>,
    video_bindings: BTreeMap<String, VideoBindingState>,
    audio_bindings: BTreeMap<String, AudioBindingState>,
}

impl SessionReducer {
    pub fn new() -> Self {
        Self {
            participants: BTreeSet::new(),
            publications: BTreeMap::new(),
            video_bindings: BTreeMap::new(),
            audio_bindings: BTreeMap::new(),
        }
    }

    pub fn apply_message(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>, SessionError> {
        debug_assert!(!bytes.is_empty());
        let message = signaling::ServerMessage::decode(bytes)
            .map_err(|error| SessionError::Decode(error.to_string()))?;
        match message.payload {
            Some(server_message::Payload::State(state)) => self.apply_state(state),
            Some(server_message::Payload::Error(error)) => Err(SessionError::Decode(error)),
            None => Err(SessionError::UnexpectedMessage),
        }
    }

    pub fn apply_state(
        &mut self,
        state: signaling::ServerState,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let backup = self.clone();
        match self.apply_state_inner(state) {
            Ok(events) => Ok(events),
            Err(error) => {
                *self = backup;
                Err(error)
            }
        }
    }

    fn apply_state_inner(
        &mut self,
        state: signaling::ServerState,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let mut events = Vec::new();
        for participant_id in state.participants_removed {
            let participant_id = participant_id_from(participant_id, "participant")?;
            if self.participants.remove(&participant_id) {
                events.push(SessionEvent::ParticipantRemoved(participant_id.clone()));
            }
            let removed: Vec<TrackId> = self
                .publications
                .values()
                .filter(|publication| publication.participant_id == participant_id)
                .map(|publication| publication.track_id.clone())
                .collect();
            for track_id in removed {
                self.remove_publication(&track_id, &mut events);
            }
        }
        for track_id in state.publications_removed {
            let track_id = track_id_from(track_id, "track")?;
            self.remove_publication(&track_id, &mut events);
        }
        for participant in state.participants_added {
            let participant_id = participant_id_from(participant.participant_id, "participant")?;
            if self.participants.insert(participant_id.clone()) {
                events.push(SessionEvent::ParticipantAdded(participant_id));
            }
        }
        for publication in state.publications_added {
            let publication = publication_from_proto(publication)?;
            if let Some(existing) = self.publications.get(&publication.track_id) {
                if existing != &publication {
                    return Err(SessionError::PublicationConflict(publication.track_id));
                }
                continue;
            }
            let track_id = publication.track_id.clone();
            self.publications.insert(track_id, publication.clone());
            events.push(SessionEvent::PublicationAdded(publication.clone()));
            self.activate_bindings(&publication, &mut events);
        }
        if let Some(video) = state.video {
            let bindings = video
                .items
                .into_iter()
                .map(video_binding_from_proto)
                .collect::<Result<Vec<_>, _>>()?;
            validate_unique_mids(bindings.iter().map(|binding| binding.mid.as_str()))?;
            self.video_bindings = bindings
                .iter()
                .cloned()
                .map(|binding| (binding.mid.clone(), binding))
                .collect();
            events.push(SessionEvent::VideoGroupReplaced(bindings));
        }
        if let Some(audio) = state.audio {
            let bindings = audio
                .items
                .into_iter()
                .map(audio_binding_from_proto)
                .collect::<Result<Vec<_>, _>>()?;
            validate_unique_mids(bindings.iter().map(|binding| binding.mid.as_str()))?;
            self.audio_bindings = bindings
                .iter()
                .cloned()
                .map(|binding| (binding.mid.clone(), binding))
                .collect();
            events.push(SessionEvent::AudioGroupReplaced(bindings));
        }
        self.validate_active_bindings()?;
        Ok(events)
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot {
            participants: self.participants.clone(),
            publications: self.publications.clone(),
            ..SessionSnapshot::default()
        };
        for (mid, binding) in &self.video_bindings {
            if self.is_kind(&binding.track_id, MediaKind::Video) {
                snapshot.video_bindings.insert(mid.clone(), binding.clone());
            } else {
                snapshot
                    .pending_video_bindings
                    .insert(mid.clone(), binding.clone());
            }
        }
        for (mid, binding) in &self.audio_bindings {
            if self.is_kind(&binding.track_id, MediaKind::Audio) {
                snapshot.audio_bindings.insert(mid.clone(), binding.clone());
            } else {
                snapshot
                    .pending_audio_bindings
                    .insert(mid.clone(), binding.clone());
            }
        }
        snapshot
    }

    pub fn publication(&self, track_id: &TrackId) -> Option<&PublicationState> {
        self.publications.get(track_id)
    }

    fn remove_publication(&mut self, track_id: &TrackId, events: &mut Vec<SessionEvent>) {
        if self.publications.remove(track_id).is_some() {
            self.video_bindings
                .retain(|_, binding| &binding.track_id != track_id);
            self.audio_bindings
                .retain(|_, binding| &binding.track_id != track_id);
            events.push(SessionEvent::PublicationRemoved(track_id.clone()));
        }
    }

    fn activate_bindings(&self, publication: &PublicationState, events: &mut Vec<SessionEvent>) {
        let mids: Vec<String> = match publication.kind {
            MediaKind::Video => self
                .video_bindings
                .values()
                .filter(|binding| binding.track_id == publication.track_id)
                .map(|binding| binding.mid.clone())
                .collect(),
            MediaKind::Audio => self
                .audio_bindings
                .values()
                .filter(|binding| binding.track_id == publication.track_id)
                .map(|binding| binding.mid.clone())
                .collect(),
            MediaKind::Data => Vec::new(),
        };
        for mid in mids {
            events.push(SessionEvent::BindingActivated {
                mid,
                track_id: publication.track_id.clone(),
                kind: publication.kind,
            });
        }
    }

    fn validate_active_bindings(&self) -> Result<(), SessionError> {
        for binding in self.video_bindings.values() {
            if let Some(publication) = self.publications.get(&binding.track_id)
                && publication.kind != MediaKind::Video
            {
                return Err(SessionError::BindingKindMismatch {
                    track_id: binding.track_id.clone(),
                    expected: MediaKind::Video,
                });
            }
        }
        for binding in self.audio_bindings.values() {
            if let Some(publication) = self.publications.get(&binding.track_id)
                && publication.kind != MediaKind::Audio
            {
                return Err(SessionError::BindingKindMismatch {
                    track_id: binding.track_id.clone(),
                    expected: MediaKind::Audio,
                });
            }
        }
        Ok(())
    }

    fn is_kind(&self, track_id: &TrackId, kind: MediaKind) -> bool {
        self.publications
            .get(track_id)
            .is_some_and(|publication| publication.kind == kind)
    }
}

impl Default for SessionReducer {
    fn default() -> Self {
        Self::new()
    }
}

fn participant_id_from(value: String, field: &'static str) -> Result<ParticipantId, SessionError> {
    if value.is_empty() {
        return Err(SessionError::EmptyId(field));
    }
    Ok(ParticipantId::from(value))
}

fn track_id_from(value: String, field: &'static str) -> Result<TrackId, SessionError> {
    if value.is_empty() {
        return Err(SessionError::EmptyId(field));
    }
    Ok(TrackId::from(value))
}

fn publication_from_proto(
    publication: signaling::Publication,
) -> Result<PublicationState, SessionError> {
    let track_id = track_id_from(publication.track_id, "track")?;
    let participant_id = participant_id_from(publication.participant_id, "participant")?;
    let kind = match signaling::TrackKind::try_from(publication.kind) {
        Ok(signaling::TrackKind::Video) => MediaKind::Video,
        Ok(signaling::TrackKind::Audio) => MediaKind::Audio,
        Ok(signaling::TrackKind::Unspecified) | Err(_) => {
            return Err(SessionError::UnknownTrackKind(publication.kind));
        }
    };
    Ok(PublicationState {
        track_id,
        participant_id,
        kind,
    })
}

fn video_binding_from_proto(
    binding: signaling::VideoBinding,
) -> Result<VideoBindingState, SessionError> {
    let track_id = track_id_from(binding.track_id, "track")?;
    if binding.mid.is_empty() {
        return Err(SessionError::EmptyId("mid"));
    }
    Ok(VideoBindingState {
        mid: binding.mid,
        track_id,
        paused: binding.paused,
    })
}

fn audio_binding_from_proto(
    binding: signaling::AudioBinding,
) -> Result<AudioBindingState, SessionError> {
    let track_id = track_id_from(binding.track_id, "track")?;
    if binding.mid.is_empty() {
        return Err(SessionError::EmptyId("mid"));
    }
    Ok(AudioBindingState {
        mid: binding.mid,
        track_id,
        level_dbov: binding.level_dbov,
    })
}

fn validate_unique_mids<'a>(mids: impl Iterator<Item = &'a str>) -> Result<(), SessionError> {
    let mut seen = BTreeSet::new();
    for mid in mids {
        if !seen.insert(mid) {
            return Err(SessionError::DuplicateBinding(mid.to_owned()));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_utils::channel;

    fn publication(
        track_id: &str,
        participant_id: &str,
        kind: signaling::TrackKind,
    ) -> signaling::Publication {
        signaling::Publication {
            track_id: track_id.to_owned(),
            participant_id: participant_id.to_owned(),
            kind: kind as i32,
        }
    }

    #[test]
    fn binding_before_publication_becomes_active_once() {
        let mut reducer = SessionReducer::new();
        reducer
            .apply_state(signaling::ServerState {
                video: Some(signaling::VideoBindings {
                    items: vec![signaling::VideoBinding {
                        track_id: "track".to_owned(),
                        mid: "0".to_owned(),
                        paused: false,
                    }],
                }),
                ..signaling::ServerState::default()
            })
            .unwrap();
        assert!(reducer.snapshot().video_bindings.is_empty());
        assert_eq!(reducer.snapshot().pending_video_bindings.len(), 1);
        let events = reducer
            .apply_state(signaling::ServerState {
                publications_added: vec![publication(
                    "track",
                    "alice",
                    signaling::TrackKind::Video,
                )],
                ..signaling::ServerState::default()
            })
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::BindingActivated { mid, .. } if mid == "0"
        )));
        assert_eq!(reducer.snapshot().video_bindings.len(), 1);
    }

    #[test]
    fn present_empty_group_clears_previous_bindings() {
        let mut reducer = SessionReducer::new();
        reducer
            .apply_state(signaling::ServerState {
                publications_added: vec![publication(
                    "track",
                    "alice",
                    signaling::TrackKind::Video,
                )],
                video: Some(signaling::VideoBindings {
                    items: vec![signaling::VideoBinding {
                        track_id: "track".to_owned(),
                        mid: "0".to_owned(),
                        paused: false,
                    }],
                }),
                ..signaling::ServerState::default()
            })
            .unwrap();
        reducer
            .apply_state(signaling::ServerState {
                video: Some(signaling::VideoBindings { items: Vec::new() }),
                ..signaling::ServerState::default()
            })
            .unwrap();
        assert!(reducer.snapshot().video_bindings.is_empty());
    }

    #[test]
    fn server_message_golden_vector_is_stable() {
        let message = signaling::ServerMessage {
            payload: Some(signaling::server_message::Payload::State(
                signaling::ServerState {
                    participants_added: vec![signaling::Participant {
                        participant_id: channel("alice").into_inner(),
                    }],
                    ..signaling::ServerState::default()
                },
            )),
        };
        assert_eq!(
            message.encode_to_vec(),
            vec![10, 9, 10, 7, 10, 5, 97, 108, 105, 99, 101]
        );
    }
}
