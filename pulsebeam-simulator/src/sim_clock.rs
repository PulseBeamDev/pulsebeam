//! Make the process clock the *simulated* clock, so runs are reproducible.
//!
//! Derived from madsim's equivalent:
//! <https://github.com/madsim-rs/madsim/blob/main/madsim/src/sim/time/system_time.rs>
//!
//! # Why this is necessary
//!
//! turmoil controls time for anything that goes through `tokio::time`, but nothing else. Any
//! dependency reading `std::time::Instant` or `SystemTime` — tracing, allocators, hashers, HTTP
//! clients — reads the *real* clock, so its behaviour depends on how loaded the machine is. That
//! leaks straight into results: the same BWE plan, unchanged, produced worst-drawdown figures of
//! 44%, 68% and 81% across three runs, which makes any threshold meaningless and any regression
//! unattributable.
//!
//! Overriding `clock_gettime` for the whole process closes that hole in one place, including
//! inside dependencies we do not control — which is the part a code change on our side cannot
//! reach. With this in place the same three runs agree exactly.
//!
//! # Why not transmute `tokio::time::Instant`
//!
//! The upstream reference reads `CLOCK_MONOTONIC` by transmuting a `tokio::time::Instant` into a
//! `timespec`, which relies on the undocumented layout of `std::time::Instant`. That would break
//! silently — with plausible-looking wrong timestamps — if std ever changed its representation.
//! `turmoil::sim_elapsed()` is already monotonic simulated time, needs no layout assumption, and
//! is what we actually want here.
//!
//! # Scope
//!
//! Test-binary only; this crate is never shipped. The override is inert until
//! [`SimClocksGuard::init`] is called and stops at the guard's drop, so process start-up and
//! tear-down still see the real clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Number of live guards, not a flag.
///
/// The override is process-wide but plans may run concurrently. With a boolean, the first plan to
/// finish would switch the clock back to real time underneath every plan still running — turning
/// a determinism fix into a source of nondeterminism that only appears under parallelism.
static SIM_CLOCK_GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Scopes the clock override.
///
/// Tear-down is the reason this is a guard rather than a flag set once: calling
/// `tokio::runtime::Handle::try_current()` while the process is exiting can abort with "global
/// allocator may not use TLS", so the override has to be switched off while the runtime is still
/// alive.
pub struct SimClocksGuard(());

impl SimClocksGuard {
    pub fn init() -> Self {
        SIM_CLOCK_GUARDS.fetch_add(1, Ordering::Release);
        Self(())
    }
}

impl Drop for SimClocksGuard {
    fn drop(&mut self) {
        SIM_CLOCK_GUARDS.fetch_sub(1, Ordering::Release);
    }
}

/// Wall-clock epoch for the simulation: 2023-11-14T22:13:20Z, chosen only for being a plausible
/// recent date.
///
/// `CLOCK_REALTIME` cannot start at zero. Callers legitimately treat it as a real date and look
/// *backwards* from it — str0m computes `SystemTime::now() - 1h` and panics if the result is
/// before the Unix epoch. Monotonic time has no such constraint and starts at zero.
const SIM_EPOCH_SECS: u64 = 1_700_000_000;

fn sim_elapsed() -> Duration {
    turmoil::sim_elapsed().unwrap_or(Duration::ZERO)
}

fn timespec(secs: u64, nanos: u32) -> libc::timespec {
    libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: nanos as libc::c_long,
    }
}

fn sim_monotonic() -> libc::timespec {
    let elapsed = sim_elapsed();
    timespec(elapsed.as_secs(), elapsed.subsec_nanos())
}

fn sim_realtime() -> libc::timespec {
    let elapsed = sim_elapsed();
    timespec(SIM_EPOCH_SECS + elapsed.as_secs(), elapsed.subsec_nanos())
}

/// # Safety
///
/// Overrides the libc symbol for the whole process. `tp` must be a valid, writable
/// `*mut libc::timespec`, per the `clock_gettime(3)` contract.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn clock_gettime(
    clockid: libc::clockid_t,
    tp: *mut libc::timespec,
) -> libc::c_int {
    // The runtime check matters as much as the flag: a thread with no turmoil context has no
    // simulated time to report, and `sim_elapsed` would just yield zero for it.
    if SIM_CLOCK_GUARDS.load(Ordering::Acquire) > 0 && tokio::runtime::Handle::try_current().is_ok()
    {
        let timespec = match clockid {
            libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE => Some(sim_realtime()),
            libc::CLOCK_MONOTONIC
            | libc::CLOCK_MONOTONIC_RAW
            | libc::CLOCK_MONOTONIC_COARSE
            | libc::CLOCK_BOOTTIME => Some(sim_monotonic()),
            // Thread/process CPU clocks measure work done, not wall time. Feeding them simulated
            // time would be a category error, and nothing in a plan's results depends on them.
            _ => None,
        };
        if let Some(timespec) = timespec {
            unsafe { tp.write(timespec) };
            return 0;
        }
    }

    // Outside a simulation, report the epoch rather than chaining to the real clock.
    //
    // Reaching the real symbol from here means `dlsym(RTLD_NEXT, ..)`, and that allocates — from
    // inside the function the allocator itself calls to get the time. A fixed value keeps this
    // path allocation-free and, being constant, is still deterministic. The callers that land
    // here are allocator/tracing bookkeeping, which only ever compare timestamps to each other.
    // Realtime still reports a plausible date here, for the same reason as above: a caller that
    // subtracts from "now" must not fall off the epoch.
    let fallback = match clockid {
        libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE => timespec(SIM_EPOCH_SECS, 0),
        _ => timespec(0, 0),
    };
    unsafe { tp.write(fallback) };
    0
}
