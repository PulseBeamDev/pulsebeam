use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Tracks the busy and idle time of a shard for load balancing.
///
/// This uses raw cumulative counters in microseconds to allow the controller
/// to derive the load over any window it chooses.
#[derive(Debug, Default)]
pub struct ShardMetrics {
    busy_time_us: AtomicU64,
    idle_time_us: AtomicU64,
}

impl ShardMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records time spent doing actual work (processing packets, timers, etc.)
    pub fn record_busy(&self, duration: Duration) {
        self.busy_time_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
    }

    /// Records time spent waiting for events (parked in select! or poll)
    pub fn record_idle(&self, duration: Duration) {
        self.idle_time_us
            .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
    }

    /// Returns the raw cumulative busy and idle times in microseconds.
    pub fn read_raw(&self) -> (u64, u64) {
        (
            self.busy_time_us.load(Ordering::Relaxed),
            self.idle_time_us.load(Ordering::Relaxed),
        )
    }

    /// Returns a snapshot of the current counters.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let (busy, idle) = self.read_raw();
        MetricsSnapshot {
            busy_us: busy,
            idle_us: idle,
        }
    }
}

/// A frozen view of the occupancy counters at a point in time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub busy_us: u64,
    pub idle_us: u64,
}

impl MetricsSnapshot {
    /// Calculates the load (0.0 to 1.0) by comparing this snapshot to a previous one.
    pub fn delta_load(&self, previous: &Self) -> f64 {
        let busy_delta = self.busy_us.wrapping_sub(previous.busy_us) as f64;
        let idle_delta = self.idle_us.wrapping_sub(previous.idle_us) as f64;
        let total_delta = busy_delta + idle_delta;

        if total_delta <= 0.0 {
            0.0
        } else {
            (busy_delta / total_delta).min(1.0).max(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta_load() {
        let s1 = MetricsSnapshot {
            busy_us: 100,
            idle_us: 100,
        };
        let s2 = MetricsSnapshot {
            busy_us: 200,
            idle_us: 200,
        };
        // Delta: 100 busy, 100 idle. Total 200. Load 0.5.
        assert_eq!(s2.delta_load(&s1), 0.5);

        let s3 = MetricsSnapshot {
            busy_us: 300,
            idle_us: 200,
        };
        // Delta: 100 busy, 0 idle. Total 100. Load 1.0.
        assert_eq!(s3.delta_load(&s2), 1.0);

        let s4 = MetricsSnapshot {
            busy_us: 300,
            idle_us: 300,
        };
        // Delta: 0 busy, 100 idle. Total 100. Load 0.0.
        assert_eq!(s4.delta_load(&s3), 0.0);
    }

    #[test]
    fn test_wrapping() {
        let s1 = MetricsSnapshot {
            busy_us: u64::MAX,
            idle_us: 0,
        };
        let s2 = MetricsSnapshot {
            busy_us: 99, // Wrapped
            idle_us: 100,
        };
        // busy_delta = 99 - u64::MAX = 100 (wrapping)
        // idle_delta = 100 - 0 = 100
        // Total 200. Load 0.5.
        assert_eq!(s2.delta_load(&s1), 0.5);
    }
}
