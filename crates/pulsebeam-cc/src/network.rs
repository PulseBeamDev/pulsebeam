#![allow(
    clippy::arithmetic_side_effects,
    reason = "bounded deterministic virtual-link fixture arithmetic"
)]

use super::*;

#[path = "network_high_rate.rs"]
mod high_rate;
use std::collections::{BTreeMap, VecDeque};

const HISTORY: usize = 256;
const QUEUE: usize = 64;
const STEP: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Original(u8),
    Repair,
    Padding,
}

struct Entry {
    epoch: u64,
    sent: Duration,
    receipt: Option<Duration>,
    bytes: u32,
    ecn: Option<EcnMark>,
    retired: bool,
    loss_reported: bool,
}

#[derive(Clone, Copy)]
struct Link {
    capacity: u64,
    rtt: Duration,
    jitter: bool,
    loss: bool,
    reordered: bool,
    feedback: bool,
    ce: bool,
    ecn: EcnValidation,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            capacity: 4_000_000,
            rtt: Duration::from_millis(20),
            jitter: false,
            loss: false,
            reordered: false,
            feedback: true,
            ce: false,
            ecn: EcnValidation::default(),
        }
    }
}

struct Host {
    cc: Controller,
    epoch: u64,
    available: bool,
    now: Duration,
    source_credit: u128,
    queue: VecDeque<(Duration, Kind, usize)>,
    ledger: BTreeMap<u64, Entry>,
    feedback: Vec<FeedbackSample>,
    report_interval_ms: u128,
    replay_reports: bool,
    credit: u128,
    discarded_flight: u128,
    highest_receipt: u64,
    reordered_receipts: u64,
    max_batch: usize,
    saw_alr: bool,
    next_id: u64,
    flight: u64,
    serial_end: Duration,
    accepted: [u64; 3],
    streams: [u64; 2],
    wire: u128,
    received: u64,
    recovered: u64,
    losses: u64,
    ignored_old_path: u64,
    source: u64,
    demand: u64,
    fail_padding: bool,
    max_target: u64,
}

impl Host {
    fn new() -> Self {
        Self {
            cc: Controller::new(Config::default()).unwrap(),
            epoch: 1,
            available: true,
            now: Duration::ZERO,
            source_credit: 0,
            queue: VecDeque::with_capacity(QUEUE),
            ledger: BTreeMap::new(),
            feedback: Vec::with_capacity(HISTORY),
            report_interval_ms: 10,
            replay_reports: false,
            credit: 0,
            discarded_flight: 0,
            highest_receipt: 0,
            reordered_receipts: 0,
            max_batch: 0,
            saw_alr: false,
            next_id: 0,
            flight: 0,
            serial_end: Duration::ZERO,
            accepted: [0; 3],
            streams: [0; 2],
            wire: 0,
            received: 0,
            recovered: 0,
            losses: 0,
            ignored_old_path: 0,
            source: 8_000_000,
            demand: 8_000_000,
            fail_padding: false,
            max_target: 0,
        }
    }

    fn observe(&mut self, link: Link) {
        self.cc
            .update(
                self.now,
                Observation {
                    path_epoch: Some(self.epoch),
                    path_available: self.available,
                    feedback: &self.feedback,
                    feedback_hold: Duration::ZERO,
                    fresh_network_feedback: !self.feedback.is_empty(),
                    bytes_in_flight: self.flight,
                    paced_queue_bytes: self.queue.iter().map(|(_, _, bytes)| *bytes as u64).sum(),
                    desired_media_rate: self.demand,
                    ecn: link.ecn,
                },
            )
            .unwrap();
        self.saw_alr |= self.cc.snapshot().application_limited;
        self.max_target = self
            .max_target
            .max(self.cc.snapshot().target_media_payload_rate);
        self.feedback.clear();
    }

    fn new_path(&mut self, link: Link) {
        self.epoch += 1;
        self.discarded_flight += u128::from(self.flight);
        self.flight = 0;
        self.feedback.clear();
        self.serial_end = self.now;
        self.observe(link);
    }

