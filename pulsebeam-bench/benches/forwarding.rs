//! Data-plane cost of forwarding media, measured against a live node.
//!
//! Run a baseline, change something, compare:
//!
//! ```text
//! git stash && cargo bench -p pulsebeam-bench -- --save-baseline before
//! git stash pop && cargo bench -p pulsebeam-bench -- --baseline before
//! ```
//!
//! criterion reports the change with a confidence interval and a p-value, so
//! "3% faster" is a claim about the distribution rather than about one run.
//!
//! The reported time is shard-thread CPU per mebibyte the clients received.
//! Lower is better; it is unaffected by how fast the load generator happens to
//! run on the day.

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use mimalloc::MiMalloc;
use pulsebeam_bench::{LoadReport, Scenario, SfuHarness};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// One criterion iteration delivers this much media. Small enough that
/// criterion can scale iterations sensibly, large enough to swamp the
/// microsecond-scale jitter of a single tick.
const BYTES_PER_ITER: u64 = 1 << 20;

/// A sample taken while the node was pinned is measuring the queue, not the
/// code. Scenarios are sized to sit well under this.
const MAX_UTILIZATION: f64 = 0.85;

/// Scenarios are fixed rather than configurable: criterion stores baselines
/// under the benchmark id, so a scenario whose shape changes silently
/// invalidates its own history.
const SCENARIOS: &[Scenario] = &[
    // One shard, one room: the narrowest signal on per-packet cost.
    Scenario::new("1shard/1room/4peers", 1, 1, 4),
    // One shard, several rooms: routing and demux work grows, tick coalescing
    // gets a chance to pay off.
    Scenario::new("1shard/4rooms/4peers", 1, 4, 4),
    // Two shards: adds the cross-shard fanout path.
    Scenario::new("2shards/4rooms/4peers", 2, 4, 4),
];

fn forwarding(c: &mut Criterion) {
    let mut group = c.benchmark_group("forwarding");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(15))
        .throughput(criterion::Throughput::Bytes(BYTES_PER_ITER));

    for scenario in SCENARIOS {
        let harness = match SfuHarness::start(*scenario) {
            Ok(harness) => harness,
            Err(err) => {
                eprintln!("skipping {}: {err:#}", scenario.name);
                continue;
            }
        };
        report_conditions(scenario.name, &harness);

        group.bench_function(scenario.name, |b| {
            b.iter_custom(|iters| {
                let report = harness.measure(BYTES_PER_ITER * iters);
                assert!(
                    report.utilization() < MAX_UTILIZATION,
                    "{}: data plane was {:.0}% utilized; the sample measures saturation, \
                     not efficiency",
                    scenario.name,
                    report.utilization() * 100.0,
                );
                // The delivered count overshoots the target by whatever arrived
                // in the last poll interval; charge only for the bytes asked for.
                let charged = report.data_plane_cpu.as_secs_f64()
                    * (BYTES_PER_ITER * iters) as f64
                    / report.delivered_bytes.max(1) as f64;
                Duration::from_secs_f64(charged)
            })
        });
    }

    group.finish();
}

/// Prints the operating point each scenario settled at, so a surprising
/// benchmark number can be traced back to the load that produced it.
fn report_conditions(name: &str, harness: &SfuHarness) {
    let report: LoadReport = harness.measure(4 << 20);
    eprintln!(
        "{name}: {:.1} Mbps delivered, {:.0}% of {} shard thread(s) busy",
        report.throughput_mbps(),
        report.utilization() * 100.0,
        report.shards,
    );
}

criterion_group!(benches, forwarding);
criterion_main!(benches);
