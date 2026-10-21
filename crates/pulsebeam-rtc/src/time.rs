use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct ConnectionEntropy([u8; 32]);

impl ConnectionEntropy {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn into_bytes(mut self) -> [u8; 32] {
        let bytes = self.0;
        self.0.fill(0);
        bytes
    }
}

/// Fixed protocol wall-clock anchor. Subsequent progress uses monotonic time only.
pub(crate) struct ProtocolClock {
    instant: Instant,
    ntp_nanos: i128,
}

impl ProtocolClock {
    pub(crate) fn new(instant: Instant, wallclock: SystemTime) -> Self {
        let unix_nanos = match wallclock.duration_since(UNIX_EPOCH) {
            Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
            Err(error) => i128::try_from(error.duration().as_nanos())
                .unwrap_or(i128::MAX)
                .saturating_neg(),
        };
        Self {
            instant,
            ntp_nanos: unix_nanos.saturating_add(2_208_988_800_000_000_000),
        }
    }

    pub(crate) fn ntp(&self, at: Instant) -> (u32, u32) {
        let delta = if at >= self.instant {
            i128::try_from(at.duration_since(self.instant).as_nanos()).unwrap_or(i128::MAX)
        } else {
            i128::try_from(self.instant.duration_since(at).as_nanos())
                .unwrap_or(i128::MAX)
                .saturating_neg()
        };
        let nanos = self.ntp_nanos.saturating_add(delta);
        let seconds = nanos.div_euclid(1_000_000_000).rem_euclid(1_i128 << 32);
        let fraction = (nanos.rem_euclid(1_000_000_000) << 32) / 1_000_000_000;
        (
            u32::try_from(seconds).unwrap_or_default(),
            u32::try_from(fraction).unwrap_or_default(),
        )
    }
}

pub(crate) struct MonotonicObserver {
    last: Instant,
    regressions: u64,
    warning_pending: bool,
}

impl MonotonicObserver {
    pub(crate) const fn starting_at(at: Instant) -> Self {
        Self {
            last: at,
            regressions: 0,
            warning_pending: false,
        }
    }

    pub(crate) fn observe(&mut self, at: Instant) -> Instant {
        if at < self.last {
            debug_assert!(false, "connection time regressed");
            self.regressions = self.regressions.saturating_add(1);
            self.warning_pending = true;
        }
        self.last = at.max(self.last);
        self.last
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
    use std::time::Duration;

    #[test]
    fn fixed_anchor_preserves_fraction_and_ntp_era_wrap() {
        let start = Instant::now();
        let clock = ProtocolClock::new(
            start,
            UNIX_EPOCH + Duration::new(2_085_978_495, 500_000_000),
        );
        assert_eq!(clock.ntp(start), (u32::MAX, 1 << 31));
        assert_eq!(clock.ntp(start + Duration::from_secs(1)), (0, 1 << 31));
        assert_eq!(
            clock.ntp(start - Duration::from_millis(250)),
            (u32::MAX, 1 << 30)
        );
    }
}
