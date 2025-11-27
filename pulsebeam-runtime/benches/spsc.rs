use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tokio::runtime::Runtime;

// Adjust import path to your project
use pulsebeam_runtime::sync::spsc::{TrySendError, channel};

// SFU CONSTANTS
const BUFFER_SIZE: usize = 1024; // Typical RTP jitter buffer size
const PARTICIPANTS: usize = 10; // 1 Gateway -> 10 Viewers
const BATCH_SIZE: usize = 1000; // Packets per "Frame" or burst

// Simulate work done by the consumer (e.g., DTLS/SRTP encryption)
// 500 spin loops is roughly 1-2 microseconds on modern CPUs,
// which is a realistic minimal cost for packet processing.
fn simulate_encryption_work() {
    for _ in 0..500 {
        std::hint::spin_loop();
    }
}

fn bench_gateway_scatter(c: &mut Criterion) {
    let mut group = c.benchmark_group("gateway_scatter");
    let rt = Runtime::new().unwrap();

    // We measure "Packets Processed by Gateway per Second"
    // Total operations = (Batch Size) * (Participants)
    group.throughput(Throughput::Elements((BATCH_SIZE * PARTICIPANTS) as u64));

    // --- SCENARIO 1: YOUR SPSC ---
    group.bench_function("spsc", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let mut total_duration = Duration::new(0, 0);

            for _ in 0..iters {
                // 1. Setup Topology: 1 Gateway, 10 Participants
                let mut producers = Vec::with_capacity(PARTICIPANTS);
                let mut handles = Vec::with_capacity(PARTICIPANTS);

                for _ in 0..PARTICIPANTS {
                    let (p, mut c) = channel::<Vec<u8>>(BUFFER_SIZE);
                    producers.push(p);

                    // Spawn Participant Task (Consumer)
                    handles.push(tokio::spawn(async move {
                        while let Some(_pkt) = c.recv().await {
                            simulate_encryption_work();
                        }
                    }));
                }

                let start = std::time::Instant::now();
                let payload = vec![0u8; 1200]; // Standard MTU size

                // 2. Gateway Loop (The Hot Path)
                for _ in 0..BATCH_SIZE {
                    for p in &mut producers {
                        // SFU Logic: Try send, drop if full. Never await.
                        // We clone cheap refs or Arcs in reality, here we mock it.
                        match p.try_send(payload.clone()) {
                            Ok(_) => {}
                            Err(TrySendError::Full(_)) => {
                                // In real SFU, we log this as packet loss
                                black_box(1);
                            }
                            Err(_) => panic!("Closed"),
                        }
                    }
                    // Simulate network arrival interval (tiny yield)
                    // This prevents the producer from strictly dominating the CPU lock
                    tokio::task::yield_now().await;
                }

                total_duration += start.elapsed();

                // Teardown
                drop(producers);
                for h in handles {
                    h.await.unwrap();
                }
            }
            total_duration
        })
    });

    // --- SCENARIO 2: TOKIO MPSC ---
    group.bench_function("tokio_mpsc", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let mut total_duration = Duration::new(0, 0);

            for _ in 0..iters {
                let mut producers = Vec::with_capacity(PARTICIPANTS);
                let mut handles = Vec::with_capacity(PARTICIPANTS);

                for _ in 0..PARTICIPANTS {
                    let (p, mut c) = tokio::sync::mpsc::channel::<Vec<u8>>(BUFFER_SIZE);
                    producers.push(p);

                    handles.push(tokio::spawn(async move {
                        while let Some(_pkt) = c.recv().await {
                            simulate_encryption_work();
                        }
                    }));
                }

                let start = std::time::Instant::now();
                let payload = vec![0u8; 1200];

                for _ in 0..BATCH_SIZE {
                    for p in &mut producers {
                        // Tokio equivalent: try_send
                        match p.try_send(payload.clone()) {
                            Ok(_) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                black_box(1);
                            }
                            Err(_) => panic!("Closed"),
                        }
                    }
                    tokio::task::yield_now().await;
                }

                total_duration += start.elapsed();

                // Teardown
                drop(producers);
                for h in handles {
                    h.await.unwrap();
                }
            }
            total_duration
        })
    });

    group.finish();
}

criterion_group!(benches, bench_gateway_scatter);
criterion_main!(benches);
