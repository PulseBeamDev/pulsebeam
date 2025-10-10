use criterion::{Criterion, criterion_group, criterion_main};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};
use tokio::runtime::Runtime;
use tokio::task;

// Make sure this path points to your updated spmc channel implementation.
use pulsebeam_runtime::sync::spmc::{RecvError, channel};

/// Measures broadcast throughput, packet loss, and p99.9 latency under heavy fan-out.
fn bench_broadcast_fanout(c: &mut Criterion) {
    // Set up a multi-threaded Tokio runtime for the benchmark.
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("spmc_broadcast_high_fanout");
    // Increase the measurement time for more stable results in a noisy environment.
    group.measurement_time(Duration::from_secs(15));
    // A smaller sample size is fine with a longer measurement time.
    group.sample_size(10);

    // --- Benchmark Definition ---
    group.bench_function("6000_subs_2000_pkts", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let mut total_duration = Duration::ZERO;
            for _ in 0..iters {
                let start_time = Instant::now();
                run_broadcast_test().await;
                total_duration += start_time.elapsed();
            }
            total_duration
        });
    });

    group.finish();
}

/// A single run of the broadcast test scenario.
async fn run_broadcast_test() {
    // 1. --- Setup ---
    // The packet will now contain its sequence and the time it was sent.
    let channel_cap = 256;
    let (tx, rx) = channel::<(usize, Instant)>(256);
    let num_subscribers = 3000;
    let num_packets_to_send = 2_000;

    // 2. --- Spawn Subscribers ---
    // Each subscriber task will return a vector of all end-to-end latencies it measured.
    let mut subscribers = Vec::with_capacity(num_subscribers);
    for _ in 0..num_subscribers {
        let mut r = rx.clone();
        let handle = task::spawn(async move {
            let mut latencies = Vec::with_capacity(num_packets_to_send);

            loop {
                match r.recv().await {
                    // Successfully received a packet with its send timestamp.
                    Ok(Some((_packet_id, send_time))) => {
                        // Calculate true end-to-end latency.
                        latencies.push(send_time.elapsed());
                    }
                    // The receiver lagged. This is expected and is the primary source of packet loss.
                    Err(RecvError::Lagged(_)) => {
                        continue;
                    }
                    // The sender was dropped, and the channel is empty. Terminate.
                    Ok(None) => {
                        break;
                    }
                }
            }
            latencies
        });
        subscribers.push(handle);
    }

    // Give a brief moment for all subscribers to spawn and be ready.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 3. --- Publisher sends packets ---
    let send_start = Instant::now();
    let yield_interval =
        channel_cap / std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    for i in 0..num_packets_to_send {
        tx.send((i, Instant::now()));
        // A smaller delay to increase pressure.
        if i % yield_interval == 0 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    let total_send_duration = send_start.elapsed();

    // 4. --- Signal Termination ---
    // Dropping the sender is the reliable way to signal subscribers to finish.
    drop(tx);

    // 5. --- Collect and Aggregate Results ---
    let mut all_latencies = Vec::with_capacity(num_subscribers * num_packets_to_send);
    for sub_handle in subscribers {
        let subscriber_latencies = sub_handle.await.unwrap();
        all_latencies.extend(subscriber_latencies);
    }

    // 6. --- Compute and Print Metrics ---
    let total_packets_delivered = all_latencies.len();
    let total_possible_deliveries = num_packets_to_send * num_subscribers;
    let lost_packets = total_possible_deliveries - total_packets_delivered;
    let loss_percentage = if total_possible_deliveries > 0 {
        lost_packets as f64 * 100.0 / total_possible_deliveries as f64
    } else {
        0.0
    };

    let throughput = total_packets_delivered as f64 / total_send_duration.as_secs_f64();

    // Calculate p99.9 latency
    let p99_9_latency = if !all_latencies.is_empty() {
        // Sort all collected latency values to find the percentile.
        // Unstable sort is faster and sufficient here.
        all_latencies.sort_unstable();
        let p99_9_idx = (all_latencies.len() as f64 * 0.999).floor() as usize;
        // Clamp the index to prevent out-of-bounds access.
        let final_idx = p99_9_idx.min(all_latencies.len() - 1);
        all_latencies[final_idx]
    } else {
        Duration::ZERO
    };

    println!(
        "Send Time: {:.2?}, Throughput: {:.0} pkt/sec, Packet Loss: {:.2}%, p99.9 Latency: {:.2?}",
        total_send_duration, throughput, loss_percentage, p99_9_latency
    );
}

criterion_group!(benches, bench_broadcast_fanout);
criterion_main!(benches);
