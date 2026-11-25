use std::{num::NonZeroU64, time::SystemTime};

use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::{Candidates, Error, Grant, Issuer, Owner, Source};
use crate::{
    cluster::handoff::Scope,
    control::controller::{
        ControllerActor,
        ownership::{ClaimRank, observations::Session},
        peer_ownership::EncoderAdvertisement,
    },
    entity::{
        ConnectionId, EndpointKey, ParticipantId, ProjectRoomKey, PublicationKey, TrackId,
        TrackKind,
    },
};

const TAG: &[u8] = &[1, 0x0e];
const LIMIT: usize = pulsebeam_link::CONTROL_FRAME_BYTES;

#[derive(Serialize, Deserialize)]
struct OwnerImage(Scope, Issuer, ConnectionId, u64, ConnectionId, u64, u64);

impl From<Owner> for OwnerImage {
    fn from(owner: Owner) -> Self {
        let endpoint = owner.endpoint;
        Self(
            Scope {
                project: endpoint.room.project,
                room: endpoint.room.room,
                participant: endpoint.participant,
            },
            owner.issuer,
            owner.rank.lineage(),
            owner.rank.position(),
            owner.rank.connection(),
            owner.authorization,
            owner.expires_at,
        )
    }
}

impl TryFrom<OwnerImage> for Owner {
    type Error = Error;

