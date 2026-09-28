use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_proto::signaling_v1::{LocalTrack, SendIntent, TrackKind as WireTrackKind};

use crate::entity::TrackKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendBindingError {
    RelabeledSender,
    MovedLabel,
}

#[derive(Default)]
pub(crate) struct SenderBindings {
    by_index: HashMap<u32, (TrackKind, String)>,
    by_label: HashMap<(TrackKind, String), u32>,
}

pub(crate) struct SendPlan {
    pub(crate) tracks: Vec<LocalTrack>,
    bindings: SenderBindings,
}

impl SenderBindings {
    /// Reconcile the entire send section without mutating established bindings.
    /// Only a successfully accepted complete Intent may commit the returned plan.
    pub(crate) fn plan(
        &self,
        intent: Option<&SendIntent>,
        negotiated: &HashMap<u32, TrackKind>,
    ) -> Result<SendPlan, SendBindingError> {
        let mut bindings = Self {
            by_index: self.by_index.clone(),
            by_label: self.by_label.clone(),
        };
        let mut tracks = Vec::new();
        let mut used_indices = HashSet::new();
        let mut used_labels = HashSet::new();
        for track in intent.into_iter().flat_map(|send| &send.tracks) {
            let kind = match WireTrackKind::try_from(track.kind) {
                Ok(WireTrackKind::Audio) => TrackKind::Audio,
                Ok(WireTrackKind::Video) => TrackKind::Video,
                _ => continue,
            };
            if track.label.is_empty()
                || track.label.len() > 64
                || negotiated.get(&track.sender_index) != Some(&kind)
            {
                continue;
            }
            let key = (kind, track.label.clone());
            if used_indices.contains(&track.sender_index) || used_labels.contains(&key) {
                continue;
            }
            if bindings
                .by_index
                .get(&track.sender_index)
                .is_some_and(|previous| previous != &key)
            {
                return Err(SendBindingError::RelabeledSender);
            }
            if bindings
                .by_label
                .get(&key)
                .is_some_and(|previous| previous != &track.sender_index)
            {
                return Err(SendBindingError::MovedLabel);
            }
            used_indices.insert(track.sender_index);
            used_labels.insert(key.clone());
            bindings.by_index.insert(track.sender_index, key.clone());
            bindings.by_label.insert(key, track.sender_index);
            tracks.push(track.clone());
        }
        Ok(SendPlan { tracks, bindings })
    }

