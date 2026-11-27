use std::{collections::BTreeSet, num::NonZeroU64};

use pulsebeam_core::fabric::MAX_HOPS;
use serde::{Deserialize, Serialize};

use crate::cluster::declarations::Role;

use super::{Member, RegionTree};

/// A protected leg is pinned from consumer to origin. It cannot be merged
/// into ordinary demand and must not reach an unlisted relay by rerouting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control::controller::regional_routing) struct ProtectionPath {
    path: Vec<Member>,
    wire_bps: NonZeroU64,
}

impl ProtectionPath {
    pub(in crate::control::controller::regional_routing) fn new(
        path: Vec<Member>,
        wire_bps: NonZeroU64,
    ) -> Option<Self> {
        let nodes: BTreeSet<_> = path.iter().map(|hop| hop.node).collect();
        (path.len() >= 2
            && path.len() <= usize::from(MAX_HOPS).saturating_add(1)
            && nodes.len() == path.len())
        .then_some(Self { path, wire_bps })
    }

    pub(in crate::control::controller::regional_routing) fn wire_bps(&self) -> NonZeroU64 {
        self.wire_bps
    }

    pub(in crate::control::controller::regional_routing) fn next(
        &self,
        current: Member,
    ) -> Option<Member> {
        let index = self.path.iter().position(|member| *member == current)?;
        self.path.get(index.checked_add(1)?).copied()
    }

    pub(in crate::control::controller::regional_routing) fn includes_relay(
        &self,
        relay: Member,
    ) -> bool {
        matches!(self.path.as_slice(), [_, middle @ .., _] if middle.contains(&relay))
    }

    pub(super) fn matches_hop(
        &self,
        tree: &RegionTree,
        local: Member,
        origin: Member,
        traversed: &[Member],
    ) -> bool {
        if self.path.last() != Some(&origin)
            || self
                .path
                .strip_prefix(traversed)
                .and_then(|rest| rest.first())
                != Some(&local)
            || self.path.len() > usize::from(MAX_HOPS).saturating_add(1)
        {
            return false;
        }
        let nodes: BTreeSet<_> = self.path.iter().map(|hop| hop.node).collect();
        nodes.len() == self.path.len()
            && self.path.windows(2).enumerate().all(|(index, edge)| {
                let [from, to] = edge else { return false };
                tree.adjacency(*from, *to).is_some_and(|adjacent| {
                    !adjacent.is_border() && (index == 0 || adjacent.roles.0 == Role::Router)
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use pulsebeam_core::identity::NodeId;

    use super::*;

    #[test]
    fn checks_the_complete_leg_and_exact_authenticated_prefix() {
        let member = || Member {
            node: NodeId::new(),
            generation: NonZeroU64::MIN,
        };
        let (consumer, relay, origin) = (member(), member(), member());
        let tree: RegionTree = serde_json::from_value(serde_json::json!({
            "regions": [{"name":"region", "members":[
                {"node":consumer.node, "generation":1, "role":"gateway"},
                {"node":relay.node, "generation":1, "role":"router"},
                {"node":origin.node, "generation":1, "role":"router"}
            ]}],
            "borders":[]
        }))
        .unwrap();
        let path = ProtectionPath::new(vec![consumer, relay, origin], NonZeroU64::MIN).unwrap();
        assert!(path.matches_hop(&tree, relay, origin, &[consumer]));
        assert!(path.matches_hop(&tree, origin, origin, &[consumer, relay]));
        assert!(!path.matches_hop(&tree, origin, origin, &[consumer]));
        assert!(!path.matches_hop(&tree, origin, origin, &[relay, consumer]));
        let restarted = Member {
            generation: NonZeroU64::new(2).unwrap(),
            ..relay
        };
        let stale =
            ProtectionPath::new(vec![consumer, restarted, origin], NonZeroU64::MIN).unwrap();
        assert!(!stale.matches_hop(&tree, origin, origin, &[consumer, restarted]));
        assert!(
            ProtectionPath::new(vec![consumer, relay, restarted, origin], NonZeroU64::MIN)
                .is_none()
        );
    }
}
