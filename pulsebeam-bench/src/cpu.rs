use std::fs;
use std::time::Duration;

/// Thread-name prefix that [`pulsebeam::node::NodeBuilder`] gives its shard
/// workers. Everything the SFU does on the data plane runs on these.
const SHARD_THREAD_PREFIX: &str = "pb-w-";

/// The CPU clock of the SFU's data-plane threads.
///
/// Wall time is the wrong yardstick for a load harness: the load generator
/// shares the machine, so a sample's elapsed time says as much about the
/// clients as about the SFU. `schedstat`'s first field is the nanoseconds a
/// task has spent on-CPU, so preemption costs the reading nothing and the
/// resolution is ~1000x finer than the `utime`/`stime` jiffies in `stat`.
pub struct DataPlaneClock {
    tids: Vec<u32>,
}

impl DataPlaneClock {
    /// Every shard thread alive right now. A harness takes this before
    /// starting a node so it can tell that node's threads from the ones an
    /// earlier scenario left behind.
    pub fn shard_tids() -> Vec<u32> {
        let Ok(entries) = fs::read_dir("/proc/self/task") else {
            return Vec::new();
        };
        let mut tids: Vec<u32> = entries
            .flatten()
            .filter(|entry| {
                fs::read_to_string(entry.path().join("comm"))
                    .is_ok_and(|comm| comm.trim_end().starts_with(SHARD_THREAD_PREFIX))
            })
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        tids.sort_unstable();
        tids
    }

    /// Binds to the shard threads that appeared since `before`. Threads
    /// spawned later are not counted, so attach once the node is up.
    pub fn attach_since(before: &[u32]) -> anyhow::Result<Self> {
        let tids: Vec<u32> = Self::shard_tids()
            .into_iter()
            .filter(|tid| !before.contains(tid))
            .collect();
        anyhow::ensure!(
            !tids.is_empty(),
            "no new `{SHARD_THREAD_PREFIX}*` threads found; is the node running?"
        );
        Ok(Self { tids })
    }

    pub fn thread_count(&self) -> usize {
        self.tids.len()
    }

    /// Total CPU consumed by the shard threads since process start.
    pub fn read(&self) -> Duration {
        let mut nanos = 0u64;
        for tid in &self.tids {
            let Ok(schedstat) = fs::read_to_string(format!("/proc/self/task/{tid}/schedstat"))
            else {
                continue;
            };
            let on_cpu_ns = schedstat
                .split_whitespace()
                .next()
                .and_then(|field| field.parse::<u64>().ok())
                .unwrap_or(0);
            nanos += on_cpu_ns;
        }
        Duration::from_nanos(nanos)
    }
}

/// Probes whether per-thread CPU accounting is readable, so a harness can fail
/// with an explanation instead of silently reporting zero.
pub fn schedstat_available() -> bool {
    fs::read_to_string("/proc/self/schedstat")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|f| f.parse::<u64>().ok()))
        .is_some_and(|ns| ns > 0)
}
