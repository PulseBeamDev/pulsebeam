use super::{ConnectionId, ProjectRoomKey, owner, wire};
use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::time::Instant;
use std::{collections::BTreeMap, num::NonZeroU64};

use crate::cluster::MAX_CATALOG_MEMBERS;

pub(in crate::control::controller) type Peer = (NodeId, NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::control::controller) struct ReadScope {
    pub(in crate::control::controller) room: ProjectRoomKey,
    pub(in crate::control::controller) viewer: ConnectionId,
    pub(in crate::control::controller) authorization: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::control::controller) struct DeclarationId {
    pub(in crate::control::controller) stream: NonZeroU64,
    pub(in crate::control::controller) interest: NonZeroU64,
    pub(in crate::control::controller) changed_at: NonZeroU64,
}

pub(in crate::control::controller) enum ReadStep {
    Row(owner::Candidate),
    Exhausted,
    Refill,
}

pub(in crate::control::controller) trait RoomRead {
    fn scope(&self) -> ReadScope;
    fn revision(&self) -> owner::Revision;
    fn next(&self, after: Option<&str>) -> Result<ReadStep, wire::FailureCode>;
}

#[derive(Clone, Copy)]
pub(in crate::control::controller) struct Reply<'a> {
    pub(in crate::control::controller) peer: Peer,
    pub(in crate::control::controller) declaration: DeclarationId,
    pub(in crate::control::controller) fresh_until: Instant,
    pub(in crate::control::controller) view: &'a dyn RoomRead,
}

/// The current state of an authenticated responder's scoped declaration sync.
#[derive(Clone, Copy)]
pub(in crate::control::controller) enum Observation<'a> {
    Ready(Reply<'a>),
    Empty {
        peer: Peer,
        scope: ReadScope,
        stream: NonZeroU64,
        interest: NonZeroU64,
        changed_at: NonZeroU64,
        fresh_until: Instant,
    },
    Gap {
        peer: Peer,
        scope: ReadScope,
    },
}

impl Observation<'_> {
    fn peer_and_scope(&self) -> (Peer, ReadScope) {
        match self {
            Self::Ready(reply) => (reply.peer, reply.view.scope()),
            Self::Gap { peer, scope } | Self::Empty { peer, scope, .. } => (*peer, *scope),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReadyRevision {
    Catalog {
        declaration: DeclarationId,
        catalog: owner::Revision,
    },
    Empty {
        stream: NonZeroU64,
        interest: NonZeroU64,
        changed_at: NonZeroU64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Cut {
    pub(super) local: owner::Revision,
    pub(super) peers: Box<[(Peer, Option<ReadyRevision>)]>,
}

impl Cut {
    pub(super) fn partial(&self) -> bool {
        self.peers.iter().any(|(_, revision)| revision.is_none())
    }
}

pub(super) struct Responders<'a> {
    views: Vec<&'a dyn RoomRead>,
    cut: Cut,
    fresh_until: Option<Instant>,
}

impl<'a> Responders<'a> {
    pub(super) fn at_cut(
        local: &'a dyn RoomRead,
        expected: impl IntoIterator<Item = Peer>,
        observations: impl IntoIterator<Item = Observation<'a>>,
        now: Instant,
    ) -> Result<Self, wire::FailureCode> {
        let mut peers = BTreeMap::new();
        let mut views = vec![local];
        let mut fresh_until: Option<Instant> = None;
        for (node, epoch) in expected {
            if let Some((previous, _)) = peers.insert(node, (epoch, None))
                && previous != epoch
            {
                return Err(wire::FailureCode::ResourceGone);
            }
            if peers.len() > MAX_CATALOG_MEMBERS {
                return Err(wire::FailureCode::CapacityExceeded);
            }
        }
        for observation in observations {
            let ((node, epoch), scope) = observation.peer_and_scope();
            let Some((expected_epoch, slot)) = peers.get_mut(&node) else {
                continue;
            };
            if *expected_epoch != epoch || scope != local.scope() {
                continue;
            }
            let (revision, deadline) = match observation {
                Observation::Gap { .. } => return Err(wire::FailureCode::ResourceGone),
                Observation::Ready(reply) if reply.fresh_until > now => {
                    views.push(reply.view);
                    (
                        ReadyRevision::Catalog {
                            declaration: reply.declaration,
                            catalog: reply.view.revision(),
                        },
                        reply.fresh_until,
                    )
                }
                Observation::Empty {
                    stream,
                    interest,
                    changed_at,
                    fresh_until,
                    ..
                } if fresh_until > now => (
                    ReadyRevision::Empty {
                        stream,
                        interest,
                        changed_at,
                    },
                    fresh_until,
                ),
                _ => continue,
            };
            fresh_until = Some(fresh_until.map_or(deadline, |current| current.min(deadline)));
            if slot.replace(revision).is_some() {
                return Err(wire::FailureCode::ResourceGone);
            }
        }
        let peers = peers
            .into_iter()
            .map(|(node, (epoch, revision))| ((node, epoch), revision))
            .collect();
        Ok(Self {
            views,
            fresh_until,
            cut: Cut {
                local: local.revision(),
                peers,
            },
        })
    }

    pub(super) fn cut(&self) -> &Cut {
        &self.cut
    }

    pub(super) fn fresh_until(&self) -> Option<Instant> {
        self.fresh_until
    }

    pub(super) fn scan_cost(&self) -> usize {
        self.views.len()
    }

    pub(super) fn next(&self, after: Option<&str>) -> Result<ReadStep, wire::FailureCode> {
        let mut first: Option<owner::Candidate> = None;
        let mut conflict = false;
        let mut refill = false;
        for view in &self.views {
            let candidate = match view.next(after)? {
                ReadStep::Row(candidate) => candidate,
                ReadStep::Exhausted => continue,
                ReadStep::Refill => {
                    refill = true;
                    continue;
                }
            };
            if after.is_some_and(|after| candidate.canonical.as_str() <= after) {
                return Err(wire::FailureCode::ResourceGone);
            }
            match first.as_mut() {
                None => first = Some(candidate),
                Some(previous) => match candidate.canonical.cmp(&previous.canonical) {
                    std::cmp::Ordering::Less => {
                        *previous = candidate;
                        conflict = false;
                    }
                    std::cmp::Ordering::Equal => {
                        if previous.entry.is_some() && candidate.entry.is_some() {
                            conflict = true;
                        } else if previous.entry.is_none() {
                            *previous = candidate;
                        }
                    }
                    std::cmp::Ordering::Greater => {}
                },
            }
        }
        if refill {
            return Ok(ReadStep::Refill);
        }
        if conflict {
            return Err(wire::FailureCode::ResourceGone);
        }
        Ok(first.map_or(ReadStep::Exhausted, ReadStep::Row))
    }
}

#[cfg(test)]
mod tests;
