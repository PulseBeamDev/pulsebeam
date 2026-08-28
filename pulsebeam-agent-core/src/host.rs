use core::time::Duration;
use log::{LevelFilter, Metadata, Record};
use spin::Once;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    micros: u64,
}

impl Instant {
    pub const ZERO: Self = Self { micros: 0 };

    pub fn from_micros(micros: u64) -> Self {
        Self { micros }
    }

    pub fn elapsed_since(self, earlier: Self) -> Duration {
        Duration::from_micros(self.micros.saturating_sub(earlier.micros))
    }
}

pub struct Host {
    pub installed_at: Instant,
    pub now: fn() -> Instant,
    pub log: fn(&Record<'_>),
}

static HOST: Once<Host> = Once::new();
static LOGGER: HostLogger = HostLogger;

struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        (host().log)(record);
    }

    fn flush(&self) {}
}

pub fn install(host: Host, level: LevelFilter) {
    HOST.call_once(|| host);

    log::set_logger(&LOGGER).expect("logger already installed");
    log::set_max_level(level);
}

fn host() -> &'static Host {
    HOST.get().expect("host not installed")
}

pub fn now() -> Instant {
    (host().now)()
}

pub fn timestamp() -> Duration {
    now().elapsed_since(host().installed_at)
}
