//! Clock and timer primitives. Instants and timers use the active runtime clock,
//! including its paused clock in tests.

pub use std::time::Duration;
pub use tokio::time::error;
pub use tokio::time::{
    Instant, Interval, MissedTickBehavior, Sleep, Timeout, advance, interval, interval_at, pause,
    resume, sleep, sleep_until, timeout, timeout_at,
};
