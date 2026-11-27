use std::num::{NonZeroU32, NonZeroU64};

use pulsebeam_core::identity::NodeId;

use super::Member;

const TAG: &[u8] = &[1, 0x13];
pub(super) const HOPS: u8 = 64;
const MEMBER_BYTES: usize = 24;
const MAX_BYTES: usize = crate::cluster::REGIONAL_FRAME_BYTES;

fn put_member(out: &mut Vec<u8>, member: Member) {
    out.extend_from_slice(member.node.as_bytes());
    out.extend_from_slice(&member.generation.get().to_be_bytes());
}

fn take_member(input: &mut &[u8]) -> Option<Member> {
    let (member, remaining) = input.split_at_checked(MEMBER_BYTES)?;
    *input = remaining;
    let (node, generation) = member.split_at(16);
    Some(Member {
        node: NodeId::from_bytes(node.try_into().ok()?),
        generation: NonZeroU64::new(u64::from_be_bytes(generation.try_into().ok()?))?,
    })
}

#[derive(Clone, Copy)]
pub(super) struct Request<'a> {
    pub(super) target: Member,
    pub(super) border: Option<(Member, Member)>,
    pub(super) budget: u8,
    pub(super) attempt: u8,
    pub(super) payload: &'a [u8],
}

fn take_request(bytes: &[u8]) -> Option<Request<'_>> {
    let (&budget, rest) = bytes.split_first()?;
    let (&attempt, rest) = rest.split_first()?;
    let (&has_border, mut payload) = rest.split_first()?;
    if !(1..=HOPS).contains(&budget) {
        return None;
    }
    let target = take_member(&mut payload)?;
    let border = match has_border {
        0 => None,
        1 => Some((take_member(&mut payload)?, take_member(&mut payload)?)),
        _ => return None,
    };
    (!payload.is_empty()).then_some(Request {
        target,
        border,
        budget,
        attempt,
        payload,
    })
}

fn put_request<'a>(bytes: &mut Vec<u8>, request: Request<'a>) -> Option<&'a [u8]> {
    if !(1..=HOPS).contains(&request.budget) {
        return None;
    }
    bytes.extend_from_slice(&[
        request.budget,
        request.attempt,
        u8::from(request.border.is_some()),
    ]);
    put_member(bytes, request.target);
    if let Some((local, remote)) = request.border {
        put_member(bytes, local);
        put_member(bytes, remote);
    }
    Some(request.payload)
}

pub(super) enum Frame<'a> {
    Request(NonZeroU64, Request<'a>),
    Reply(NonZeroU64, &'a [u8]),
    Branch(Request<'a>),
    BranchRetired(NonZeroU32),
}

pub(in crate::control::controller) fn recognizes(frame: &[u8]) -> bool {
    frame.starts_with(TAG)
}

impl Frame<'_> {
    pub(super) fn decode(bytes: &[u8]) -> Option<Frame<'_>> {
        if bytes.len() > MAX_BYTES {
            return None;
        }
        let bytes = bytes.strip_prefix(TAG)?;
        let (&kind, rest) = bytes.split_first()?;
        match kind {
            2 => return Some(Frame::Branch(take_request(rest)?)),
            3 => {
                return Some(Frame::BranchRetired(NonZeroU32::new(u32::from_be_bytes(
                    rest.try_into().ok()?,
                ))?));
            }
            _ => {}
        }
        let (id, rest) = rest.split_at_checked(8)?;
        let id = NonZeroU64::new(u64::from_be_bytes(id.try_into().ok()?))?;
        match kind {
            0 => Some(Frame::Request(id, take_request(rest)?)),
            1 if !rest.is_empty() => Some(Frame::Reply(id, rest)),
            _ => None,
        }
    }

    pub(super) fn encode(self) -> Option<Box<[u8]>> {
        let mut bytes = TAG.to_vec();
        let payload = match self {
            Self::Request(id, request) => {
                bytes.push(0);
                bytes.extend_from_slice(&id.get().to_be_bytes());
                put_request(&mut bytes, request)?
            }
            Self::Reply(id, payload) => {
                bytes.push(1);
                bytes.extend_from_slice(&id.get().to_be_bytes());
                payload
            }
            Self::Branch(request) => {
                bytes.push(2);
                put_request(&mut bytes, request)?
            }
            Self::BranchRetired(label) => {
                bytes.push(3);
                bytes.extend_from_slice(&label.get().to_be_bytes());
                return Some(bytes.into_boxed_slice());
            }
        };
        if bytes.len().saturating_add(payload.len()) > MAX_BYTES || payload.is_empty() {
            return None;
        }
        bytes.extend_from_slice(payload);
        Some(bytes.into_boxed_slice())
    }
}
