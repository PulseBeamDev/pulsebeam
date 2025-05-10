use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task;

// --- Common Configuration ---
const PACKET_PAYLOAD_SIZE: usize = 1200; // Simulate MTU-sized RTP packet
const CHANNEL_CAPACITY: usize = 128; // Common channel capacity
const PER_PACKET_CPU_SIM_WORK: Duration = Duration::from_nanos(50); // 50ns, adjust as needed

// --- Fan-Out (1:N) Specific Configuration ---
const NUM_SUBSCRIBERS_FAN_OUT: usize = 5; // N: Number of subscribers for the single publisher
const MESSAGES_BY_PUBLISHER_FAN_OUT: usize = 1_000_000; // Messages the single publisher sends (cloned N times)

// --- Fan-In (M:1) Specific Configuration ---
const NUM_PUBLISHERS_FAN_IN: usize = 5; // M: Number of publishers sending to one subscriber
const MESSAGES_PER_PUBLISHER_FAN_IN: usize = 1_000_000; // Messages each of M publishers sends

#[derive(Clone)]
struct MediaPacket {
    data: Arc<Vec<u8>>,
    // Add more fields if needed, e.g., publisher_id, sequence_number
}

impl MediaPacket {
    fn new() -> Self {
        Self {
            data: Arc::new(vec![0; PACKET_PAYLOAD_SIZE]),
        }
    }
}

async fn simulate_packet_processing_work() {
    if PER_PACKET_CPU_SIM_WORK > Duration::ZERO {
        let start = Instant::now();
        while start.elapsed() < PER_PACKET_CPU_SIM_WORK {
            std::hint::spin_loop(); // Busy wait for very short durations
        }
        task::yield_now().await; // Yield to allow other tasks to run
    }
}

// --- Generic Benchmark Runner ---
async fn run_bench<F, Fut>(scenario_name: &str, bench_name: &str, config_details: &str, bench_fn: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = u64>, // Returns total packets processed at the final destination
{
    println!(
        "Running {} for {} ({})...",
        scenario_name, bench_name, config_details
    );

    // Optional: Warm-up run (can be useful for very short tests or JITted environments)
    // bench_fn().await;
    // println!("Warm-up for {} done.", bench_name);

    let start_time = Instant::now();
    let total_packets_processed = bench_fn().await;
    let elapsed_time = start_time.elapsed();

    let packets_per_second = total_packets_processed as f64 / elapsed_time.as_secs_f64();

    println!(
        "{} for {} DONE in {:.2?}. Total packets processed: {}. Throughput: {:.2} packets/sec",
        scenario_name, bench_name, elapsed_time, total_packets_processed, packets_per_second
    );
}

#[tokio::main(worker_threads = 4)] // Pin worker threads for more consistent results
async fn main() {
    println!("--- SFU Channel Benchmark Suite ---");
    println!(
        "Packet Size: {} bytes, Channel Capacity: {}, Simulated Work: {:?}",
        PACKET_PAYLOAD_SIZE, CHANNEL_CAPACITY, PER_PACKET_CPU_SIM_WORK
    );
    println!("Tokio worker_threads: 4\n");

    // --- Fan-Out (1 Publisher to N Subscribers) Benchmarks ---
    println!("--- Scenario: Fan-Out (1 Publisher -> N Subscribers) ---");
    let fan_out_config = format!(
        "1 pub, {} subs, {} msgs/pub (total {} msg sends)",
        NUM_SUBSCRIBERS_FAN_OUT,
        MESSAGES_BY_PUBLISHER_FAN_OUT,
        MESSAGES_BY_PUBLISHER_FAN_OUT * NUM_SUBSCRIBERS_FAN_OUT
    );
    run_bench(
        "Fan-Out",
        "async-channel",
        &fan_out_config,
        bench_async_channel_fan_out,
    )
    .await;
    run_bench("Fan-Out", "flume", &fan_out_config, bench_flume_fan_out).await;
    run_bench(
        "Fan-Out",
        "tokio::mpsc",
        &fan_out_config,
        bench_tokio_mpsc_fan_out,
    )
    .await;
    println!("---\n");

    // --- Fan-In (M Publishers to 1 Subscriber) Benchmarks ---
    println!("--- Scenario: Fan-In (M Publishers -> 1 Subscriber) ---");
    let fan_in_config = format!(
        "{} pubs, 1 sub, {} msgs/pub (total {} msgs received)",
        NUM_PUBLISHERS_FAN_IN,
        MESSAGES_PER_PUBLISHER_FAN_IN,
        NUM_PUBLISHERS_FAN_IN * MESSAGES_PER_PUBLISHER_FAN_IN
    );
    run_bench(
        "Fan-In",
        "async-channel",
        &fan_in_config,
        bench_async_channel_fan_in,
    )
    .await;
    run_bench("Fan-In", "flume", &fan_in_config, bench_flume_fan_in).await;
    run_bench(
        "Fan-In",
        "tokio::mpsc",
        &fan_in_config,
        bench_tokio_mpsc_fan_in,
    )
    .await;
    println!("---");
}

// --- Implementations for Fan-Out (1:N) ---

async fn bench_flume_fan_out() -> u64 {
    let mut publisher_sinks = Vec::new();
    let mut subscriber_handles = Vec::new();
    let mut total_received_count = 0_u64;

    for _i in 0..NUM_SUBSCRIBERS_FAN_OUT {
        let (tx, rx) = flume::bounded::<MediaPacket>(CHANNEL_CAPACITY);
        publisher_sinks.push(tx);

        let handle = task::spawn(async move {
            let mut count = 0;
            for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
                // Each subscriber expects this many
                if let Ok(_packet) = rx.recv_async().await {
                    simulate_packet_processing_work().await;
                    count += 1;
                } else {
                    break;
                }
            }
            count
        });
        subscriber_handles.push(handle);
    }

    let publisher_handle = task::spawn(async move {
        for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
            let original_packet = MediaPacket::new();
            for sink in &publisher_sinks {
                // Packet is cloned for each sink
                if sink.send_async(original_packet.clone()).await.is_err() {
                    // Receiver likely dropped
                }
            }
        }
    });

    publisher_handle.await.unwrap();
    for handle in subscriber_handles {
        total_received_count += handle.await.unwrap_or(0);
    }
    total_received_count
}

