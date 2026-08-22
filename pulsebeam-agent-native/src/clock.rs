use std::time::Duration;

use pulsebeam_agent_core::MonotonicTime;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct ClockAnchor {
    instant: Instant,
    monotonic: MonotonicTime,
}

impl ClockAnchor {
    pub fn new() -> Self {
        Self {
            instant: Instant::now(),
            monotonic: MonotonicTime::ZERO,
        }
    }

    pub fn now(&self) -> MonotonicTime {
        self.monotonic.saturating_add(self.instant.elapsed())
    }

    pub fn at(&self, now: MonotonicTime) -> Instant {
        let delta = now.duration_since(self.monotonic);
        self.instant.checked_add(delta).unwrap_or(self.instant)
    }
}

impl Default for ClockAnchor {
    fn default() -> Self {
        Self::new()
    }
}

pub fn duration_until(now: MonotonicTime, deadline: Option<MonotonicTime>) -> Option<Duration> {
    deadline.map(|deadline| deadline.duration_since(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_anchor_never_moves_backwards() {
        let anchor = ClockAnchor::default();
        assert!(anchor.now() >= MonotonicTime::ZERO);
        assert_eq!(
            duration_until(
                MonotonicTime::from_secs(2),
                Some(MonotonicTime::from_secs(1))
            ),
            Some(Duration::ZERO)
        );
    }
}
