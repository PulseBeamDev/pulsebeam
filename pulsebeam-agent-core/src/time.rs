use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct MonotonicTime(Duration);

impl MonotonicTime {
    pub const ZERO: Self = Self(Duration::ZERO);

    pub const fn from_nanos(nanos: u64) -> Self {
        Self(Duration::from_nanos(nanos))
    }

    pub const fn from_millis(millis: u64) -> Self {
        Self(Duration::from_millis(millis))
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn as_duration(self) -> Duration {
        self.0
    }

    pub const fn as_nanos(self) -> u128 {
        self.0.as_nanos()
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }

    pub fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration))
    }

    pub fn duration_since(self, earlier: Self) -> Duration {
        self.0.checked_sub(earlier.0).unwrap_or(Duration::ZERO)
    }
}

impl From<Duration> for MonotonicTime {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<MonotonicTime> for Duration {
    fn from(time: MonotonicTime) -> Self {
        time.0
    }
}

impl fmt::Display for MonotonicTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}ns", self.as_nanos())
    }
}