async fn bench_tokio_mpsc_fan_out() -> u64 {
    let mut publisher_sinks = Vec::new();
    let mut subscriber_handles = Vec::new();
    let mut total_received_count = 0_u64;

    for _i in 0..NUM_SUBSCRIBERS_FAN_OUT {
        // For fan-out with MPSC, each subscriber needs its own MPSC channel.
        // The "publisher" will hold multiple Senders.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaPacket>(CHANNEL_CAPACITY);
        publisher_sinks.push(tx);

        let handle = task::spawn(async move {
            let mut count = 0;
            // Loop should ideally consume MESSAGES_BY_PUBLISHER_FAN_OUT
            // or until channel closes
            for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
                if let Some(_packet) = rx.recv().await {
                    simulate_packet_processing_work().await;
                    count += 1;
                } else {
                    break; // Channel closed
                }
            }
            // Alternative if publisher doesn't explicitly close/drop senders one by one:
            // while let Some(_packet) = rx.recv().await {
            //     simulate_packet_processing_work().await;
            //     count += 1;
            // }
            count
        });
        subscriber_handles.push(handle);
    }

    let publisher_handle = task::spawn(async move {
        for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
            let original_packet = MediaPacket::new();
            for sink in &publisher_sinks {
                if sink.send(original_packet.clone()).await.is_err() {
                    // Receiver dropped
                }
            }
        }
        // Sinks are dropped when publisher_handle finishes, closing channels
    });

    publisher_handle.await.unwrap();
    for handle in subscriber_handles {
        total_received_count += handle.await.unwrap_or(0);
    }
    total_received_count
}

async fn bench_async_channel_fan_out() -> u64 {
    let mut publisher_sinks = Vec::new();
    let mut subscriber_handles = Vec::new();
    let mut total_received_count = 0_u64;

    for _i in 0..NUM_SUBSCRIBERS_FAN_OUT {
        let (tx, rx) = async_channel::bounded::<MediaPacket>(CHANNEL_CAPACITY);
        publisher_sinks.push(tx);

        let handle = task::spawn(async move {
            let mut count = 0;
            for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
                if let Ok(_packet) = rx.recv().await {
                    simulate_packet_processing_work().await;
                    count += 1;
                } else {
                    break;
                }
            }
            count
        });
        subscriber_handles.push(handle);
    }

    let publisher_handle = task::spawn(async move {
        for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
            let original_packet = MediaPacket::new();
            for sink in &publisher_sinks {
                if sink.send(original_packet.clone()).await.is_err() {
                    // Receiver likely dropped
                }
            }
        }
    });

    publisher_handle.await.unwrap();
    for handle in subscriber_handles {
        total_received_count += handle.await.unwrap_or(0);
    }
    total_received_count
}

