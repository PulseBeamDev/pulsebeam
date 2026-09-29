#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "bounded benchmark matrix and fixture invariants"
)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use pulsebeam_rtc::{Command, CommandError, GlobalMediaTime, MediaPacket, Output, TimePoint};
use support::{PeerFixture, forwarded};

const DURATION: Duration = Duration::from_secs(30);
const QUANTUM: Duration = Duration::from_millis(1);
const CONNECTION_COUNTS: [usize; 3] = [1, 16, 64];
const SENDER_COUNTS: [usize; 3] = [1, 8, 32];

fn many_connections_many_streams(criterion: &mut Criterion) {
    let mut source = PeerFixture::connected();
    let packet = source.send_source(&vec![0x5a; 1_000]);
    let mut group = criterion.benchmark_group("many_connections_many_streams");
    group.sample_size(10);
    for connection_count in CONNECTION_COUNTS {
        for sender_count in SENDER_COUNTS {
            group.bench_function(
                BenchmarkId::new(
                    format!("connections_{connection_count}"),
                    format!("senders_{sender_count}"),
                ),
                |bencher| {
                    bencher.iter_batched_ref(
                        || ScalingCell::new(connection_count, sender_count, &packet),
                        |cell| black_box(cell.run()),
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }
    group.finish();
}

struct ScalingCell {
    connections: Vec<PeerFixture>,
    start: Instant,
    start_global: u64,
    due: BinaryHeap<Reverse<(Instant, usize)>>,
    pending: Vec<bool>,
}

impl ScalingCell {
    fn new(connection_count: usize, sender_count: usize, packet: &MediaPacket) -> Self {
        let mut connections = (0..connection_count)
            .map(|_| PeerFixture::connected_with_senders(sender_count))
            .collect::<Vec<_>>();
        for fixture in &mut connections {
            for (index, sender) in fixture.senders.clone().into_iter().enumerate() {
                match fixture.connection.command(
                    fixture.at(),
                    Command::SendMedia {
                        sender,
                        media: forwarded(
                            packet.clone(),
                            u64::try_from(index).expect("bounded sender index") + 1,
                        ),
                    },
                ) {
                    Ok(()) | Err(CommandError::WouldBlock) => {}
                    Err(error) => panic!("unexpected media admission error: {error:?}"),
                }
            }
        }
        let start = connections
            .iter()
            .map(|fixture| fixture.at().monotonic)
            .max()
            .expect("nonempty scaling cell");
        let start_global = connections
            .iter()
            .map(|fixture| fixture.at().global.as_micros())
            .max()
            .expect("nonempty scaling cell");
        let mut cell = Self {
            connections,
            start,
            start_global,
            due: BinaryHeap::with_capacity(connection_count),
            pending: vec![false; connection_count],
        };
        for index in 0..connection_count {
            cell.schedule(start, index);
        }
        cell
    }

    fn run(&mut self) -> u64 {
        let end = self.start + DURATION;
        let mut polls = 0_u64;
        while let Some(Reverse((at, index))) = self.due.pop() {
            if at > end {
                break;
            }
            self.pending[index] = false;
            polls = polls.saturating_add(1);
            let now = TimePoint {
                monotonic: at,
                global: GlobalMediaTime::from_micros(self.start_global.saturating_add(
                    u64::try_from(at.duration_since(self.start).as_micros()).unwrap_or(u64::MAX),
                )),
            };
            match black_box(self.connections[index].connection.poll(now)) {
                Output::Idle {
                    next_wakeup: Some(next),
                } => self.schedule(next.max(at + QUANTUM), index),
                Output::Idle { next_wakeup: None } | Output::Closed(_) => {}
                Output::Transmit(_) | Output::Event(_) => self.schedule(at, index),
                _ => panic!("unreviewed future output"),
            }
        }
        polls
    }

    fn schedule(&mut self, at: Instant, index: usize) {
        assert!(
            !self.pending[index],
            "at most one pending wakeup per connection"
        );
        self.pending[index] = true;
        self.due.push(Reverse((at, index)));
    }
}

criterion_group!(benches, many_connections_many_streams);
criterion_main!(benches);
