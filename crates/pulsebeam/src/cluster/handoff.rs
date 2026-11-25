//! Host-owned handoff checkpoints. Use from a blocking host task, never from
//! the controller or shard loop. A successful write is the publication barrier
//! for a transfer whose authorization and quiescence were checked upstream.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
    num::NonZeroU64,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use pulsebeam_core::identity::{ConnectionId, NodeId, ParticipantId, ProjectId, RoomId};
use serde::{Deserialize, Serialize};

const STATE: &str = "publisher-handoffs.json";
const NEXT: &str = "publisher-handoffs.next";
const LOCK: &str = "publisher-handoffs.lock";
const MAX_RECORDS: usize = 4096;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const VERSION: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    pub(crate) project: ProjectId,
    pub(crate) room: RoomId,
    pub(crate) participant: ParticipantId,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    pub(crate) domain: [u8; 16],
    pub(crate) node: NodeId,
    pub(crate) generation: NonZeroU64,
    pub(crate) connection: ConnectionId,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Handoff {
    pub(crate) scope: Scope,
    pub(crate) sequence: NonZeroU64,
    pub(crate) predecessor: Owner,
    pub(crate) successor: Owner,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingDelivery {
    pub(crate) scope: Scope,
    pub(crate) lineage: ConnectionId,
    pub(crate) position: u64,
    pub(crate) offer: Box<[u8]>,
    pub(crate) peers: Box<[(NodeId, NonZeroU64)]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    version: u8,
    records: Vec<Handoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingDelivery>,
}

#[derive(Default)]
pub(crate) struct Fences(BTreeMap<Scope, Handoff>);

impl Fences {
    pub(crate) fn successor(&self, scope: Scope) -> Option<Owner> {
        self.0.get(&scope).map(|handoff| handoff.successor)
    }

    pub(crate) fn allows(&self, scope: Scope, owner: Owner) -> bool {
        self.0
            .get(&scope)
            .is_none_or(|handoff| handoff.successor == owner)
    }
}

pub(crate) struct Store {
    directory: PathBuf,
    records: BTreeMap<Scope, Handoff>,
    pending: Option<PendingDelivery>,
    poisoned: bool,
    _lock: File,
}

impl Store {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join(LOCK))
            .context("opening publisher handoff lock")?;
        lock.try_lock()
            .context("locking publisher handoff checkpoint")?;

        let mut records = BTreeMap::new();
        let mut pending = None;
        match File::open(directory.join(STATE)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                (&file)
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .context("reading publisher handoffs")?;
                ensure!(
                    bytes.len() as u64 <= MAX_BYTES,
                    "publisher handoff checkpoint is oversized"
                );
                let image: Image =
                    serde_json::from_slice(&bytes).context("decoding publisher handoffs")?;
                ensure!(
                    image.version == VERSION,
                    "unsupported publisher handoff checkpoint version"
                );
                ensure!(
                    image.records.len() <= MAX_RECORDS,
                    "too many publisher handoff records"
                );
                for handoff in image.records {
                    ensure!(
                        handoff.predecessor != handoff.successor,
                        "invalid publisher handoff"
                    );
                    ensure!(
                        records.insert(handoff.scope, handoff).is_none(),
                        "duplicate publisher handoff scope"
                    );
                }
                if let Some(delivery) = &image.pending {
                    ensure!(
                        records.contains_key(&delivery.scope),
                        "pending handoff has no committed record"
                    );
                }
                pending = image.pending;
                // A previous process may have renamed the file but failed to
                // sync the directory. Reestablish durability before exposing it.
                file.sync_all()
                    .context("synchronizing recovered publisher handoffs")?;
                File::open(directory)?
                    .sync_all()
                    .context("synchronizing recovered handoff directory")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("opening publisher handoffs"),
        }
        Ok(Self {
            directory: directory.to_owned(),
            records,
            pending,
            poisoned: false,
            _lock: lock,
        })
    }

    #[cfg(test)]
    pub(crate) fn load_fences(directory: &Path) -> Result<Fences> {
        Ok(Self::open(directory)?.fences())
    }

    pub(crate) fn fences(&self) -> Fences {
        Fences(self.records.clone())
    }

    pub(crate) fn current(&self, scope: Scope) -> Result<Option<Handoff>> {
        ensure!(
            !self.poisoned,
            "publisher handoff checkpoint needs reopening"
        );
        Ok(self.records.get(&scope).copied())
    }

    pub(crate) fn commit(&mut self, handoff: Handoff) -> Result<()> {
        self.commit_with_sync(handoff, File::sync_all)
    }

    pub(crate) fn pending(&self) -> Option<&PendingDelivery> {
        self.pending.as_ref()
    }

    pub(crate) fn commit_outgoing(
        &mut self,
        handoff: Handoff,
        pending: PendingDelivery,
    ) -> Result<()> {
        ensure!(
            self.pending.is_none() && pending.scope == handoff.scope,
            "another publisher handoff is pending"
        );
        ensure!(
            self.needs_commit(handoff)?,
            "publisher handoff already committed"
        );
        let mut records = self.records.clone();
        records.insert(handoff.scope, handoff);
        self.persist(records, Some(pending), File::sync_all)
    }

    pub(crate) fn confirm_delivery(&mut self, scope: Scope) -> Result<()> {
        ensure!(
            !self.poisoned
                && self
                    .pending
                    .as_ref()
                    .is_some_and(|entry| entry.scope == scope),
            "no matching publisher handoff pending"
        );
        self.persist(self.records.clone(), None, File::sync_all)
    }

    pub(crate) fn needs_commit(&self, handoff: Handoff) -> Result<bool> {
        ensure!(
            handoff.predecessor != handoff.successor,
            "publisher handoff cannot name the same epoch"
        );
        match self.current(handoff.scope)? {
            Some(old) => {
                if old == handoff {
                    return Ok(false);
                }
                ensure!(
                    self.pending
                        .as_ref()
                        .is_none_or(|entry| entry.scope != handoff.scope),
                    "publisher handoff delivery is unconfirmed"
                );
                ensure!(
                    old.sequence.get().checked_add(1) == Some(handoff.sequence.get()),
                    "publisher handoff sequence is not consecutive"
                );
                ensure!(
                    old.successor == handoff.predecessor,
                    "publisher handoff does not continue the committed owner"
                );
            }
            None => {
                ensure!(
                    handoff.sequence == NonZeroU64::MIN,
                    "publisher handoff must start at sequence one"
                );
                ensure!(
                    self.records.len() < MAX_RECORDS,
                    "publisher handoff checkpoint is full"
                );
            }
        }
        Ok(true)
    }

    fn commit_with_sync(
        &mut self,
        handoff: Handoff,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<()> {
        if !self.needs_commit(handoff)? {
            return Ok(());
        }
        let mut records = self.records.clone();
        records.insert(handoff.scope, handoff);
        self.persist(records, self.pending.clone(), sync)
    }

    fn persist(
        &mut self,
        records: BTreeMap<Scope, Handoff>,
        pending: Option<PendingDelivery>,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<()> {
        let image = Image {
            version: VERSION,
            records: records.values().copied().collect(),
            pending,
        };
        self.poisoned = true;
        super::identity::replace_json(&self.directory, STATE, NEXT, &image, sync)?;
        self.records = records;
        self.pending = image.pending;
        self.poisoned = false;
        Ok(())
    }
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests;
