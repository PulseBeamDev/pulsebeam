use std::future::Future;
use std::pin::Pin;
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task;

// Add kanal to use statements
use kanal;

// --- Common Configuration ---
const PACKET_PAYLOAD_SIZE: usize = 1200; // Simulate MTU-sized RTP packet
const CHANNEL_CAPACITY: usize = 128; // Common channel capacity
const PER_PACKET_CPU_SIM_WORK: Duration = Duration::from_nanos(0); // 50ns, adjust as needed

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
    // let warmup_fn_wrapper = || Box::pin(async { bench_fn_factory().await }); // Requires bench_fn to be a factory or clonable
    // warmup_fn_wrapper().await;
    // println!("Warm-up for {} done.", bench_name);

    let start_time = Instant::now();
    let total_packets_processed = bench_fn().await;
    let elapsed_time = start_time.elapsed();

    let packets_per_second = if elapsed_time.as_secs_f64() > 0.0 {
        total_packets_processed as f64 / elapsed_time.as_secs_f64()
    } else {
        f64::INFINITY // Avoid division by zero if elapsed time is too short
    };

    println!(
        "{} for {} DONE in {:.2?}. Total packets processed: {}. Throughput: {:.2} packets/sec",
        scenario_name, bench_name, elapsed_time, total_packets_processed, packets_per_second
    );
}

struct BenchTask {
    id: String,
    scenario_name: String,
    channel_name: String,
    config: String,
    func: fn() -> Pin<Box<dyn Future<Output = u64> + Send>>,
    scenario_header_text: String,
}