    fn report(&mut self, link: Link) {
        if !link.feedback {
            return;
        }
        let credit_before = self.credit;
        let mut remove = Vec::new();
        for (&id, entry) in &mut self.ledger {
            let receipt = entry
                .receipt
                .filter(|receipt| *receipt + link.rtt / 2 <= self.now);
            if entry.epoch != self.epoch {
                if receipt.is_some() || self.now >= entry.sent + Duration::from_millis(200) {
                    self.ignored_old_path += 1;
                    remove.push(id);
                }
                continue;
            }
            if let Some(arrival) = receipt {
                if !entry.retired {
                    self.flight -= u64::from(entry.bytes);
                    self.credit += u128::from(entry.bytes);
                }
                self.reordered_receipts += u64::from(id < self.highest_receipt);
                self.highest_receipt = self.highest_receipt.max(id);
                self.feedback.push(FeedbackSample {
                    sent_at: entry.sent,
                    received_at: self.now,
                    transport_bytes: entry.bytes,
                    received: true,
                    newly_acked: !entry.retired,
                    lost: false,
                    recovered: entry.loss_reported,
                    receiver_arrival_micros: Some(
                        i64::try_from(arrival.as_micros()).unwrap() - 9_000_000,
                    ),
                    ecn: entry.ecn,
                });
                self.received += u64::from(entry.bytes);
                self.recovered += u64::from(entry.loss_reported);
                remove.push(id);
            } else if !entry.loss_reported
                && self.now >= entry.sent + link.rtt + Duration::from_millis(40)
            {
                entry.retired = true;
                entry.loss_reported = true;
                self.flight -= u64::from(entry.bytes);
                self.credit += u128::from(entry.bytes);
                self.losses += 1;
                self.feedback.push(FeedbackSample {
                    sent_at: entry.sent,
                    received_at: self.now,
                    transport_bytes: entry.bytes,
                    received: false,
                    newly_acked: true,
                    lost: true,
                    recovered: false,
                    receiver_arrival_micros: None,
                    ecn: None,
                });
                if self.queue.len() == QUEUE {
                    self.queue.pop_back();
                }
                self.queue.push_front((self.now, Kind::Repair, 1200));
            } else if entry.retired && entry.receipt.is_none() {
                remove.push(id);
            }
        }
        for id in remove {
            self.ledger.remove(&id);
        }
        assert!(self.feedback.len() <= HISTORY);
        self.max_batch = self.max_batch.max(self.feedback.len());
        let batch_credit: u128 = self
            .feedback
            .iter()
            .filter(|sample| sample.newly_acked)
            .map(|sample| u128::from(sample.transport_bytes))
            .sum();
        assert_eq!(self.credit - credit_before, batch_credit);
        // Replaying a report is absorbed by the host's identity ledger: removed
        // receipts and first-loss flags cannot emit a second normalized credit.
    }

    fn accept(&mut self, kind: Kind, bytes: usize, link: Link) -> bool {
        if self.ledger.len() == HISTORY
            || self.cc.ready(self.now, self.flight, bytes) != Readiness::Ready
        {
            return false;
        }
        let wire = u32::try_from(bytes).unwrap();
        self.flight = self.flight.checked_add(u64::from(wire)).unwrap();
        assert!(self.flight <= self.cc.snapshot().max_rtp_bytes_in_flight);
        self.cc
            .commit_send(self.now, bytes, self.flight, kind == Kind::Padding);
        self.wire += bytes as u128;
        self.accepted[match kind {
            Kind::Original(stream) => {
                self.streams[usize::from(stream)] += 1;
                0
            }
            Kind::Repair => 1,
            Kind::Padding => 2,
        }] += 1;
        self.next_id += 1;
        let service = Duration::from_nanos(
            u64::try_from((bytes as u128 * 8_000_000_000).div_ceil(u128::from(link.capacity)))
                .unwrap(),
        );
        self.serial_end = self.serial_end.max(self.now) + service;
        let jitter = if link.jitter && self.next_id.is_multiple_of(3) {
            Duration::from_millis(3)
        } else {
            Duration::ZERO
        };
        let reorder = if link.reordered && self.next_id.is_multiple_of(11) {
            Duration::from_millis(100)
        } else {
            Duration::ZERO
        };
        let dropped = link.loss && self.next_id.is_multiple_of(13);
        self.ledger.insert(
            self.next_id,
            Entry {
                epoch: self.epoch,
                sent: self.now,
                receipt: (!dropped).then_some(self.serial_end + link.rtt / 2 + jitter + reorder),
                bytes: wire,
                ecn: Some(if link.ce && self.next_id.is_multiple_of(4) {
                    EcnMark::Ce
                } else {
                    EcnMark::Ect1
                }),
                retired: false,
                loss_reported: false,
            },
        );
        assert_eq!(self.cc.diagnostics().charged_bytes, self.wire);
        true
    }

