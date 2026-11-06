use std::{fmt, io, str::FromStr};

/// A restart-time process execution profile, not a live scheduler conversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    Adaptive,
    WorkStealing,
    ThreadPerCore,
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Adaptive => "adaptive",
            Self::WorkStealing => "work-stealing",
            Self::ThreadPerCore => "thread-per-core",
        })
    }
}

impl FromStr for Profile {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "adaptive" => Ok(Self::Adaptive),
            "work-stealing" => Ok(Self::WorkStealing),
            "thread-per-core" => Ok(Self::ThreadPerCore),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime profile must be adaptive, work-stealing or thread-per-core",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionConfig {
    pub profile: Profile,
    pub workers: usize,
    pub shards_per_worker: usize,
    pub(crate) current_runtime: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            profile: Profile::Adaptive,
            workers: 1,
            shards_per_worker: 1,
            current_runtime: false,
        }
    }
}

impl ExecutionConfig {
    pub fn new(profile: Profile, workers: usize, shards_per_worker: usize) -> Self {
        Self {
            profile,
            workers,
            shards_per_worker,
            current_runtime: false,
        }
    }

    /// The caller owns this executor. Synthetic sockets never leave its thread.
    pub fn on_current_runtime(workers: usize) -> Self {
        Self {
            workers,
            current_runtime: true,
            ..Self::default()
        }
    }

    pub fn is_current_runtime(self) -> bool {
        self.current_runtime
    }

    pub fn shard_count(self) -> io::Result<usize> {
        if self.workers == 0 || self.shards_per_worker == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workers and shards per worker must be nonzero",
            ));
        }
        if self.profile == Profile::ThreadPerCore && self.shards_per_worker != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread-per-core requires exactly one shard per worker",
            ));
        }
        self.workers
            .checked_mul(self.shards_per_worker)
            .filter(|count| *count <= tokio::sync::Semaphore::MAX_PERMITS)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "configured shard count exceeds runtime capacity",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_and_topology_are_explicit() {
        let default = ExecutionConfig::default();
        assert_eq!(default.profile, Profile::Adaptive);
        assert_eq!(default.shard_count().ok(), Some(1));
        for profile in [
            Profile::Adaptive,
            Profile::WorkStealing,
            Profile::ThreadPerCore,
        ] {
            assert_eq!(profile.to_string().parse::<Profile>().ok(), Some(profile));
        }
        let mut config = ExecutionConfig {
            workers: 3,
            shards_per_worker: 16,
            ..default
        };
        assert_eq!(config.shard_count().ok(), Some(48));
        config.profile = Profile::ThreadPerCore;
        assert!(config.shard_count().is_err());
        config.shards_per_worker = 1;
        assert_eq!(config.shard_count().ok(), Some(3));
        config.workers = 0;
        assert!(config.shard_count().is_err());
        config.workers = usize::MAX;
        config.profile = Profile::Adaptive;
        config.shards_per_worker = 2;
        assert!(config.shard_count().is_err());
        assert!(ExecutionConfig::on_current_runtime(2).is_current_runtime());
    }
}
