//! Complete peer snapshots. Origin and freshness come from the authenticated
//! session, never from the decoded document.

use std::{collections::BTreeSet, num::NonZeroU64};

use pulsebeam_proto::{prelude::Message, state::Track};
use serde::{Deserialize, Serialize};

use super::{
    ClaimRank,
    observations::{EndpointFact, Error, PublicationFact, Snapshot, validate_publications},
};
use crate::{
    control::controller::peer_ownership::{
        MAX_CLAIMS_PER_ORIGIN, MAX_PUBLICATIONS_PER_ORIGIN, transfer::MAX_IMAGE_BYTES,
    },
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectId, ProjectRoomKey, RoomId},
    media::Rid,
    track::{LayerQuality, SourceEncoding},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    revision: NonZeroU64,
    endpoints: Box<[EndpointImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointImage {
    project: ProjectId,
    room: RoomId,
    participant: ParticipantId,
    root: ConnectionId,
    position: u64,
    connection: ConnectionId,
    publications: Box<[PublicationImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationImage {
    descriptor: Box<[u8]>,
    layers: Box<[LayerImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerImage {
    encoding: EncodingImage,
    rid: Option<String>,
    quality: QualityImage,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EncodingImage {
    Rid(Option<String>),
    Ssrc(u32),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualityImage {
    Low,
    Medium,
    High,
}

impl From<&PublicationFact> for PublicationImage {
    fn from(fact: &PublicationFact) -> Self {
        Self {
            descriptor: fact.descriptor.encode_to_vec().into_boxed_slice(),
            layers: fact
                .layers
                .iter()
                .map(|&(encoding, rid, quality)| LayerImage {
                    encoding: match encoding {
                        SourceEncoding::Rid(value) => {
                            EncodingImage::Rid(value.map(|rid| rid.to_string()))
                        }
                        SourceEncoding::Ssrc(value) => EncodingImage::Ssrc(value.into()),
                    },
                    rid: rid.map(|rid| rid.to_string()),
                    quality: match quality {
                        LayerQuality::Low => QualityImage::Low,
                        LayerQuality::Medium => QualityImage::Medium,
                        LayerQuality::High => QualityImage::High,
                    },
                })
                .collect(),
        }
    }
}

impl TryFrom<PublicationImage> for PublicationFact {
    type Error = Error;

    fn try_from(image: PublicationImage) -> Result<Self, Error> {
        if image.layers.len() > 3 {
            return Err(Error::Invalid);
        }
        let mut encodings = Vec::with_capacity(image.layers.len());
        let mut layers = Vec::with_capacity(image.layers.len());
        for layer in image.layers {
            let rid = parse_rid(layer.rid.as_deref())?;
            let encoding = match layer.encoding {
                EncodingImage::Rid(value) if value.as_deref() == layer.rid.as_deref() => {
                    SourceEncoding::Rid(rid)
                }
                EncodingImage::Ssrc(value) if value != 0 => SourceEncoding::Ssrc(value.into()),
                _ => return Err(Error::Invalid),
            };
            if encodings.contains(&encoding) {
                return Err(Error::Invalid);
            }
            encodings.push(encoding);
            let quality = match layer.quality {
                QualityImage::Low => LayerQuality::Low,
                QualityImage::Medium => LayerQuality::Medium,
                QualityImage::High => LayerQuality::High,
            };
            layers.push((encoding, rid, quality));
        }
        Ok(Self {
            descriptor: Track::decode(image.descriptor.as_ref()).map_err(|_| Error::Invalid)?,
            layers: layers.into_boxed_slice(),
        })
    }
}

fn parse_rid(input: Option<&str>) -> Result<Option<Rid>, Error> {
    input.map(Rid::new).transpose().map_err(|_| Error::Invalid)
}

pub(crate) fn encode(revision: NonZeroU64, snapshot: &Snapshot) -> Result<Vec<u8>, Error> {
    validate(snapshot)?;
    let endpoints = snapshot
        .iter()
        .map(|(key, fact)| EndpointImage {
            project: key.room.project,
            room: key.room.room,
            participant: key.participant,
            root: fact.claim.root,
            position: fact.claim.position,
            connection: fact.claim.connection,
            publications: fact
                .publications
                .iter()
                .map(PublicationImage::from)
                .collect(),
        })
        .collect();
    let mut bytes = ImageWriter(Vec::new());
    serde_json::to_writer(
        &mut bytes,
        &Image {
            revision,
            endpoints,
        },
    )
    .map_err(|error| {
        if error.is_io() {
            Error::Capacity
        } else {
            Error::Invalid
        }
    })?;
    Ok(bytes.0)
}

struct ImageWriter(Vec<u8>);

impl std::io::Write for ImageWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("peer image capacity"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn decode(data: &[u8]) -> Result<(NonZeroU64, Snapshot), Error> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(Error::Capacity);
    }
    let image: Image = serde_json::from_slice(data).map_err(|_| Error::Invalid)?;
    if image.endpoints.len() > MAX_CLAIMS_PER_ORIGIN {
        return Err(Error::Capacity);
    }
    let snapshot = image
        .endpoints
        .into_vec()
        .into_iter()
        .map(|endpoint| {
            if endpoint.publications.len() > MAX_PUBLICATIONS_PER_ORIGIN {
                return Err(Error::Capacity);
            }
            if (endpoint.position == 0) != (endpoint.root == endpoint.connection) {
                return Err(Error::Invalid);
            }
            Ok((
                EndpointKey {
                    room: ProjectRoomKey {
                        project: endpoint.project,
                        room: endpoint.room,
                    },
                    participant: endpoint.participant,
                },
                EndpointFact {
                    claim: ClaimRank {
                        root: endpoint.root,
                        position: endpoint.position,
                        connection: endpoint.connection,
                    },
                    publications: endpoint
                        .publications
                        .into_vec()
                        .into_iter()
                        .map(PublicationFact::try_from)
                        .collect::<Result<_, _>>()?,
                },
            ))
        })
        .collect::<Result<Snapshot, Error>>()?;
    validate(&snapshot)?;
    Ok((image.revision, snapshot))
}

fn validate(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.len() > MAX_CLAIMS_PER_ORIGIN {
        return Err(Error::Capacity);
    }
    let mut endpoints = BTreeSet::new();
    for (key, _) in snapshot {
        if !endpoints.insert(*key) {
            return Err(Error::Invalid);
        }
    }
    validate_publications(
        snapshot.iter().map(|(key, fact)| (*key, fact)),
        MAX_PUBLICATIONS_PER_ORIGIN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_proto::state::{CodecFormat, VideoTrack, track};

    fn snapshot() -> Snapshot {
        let participant = ParticipantId::new("sender").unwrap();
        let room = RoomId::new("stage").unwrap();
        let media = Rid::new("f").unwrap();
        Box::new([(
            EndpointKey {
                room: ProjectRoomKey {
                    project: ProjectId::new(),
                    room,
                },
                participant,
            },
            EndpointFact {
                claim: ClaimRank::root(ConnectionId::new())
                    .successor(ConnectionId::new())
                    .unwrap(),
                publications: Box::new([PublicationFact {
                    descriptor: Track {
                        room_id: room.to_string(),
                        participant_id: participant.to_string(),
                        publication_id: crate::entity::PublicationId::mint().to_string(),
                        track_id: "camera".into(),
                        kind: Some(track::Kind::Video(VideoTrack {
                            format: Some(CodecFormat {
                                mime_type: "video/vp8".into(),
                                clock_rate: 90_000,
                                channels: None,
                                parameters: Vec::new(),
                            }),
                        })),
                    },
                    layers: Box::new([
                        (
                            SourceEncoding::Rid(Some(media)),
                            Some(media),
                            LayerQuality::High,
                        ),
                        (SourceEncoding::Ssrc(12.into()), None, LayerQuality::Low),
                    ]),
                }]),
            },
        )])
    }

    #[test]
    fn complete_snapshot_round_trips_with_exact_publication_and_encoding_identity() {
        let snapshot = snapshot();
        let bytes = encode(NonZeroU64::MIN, &snapshot).unwrap();
        assert_eq!(decode(&bytes).unwrap(), (NonZeroU64::MIN, snapshot));
        assert_eq!(
            decode(bytes.get(..bytes.len().saturating_sub(1)).unwrap()),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn invalid_scope_or_rank_cannot_enter_a_peer_snapshot() {
        let data = encode(NonZeroU64::MIN, &snapshot()).unwrap();
        let mut image: Image = serde_json::from_slice(&data).unwrap();
        image.endpoints.first_mut().unwrap().position = 0;
        assert_eq!(
            decode(&serde_json::to_vec(&image).unwrap()),
            Err(Error::Invalid)
        );
        let mut snapshot = snapshot();
        snapshot.first_mut().unwrap().0.participant = ParticipantId::new("wrong").unwrap();
        assert_eq!(encode(NonZeroU64::MIN, &snapshot), Err(Error::Invalid));
    }
}
