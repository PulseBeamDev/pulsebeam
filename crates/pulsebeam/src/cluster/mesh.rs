//! Weighted routes within one region. Publication ownership and installed
//! Fabric branches are deliberately outside this graph.

use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU64},
    time::Duration,
};

use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::{MAX_PROVISIONED_PEERS, declarations::Role};

mod exchange;
pub(crate) use exchange::{Exchange, Outbound};

pub(crate) struct Port {
    pub(crate) sessions: pulsebeam_runtime::sync::mpsc::Receiver<pulsebeam_link::PeerControl>,
    pub(crate) roles: (Role, Role),
    pub(crate) cost: LinkCost,
}

pub(crate) enum LinkCost {
    #[cfg(not(feature = "sim"))]
    Fixed(NonZeroU16),
    #[cfg(feature = "sim")]
    Sim(SimLinkCost),
}

impl LinkCost {
    pub(crate) fn get(&self) -> NonZeroU16 {
        match self {
            #[cfg(not(feature = "sim"))]
            Self::Fixed(cost) => *cost,
            #[cfg(feature = "sim")]
            Self::Sim(cost) => cost.get(),
        }
    }
}

#[cfg(feature = "sim")]
#[derive(Clone)]
pub struct SimLinkCost(std::sync::Arc<std::sync::atomic::AtomicU16>);

#[cfg(feature = "sim")]
impl SimLinkCost {
    pub fn new(cost: NonZeroU16) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU16::new(
            cost.get(),
        )))
    }

    pub fn set(&self, cost: NonZeroU16) {
        self.0
            .store(cost.get(), std::sync::atomic::Ordering::Relaxed);
    }

    fn get(&self) -> NonZeroU16 {
        NonZeroU16::new(self.0.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_else(default_link_cost)
    }
}

pub(super) const MAX_MEMBERS: usize = 64;
const HOLD: Duration = Duration::from_secs(1);

pub(super) const fn default_link_cost() -> NonZeroU16 {
    const { NonZeroU16::new(100).unwrap() }
}

/// A restart changes every incident link, even when a node keeps its ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Member {
    pub(crate) node: NodeId,
    pub(crate) generation: NonZeroU64,
}

#[derive(Clone, PartialEq, Eq)]
struct Advertisement {
    pub(super) origin: Member,
    pub(super) role: Role,
    pub(super) neighbors: BTreeMap<Member, NonZeroU16>,
}

/// The graph consumes the current, fresh advertisements from the regional lane.
/// It does not admit or retain remote state itself.
pub(super) struct Graph<'a>(BTreeMap<NodeId, &'a Advertisement>);

impl<'a> Graph<'a> {
    pub(super) fn from_reports(
        reports: impl IntoIterator<Item = &'a Advertisement>,
    ) -> Option<Self> {
        let mut nodes = BTreeMap::new();
        for report in reports {
            if nodes.len() == MAX_MEMBERS
                || report.neighbors.len() > MAX_PROVISIONED_PEERS
                || report
                    .neighbors
                    .keys()
                    .any(|peer| peer.node == report.origin.node)
                || nodes.insert(report.origin.node, report).is_some()
            {
                return None;
            }
        }
        Some(Self(nodes))
    }

    fn advertisement(&self, member: Member) -> Option<&Advertisement> {
        self.0
            .get(&member.node)
            .copied()
            .filter(|report| report.origin == member)
    }

    fn cost(&self, a: &Advertisement, b: &Advertisement) -> Option<u32> {
        if !a.role.adjacent(b.role) {
            return None;
        }
        let outgoing = a.neighbors.get(&b.origin)?;
        let incoming = b.neighbors.get(&a.origin)?;
        Some(u32::from(outgoing.max(incoming).get()))
    }

    fn carries(&self, source: Member, path: &[Member]) -> bool {
        let Some(mut from) = self.advertisement(source) else {
            return false;
        };
        for (index, &node) in path.iter().enumerate() {
            let Some(to) = self.advertisement(node) else {
                return false;
            };
            if self.cost(from, to).is_none()
                || (index < path.len().saturating_sub(1) && to.role != Role::Router)
            {
                return false;
            }
            from = to;
        }
        true
    }

    fn shortest_paths(&self, source: Member) -> BTreeMap<Member, Vec<Member>> {
        let mut routes = BTreeMap::new();
        if self.advertisement(source).is_none() {
            return routes;
        }
        let mut candidates = BTreeMap::from([(source, (0u32, Vec::new()))]);
        while let Some((&best, _)) = candidates.iter().min_by(|(_, a), (_, b)| a.cmp(b)) {
            let Some((distance, path)) = candidates.remove(&best) else {
                break;
            };
            let Some(origin) = self.advertisement(best) else {
                continue;
            };
            if best == source || origin.role == Role::Router {
                for &peer in origin.neighbors.keys() {
                    if peer == source || routes.contains_key(&peer) {
                        continue;
                    }
                    let Some(cost) = self
                        .advertisement(peer)
                        .and_then(|report| self.cost(origin, report))
                    else {
                        continue;
                    };
                    let mut next = path.clone();
                    next.push(peer);
                    let option = (distance.saturating_add(cost), next);
                    let current = candidates.entry(peer).or_insert_with(|| option.clone());
                    if option < *current {
                        *current = option;
                    }
                }
            }
            if best != source {
                routes.insert(best, path);
            }
        }
        routes
    }
}

struct Route {
    active: Vec<Member>,
    pending: Option<(Vec<Member>, Instant)>,
}

/// Hold only live paths. A lost link or incarnation bypasses the hold; a
/// cheaper healthy path must remain preferred before displacing the old one.
#[derive(Default)]
pub(super) struct Routes(BTreeMap<Member, Route>);

impl Routes {
    pub(super) fn reconcile(&mut self, source: Member, graph: &Graph<'_>, now: Instant) {
        let mut preferred = graph.shortest_paths(source);
        self.0.retain(|destination, route| {
            let Some(best) = preferred.remove(destination) else {
                return false;
            };
            if route.active == best {
                route.pending = None;
            } else if !graph.carries(source, &route.active)
                || route.pending.as_ref().is_some_and(|(candidate, since)| {
                    *candidate == best && now.saturating_duration_since(*since) >= HOLD
                })
            {
                *route = Route {
                    active: best,
                    pending: None,
                };
            } else if !route
                .pending
                .as_ref()
                .is_some_and(|(candidate, _)| *candidate == best)
            {
                route.pending = Some((best, now));
            }
            true
        });
        self.0.extend(preferred.into_iter().map(|(member, active)| {
            (
                member,
                Route {
                    active,
                    pending: None,
                },
            )
        }));
    }

    pub(super) fn path(&self, destination: Member) -> Option<&[Member]> {
        Some(&self.0.get(&destination)?.active)
    }
}

#[cfg(test)]
mod tests;