    pub(crate) fn commit(&mut self, plan: SendPlan) -> Vec<LocalTrack> {
        *self = plan.bindings;
        plan.tracks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogError {
    MissingIdentity,
    DuplicateIdentity,
    MissingPublisher,
    ChangedIdentity,
    OversizedSnapshot,
    RevisionExhausted,
}

#[derive(Default)]
pub(crate) struct CatalogState {
    revision: u64,
    participants: HashMap<String, String>,
    tracks: HashMap<String, pulsebeam_proto::signaling_v1::RemoteTrack>,
    known_participants: HashMap<String, String>,
    known_tracks: HashMap<String, pulsebeam_proto::signaling_v1::RemoteTrack>,
}

pub(crate) struct CatalogPlan {
    pub(crate) message: pulsebeam_proto::signaling_v1::ServerMessage,
    next: CatalogState,
}

impl CatalogState {
    pub(crate) fn plan(
        &self,
        peers: impl IntoIterator<Item = super::effect::RoomPeer>,
        tracks: impl IntoIterator<Item = crate::track::TrackMeta>,
        recipient: crate::entity::ParticipantId,
        recipient_external_id: &str,
    ) -> Result<Option<CatalogPlan>, CatalogError> {
        use pulsebeam_proto::signaling_v1::{
            Catalog, CatalogDelta, CatalogSnapshot, Participant, RemoteTrack, ServerMessage,
            TrackKind as WireKind, catalog, server_message,
        };

        if recipient_external_id.is_empty() || recipient_external_id.len() > 256 {
            return Err(CatalogError::MissingIdentity);
        }
        let mut participants = HashMap::new();
        let mut external_ids = HashSet::new();
        external_ids.insert(recipient_external_id.to_owned());
        for peer in peers {
            if peer.id == recipient {
                continue;
            }
            let external = peer.external_id.ok_or(CatalogError::MissingIdentity)?;
            let id = peer.id.as_str();
            let external = external.as_str().to_owned();
            if id.is_empty() || id.len() > 128 || external.is_empty() || external.len() > 256 {
                return Err(CatalogError::MissingIdentity);
            }
            if participants.insert(id, external.clone()).is_some() || !external_ids.insert(external)
            {
                return Err(CatalogError::DuplicateIdentity);
            }
        }
        let mut visible_tracks = HashMap::new();
        let mut labels = HashSet::new();
        for meta in tracks {
            if meta.origin == recipient || meta.id.kind() == TrackKind::Data {
                continue;
            }
            let publisher = meta.origin.as_str();
            if !participants.contains_key(&publisher) {
                return Err(CatalogError::MissingPublisher);
            }
            let label = meta.label.ok_or(CatalogError::MissingIdentity)?;
            let kind = match meta.id.kind() {
                TrackKind::Audio => WireKind::Audio,
                TrackKind::Video => WireKind::Video,
                TrackKind::Data => continue,
            };
            if label.is_empty() || label.len() > 64 {
                return Err(CatalogError::MissingIdentity);
            }
            if !labels.insert((publisher.clone(), kind as i32, label.clone())) {
                return Err(CatalogError::DuplicateIdentity);
            }
            let id = meta.id.as_str();
            let remote = RemoteTrack {
                track_id: id.clone(),
                participant_id: publisher,
                kind: kind as i32,
                label,
            };
            if visible_tracks.insert(id, remote).is_some() {
                return Err(CatalogError::DuplicateIdentity);
            }
        }
        for (id, external) in &participants {
            if self
                .known_participants
                .get(id)
                .is_some_and(|old| old != external)
            {
                return Err(CatalogError::ChangedIdentity);
            }
        }
        for (id, track) in &visible_tracks {
            if self.known_tracks.get(id).is_some_and(|old| old != track) {
                return Err(CatalogError::ChangedIdentity);
            }
        }
        if self.revision != 0 && self.participants == participants && self.tracks == visible_tracks
        {
            return Ok(None);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(CatalogError::RevisionExhausted)?;
        let mut snapshot = CatalogSnapshot {
            participants: participants
                .iter()
                .map(|(id, external)| Participant {
                    participant_id: id.clone(),
                    participant_external_id: external.clone(),
                })
                .collect(),
            tracks: visible_tracks.values().cloned().collect(),
        };
        snapshot
            .participants
            .sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
        snapshot.tracks.sort_by(|a, b| a.track_id.cmp(&b.track_id));
        let wrap = |state| ServerMessage {
            payload: Some(server_message::Payload::Catalog(Catalog {
                revision,
                state: Some(state),
            })),
        };
        let full = wrap(catalog::State::Snapshot(snapshot));
        pulsebeam_proto::codec::encode_server(&full)
            .map_err(|_| CatalogError::OversizedSnapshot)?;
        let message = if self.revision == 0 {
            full
        } else {
            let mut delta = CatalogDelta {
                added_participants: participants
                    .iter()
                    .filter(|(id, _)| !self.participants.contains_key(*id))
                    .map(|(id, external)| Participant {
                        participant_id: id.clone(),
                        participant_external_id: external.clone(),
                    })
                    .collect(),
                removed_participant_ids: self
                    .participants
                    .keys()
                    .filter(|id| !participants.contains_key(*id))
                    .cloned()
                    .collect(),
                added_tracks: visible_tracks
                    .iter()
                    .filter(|(id, _)| !self.tracks.contains_key(*id))
                    .map(|(_, track)| track.clone())
                    .collect(),
                removed_track_ids: self
                    .tracks
                    .iter()
                    .filter(|(id, track)| {
                        !visible_tracks.contains_key(*id)
                            && participants.contains_key(&track.participant_id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
            };
            delta
                .added_participants
                .sort_by(|a, b| a.participant_id.cmp(&b.participant_id));
            delta.removed_participant_ids.sort();
            delta
                .added_tracks
                .sort_by(|a, b| a.track_id.cmp(&b.track_id));
            delta.removed_track_ids.sort();
            let change = wrap(catalog::State::Delta(delta));
            if pulsebeam_proto::codec::encode_server(&change).is_ok() {
                change
            } else {
                full
            }
        };
        Ok(Some(CatalogPlan {
            message,
            next: Self {
                revision,
                known_participants: self
                    .known_participants
                    .iter()
                    .chain(&participants)
                    .map(|(id, external)| (id.clone(), external.clone()))
                    .collect(),
                known_tracks: self
                    .known_tracks
                    .iter()
                    .chain(&visible_tracks)
                    .map(|(id, track)| (id.clone(), track.clone()))
                    .collect(),
                participants,
                tracks: visible_tracks,
            },
        }))
    }

    pub(crate) fn commit(&mut self, plan: CatalogPlan) {
        *self = plan.next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media() -> HashMap<u32, TrackKind> {
        HashMap::from_iter([
            (0, TrackKind::Audio),
            (2, TrackKind::Video),
            (3, TrackKind::Audio),
        ])
    }

    fn track(sender_index: u32, kind: WireTrackKind, label: &str) -> LocalTrack {
        LocalTrack {
            sender_index,
            kind: kind.into(),
            label: label.to_owned(),
        }
    }

    fn peer(id: crate::entity::ParticipantId, external: &str) -> super::super::effect::RoomPeer {
        super::super::effect::RoomPeer {
            id,
            external_id: Some(crate::entity::ParticipantExternalId::new(external).unwrap()),
        }
    }

    fn remote_audio(
        publisher: crate::entity::ParticipantId,
        label: &str,
    ) -> crate::track::TrackMeta {
        let (_, mut descriptor) = crate::track::test_utils::make_audio_track(
            publisher,
            str0m::media::Mid::from("remote-mid"),
        );
        let meta = descriptor.meta_mut();
        meta.id = publisher.derive_track_id(TrackKind::Audio, label);
        meta.label = Some(label.to_owned());
        meta.clone()
    }

    #[test]
    fn catalog_snapshot_and_delta_remove_publisher_tracks_implicitly() {
        use pulsebeam_proto::signaling_v1::{catalog, server_message};
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let mut state = CatalogState::default();
        let plan = state
            .plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "mic")],
                recipient,
                "self",
            )
            .unwrap()
            .unwrap();
        let Some(server_message::Payload::Catalog(first)) = &plan.message.payload else {
            panic!("initial catalog message missing");
        };
        assert_eq!(first.revision, 1);
        assert!(matches!(first.state, Some(catalog::State::Snapshot(_))));
        state.commit(plan);
        let plan = state.plan([], [], recipient, "self").unwrap().unwrap();
        let Some(server_message::Payload::Catalog(next)) = &plan.message.payload else {
            panic!("catalog delta missing");
        };
        assert_eq!(next.revision, 2);
        let Some(catalog::State::Delta(delta)) = &next.state else {
            panic!("expected delta");
        };
        assert_eq!(delta.removed_participant_ids, vec![publisher.as_str()]);
        assert!(delta.removed_track_ids.is_empty());
        state.commit(plan);
        assert!(state.plan([], [], recipient, "self").unwrap().is_none());
    }

    #[test]
    fn catalog_refuses_identity_reuse_after_removal() {
        let recipient = crate::entity::ParticipantId::new();
        let publisher = crate::entity::ParticipantId::new();
        let mut state = CatalogState::default();
        let first = state
            .plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "mic")],
                recipient,
                "self",
            )
            .unwrap()
            .unwrap();
        state.commit(first);
        let removed = state.plan([], [], recipient, "self").unwrap().unwrap();
        state.commit(removed);
        assert!(matches!(
            state.plan([peer(publisher, "bob")], [], recipient, "self"),
            Err(CatalogError::ChangedIdentity)
        ));
        assert!(matches!(
            state.plan(
                [peer(publisher, "alice")],
                [remote_audio(publisher, "different")],
                recipient,
                "self",
            ),
            Ok(Some(_))
        ));
        let mut changed_track = remote_audio(publisher, "mic");
        changed_track.label = Some("renamed".to_owned());
        assert!(matches!(
            state.plan(
                [peer(publisher, "alice")],
                [changed_track],
                recipient,
                "self"
            ),
            Err(CatalogError::ChangedIdentity)
        ));
    }

    #[test]
    fn catalog_rejects_duplicate_external_identity_and_oversized_snapshots() {
        let recipient = crate::entity::ParticipantId::new();
        let a = crate::entity::ParticipantId::new();
        let b = crate::entity::ParticipantId::new();
        let state = CatalogState::default();
        assert!(matches!(
            state.plan([peer(a, "same"), peer(b, "same")], [], recipient, "self"),
            Err(CatalogError::DuplicateIdentity)
        ));
        let peers = (0..600)
            .map(|n| {
                peer(
                    crate::entity::ParticipantId::new(),
                    &format!("peer-{n:03}-abcdefghijklmnopqrst"),
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            state.plan(peers, [], recipient, "self"),
            Err(CatalogError::OversizedSnapshot)
        ));
    }

    #[test]
    fn first_usable_entries_win_and_omission_preserves_binding() {
        let mut state = SenderBindings::default();
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Video, "wrong-kind"),
                track(0, WireTrackKind::Audio, "mic"),
                track(0, WireTrackKind::Audio, "later"),
                track(3, WireTrackKind::Audio, "mic"),
                track(2, WireTrackKind::Video, "camera"),
            ],
        };
        let plan = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(state.commit(plan).len(), 2);
        assert!(state.commit(state.plan(None, &media()).unwrap()).is_empty());
        let restored = state.plan(Some(&intent), &media()).unwrap();
        assert_eq!(restored.tracks.len(), 2);
    }

    #[test]
    fn established_relabel_or_rebind_is_fatal_without_partial_commit() {
        let mut state = SenderBindings::default();
        let first = SendIntent {
            tracks: vec![track(0, WireTrackKind::Audio, "mic")],
        };
        let plan = state.plan(Some(&first), &media()).unwrap();
        state.commit(plan);
        let relabel = SendIntent {
            tracks: vec![
                track(2, WireTrackKind::Video, "camera"),
                track(0, WireTrackKind::Audio, "other"),
            ],
        };
        assert!(matches!(
            state.plan(Some(&relabel), &media()),
            Err(SendBindingError::RelabeledSender)
        ));
        let moved = SendIntent {
            tracks: vec![track(3, WireTrackKind::Audio, "mic")],
        };
        assert!(matches!(
            state.plan(Some(&moved), &media()),
            Err(SendBindingError::MovedLabel)
        ));
        assert_eq!(state.plan(Some(&first), &media()).unwrap().tracks.len(), 1);
    }

    #[test]
    fn invalid_labels_and_unknown_indices_are_skipped() {
        let intent = SendIntent {
            tracks: vec![
                track(0, WireTrackKind::Audio, ""),
                track(0, WireTrackKind::Audio, &"é".repeat(33)),
                track(9, WireTrackKind::Audio, "missing"),
                track(0, WireTrackKind::Audio, "valid"),
            ],
        };
        assert_eq!(
            SenderBindings::default()
                .plan(Some(&intent), &media())
                .unwrap()
                .tracks
                .len(),
            1
        );
    }
}
