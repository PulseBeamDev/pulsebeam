use criterion::{Criterion, criterion_group, criterion_main};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::task;

use pulsebeam_runtime::sync::spmc::channel; // your broadcast ring crate/module

/// Benchmark broadcast throughput and per-packet latency for 6,000 subscribers
fn bench_broadcast_6000(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("broadcast_6000_subs", |b| {
        b.to_async(&rt).iter(|| async {
            // 1. Create the channel
            let capacity = 128;
            let (tx, rx) = channel::<usize>(capacity);

            let num_subscribers = 6000;
            let num_packets = 100;

            // 2. Spawn subscribers
            let mut subscribers = Vec::with_capacity(num_subscribers);
            for _ in 0..num_subscribers {
                let mut r = rx.clone();
                let handle = task::spawn(async move {
                    let mut last_received_time = Vec::with_capacity(num_packets);
                    loop {
                        let start = Instant::now();
                        let pkt = r.recv().await.unwrap();
                        let elapsed = start.elapsed();
                        last_received_time.push(elapsed);
                        if *pkt == num_packets-1 {
                            break;
                        }
                    }
                    last_received_time
                });
                subscribers.push(handle);
            }

            // 3. Publisher sends packets and measures latency
            let mut send_times = Vec::with_capacity(num_packets);
            for pkt_id in 0..num_packets {
                let now = Instant::now();
                tx.try_send(Arc::new(pkt_id)).unwrap();
                tx.notify_subscribers();
                send_times.push(now);

                if pkt_id % 8 == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }

            // 4. Wait for all subscribers to finish
            let mut all_latency = Vec::with_capacity(num_packets);
            for sub in subscribers {
                let received_times = sub.await.unwrap();
                for (i, t) in received_times.into_iter().enumerate() {
                    if all_latency.len() <= i {
                        all_latency.push(t);
                    } else if t > all_latency[i] {
                        all_latency[i] = t; // max latency for this packet across all subscribers
                    }
                }
            }

            // 5. Compute throughput and average latency
            let total_duration = send_times.last().unwrap().elapsed();
            let throughput = (num_packets as f64 * num_subscribers as f64) / total_duration.as_secs_f64();

            let avg_latency: Duration = all_latency.iter().sum::<Duration>() / (num_packets as u32);

            println!(
                "Packets: {}, Subscribers: {}, Throughput: {:.0} pkt/sec, Avg per-packet latency: {:.2?}",
                num_packets, num_subscribers, throughput, avg_latency
            );
        });
    });
}

criterion_group!(benches, bench_broadcast_6000);
criterion_main!(benches);
