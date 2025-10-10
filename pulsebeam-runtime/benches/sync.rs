use criterion::{Criterion, criterion_group, criterion_main};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::task;

// Make sure this path points to your updated spmc channel implementation.
use pulsebeam_runtime::sync::spmc::{RecvError, channel};

/// Measures broadcast throughput and average per-packet delivery latency for a large number of subscribers.
fn bench_broadcast_fanout(c: &mut Criterion) {
    // Set up a multi-threaded Tokio runtime for the benchmark.
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("spmc_broadcast");
    // This increases the sample size for more stable results.
    group.sample_size(10);

    // --- Benchmark Definition ---
    group.bench_function("6000_subscribers_1000_packets", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let mut total_duration = Duration::ZERO;
            // The custom loop allows us to measure total time accurately.
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

async fn run_broadcast_test() {
    // 1. --- Setup ---
    let capacity = 256;
    let (tx, rx) = channel::<usize>(capacity);
    let num_subscribers = 6000;
    let num_packets_to_send = 1_000;

    // 2. --- Spawn Subscribers ---
    // Each subscriber task will return the number of packets it successfully
    // received and the sum of the latencies for those packets.
    let mut subscribers = Vec::with_capacity(num_subscribers);
    for _ in 0..num_subscribers {
        let mut r = rx.clone();
        let handle = task::spawn(async move {
            let mut packets_received = 0;
            let mut total_latency = Duration::ZERO;

            loop {
                let recv_start_time = Instant::now();
                match r.recv().await {
                    // Successfully received a packet.
                    Ok(Some(_packet)) => {
                        total_latency += recv_start_time.elapsed();
                        packets_received += 1;
                    }
                    // The receiver lagged. This is expected under heavy load.
                    // We simply continue the loop to try receiving again.
                    Err(RecvError::Lagged(_)) => {
                        continue;
                    }
                    // The sender was dropped, and the channel is empty.
                    // This is our signal to terminate.
                    Ok(None) => {
                        break;
                    }
                }
            }
            (packets_received, total_latency)
        });
        subscribers.push(handle);
    }

    // Give a brief moment for all subscribers to spawn and be ready to receive.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // 3. --- Publisher sends packets ---
    let send_start = Instant::now();
    for i in 0..num_packets_to_send {
        // Use the new, simplified `send` method.
        tx.send(i);

        // Introduce a small, artificial delay to simulate a real-world workload
        // and prevent the publisher from overwhelming the receivers instantly.
        if i % 32 == 0 {
            tokio::time::sleep(Duration::from_micros(50)).await;
        }
    }
    let total_send_duration = send_start.elapsed();

    // 4. --- Signal Termination ---
    // Drop the sender. This will cause all `recv()` calls to eventually
    // return `Ok(None)`, reliably terminating the subscriber tasks.
    drop(tx);

    // 5. --- Collect and Aggregate Results ---
    let mut total_packets_delivered = 0;
    let mut total_latency_sum = Duration::ZERO;

    for sub_handle in subscribers {
        // Await the result from each subscriber task.
        let (packets_count, latency_sum) = sub_handle.await.unwrap();
        total_packets_delivered += packets_count;
        total_latency_sum += latency_sum;
    }

    // 6. --- Compute and Print Metrics ---
    // Throughput is the total number of packets successfully delivered to all
    // subscribers, divided by the time it took the sender to send them all.
    let throughput = total_packets_delivered as f64 / total_send_duration.as_secs_f64();

    // Average latency is the total latency sum divided by the number of deliveries.
    // Avoid division by zero if no packets were delivered.
    let avg_latency = if total_packets_delivered > 0 {
        total_latency_sum / total_packets_delivered as u32
    } else {
        Duration::ZERO
    };

    println!(
        "Delivered: {} packets, Total Send Time: {:.2?}, Throughput: {:.0} pkt/sec, Avg Delivery Latency: {:.2?}",
        total_packets_delivered, total_send_duration, throughput, avg_latency
    );
}

criterion_group!(benches, bench_broadcast_fanout);
criterion_main!(benches);
