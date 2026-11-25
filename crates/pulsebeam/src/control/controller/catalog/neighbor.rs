//! Single-hop point reads on the authenticated regional control stream.

use super::{
    ControllerActor, ORDERING_VERSION, ParticipantId, ProjectId, RoomId, limits, owner, wire,
};
use crate::cluster::{REGIONAL_FRAME_BYTES, declarations::Exchange};
use prost::{Message, Oneof};
use pulsebeam_core::auth::grants::RoomDiscover;
use pulsebeam_runtime::time::Instant;
use std::num::NonZeroU64;

pub(in crate::control::controller) mod client;

const TAG: &[u8] = &[1, 0x11];

#[derive(Clone, PartialEq, Message)]
struct RequestWire {
    #[prost(uint32, tag = "1")]
    ordering: u32,
    #[prost(string, tag = "2")]
    project: String,
    #[prost(bytes, tag = "3")]
    discover: Vec<u8>,
    #[prost(uint64, tag = "4")]
    interest: u64,
    #[prost(string, optional, tag = "5")]
    after: Option<String>,
    #[prost(message, optional, tag = "6")]
    revision: Option<RevisionWire>,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct RevisionWire {
    #[prost(uint64, tag = "1")]
    presence: u64,
    #[prost(uint64, tag = "2")]
    publications: u64,
}

impl From<RevisionWire> for owner::Revision {
    fn from(wire: RevisionWire) -> Self {
        Self {
            presence: wire.presence,
            publications: wire.publications,
        }
    }
}

impl From<owner::Revision> for RevisionWire {
    fn from(revision: owner::Revision) -> Self {
        Self {
            presence: revision.presence,
            publications: revision.publications,
        }
    }
}

#[derive(Clone, PartialEq, Message)]
struct ReplyWire {
    #[prost(message, optional, tag = "1")]
    revision: Option<RevisionWire>,
    #[prost(oneof = "ResultWire", tags = "2, 3, 4, 5")]
    result: Option<ResultWire>,
}

#[derive(Clone, PartialEq, Oneof)]
enum ResultWire {
    #[prost(message, tag = "2")]
    Row(wire::CatalogParticipantEntry),
    #[prost(string, tag = "3")]
    ScanMore(String),
    #[prost(bool, tag = "4")]
    Exhausted(bool),
    #[prost(enumeration = "wire::FailureCode", tag = "5")]
    Error(i32),
}

#[derive(Clone, PartialEq, Message)]
struct Envelope {
    #[prost(uint64, tag = "1")]
    request: u64,
    #[prost(oneof = "Body", tags = "2, 3")]
    body: Option<Body>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Body {
    #[prost(message, tag = "2")]
    Get(RequestWire),
    #[prost(message, tag = "3")]
    Reply(ReplyWire),
}

pub(in crate::control::controller) struct Get {
    pub(in crate::control::controller) project: ProjectId,
    pub(in crate::control::controller) discover: RoomDiscover,
    pub(in crate::control::controller) interest: NonZeroU64,
    pub(in crate::control::controller) after: Option<ParticipantId>,
    pub(in crate::control::controller) revision: Option<owner::Revision>,
}

pub(in crate::control::controller) enum MessageOnStream {
    Get(NonZeroU64, Get),
    Reply(NonZeroU64, Result<owner::NeighborWindow, wire::FailureCode>),
}

pub(in crate::control::controller) fn recognizes(frame: &[u8]) -> bool {
    frame.starts_with(TAG)
}

pub(in crate::control::controller) fn decode(frame: &[u8]) -> Option<MessageOnStream> {
    if frame.len() > REGIONAL_FRAME_BYTES {
        return None;
    }
    let envelope = Envelope::decode(frame.strip_prefix(TAG)?).ok()?;
    let request = NonZeroU64::new(envelope.request)?;
    match envelope.body? {
        Body::Get(get) if get.ordering == u32::from(ORDERING_VERSION) => {
            Some(MessageOnStream::Get(
                request,
                Get {
                    project: get.project.parse().ok()?,
                    discover: serde_json::from_slice(&get.discover).ok()?,
                    interest: NonZeroU64::new(get.interest)?,
                    after: match get.after {
                        Some(after) => Some(ParticipantId::new(&after).ok()?),
                        None => None,
                    },
                    revision: get.revision.map(Into::into),
                },
            ))
        }
        Body::Get(_) => None,
        Body::Reply(reply) => Some(MessageOnStream::Reply(request, decode_reply(reply)?)),
    }
}

fn decode_reply(reply: ReplyWire) -> Option<Result<owner::NeighborWindow, wire::FailureCode>> {
    let result = match reply.result? {
        ResultWire::Row(entry)
            if entry.encoded_len() <= limits::MAX_CATALOG_ENTRY_BYTES
                && entry.tracks.len() <= limits::MAX_TRACKS_PER_PARTICIPANT =>
        {
            let canonical = entry.participant.as_ref()?.participant_id.clone();
            ParticipantId::new(&canonical).ok()?;
            owner::NeighborWindow::Row {
                revision: reply.revision?.into(),
                candidate: owner::Candidate {
                    canonical,
                    entry: Some(entry),
                },
            }
        }
        ResultWire::ScanMore(scanned_through) => {
            ParticipantId::new(&scanned_through).ok()?;
            owner::NeighborWindow::ScanMore {
                revision: reply.revision?.into(),
                scanned_through,
            }
        }
        ResultWire::Exhausted(true) => owner::NeighborWindow::Exhausted {
            revision: reply.revision?.into(),
        },
        ResultWire::Error(code) if reply.revision.is_none() => {
            return Some(Err(wire::FailureCode::try_from(code).ok()?));
        }
        _ => return None,
    };
    Some(Ok(result))
}

impl Get {
    pub(in crate::control::controller) fn encode(&self, request: NonZeroU64) -> Option<Box<[u8]>> {
        encode(
            request,
            Body::Get(RequestWire {
                ordering: u32::from(ORDERING_VERSION),
                project: self.project.as_str(),
                discover: serde_json::to_vec(&self.discover).ok()?,
                interest: self.interest.get(),
                after: self.after.as_ref().map(|after| after.as_str().to_owned()),
                revision: self.revision.map(Into::into),
            }),
        )
    }

