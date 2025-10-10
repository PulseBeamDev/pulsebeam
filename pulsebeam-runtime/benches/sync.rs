use criterion::{Criterion, criterion_group, criterion_main};
use futures::{StreamExt, future::join_all, stream::FuturesUnordered};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::task;

// Use the specified import path for the SPMC channel implementation.
use pulsebeam_runtime::sync::spmc::{RecvError, Sender, channel};

// --- Benchmark Group Definition ---
criterion_group!(
    benches,
    bench_interactive_room_mesh_subs,
    bench_interactive_room_isolated_subs,
    bench_single_fanout,
);
criterion_main!(benches);

// =================================================================================
// Benchmark 1: Single Publisher High Fan-Out
// =================================================================================

/// Measures broadcast throughput, packet loss, and p99.9 latency under heavy fan-out.
fn bench_single_fanout(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_single_publisher_fanout");
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(10);

    group.bench_function("3000_subs_2000_pkts", |b| {
        b.to_async(&rt)
            .iter_custom(|iters| async move { run_test_loop(iters, run_single_fanout_test).await });
    });
    group.finish();
}

async fn run_single_fanout_test() {
    let (tx, rx) = channel::<(usize, Instant)>(256);
    let num_subscribers = 3000;
    let num_packets_to_send = 2_000;

    let subscriber_tasks = (0..num_subscribers)
        .map(|_| {
            let mut r = rx.clone();
            task::spawn(async move {
                let mut latencies = Vec::with_capacity(num_packets_to_send);
                loop {
                    match r.recv().await {
                        Ok(Some((_, send_time))) => latencies.push(send_time.elapsed()),
                        Err(RecvError::Lagged(_)) => continue,
                        Ok(None) => break,
                    }
                }
                latencies
            })
        })
        .collect();

    let publisher_task = task::spawn(async move {
        // Give subscribers a moment to spin up before starting.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let send_start = Instant::now();
        for i in 0..num_packets_to_send {
            tx.send((i, Instant::now()));
            // Simulate a bursty, video-like packet sending load.
            if i % 10 == 0 && i % 20 != 0 {
                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        }
        send_start.elapsed()
    });

    let send_duration = publisher_task.await.unwrap();
    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    print_metrics(
        "Single Fan-Out",
        send_duration,
        all_latencies,
        num_packets_to_send * num_subscribers,
    );
}

// =================================================================================
// Benchmark 2: Multi-Publisher with Isolated Subscribers
// =================================================================================

/// Simulates an interactive room where subscribers listen to a single publisher.
fn bench_interactive_room_isolated_subs(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_isolated");
    group.measurement_time(Duration::from_secs(20));
    group.sample_size(10);

    group.bench_function("150_pubs_150_subs_each", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            run_test_loop(iters, run_interactive_room_isolated_test).await
        });
    });
    group.finish();
}

async fn run_interactive_room_isolated_test() {
    let num_publishers = 150;
    let subscribers_per_publisher = 150;
    let num_packets_per_publisher = 1_000;

    let mut publisher_tasks = Vec::with_capacity(num_publishers);
    let mut subscriber_tasks = Vec::with_capacity(num_publishers * subscribers_per_publisher);

    for _ in 0..num_publishers {
        let (tx, rx) = channel::<(usize, Instant)>(256);
        for _ in 0..subscribers_per_publisher {
            let mut r = rx.clone();
            subscriber_tasks.push(task::spawn(async move {
                let mut latencies = Vec::with_capacity(num_packets_per_publisher);
                loop {
                    match r.recv().await {
                        Ok(Some((_, send_time))) => latencies.push(send_time.elapsed()),
                        Err(RecvError::Lagged(_)) => continue,
                        Ok(None) => break,
                    }
                }
                latencies
            }));
        }
        publisher_tasks.push(task::spawn(create_publisher_load(
            tx,
            num_packets_per_publisher,
        )));
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    print_metrics(
        "Interactive Room (Isolated)",
        total_send_duration,
        all_latencies,
        num_packets_per_publisher * subscribers_per_publisher * num_publishers,
    );
}

// =================================================================================
// Benchmark 3: Multi-Publisher with Mesh Subscribers (Using FuturesUnordered)
// =================================================================================

/// Simulates an interactive room where subscribers listen to ALL publishers.
fn bench_interactive_room_mesh_subs(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_mesh");
    group.measurement_time(Duration::from_secs(25));
    group.sample_size(10);

    group.bench_function("150_pubs_150_total_subs", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            run_test_loop(iters, run_interactive_room_mesh_test).await
        });
    });
    group.finish();
}

