use std::time::{Duration, Instant};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GlobalMediaTime(u64);

impl GlobalMediaTime {
    pub const fn from_micros(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_micros(self) -> u64 {
        self.0
    }

    pub const fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub const fn from_be_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let micros = u64::try_from(duration.as_micros()).ok()?;
        self.0.checked_add(micros).map(Self)
    }

    pub fn checked_sub(self, duration: Duration) -> Option<Self> {
        let micros = u64::try_from(duration.as_micros()).ok()?;
        self.0.checked_sub(micros).map(Self)
    }

    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_micros)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimePoint {
    pub monotonic: Instant,
    pub global: GlobalMediaTime,
}

#[allow(
    dead_code,
    reason = "entropy is consumed by Connection::accept beginning in Plan 02"
)]
pub struct ConnectionEntropy([u8; 32]);

impl ConnectionEntropy {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[allow(
        dead_code,
        reason = "entropy is consumed by Connection::accept beginning in Plan 02"
    )]
    pub(crate) fn into_bytes(mut self) -> [u8; 32] {
        let bytes = self.0;
        self.0.fill(0);
        bytes
    }
}

#[derive(Default)]
#[allow(
    dead_code,
    reason = "the observer is exercised by Connection methods beginning in Plan 02"
)]
pub(crate) struct MonotonicObserver {
    last: Option<TimePoint>,
    regressions: u64,
    warning_pending: bool,
}

#[allow(
    dead_code,
    reason = "the observer is exercised by Connection methods beginning in Plan 02"
)]
impl MonotonicObserver {
    pub(crate) fn observe(&mut self, at: TimePoint) -> TimePoint {
        self.observe_with_debug_assertions::<{ cfg!(debug_assertions) }>(at)
    }

    fn observe_with_debug_assertions<const ASSERT: bool>(
        &mut self,
        mut at: TimePoint,
    ) -> TimePoint {
        if let Some(last) = self.last {
            let monotonic_regressed = at.monotonic < last.monotonic;
            let global_regressed = at.global < last.global;
            if monotonic_regressed || global_regressed {
                assert!(!ASSERT, "connection time regressed");
                at.monotonic = at.monotonic.max(last.monotonic);
                at.global = at.global.max(last.global);
                self.regressions = self.regressions.saturating_add(1);
                self.warning_pending = true;
            }
        }
        self.last = Some(at);
        at
    }

    pub(crate) const fn regressions(&self) -> u64 {
        self.regressions
    }

    pub(crate) fn take_warning(&mut self) -> bool {
        std::mem::take(&mut self.warning_pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "connection time regressed")]
    fn debug_observer_asserts_on_regression() {
        let start = Instant::now();
        let mut observer = MonotonicObserver::default();
        observer.observe_with_debug_assertions::<true>(TimePoint {
            monotonic: start + Duration::from_millis(1),
            global: GlobalMediaTime::from_micros(1),
        });
        observer.observe_with_debug_assertions::<true>(TimePoint {
            monotonic: start,
            global: GlobalMediaTime::from_micros(0),
        });
    }

    #[test]
    fn release_observer_clamps_and_counts_regression() {
        let start = Instant::now();
        let mut observer = MonotonicObserver::default();
        let accepted = TimePoint {
            monotonic: start + Duration::from_millis(2),
            global: GlobalMediaTime::from_micros(20),
        };
        observer.observe_with_debug_assertions::<false>(accepted);

        let clamped = observer.observe_with_debug_assertions::<false>(TimePoint {
            monotonic: start,
            global: GlobalMediaTime::from_micros(10),
        });

        assert_eq!(clamped.monotonic, accepted.monotonic);
        assert_eq!(clamped.global, accepted.global);
        assert_eq!(observer.regressions(), 1);
        assert!(observer.take_warning());
        assert!(!observer.take_warning());
    }
}
