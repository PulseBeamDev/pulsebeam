use criterion::{Criterion, criterion_group, criterion_main};
use futures::{StreamExt, future::join_all};
use futures_concurrency::stream::Merge;
use rand::{Rng, seq::index};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task;

// Use the specified import path for the SPMC channel implementation.
use pulsebeam_runtime::sync::spmc::{RecvError, Sender, channel};

const NUM_PUBLISHERS: usize = 1;
const NUM_SUBSCRIBERS: usize = 3000;
const NUM_PACKETS_PER_PUBLISHER: usize = 1_000;
const SUBSCRIPTIONS_PER_SUBSCRIBER: usize = 1;

// --- Benchmark Group Definition ---
criterion_group!(
    benches,
    bench_interactive_room_mesh_mpsc_fanout,
    bench_interactive_room_mesh_futures_unordered,
    bench_interactive_room_mesh_poll,
    bench_interactive_room_mesh_spawn,
);
criterion_main!(benches);

// =================================================================================
// Benchmark 1: Mesh Room using "MPSC Fanout"
// =================================================================================
fn bench_interactive_room_mesh_mpsc_fanout(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_mesh_mpsc_fanout");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);
    group.bench_function(
        format!(
            "{NUM_PUBLISHERS}_pubs_{NUM_SUBSCRIBERS}_subs_{NUM_PACKETS_PER_PUBLISHER}_pkts_{SUBSCRIPTIONS_PER_SUBSCRIBER}_persub_mpsc_fanout"
        ),
        |b| {
            b.to_async(&rt).iter_custom(|iters| async move {
                run_test_loop(iters, run_interactive_room_mesh_mpsc_fanout_test).await
            });
        },
    );
    group.finish();
}

