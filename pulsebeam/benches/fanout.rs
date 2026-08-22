#![allow(clippy::arithmetic_side_effects, clippy::print_stdout)]

#[cfg(feature = "bench")]
use pulsebeam::shard::bench::{self, Percentiles};

#[cfg(feature = "bench")]
fn main() {
    let samples = argument("--samples", 100_000);
    let churn_every = argument("--churn-every", 64);
    let report = bench::run(samples, churn_every);

    println!("forwarding samples: {}", report.samples);
    println!("churn interval: {} packets", report.churn_every);
    print_percentiles("steady", report.steady);
    print_percentiles("churn", report.churn);
}

#[cfg(not(feature = "bench"))]
fn main() {}

#[cfg(feature = "bench")]
fn argument(name: &str, default: usize) -> usize {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default);
        }
    }
    default
}

#[cfg(feature = "bench")]
fn print_percentiles(label: &str, values: Percentiles) {
    println!(
        "{label}: p50={} p99={} p99.9={} p99.99={} max={}",
        bench::format_duration(values.p50_ns).as_nanos(),
        bench::format_duration(values.p99_ns).as_nanos(),
        bench::format_duration(values.p999_ns).as_nanos(),
        bench::format_duration(values.p9999_ns).as_nanos(),
        bench::format_duration(values.max_ns).as_nanos(),
    );
}