    fn step(&mut self, link: Link) {
        self.now += STEP;
        self.source_credit += u128::from(self.source) * STEP.as_nanos();
        for stream in 0..QUEUE {
            let bytes = if stream % 2 == 0 { 1200 } else { 257 };
            let cost = bytes as u128 * 8_000_000_000;
            if self.source_credit < cost || self.queue.len() == QUEUE {
                break;
            }
            self.source_credit -= cost;
            self.queue.push_back((
                self.now,
                Kind::Original(u8::try_from(stream % 2).unwrap()),
                bytes,
            ));
        }
        // Bounded host admission drops excess offered work, not a CC target-rate throttle.
        self.source_credit %= 1200 * 8_000_000_000_u128;
        if self.now.as_millis().is_multiple_of(self.report_interval_ms) {
            self.report(link);
        }
        self.observe(link);
        if self.replay_reports && self.now.as_millis().is_multiple_of(self.report_interval_ms) {
            let window = self.cc.snapshot().reference_window;
            let credit = self.credit;
            self.report(link);
            assert!(
                self.feedback.is_empty(),
                "duplicate callback has no normalized credit"
            );
            assert_eq!(self.credit, credit);
            self.observe(link);
            assert_eq!(self.cc.snapshot().reference_window, window);
        }
        let queue_bytes = self.queue.iter().map(|(_, _, bytes)| *bytes as u64).sum();
        let age = self
            .queue
            .front()
            .map_or(Duration::ZERO, |(at, _, _)| self.now - *at);
        self.cc.update_queue(queue_bytes, age);
        for _ in 0..QUEUE {
            let Some(&(_, kind, bytes)) = self.queue.front() else {
                break;
            };
            if !self.accept(kind, bytes, link) {
                break;
            }
            self.queue.pop_front();
        }
        if self.queue.is_empty()
            && let Some(requested) = self.cc.prepare_padding(self.now)
        {
            if self.fail_padding {
                self.cc.padding_unavailable();
            } else {
                self.accept(Kind::Padding, requested.clamp(1, 255) + 32, link);
            }
        }
        self.cc.process_idle(self.now);
        assert!(self.queue.len() <= QUEUE && self.ledger.len() <= HISTORY);
        let live: u64 = self
            .ledger
            .values()
            .filter(|entry| entry.epoch == self.epoch && !entry.retired)
            .map(|entry| u64::from(entry.bytes))
            .sum();
        assert_eq!(self.flight, live);
        assert_eq!(
            self.wire,
            self.credit + self.discarded_flight + u128::from(self.flight)
        );
        assert_eq!(self.cc.diagnostics().charged_bytes, self.wire);
    }

    fn run(&mut self, milliseconds: usize, link: Link) {
        for _ in 0..milliseconds {
            self.step(link);
        }
    }
}