    pub(in crate::control::controller) fn serve(
        self,
        request: NonZeroU64,
        controller: &ControllerActor,
        declarations: &Exchange,
        now: Instant,
    ) -> Option<Box<[u8]>> {
        let room = RoomId::new(self.discover.room()).ok()?;
        if !declarations.published_queryable(self.interest, self.project, room, now) {
            return None;
        }
        self.serve_routed(request, controller)
    }

    pub(in crate::control::controller) fn serve_routed(
        self,
        request: NonZeroU64,
        controller: &ControllerActor,
    ) -> Option<Box<[u8]>> {
        let result = owner::read_neighbor(
            controller,
            owner::NeighborRead {
                project: self.project,
                discover: self.discover,
                after: self.after.as_ref().map(ParticipantId::as_str),
                revision: self.revision,
            },
        );
        encode(request, Body::Reply(encode_reply(result)))
    }
}

fn encode_reply(result: Result<owner::NeighborWindow, wire::FailureCode>) -> ReplyWire {
    let (revision, result) = match result {
        Ok(owner::NeighborWindow::Row {
            revision,
            candidate: owner::Candidate {
                entry: Some(entry), ..
            },
        }) if entry.encoded_len() <= limits::MAX_CATALOG_ENTRY_BYTES => {
            (Some(revision), ResultWire::Row(entry))
        }
        Ok(owner::NeighborWindow::Row { .. }) => (
            None,
            ResultWire::Error(wire::FailureCode::CapacityExceeded.into()),
        ),
        Ok(owner::NeighborWindow::ScanMore {
            revision,
            scanned_through,
        }) => (Some(revision), ResultWire::ScanMore(scanned_through)),
        Ok(owner::NeighborWindow::Exhausted { revision }) => {
            (Some(revision), ResultWire::Exhausted(true))
        }
        Err(code) => (None, ResultWire::Error(code.into())),
    };
    ReplyWire {
        revision: revision.map(Into::into),
        result: Some(result),
    }
}

fn encode(request: NonZeroU64, body: Body) -> Option<Box<[u8]>> {
    let envelope = Envelope {
        request: request.get(),
        body: Some(body),
    };
    let len = envelope.encoded_len().checked_add(TAG.len())?;
    if len > REGIONAL_FRAME_BYTES {
        return None;
    }
    let mut frame = Vec::with_capacity(len);
    frame.extend_from_slice(TAG);
    envelope.encode(&mut frame).ok()?;
    Some(frame.into_boxed_slice())
}
