use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bitrate(u64);

impl Bitrate {
    pub const ZERO: Self = Self(0);

    pub const fn bps(value: u64) -> Self {
        Self(value)
    }

    pub const fn kbps(value: u64) -> Self {
        Self(value.saturating_mul(1_000))
    }

    pub const fn mbps(value: u64) -> Self {
        Self(value.saturating_mul(1_000_000))
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn as_f64(self) -> f64 {
        self.0 as f64
    }

    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

impl From<u64> for Bitrate {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for Bitrate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocationInput {
    pub estimate: Bitrate,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocationOutput {
    pub desired: Bitrate,
    pub allocated: Bitrate,
}
