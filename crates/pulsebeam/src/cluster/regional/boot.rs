//! Version 2 boot negotiation on one mutually leaf-pinned control stream.

use std::num::NonZeroU64;

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::PeerFingerprint;
use sha3::{Digest, Sha3_256};

use crate::cluster::mesh::Member;

const OFFER: &[u8] = b"PBRB\x02\x01";
const REPLY: &[u8] = b"PBRB\x02\x02";
const CONFIRM: &[u8] = b"PBRB\x02\x03";
const ACCEPTED: &[u8] = b"PBRB\x02\x04";
pub(super) const FRAME_BYTES: usize = 6 + 32 + 8 + 32 + 16;

type Challenge = [u8; 16];
type DigestBytes = [u8; 32];

#[derive(Clone, Copy)]
pub(super) struct Scope {
    pub(super) epoch: [u8; 16],
    pub(super) roster: DigestBytes,
    pub(super) initiator: NodeId,
    pub(super) responder: NodeId,
}

impl Scope {
    fn fingerprint(self) -> DigestBytes {
        hash(&[
            b"pulsebeam-regional-boot-v2\0",
            &self.epoch,
            &self.roster,
            self.initiator.as_bytes(),
            self.responder.as_bytes(),
        ])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Boot {
    pub(super) generation: NonZeroU64,
    pub(super) media_leaf: PeerFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Binding {
    pub(super) peer: Member,
    pub(super) media_leaf: PeerFingerprint,
    pub(super) session: DigestBytes,
}

impl Binding {
    pub(super) fn acceptance(self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(ACCEPTED.len().saturating_add(self.session.len()));
        frame.extend_from_slice(ACCEPTED);
        frame.extend_from_slice(&self.session);
        frame
    }

    pub(super) fn accepted(self, frame: &[u8]) -> bool {
        frame.strip_prefix(ACCEPTED) == Some(self.session.as_slice())
    }
}

pub(super) struct AwaitReply {
    peer: NodeId,
    offer: DigestBytes,
}

pub(super) struct AwaitConfirm {
    binding: Binding,
}

pub(super) fn start(scope: Scope, local: Boot, challenge: Challenge) -> (Vec<u8>, AwaitReply) {
    let offer = encode(OFFER, scope.fingerprint(), local, challenge);
    let awaiting = AwaitReply {
        peer: scope.responder,
        offer: hash(&[&offer]),
    };
    (offer, awaiting)
}

pub(super) fn answer(
    scope: Scope,
    offer: &[u8],
    local: Boot,
    challenge: Challenge,
) -> Option<(Vec<u8>, AwaitConfirm)> {
    let (expected, remote) = decode(OFFER, offer)?;
    if expected != scope.fingerprint() {
        return None;
    }
    let offered = hash(&[offer]);
    let reply = encode(REPLY, offered, local, challenge);
    let binding = bind(scope.initiator, remote, offered, &reply);
    Some((reply, AwaitConfirm { binding }))
}

impl AwaitReply {
    pub(super) fn finish(self, reply: &[u8]) -> Option<(Vec<u8>, Binding)> {
        let (echo, remote) = decode(REPLY, reply)?;
        if echo != self.offer {
            return None;
        }
        let binding = bind(self.peer, remote, self.offer, reply);
        let mut confirm = Vec::with_capacity(CONFIRM.len().saturating_add(binding.session.len()));
        confirm.extend_from_slice(CONFIRM);
        confirm.extend_from_slice(&binding.session);
        Some((confirm, binding))
    }
}

impl AwaitConfirm {
    pub(super) fn finish(self, confirm: &[u8]) -> Option<Binding> {
        (confirm.strip_prefix(CONFIRM)? == self.binding.session).then_some(self.binding)
    }
}

fn bind(peer: NodeId, remote: Boot, offer: DigestBytes, reply: &[u8]) -> Binding {
    Binding {
        peer: Member {
            node: peer,
            generation: remote.generation,
        },
        media_leaf: remote.media_leaf,
        session: hash(&[b"pulsebeam-regional-session-v2\0", &offer, reply]),
    }
}

fn encode(tag: &[u8], echo: DigestBytes, boot: Boot, challenge: Challenge) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_BYTES);
    frame.extend_from_slice(tag);
    frame.extend_from_slice(&echo);
    frame.extend_from_slice(&boot.generation.get().to_be_bytes());
    frame.extend_from_slice(&boot.media_leaf.to_bytes());
    frame.extend_from_slice(&challenge);
    frame
}

fn decode(tag: &[u8], frame: &[u8]) -> Option<(DigestBytes, Boot)> {
    if frame.len() != FRAME_BYTES {
        return None;
    }
    let (echo, fields) = frame.strip_prefix(tag)?.split_first_chunk::<32>()?;
    let (generation, fields) = fields.split_first_chunk::<8>()?;
    let (pin, _) = fields.split_first_chunk::<32>()?;
    Some((
        *echo,
        Boot {
            generation: NonZeroU64::new(u64::from_be_bytes(*generation))?,
            media_leaf: PeerFingerprint::from_bytes(*pin),
        },
    ))
}

fn hash(parts: &[&[u8]]) -> DigestBytes {
    let mut hasher = Sha3_256::new();
    for bytes in parts {
        hasher.update(bytes);
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(generation: u64, pin: u8) -> Boot {
        Boot {
            generation: NonZeroU64::new(generation).unwrap(),
            media_leaf: PeerFingerprint::from_bytes([pin; 32]),
        }
    }

    fn scope() -> Scope {
        Scope {
            epoch: [1; 16],
            roster: [2; 32],
            initiator: NodeId::new(),
            responder: NodeId::new(),
        }
    }

    #[test]
    fn fresh_exchange_binds_both_boots_and_pins() {
        let scope = scope();
        let first = boot(3, 4);
        let second = boot(5, 6);
        let (offer, caller) = start(scope, first, [7; 16]);
        let (reply, listener) = answer(scope, &offer, second, [8; 16]).unwrap();
        let (confirm, outbound) = caller.finish(&reply).unwrap();
        let inbound = listener.finish(&confirm).unwrap();
        assert_eq!(
            outbound.peer,
            Member {
                node: scope.responder,
                generation: second.generation
            }
        );
        assert_eq!(outbound.media_leaf, second.media_leaf);
        assert_eq!(
            inbound.peer,
            Member {
                node: scope.initiator,
                generation: first.generation
            }
        );
        assert_eq!(inbound.media_leaf, first.media_leaf);
        assert_eq!(outbound.session, inbound.session);
    }

    #[test]
    fn rejects_wrong_scope_malformed_frames_and_crossed_attempts() {
        let scope = scope();
        let first = boot(1, 4);
        let second = boot(2, 6);
        let (offer, caller) = start(scope, first, [7; 16]);
        for wrong in [
            Scope {
                epoch: [3; 16],
                ..scope
            },
            Scope {
                roster: [4; 32],
                ..scope
            },
            Scope {
                initiator: scope.responder,
                responder: scope.initiator,
                ..scope
            },
        ] {
            assert!(answer(wrong, &offer, second, [8; 16]).is_none());
        }
        for end in 0..offer.len() {
            assert!(answer(scope, &offer[..end], second, [8; 16]).is_none());
        }
        let mut legacy = offer.clone();
        legacy[4] = 1;
        assert!(answer(scope, &legacy, second, [8; 16]).is_none());
        let mut zero_generation = offer.clone();
        zero_generation[38..46].fill(0);
        assert!(answer(scope, &zero_generation, second, [8; 16]).is_none());
        let (reply, _) = answer(scope, &offer, second, [8; 16]).unwrap();
        let (confirm, _) = caller.finish(&reply).unwrap();
        let (_, other_caller) = start(scope, first, [9; 16]);
        assert!(other_caller.finish(&reply).is_none());
        let (_, newer_listener) = answer(scope, &offer, boot(3, 7), [10; 16]).unwrap();
        assert!(newer_listener.finish(&confirm).is_none());
    }
}
