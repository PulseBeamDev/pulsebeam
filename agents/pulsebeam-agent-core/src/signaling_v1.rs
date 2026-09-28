use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    string::String,
    vec::Vec,
};

use pulsebeam_proto::signaling_v1::{self as wire, catalog};

use crate::{
    AudioBinding, MediaDirection, MediaKind, MediaSlot, Notification, Participant, Publication,
    SlotBinding, Snapshot, VideoBinding, signaling::SignalingError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogError {
    Revision,
    MissingState,
    EmptyDelta,
    Duplicate,
    Missing,
    InvalidIdentity,
    IdentityChanged,
    MappingReference,
    MappingShape,
    MappingRevision,
}

#[derive(Clone, Default)]
pub(crate) struct CatalogState {
    pub(crate) revision: u64,
    pub(crate) participants: BTreeMap<String, wire::Participant>,
    pub(crate) tracks: BTreeMap<String, wire::RemoteTrack>,
    pub(crate) video: BTreeMap<u32, String>,
    pub(crate) audio: BTreeMap<u32, String>,
    pub(crate) intent_revision: u64,
    known_participants: BTreeMap<String, String>,
    known_tracks: BTreeMap<String, wire::RemoteTrack>,
}

impl CatalogState {
    pub(crate) fn resolve(
        &self,
        selector: &crate::TrackSelector,
        kind: crate::MediaKind,
    ) -> Option<&str> {
        let wire_kind = match kind {
            crate::MediaKind::Video => wire::TrackKind::Video,
            crate::MediaKind::Audio => wire::TrackKind::Audio,
        };
        self.tracks.iter().find_map(|(id, track)| {
            (track.kind == wire_kind as i32
                && track.label == selector.label
                && self
                    .participants
                    .get(&track.participant_id)
                    .is_some_and(|participant| {
                        participant.participant_external_id == selector.participant_external_id
                    }))
            .then_some(id.as_str())
        })
    }

    pub(crate) fn apply(
        &mut self,
        incoming: wire::Catalog,
        recipient_id: &str,
        recipient_external_id: &str,
    ) -> Result<(), CatalogError> {
        if incoming.revision == 0 {
            return Err(CatalogError::Revision);
        }
        let mut next = Self {
            revision: incoming.revision,
            participants: self.participants.clone(),
            tracks: self.tracks.clone(),
            video: self.video.clone(),
            audio: self.audio.clone(),
            intent_revision: self.intent_revision,
            known_participants: self.known_participants.clone(),
            known_tracks: self.known_tracks.clone(),
        };
        match incoming.state.ok_or(CatalogError::MissingState)? {
            catalog::State::Snapshot(snapshot) => {
                if incoming.revision <= self.revision {
                    return Err(CatalogError::Revision);
                }
                next.participants.clear();
                next.tracks.clear();
                for participant in snapshot.participants {
                    let id = participant.participant_id.clone();
                    if next.participants.insert(id, participant).is_some() {
                        return Err(CatalogError::Duplicate);
                    }
                }
                for track in snapshot.tracks {
                    let id = track.track_id.clone();
                    if next.tracks.insert(id, track).is_some() {
                        return Err(CatalogError::Duplicate);
                    }
                }
            }
            catalog::State::Delta(delta) => {
                if self.revision == 0 || self.revision.checked_add(1) != Some(incoming.revision) {
                    return Err(CatalogError::Revision);
                }
                if delta.added_participants.is_empty()
                    && delta.removed_participant_ids.is_empty()
                    && delta.added_tracks.is_empty()
                    && delta.removed_track_ids.is_empty()
                {
                    return Err(CatalogError::EmptyDelta);
                }
                let mut removed_participants = BTreeSet::new();
                for id in delta.removed_participant_ids {
                    if !removed_participants.insert(id.clone()) {
                        return Err(CatalogError::Duplicate);
                    }
                    if !self.participants.contains_key(&id) {
                        return Err(CatalogError::Missing);
                    }
                }
                let mut removed_tracks = BTreeSet::new();
                for id in delta.removed_track_ids {
                    if !removed_tracks.insert(id.clone()) {
                        return Err(CatalogError::Duplicate);
                    }
                    let Some(track) = self.tracks.get(&id) else {
                        return Err(CatalogError::Missing);
                    };
                    if removed_participants.contains(&track.participant_id) {
                        return Err(CatalogError::Duplicate);
                    }
                }
                for id in removed_tracks {
                    next.tracks.remove(&id);
                }
                for id in removed_participants {
                    next.participants.remove(&id);
                    next.tracks.retain(|_, track| track.participant_id != id);
                }
                for participant in delta.added_participants {
                    let id = participant.participant_id.clone();
                    if self.participants.contains_key(&id)
                        || next.participants.insert(id, participant).is_some()
                    {
                        return Err(CatalogError::Duplicate);
                    }
                }
                for track in delta.added_tracks {
                    let id = track.track_id.clone();
                    if self.tracks.contains_key(&id) || next.tracks.insert(id, track).is_some() {
                        return Err(CatalogError::Duplicate);
                    }
                }
            }
        }
        next.validate(recipient_id, recipient_external_id)?;
        if self
            .video
            .values()
            .chain(self.audio.values())
            .any(|id| !next.tracks.contains_key(id))
        {
            return Err(CatalogError::MappingReference);
        }
        *self = next;
        Ok(())
    }

    pub(crate) fn apply_mapping(
        &mut self,
        mapping: wire::Mapping,
        coordinates: &BTreeMap<MediaSlot, SlotBinding>,
    ) -> Result<(), CatalogError> {
        if self.revision == 0 || mapping.intent_revision < self.intent_revision {
            return Err(CatalogError::MappingRevision);
        }
        let (Some(video), Some(audio)) = (mapping.video, mapping.audio) else {
            return Err(CatalogError::MappingShape);
        };
        let receivers: BTreeMap<u32, MediaKind> = coordinates
            .values()
            .filter(|binding| binding.direction == MediaDirection::ReceiveOnly)
            .map(|binding| (binding.media_index, binding.kind))
            .collect();
        let mut used_tracks = BTreeSet::new();
        let mut used_receivers = BTreeSet::new();
        let mut parse = |entries: alloc::vec::Vec<wire::TrackMapping>, kind| {
            let mut result = BTreeMap::new();
            for entry in entries {
                if !used_receivers.insert(entry.receiver_index)
                    || !used_tracks.insert(entry.track_id.clone())
                    || receivers.get(&entry.receiver_index) != Some(&kind)
                    || self.tracks.get(&entry.track_id).map(|track| track.kind)
                        != Some(match kind {
                            MediaKind::Video => wire::TrackKind::Video as i32,
                            MediaKind::Audio => wire::TrackKind::Audio as i32,
                        })
                {
                    return Err(CatalogError::MappingShape);
                }
                result.insert(entry.receiver_index, entry.track_id);
            }
            Ok(result)
        };
        let video = parse(video.tracks, MediaKind::Video)?;
        let audio = parse(audio.tracks, MediaKind::Audio)?;
        self.video = video;
        self.audio = audio;
        self.intent_revision = mapping.intent_revision;
        Ok(())
    }

    fn validate(
        &mut self,
        recipient_id: &str,
        recipient_external_id: &str,
    ) -> Result<(), CatalogError> {
        let mut external_ids = BTreeSet::new();
        external_ids.insert(recipient_external_id);
        for (id, participant) in &self.participants {
            if !valid(id, 128)
                || !valid(&participant.participant_external_id, 256)
                || id == recipient_id
            {
                return Err(CatalogError::InvalidIdentity);
            }
            if !external_ids.insert(&participant.participant_external_id) {
                return Err(CatalogError::Duplicate);
            }
            if self
                .known_participants
                .get(id)
                .is_some_and(|old| old != &participant.participant_external_id)
            {
                return Err(CatalogError::IdentityChanged);
            }
            self.known_participants
                .insert(id.clone(), participant.participant_external_id.clone());
        }
        let mut selectors = BTreeSet::new();
        for (id, track) in &self.tracks {
            if !valid(id, 128)
                || !valid(&track.label, 64)
                || !self.participants.contains_key(&track.participant_id)
                || track.participant_id == recipient_id
                || !matches!(
                    wire::TrackKind::try_from(track.kind),
                    Ok(wire::TrackKind::Audio | wire::TrackKind::Video)
                )
            {
                return Err(CatalogError::InvalidIdentity);
            }
            if !selectors.insert((&track.participant_id, track.kind, &track.label)) {
                return Err(CatalogError::Duplicate);
            }
            if self.known_tracks.get(id).is_some_and(|old| old != track) {
                return Err(CatalogError::IdentityChanged);
            }
            self.known_tracks.insert(id.clone(), track.clone());
        }
        Ok(())
    }
}

pub(crate) enum ServerOutput {
    StateChanged,
    Authorization(i64),
    ServerError(wire::Error),
    Reconnect,
}

pub(crate) fn decode_and_apply(
    payload: &[u8],
    state: &mut CatalogState,
    snapshot: &mut Snapshot,
    notifications: &mut VecDeque<Notification>,
    coordinates: &BTreeMap<MediaSlot, SlotBinding>,
    recipient_id: &str,
    recipient_external_id: &str,
) -> Result<ServerOutput, SignalingError> {
    let message =
        pulsebeam_proto::codec::decode_server(payload).map_err(|_| SignalingError::Malformed)?;
    match message.payload.ok_or(SignalingError::MissingPayload)? {
        wire::server_message::Payload::Catalog(catalog) => {
            state
                .apply(catalog, recipient_id, recipient_external_id)
                .map_err(|_| SignalingError::Invalid("catalog"))?;
            update_snapshot(state, snapshot, notifications, coordinates)?;
            Ok(ServerOutput::StateChanged)
        }
        wire::server_message::Payload::Mapping(mapping) => {
            state
                .apply_mapping(mapping, coordinates)
                .map_err(|_| SignalingError::Invalid("mapping"))?;
            update_snapshot(state, snapshot, notifications, coordinates)?;
            Ok(ServerOutput::StateChanged)
        }
        wire::server_message::Payload::Authorization(authorization) => Ok(
            ServerOutput::Authorization(authorization.expires_at_unix_seconds),
        ),
        wire::server_message::Payload::Error(error) => {
            let valid = match wire::ErrorCode::try_from(error.code) {
                Ok(
                    wire::ErrorCode::InvalidMessage
                    | wire::ErrorCode::AuthorizationExpired
                    | wire::ErrorCode::ProtocolError,
                ) => error.fatal,
                Ok(wire::ErrorCode::AuthorizationRejected) => !error.fatal,
                Ok(wire::ErrorCode::Internal) => true,
                _ => false,
            };
            if !valid {
                return Err(SignalingError::Invalid("error classification"));
            }
            Ok(ServerOutput::ServerError(error))
        }
        wire::server_message::Payload::Reconnect(_) => Ok(ServerOutput::Reconnect),
    }
}

fn update_snapshot(
    state: &CatalogState,
    snapshot: &mut Snapshot,
    notifications: &mut VecDeque<Notification>,
    coordinates: &BTreeMap<MediaSlot, SlotBinding>,
) -> Result<(), SignalingError> {
    let participants: BTreeMap<_, _> = state
        .participants
        .iter()
        .map(|(id, participant)| {
            (
                id.clone(),
                Participant {
                    id: id.clone(),
                    external_id: participant.participant_external_id.clone(),
                },
            )
        })
        .collect();
    let publications: BTreeMap<_, _> = state
        .tracks
        .iter()
        .map(|(id, track)| {
            (
                id.clone(),
                Publication {
                    id: id.clone(),
                    participant_id: track.participant_id.clone(),
                    kind: if track.kind == wire::TrackKind::Audio as i32 {
                        MediaKind::Audio
                    } else {
                        MediaKind::Video
                    },
                    label: track.label.clone(),
                },
            )
        })
        .collect();
    let mids: BTreeMap<_, _> = coordinates
        .values()
        .map(|binding| (binding.media_index, binding.mid.as_str()))
        .collect();
    let video: BTreeMap<_, _> = state
        .video
        .iter()
        .map(|(index, id)| {
            let mid = String::from(
                *mids
                    .get(index)
                    .ok_or(SignalingError::Invalid("mapping index"))?,
            );
            Ok((
                mid.clone(),
                VideoBinding {
                    track_id: id.clone(),
                    mid,
                    paused: false,
                },
            ))
        })
        .collect::<Result<_, SignalingError>>()?;
    let audio: Vec<_> = state
        .audio
        .iter()
        .map(|(index, id)| {
            Ok(AudioBinding {
                track_id: id.clone(),
                mid: String::from(
                    *mids
                        .get(index)
                        .ok_or(SignalingError::Invalid("mapping index"))?,
                ),
                level_dbov: 0,
            })
        })
        .collect::<Result<_, SignalingError>>()?;
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

fn emit_participant_changes(
    old: &BTreeMap<String, Participant>,
    new: &BTreeMap<String, Participant>,
    notifications: &mut VecDeque<Notification>,
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
    notifications: &mut VecDeque<Notification>,
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
    notifications: &mut VecDeque<Notification>,
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

fn valid(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn participant() -> wire::Participant {
        wire::Participant {
            participant_id: "peer".into(),
            participant_external_id: "alice".into(),
        }
    }
    fn track() -> wire::RemoteTrack {
        wire::RemoteTrack {
            track_id: "opaque".into(),
            participant_id: "peer".into(),
            kind: wire::TrackKind::Audio.into(),
            label: "mic".into(),
        }
    }
    fn snapshot(
        revision: u64,
        participants: alloc::vec::Vec<wire::Participant>,
        tracks: alloc::vec::Vec<wire::RemoteTrack>,
    ) -> wire::Catalog {
        wire::Catalog {
            revision,
            state: Some(catalog::State::Snapshot(wire::CatalogSnapshot {
                participants,
                tracks,
            })),
        }
    }
    fn delta(revision: u64, value: wire::CatalogDelta) -> wire::Catalog {
        wire::Catalog {
            revision,
            state: Some(catalog::State::Delta(value)),
        }
    }
    fn apply(
        state: &mut CatalogState,
        message: wire::Catalog,
        mapped: &[&str],
    ) -> Result<(), CatalogError> {
        for id in mapped {
            state.audio.insert(0, String::from(*id));
        }
        state.apply(message, "self", "me")
    }

    #[test]
    fn catalog_requires_first_snapshot_and_atomic_consecutive_deltas() {
        let mut state = CatalogState::default();
        assert_eq!(
            apply(&mut state, delta(1, wire::CatalogDelta::default()), &[]),
            Err(CatalogError::Revision)
        );
        apply(
            &mut state,
            snapshot(1, vec![participant()], vec![track()]),
            &[],
        )
        .unwrap();
        assert_eq!(
            apply(
                &mut state,
                delta(
                    3,
                    wire::CatalogDelta {
                        removed_track_ids: vec!["opaque".into()],
                        ..Default::default()
                    }
                ),
                &[]
            ),
            Err(CatalogError::Revision)
        );
        assert_eq!(
            apply(
                &mut state,
                delta(
                    2,
                    wire::CatalogDelta {
                        removed_track_ids: vec!["opaque".into()],
                        ..Default::default()
                    }
                ),
                &["opaque"]
            ),
            Err(CatalogError::MappingReference)
        );
        assert_eq!(state.revision, 1);
        assert!(state.tracks.contains_key("opaque"));
        state.audio.clear();
        apply(
            &mut state,
            delta(
                2,
                wire::CatalogDelta {
                    removed_participant_ids: vec!["peer".into()],
                    ..Default::default()
                },
            ),
            &[],
        )
        .unwrap();
        assert!(state.tracks.is_empty());
        assert_eq!(
            apply(&mut state, delta(3, wire::CatalogDelta::default()), &[]),
            Err(CatalogError::EmptyDelta)
        );
        assert_eq!(state.revision, 2);
    }

    #[test]
    fn catalog_rejects_duplicate_selectors_and_immutable_id_reuse() {
        let mut state = CatalogState::default();
        apply(
            &mut state,
            snapshot(1, vec![participant()], vec![track()]),
            &[],
        )
        .unwrap();
        let mut impostor = track();
        impostor.track_id = "different".into();
        assert_eq!(
            apply(
                &mut state,
                snapshot(2, vec![participant()], vec![track(), impostor]),
                &[]
            ),
            Err(CatalogError::Duplicate)
        );
        let mut altered = track();
        altered.label = "other".into();
        assert_eq!(
            apply(
                &mut state,
                snapshot(2, vec![participant()], vec![altered]),
                &[]
            ),
            Err(CatalogError::IdentityChanged)
        );
        assert_eq!(state.revision, 1);
    }

    #[test]
    fn mapping_is_complete_kind_checked_and_causally_fenced() {
        let mut state = CatalogState::default();
        apply(
            &mut state,
            snapshot(1, vec![participant()], vec![track()]),
            &[],
        )
        .unwrap();
        let slot = MediaSlot::RemoteAudio(0);
        let mut coordinates = BTreeMap::new();
        coordinates.insert(
            slot.clone(),
            SlotBinding {
                slot,
                mid: "remote-audio".into(),
                media_index: 3,
                kind: MediaKind::Audio,
                direction: MediaDirection::ReceiveOnly,
            },
        );
        let valid = wire::Mapping {
            intent_revision: 2,
            video: Some(wire::TrackMappings::default()),
            audio: Some(wire::TrackMappings {
                tracks: vec![wire::TrackMapping {
                    receiver_index: 3,
                    track_id: "opaque".into(),
                }],
            }),
        };
        assert_eq!(
            state.apply_mapping(
                wire::Mapping {
                    video: None,
                    ..valid.clone()
                },
                &coordinates
            ),
            Err(CatalogError::MappingShape)
        );
        state.apply_mapping(valid.clone(), &coordinates).unwrap();
        assert_eq!(state.audio.get(&3).map(String::as_str), Some("opaque"));
        assert_eq!(
            state.apply_mapping(
                wire::Mapping {
                    intent_revision: 1,
                    ..valid.clone()
                },
                &coordinates
            ),
            Err(CatalogError::MappingRevision)
        );
        let invalid = wire::Mapping {
            video: Some(wire::TrackMappings {
                tracks: vec![wire::TrackMapping {
                    receiver_index: 3,
                    track_id: "opaque".into(),
                }],
            }),
            ..valid
        };
        assert_eq!(
            state.apply_mapping(invalid, &coordinates),
            Err(CatalogError::MappingShape)
        );
        assert_eq!(state.intent_revision, 2);
        assert_eq!(
            apply(
                &mut state,
                delta(
                    2,
                    wire::CatalogDelta {
                        removed_track_ids: vec!["opaque".into()],
                        ..Default::default()
                    }
                ),
                &[]
            ),
            Err(CatalogError::MappingReference)
        );
        state
            .apply_mapping(
                wire::Mapping {
                    intent_revision: 2,
                    video: Some(wire::TrackMappings::default()),
                    audio: Some(wire::TrackMappings::default()),
                },
                &coordinates,
            )
            .unwrap();
        apply(
            &mut state,
            delta(
                2,
                wire::CatalogDelta {
                    removed_track_ids: vec!["opaque".into()],
                    ..Default::default()
                },
            ),
            &[],
        )
        .unwrap();
        assert!(state.tracks.is_empty());
    }

    #[test]
    fn encoded_catalog_and_mapping_update_observation_without_inventing_reconnect() {
        let mut state = CatalogState::default();
        let mut snapshot = Snapshot::default();
        let mut notifications = VecDeque::new();
        let slot = MediaSlot::RemoteAudio(0);
        let coordinates = BTreeMap::from([(
            slot.clone(),
            SlotBinding {
                slot,
                mid: "non-numeric".into(),
                media_index: 2,
                kind: MediaKind::Audio,
                direction: MediaDirection::ReceiveOnly,
            },
        )]);
        let encoded = pulsebeam_proto::codec::encode_server(&wire::ServerMessage {
            payload: Some(wire::server_message::Payload::Catalog(snapshot_message())),
        })
        .unwrap();
        assert!(matches!(
            decode_and_apply(
                &encoded,
                &mut state,
                &mut snapshot,
                &mut notifications,
                &coordinates,
                "self",
                "me"
            ),
            Ok(ServerOutput::StateChanged)
        ));
        assert!(snapshot.publications.contains_key("opaque"));
        let encoded = pulsebeam_proto::codec::encode_server(&wire::ServerMessage {
            payload: Some(wire::server_message::Payload::Mapping(wire::Mapping {
                intent_revision: 1,
                video: Some(wire::TrackMappings::default()),
                audio: Some(wire::TrackMappings {
                    tracks: vec![wire::TrackMapping {
                        receiver_index: 2,
                        track_id: "opaque".into(),
                    }],
                }),
            })),
        })
        .unwrap();
        assert!(matches!(
            decode_and_apply(
                &encoded,
                &mut state,
                &mut snapshot,
                &mut notifications,
                &coordinates,
                "self",
                "me"
            ),
            Ok(ServerOutput::StateChanged)
        ));
        assert_eq!(snapshot.audio[0].mid, "non-numeric");
        assert_eq!(state.intent_revision, 1);
    }

    fn snapshot_message() -> wire::Catalog {
        snapshot(1, vec![participant()], vec![track()])
    }
}
