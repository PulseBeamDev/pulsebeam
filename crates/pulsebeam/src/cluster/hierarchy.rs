//! Configured region membership and border authorization. No liveness or
//! publication state belongs to this tree.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use anyhow::{Context, Result, ensure};
use pulsebeam_core::{auth::ApiPublicKey, identity::NodeId};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

use super::{declarations::Role, mesh::Member};

const MAX_REGIONS: usize = 16;
const MAX_MEMBERS: usize = 256;
pub(crate) const MAX_CATALOG_MEMBERS: usize = 16;
const MAX_BORDER_PAIRS: usize = super::config::MAX_PROVISIONED_PEERS;

#[derive(Deserialize)]
#[cfg_attr(test, derive(Clone, Serialize))]
#[serde(deny_unknown_fields)]
struct Manifest {
    regions: Vec<RegionConfig>,
    borders: Vec<Border>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Clone, Serialize))]
#[serde(deny_unknown_fields)]
struct RegionConfig {
    name: String,
    parent: Option<String>,
    members: Vec<ConfiguredMember>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredMember {
    node: NodeId,
    generation: NonZeroU64,
    role: Role,
    #[serde(default)]
    catalog: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_key: Option<ApiPublicKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot_key: Option<ApiPublicKey>,
}

impl ConfiguredMember {
    fn identity(self) -> Member {
        Member {
            node: self.node,
            generation: self.generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Border {
    child: NodeId,
    parent: NodeId,
}

/// An opt-in roster of exact node incarnations and the physical pairs allowed
/// on each logical parent-child border.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "Manifest")]
pub struct RegionTree {
    parents: BTreeMap<String, Option<String>>,
    members: BTreeMap<NodeId, (String, ConfiguredMember)>,
    borders: BTreeMap<String, BTreeSet<Border>>,
    fingerprint: [u8; 32],
    admitted: Option<AdmittedBoots>,
}

#[derive(Clone, Debug)]
struct AdmittedBoots {
    local: Member,
    remote: BTreeMap<NodeId, NonZeroU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Adjacency<'a> {
    pub(crate) local_region: &'a str,
    pub(crate) peer_region: &'a str,
    pub(crate) roles: (Role, Role),
}

impl Adjacency<'_> {
    pub(crate) fn is_border(&self) -> bool {
        self.local_region != self.peer_region
    }
}

impl TryFrom<Manifest> for RegionTree {
    type Error = anyhow::Error;

    fn try_from(config: Manifest) -> Result<Self> {
        ensure!(
            (1..=MAX_REGIONS).contains(&config.regions.len()),
            "invalid number of regions"
        );
        ensure!(
            config.borders.len()
                <= config
                    .regions
                    .len()
                    .saturating_sub(1)
                    .saturating_mul(MAX_BORDER_PAIRS),
            "too many configured gateway pairs"
        );
        let mut parents = BTreeMap::new();
        let mut members = BTreeMap::new();
        let mut member_keys = BTreeMap::new();
        for region in config.regions {
            ensure!(valid_region_name(&region.name), "invalid region name");
            ensure!(
                (1..=super::mesh::MAX_MEMBERS).contains(&region.members.len())
                    && members.len().saturating_add(region.members.len()) <= MAX_MEMBERS,
                "invalid region membership size"
            );
            for member in region.members {
                for key in [member.media_key, member.boot_key].into_iter().flatten() {
                    ensure!(
                        *member_keys.entry(*key.as_bytes()).or_insert(member.node) == member.node,
                        "reuse of an origin key across configured member slots"
                    );
                }
                ensure!(
                    members
                        .insert(member.node, (region.name.clone(), member))
                        .is_none(),
                    "node belongs to more than one configured member slot"
                );
            }
            ensure!(
                parents.insert(region.name, region.parent).is_none(),
                "duplicate region"
            );
        }
        ensure!(
            members
                .values()
                .filter(|(_, member)| member.catalog)
                .count()
                <= MAX_CATALOG_MEMBERS,
            "too many configured catalog responders"
        );
        ensure!(
            parents.values().filter(|parent| parent.is_none()).count() == 1,
            "region tree requires exactly one root"
        );
        for region in parents.keys() {
            let mut current = region.as_str();
            for depth in 0..parents.len() {
                let parent = parents.get(current).context("unknown parent region")?;
                let Some(parent) = parent else { break };
                ensure!(
                    depth < parents.len().saturating_sub(1),
                    "cycle in region parents"
                );
                current = parent;
            }
        }
        let mut borders: BTreeMap<String, BTreeSet<Border>> = BTreeMap::new();
        for pair in config.borders {
            let (child, child_member) =
                members.get(&pair.child).context("unknown child gateway")?;
            let (parent, parent_member) = members
                .get(&pair.parent)
                .context("unknown parent gateway")?;
            ensure!(
                parents.get(child).and_then(Option::as_ref) == Some(parent)
                    && child_member.role == Role::Gateway
                    && parent_member.role == Role::Gateway,
                "gateway pair must cross a configured parent-child border"
            );
            let pairs = borders.entry(child.clone()).or_default();
            ensure!(
                pairs.len() < MAX_BORDER_PAIRS && pairs.insert(pair),
                "duplicate gateway pair or border capacity exceeded"
            );
        }
        ensure!(
            parents
                .iter()
                .all(|(region, parent)| parent.is_none() || borders.contains_key(region)),
            "each parent-child border requires a gateway pair"
        );
        let canonical = serde_json::to_vec(&(&parents, &members, &borders))?;
        let fingerprint = Sha3_256::digest(canonical).into();
        Ok(Self {
            parents,
            members,
            borders,
            fingerprint,
            admitted: None,
        })
    }
}

impl RegionTree {
    pub(crate) fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    /// Version 2 advertises stable policy. Configured generations become enrollment
    /// floors, not authorization for a running boot.
    pub(crate) fn begin_boot_admission(&mut self, local: Member) -> Result<()> {
        ensure!(
            self.admitted.is_none(),
            "regional boot admission is already enabled"
        );
        let (_, enrolled) = self
            .members
            .get(&local.node)
            .context("local node is not enrolled")?;
        ensure!(
            local.generation >= enrolled.generation,
            "local generation is below enrollment"
        );
        let stable_members: BTreeMap<_, _> = self
            .members
            .iter()
            .map(|(&node, (region, member))| {
                (
                    node,
                    (
                        region,
                        member.role,
                        member.catalog,
                        member.media_key,
                        member.boot_key,
                    ),
                )
            })
            .collect();
        let encoded = serde_json::to_vec(&(
            "pulsebeam-region-roster-v2",
            &self.parents,
            stable_members,
            &self.borders,
        ))?;
        self.fingerprint = Sha3_256::digest(encoded).into();
        self.admitted = Some(AdmittedBoots {
            local,
            remote: BTreeMap::new(),
        });
        Ok(())
    }

    /// Called only after boot authentication and durable floor admission.
    pub(crate) fn admit_boot(&mut self, member: Member) -> bool {
        let Some((_, enrolled)) = self.members.get(&member.node) else {
            return false;
        };
        let Some(admitted) = &mut self.admitted else {
            return false;
        };
        if member.node == admitted.local.node
            || member.generation < enrolled.generation
            || admitted
                .remote
                .get(&member.node)
                .is_some_and(|old| *old > member.generation)
        {
            return false;
        }
        admitted.remote.insert(member.node, member.generation);
        true
    }

    pub(crate) fn current_member(&self, node: NodeId) -> Option<Member> {
        self.current(self.members.get(&node)?.1)
    }

    pub(crate) fn withdraw_boot(&mut self, member: Member) {
        if let Some(admitted) = &mut self.admitted
            && admitted.remote.get(&member.node) == Some(&member.generation)
        {
            admitted.remote.remove(&member.node);
        }
    }

    fn current(&self, member: ConfiguredMember) -> Option<Member> {
        match &self.admitted {
            None => Some(member.identity()),
            Some(admitted) if member.node == admitted.local.node => Some(admitted.local),
            Some(admitted) => Some(Member {
                node: member.node,
                generation: *admitted.remote.get(&member.node)?,
            }),
        }
    }

    pub(crate) fn region(&self, identity: Member) -> Option<&str> {
        let (region, member) = self.members.get(&identity.node)?;
        (self.current(*member)? == identity).then_some(region.as_str())
    }

    pub(super) fn has_discovery_keys(&self) -> bool {
        self.members
            .values()
            .all(|(_, member)| member.boot_key.is_some())
    }

    pub(super) fn discovery_slots(&self) -> impl Iterator<Item = Member> + '_ {
        self.members
            .values()
            .filter_map(|(_, member)| member.boot_key.map(|_| member.identity()))
    }

    pub(crate) fn discovery_key(&self, origin: Member) -> Option<ApiPublicKey> {
        let (_, slot) = self.members.get(&origin.node)?;
        (origin.generation >= slot.generation)
            .then_some(slot.boot_key)
            .flatten()
    }

    pub(crate) fn media_key(&self, origin: Member) -> Option<ApiPublicKey> {
        let (_, member) = self.members.get(&origin.node)?;
        (self.current(*member)? == origin)
            .then_some(member.media_key)
            .flatten()
    }

    pub(crate) fn catalog_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.members
            .values()
            .filter(|(_, member)| member.catalog)
            .filter_map(|(_, member)| self.current(*member))
    }

