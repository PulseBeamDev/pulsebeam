#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "bounded benchmark matrix and fixture invariants"
)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::{hint::black_box, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use pulsebeam_rtc::{Command, CommandError, MediaPacket, SenderId};
use support::{PeerFixture, forwarded};

const FRAMES: usize = 64;
const CAPTURE_INTERVAL: Duration = Duration::from_millis(20);
const SENDER_COUNTS: [usize; 3] = [1, 8, 32];
const BURST_DEPTHS: [usize; 2] = [1, 8];

fn active_media(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("active_media");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(1));
    for senders in SENDER_COUNTS {
        for burst in BURST_DEPTHS {
            report(senders, burst, ScalingCell::new(senders, burst).run());
            group.bench_function(
                BenchmarkId::new(format!("senders_{senders}"), format!("burst_{burst}")),
                |bencher| {
                    bencher.iter_batched_ref(
                        || ScalingCell::new(senders, burst),
                        |cell| black_box(cell.run()),
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }
    group.finish();
}

#[allow(
    clippy::print_stderr,
    reason = "untimed benchmark service and backlog evidence, not application logging"
)]
fn report(
    senders: usize,
    burst: usize,
    (admitted, blocked, originals, feedback, peak): (usize, usize, u64, usize, usize),
) {
    eprintln!(
        "active_media senders={senders} burst={burst}: admitted={admitted} blocked={blocked} originals={originals} feedback={feedback} peak_queue={peak}"
    );
}

struct ScalingCell {
    fixture: PeerFixture,
    senders: Vec<SenderId>,
    burst: usize,
    pending: Vec<(MediaPacket, u64)>,
    payload: Vec<u8>,
}

impl ScalingCell {
    fn new(senders: usize, burst: usize) -> Self {
        let mut fixture = PeerFixture::connected_with_senders(senders).without_egress_observation();
        fixture.configure_bottleneck(500_000);
        let senders = fixture.senders.clone();
        for sender in &senders {
            let mut assignment = fixture.assignment(*sender);
            assignment.delivery.max_media_age = Duration::from_secs(2);
            assignment.delivery.max_queue_residence = Duration::from_secs(2);
            fixture.command(Command::SetSenderAssignment {
                sender: *sender,
                assignment: Some(assignment),
            });
        }
        Self {
            fixture,
            senders,
            burst,
            pending: Vec::with_capacity(burst),
            payload: vec![0x5a; 160],
        }
    }

    // Includes native capture, transport pumping and authenticated feedback. The
    // immutable sample fanout is one publisher, not reset/replayed source state.
    fn run(&mut self) -> (usize, usize, u64, usize, usize) {
        let mut admitted = 0;
        let mut blocked = 0;
        let mut peak = 0;
        let feedback_before = self.fixture.twcc_sent();
        for frame in 0..FRAMES {
            let packet = self.fixture.send_source(&self.payload);
            self.pending
                .push((packet, u64::try_from(frame).expect("bounded frame") + 1));
            self.fixture.drive_for(CAPTURE_INTERVAL);
            if self.pending.len() != self.burst {
                continue;
            }
            for (packet, id) in self.pending.drain(..) {
                for sender in &self.senders {
                    match self.fixture.try_command(Command::SendMedia {
                        sender: *sender,
                        media: forwarded(packet.clone(), id),
                    }) {
                        Ok(()) => admitted += 1,
                        Err(pulsebeam_rtc::RejectedCommand {
                            reason: CommandError::WouldBlock,
                            ..
                        }) => blocked += 1,
                        Err(error) => panic!("unexpected admission error: {error:?}"),
                    }
                }
            }
            peak = peak.max(
                self.fixture
                    .connection
                    .stats()
                    .senders
                    .iter()
                    .map(|s| s.queued_packets)
                    .sum(),
            );
        }
        self.fixture.drive_for(Duration::from_millis(500));
        let originals = self
            .fixture
            .connection
            .stats()
            .senders
            .iter()
            .map(|s| s.transmitted_packets)
            .sum::<u64>();
        let feedback = self.fixture.twcc_sent() - feedback_before;
        assert!(admitted > self.senders.len());
        assert!(originals > u64::try_from(self.senders.len()).expect("bounded sender count"));
        assert!(feedback > 0);
        assert!(peak >= self.burst);
        (admitted, blocked, originals, feedback, peak)
    }
}

criterion_group!(benches, active_media);
criterion_main!(benches);
