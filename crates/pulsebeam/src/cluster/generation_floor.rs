//! Durable receiver-side floors for authenticated regional incarnations.

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use pulsebeam_link::PeerFingerprint;
use pulsebeam_membership::NodeId;
use serde::{Deserialize, Serialize};

use super::{
    boot_announcement::VerifiedBoot,
    identity::{Identity, replace_json},
};

const STATE: &str = "regional-generations.json";
const NEXT: &str = "regional-generations.next";
const VERSION: u32 = 1;
const MAX_NODES: usize = 256;
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    local: NodeId,
    epoch: [u8; 16],
    floors: Vec<(NodeId, Floor)>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Floor {
    generation: NonZeroU64,
    source: Source,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Direct { media_leaf: Option<[u8; 32]> },
    Discovery,
}

/// One admission owner serializes observations. The retained identity lock also
/// fences an in-flight write while a node is shutting down.
pub(super) struct Store {
    directory: PathBuf,
    local: NodeId,
    epoch: [u8; 16],
    floors: BTreeMap<NodeId, Floor>,
    uncertain: bool,
    _identity_lock: File,
}

impl Store {
    pub(super) fn enroll(
        directory: &Path,
        identity: &Identity,
        epoch: [u8; 16],
        direct: impl IntoIterator<Item = (NodeId, NonZeroU64)>,
        discovery: impl IntoIterator<Item = (NodeId, NonZeroU64)>,
    ) -> Result<Self> {
        ensure!(
            !directory.join(STATE).try_exists()?,
            "regional generation floors are already enrolled"
        );
        let store = Self::from_floors(
            directory,
            identity,
            epoch,
            direct
                .into_iter()
                .map(|(node, generation)| {
                    (
                        node,
                        Floor {
                            generation,
                            source: Source::Direct { media_leaf: None },
                        },
                    )
                })
                .chain(discovery.into_iter().map(|(node, generation)| {
                    (
                        node,
                        Floor {
                            generation,
                            source: Source::Discovery,
                        },
                    )
                })),
        )?;
        store.persist(&store.floors, File::sync_all)?;
        Ok(store)
    }