    fn try_from(image: OwnerImage) -> Result<Self, Error> {
        let OwnerImage(scope, issuer, lineage, position, connection, authorization, expires_at) =
            image;
        Ok(Self {
            endpoint: EndpointKey {
                room: ProjectRoomKey {
                    project: scope.project,
                    room: scope.room,
                },
                participant: scope.participant,
            },
            issuer,
            rank: ClaimRank::from_persisted(lineage, position, connection).ok_or(Error::Invalid)?,
            authorization,
            expires_at,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct SourceImage(NodeId, NonZeroU64, ConnectionId, u8, String, TrackId);

impl From<Source> for SourceImage {
    fn from(source: Source) -> Self {
        Self(
            source.node,
            source.generation,
            source.connection,
            source.publication.kind() as u8,
            source.publication.as_str(),
            source.track,
        )
    }
}

impl TryFrom<SourceImage> for Source {
    type Error = Error;

    fn try_from(image: SourceImage) -> Result<Self, Error> {
        let SourceImage(node, generation, connection, kind, publication, track) = image;
        let kind = match kind {
            kind if kind == TrackKind::Audio as u8 => TrackKind::Audio,
            kind if kind == TrackKind::Video as u8 => TrackKind::Video,
            _ => return Err(Error::Invalid),
        };
        let publication =
            PublicationKey::new(kind, publication.parse().map_err(|_| Error::Invalid)?);
        Ok(Self {
            node,
            generation,
            connection,
            publication,
            track,
        })
    }
}

#[derive(Serialize, Deserialize)]
pub(in crate::control::controller) struct DelegationImage(OwnerImage, SourceImage, u64, Vec<u8>);

impl DelegationImage {
    pub(in crate::control::controller) fn new(owner: Owner, source: Source, grant: Grant) -> Self {
        Self(
            owner.into(),
            source.into(),
            grant.expires_at,
            grant.signature.to_vec(),
        )
    }

    pub(in crate::control::controller) fn into_parts(
        self,
    ) -> Result<(Owner, Source, Grant), Error> {
        let Self(owner, source, expires_at, signature) = self;
        Ok((
            owner.try_into()?,
            source.try_into()?,
            Grant {
                expires_at,
                signature: signature.try_into().map_err(|_| Error::Invalid)?,
            },
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Payload {
    Encoder(OwnerImage, SourceImage, u64, ParticipantId),
    Primary(OwnerImage, SourceImage, NonZeroU64),
    Standby(OwnerImage, SourceImage, NonZeroU64, SourceImage, u64),
}

pub(in crate::control::controller) enum Frame {
    Encoder {
        owner: Owner,
        source: Source,
        grant: Grant,
        publisher: ParticipantId,
    },
    Select {
        owner: Owner,
        primary: Source,
        revision: NonZeroU64,
        delegated: Option<(Source, Grant)>,
    },
}

impl Frame {
    pub(in crate::control::controller) fn encode(&self) -> Result<Box<[u8]>, Error> {
        let mut bytes = TAG.to_vec();
        let (payload, grant) = match self {
            Self::Encoder {
                owner,
                source,
                grant,
                publisher,
            } => (
                Payload::Encoder(
                    (*owner).into(),
                    (*source).into(),
                    grant.expires_at,
                    *publisher,
                ),
                Some(grant),
            ),
            Self::Select {
                owner,
                primary,
                revision,
                delegated: None,
            } => (
                Payload::Primary((*owner).into(), (*primary).into(), *revision),
                None,
            ),
            Self::Select {
                owner,
                primary,
                revision,
                delegated: Some((source, grant)),
            } => (
                Payload::Standby(
                    (*owner).into(),
                    (*primary).into(),
                    *revision,
                    (*source).into(),
                    grant.expires_at,
                ),
                Some(grant),
            ),
        };
        bytes.push(u8::from(grant.is_some()));
        if let Some(grant) = grant {
            bytes.extend_from_slice(&grant.signature);
        }
        serde_json::to_writer(&mut bytes, &payload).map_err(|_| Error::Invalid)?;
        if bytes.len() > LIMIT {
            return Err(Error::Invalid);
        }
        Ok(bytes.into_boxed_slice())
    }

    pub(in crate::control::controller) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > LIMIT {
            return None;
        }
        let (&flags, bytes) = bytes.strip_prefix(TAG)?.split_first()?;
        let (signature, bytes) = match flags {
            0 => (None, bytes),
            1 => {
                let (signature, bytes) = bytes.split_first_chunk::<64>()?;
                (Some(*signature), bytes)
            }
            _ => return None,
        };
        let grant = |expires_at| {
            Some(Grant {
                expires_at,
                signature: signature?,
            })
        };
        Some(match serde_json::from_slice(bytes).ok()? {
            Payload::Encoder(owner, source, expires_at, publisher) => Self::Encoder {
                owner: owner.try_into().ok()?,
                source: source.try_into().ok()?,
                grant: grant(expires_at)?,
                publisher,
            },
            Payload::Primary(owner, primary, revision) if signature.is_none() => Self::Select {
                owner: owner.try_into().ok()?,
                primary: primary.try_into().ok()?,
                revision,
                delegated: None,
            },
            Payload::Standby(owner, primary, revision, source, expires_at) => Self::Select {
                owner: owner.try_into().ok()?,
                primary: primary.try_into().ok()?,
                revision,
                delegated: Some((source.try_into().ok()?, grant(expires_at)?)),
            },
            Payload::Primary(..) => return None,
        })
    }
}

impl ControllerActor {
    pub(in crate::control::controller) fn receive_standby_control(
        &mut self,
        session: Session,
        frame: Frame,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<(), Error> {
        match frame {
            Frame::Encoder {
                owner,
                source,
                grant,
                publisher,
            } => {
                let authority = self
                    .peer_ownership
                    .active_session(owner.issuer.node, owner.issuer.incarnation)
                    .ok_or(Error::NoAuthority)?;
                self.observe_standby_encoder(
                    authority,
                    owner,
                    EncoderAdvertisement {
                        session,
                        source,
                        publisher,
                        grant,
                    },
                    now,
                    wall_now,
                )
            }
            Frame::Select {
                owner,
                primary,
                revision,
                delegated,
            } => {
                let trust = self.standby_authority(session, owner, now)?;
                let unix_now = wall_now
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| Error::Invalid)?
                    .as_secs();
                let mut candidates = Candidates::new(owner, primary)?;
                let selected = if let Some((source, grant)) = delegated {
                    candidates.admit(owner, trust, source, grant, unix_now)?;
                    source
                } else {
                    primary
                };
                candidates.select(owner, trust, revision, selected, unix_now)?;
                self.reconcile_selected_peer_source(session, owner, &candidates, now, wall_now)
            }
        }
    }
}
