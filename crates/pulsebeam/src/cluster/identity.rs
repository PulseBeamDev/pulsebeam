//! Linux host-owned incarnation allocation. Never called from shard execution.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use pulsebeam_membership::NodeId;
use serde::{Deserialize, Serialize};

const STATE: &str = "membership-state.json";
const NEXT: &str = "membership-state.next";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    node_id: NodeId,
    generation: u64,
}

enum ExpectedIdentity {
    Exact(NodeId, NonZeroU64),
    Enrolled(NodeId),
}

pub(super) struct Identity {
    pub node_id: NodeId,
    pub generation: u64,
    lock: File,
}

impl Identity {
    pub(super) fn retain_lock(&self) -> Result<File> {
        self.lock
            .try_clone()
            .context("retaining membership identity lock")
    }

    pub fn allocate(directory: &Path) -> Result<Self> {
        Self::allocate_with_sync(directory, None, File::sync_all)
    }

    pub(super) fn allocate_provisioned(
        directory: &Path,
        expected: (NodeId, NonZeroU64),
    ) -> Result<Self> {
        Self::allocate_with_sync(
            directory,
            Some(ExpectedIdentity::Exact(expected.0, expected.1)),
            File::sync_all,
        )
    }

    pub(super) fn allocate_enrolled(directory: &Path, node: NodeId) -> Result<Self> {
        Self::allocate_with_sync(
            directory,
            Some(ExpectedIdentity::Enrolled(node)),
            File::sync_all,
        )
    }

    pub(super) fn lock_enrolled(directory: &Path, node: NodeId) -> Result<Self> {
        let lock = Self::lock(directory)?;
        let state = Self::read_state(directory)?
            .context("regional enrollment requires an existing membership identity")?;
        ensure!(
            state.node_id == node,
            "regional enrollment does not match the stored membership identity"
        );
        Ok(Self {
            node_id: state.node_id,
            generation: state.generation,
            lock,
        })
    }

    fn lock(directory: &Path) -> Result<File> {
        create_directory(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("membership.lock"))
            .context("opening stable membership lock")?;
        lock.try_lock()
            .context("membership data directory is already in use or cannot be locked")?;
        Ok(lock)
    }

    fn read_state(directory: &Path) -> Result<Option<State>> {
        match File::open(directory.join(STATE)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .context("reading membership identity")?;
                ensure!(
                    bytes.len() <= 4096,
                    "membership identity state is too large"
                );
                let state: State = serde_json::from_slice(&bytes).context(
                    "invalid membership identity state; do not reset or restore an old identity",
                )?;
                ensure!(
                    state.version == 1,
                    "unsupported membership identity version {}",
                    state.version
                );
                ensure!(
                    state.generation > 0,
                    "membership identity generation must be positive"
                );
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("opening membership identity state"),
        }
    }