    pub(crate) fn catalog_coverage_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.members
            .values()
            .filter(|(_, member)| member.catalog)
            .map(|(_, member)| {
                // The enrollment generation marks missing coverage, never a routable boot.
                self.current(*member).unwrap_or_else(|| member.identity())
            })
    }

    pub(crate) fn adjacency(&self, local: Member, peer: Member) -> Option<Adjacency<'_>> {
        self.region(local)?;
        self.region(peer)?;
        self.enrolled_adjacency(local.node, peer.node)
    }

    pub(crate) fn enrolled_adjacency(&self, local: NodeId, peer: NodeId) -> Option<Adjacency<'_>> {
        let (local_region, local_member) = self.members.get(&local)?;
        let (peer_region, peer_member) = self.members.get(&peer)?;
        let (local_region, peer_region) = (local_region.as_str(), peer_region.as_str());
        let roles = (local_member.role, peer_member.role);
        let allowed = if local_region == peer_region {
            local != peer && roles.0.adjacent(roles.1)
        } else {
            self.border_ids(local_region, peer_region)
                .any(|pair| pair == (local, peer))
        };
        allowed.then_some(Adjacency {
            local_region,
            peer_region,
            roles,
        })
    }

    /// The unique next region on a tree path. A local destination has no
    /// next region, as does a destination outside the configured roster.
    pub(crate) fn next_region(&self, from: &str, to: &str) -> Option<&str> {
        if from == to {
            return None;
        }
        let (mut child, mut parent) = self.parents.get_key_value(to)?;
        while let Some(ancestor) = parent {
            if ancestor == from {
                return Some(child);
            }
            (child, parent) = self.parents.get_key_value(ancestor)?;
        }
        self.parents.get(from)?.as_deref()
    }

    pub(crate) fn border_pairs<'a>(
        &'a self,
        from: &str,
        to: &str,
    ) -> impl Iterator<Item = (Member, Member)> + 'a {
        self.border_ids(from, to).filter_map(|(from, to)| {
            Some((
                self.current(self.members.get(&from)?.1)?,
                self.current(self.members.get(&to)?.1)?,
            ))
        })
    }

    fn border_ids<'a>(
        &'a self,
        from: &str,
        to: &str,
    ) -> impl Iterator<Item = (NodeId, NodeId)> + 'a {
        let up = self.parents.get(from).and_then(Option::as_deref) == Some(to);
        let down = self.parents.get(to).and_then(Option::as_deref) == Some(from);
        let pairs = if up {
            self.borders.get(from)
        } else if down {
            self.borders.get(to)
        } else {
            None
        };
        pairs.into_iter().flatten().map(move |pair| {
            if up {
                (pair.child, pair.parent)
            } else {
                (pair.parent, pair.child)
            }
        })
    }
}

pub(super) fn valid_region_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests;
