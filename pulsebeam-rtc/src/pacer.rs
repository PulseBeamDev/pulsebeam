use std::time::{Duration, Instant};

const MIN_RATE_BPS: u64 = 30_000;
const MAX_BURST_BYTES: u64 = 1_600;
const MAX_DEBT_TIME: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PacingClass {
    Audio,
    Retransmission,
    Video,
    Padding,
}

impl PacingClass {
    pub const fn bypasses_wait(self) -> bool {
        matches!(self, Self::Audio)
    }
}

pub struct PacketPacer {
    rate_bps: u64,
    probe: Option<ActiveProbe>,
    media_debt_bytes: u64,
    padding_debt_bytes: u64,
    last_updated: Instant,
}

#[derive(Clone, Copy)]
struct ActiveProbe {
    id: u32,
    rate_bps: u64,
    packets_remaining: u8,
    bytes_sent: u64,
    target_bytes: u64,
    first_departure_at: Option<Instant>,
    next_departure_at: Instant,
    min_duration: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacerDecision {
    Admitted {
        eligible_at: Instant,
        probe_id: Option<u32>,
        probe_complete: bool,
    },
    Deferred {
        eligible_at: Instant,
    },
}

impl PacketPacer {
    pub fn new(now: Instant, rate_bps: u64) -> Self {
        Self {
            rate_bps: rate_bps.max(MIN_RATE_BPS),
            probe: None,
            media_debt_bytes: 0,
            padding_debt_bytes: 0,
            last_updated: now,
        }
    }

    pub fn set_rate(&mut self, now: Instant, rate_bps: u64) {
        self.replenish(now);
        self.rate_bps = rate_bps.max(MIN_RATE_BPS);
    }

    pub fn has_active_probe(&self) -> bool {
        self.probe.is_some()
    }

    pub fn start_probe(
        &mut self,
        now: Instant,
        id: u32,
        rate_bps: u64,
        packet_count: u8,
        min_duration: Duration,
    ) {
        self.replenish(now);
        if packet_count == 0 {
            debug_assert!(false, "a GCC probe must include packets");
            return;
        }
        let rate_bps = rate_bps.max(MIN_RATE_BPS);
        self.probe = Some(ActiveProbe {
            id,
            rate_bps,
            packets_remaining: packet_count,
            bytes_sent: 0,
            target_bytes: rate_bps
                .saturating_mul(u64::try_from(min_duration.as_micros()).unwrap_or(u64::MAX))
                .saturating_add(7_999_999)
                .saturating_div(8_000_000),
            first_departure_at: None,
            next_departure_at: now,
            min_duration,
        });
    }

    pub fn admit(&mut self, now: Instant, bytes: usize, class: PacingClass) -> PacerDecision {
        debug_assert!(bytes > 0);
        let eligible_at = self.next_ready(now, bytes, class);
        if eligible_at > now {
            return PacerDecision::Deferred { eligible_at };
        }
        self.replenish(now);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if !class.bypasses_wait() {
            self.add_debt(bytes);
        }
        let mut probe_id = None;
        let mut probe_complete = false;
        if !class.bypasses_wait()
            && let Some(probe) = self.probe.as_mut()
        {
            let scheduled_departure = if probe.first_departure_at.is_some() {
                probe.next_departure_at
            } else {
                now
            };
            probe.first_departure_at.get_or_insert(now);
            probe_id = Some(probe.id);
            probe.packets_remaining = probe.packets_remaining.saturating_sub(1);
            probe.bytes_sent = probe.bytes_sent.saturating_add(bytes);
            probe.next_departure_at = scheduled_departure
                .checked_add(duration_for(bytes, probe.rate_bps))
                .unwrap_or(now);
            if probe.packets_remaining == 0
                && probe.bytes_sent >= probe.target_bytes
                && probe.first_departure_at.is_some_and(|started| {
                    now.saturating_duration_since(started) >= probe.min_duration
                })
            {
                self.probe = None;
                probe_complete = true;
            }
        }
        PacerDecision::Admitted {
            eligible_at,
            probe_id,
            probe_complete,
        }
    }

    pub fn next_ready(&mut self, now: Instant, bytes: usize, class: PacingClass) -> Instant {
        debug_assert!(bytes > 0);
        self.replenish(now);
        if class.bypasses_wait() {
            return now;
        }
        let probe_ready = self.probe.map(|probe| probe.next_departure_at);
        let debt = match class {
            PacingClass::Padding => self.media_debt_bytes.max(self.padding_debt_bytes),
            PacingClass::Audio | PacingClass::Retransmission | PacingClass::Video => {
                self.media_debt_bytes
            }
        };
        let rate = self.effective_rate();
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let burst_bytes = MAX_BURST_BYTES.max(bytes);
        let projected_debt = debt.saturating_add(bytes);
        let debt_ready = if projected_debt <= burst_bytes {
            now
        } else {
            let deficit = projected_debt.saturating_sub(burst_bytes);
            now.checked_add(duration_for(deficit, rate)).unwrap_or(now)
        };
        probe_ready.map_or(debt_ready, |ready| ready.max(debt_ready))
    }