#[tokio::main(worker_threads = 4)] // Pin worker threads for more consistent results
async fn main() {
    println!("--- SFU Channel Benchmark Suite ---");
    println!(
        "Packet Size: {} bytes, Channel Capacity: {}, Simulated Work: {:?}",
        PACKET_PAYLOAD_SIZE, CHANNEL_CAPACITY, PER_PACKET_CPU_SIM_WORK
    );
    println!("Tokio worker_threads: 4");

    let args: Vec<String> = std::env::args().collect();
    let specific_benchmark_arg = if args.len() > 1 {
        Some(args[1].as_str())
    } else {
        None
    };

    let mut benchmarks: Vec<BenchTask> = Vec::new();

    // --- Fan-Out (1 Publisher to N Subscribers) Benchmarks ---
    let fan_out_config = format!(
        "1 pub, {} subs, {} msgs/pub (total {} cloned msg sends to subscribers)",
        NUM_SUBSCRIBERS_FAN_OUT,
        MESSAGES_BY_PUBLISHER_FAN_OUT,
        MESSAGES_BY_PUBLISHER_FAN_OUT * NUM_SUBSCRIBERS_FAN_OUT
    );
    let fan_out_header = "--- Scenario: Fan-Out (1 Publisher -> N Subscribers) ---".to_string();

    benchmarks.push(BenchTask {
        id: "fan_out-async_channel".to_string(),
        scenario_name: "Fan-Out".to_string(),
        channel_name: "async-channel".to_string(),
        config: fan_out_config.clone(),
        func: || Box::pin(bench_async_channel_fan_out()),
        scenario_header_text: fan_out_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_out-flume".to_string(),
        scenario_name: "Fan-Out".to_string(),
        channel_name: "flume".to_string(),
        config: fan_out_config.clone(),
        func: || Box::pin(bench_flume_fan_out()),
        scenario_header_text: fan_out_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_out-tokio_mpsc".to_string(),
        scenario_name: "Fan-Out".to_string(),
        channel_name: "tokio::mpsc".to_string(),
        config: fan_out_config.clone(),
        func: || Box::pin(bench_tokio_mpsc_fan_out()),
        scenario_header_text: fan_out_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_out-kanal".to_string(),
        scenario_name: "Fan-Out".to_string(),
        channel_name: "kanal".to_string(),
        config: fan_out_config.clone(),
        func: || Box::pin(bench_kanal_fan_out()),
        scenario_header_text: fan_out_header.clone(),
    });

    // --- Fan-In (M Publishers to 1 Subscriber) Benchmarks ---
    let fan_in_config = format!(
        "{} pubs, 1 sub, {} msgs/pub (total {} msgs received by subscriber)",
        NUM_PUBLISHERS_FAN_IN,
        MESSAGES_PER_PUBLISHER_FAN_IN,
        NUM_PUBLISHERS_FAN_IN * MESSAGES_PER_PUBLISHER_FAN_IN
    );
    let fan_in_header = "--- Scenario: Fan-In (M Publishers -> 1 Subscriber) ---".to_string();

    benchmarks.push(BenchTask {
        id: "fan_in-async_channel".to_string(),
        scenario_name: "Fan-In".to_string(),
        channel_name: "async-channel".to_string(),
        config: fan_in_config.clone(),
        func: || Box::pin(bench_async_channel_fan_in()),
        scenario_header_text: fan_in_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_in-flume".to_string(),
        scenario_name: "Fan-In".to_string(),
        channel_name: "flume".to_string(),
        config: fan_in_config.clone(),
        func: || Box::pin(bench_flume_fan_in()),
        scenario_header_text: fan_in_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_in-tokio_mpsc".to_string(),
        scenario_name: "Fan-In".to_string(),
        channel_name: "tokio::mpsc".to_string(),
        config: fan_in_config.clone(),
        func: || Box::pin(bench_tokio_mpsc_fan_in()),
        scenario_header_text: fan_in_header.clone(),
    });
    benchmarks.push(BenchTask {
        id: "fan_in-kanal".to_string(),
        scenario_name: "Fan-In".to_string(),
        channel_name: "kanal".to_string(),
        config: fan_in_config.clone(),
        func: || Box::pin(bench_kanal_fan_in()),
        scenario_header_text: fan_in_header.clone(),
    });

    let benchmarks_to_run: Vec<&BenchTask> = if let Some(requested_id) = specific_benchmark_arg {
        if requested_id == "all" {
            benchmarks.iter().collect()
        } else {
            benchmarks.iter().filter(|b| b.id == requested_id).collect()
        }
    } else {
        benchmarks.iter().collect() // Run all if no arg
    };

    if benchmarks_to_run.is_empty() {
        if let Some(requested_id) = specific_benchmark_arg {
            // This implies requested_id was not "all" and not found
            println!("\nUnknown benchmark: '{}'", requested_id);
            println!("Available benchmarks are:");
            for bench_meta in &benchmarks {
                // Iterate over original benchmarks to list all IDs
                println!("  {}", bench_meta.id);
            }
            println!("  all");
            println!("\n--- Benchmark Suite Aborted ---");
            exit(1);
        }
        // If specific_benchmark_arg was None, benchmarks_to_run would not be empty.
        // This path should ideally not be hit if logic is correct, means no benchmarks defined.
    }

    let mut last_scenario_header = String::new();
    for (i, bench_meta) in benchmarks_to_run.iter().enumerate() {
        if bench_meta.scenario_header_text != last_scenario_header {
            if !last_scenario_header.is_empty() {
                println!("---"); // Separator line if changing scenarios
            }
            println!("\n{}", bench_meta.scenario_header_text);
            last_scenario_header = bench_meta.scenario_header_text.clone();
        } else if i > 0 { // Add a small visual break if same scenario, but not the first one
            // println!("...");
        }

        run_bench(
            &bench_meta.scenario_name,
            &bench_meta.channel_name,
            &bench_meta.config,
            bench_meta.func,
        )
        .await;
    }

    if !benchmarks_to_run.is_empty() {
        println!("---"); // Final separator if any benchmark ran
    }
    println!("--- Benchmark Suite Complete ---");
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaPacket>(CHANNEL_CAPACITY);
        publisher_sinks.push(tx);

        let handle = task::spawn(async move {
            let mut count = 0;
            // let mut buf = Vec::with_capacity(32);
            for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
                // let size = rx.recv_many(&mut buf, 32).await;
                // if size == 0 {
                //     break;
                // }
                // for _packet in buf.iter() {
                //     simulate_packet_processing_work().await;
                //     count += 1;
                // }
                // buf.clear();

                if let Some(_packet) = rx.recv().await {
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
                    // Receiver dropped
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

async fn bench_kanal_fan_out() -> u64 {
    let mut publisher_sinks = Vec::new();
    let mut subscriber_handles = Vec::new();
    let mut total_received_count = 0_u64;

    for _i in 0..NUM_SUBSCRIBERS_FAN_OUT {
        let (tx, rx) = kanal::bounded_async::<MediaPacket>(CHANNEL_CAPACITY);
        publisher_sinks.push(tx);

        let handle = task::spawn(async move {
            let mut count = 0;
            for _ in 0..MESSAGES_BY_PUBLISHER_FAN_OUT {
                if let Ok(_packet) = rx.recv().await {
                    simulate_packet_processing_work().await;
                    count += 1;
                } else {
                    break; // Channel closed
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
        let publisher_tx = tx.clone();
        let handle = task::spawn(async move {
            for _ in 0..MESSAGES_PER_PUBLISHER_FAN_IN {
                if publisher_tx.send_async(MediaPacket::new()).await.is_err() {
                    break;
                }
            }
        });
        publisher_handles.push(handle);
    }
    drop(tx);

    let subscriber_handle = task::spawn(async move {
        let mut count = 0_u64;
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
        "Flume Fan-In: Packet count mismatch! Expected {}, got {}",
        total_expected_messages, received_count
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
    drop(tx);

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
        "Tokio MPSC Fan-In: Packet count mismatch! Expected {}, got {}",
        total_expected_messages, received_count
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
    drop(tx);

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
        "Async-channel Fan-In: Packet count mismatch! Expected {}, got {}",
        total_expected_messages, received_count
    );
    received_count
}

async fn bench_kanal_fan_in() -> u64 {
    let (tx, rx) = kanal::bounded_async::<MediaPacket>(CHANNEL_CAPACITY);
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
    drop(tx);

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
        "Kanal Fan-In: Packet count mismatch! Expected {}, got {}",
        total_expected_messages, received_count
    );
    received_count
}
