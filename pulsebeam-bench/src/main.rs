//! Prints the operating point of one load shape, so a scenario can be sized
//! before it is frozen into a criterion benchmark id.
//!
//! ```text
//! cargo run --release -p pulsebeam-bench -- <shards> <rooms> <peers-per-room> [samples]
//! ```

use mimalloc::MiMalloc;
use pulsebeam_bench::{Scenario, SfuHarness};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn sample_bytes() -> u64 {
    std::env::var("PB_BENCH_SAMPLE_MIB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4)
        << 20
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parse = |i: usize, default: usize| -> usize {
        args.get(i).and_then(|a| a.parse().ok()).unwrap_or(default)
    };
    let scenario = Scenario::new("probe", parse(0, 1), parse(1, 4), parse(2, 4));
    let samples = parse(3, 8);

    println!(
        "probing {} shard(s), {} room(s), {} peers/room ({} peers total)",
        scenario.shards,
        scenario.rooms,
        scenario.peers_per_room,
        scenario.peers()
    );
    let harness = SfuHarness::start(scenario)?;

    let mut per_mib = Vec::with_capacity(samples);
    for i in 0..samples {
        let report = harness.measure(sample_bytes());
        let ms_per_mib =
            report.data_plane_cpu.as_secs_f64() * 1000.0 / (sample_bytes() >> 20) as f64;
        per_mib.push(ms_per_mib);
        println!(
            "  sample {i:>2}: {ms_per_mib:>7.2} ms/MiB   {:>5.1} Mbps   {:>4.0}% busy   {:>5.2}s wall",
            report.throughput_mbps(),
            report.utilization() * 100.0,
            report.wall.as_secs_f64(),
        );
    }

    let mean = per_mib.iter().sum::<f64>() / per_mib.len() as f64;
    let variance =
        per_mib.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / per_mib.len().max(2) as f64;
    println!(
        "mean {mean:.2} ms/MiB, cv {:.2}%",
        100.0 * variance.sqrt() / mean
    );

    // The node's shard threads outlive a cancelled node (see `SfuHarness`), so
    // leave the process rather than wait on threads that never join.
    drop(harness);
    std::process::exit(0);
}