    fn replenish(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_updated);
        let earned = u128::from(self.effective_rate())
            .saturating_mul(elapsed.as_nanos())
            .saturating_div(8_000_000_000);
        let earned = u64::try_from(earned).unwrap_or(u64::MAX);
        self.media_debt_bytes = self.media_debt_bytes.saturating_sub(earned);
        self.padding_debt_bytes = self.padding_debt_bytes.saturating_sub(earned);
        self.last_updated = now;
    }

    fn add_debt(&mut self, bytes: u64) {
        let rate = self.effective_rate();
        let maximum = bytes_for(rate, MAX_DEBT_TIME);
        debug_assert!(maximum > 0);
        self.media_debt_bytes = self.media_debt_bytes.saturating_add(bytes).min(maximum);
        self.padding_debt_bytes = self.padding_debt_bytes.saturating_add(bytes).min(maximum);
    }

    fn effective_rate(&self) -> u64 {
        self.probe.map_or(self.rate_bps, |probe| probe.rate_bps)
    }
}

fn bytes_for(rate_bps: u64, duration: Duration) -> u64 {
    let bytes = u128::from(rate_bps)
        .saturating_mul(duration.as_nanos())
        .saturating_div(8_000_000_000);
    u64::try_from(bytes).unwrap_or(u64::MAX).max(1)
}