// --- Implementations for Fan-In (M:1) ---

async fn bench_flume_fan_in() -> u64 {
    let (tx, rx) = flume::bounded::<MediaPacket>(CHANNEL_CAPACITY);
    let mut publisher_handles = Vec::new();
    let total_expected_messages = (NUM_PUBLISHERS_FAN_IN * MESSAGES_PER_PUBLISHER_FAN_IN) as u64;

    for _i in 0..NUM_PUBLISHERS_FAN_IN {
        let publisher_tx = tx.clone(); // Clone sender for each publisher
        let handle = task::spawn(async move {
            for _ in 0..MESSAGES_PER_PUBLISHER_FAN_IN {
                if publisher_tx.send_async(MediaPacket::new()).await.is_err() {
                    break;
                }
            }
        });
        publisher_handles.push(handle);
    }
    drop(tx); // Drop the original sender, channel closes when all clones are dropped.

    let subscriber_handle = task::spawn(async move {
        let mut count = 0_u64;
        // Loop until channel is empty AND all senders are dropped
        while let Ok(_packet) = rx.recv_async().await {
            simulate_packet_processing_work().await;
            count += 1;
        }
        count
    });

    for handle in publisher_handles {
        handle.await.unwrap();
    }
    let received_count = subscriber_handle.await.unwrap_or(0);
    assert_eq!(
        received_count, total_expected_messages,
        "Flume Fan-In: Packet count mismatch!"
    );
    received_count
}

async fn bench_tokio_mpsc_fan_in() -> u64 {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaPacket>(CHANNEL_CAPACITY);
    let mut publisher_handles = Vec::new();
    let total_expected_messages = (NUM_PUBLISHERS_FAN_IN * MESSAGES_PER_PUBLISHER_FAN_IN) as u64;

    for _i in 0..NUM_PUBLISHERS_FAN_IN {
        let publisher_tx = tx.clone();
        let handle = task::spawn(async move {
            for _ in 0..MESSAGES_PER_PUBLISHER_FAN_IN {
                if publisher_tx.send(MediaPacket::new()).await.is_err() {
                    break;
                }
            }
        });
        publisher_handles.push(handle);
    }
    drop(tx); // Crucial for mpsc: drop original sender so recv() can eventually return None.

    let subscriber_handle = task::spawn(async move {
        let mut count = 0_u64;
        while let Some(_packet) = rx.recv().await {
            simulate_packet_processing_work().await;
            count += 1;
        }
        count
    });

    for handle in publisher_handles {
        handle.await.unwrap();
    }
    let received_count = subscriber_handle.await.unwrap_or(0);
    assert_eq!(
        received_count, total_expected_messages,
        "Tokio MPSC Fan-In: Packet count mismatch!"
    );
    received_count
}

async fn bench_async_channel_fan_in() -> u64 {
    let (tx, rx) = async_channel::bounded::<MediaPacket>(CHANNEL_CAPACITY);
    let mut publisher_handles = Vec::new();
    let total_expected_messages = (NUM_PUBLISHERS_FAN_IN * MESSAGES_PER_PUBLISHER_FAN_IN) as u64;

    for _i in 0..NUM_PUBLISHERS_FAN_IN {
        let publisher_tx = tx.clone();
        let handle = task::spawn(async move {
            for _ in 0..MESSAGES_PER_PUBLISHER_FAN_IN {
                if publisher_tx.send(MediaPacket::new()).await.is_err() {
                    break;
                }
            }
        });
        publisher_handles.push(handle);
    }
    drop(tx); // Drop original sender.

    let subscriber_handle = task::spawn(async move {
        let mut count = 0_u64;
        while let Ok(_packet) = rx.recv().await {
            simulate_packet_processing_work().await;
            count += 1;
        }
        count
    });

    for handle in publisher_handles {
        handle.await.unwrap();
    }
    let received_count = subscriber_handle.await.unwrap_or(0);
    assert_eq!(
        received_count, total_expected_messages,
        "Async-channel Fan-In: Packet count mismatch!"
    );
    received_count
}