async fn run_interactive_room_mesh_test() {
    let num_publishers = 150;
    let subscribers_per_publisher = 150;
    let num_packets_per_publisher = 1_000;

    let mut publisher_tasks = Vec::with_capacity(num_publishers);
    let mut subscriber_tasks = Vec::with_capacity(num_publishers);

    for _ in 0..num_publishers {
        let mut futs = FuturesUnordered::new();
        let (tx, rx) = channel::<(usize, Instant)>(256);
        for _ in 0..subscribers_per_publisher {
            let mut r = rx.clone();
            futs.push(task::spawn(async move {
                let mut latencies = Vec::with_capacity(num_packets_per_publisher);
                loop {
                    match r.recv().await {
                        Ok(Some((_, send_time))) => latencies.push(send_time.elapsed()),
                        Err(RecvError::Lagged(_)) => continue,
                        Ok(None) => break,
                    }
                }
                latencies
            }));
        }

        subscriber_tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(num_packets_per_publisher);
            while let Some(res) = futs.next().await {
                let latency = res.unwrap();
                latencies.extend(latency);
            }
            latencies
        }));
        publisher_tasks.push(task::spawn(create_publisher_load(
            tx,
            num_packets_per_publisher,
        )));
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    print_metrics(
        "Interactive Room (FuturesUnordered)",
        total_send_duration,
        all_latencies,
        num_packets_per_publisher * subscribers_per_publisher * num_publishers,
    );
}

// =================================================================================
// Helper Functions
// =================================================================================

/// Helper to run a test function `iters` times for Criterion's iter_custom.
async fn run_test_loop<F, Fut>(iters: u64, test_fn: F) -> Duration
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut total_duration = Duration::ZERO;
    for _ in 0..iters {
        let start_time = Instant::now();
        test_fn().await;
        total_duration += start_time.elapsed();
    }
    total_duration
}

/// Creates a publisher task that sends a bursty load.
async fn create_publisher_load(tx: Sender<(usize, Instant)>, num_packets: usize) {
    tokio::time::sleep(Duration::from_millis(20)).await; // Stagger starts slightly
    for i in 0..num_packets {
        tx.send((i, Instant::now()));
        if i % 10 == 0 && i % 20 != 0 {
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    }
}

/// Waits for all subscriber tasks and aggregates their latency results into a single vector.
async fn aggregate_latencies(handles: Vec<task::JoinHandle<Vec<Duration>>>) -> Vec<Duration> {
    join_all(handles)
        .await
        .into_iter()
        .flat_map(|res| res.unwrap()) // Panicking is fine in a benchmark on error
        .collect()
}

/// Computes and prints the final performance metrics.
fn print_metrics(
    context: &str,
    send_duration: Duration,
    mut all_latencies: Vec<Duration>,
    total_possible_deliveries: usize,
) {
    let total_packets_delivered = all_latencies.len();
    let lost_packets = total_possible_deliveries.saturating_sub(total_packets_delivered);
    let loss_percentage = if total_possible_deliveries > 0 {
        lost_packets as f64 * 100.0 / total_possible_deliveries as f64
    } else {
        0.0
    };

    let throughput = total_packets_delivered as f64 / send_duration.as_secs_f64();

    let p99_9_latency = if !all_latencies.is_empty() {
        all_latencies.sort_unstable();
        let p99_9_idx = (all_latencies.len() as f64 * 0.999).floor() as usize;
        let final_idx = p99_9_idx.min(all_latencies.len() - 1);
        all_latencies[final_idx]
    } else {
        Duration::ZERO
    };

    println!(
        "[{}] Send Time: {:.2?}, Throughput: {:.0} pkt/sec, Packet Loss: {:.2}%, p99.9 Latency: {:.2?}",
        context, send_duration, throughput, loss_percentage, p99_9_latency
    );
}