#[test]
fn real_controller_virtual_bottleneck_phases_preserve_exact_credit_and_independence() {
    let mut forward = Host::new();
    let mut reverse = Host::new();
    let mut link = Link::default();
    forward.replay_reports = true;
    for _ in 0..200 {
        forward.step(link);
        reverse.step(link);
    }
    assert!(forward.wire > 0 && reverse.wire > 0);
    forward.run(1300, link);
    assert!(forward.received > 0 && forward.accepted[0] > 0);
    assert!(
        forward.streams.iter().all(|accepted| *accepted > 0),
        "both streams share the same window and pacer"
    );
    link.capacity = 1_000_000;
    link.rtt = Duration::from_millis(40);
    link.jitter = true;
    link.loss = true;
    link.reordered = true;
    forward.run(1500, link);
    assert!(
        forward.losses > 0 && forward.recovered > 0 && forward.accepted[1] > 0,
        "losses={} recovered={} accepted={:?}",
        forward.losses,
        forward.recovered,
        forward.accepted
    );
    link.ecn = EcnValidation {
        classic: true,
        ..EcnValidation::default()
    };
    link.ce = true;
    forward.run(300, link);
    assert!(!forward.cc.snapshot().l4s_enabled);
    link.ecn = EcnValidation {
        l4s: true,
        ..EcnValidation::default()
    };
    forward.run(300, link);
    assert!(forward.cc.snapshot().l4s_enabled);
    link.ecn.bleached = true;
    forward.run(300, link);
    assert!(!forward.cc.snapshot().l4s_enabled);

    assert!(forward.reordered_receipts > 0 && forward.max_batch > 1);
    forward.report_interval_ms = 50;
    forward.run(200, link);
    forward.report_interval_ms = 10;
    link.feedback = false;
    forward.run(1000, link);
    assert!(forward.cc.snapshot().feedback_stale);
    let stale = forward.cc.snapshot().reference_window;
    forward.run(100, link);
    assert_eq!(forward.cc.snapshot().reference_window, stale);
    let before_outage = forward.wire;
    forward.available = false;
    for _ in 0..200 {
        forward.step(link);
        reverse.step(Link::default());
    }
    assert_eq!(forward.wire, before_outage);
    assert!(
        reverse.wire > 0,
        "independent reverse direction progresses during outage"
    );
    forward.available = true;
    link.feedback = true;
    forward.run(300, link);
    assert!(!forward.cc.snapshot().feedback_stale);
    let target = forward.cc.snapshot().target_media_payload_rate;
    let reverse_snapshot = reverse.cc.snapshot();
    forward.source = 0;
    forward.demand = 0;
    forward.queue.clear();
    forward.new_path(link);
    assert_eq!(reverse.cc.snapshot(), reverse_snapshot);
    assert_eq!(forward.cc.snapshot().target_media_payload_rate, target);
    assert_eq!(forward.cc.snapshot().reference_window, 1000);
    forward.run(300, link);
    assert!(forward.ignored_old_path > 0);
    assert_eq!(
        forward.cc.snapshot().reference_window,
        1000,
        "old-path receipts cannot grow the new window"
    );
    assert_eq!(forward.cc.snapshot().target_media_payload_rate, target);
    forward.demand = 8_000_000;
    forward.source = 0;
    forward.run(1500, Link::default());
    assert!(
        forward.accepted.iter().all(|count| *count > 0),
        "originals, repairs and padding share one CC budget"
    );

    let mut sparse = Host::new();
    sparse.run(1500, Link::default());
    sparse.queue.clear();
    sparse.source = 64_000;
    sparse.demand = 128_000;
    sparse.run(1500, Link::default());
    assert!(
        sparse.received > 0 && sparse.saw_alr,
        "sparse real receipts establish ALR"
    );

    let mut padding = Host::new();
    padding.source = 0;
    padding.run(500, Link::default());
    assert!(padding.accepted[2] > 0 && padding.accepted[0] == 0);
    padding.demand = 0;
    let stopped = padding.wire;
    padding.run(100, Link::default());
    assert_eq!(
        padding.wire, stopped,
        "absent demand disables speculative padding"
    );
    padding.demand = 1_000_000;
    padding.fail_padding = true;
    padding.run(100, Link::default());
    assert_eq!(
        padding.wire, stopped,
        "failed padding generation is not a wire departure"
    );
}
