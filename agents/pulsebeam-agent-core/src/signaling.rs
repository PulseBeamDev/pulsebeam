use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};

use pulsebeam_proto::{
    prelude::Message,
    signaling::{self, client_message, server_message},
};

use crate::{
    AudioBinding, DesiredState, MediaKind, MediaSlot, MediaTopology, Notification, Participant,
    PlayoutDelay, Publication, Snapshot, VideoBinding, validate_identifier,
};

pub(crate) const SIGNALING_LABEL: &str = "v1/sys/signaling";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    #[error("signaling message is malformed")]
    Malformed,
    #[error("signaling message has no payload")]
    MissingPayload,
    #[error("signaling state contains an invalid {0}")]
    Invalid(&'static str),
    #[error("signaling state contains duplicate {field}: {value}")]
    Duplicate { field: &'static str, value: String },
    #[error("signaling state references unknown {field}: {value}")]
    Unknown { field: &'static str, value: String },
    #[error("media topology has no negotiated mid for {0:?}")]
    MissingMid(MediaSlot),
}

pub(crate) enum ServerOutput {
    StateChanged,
    ServerError(String),
}

pub(crate) fn encode_intent(
    desired: &DesiredState,
    topology: &MediaTopology,
    mids: &BTreeMap<MediaSlot, String>,
) -> Result<Vec<u8>, SignalingError> {
    let active: BTreeMap<&str, bool> = desired
        .publications
        .iter()
        .map(|publication| (publication.slot.as_str(), publication.active))
        .collect();
    let mut publish = Vec::with_capacity(
        topology
            .local_video
            .len()
            .saturating_add(topology.local_audio.len()),
    );
    for slot in &topology.local_video {
        let media_slot = MediaSlot::LocalVideo(slot.clone());
        publish.push(signaling::PublishIntent {
            mid: mid_for(mids, &media_slot)?.to_string(),
            active: active.get(slot.as_str()).copied().unwrap_or(false),
        });
    }
    for slot in &topology.local_audio {
        let media_slot = MediaSlot::LocalAudio(slot.clone());
        publish.push(signaling::PublishIntent {
            mid: mid_for(mids, &media_slot)?.to_string(),
            active: active.get(slot.as_str()).copied().unwrap_or(false),
        });
    }

    let mut video = Vec::with_capacity(desired.video.len());
    for subscription in &desired.video {
        video.push(signaling::VideoIntent {
            mid: mid_for(mids, &MediaSlot::RemoteVideo(subscription.slot))?.to_string(),
            track_id: subscription.track_id.clone(),
            height: subscription.height,
            min_height: subscription.min_height,
            min_fps: subscription.min_fps,
            priority: subscription.priority,
        });
    }

    let ext = match desired.playout_delay {
        PlayoutDelay::Adaptive => None,
        PlayoutDelay::Fixed { min_ms, max_ms } => Some(signaling::Extensions {
            playout_delay: Some(signaling::PlayoutDelay { min_ms, max_ms }),
        }),
    };
    let message = signaling::ClientMessage {
        payload: Some(client_message::Payload::Intent(signaling::ClientIntent {
            video,
            audio: Some(signaling::AudioIntent {
                pinned: desired.audio.pinned.clone(),
                auto: desired.audio.automatic,
            }),
            publish,
            ext,
        })),
    };
    Ok(message.encode_to_vec())
}

pub(crate) fn apply_server_message(
    payload: &[u8],
    snapshot: &mut Snapshot,
    notifications: &mut alloc::collections::VecDeque<Notification>,
    mids: &BTreeMap<MediaSlot, String>,
) -> Result<ServerOutput, SignalingError> {
    let message =
        signaling::ServerMessage::decode(payload).map_err(|_| SignalingError::Malformed)?;
    let Some(payload) = message.payload else {
        return Err(SignalingError::MissingPayload);
    };
    match payload {
        server_message::Payload::Error(message) => {
            if message.is_empty() || message.chars().any(char::is_control) {
                return Err(SignalingError::Invalid("server error"));
            }
            Ok(ServerOutput::ServerError(message))
        }
        server_message::Payload::State(state) => {
            apply_state(state, snapshot, notifications, mids)?;
            Ok(ServerOutput::StateChanged)
        }
    }
}

fn apply_state(
    state: signaling::ServerState,
    snapshot: &mut Snapshot,
    notifications: &mut alloc::collections::VecDeque<Notification>,
    mids: &BTreeMap<MediaSlot, String>,
) -> Result<(), SignalingError> {
    validate_update_shape(&state)?;

    let mut participants = if state.snapshot {
        BTreeMap::new()
    } else {
        snapshot.participants.clone()
    };
    let mut publications = if state.snapshot {
        BTreeMap::new()
    } else {
        snapshot.publications.clone()
    };
    let mut video = snapshot.video.clone();
    let mut audio = snapshot.audio.clone();

    for id in &state.participants_removed {
        let _ = participants.remove(id);
    }
    for id in &state.publications_removed {
        let _ = publications.remove(id);
    }
    for participant in state.participants_added {
        validate_wire_id("participant_id", &participant.participant_id)?;
        participants.insert(
            participant.participant_id.clone(),
            Participant {
                id: participant.participant_id,
            },
        );
    }
    for publication in state.publications_added {
        validate_wire_id("track_id", &publication.track_id)?;
        validate_wire_id("participant_id", &publication.participant_id)?;
        let kind = match signaling::TrackKind::try_from(publication.kind) {
            Ok(signaling::TrackKind::Video) => MediaKind::Video,
            Ok(signaling::TrackKind::Audio) => MediaKind::Audio,
            Ok(signaling::TrackKind::Unspecified) | Err(_) => {
                return Err(SignalingError::Invalid("publication kind"));
            }
        };
        publications.insert(
            publication.track_id.clone(),
            Publication {
                id: publication.track_id,
                participant_id: publication.participant_id,
                kind,
            },
        );
    }

    for publication in publications.values() {
        if !participants.contains_key(&publication.participant_id) {
            return Err(SignalingError::Unknown {
                field: "publication participant",
                value: publication.participant_id.clone(),
            });
        }
    }

    video.retain(|_, binding| publications.contains_key(&binding.track_id));
    audio.retain(|binding| publications.contains_key(&binding.track_id));

    if let Some(bindings) = state.video {
        video = validate_video_bindings(bindings.items, &publications, mids)?;
    }
    if let Some(bindings) = state.audio {
        audio = validate_audio_bindings(bindings.items, &publications, mids)?;
    }

    emit_participant_changes(&snapshot.participants, &participants, notifications);
    emit_publication_changes(&snapshot.publications, &publications, notifications);
    emit_video_changes(&snapshot.video, &video, notifications);
    if snapshot.audio != audio {
        notifications.push_back(Notification::AudioBindingsChanged(audio.clone()));
    }

    snapshot.participants = participants;
    snapshot.publications = publications;
    snapshot.video = video;
    snapshot.audio = audio;
    snapshot.version = snapshot.version.saturating_add(1);
    Ok(())
}

fn validate_update_shape(state: &signaling::ServerState) -> Result<(), SignalingError> {
    reject_duplicates(
        "participant addition",
        state
            .participants_added
            .iter()
            .map(|participant| participant.participant_id.as_str()),
    )?;
    reject_duplicates(
        "participant removal",
        state.participants_removed.iter().map(String::as_str),
    )?;
    reject_duplicates(
        "publication addition",
        state
            .publications_added
            .iter()
            .map(|publication| publication.track_id.as_str()),
    )?;
    reject_duplicates(
        "publication removal",
        state.publications_removed.iter().map(String::as_str),
    )?;
    let participant_removals: BTreeSet<&str> = state
        .participants_removed
        .iter()
        .map(String::as_str)
        .collect();
    for participant in &state.participants_added {
        if participant_removals.contains(participant.participant_id.as_str()) {
            return Err(SignalingError::Duplicate {
                field: "participant add/remove",
                value: participant.participant_id.clone(),
            });
        }
    }
    let publication_removals: BTreeSet<&str> = state
        .publications_removed
        .iter()
        .map(String::as_str)
        .collect();
    for publication in &state.publications_added {
        if publication_removals.contains(publication.track_id.as_str()) {
            return Err(SignalingError::Duplicate {
                field: "publication add/remove",
                value: publication.track_id.clone(),
            });
        }
    }
    Ok(())
}

fn validate_video_bindings(
    bindings: Vec<signaling::VideoBinding>,
    publications: &BTreeMap<String, Publication>,
    mids: &BTreeMap<MediaSlot, String>,
) -> Result<BTreeMap<String, VideoBinding>, SignalingError> {
    let allowed = remote_mids(mids, MediaKind::Video);
    let mut result = BTreeMap::new();
    let mut tracks = BTreeSet::new();
    for binding in bindings {
        validate_wire_mid(&binding.mid)?;
        validate_wire_id("video binding track_id", &binding.track_id)?;
        if !allowed.contains(binding.mid.as_str()) {
            return Err(SignalingError::Unknown {
                field: "video mid",
                value: binding.mid,
            });
        }
        let Some(publication) = publications.get(&binding.track_id) else {
            return Err(SignalingError::Unknown {
                field: "video publication",
                value: binding.track_id,
            });
        };
        if publication.kind != MediaKind::Video {
            return Err(SignalingError::Invalid("video binding kind"));
        }
        if !tracks.insert(binding.track_id.clone()) {
            return Err(SignalingError::Duplicate {
                field: "video binding track",
                value: binding.track_id,
            });
        }
        let value = VideoBinding {
            track_id: binding.track_id,
            mid: binding.mid.clone(),
            paused: binding.paused,
        };
        if result.insert(binding.mid.clone(), value).is_some() {
            return Err(SignalingError::Duplicate {
                field: "video binding mid",
                value: binding.mid,
            });
        }
    }
    Ok(result)
}

fn validate_audio_bindings(
    bindings: Vec<signaling::AudioBinding>,
    publications: &BTreeMap<String, Publication>,
    mids: &BTreeMap<MediaSlot, String>,
) -> Result<Vec<AudioBinding>, SignalingError> {
    let allowed = remote_mids(mids, MediaKind::Audio);
    let mut result = Vec::with_capacity(bindings.len());
    let mut mids_seen = BTreeSet::new();
    let mut tracks = BTreeSet::new();
    for binding in bindings {
        validate_wire_mid(&binding.mid)?;
        validate_wire_id("audio binding track_id", &binding.track_id)?;
        if !allowed.contains(binding.mid.as_str()) {
            return Err(SignalingError::Unknown {
                field: "audio mid",
                value: binding.mid,
            });
        }
        let Some(publication) = publications.get(&binding.track_id) else {
            return Err(SignalingError::Unknown {
                field: "audio publication",
                value: binding.track_id,
            });
        };
        if publication.kind != MediaKind::Audio || !(-127..=0).contains(&binding.level_dbov) {
            return Err(SignalingError::Invalid("audio binding"));
        }
        if !mids_seen.insert(binding.mid.clone()) {
            return Err(SignalingError::Duplicate {
                field: "audio binding mid",
                value: binding.mid,
            });
        }
        if !tracks.insert(binding.track_id.clone()) {
            return Err(SignalingError::Duplicate {
                field: "audio binding track",
                value: binding.track_id,
            });
        }
        let level_dbov =
            i8::try_from(binding.level_dbov).map_err(|_| SignalingError::Invalid("audio level"))?;
        result.push(AudioBinding {
            track_id: binding.track_id,
            mid: binding.mid,
            level_dbov,
        });
    }
    Ok(result)
}

fn emit_participant_changes(
    old: &BTreeMap<String, Participant>,
    new: &BTreeMap<String, Participant>,
    notifications: &mut alloc::collections::VecDeque<Notification>,
) {
    for id in old.keys() {
        if !new.contains_key(id) {
            notifications.push_back(Notification::ParticipantRemoved(id.clone()));
        }
    }
    for (id, participant) in new {
        if old.get(id) != Some(participant) {
            notifications.push_back(Notification::ParticipantAdded(participant.clone()));
        }
    }
}

fn emit_publication_changes(
    old: &BTreeMap<String, Publication>,
    new: &BTreeMap<String, Publication>,
    notifications: &mut alloc::collections::VecDeque<Notification>,
) {
    for (id, publication) in old {
        if new.get(id) != Some(publication) {
            notifications.push_back(Notification::PublicationRemoved(id.clone()));
        }
    }
    for (id, publication) in new {
        if old.get(id) != Some(publication) {
            notifications.push_back(Notification::PublicationAdded(publication.clone()));
        }
    }
}

fn emit_video_changes(
    old: &BTreeMap<String, VideoBinding>,
    new: &BTreeMap<String, VideoBinding>,
    notifications: &mut alloc::collections::VecDeque<Notification>,
) {
    let mids: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for mid in mids {
        if old.get(mid) != new.get(mid) {
            notifications.push_back(Notification::VideoBindingChanged {
                mid: mid.clone(),
                binding: new.get(mid).cloned(),
            });
        }
    }
}

fn reject_duplicates<'a>(
    field: &'static str,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), SignalingError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(SignalingError::Duplicate {
                field,
                value: value.to_string(),
            });
        }
    }
    Ok(())
}

fn remote_mids(mids: &BTreeMap<MediaSlot, String>, kind: MediaKind) -> BTreeSet<&str> {
    mids.iter()
        .filter_map(|(slot, mid)| {
            let matches = matches!(
                (kind, slot),
                (MediaKind::Video, MediaSlot::RemoteVideo(_))
                    | (MediaKind::Audio, MediaSlot::RemoteAudio(_))
            );
            matches.then_some(mid.as_str())
        })
        .collect()
}

fn mid_for<'a>(
    mids: &'a BTreeMap<MediaSlot, String>,
    slot: &MediaSlot,
) -> Result<&'a str, SignalingError> {
    mids.get(slot)
        .map(String::as_str)
        .ok_or_else(|| SignalingError::MissingMid(slot.clone()))
}

fn validate_wire_id(field: &'static str, value: &str) -> Result<(), SignalingError> {
    validate_identifier(field, value, 256, true).map_err(|_| SignalingError::Invalid(field))
}

fn validate_wire_mid(value: &str) -> Result<(), SignalingError> {
    validate_identifier("mid", value, crate::MAX_MID_BYTES, false)
        .map_err(|_| SignalingError::Invalid("mid"))
}