fn duration_for(bytes: u64, rate_bps: u64) -> Duration {
    let nanos = u128::from(bytes)
        .saturating_mul(8_000_000_000)
        .saturating_add(u128::from(rate_bps.saturating_sub(1)))
        .checked_div(u128::from(rate_bps))
        .unwrap_or_default();
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[derive(Clone, Copy)]
    struct QueuedPacket {
        id: u64,
        bytes: usize,
        enqueued_at: Instant,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct PacerSummary {
        departures: Vec<(Duration, u64, Option<u32>)>,
        max_queue_delay: Duration,
    }

    struct PacerSimulation {
        epoch: Instant,
        now: Instant,
        pacer: PacketPacer,
        queue: VecDeque<QueuedPacket>,
        summary: PacerSummary,
    }

    impl PacerSimulation {
        fn new(rate_bps: u64) -> Self {
            let epoch = Instant::now();
            Self {
                epoch,
                now: epoch,
                pacer: PacketPacer::new(epoch, rate_bps),
                queue: VecDeque::new(),
                summary: PacerSummary {
                    departures: Vec::new(),
                    max_queue_delay: Duration::ZERO,
                },
            }
        }

        fn enqueue(&mut self, id: u64, bytes: usize) {
            debug_assert!(bytes > 0);
            debug_assert!(self.queue.back().is_none_or(|packet| packet.id < id));
            self.queue.push_back(QueuedPacket {
                id,
                bytes,
                enqueued_at: self.now,
            });
        }

        fn set_rate(&mut self, rate_bps: u64) {
            debug_assert!(rate_bps > 0);
            self.pacer.set_rate(self.now, rate_bps);
        }

        fn start_probe(
            &mut self,
            id: u32,
            rate_bps: u64,
            packet_count: u8,
            min_duration: Duration,
        ) {
            self.pacer
                .start_probe(self.now, id, rate_bps, packet_count, min_duration);
        }

        fn drain(&mut self) {
            while !self.queue.is_empty() {
                self.drain_one();
            }
        }

        fn drain_departures(&mut self, count: usize) {
            let target = self.summary.departures.len().saturating_add(count);
            while !self.queue.is_empty() && self.summary.departures.len() < target {
                self.drain_one();
            }
        }

        fn drain_one(&mut self) {
            let Some(packet) = self.queue.front().copied() else {
                return;
            };
            match self.pacer.admit(self.now, packet.bytes, PacingClass::Video) {
                PacerDecision::Admitted {
                    eligible_at,
                    probe_id,
                    ..
                } => {
                    debug_assert!(eligible_at <= self.now);
                    let Some(departed) = self.queue.pop_front() else {
                        debug_assert!(false, "an admitted packet remains at the queue front");
                        return;
                    };
                    let queue_delay = self.now.saturating_duration_since(departed.enqueued_at);
                    self.summary.max_queue_delay = self.summary.max_queue_delay.max(queue_delay);
                    self.summary.departures.push((
                        self.now.saturating_duration_since(self.epoch),
                        departed.id,
                        probe_id,
                    ));
                }
                PacerDecision::Deferred { eligible_at } => {
                    debug_assert!(eligible_at > self.now);
                    self.now = eligible_at;
                }
            }
        }
    }

    #[test]
    fn packet_pacer_spaces_packets_after_its_burst_budget() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);

        assert_eq!(
            pacer.admit(now, 1_600, PacingClass::Video),
            PacerDecision::Admitted {
                eligible_at: now,
                probe_id: None,
                probe_complete: false,
            }
        );
        assert_eq!(
            pacer.admit(now, 1_250, PacingClass::Video),
            PacerDecision::Deferred {
                eligible_at: now + Duration::from_millis(10)
            }
        );
        let ready = pacer.next_ready(now, 1_250, PacingClass::Video);
        assert_eq!(ready.duration_since(now), Duration::from_millis(10));
        assert_eq!(
            pacer.admit(ready, 1_250, PacingClass::Video),
            PacerDecision::Admitted {
                eligible_at: ready,
                probe_id: None,
                probe_complete: false,
            }
        );
    }

    #[test]
    fn media_bursts_are_bounded_by_time_debt() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 2_500_000);

        assert!(matches!(
            pacer.admit(now, 1_200, PacingClass::Video),
            PacerDecision::Admitted { .. }
        ));
        assert_eq!(
            pacer.admit(now, 1_200, PacingClass::Video),
            PacerDecision::Deferred {
                eligible_at: now + Duration::from_micros(2_560)
            }
        );
    }

    #[test]
    fn audio_bypasses_pacing_while_retransmissions_remain_bounded() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);
        assert!(matches!(
            pacer.admit(now, 1_600, PacingClass::Video),
            PacerDecision::Admitted { .. }
        ));

        assert!(matches!(
            pacer.admit(now, 200, PacingClass::Audio),
            PacerDecision::Admitted { eligible_at, .. } if eligible_at == now
        ));
        assert_eq!(
            pacer.admit(now, 1_200, PacingClass::Retransmission),
            PacerDecision::Deferred {
                eligible_at: now + Duration::from_micros(9_600)
            }
        );
        assert_eq!(
            pacer.next_ready(now, 1_200, PacingClass::Video),
            now + Duration::from_micros(9_600)
        );
    }

    #[test]
    fn padding_cannot_run_ahead_of_media_debt() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);
        assert!(matches!(
            pacer.admit(now, 1_600, PacingClass::Video),
            PacerDecision::Admitted { .. }
        ));

        assert_eq!(
            pacer.admit(now, 255, PacingClass::Padding),
            PacerDecision::Deferred {
                eligible_at: now + Duration::from_micros(2_040)
            }
        );
    }

    #[test]
    fn a_rate_increase_reprices_existing_debt_immediately() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);
        assert!(matches!(
            pacer.admit(now, 1_600, PacingClass::Video),
            PacerDecision::Admitted { .. }
        ));

        pacer.set_rate(now, 2_000_000);

        assert_eq!(
            pacer.next_ready(now, 1_200, PacingClass::Video),
            now + Duration::from_micros(4_800)
        );
    }

    #[test]
    fn probe_uses_its_rate_for_exactly_its_packet_budget() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 100_000);
        pacer.start_probe(now, 7, 1_000_000, 2, Duration::ZERO);
        assert_eq!(pacer.effective_rate(), 1_000_000);
        let first = pacer.admit(now, 1_600, PacingClass::Video);
        assert!(matches!(
            first,
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: false,
                ..
            }
        ));
        let ready = pacer.next_ready(now, 1_250, PacingClass::Video);
        assert_eq!(ready.duration_since(now), Duration::from_micros(12_800));
        assert!(matches!(
            pacer.admit(ready, 1_250, PacingClass::Video),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: true,
                ..
            }
        ));
        assert_eq!(pacer.effective_rate(), 100_000);
    }

    #[test]
    fn probe_uses_the_requested_proof_rate() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 2_000_000);
        pacer.start_probe(now, 7, 900_000, 5, Duration::from_millis(15));

        assert_eq!(pacer.effective_rate(), 900_000);
    }

    #[test]
    fn recovery_traffic_can_supply_probe_evidence() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);
        pacer.start_probe(now, 7, 1_000_000, 2, Duration::ZERO);

        assert!(matches!(
            pacer.admit(now, 1_200, PacingClass::Retransmission),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: false,
                ..
            }
        ));
        let ready = pacer.next_ready(now, 1_200, PacingClass::Video);
        assert!(matches!(
            pacer.admit(ready, 1_200, PacingClass::Video),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: true,
                ..
            }
        ));
    }

    #[test]
    fn probe_holds_its_cluster_open_for_the_required_duration() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);
        pacer.start_probe(now, 7, 2_000_000, 2, Duration::from_millis(15));

        assert!(matches!(
            pacer.admit(now, 1_600, PacingClass::Video),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: false,
                ..
            }
        ));
        let second_at = pacer.next_ready(now, 1_250, PacingClass::Video);
        assert!(matches!(
            pacer.admit(second_at, 1_250, PacingClass::Video),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: false,
                ..
            }
        ));
        let final_at = now + Duration::from_millis(15);
        assert!(matches!(
            pacer.admit(final_at, 1_250, PacingClass::Video),
            PacerDecision::Admitted {
                probe_id: Some(7),
                probe_complete: true,
                ..
            }
        ));
    }

    #[test]
    fn delayed_probe_still_measures_duration_from_its_first_packet() {
        let scheduled_at = Instant::now();
        let first_at = scheduled_at + Duration::from_secs(60);
        let mut pacer = PacketPacer::new(scheduled_at, 1_000_000);
        pacer.start_probe(scheduled_at, 7, 2_000_000, 2, Duration::from_millis(15));

        assert!(matches!(
            pacer.admit(first_at, 1_600, PacingClass::Video),
            PacerDecision::Admitted {
                probe_complete: false,
                ..
            }
        ));
        let second_at = pacer.next_ready(first_at, 1_250, PacingClass::Video);
        assert!(matches!(
            pacer.admit(second_at, 1_250, PacingClass::Video),
            PacerDecision::Admitted {
                probe_complete: false,
                ..
            }
        ));
        assert!(matches!(
            pacer.admit(
                first_at + Duration::from_millis(15),
                1_250,
                PacingClass::Video
            ),
            PacerDecision::Admitted {
                probe_complete: true,
                ..
            }
        ));
    }

    #[test]
    fn pacer_simulation_replays_identically() {
        fn run() -> PacerSummary {
            let mut simulation = PacerSimulation::new(800_000);
            for id in 0..100 {
                simulation.enqueue(id, 1_200);
            }
            simulation.start_probe(9, 2_000_000, 5, Duration::from_millis(15));
            simulation.drain();
            simulation.summary
        }

        assert_eq!(run(), run());
    }

    #[test]
    fn pacer_burst_debt_and_queue_delay_are_bounded() {
        let mut simulation = PacerSimulation::new(1_000_000);
        for id in 0..100 {
            simulation.enqueue(id, 1_200);
        }

        simulation.drain();

        let immediate = simulation
            .summary
            .departures
            .iter()
            .filter(|(at, _, _)| at.is_zero())
            .count();
        assert_eq!(immediate, 1);
        assert!(
            simulation
                .summary
                .departures
                .windows(2)
                .all(|departures| matches!(departures, [left, right] if left.0 <= right.0))
        );
        assert!(simulation.summary.max_queue_delay < Duration::from_secs(1));
    }

    #[test]
    fn pacer_rate_increase_drains_existing_backlog_promptly() {
        let mut simulation = PacerSimulation::new(300_000);
        for id in 0..100 {
            simulation.enqueue(id, 1_200);
        }
        simulation.drain_departures(10);
        let changed_at = simulation.now.saturating_duration_since(simulation.epoch);

        simulation.set_rate(2_000_000);
        simulation.drain();
        let completed_at = simulation
            .summary
            .departures
            .last()
            .map_or(Duration::ZERO, |(at, _, _)| *at);

        assert!(completed_at.saturating_sub(changed_at) < Duration::from_millis(500));
        assert_eq!(simulation.summary.departures.len(), 100);
    }

    #[test]
    fn pacer_probe_departures_meet_the_cluster_contract() {
        let mut simulation = PacerSimulation::new(300_000);
        simulation.start_probe(11, 2_000_000, 5, Duration::from_millis(15));
        for id in 0..10 {
            simulation.enqueue(id, 1_200);
        }

        simulation.drain();
        let probe_departures: Vec<_> = simulation
            .summary
            .departures
            .iter()
            .filter(|(_, _, probe_id)| *probe_id == Some(11))
            .collect();

        assert!(probe_departures.len() >= 5);
        let first = probe_departures
            .first()
            .map_or(Duration::ZERO, |(at, _, _)| *at);
        let last = probe_departures
            .last()
            .map_or(Duration::ZERO, |(at, _, _)| *at);
        assert!(last.saturating_sub(first) >= Duration::from_millis(15));
        assert!(!simulation.pacer.has_active_probe());
    }
}
