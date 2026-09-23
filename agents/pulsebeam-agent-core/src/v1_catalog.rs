use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
};

use pulsebeam_proto::signaling_v1 as wire;

use crate::{
    DesiredState, MediaKind, MediaSlot, PlayoutDelay, SlotBinding, signaling::SignalingError,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CatalogState {
    pub revision: u64,
    pub participants: BTreeMap<String, String>,
    pub tracks: BTreeMap<String, wire::RemoteTrack>,
    seen_participants: BTreeMap<String, String>,
    seen_tracks: BTreeMap<String, wire::RemoteTrack>,
}

impl CatalogState {
    pub fn apply(&mut self, catalog: wire::Catalog) -> Result<bool, SignalingError> {
        let Some(state) = catalog.state else {
            return Err(SignalingError::Invalid("catalog state"));
        };
        if catalog.revision == 0 {
            return Err(SignalingError::Invalid("catalog revision"));
        }
        match state {
            wire::catalog::State::Snapshot(snapshot) => {
                if self.revision != 0 && catalog.revision <= self.revision {
                    return Ok(false);
                }
                if self.revision == 0 && catalog.revision != 1 {
                    return Err(SignalingError::Invalid("initial catalog revision"));
                }
                let mut next = Self {
                    revision: catalog.revision,
                    seen_participants: self.seen_participants.clone(),
                    seen_tracks: self.seen_tracks.clone(),
                    ..Self::default()
                };
                for participant in snapshot.participants {
                    next.add_participant(participant)?;
                }
                for track in snapshot.tracks {
                    next.add_track(track)?;
                }
                *self = next;
                Ok(true)
            }
            wire::catalog::State::Delta(delta) => {
                if self.revision == 0 {
                    return Err(SignalingError::Invalid("catalog delta before snapshot"));
                }
                if catalog.revision <= self.revision {
                    return Ok(false);
                }
                if catalog.revision != self.revision.saturating_add(1) {
                    return Err(SignalingError::Invalid("catalog revision gap"));
                }
                if delta.added_participants.is_empty()
                    && delta.removed_participant_ids.is_empty()
                    && delta.added_tracks.is_empty()
                    && delta.removed_track_ids.is_empty()
                {
                    return Err(SignalingError::Invalid("empty catalog delta"));
                }
                let mut next = self.clone();
                let added_track_ids: BTreeSet<_> = delta
                    .added_tracks
                    .iter()
                    .map(|track| track.track_id.as_str())
                    .collect();
                let added_participant_ids: BTreeSet<_> = delta
                    .added_participants
                    .iter()
                    .map(|participant| participant.participant_id.as_str())
                    .collect();
                let removed_participant_ids: BTreeSet<_> = delta
                    .removed_participant_ids
                    .iter()
                    .map(String::as_str)
                    .collect();
                if delta.removed_track_ids.iter().any(|id| {
                    added_track_ids.contains(id.as_str())
                        || self.tracks.get(id).is_some_and(|track| {
                            removed_participant_ids.contains(track.participant_id.as_str())
                        })
                }) || delta
                    .removed_participant_ids
                    .iter()
                    .any(|id| added_participant_ids.contains(id.as_str()))
                {
                    return Err(SignalingError::Invalid("catalog add/remove collision"));
                }
                let mut removals = BTreeSet::new();
                for id in delta.removed_track_ids {
                    if !removals.insert(id.clone()) || next.tracks.remove(&id).is_none() {
                        return Err(SignalingError::Invalid("catalog track removal"));
                    }
                }
                removals.clear();
                for id in delta.removed_participant_ids {
                    if !removals.insert(id.clone()) || next.participants.remove(&id).is_none() {
                        return Err(SignalingError::Invalid("catalog participant removal"));
                    }
                    next.tracks.retain(|_, track| track.participant_id != id);
                }
                for participant in delta.added_participants {
                    next.add_participant(participant)?;
                }
                for track in delta.added_tracks {
                    next.add_track(track)?;
                }
                next.revision = catalog.revision;
                *self = next;
                Ok(true)
            }
        }
    }

    fn add_participant(&mut self, participant: wire::Participant) -> Result<(), SignalingError> {
        check_id(&participant.participant_id, 128, "participant id")?;
        check_id(
            &participant.participant_external_id,
            256,
            "participant external id",
        )?;
        let id = participant.participant_id;
        let external = participant.participant_external_id;
        if self
            .participants
            .values()
            .any(|previous| previous == &external)
            || self.participants.contains_key(&id)
        {
            return Err(SignalingError::Invalid("duplicate catalog participant"));
        }
        if self
            .seen_participants
            .get(&id)
            .is_some_and(|previous| previous != &external)
        {
            return Err(SignalingError::Invalid("participant identity changed"));
        }
        self.seen_participants.insert(id.clone(), external.clone());
        self.participants.insert(id, external);
        Ok(())
    }

    fn add_track(&mut self, track: wire::RemoteTrack) -> Result<(), SignalingError> {
        check_id(&track.track_id, 128, "track id")?;
        check_id(&track.participant_id, 128, "track participant id")?;
        check_id(&track.label, 64, "track label")?;
        if !self.participants.contains_key(&track.participant_id) {
            return Err(SignalingError::Invalid("unknown track participant"));
        }
        if !matches!(
            wire::TrackKind::try_from(track.kind),
            Ok(wire::TrackKind::Video | wire::TrackKind::Audio)
        ) {
            return Err(SignalingError::Invalid("track kind"));
        }
        if self.tracks.values().any(|other| {
            other.participant_id == track.participant_id
                && other.kind == track.kind
                && other.label == track.label
        }) {
            return Err(SignalingError::Invalid("duplicate catalog track label"));
        }
        if self.tracks.contains_key(&track.track_id) {
            return Err(SignalingError::Invalid("duplicate catalog track id"));
        }
        if self
            .seen_tracks
            .get(&track.track_id)
            .is_some_and(|previous| previous != &track)
        {
            return Err(SignalingError::Invalid("track identity changed"));
        }
        self.seen_tracks
            .insert(track.track_id.clone(), track.clone());
        self.tracks.insert(track.track_id.clone(), track);
        Ok(())
    }
}

pub(crate) fn encode_intent(
    desired: &DesiredState,
    coordinates: &BTreeMap<MediaSlot, SlotBinding>,
    catalog: &CatalogState,
    revision: u64,
) -> Result<alloc::vec::Vec<u8>, SignalingError> {
    if revision == 0 {
        return Err(SignalingError::Invalid("intent revision"));
    }
    let mut send = alloc::vec::Vec::new();
    for publication in desired
        .publications
        .iter()
        .filter(|publication| publication.active)
    {
        let Some(slot) = coordinates.iter().find(|(slot, binding)| {
            matches!(slot, MediaSlot::LocalVideo(label) | MediaSlot::LocalAudio(label) if label == &publication.slot)
                && binding.direction == crate::MediaDirection::SendOnly
                && binding.kind == slot.kind()
        }) else {
            // A reservation can outlive the compatible sender sections of this connection.
            continue;
        };
        let kind = match slot.0.kind() {
            MediaKind::Video => wire::TrackKind::Video,
            MediaKind::Audio => wire::TrackKind::Audio,
        };
        send.push(wire::LocalTrack {
            sender_index: slot.1.media_index,
            kind: kind.into(),
            label: publication.slot.clone(),
        });
    }
    let playout = match desired.playout_delay {
        PlayoutDelay::Adaptive => None,
        PlayoutDelay::Fixed { min_ms, max_ms } => Some(wire::PlayoutDelay { min_ms, max_ms }),
    };
    let mut video = alloc::vec::Vec::new();
    for subscription in &desired.video {
        if catalog
            .tracks
            .get(&subscription.track_id)
            .is_some_and(|track| track.kind == wire::TrackKind::Video as i32)
        {
            video.push(wire::VideoTrackIntent {
                track_id: subscription.track_id.clone(),
                options: Some(wire::VideoOptions {
                    height: subscription.height,
                    min_height: subscription.min_height,
                    min_fps: subscription.min_fps,
                    priority: subscription.priority,
                    playout_delay: playout.clone(),
                }),
            });
        }
    }
    let audio = desired
        .audio
        .pinned
        .iter()
        .filter(|id| {
            catalog
                .tracks
                .get(*id)
                .is_some_and(|track| track.kind == wire::TrackKind::Audio as i32)
        })
        .map(|id| wire::AudioTrackIntent {
            track_id: id.clone(),
            options: Some(wire::AudioOptions {
                playout_delay: playout.clone(),
            }),
        })
        .collect();
    let message = wire::ClientMessage {
        payload: Some(wire::client_message::Payload::Intent(wire::Intent {
            revision,
            send: Some(wire::SendIntent { tracks: send }),
            receive: Some(wire::ReceiveIntent {
                video: Some(wire::VideoIntent { tracks: video }),
                audio: Some(wire::AudioIntent {
                    tracks: audio,
                    mode: if desired.audio.automatic {
                        wire::AudioMode::Auto.into()
                    } else {
                        wire::AudioMode::ExplicitOnly.into()
                    },
                }),
            }),
        })),
    };
    pulsebeam_proto::codec::encode_client(&message)
        .map_err(|_| SignalingError::Invalid("intent size"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MappingState {
    pub intent_revision: u64,
    pub receivers: BTreeMap<u32, String>,
}

impl MappingState {
    pub fn apply(
        &mut self,
        mapping: wire::Mapping,
        catalog: &CatalogState,
        receivers: &BTreeMap<u32, wire::TrackKind>,
        sent_revision: u64,
    ) -> Result<(), SignalingError> {
        if catalog.revision == 0 {
            return Err(SignalingError::Invalid("mapping before catalog"));
        }
        if mapping.intent_revision < self.intent_revision || mapping.intent_revision > sent_revision
        {
            return Err(SignalingError::Invalid("mapping intent revision"));
        }
        let video = mapping
            .video
            .ok_or(SignalingError::Invalid("missing video mappings"))?;
        let audio = mapping
            .audio
            .ok_or(SignalingError::Invalid("missing audio mappings"))?;
        let mut next = BTreeMap::new();
        let mut tracks = BTreeSet::new();
        for (group, kind) in [
            (video, wire::TrackKind::Video),
            (audio, wire::TrackKind::Audio),
        ] {
            for entry in group.tracks {
                if receivers.get(&entry.receiver_index) != Some(&kind) {
                    return Err(SignalingError::Invalid("mapping receiver kind"));
                }
                if catalog.tracks.get(&entry.track_id).map(|track| track.kind) != Some(kind as i32)
                {
                    return Err(SignalingError::Invalid("mapping track kind"));
                }
                if !tracks.insert(entry.track_id.clone())
                    || next.insert(entry.receiver_index, entry.track_id).is_some()
                {
                    return Err(SignalingError::Invalid("duplicate mapping"));
                }
            }
        }
        self.intent_revision = mapping.intent_revision;
        self.receivers = next;
        Ok(())
    }
}

fn check_id(value: &str, maximum: usize, field: &'static str) -> Result<(), SignalingError> {
    if value.is_empty() || value.len() > maximum {
        return Err(SignalingError::Invalid(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn participant() -> wire::Participant {
        wire::Participant {
            participant_id: "canonical".into(),
            participant_external_id: "external".into(),
        }
    }
    fn track() -> wire::RemoteTrack {
        wire::RemoteTrack {
            track_id: "track".into(),
            participant_id: "canonical".into(),
            kind: wire::TrackKind::Video.into(),
            label: "camera".into(),
        }
    }
    fn snapshot(revision: u64) -> wire::Catalog {
        wire::Catalog {
            revision,
            state: Some(wire::catalog::State::Snapshot(wire::CatalogSnapshot {
                participants: vec![participant()],
                tracks: vec![track()],
            })),
        }
    }
    #[test]
    fn intent_uses_negotiated_indices_and_independent_raw_lz4() {
        let mut catalog = CatalogState::default();
        catalog.apply(snapshot(1)).unwrap();
        let mut desired = DesiredState::default();
        desired.publications.push(crate::PublicationIntent {
            slot: "camera".into(),
            active: true,
        });
        desired.video.push(crate::VideoSubscription {
            slot: 0,
            track_id: "track".into(),
            height: 360,
            min_height: 720,
            min_fps: 24,
            priority: 1,
        });
        desired.playout_delay = PlayoutDelay::Fixed {
            min_ms: 15,
            max_ms: 50,
        };
        let sender = MediaSlot::LocalVideo("camera".into());
        let coordinates = BTreeMap::from([(
            sender.clone(),
            SlotBinding {
                slot: sender,
                mid: "non-numeric-mid".into(),
                media_index: 3,
                kind: MediaKind::Video,
                direction: crate::MediaDirection::SendOnly,
            },
        )]);
        let bytes = encode_intent(&desired, &coordinates, &catalog, 1).unwrap();
        let decoded = pulsebeam_proto::codec::decode_client(&bytes).unwrap();
        let Some(wire::client_message::Payload::Intent(intent)) = decoded.payload else {
            panic!("expected Intent");
        };
        assert_eq!(intent.revision, 1);
        assert_eq!(intent.send.unwrap().tracks[0].sender_index, 3);
        let video = intent.receive.unwrap().video.unwrap().tracks;
        assert_eq!(video[0].track_id, "track");
        assert_eq!(video[0].options.as_ref().unwrap().min_height, 720);
        assert_eq!(
            video[0]
                .options
                .as_ref()
                .unwrap()
                .playout_delay
                .as_ref()
                .unwrap()
                .min_ms,
            15
        );
    }

    #[test]
    fn unavailable_sender_keeps_desire_without_inventing_a_coordinate() {
        let mut desired = DesiredState::default();
        desired.publications.push(crate::PublicationIntent {
            slot: "camera".into(),
            active: true,
        });
        let sender = MediaSlot::LocalVideo("camera".into());
        let incompatible = SlotBinding {
            slot: sender.clone(),
            mid: "recv".into(),
            media_index: 4,
            kind: MediaKind::Video,
            direction: crate::MediaDirection::ReceiveOnly,
        };
        for coordinates in [
            BTreeMap::new(),
            BTreeMap::from([(sender.clone(), incompatible)]),
        ] {
            let bytes = encode_intent(&desired, &coordinates, &CatalogState::default(), 1)
                .expect("missing sender is feasible");
            let message = pulsebeam_proto::codec::decode_client(&bytes).unwrap();
            let Some(wire::client_message::Payload::Intent(intent)) = message.payload else {
                panic!("expected Intent");
            };
            assert!(intent.send.unwrap().tracks.is_empty());
            assert_eq!(desired.publications[0].slot, "camera");
        }
    }

    #[test]
    fn catalog_snapshots_and_deltas_are_atomic_and_revisioned() {
        let mut state = CatalogState::default();
        assert!(state.apply(snapshot(1)).unwrap());
        let initial = state.clone();
        assert!(state.apply(snapshot(0)).is_err());
        assert_eq!(state, initial);
        assert!(!state.apply(snapshot(1)).unwrap());
        assert_eq!(state, initial);
        let removal = wire::Catalog {
            revision: 2,
            state: Some(wire::catalog::State::Delta(wire::CatalogDelta {
                removed_track_ids: vec!["track".into()],
                ..Default::default()
            })),
        };
        assert!(state.apply(removal).unwrap());
        assert!(state.tracks.is_empty());
        assert!(!state.apply(snapshot(1)).unwrap());
        assert!(state.apply(snapshot(3)).unwrap());
        assert_eq!(state.tracks.len(), 1);
        let encoded = pulsebeam_proto::codec::encode_server(&wire::ServerMessage {
            payload: Some(wire::server_message::Payload::Catalog(snapshot(4))),
        })
        .unwrap();
        let decoded = pulsebeam_proto::codec::decode_server(&encoded).unwrap();
        let Some(wire::server_message::Payload::Catalog(catalog)) = decoded.payload else {
            panic!("expected catalog");
        };
        assert!(state.apply(catalog).unwrap());
    }

    #[test]
    fn removed_canonical_identity_cannot_reappear_as_another_track() {
        let mut state = CatalogState::default();
        state.apply(snapshot(1)).unwrap();
        state
            .apply(wire::Catalog {
                revision: 2,
                state: Some(wire::catalog::State::Delta(wire::CatalogDelta {
                    removed_track_ids: vec!["track".into()],
                    ..Default::default()
                })),
            })
            .unwrap();
        let previous = state.clone();
        let mut changed = track();
        changed.label = "screen".into();
        assert!(
            state
                .apply(wire::Catalog {
                    revision: 3,
                    state: Some(wire::catalog::State::Delta(wire::CatalogDelta {
                        added_tracks: vec![changed],
                        ..Default::default()
                    })),
                })
                .is_err()
        );
        assert_eq!(state, previous);
    }

    #[test]
    fn mapping_replaces_both_kinds_and_rejects_cross_kind_indices_atomically() {
        let mut catalog = CatalogState::default();
        catalog.apply(snapshot(1)).unwrap();
        let mut audio = track();
        audio.track_id = "audio".into();
        audio.kind = wire::TrackKind::Audio.into();
        audio.label = "mic".into();
        catalog.tracks.insert(audio.track_id.clone(), audio);
        let receivers = BTreeMap::from([(1, wire::TrackKind::Video), (2, wire::TrackKind::Audio)]);
        let mut mapping = MappingState::default();
        let complete = wire::Mapping {
            intent_revision: 1,
            video: Some(wire::TrackMappings {
                tracks: vec![wire::TrackMapping {
                    receiver_index: 1,
                    track_id: "track".into(),
                }],
            }),
            audio: Some(wire::TrackMappings {
                tracks: vec![wire::TrackMapping {
                    receiver_index: 2,
                    track_id: "audio".into(),
                }],
            }),
        };
        mapping
            .apply(complete.clone(), &catalog, &receivers, 1)
            .unwrap();
        let previous = mapping.clone();
        let mut invalid = complete;
        invalid.audio.as_mut().unwrap().tracks[0].receiver_index = 1;
        assert!(mapping.apply(invalid, &catalog, &receivers, 1).is_err());
        assert_eq!(mapping, previous);
        let clear = wire::Mapping {
            intent_revision: 1,
            video: Some(wire::TrackMappings { tracks: vec![] }),
            audio: Some(wire::TrackMappings { tracks: vec![] }),
        };
        mapping.apply(clear, &catalog, &receivers, 1).unwrap();
        assert!(mapping.receivers.is_empty());
    }

    #[test]
    fn invalid_delta_never_mutates_catalog() {
        let mut state = CatalogState::default();
        state.apply(snapshot(1)).unwrap();
        let initial = state.clone();
        let invalid = wire::Catalog {
            revision: 2,
            state: Some(wire::catalog::State::Delta(wire::CatalogDelta {
                removed_track_ids: vec!["missing".into()],
                removed_participant_ids: vec!["canonical".into()],
                ..Default::default()
            })),
        };
        assert!(state.apply(invalid).is_err());
        assert_eq!(state, initial);
    }
}
