use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::{
    Gcc, GccOutcome, MAX_BITRATE_BPS, MIN_BITRATE_BPS, ProbeDecision, TwccFeedback, TwccStatus,
};
use crate::SendId;

const PACKET_BYTES: usize = 1_200;
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FeedbackImpairment {
    pub(super) drop_every: Option<u64>,
    pub(super) reverse_every: Option<u64>,
    pub(super) duplicate_every: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SimulationSummary {
    pub(super) estimates: Vec<(Duration, u64)>,
    pub(super) probes: Vec<(Duration, u32, u64)>,
    pub(super) acknowledged: u64,
    pub(super) lost: u64,
    pub(super) dropped_feedback: u64,
    pub(super) duplicate_feedback: u64,
    pub(super) max_queue_delay: Duration,
}

impl SimulationSummary {
    pub(super) fn last_estimate(&self) -> u64 {
        self.estimates
            .last()
            .map_or(MIN_BITRATE_BPS, |(_, estimate)| *estimate)
    }

    pub(super) fn minimum_estimate_since(&self, since: Duration) -> u64 {
        self.estimates
            .iter()
            .filter_map(|(at, estimate)| (*at >= since).then_some(*estimate))
            .min()
            .unwrap_or_else(|| self.last_estimate())
    }

    pub(super) fn maximum_estimate_since(&self, since: Duration) -> u64 {
        self.estimates
            .iter()
            .filter_map(|(at, estimate)| (*at >= since).then_some(*estimate))
            .max()
            .unwrap_or_else(|| self.last_estimate())
    }
}

#[derive(Clone, Copy)]
struct ActiveProbe {
    decision: ProbeDecision,
    sent: u8,
    first_departure: Option<Instant>,
}

pub(super) struct GccSimulation {
    gcc: Gcc,
    epoch: Instant,
    now: Instant,
    next_departure: Instant,
    link_available: Duration,
    feedback: VecDeque<(Instant, Vec<TwccStatus>)>,
    capacity_bps: u64,
    propagation: Duration,
    loss_every: Option<u64>,
    impairment: FeedbackImpairment,
    next_send_id: u64,
    packet_count: u64,
    feedback_count: u64,
    active_probe: Option<ActiveProbe>,
    summary: SimulationSummary,
}

impl GccSimulation {
    pub(super) fn new(capacity_bps: u64) -> Self {
        debug_assert!(capacity_bps > 0);
        let epoch = Instant::now();
        let mut gcc = Gcc::new(8_192);
        let initial = gcc.start(epoch);
        let mut simulation = Self {
            gcc,
            epoch,
            now: epoch,
            next_departure: epoch,
            link_available: Duration::ZERO,
            feedback: VecDeque::new(),
            capacity_bps,
            propagation: Duration::from_millis(20),
            loss_every: None,
            impairment: FeedbackImpairment::default(),
            next_send_id: 1,
            packet_count: 0,
            feedback_count: 0,
            active_probe: None,
            summary: SimulationSummary {
                estimates: Vec::new(),
                probes: Vec::new(),
                acknowledged: 0,
                lost: 0,
                dropped_feedback: 0,
                duplicate_feedback: 0,
                max_queue_delay: Duration::ZERO,
            },
        };
        simulation.apply(initial);
        simulation
    }

    pub(super) fn with_loss_every(mut self, every: u64) -> Self {
        debug_assert!(every > 0);
        self.loss_every = Some(every.max(1));
        self
    }

    pub(super) fn with_feedback_impairment(mut self, impairment: FeedbackImpairment) -> Self {
        debug_assert!(impairment.drop_every != Some(0));
        debug_assert!(impairment.reverse_every != Some(0));
        debug_assert!(impairment.duplicate_every != Some(0));
        self.impairment = impairment;
        self
    }

    pub(super) fn set_capacity(&mut self, capacity_bps: u64) {
        debug_assert!(capacity_bps > 0);
        self.capacity_bps = capacity_bps.max(1);
    }

    pub(super) fn run_for(&mut self, duration: Duration, demand_bps: u64) {
        debug_assert!(!duration.is_zero());
        debug_assert!(demand_bps > 0);
        let end = self.now.checked_add(duration).unwrap_or(self.now);
        while self.now < end {
            let feedback_at = self.feedback.front().map(|(at, _)| *at);
            let timeout_at = self.gcc.next_deadline(self.now);
            let next_event = [
                Some(self.next_departure),
                feedback_at,
                timeout_at,
                Some(end),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(end);
            if next_event >= end {
                self.now = end;
                break;
            }
            if feedback_at == Some(next_event) {
                self.process_feedback();
            } else if timeout_at == Some(next_event) {
                self.now = next_event;
                let Some(outcome) = self.gcc.handle_timeout(self.now) else {
                    debug_assert!(false, "a selected GCC timeout deadline must be due");
                    break;
                };
                self.apply(outcome);
            } else {
                self.send_packet(demand_bps);
            }
        }
    }

    pub(super) fn run_feedback_outage_for(&mut self, duration: Duration, demand_bps: u64) {
        let previous = self.impairment.drop_every;
        self.impairment.drop_every = Some(1);
        self.run_for(duration, demand_bps);
        self.impairment.drop_every = previous;
    }

    pub(super) fn summary(&self) -> &SimulationSummary {
        &self.summary
    }

    fn send_packet(&mut self, demand_bps: u64) {
        self.now = self.next_departure;
        let send_rate = self.active_probe.map_or_else(
            || self.gcc.estimate(self.now).bitrate_bps().min(demand_bps),
            |probe| probe.decision.target_bitrate_bps(),
        );
        let send_rate = send_rate.max(MIN_BITRATE_BPS);
        let send_id = SendId::new(self.next_send_id);
        self.next_send_id = self.next_send_id.saturating_add(1);
        let assigned = match self.active_probe {
            Some(probe) => self
                .gcc
                .assign_probe(send_id, PACKET_BYTES, probe.decision.id())
                .expect("simulation send identity is unique"),
            None => self
                .gcc
                .assign(send_id, PACKET_BYTES)
                .expect("simulation send identity is unique"),
        };
        self.gcc
            .record_departure(send_id, self.now)
            .expect("simulation records one departure per send");

        let departure = self.now.duration_since(self.epoch);
        let serialization_start = departure.max(self.link_available);
        self.summary.max_queue_delay = self
            .summary
            .max_queue_delay
            .max(serialization_start.saturating_sub(departure));
        self.link_available =
            serialization_start.saturating_add(duration_for_bytes(PACKET_BYTES, self.capacity_bps));
        let arrival = self.link_available.saturating_add(self.propagation);
        self.packet_count = self.packet_count.saturating_add(1);
        let lost = self
            .loss_every
            .is_some_and(|every| self.packet_count.is_multiple_of(every));
        let status = TwccStatus {
            sequence: assigned.transport_sequence(),
            received_at: (!lost).then_some(arrival),
        };
        self.enqueue_feedback(arrival, status);
        self.advance_probe(self.now);
        self.next_departure = self
            .now
            .checked_add(duration_for_bytes(PACKET_BYTES, send_rate))
            .unwrap_or(self.now);
    }

    fn enqueue_feedback(&mut self, arrival: Duration, status: TwccStatus) {
        let return_at = arrival.saturating_add(self.propagation);
        let interval_micros = u64::try_from(FEEDBACK_INTERVAL.as_micros()).unwrap_or(u64::MAX);
        let return_micros = u64::try_from(return_at.as_micros()).unwrap_or(u64::MAX);
        let rounded_micros = return_micros
            .saturating_add(interval_micros.saturating_sub(1))
            .checked_div(interval_micros)
            .unwrap_or_default()
            .saturating_mul(interval_micros);
        let due = self
            .epoch
            .checked_add(Duration::from_micros(rounded_micros))
            .unwrap_or(self.now);
        if let Some((last_due, statuses)) = self.feedback.back_mut()
            && *last_due == due
        {
            statuses.push(status);
            return;
        }
        debug_assert!(
            self.feedback
                .back()
                .is_none_or(|(last_due, _)| *last_due < due)
        );
        self.feedback.push_back((due, vec![status]));
    }

    fn process_feedback(&mut self) {
        let Some((at, mut statuses)) = self.feedback.pop_front() else {
            debug_assert!(false, "a feedback event must have statuses");
            return;
        };
        debug_assert!(at >= self.now);
        self.now = at;
        self.feedback_count = self.feedback_count.saturating_add(1);
        if self
            .impairment
            .drop_every
            .is_some_and(|every| self.feedback_count.is_multiple_of(every))
        {
            self.summary.dropped_feedback = self.summary.dropped_feedback.saturating_add(1);
            return;
        }
        if self
            .impairment
            .reverse_every
            .is_some_and(|every| self.feedback_count.is_multiple_of(every))
        {
            statuses.reverse();
        }
        let feedback = TwccFeedback {
            statuses: statuses.into_boxed_slice(),
        };
        let outcome = self.gcc.process_feedback(self.now, &feedback);
        self.apply(outcome);
        if self
            .impairment
            .duplicate_every
            .is_some_and(|every| self.feedback_count.is_multiple_of(every))
        {
            let duplicate = self.gcc.process_feedback(self.now, &feedback);
            debug_assert_eq!(duplicate.acknowledged(), 0);
            debug_assert_eq!(duplicate.lost(), 0);
            self.summary.duplicate_feedback = self.summary.duplicate_feedback.saturating_add(1);
            self.apply(duplicate);
        }
    }

    fn advance_probe(&mut self, departed_at: Instant) {
        let Some(probe) = self.active_probe.as_mut() else {
            return;
        };
        let first_departure = *probe.first_departure.get_or_insert(departed_at);
        probe.sent = probe.sent.saturating_add(1);
        if probe.sent >= probe.decision.packet_count()
            && departed_at.saturating_duration_since(first_departure)
                >= probe.decision.min_duration()
        {
            self.gcc.complete_probe(probe.decision.id());
            self.active_probe = None;
        }
    }

    fn apply(&mut self, outcome: GccOutcome) {
        let at = self.now.saturating_duration_since(self.epoch);
        let estimate = outcome.estimate().bitrate_bps();
        debug_assert!((MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&estimate));
        self.summary.estimates.push((at, estimate));
        self.summary.acknowledged = self
            .summary
            .acknowledged
            .saturating_add(u64::try_from(outcome.acknowledged()).unwrap_or(u64::MAX));
        self.summary.lost = self
            .summary
            .lost
            .saturating_add(u64::try_from(outcome.lost()).unwrap_or(u64::MAX));
        if let Some(decision) = outcome.probe() {
            debug_assert!(self.active_probe.is_none());
            debug_assert!(decision.target_bitrate_bps() >= estimate);
            debug_assert!(decision.packet_count() > 0);
            debug_assert!(!decision.min_duration().is_zero());
            self.summary
                .probes
                .push((at, decision.id(), decision.target_bitrate_bps()));
            self.active_probe = Some(ActiveProbe {
                decision,
                sent: 0,
                first_departure: None,
            });
        }
    }
}

fn duration_for_bytes(bytes: usize, bitrate_bps: u64) -> Duration {
    debug_assert!(bytes > 0);
    debug_assert!(bitrate_bps > 0);
    let numerator = u128::try_from(bytes)
        .unwrap_or(u128::MAX)
        .saturating_mul(8_000_000)
        .saturating_add(u128::from(bitrate_bps.saturating_sub(1)));
    let micros = numerator
        .checked_div(u128::from(bitrate_bps))
        .unwrap_or_default();
    Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX).max(1))
}