async fn run_interactive_room_mesh_mpsc_fanout_test() {
    let mut rng = rand::rng();

    // Create the communication channels between publishers, fanout tasks, and subscribers
    let mut fanout_targets: Vec<Vec<mpsc::Sender<(usize, Instant)>>> = vec![vec![]; NUM_PUBLISHERS];

    // CORRECTED INITIALIZATION: Build the Vec of Vecs without cloning.
    let mut subscriber_receivers: Vec<Vec<mpsc::Receiver<(usize, Instant)>>> =
        Vec::with_capacity(NUM_SUBSCRIBERS);
    for _ in 0..NUM_SUBSCRIBERS {
        subscriber_receivers.push(Vec::with_capacity(SUBSCRIPTIONS_PER_SUBSCRIBER));
    }

    for s_idx in 0..NUM_SUBSCRIBERS {
        let publisher_indices =
            index::sample(&mut rng, NUM_PUBLISHERS, SUBSCRIPTIONS_PER_SUBSCRIBER);
        for p_idx in publisher_indices.iter() {
            let (tx, rx) = mpsc::channel(256);
            fanout_targets[p_idx].push(tx);
            subscriber_receivers[s_idx].push(rx);
        }
    }

    // Spawn subscriber tasks
    let mut subscriber_tasks = Vec::with_capacity(NUM_SUBSCRIBERS);
    for receivers in subscriber_receivers {
        let handle = task::spawn(async move {
            let mut streams = Vec::new();
            for mut receiver in receivers {
                streams.push(async_stream::stream! {
                    while let Some(res) = receiver.recv().await {
                        yield res.1.elapsed();
                    }
                });
            }
            let merged_stream = streams.merge();
            merged_stream.collect::<Vec<Duration>>().await
        });
        subscriber_tasks.push(handle);
    }

    // Spawn publisher and fanout tasks
    let mut publisher_tasks = Vec::with_capacity(NUM_PUBLISHERS);
    for targets in fanout_targets {
        let (pub_tx, mut fanout_rx) = mpsc::channel(256);

        // Spawn the fanout task for this publisher
        task::spawn(async move {
            while let Some(msg) = fanout_rx.recv().await {
                for target_tx in &targets {
                    // If a subscriber is slow or gone, we don't want it to block the fanout task.
                    // We use try_send and ignore the error if the channel is full or closed.
                    let _ = target_tx.try_send(msg);
                }
            }
        });

        // Spawn the publisher task that sends to the fanout task
        let handle = task::spawn(create_publisher_load_mpsc(
            pub_tx,
            NUM_PACKETS_PER_PUBLISHER,
        ));
        publisher_tasks.push(handle);
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    let total_possible_deliveries =
        NUM_PACKETS_PER_PUBLISHER * SUBSCRIPTIONS_PER_SUBSCRIBER * NUM_SUBSCRIBERS;

    print_metrics(
        "Interactive Room (MPSC Fanout)",
        total_send_duration,
        all_latencies,
        total_possible_deliveries,
    );
}

// =================================================================================
// Benchmark 2: Mesh Room using "Spawn per Subscription"
// =================================================================================

fn bench_interactive_room_mesh_spawn(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_mesh_spawn");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);
    group.bench_function(
        format!(
            "{NUM_PUBLISHERS}_pubs_{NUM_SUBSCRIBERS}_subs_{NUM_PACKETS_PER_PUBLISHER}_pkts_{SUBSCRIPTIONS_PER_SUBSCRIBER}_persub_spawn_per_sub"
        ),
        |b| {
            b.to_async(&rt).iter_custom(|iters| async move {
                run_test_loop(iters, run_interactive_room_mesh_spawn_test).await
            });
        },
    );
    group.finish();
}

async fn run_interactive_room_mesh_spawn_test() {
    let mut senders = Vec::with_capacity(NUM_PUBLISHERS);
    let mut initial_receivers = Vec::with_capacity(NUM_PUBLISHERS);
    for _ in 0..NUM_PUBLISHERS {
        let (tx, rx) = channel::<(usize, Instant)>(256);
        senders.push(tx);
        initial_receivers.push(rx);
    }

    let mut subscriber_tasks = Vec::with_capacity(NUM_PUBLISHERS);
    // Create the random number generator once, outside the loop for efficiency.
    let mut rng = rand::rng();

    for _ in 0..NUM_SUBSCRIBERS {
        // --- CHANGE 2: Select 15 random receivers for this specific subscriber. ---
        // First, sample 15 unique random indices from the full range of publishers.
        let random_indices = index::sample(&mut rng, NUM_PUBLISHERS, SUBSCRIPTIONS_PER_SUBSCRIBER);

        // Then, create the list of receivers by cloning only the ones at the random indices.
        let mut subs_receivers = Vec::with_capacity(SUBSCRIPTIONS_PER_SUBSCRIBER);
        for i in random_indices.iter() {
            subs_receivers.push(initial_receivers[i].clone());
        }

        let handle = task::spawn(async move {
            let (latency_tx, mut latency_rx) = mpsc::channel(256);

            // This loop now correctly iterates over the 15 randomly selected receivers.
            for mut receiver in subs_receivers {
                let tx_clone = latency_tx.clone();
                task::spawn(async move {
                    loop {
                        match receiver.recv().await {
                            Ok(res) => {
                                if tx_clone.send(res.1.elapsed()).await.is_err() {
                                    break;
                                }
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
            }
            drop(latency_tx);

            let mut latencies = Vec::new();
            while let Some(latency) = latency_rx.recv().await {
                latencies.push(latency);
            }
            latencies
        });
        subscriber_tasks.push(handle);
    }

    let mut publisher_tasks = Vec::with_capacity(senders.len());
    for tx in senders {
        let handle = task::spawn(create_publisher_load(tx, NUM_PACKETS_PER_PUBLISHER));
        publisher_tasks.push(handle);
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;

    let total_possible_deliveries =
        NUM_PACKETS_PER_PUBLISHER * SUBSCRIPTIONS_PER_SUBSCRIBER * NUM_SUBSCRIBERS;

    print_metrics(
        // Updated context name for clarity in the results.
        "Interactive Room (Mesh-Spawn-Random-15)",
        total_send_duration,
        all_latencies,
        total_possible_deliveries,
    );
}

// =================================================================================
// Benchmark 3: Mesh Room using "FuturesUnordered"
// =================================================================================

fn bench_interactive_room_mesh_futures_unordered(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_mesh_futures_unordered");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);
    group.bench_function(
        format!(
            "{NUM_PUBLISHERS}_pubs_{NUM_SUBSCRIBERS}_subs_{NUM_PACKETS_PER_PUBLISHER}_pkts_{SUBSCRIPTIONS_PER_SUBSCRIBER}_persub_futures_unordered"
        ),
        |b| {
            b.to_async(&rt).iter_custom(|iters| async move {
                run_test_loop(iters, run_interactive_room_mesh_futures_unordered_test).await
            });
        },
    );
    group.finish();
}

async fn run_interactive_room_mesh_futures_unordered_test() {
    let mut senders = Vec::with_capacity(NUM_PUBLISHERS);
    let mut initial_receivers = Vec::with_capacity(NUM_PUBLISHERS);
    for _ in 0..NUM_PUBLISHERS {
        let (tx, rx) = channel::<(usize, Instant)>(256);
        senders.push(tx);
        initial_receivers.push(rx);
    }

    let mut rng = rand::rng();

    let mut subscriber_tasks = Vec::with_capacity(NUM_SUBSCRIBERS);
    for _ in 0..NUM_SUBSCRIBERS {
        let random_indices = index::sample(&mut rng, NUM_PUBLISHERS, SUBSCRIPTIONS_PER_SUBSCRIBER);

        let mut subs_receivers = Vec::with_capacity(SUBSCRIPTIONS_PER_SUBSCRIBER);
        for i in random_indices.iter() {
            subs_receivers.push(initial_receivers[i].clone());
        }

        let handle = task::spawn(async move {
            let mut futs = Vec::new();

            for mut receiver in subs_receivers {
                futs.push(async_stream::stream! {
                    loop {
                        match receiver.recv().await {
                            Ok(res) => {
                                yield res.1.elapsed();
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
            }
            let latencies = futs.merge();
            latencies.collect().await
        });
        subscriber_tasks.push(handle);
    }

    let mut publisher_tasks = Vec::with_capacity(senders.len());
    for tx in senders {
        let handle = task::spawn(create_publisher_load(tx, NUM_PACKETS_PER_PUBLISHER));
        publisher_tasks.push(handle);
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    let total_possible_deliveries =
        NUM_PACKETS_PER_PUBLISHER * SUBSCRIPTIONS_PER_SUBSCRIBER * NUM_SUBSCRIBERS;
    print_metrics(
        "Interactive Room (Mesh-Spawn)",
        total_send_duration,
        all_latencies,
        total_possible_deliveries,
    );
}

fn bench_interactive_room_mesh_poll(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("spmc_interactive_room_mesh_poll");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);
    group.bench_function(
        format!(
            "{NUM_PUBLISHERS}_pubs_{NUM_SUBSCRIBERS}_subs_{NUM_PACKETS_PER_PUBLISHER}_pkts_{SUBSCRIPTIONS_PER_SUBSCRIBER}_persub_poll"
        ),
        |b| {
            b.to_async(&rt).iter_custom(|iters| async move {
                run_test_loop(iters, run_interactive_room_mesh_poll_test).await
            });
        },
    );
    group.finish();
}

async fn run_interactive_room_mesh_poll_test() {
    let mut senders = Vec::with_capacity(NUM_PUBLISHERS);
    let mut initial_receivers = Vec::with_capacity(NUM_PUBLISHERS);
    for _ in 0..NUM_PUBLISHERS {
        let (tx, rx) = channel::<(usize, Instant)>(256);
        senders.push(tx);
        initial_receivers.push(rx);
    }

    let mut rng = rand::rng();

    let mut subscriber_tasks = Vec::with_capacity(NUM_SUBSCRIBERS);
    for _ in 0..NUM_SUBSCRIBERS {
        let random_indices = index::sample(&mut rng, NUM_PUBLISHERS, SUBSCRIPTIONS_PER_SUBSCRIBER);

        let mut subs_receivers = Vec::with_capacity(SUBSCRIPTIONS_PER_SUBSCRIBER);
        for i in random_indices.iter() {
            subs_receivers.push(initial_receivers[i].clone());
        }

        let handle = task::spawn(async move {
            let mut latencies = Vec::new();

            // --- TUNING PARAMETERS ---
            // 1. Busy Budget: How many times to 'continue' draining before forced yield.
            // Prevents starvation when under heavy load.
            const BUSY_BUDGET_MAX: u32 = 16;

            // 2. Idle Spins: How many times to 'yield_now' before resorting to sleep.
            // Keeps latency low during short gaps between packets.
            const IDLE_SPINS_MAX: u32 = 50;

            // 3. Sleep Backoff: Only used during long silences (between frames).
            // Kept very low to catch the start of the next frame quickly.
            let min_sleep = Duration::from_micros(50);
            let max_sleep = Duration::from_millis(1); // Max 1ms sleep

            // --- STATE ---
            let mut busy_budget = BUSY_BUDGET_MAX;
            let mut idle_spins = 0;
            let mut current_sleep = min_sleep;

            loop {
                if subs_receivers.is_empty() {
                    break;
                }

                let mut work_done = false;

                // Greedy non-blocking drain
                for receiver in subs_receivers.iter_mut() {
                    while let Ok(Some(msg)) = receiver.try_recv() {
                        latencies.push(msg.1.elapsed());
                        work_done = true;
                    }
                }
                subs_receivers.retain(|receiver| !receiver.is_closed());

                if work_done {
                    // --- BUSY STATE ---
                    // Reset idle counters because we found work.
                    idle_spins = 0;
                    current_sleep = min_sleep;

                    busy_budget -= 1;
                    if busy_budget == 0 {
                        // Exhausted budget, yield to prevent starvation.
                        busy_budget = BUSY_BUDGET_MAX;
                        tokio::task::yield_now().await;
                    } else {
                        // Stay hot, check again immediately.
                        continue;
                    }
                } else {
                    // --- IDLE STATE ---
                    // Reset busy budget.
                    busy_budget = BUSY_BUDGET_MAX;

                    if idle_spins < IDLE_SPINS_MAX {
                        // Phase 1: Spin/Yield. High CPU, very low latency.
                        idle_spins += 1;
                        tokio::task::yield_now().await;
                    } else {
                        // Phase 2: Sleep. Saves CPU during the ~30ms gap between frames.
                        tokio::time::sleep(current_sleep).await;
                        current_sleep = (current_sleep * 2).min(max_sleep);
                    }
                }
            }
            latencies
        });

        subscriber_tasks.push(handle);
    }

    let mut publisher_tasks = Vec::with_capacity(senders.len());
    for tx in senders {
        let handle = task::spawn(create_publisher_load(tx, NUM_PACKETS_PER_PUBLISHER));
        publisher_tasks.push(handle);
    }

    let simulation_start = Instant::now();
    join_all(publisher_tasks).await;
    let total_send_duration = simulation_start.elapsed();

    let all_latencies = aggregate_latencies(subscriber_tasks).await;
    let total_possible_deliveries =
        NUM_PACKETS_PER_PUBLISHER * SUBSCRIPTIONS_PER_SUBSCRIBER * NUM_SUBSCRIBERS;
    print_metrics(
        "Interactive Room (Mesh-Spawn)",
        total_send_duration,
        all_latencies,
        total_possible_deliveries,
    );
}

// =================================================================================
// Helper Functions
// =================================================================================

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

async fn create_publisher_load(tx: Sender<(usize, Instant)>, num_packets: usize) {
    // --- SFU WORKLOAD SIMULATION ---

    // Define the characteristics of the simulated video stream.
    const FPS: f64 = 30.0;
    let frame_duration = Duration::from_secs_f64(1.0 / FPS); // Approx 33.3ms

    // --- REALISTIC START TIME RANDOMIZATION ---
    // In a real SFU, publishers don't all start sending at the exact same moment.
    // We introduce a random "join" delay to stagger the start of the streams.
    // This creates a more realistic, less perfectly synchronized initial load.
    let random = rand::random_range(0..500);
    // Each publisher will wait for a random duration between 0 and 500ms before starting.
    let start_delay = Duration::from_millis(random);
    tokio::time::sleep(start_delay).await;

    let mut packets_sent = 0;
    let mut frame_index = 0;

    // The main loop now operates on frames, not individual packets.
    while packets_sent < num_packets {
        let frame_start_time = Instant::now();

        // Determine the number of packets in this frame's burst.
        // We send 12 packets for 3 out of 10 frames, and 11 for the other 7.
        // This averages to 11.3 packets/frame, achieving our target ~339 pps.
        let burst_size = if frame_index % 10 < 3 { 12 } else { 11 };

        // Send all packets for the current frame in a tight burst.
        for _ in 0..burst_size {
            if packets_sent >= num_packets {
                break;
            }
            // Error ignored intentionally, if receiver is gone, we just stop sending.
            let _ = tx.send((packets_sent, Instant::now()));
            packets_sent += 1;
        }

        // Calculate how long the burst took and sleep for the remaining frame time.
        let burst_duration = frame_start_time.elapsed();
        if let Some(sleep_duration) = frame_duration.checked_sub(burst_duration) {
            tokio::time::sleep(sleep_duration).await;
        }
        // If the burst took longer than the frame time (a system hiccup),
        // we just continue to the next frame immediately.

        frame_index += 1;
    }
}

/// A version of the publisher load generator that uses a tokio mpsc channel.
async fn create_publisher_load_mpsc(tx: mpsc::Sender<(usize, Instant)>, num_packets: usize) {
    const FPS: f64 = 30.0;
    let frame_duration = Duration::from_secs_f64(1.0 / FPS);

    let random = rand::random_range(0..500);
    let start_delay = Duration::from_millis(random);
    tokio::time::sleep(start_delay).await;

    let mut packets_sent = 0;
    let mut frame_index = 0;

    while packets_sent < num_packets {
        let frame_start_time = Instant::now();
        let burst_size = if frame_index % 10 < 3 { 12 } else { 11 };

        for _ in 0..burst_size {
            if packets_sent >= num_packets {
                break;
            }
            // Stop sending if the fanout task has shut down
            if tx.send((packets_sent, Instant::now())).await.is_err() {
                return;
            }
            packets_sent += 1;
        }

        let burst_duration = frame_start_time.elapsed();
        if let Some(sleep_duration) = frame_duration.checked_sub(burst_duration) {
            tokio::time::sleep(sleep_duration).await;
        }
        frame_index += 1;
    }
}

async fn aggregate_latencies(handles: Vec<task::JoinHandle<Vec<Duration>>>) -> Vec<Duration> {
    let results = join_all(handles).await;
    let mut all_latencies = Vec::new();
    for result in results {
        if let Ok(subscriber_latencies) = result {
            all_latencies.extend(subscriber_latencies);
        }
    }
    all_latencies
}

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
