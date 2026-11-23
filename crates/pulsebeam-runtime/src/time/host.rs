use super::{Duration, Instant};
use rustix::time::{ClockId, DynamicClockId, Timespec, clock_gettime, clock_gettime_dynamic};

/// Linux host reads bracketed by the same monotonic clock used by the node.
/// These are measurements, not proof that the host is configured for unsmeared UTC.
pub struct HostClockRead {
    pub before: Instant,
    pub after: Instant,
    pub unix_nanos: i64,
    pub tai_nanos: Option<i64>,
    pub kernel_max_error: Option<Duration>,
}

pub fn read_host_clocks() -> Option<HostClockRead> {
    let before = Instant::now();
    let error_before = kernel_max_error();
    let unix_nanos = nanos(clock_gettime(ClockId::Realtime))?;
    let tai_nanos = clock_gettime_dynamic(DynamicClockId::Tai)
        .ok()
        .and_then(nanos);
    let error_after = kernel_max_error();
    let after = Instant::now();
    let kernel_max_error = error_before.zip(error_after).map(|(a, b)| a.max(b));
    Some(HostClockRead {
        before,
        after,
        unix_nanos,
        tai_nanos,
        kernel_max_error,
    })
}

fn nanos(time: Timespec) -> Option<i64> {
    let nanos = i64::try_from(time.tv_nsec).ok()?;
    if !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    i64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos)
}

#[allow(
    unsafe_code,
    reason = "read-only Linux timex query using a zero-modes C buffer"
)]
fn kernel_max_error() -> Option<Duration> {
    // SAFETY: timex consists of integer fields and padding; zero is valid. With
    // modes zero adjtimex only fills the writable buffer and does not set the clock.
    let (state, timex) = unsafe {
        let mut timex: libc::timex = std::mem::zeroed();
        (libc::adjtimex(&mut timex), timex)
    };
    if state < 0
        || state == libc::TIME_ERROR
        || timex.status & (libc::STA_UNSYNC | libc::STA_CLOCKERR) != 0
    {
        return None;
    }
    Some(Duration::from_micros(u64::try_from(timex.maxerror).ok()?))
}