    pub(super) fn open(directory: &Path, identity: &Identity, epoch: [u8; 16]) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(directory.join(STATE))
            .context("opening enrolled regional generation floors")?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading regional generation floors")?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "regional generation floors exceed their storage limit"
        );
        let document: Document = serde_json::from_slice(&bytes)
            .context("invalid regional generation floor state; recover enrollment explicitly")?;
        ensure!(
            document.version == VERSION
                && document.local == identity.node_id
                && document.epoch == epoch,
            "regional generation floors belong to another identity, epoch, or storage version"
        );
        Self::from_floors(directory, identity, epoch, document.floors)
    }

    fn from_floors(
        directory: &Path,
        identity: &Identity,
        epoch: [u8; 16],
        floors: impl IntoIterator<Item = (NodeId, Floor)>,
    ) -> Result<Self> {
        let mut registered = BTreeMap::new();
        for (peer, floor) in floors {
            ensure!(
                registered.len() < MAX_NODES,
                "too many regional generation floors"
            );
            ensure!(
                peer != identity.node_id && registered.insert(peer, floor).is_none(),
                "regional generation floors must name distinct remote nodes"
            );
        }
        ensure!(
            !registered.is_empty(),
            "regional generation floors are empty"
        );
        Ok(Self {
            directory: directory.to_path_buf(),
            local: identity.node_id,
            epoch,
            floors: registered,
            uncertain: false,
            _identity_lock: identity.retain_lock()?,
        })
    }

    pub(super) fn check_slots(
        &self,
        direct: impl IntoIterator<Item = (NodeId, NonZeroU64)>,
        discovery: impl IntoIterator<Item = (NodeId, NonZeroU64)>,
    ) -> Result<()> {
        let slots: BTreeMap<_, _> = direct
            .into_iter()
            .map(|(node, minimum)| (node, (minimum, true)))
            .chain(
                discovery
                    .into_iter()
                    .map(|(node, minimum)| (node, (minimum, false))),
            )
            .collect();
        ensure!(
            slots.len() == self.floors.len()
                && slots.into_iter().all(|(node, (minimum, direct))| {
                    self.floors.get(&node).is_some_and(|floor| {
                        floor.generation >= minimum
                            && matches!(floor.source, Source::Direct { .. }) == direct
                    })
                }),
            "regional generation store does not cover the configured enrollment slots"
        );
        Ok(())
    }

    pub(super) fn admit(
        &mut self,
        peer: NodeId,
        generation: NonZeroU64,
        media_leaf: PeerFingerprint,
    ) -> Result<bool> {
        self.admit_with_sync(peer, generation, media_leaf, File::sync_all)
    }

    fn admit_with_sync(
        &mut self,
        peer: NodeId,
        generation: NonZeroU64,
        media_leaf: PeerFingerprint,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<bool> {
        ensure!(
            !self.uncertain,
            "regional generation persistence is uncertain"
        );
        let Some(&Floor {
            source: Source::Direct { media_leaf: prior },
            generation: floor,
        }) = self.floors.get(&peer)
        else {
            return Ok(false);
        };
        let pin = media_leaf.to_bytes();
        if generation < floor {
            return Ok(false);
        }
        if generation == floor
            && let Some(prior) = prior
        {
            return Ok(pin == prior);
        }
        if prior == Some(pin) {
            return Ok(false);
        }
        self.advance(
            peer,
            Floor {
                generation,
                source: Source::Direct {
                    media_leaf: Some(pin),
                },
            },
            sync,
        )
    }

    pub(super) fn discover(&mut self, proof: &VerifiedBoot) -> Result<bool> {
        ensure!(
            !self.uncertain,
            "regional generation persistence is uncertain"
        );
        let origin = proof.origin;
        let Some(&Floor {
            source: Source::Discovery,
            generation: floor,
        }) = self.floors.get(&origin.node)
        else {
            return Ok(false);
        };
        if origin.generation <= floor {
            return Ok(origin.generation == floor);
        }
        self.advance(
            origin.node,
            Floor {
                generation: origin.generation,
                source: Source::Discovery,
            },
            File::sync_all,
        )
    }

    fn advance(
        &mut self,
        peer: NodeId,
        floor: Floor,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<bool> {
        let mut next = self.floors.clone();
        next.insert(peer, floor);
        self.uncertain = true;
        self.persist(&next, sync)?;
        self.floors = next;
        self.uncertain = false;
        Ok(true)
    }

    fn persist(
        &self,
        floors: &BTreeMap<NodeId, Floor>,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<()> {
        let document = Document {
            version: VERSION,
            local: self.local,
            epoch: self.epoch,
            floors: floors.iter().map(|(&node, &floor)| (node, floor)).collect(),
        };
        replace_json(&self.directory, STATE, NEXT, &document, sync)
            .context("persisting regional generation floors")
    }
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests {
    use std::{cell::Cell, fs, io};

    use super::*;

    const EPOCH: [u8; 16] = [7; 16];

    fn generation(value: u64) -> NonZeroU64 {
        assert_ne!(value, 0, "test generation must be nonzero");
        NonZeroU64::new(value).unwrap_or(NonZeroU64::MIN)
    }

    fn pin(byte: u8) -> PeerFingerprint {
        PeerFingerprint::from_bytes([byte; 32])
    }

    #[test]
    fn enrolled_floors_survive_reopen_and_allow_equal_generation_reconnect() {
        let root = tempfile::tempdir().unwrap();
        let identity = Identity::allocate(root.path()).unwrap();
        let peer = NodeId::new();
        let mut store =
            Store::enroll(root.path(), &identity, EPOCH, [(peer, generation(2))], []).unwrap();
        assert!(!store.admit(peer, generation(1), pin(1)).unwrap());
        assert!(!store.admit(NodeId::new(), generation(3), pin(3)).unwrap());
        assert!(store.admit(peer, generation(2), pin(2)).unwrap());
        assert!(
            store
                .admit_with_sync(peer, generation(2), pin(2), |_| panic!(
                    "equal reconnect rewrote the floor"
                ))
                .unwrap()
        );
        assert!(!store.admit(peer, generation(9), pin(2)).unwrap());
        assert!(store.admit(peer, generation(9), pin(9)).unwrap());
        drop(store);
        let mut store = Store::open(root.path(), &identity, EPOCH).unwrap();
        assert!(!store.admit(peer, generation(8), pin(8)).unwrap());
        assert!(!store.admit(peer, generation(9), pin(8)).unwrap());
        assert!(store.admit(peer, generation(9), pin(9)).unwrap());
        assert!(Store::enroll(root.path(), &identity, EPOCH, [(peer, generation(1))], []).is_err());
    }

    #[test]
    fn absent_corrupt_and_wrong_epoch_state_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let identity = Identity::allocate(root.path()).unwrap();
        let peer = NodeId::new();
        let one = NonZeroU64::MIN;
        assert!(Store::open(root.path(), &identity, EPOCH).is_err());
        assert!(
            Store::enroll(
                root.path(),
                &identity,
                EPOCH,
                [(peer, one), (peer, one)],
                []
            )
            .is_err()
        );
        Store::enroll(root.path(), &identity, EPOCH, [(peer, one)], []).unwrap();
        assert!(Store::open(root.path(), &identity, [8; 16]).is_err());
        fs::write(root.path().join(STATE), b"{").unwrap();
        assert!(Store::open(root.path(), &identity, EPOCH).is_err());
        assert!(Store::enroll(root.path(), &identity, EPOCH, [(peer, one)], []).is_err());
    }

    #[test]
    fn discovered_floor_is_durable_and_cannot_admit_a_media_pin() {
        use crate::cluster::{
            boot_announcement::{Announcement, Scope},
            mesh::Member,
        };
        use pulsebeam_core::auth::ApiSigningKey;

        let root = tempfile::tempdir().unwrap();
        let identity = Identity::allocate(root.path()).unwrap();
        let direct = NodeId::new();
        let routed = NodeId::new();
        let floor = generation(2);
        let scope = Scope {
            epoch: EPOCH,
            roster: [5; 32],
        };
        let key = ApiSigningKey::from_seed([6; 32]);
        let proof = |node, value| {
            let origin = Member {
                node,
                generation: generation(value),
            };
            let frame = Announcement::encode(scope, origin, 1_000, &key);
            Announcement::decode(&frame)
                .unwrap()
                .verify(scope, key.public_key(), 1_000)
                .unwrap()
        };
        assert!(
            Store::enroll(
                root.path(),
                &identity,
                EPOCH,
                [(direct, floor)],
                [(direct, floor)]
            )
            .is_err()
        );
        let mut store = Store::enroll(
            root.path(),
            &identity,
            EPOCH,
            [(direct, floor)],
            [(routed, floor)],
        )
        .unwrap();
        assert!(!store.discover(&proof(direct, 5)).unwrap());
        assert!(store.admit(direct, floor, pin(1)).unwrap());
        assert!(store.discover(&proof(routed, 5)).unwrap());
        assert!(!store.admit(routed, generation(6), pin(2)).unwrap());
        drop(store);

        let mut store = Store::open(root.path(), &identity, EPOCH).unwrap();
        assert!(
            store
                .check_slots([(direct, floor)], [(routed, floor)])
                .is_ok()
        );
        assert!(
            store
                .check_slots([(routed, floor)], [(direct, floor)])
                .is_err()
        );
        assert!(!store.discover(&proof(routed, 4)).unwrap());
        assert!(store.discover(&proof(routed, 5)).unwrap());
        assert!(store.admit(direct, floor, pin(1)).unwrap());
        assert!(!store.admit(direct, floor, pin(2)).unwrap());
    }

    #[test]
    fn directory_sync_failure_cannot_publish_a_boot_or_reaccept_the_older_boot() {
        let root = tempfile::tempdir().unwrap();
        let identity = Identity::allocate(root.path()).unwrap();
        let peer = NodeId::new();
        let mut store =
            Store::enroll(root.path(), &identity, EPOCH, [(peer, generation(1))], []).unwrap();
        let calls = Cell::new(0);
        let attempted = store.admit_with_sync(peer, generation(2), pin(2), |file| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                Err(io::Error::other("directory sync failed"))
            } else {
                file.sync_all()
            }
        });
        assert!(attempted.is_err());
        assert_eq!(calls.get(), 2);
        assert!(store.admit(peer, generation(1), pin(1)).is_err());
        assert!(store.admit(peer, generation(2), pin(2)).is_err());
    }
}
