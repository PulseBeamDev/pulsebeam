#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::print_stderr,
    reason = "the deterministic scaling matrix reports every seeded cell and stops on unknown output"
)]

mod support;

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    time::{Duration, Instant},
};

use pulsebeam_rtc::{Command, GlobalMediaTime, Output, TimePoint};
use support::{PeerFixture, forwarded};

const SEED: u64 = 0x11ff;
const DURATION: Duration = Duration::from_secs(30);
const QUANTUM: Duration = Duration::from_millis(1);
const CONNECTION_COUNTS: [usize; 3] = [1, 16, 64];
const SENDER_COUNTS: [usize; 3] = [1, 8, 32];

#[test]
#[ignore = "fixed 243-connection deterministic scaling gate"]
fn many_connections_many_streams() {
    let mut source = PeerFixture::connected();
    let packet = source.send_source(&vec![0x5a; 1_000]);
    for connections in CONNECTION_COUNTS {
        for senders in SENDER_COUNTS {
            run_cell(connections, senders, &packet);
        }
    }
}

fn run_cell(connection_count: usize, sender_count: usize, packet: &pulsebeam_rtc::MediaPacket) {
    let mut connections = (0..connection_count)
        .map(|_| PeerFixture::connected_with_senders(sender_count))
        .collect::<Vec<_>>();
    let mut admitted = 0_u64;
    for fixture in &mut connections {
        for (index, sender) in fixture.senders.clone().into_iter().enumerate() {
            if fixture
                .connection
                .command(
                    fixture.at(),
                    Command::SendMedia {
                        sender,
                        media: forwarded(
                            packet.clone(),
                            u64::try_from(index).expect("bounded sender index") + 1,
                        ),
                    },
                )
                .is_ok()
            {
                admitted = admitted.saturating_add(1);
            }
        }
    }

    let start = connections
        .iter()
        .map(PeerFixture::at)
        .map(|at| at.monotonic)
        .max()
        .unwrap_or_else(Instant::now);
    let start_global = connections
        .iter()
        .map(PeerFixture::at)
        .map(|at| at.global.as_micros())
        .max()
        .unwrap_or(1_000_000);
    let end = start + DURATION;
    let mut due = BinaryHeap::new();
    let mut pending = vec![false; connection_count];
    let mut poll_work = Vec::new();
    let mut wakeups = 0_u64;
    let mut popped = 0_u64;

    for index in 0..connection_count {
        schedule(&mut due, &mut pending, start, index);
    }

    while let Some(Reverse((at, index))) = due.pop() {
        if at > end {
            break;
        }
        pending[index] = false;
        popped = popped.saturating_add(1);
        let now = TimePoint {
            monotonic: at,
            global: GlobalMediaTime::from_micros(start_global.saturating_add(
                u64::try_from(at.duration_since(start).as_micros()).unwrap_or(u64::MAX),
            )),
        };
        let output = connections[index].connection.poll(now);
        poll_work.push(1_u64);
        match output {
            Output::Idle {
                next_wakeup: Some(next),
            } => {
                wakeups = wakeups.saturating_add(1);
                schedule(&mut due, &mut pending, next.max(at + QUANTUM), index);
            }
            Output::Idle { next_wakeup: None } | Output::Closed(_) => {}
            Output::Transmit(_) | Output::Event(_) => schedule(&mut due, &mut pending, at, index),
            _ => panic!("unreviewed future output"),
        }
        assert!(pending.iter().filter(|value| **value).count() <= connection_count);
    }

    poll_work.sort_unstable();
    let snapshots = connections
        .iter()
        .map(|fixture| fixture.connection.stats())
        .collect::<Vec<_>>();
    let queue_high_water = snapshots
        .iter()
        .map(|stats| stats.connection.queued_media_bytes)
        .max()
        .unwrap_or(0);
    let transmitted = snapshots
        .iter()
        .flat_map(|stats| &stats.senders)
        .map(|sender| sender.transmitted_packets)
        .sum::<u64>();
    let utilization = if admitted == 0 {
        0
    } else {
        transmitted.saturating_mul(100) / admitted
    };
    let p50 = percentile(&poll_work, 50);
    let p95 = percentile(&poll_work, 95);
    let p99 = percentile(&poll_work, 99);
    assert_eq!((p50, p95, p99), (1, 1, 1));
    assert!(queue_high_water <= 8 * 1024 * 1024);
    assert!(
        snapshots
            .iter()
            .all(|stats| stats.senders.len() == sender_count)
    );
    assert_eq!(popped, u64::try_from(poll_work.len()).expect("work count"));
    assert!(utilization >= 85);

    eprintln!(
        "cell=connections:{connection_count},senders:{sender_count} seed={SEED:#06x} duration={DURATION:?} quantum={QUANTUM:?} path=4Mbit/50ms p50_work={p50} p95_work={p95} p99_work={p99} wakeups={wakeups} queue_bytes_high_water={queue_high_water} utilization_percent={utilization} popped_due={popped}"
    );
}

fn schedule(
    due: &mut BinaryHeap<Reverse<(Instant, usize)>>,
    pending: &mut [bool],
    at: Instant,
    index: usize,
) {
    assert!(!pending[index], "at most one pending wakeup per connection");
    pending[index] = true;
    due.push(Reverse((at, index)));
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let index = sorted.len().saturating_sub(1).saturating_mul(percentile) / 100;
    sorted.get(index).copied().unwrap_or(0)
}