    fn allocate_with_sync(
        directory: &Path,
        expected: Option<ExpectedIdentity>,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<Self> {
        let lock = Self::lock(directory)?;
        let mut state = match Self::read_state(directory)? {
            Some(state) => state,
            None => {
                ensure!(
                    expected.is_none(),
                    "provisioned ownership requires existing membership identity"
                );
                State {
                    version: 1,
                    node_id: NodeId::new(),
                    generation: 0,
                }
            }
        };
        state.generation = state
            .generation
            .checked_add(1)
            .context("membership generation exhausted")?;
        match expected {
            Some(ExpectedIdentity::Exact(node, generation)) => ensure!(
                node == state.node_id && generation.get() == state.generation,
                "provisioned ownership identity does not match the node's next membership incarnation"
            ),
            Some(ExpectedIdentity::Enrolled(node)) => ensure!(
                node == state.node_id,
                "regional enrollment does not match the stored membership identity"
            ),
            None => {}
        }
        replace_json(directory, STATE, NEXT, &state, sync)?;
        Ok(Self {
            node_id: state.node_id,
            generation: state.generation,
            lock,
        })
    }
}

pub(super) fn replace_json(
    directory: &Path,
    state: &str,
    next: &str,
    value: &impl Serialize,
    sync: impl Fn(&File) -> std::io::Result<()>,
) -> Result<()> {
    let next = directory.join(next);
    match fs::remove_file(&next) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing interrupted state replacement"),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&next)
        .context("creating state replacement")?;
    serde_json::to_writer(&mut file, value).context("encoding durable state")?;
    file.write_all(b"\n")?;
    sync(&file).context("synchronizing state replacement")?;
    fs::rename(&next, directory.join(state)).context("committing state replacement")?;
    sync(&File::open(directory)?).context("synchronizing state directory")?;
    Ok(())
}

fn create_directory(directory: &Path) -> Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    let parent = directory
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_directory(parent)?;
    fs::create_dir(directory)
        .with_context(|| format!("creating data directory {}", directory.display()))?;
    File::open(parent)?
        .sync_all()
        .context("synchronizing new data directory")?;
    Ok(())
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn restarts_keep_identity_and_increase_generation_after_interrupted_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::allocate(dir.path()).unwrap();
        let id = first.node_id;
        assert_eq!(first.generation, 1);
        assert!(Identity::allocate(dir.path()).is_err());
        drop(first);
        fs::write(dir.path().join(NEXT), b"interrupted").unwrap();
        let second = Identity::allocate(dir.path()).unwrap();
        assert_eq!(second.node_id, id);
        assert_eq!(second.generation, 2);
        drop(second);
        let third = Identity::allocate(dir.path()).unwrap();
        assert_eq!(third.generation, 3);
    }

    #[test]
    fn provisioned_identity_must_match_the_next_committed_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::allocate(dir.path()).unwrap();
        let id = first.node_id;
        drop(first);

        let next = NonZeroU64::new(2).unwrap();
        assert!(Identity::allocate_provisioned(dir.path(), (id, NonZeroU64::MIN)).is_err());
        assert!(Identity::allocate_provisioned(dir.path(), (NodeId::new(), next)).is_err());
        let second = Identity::allocate_provisioned(dir.path(), (id, next)).unwrap();
        assert_eq!((second.node_id, second.generation), (id, 2));
        drop(second);
        assert!(Identity::allocate_provisioned(dir.path(), (id, next)).is_err());

        let empty = tempfile::tempdir().unwrap();
        assert!(Identity::allocate_provisioned(empty.path(), (id, NonZeroU64::MIN)).is_err());
        assert!(!empty.path().join(STATE).exists());
    }

    #[test]
    fn enrolled_identity_accepts_later_boots_but_cannot_replace_missing_or_other_identity() {
        let dir = tempfile::tempdir().unwrap();
        let enrolled = Identity::allocate(dir.path()).unwrap().node_id;
        assert!(Identity::allocate_enrolled(dir.path(), NodeId::new()).is_err());
        for expected in [2, 3, 4] {
            let boot = Identity::allocate_enrolled(dir.path(), enrolled).unwrap();
            assert_eq!((boot.node_id, boot.generation), (enrolled, expected));
            assert!(Identity::allocate_enrolled(dir.path(), enrolled).is_err());
        }
        fs::remove_file(dir.path().join(STATE)).unwrap();
        assert!(Identity::allocate_enrolled(dir.path(), enrolled).is_err());
        assert!(!dir.path().join(STATE).exists());
    }

    #[test]
    #[ignore = "subprocess fixture, invoked by process_lock_survives_replacement_and_releases_on_crash"]
    fn process_child() {
        let directory = std::env::var_os("PB_MEMBERSHIP_TEST_DIR").unwrap();
        let directory = Path::new(&directory);
        let action = std::env::var("PB_MEMBERSHIP_TEST_ACTION").unwrap();
        if action == "blocked" {
            assert!(Identity::allocate(directory).is_err());
            return;
        }
        let identity = Identity::allocate(directory).unwrap();
        if action == "hold" {
            let mut out = std::io::stdout();
            out.write_all(b"IDENTITY_LOCKED\n").unwrap();
            out.flush().unwrap();
            let mut byte = [0];
            let _ = std::io::stdin().read_exact(&mut byte);
        } else {
            assert_eq!(
                identity.node_id.to_string(),
                std::env::var("PB_MEMBERSHIP_TEST_ID").unwrap()
            );
            assert_eq!(identity.generation, 3);
        }
    }

    #[test]
    fn process_lock_survives_replacement_and_releases_on_crash() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::allocate(dir.path()).unwrap();
        let id = first.node_id;
        drop(first);
        let command = |action: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "cluster::identity::tests::process_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PB_MEMBERSHIP_TEST_DIR", dir.path())
                .env("PB_MEMBERSHIP_TEST_ACTION", action)
                .env("PB_MEMBERSHIP_TEST_ID", id.to_string());
            command
        };
        let mut holder = command("hold")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(holder.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "identity holder exited before taking its lock"
            );
            if line.contains("IDENTITY_LOCKED") {
                break;
            }
        }
        assert!(Identity::allocate(dir.path()).is_err());
        assert!(command("blocked").status().unwrap().success());
        holder.kill().unwrap();
        assert!(!holder.wait().unwrap().success());
        assert!(command("check").status().unwrap().success());
        let next = Identity::allocate(dir.path()).unwrap();
        assert_eq!(next.node_id, id);
        assert_eq!(next.generation, 4);
    }

    #[test]
    fn corrupt_unsupported_and_exhausted_states_fail_closed() {
        for bytes in ["{", "{}", "{\"version\":99}"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(STATE), bytes).unwrap();
            assert!(Identity::allocate(dir.path()).is_err());
            assert_eq!(fs::read_to_string(dir.path().join(STATE)).unwrap(), bytes);
        }
        let dir = tempfile::tempdir().unwrap();
        for (version, generation) in [(99, 1), (1, 0), (1, u64::MAX)] {
            let state = State {
                version,
                node_id: NodeId::new(),
                generation,
            };
            fs::write(dir.path().join(STATE), serde_json::to_vec(&state).unwrap()).unwrap();
            assert!(Identity::allocate(dir.path()).is_err());
        }
    }

    #[test]
    fn file_and_directory_sync_failures_cannot_return_a_join_identity() {
        for failure in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let first = Identity::allocate(dir.path()).unwrap();
            let id = first.node_id;
            drop(first);
            let calls = Cell::new(0);
            assert!(
                Identity::allocate_with_sync(dir.path(), None, |file| {
                    calls.set(calls.get() + 1);
                    if calls.get() == failure {
                        Err(std::io::Error::other("injected sync failure"))
                    } else {
                        file.sync_all()
                    }
                })
                .is_err()
            );
            let next = Identity::allocate(dir.path()).unwrap();
            assert_eq!(next.node_id, id);
            assert_eq!(next.generation, if failure == 1 { 2 } else { 3 });
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(STATE)).unwrap();
        assert!(Identity::allocate(dir.path()).is_err());
        fs::remove_dir(dir.path().join(STATE)).unwrap();
        fs::create_dir(dir.path().join(NEXT)).unwrap();
        assert!(Identity::allocate(dir.path()).is_err());
    }
}
