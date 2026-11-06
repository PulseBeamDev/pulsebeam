use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, ensure};
use chitchat::Chitchat;
use tokio::time::Instant;

pub use pulsebeam_core::identity::NodeId;

/// Kept for 48 hours after the highest generation disappears from protocol state.
pub const HISTORY_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
pub type Metadata = BTreeMap<String, String>;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Incarnation {
    pub node_id: NodeId,
    pub generation_id: u64,
    pub gossip_advertise_addr: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Liveness {
    Known,
    Live,
    Suspected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    pub liveness: Liveness,
    pub superseded: bool,
    pub conflicting: bool,
    pub supported: bool,
    pub metadata: Metadata,
}

impl Member {
    pub fn is_usable(&self) -> bool {
        self.supported && self.liveness == Liveness::Live && !self.superseded && !self.conflicting
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    Joined(Incarnation),
    Removed(Incarnation),
    Liveness(Incarnation),
    Metadata(Incarnation),
    Status(Incarnation),
    Incarnation(NodeId),
}

/// A complete local observation. Changes describe only the preceding revision;
/// a slow subscriber must resynchronize from members and highest_generations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub revision: u64,
    pub members: BTreeMap<Incarnation, Member>,
    pub highest_generations: BTreeMap<NodeId, u64>,
    pub changes: Vec<Change>,
}

pub(crate) fn validate_metadata(metadata: &Metadata) -> Result<()> {
    ensure!(
        metadata.len() <= 16,
        "membership metadata permits at most 16 entries"
    );
    let mut size = 0usize;
    for (key, value) in metadata {
        ensure!(
            !key.is_empty()
                && key.len() <= 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid membership metadata key"
        );
        ensure!(
            value.len() <= 1024,
            "membership metadata value exceeds 1024 bytes"
        );
        size = size.saturating_add(key.len()).saturating_add(value.len());
    }
    ensure!(size <= 8192, "membership metadata exceeds 8192 bytes");
    Ok(())
}

pub(crate) fn usable_address(addr: SocketAddr) -> bool {
    addr.port() != 0
        && !addr.ip().is_unspecified()
        && !addr.ip().is_multicast()
        && match addr.ip() {
            std::net::IpAddr::V4(ip) => !ip.is_broadcast(),
            std::net::IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

fn supported_metadata(record: &chitchat::NodeState) -> Option<Metadata> {
    if record.get("pb.schema") != Some("1") {
        return None;
    }
    let metadata = record
        .iter_prefix("meta.")
        .take(17)
        .map(|(key, value)| {
            let key = key.strip_prefix("meta.")?;
            (key.len() <= 64 && value.value.len() <= 1024)
                .then(|| (key.to_owned(), value.value.clone()))
        })
        .collect::<Option<Metadata>>()?;
    validate_metadata(&metadata).ok()?;
    Some(metadata)
}

struct History {
    generation: u64,
    last_observed: Instant,
    first_address: SocketAddr,
    conflicting: bool,
}

pub(crate) struct View {
    pub snapshot: Snapshot,
    history: BTreeMap<NodeId, History>,
}

impl View {
    pub fn new() -> Self {
        Self {
            snapshot: Snapshot::default(),
            history: BTreeMap::new(),
        }
    }

    pub fn reconcile(&mut self, state: &Chitchat) -> Result<bool> {
        let live: BTreeSet<_> = state.live_nodes().collect();
        let suspected: BTreeSet<_> = state
            .dead_nodes()
            .chain(state.scheduled_for_deletion_nodes())
            .collect();
        let records = state
            .node_states()
            .iter()
            .filter_map(|(id, record)| {
                let incarnation = Incarnation {
                    node_id: id.node_id.parse().ok()?,
                    generation_id: id.generation_id,
                    gossip_advertise_addr: id.gossip_advertise_addr,
                };
                if incarnation.generation_id == 0 {
                    return None;
                }
                // Unusable endpoints still contribute generation/conflict evidence.
                let metadata = usable_address(incarnation.gossip_advertise_addr)
                    .then(|| supported_metadata(record))
                    .flatten();
                let liveness = if live.contains(id) {
                    Liveness::Live
                } else if suspected.contains(id) {
                    Liveness::Suspected
                } else {
                    Liveness::Known
                };
                Some((incarnation, liveness, metadata))
            })
            .collect();
        self.update(records, Instant::now())
    }

    fn update(
        &mut self,
        records: Vec<(Incarnation, Liveness, Option<Metadata>)>,
        now: Instant,
    ) -> Result<bool> {
        self.history.retain(|_, history| {
            now.saturating_duration_since(history.last_observed) <= HISTORY_RETENTION
        });
        for (id, _, _) in &records {
            let history = self.history.entry(id.node_id).or_insert(History {
                generation: id.generation_id,
                last_observed: now,
                first_address: id.gossip_advertise_addr,
                conflicting: false,
            });
            if id.generation_id > history.generation {
                history.generation = id.generation_id;
                history.first_address = id.gossip_advertise_addr;
                history.conflicting = false;
            }
            if id.generation_id == history.generation {
                history.last_observed = now;
                history.conflicting |= history.first_address != id.gossip_advertise_addr;
            }
        }
        let highest_generations: BTreeMap<_, _> = self
            .history
            .iter()
            .map(|(id, history)| (*id, history.generation))
            .collect();
        let mut addresses: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for (id, _, _) in &records {
            addresses
                .entry((id.node_id, id.generation_id))
                .or_default()
                .insert(id.gossip_advertise_addr);
        }
        let members = records
            .into_iter()
            .map(|(id, liveness, metadata)| {
                let supported = metadata.is_some();
                let metadata = metadata.unwrap_or_default();
                let superseded = highest_generations
                    .get(&id.node_id)
                    .is_some_and(|generation| *generation > id.generation_id);
                let conflicting = addresses
                    .get(&(id.node_id, id.generation_id))
                    .is_some_and(|addrs| addrs.len() > 1)
                    || self.history.get(&id.node_id).is_some_and(|history| {
                        history.generation == id.generation_id && history.conflicting
                    });
                (
                    id,
                    Member {
                        liveness,
                        superseded,
                        conflicting,
                        supported,
                        metadata,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        if self.snapshot.revision != 0
            && self.snapshot.members == members
            && self.snapshot.highest_generations == highest_generations
        {
            return Ok(false);
        }
        let mut changes = Vec::new();
        for (id, member) in &members {
            if let Some(old) = self.snapshot.members.get(id) {
                if old.liveness != member.liveness {
                    changes.push(Change::Liveness(id.clone()));
                }
                if old.metadata != member.metadata {
                    changes.push(Change::Metadata(id.clone()));
                }
                if old.superseded != member.superseded
                    || old.conflicting != member.conflicting
                    || old.supported != member.supported
                {
                    changes.push(Change::Status(id.clone()));
                }
            } else {
                changes.push(Change::Joined(id.clone()));
            }
        }
        for id in self
            .snapshot
            .members
            .keys()
            .filter(|id| !members.contains_key(*id))
        {
            changes.push(Change::Removed(id.clone()));
        }
        let nodes: BTreeSet<_> = self
            .snapshot
            .highest_generations
            .keys()
            .chain(highest_generations.keys())
            .copied()
            .collect();
        for id in nodes {
            if self.snapshot.highest_generations.get(&id) != highest_generations.get(&id) {
                changes.push(Change::Incarnation(id));
            }
        }
        let revision = self
            .snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("membership revision exhausted"))?;
        self.snapshot = Snapshot {
            revision,
            members,
            highest_generations,
            changes,
        };
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incarnation(generation_id: u64, port: u16) -> Incarnation {
        Incarnation {
            node_id: NodeId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]),
            generation_id,
            gossip_advertise_addr: ([127, 0, 0, 1], port).into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn high_water_survives_protocol_gc_and_expires_only_after_horizon() {
        let mut view = View::new();
        let old = incarnation(1, 7946);
        let new = incarnation(2, 7947);
        let now = Instant::now();
        view.update(
            vec![
                (old.clone(), Liveness::Live, Some(Metadata::new())),
                (new.clone(), Liveness::Suspected, Some(Metadata::new())),
            ],
            now,
        )
        .unwrap();
        assert!(view.snapshot.members[&old].superseded);
        assert!(!view.snapshot.members[&new].superseded);
        view.update(
            vec![(old.clone(), Liveness::Live, Some(Metadata::new()))],
            now + HISTORY_RETENTION,
        )
        .unwrap();
        assert!(view.snapshot.members[&old].superseded);
        assert_eq!(view.snapshot.highest_generations[&old.node_id], 2);
        view.update(
            vec![(old.clone(), Liveness::Live, Some(Metadata::new()))],
            now + HISTORY_RETENTION + Duration::from_secs(1),
        )
        .unwrap();
        // No forever fencing guarantee after history is forgotten.
        assert!(!view.snapshot.members[&old].superseded);
        assert_eq!(view.snapshot.highest_generations[&old.node_id], 1);
    }

    #[tokio::test(start_paused = true)]
    async fn conflicts_and_invalid_metadata_never_create_a_usable_endpoint() {
        let mut view = View::new();
        let first = incarnation(1, 7946);
        let second = incarnation(1, 7947);
        view.update(
            vec![
                (first.clone(), Liveness::Live, Some(Metadata::new())),
                (second, Liveness::Live, None),
            ],
            Instant::now(),
        )
        .unwrap();
        assert_eq!(view.snapshot.members.len(), 2);
        assert!(!view.snapshot.members[&first].is_usable());
        assert!(view.snapshot.members[&first].conflicting);
        let revision = view.snapshot.revision;
        view.update(Vec::new(), Instant::now()).unwrap();
        assert!(view.snapshot.revision > revision);
        assert!(view.snapshot.changes.contains(&Change::Removed(first)));
    }

    #[tokio::test(start_paused = true)]
    async fn unusable_peer_address_still_advances_high_water() {
        let old = incarnation(1, 7946);
        let invalid = incarnation(2, 0);
        let config = chitchat::ChitchatConfig {
            chitchat_id: chitchat::ChitchatId::new(
                invalid.node_id.to_string(),
                2,
                invalid.gossip_advertise_addr,
            ),
            cluster_id: "test".into(),
            gossip_interval: Duration::from_secs(1),
            listen_addr: invalid.gossip_advertise_addr,
            seed_nodes: Vec::new(),
            failure_detector_config: Default::default(),
            marked_for_deletion_grace_period: Duration::from_secs(900),
            catchup_callback: None,
            extra_liveness_predicate: None,
            protocol_version: chitchat::ProtocolVersion::V0,
        };
        let mut state = Chitchat::with_chitchat_id_and_seeds(
            config,
            tokio::sync::watch::channel(Default::default()).1,
            vec![("pb.schema".into(), "1".into())],
        );
        let old_id =
            chitchat::ChitchatId::new(old.node_id.to_string(), 1, old.gossip_advertise_addr);
        state.reset_node_state_if_update(
            &old_id,
            vec![(
                "pb.schema".into(),
                chitchat::VersionedValue {
                    value: "1".into(),
                    version: 1,
                    status: chitchat::DeletionStatus::Set,
                },
            )]
            .into_iter(),
            1,
            0,
        );
        let mut view = View::new();
        view.reconcile(&state).unwrap();
        assert_eq!(view.snapshot.highest_generations[&old.node_id], 2);
        assert!(view.snapshot.members[&old].superseded);
        assert!(!view.snapshot.members[&invalid].supported);
        assert!(!view.snapshot.members[&invalid].is_usable());
    }

    fn protocol_config(id: &Incarnation) -> chitchat::ChitchatConfig {
        chitchat::ChitchatConfig {
            chitchat_id: chitchat::ChitchatId::new(
                id.node_id.to_string(),
                id.generation_id,
                id.gossip_advertise_addr,
            ),
            cluster_id: "gc-proof".into(),
            gossip_interval: Duration::from_secs(1),
            listen_addr: id.gossip_advertise_addr,
            seed_nodes: Vec::new(),
            failure_detector_config: Default::default(),
            marked_for_deletion_grace_period: Duration::from_secs(1),
            catchup_callback: None,
            extra_liveness_predicate: None,
            protocol_version: chitchat::ProtocolVersion::V0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_protocol_records_are_not_usable() {
        let id = incarnation(1, 7946);
        let cases = [
            Vec::new(),
            vec![("pb.schema".into(), "99".into())],
            vec![
                ("pb.schema".into(), "1".into()),
                ("meta.bad key".into(), "x".into()),
            ],
            vec![
                ("pb.schema".into(), "1".into()),
                ("meta.large".into(), "x".repeat(1025)),
            ],
            std::iter::once(("pb.schema".into(), "1".into()))
                .chain((0..17).map(|n| (format!("meta.key{n}"), "x".into())))
                .collect(),
        ];
        for metadata in cases {
            let state = Chitchat::with_chitchat_id_and_seeds(
                protocol_config(&id),
                tokio::sync::watch::channel(Default::default()).1,
                metadata,
            );
            let mut view = View::new();
            view.reconcile(&state).unwrap();
            assert!(!view.snapshot.members[&id].supported);
            assert!(!view.snapshot.members[&id].is_usable());
        }
        let mut config = protocol_config(&id);
        config.chitchat_id.node_id = "not-a-node-id".into();
        let state = Chitchat::with_chitchat_id_and_seeds(
            config,
            tokio::sync::watch::channel(Default::default()).1,
            vec![("pb.schema".into(), "1".into())],
        );
        let mut view = View::new();
        view.reconcile(&state).unwrap();
        assert!(view.snapshot.members.is_empty() && view.snapshot.highest_generations.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn address_conflict_survives_record_removal_until_a_new_generation() {
        let first = incarnation(1, 7946);
        let second = incarnation(1, 7947);
        let next = incarnation(2, 7948);
        let mut view = View::new();
        let now = Instant::now();
        view.update(
            vec![
                (first.clone(), Liveness::Live, Some(Metadata::new())),
                (second, Liveness::Live, Some(Metadata::new())),
            ],
            now,
        )
        .unwrap();
        view.update(
            vec![(first.clone(), Liveness::Live, Some(Metadata::new()))],
            now + Duration::from_secs(1),
        )
        .unwrap();
        assert!(view.snapshot.members[&first].conflicting);
        assert!(!view.snapshot.members[&first].is_usable());
        view.update(
            vec![
                (first, Liveness::Live, Some(Metadata::new())),
                (next.clone(), Liveness::Live, Some(Metadata::new())),
            ],
            now + Duration::from_secs(2),
        )
        .unwrap();
        assert!(view.snapshot.members[&next].is_usable());
    }

    #[tokio::test(start_paused = true)]
    async fn real_protocol_gc_preserves_the_higher_generation_fence() {
        use chitchat::transport::ChannelTransport;
        let old = incarnation(1, 7946);
        let new = incarnation(2, 7947);
        let mut observer = incarnation(1, 7950);
        observer.node_id = NodeId::new();
        let transport = ChannelTransport::default();
        // Reach the upstream GC boundary quickly. Production still uses the
        // default 24-hour dead grace and 15-minute tombstone retention.
        let make_config = |id: &Incarnation| {
            let mut config = protocol_config(id);
            config.failure_detector_config.dead_node_grace_period = Duration::from_secs(2);
            if id != &observer {
                config.seed_nodes = vec![observer.gossip_advertise_addr.to_string()];
            }
            config
        };
        let schema = || vec![("pb.schema".to_owned(), "1".to_owned())];
        let observer_handle =
            chitchat::spawn_chitchat(make_config(&observer), schema(), &transport)
                .await
                .unwrap();
        let mut metadata = schema();
        metadata.push(("meta.build".into(), "old".into()));
        let old_handle = chitchat::spawn_chitchat(make_config(&old), metadata, &transport)
            .await
            .unwrap();
        let new_handle = chitchat::spawn_chitchat(make_config(&new), schema(), &transport)
            .await
            .unwrap();
        let mut view = View::new();
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                observer_handle
                    .with_chitchat(|state| view.reconcile(state))
                    .await
                    .unwrap();
                if view
                    .snapshot
                    .members
                    .get(&new)
                    .is_some_and(Member::is_usable)
                    && view.snapshot.members.get(&old).is_some_and(|member| {
                        member.liveness == Liveness::Live && member.metadata.contains_key("build")
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .unwrap();
        old_handle
            .with_chitchat(|state| state.self_node_state().delete("meta.build"))
            .await;
        new_handle.shutdown().await.unwrap();
        let new_id =
            chitchat::ChitchatId::new(new.node_id.to_string(), 2, new.gossip_advertise_addr);
        let old_id =
            chitchat::ChitchatId::new(old.node_id.to_string(), 1, old.gossip_advertise_addr);
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let collected = observer_handle
                    .with_chitchat(|state| {
                        view.reconcile(state).unwrap();
                        state.node_state(&new_id).is_none()
                            && state
                                .node_state(&old_id)
                                .is_some_and(|record| record.get_versioned("meta.build").is_none())
                    })
                    .await;
                if collected {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(view.snapshot.highest_generations[&old.node_id], 2);
        assert!(view.snapshot.members[&old].superseded);
        assert_eq!(view.snapshot.members[&old].liveness, Liveness::Live);
        assert!(!view.snapshot.members.contains_key(&new));
        assert!(!view.snapshot.members[&old].metadata.contains_key("build"));
        old_handle.shutdown().await.unwrap();
        observer_handle.shutdown().await.unwrap();
    }

    #[test]
    fn metadata_is_bounded_and_protocol_keys_are_separate() {
        let mut metadata = Metadata::new();
        metadata.insert("build".into(), "local".into());
        validate_metadata(&metadata).unwrap();
        metadata.insert("bad key".into(), "value".into());
        assert!(validate_metadata(&metadata).is_err());
        metadata.remove("bad key");
        metadata.insert("oversize".into(), "x".repeat(1025));
        assert!(validate_metadata(&metadata).is_err());
    }
}
