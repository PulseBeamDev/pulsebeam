use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
};

use pulsebeam_proto::signaling_v1;

use crate::{MediaKind, ParticipantId, TrackId, validate_identifier};

const MAX_OPAQUE_ID_BYTES: usize = 256;
const MAX_EXTERNAL_ID_BYTES: usize = 256;
const MAX_MEDIA_LABEL_BYTES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogParticipant {
    pub id: ParticipantId,
    pub external_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTrack {
    pub id: TrackId,
    pub participant_id: ParticipantId,
    pub kind: MediaKind,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackSelector {
    pub participant_external_id: String,
    pub kind: MediaKind,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    participants: BTreeMap<String, CatalogParticipant>,
    tracks: BTreeMap<String, CatalogTrack>,
    selectors: BTreeMap<TrackSelector, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog contains an invalid {0}")]
    Invalid(&'static str),
    #[error("catalog contains a duplicate {0}: {1}")]
    Duplicate(&'static str, String),
    #[error("catalog track references an unknown participant: {0}")]
    UnknownParticipant(String),
    #[error("catalog contains a track owned by its recipient: {0}")]
    SelfTrack(String),
    #[error("catalog contains an ambiguous application selector")]
    AmbiguousSelector,
}

impl Catalog {
    pub fn from_server(
        value: signaling_v1::Catalog,
        recipient: &ParticipantId,
    ) -> Result<Self, CatalogError> {
        let mut participants = BTreeMap::new();
        let mut external_ids = BTreeSet::new();
        for participant in value.participants {
            validate_opaque("participant ID", &participant.participant_id)?;
            validate_identifier(
                "participant external ID",
                &participant.participant_external_id,
                MAX_EXTERNAL_ID_BYTES,
                false,
            )
            .map_err(|_| CatalogError::Invalid("participant external ID"))?;
            if !external_ids.insert(participant.participant_external_id.clone()) {
                return Err(CatalogError::Duplicate(
                    "participant external ID",
                    participant.participant_external_id,
                ));
            }
            let key = participant.participant_id.clone();
            if participants
                .insert(
                    key.clone(),
                    CatalogParticipant {
                        id: ParticipantId::from_server(participant.participant_id),
                        external_id: participant.participant_external_id,
                    },
                )
                .is_some()
            {
                return Err(CatalogError::Duplicate("participant ID", key));
            }
        }

        let mut tracks = BTreeMap::new();
        let mut selectors = BTreeMap::new();
        for track in value.tracks {
            validate_opaque("track ID", &track.track_id)?;
            validate_opaque("track participant ID", &track.participant_id)?;
            validate_identifier("track label", &track.label, MAX_MEDIA_LABEL_BYTES, false)
                .map_err(|_| CatalogError::Invalid("track label"))?;
            if track.participant_id == recipient.as_str() {
                return Err(CatalogError::SelfTrack(track.track_id));
            }
            let Some(participant) = participants.get(&track.participant_id) else {
                return Err(CatalogError::UnknownParticipant(track.participant_id));
            };
            let kind = match signaling_v1::TrackKind::try_from(track.kind) {
                Ok(signaling_v1::TrackKind::Audio) => MediaKind::Audio,
                Ok(signaling_v1::TrackKind::Video) => MediaKind::Video,
                Ok(signaling_v1::TrackKind::Unspecified) | Err(_) => {
                    return Err(CatalogError::Invalid("track kind"));
                }
            };
            let selector = TrackSelector {
                participant_external_id: participant.external_id.clone(),
                kind,
                label: track.label.clone(),
            };
            if selectors.insert(selector, track.track_id.clone()).is_some() {
                return Err(CatalogError::AmbiguousSelector);
            }
            let key = track.track_id.clone();
            if tracks
                .insert(
                    key.clone(),
                    CatalogTrack {
                        id: TrackId::from_server(track.track_id),
                        participant_id: participant.id.clone(),
                        kind,
                        label: track.label,
                    },
                )
                .is_some()
            {
                return Err(CatalogError::Duplicate("track ID", key));
            }
        }

        Ok(Self {
            participants,
            tracks,
            selectors,
        })
    }

    pub fn participants(&self) -> impl Iterator<Item = &CatalogParticipant> {
        self.participants.values()
    }

    pub fn tracks(&self) -> impl Iterator<Item = &CatalogTrack> {
        self.tracks.values()
    }

    pub fn resolve(&self, selector: &TrackSelector) -> Option<&TrackId> {
        let track_id = self.selectors.get(selector)?;
        self.tracks.get(track_id).map(|track| &track.id)
    }
}

fn validate_opaque(field: &'static str, value: &str) -> Result<(), CatalogError> {
    validate_identifier(field, value, MAX_OPAQUE_ID_BYTES, true)
        .map_err(|_| CatalogError::Invalid(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(id: &str, external_id: &str) -> signaling_v1::Participant {
        signaling_v1::Participant {
            participant_id: id.to_string(),
            participant_external_id: external_id.to_string(),
        }
    }

    fn track(
        id: &str,
        participant_id: &str,
        kind: signaling_v1::TrackKind,
        label: &str,
    ) -> signaling_v1::RemoteTrack {
        signaling_v1::RemoteTrack {
            track_id: id.to_string(),
            participant_id: participant_id.to_string(),
            kind: kind.into(),
            label: label.to_string(),
        }
    }

    fn recipient() -> ParticipantId {
        ParticipantId::from_server("self".to_string())
    }

    #[test]
    fn resolves_opaque_ids_by_external_identity_kind_and_label() {
        let catalog = Catalog::from_server(
            signaling_v1::Catalog {
                participants: vec![participant("opaque/alice", "alice")],
                tracks: vec![
                    track(
                        "opaque/audio",
                        "opaque/alice",
                        signaling_v1::TrackKind::Audio,
                        "main",
                    ),
                    track(
                        "opaque/video",
                        "opaque/alice",
                        signaling_v1::TrackKind::Video,
                        "main",
                    ),
                ],
            },
            &recipient(),
        )
        .unwrap();

        for (kind, expected) in [
            (MediaKind::Audio, "opaque/audio"),
            (MediaKind::Video, "opaque/video"),
        ] {
            let resolved = catalog
                .resolve(&TrackSelector {
                    participant_external_id: "alice".to_string(),
                    kind,
                    label: "main".to_string(),
                })
                .unwrap();
            assert_eq!(resolved.as_str(), expected);
        }
        assert!(
            catalog
                .resolve(&TrackSelector {
                    participant_external_id: "alice".to_string(),
                    kind: MediaKind::Video,
                    label: "missing".to_string(),
                })
                .is_none()
        );
    }

    #[test]
    fn comparison_ignores_wire_order() {
        let first = signaling_v1::Catalog {
            participants: vec![participant("p1", "one"), participant("p2", "two")],
            tracks: vec![
                track("t1", "p1", signaling_v1::TrackKind::Audio, "mic"),
                track("t2", "p2", signaling_v1::TrackKind::Video, "camera"),
            ],
        };
        let mut reversed = first.clone();
        reversed.participants.reverse();
        reversed.tracks.reverse();

        assert_eq!(
            Catalog::from_server(first, &recipient()).unwrap(),
            Catalog::from_server(reversed, &recipient()).unwrap()
        );
    }

    #[test]
    fn malformed_or_ambiguous_catalogs_reject_atomically() {
        let ambiguous = signaling_v1::Catalog {
            participants: vec![participant("p1", "alice")],
            tracks: vec![
                track("t1", "p1", signaling_v1::TrackKind::Video, "camera"),
                track("t2", "p1", signaling_v1::TrackKind::Video, "camera"),
            ],
        };
        assert_eq!(
            Catalog::from_server(ambiguous, &recipient()),
            Err(CatalogError::AmbiguousSelector)
        );

        let unknown = signaling_v1::Catalog {
            participants: Vec::new(),
            tracks: vec![track(
                "t1",
                "missing",
                signaling_v1::TrackKind::Video,
                "camera",
            )],
        };
        assert!(matches!(
            Catalog::from_server(unknown, &recipient()),
            Err(CatalogError::UnknownParticipant(_))
        ));
    }

    #[test]
    fn self_tracks_and_duplicate_external_identities_reject() {
        let self_track = signaling_v1::Catalog {
            participants: vec![participant("self", "me")],
            tracks: vec![track("mine", "self", signaling_v1::TrackKind::Audio, "mic")],
        };
        assert!(matches!(
            Catalog::from_server(self_track, &recipient()),
            Err(CatalogError::SelfTrack(_))
        ));

        let duplicate_external = signaling_v1::Catalog {
            participants: vec![participant("p1", "alice"), participant("p2", "alice")],
            tracks: Vec::new(),
        };
        assert!(matches!(
            Catalog::from_server(duplicate_external, &recipient()),
            Err(CatalogError::Duplicate("participant external ID", _))
        ));
    }
}
