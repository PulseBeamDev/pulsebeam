use std::sync::OnceLock;
use std::time::SystemTime;
use tokio::time::Instant;

pub struct ClockAnchor {
    instant: Instant,
    system_time: SystemTime,
}

impl Default for ClockAnchor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockAnchor {
    pub fn new() -> Self {
        let instant = Instant::now();
        let system_time = SystemTime::now();
        Self {
            instant,
            system_time,
        }
    }

    pub fn wallclock(&self, now: Instant) -> SystemTime {
        self.system_time + now.duration_since(self.instant)
    }
}

static CLOCK_ANCHOR: OnceLock<ClockAnchor> = OnceLock::new();

pub fn clock_anchor() -> &'static ClockAnchor {
    CLOCK_ANCHOR.get_or_init(ClockAnchor::new)
}

pub fn capture_wallclock() -> SystemTime {
    clock_anchor().wallclock(Instant::now())
}

pub fn wallclock_at(now: Instant) -> SystemTime {
    clock_anchor().wallclock(now)
}
