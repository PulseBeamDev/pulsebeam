use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoPreset {
    Camera,
    Screen,
}

impl VideoPreset {
    pub const fn base_bitrate_bps(self) -> u64 {
        match self {
            Self::Camera => 1_250_000,
            Self::Screen => 2_500_000,
        }
    }

    pub const fn content_hint(self) -> &'static str {
        match self {
            Self::Camera => "motion",
            Self::Screen => "text",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayoutPreset {
    Adaptive,
    Interactive,
    Balanced,
    Resilient,
}

impl PlayoutPreset {
    pub const fn bounds(self) -> Option<(u32, u32)> {
        match self {
            Self::Adaptive => None,
            Self::Interactive => Some((0, 0)),
            Self::Balanced => Some((40, 120)),
            Self::Resilient => Some((100, 300)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LatencyLock {
    bounds: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatencyLockError {
    InvalidBounds {
        min_ms: u32,
        max_ms: u32,
    },
    AlreadyLocked {
        current: (u32, u32),
        requested: (u32, u32),
    },
}

impl fmt::Display for LatencyLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBounds { min_ms, max_ms } => {
                write!(
                    formatter,
                    "minimum latency {min_ms} exceeds maximum {max_ms}"
                )
            }
            Self::AlreadyLocked { current, requested } => {
                write!(
                    formatter,
                    "latency is locked at {current:?}, cannot change to {requested:?}"
                )
            }
        }
    }
}

impl std::error::Error for LatencyLockError {}

impl LatencyLock {
    pub const fn adaptive() -> Self {
        Self { bounds: None }
    }

    pub fn set(&mut self, min_ms: u32, max_ms: u32) -> Result<(), LatencyLockError> {
        if min_ms > max_ms {
            return Err(LatencyLockError::InvalidBounds { min_ms, max_ms });
        }
        let requested = (min_ms, max_ms);
        if let Some(current) = self.bounds {
            if current != requested {
                return Err(LatencyLockError::AlreadyLocked { current, requested });
            }
            return Ok(());
        }
        self.bounds = Some(requested);
        Ok(())
    }

    pub const fn bounds(self) -> Option<(u32, u32)> {
        self.bounds
    }

    pub const fn is_locked(self) -> bool {
        self.bounds.is_some()
    }

    pub fn apply(&mut self, preset: PlayoutPreset) -> Result<(), LatencyLockError> {
        match preset.bounds() {
            Some((min_ms, max_ms)) => self.set(min_ms, max_ms),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_lock_is_one_way_but_idempotent() {
        let mut lock = LatencyLock::default();
        lock.apply(PlayoutPreset::Interactive).unwrap();
        assert!(lock.apply(PlayoutPreset::Interactive).is_ok());
        assert_eq!(
            lock.apply(PlayoutPreset::Balanced),
            Err(LatencyLockError::AlreadyLocked {
                current: (0, 0),
                requested: (40, 120)
            })
        );
    }
}
