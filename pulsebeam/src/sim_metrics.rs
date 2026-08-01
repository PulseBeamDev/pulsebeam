//! Test-only observation points for the simulator.
//!
//! The simulator runs the SFU in-process under turmoil, so a process-global sink is enough to get
//! internal state out to an assertion without threading a handle through every layer.
//!
//! This exists because the interesting congestion-control failures are not visible in received
//! byte counts. A bandwidth estimate can be poisoned - pulled far below what the link carries -
//! while throughput still looks acceptable, because the allocator simply picks a lower simulcast
//! layer and the viewer keeps receiving *something*. Asserting on bytes alone lets that regress
//! silently; asserting on the estimate catches it directly.
//!
//! Compiled only under the `sim` feature.

use std::sync::{Mutex, OnceLock};

/// Downstream bandwidth estimate observations, since the last [`reset`].
#[derive(Debug, Default, Clone)]
struct Samples {
    min_bwe_bps: Option<u64>,
    last_bwe_bps: Option<u64>,
    /// Number of allocation passes observed. Distinguishes "estimate stayed high" from
    /// "nothing was ever recorded", which would otherwise both satisfy a minimum.
    count: u64,
}

fn samples() -> &'static Mutex<Samples> {
    static SAMPLES: OnceLock<Mutex<Samples>> = OnceLock::new();
    SAMPLES.get_or_init(|| Mutex::new(Samples::default()))
}

/// Record one downstream allocation pass. Called from the allocator's reporting path.
pub fn record_downstream_bwe(bwe_bps: u64) {
    let mut s = samples().lock().expect("sim metrics poisoned");
    s.min_bwe_bps = Some(match s.min_bwe_bps {
        Some(m) => m.min(bwe_bps),
        None => bwe_bps,
    });
    s.last_bwe_bps = Some(bwe_bps);
    s.count += 1;
}

/// Clear observations. The harness calls this at the start of each timed step so assertions
/// describe the window just run, matching the byte-counter semantics.
pub fn reset() {
    *samples().lock().expect("sim metrics poisoned") = Samples::default();
}

/// Lowest downstream estimate seen on any participant since [`reset`], with the sample count.
///
/// Returns `None` when nothing was recorded, so a test can tell an untested path from a healthy
/// one rather than vacuously passing.
pub fn min_downstream_bwe_bps() -> Option<(u64, u64)> {
    let s = samples().lock().expect("sim metrics poisoned");
    s.min_bwe_bps.map(|m| (m, s.count))
}

/// Most recent downstream estimate seen on any participant since [`reset`].
pub fn last_downstream_bwe_bps() -> Option<u64> {
    samples().lock().expect("sim metrics poisoned").last_bwe_bps
}
