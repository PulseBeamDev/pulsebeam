use std::time::{Duration, Instant};

const MAX_PROBE_SCHEDULE_LAG: Duration = Duration::from_millis(1);

#[derive(Debug, Default)]
pub(crate) struct Pacer {
    next_send: Option<Instant>,
    rate_bps: u64,
    charged_bytes: u64,
}

impl Pacer {
    pub(crate) fn reset(&mut self) {
        self.next_send = None;
        self.rate_bps = 0;
    }

    pub(crate) fn reprice(&mut self, now: Instant, rate_bps: u64) {
        let rate_bps = rate_bps.max(1);
        if self.rate_bps != 0 && self.rate_bps != rate_bps {
            if let Some(deadline) = self.next_send.filter(|deadline| *deadline > now) {
                // Preserve remaining byte debt, not the old rate's drain time.
                let nanos = deadline
                    .saturating_duration_since(now)
                    .as_nanos()
                    .saturating_mul(u128::from(self.rate_bps))
                    .div_ceil(u128::from(rate_bps));
                let remaining = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
                self.next_send = now.checked_add(remaining);
            }
        }
        self.rate_bps = rate_bps;
    }

    pub(crate) fn eligible(&self, now: Instant) -> bool {
        self.next_send.is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_send
    }

    pub(crate) fn commit(&mut self, now: Instant, wire_bytes: usize, rate_bps: u64) {
        self.reprice(now, rate_bps);
        let start = self.next_send.map_or(now, |deadline| deadline.max(now));
        self.charge(start, wire_bytes, rate_bps);
    }

    pub(crate) fn commit_probe_cluster(&mut self, now: Instant, wire_bytes: usize, rate_bps: u64) {
        self.reprice(now, rate_bps);
        let earliest = now.checked_sub(MAX_PROBE_SCHEDULE_LAG).unwrap_or(now);
        let start = self
            .next_send
            .map_or(now, |deadline| deadline.max(earliest));
        self.charge(start, wire_bytes, rate_bps);
    }

    fn charge(&mut self, start: Instant, wire_bytes: usize, rate_bps: u64) {
        let nanos = (wire_bytes as u128)
            .saturating_mul(8_000_000_000)
            .div_ceil(u128::from(rate_bps.max(1)));
        let interval = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        self.next_send = start.checked_add(interval);
        self.charged_bytes = self.charged_bytes.saturating_add(wire_bytes as u64);
    }

    #[cfg(test)]
    pub(crate) const fn charged_bytes(&self) -> u64 {
        self.charged_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_late_commit_discards_schedule_credit() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.commit(now, 100, 80_000);
        pacer.commit(now + Duration::from_millis(25), 100, 80_000);
        assert_eq!(pacer.next_deadline(), Some(now + Duration::from_millis(35)));
        assert_eq!(pacer.charged_bytes(), 200);
    }

    #[test]
    fn probe_schedule_catches_up_at_most_one_timer_quantum() {
        let now = Instant::now();
        for (late, expected_packets) in
            [(Duration::from_millis(1), 10), (Duration::from_secs(1), 11)]
        {
            let mut pacer = Pacer::default();
            pacer.commit_probe_cluster(now, 100, 8_000_000);
            let at = now + late;
            let mut packets = 0;
            while pacer.eligible(at) {
                assert!(packets < 11, "probe catch-up exceeded its bounded quantum");
                pacer.commit_probe_cluster(at, 100, 8_000_000);
                packets += 1;
            }
            assert_eq!(packets, expected_packets);
            assert_eq!(pacer.next_deadline(), Some(at + Duration::from_micros(100)));
            assert_eq!(pacer.charged_bytes(), (packets + 1) * 100);
        }
    }

    #[test]
    fn rate_changes_preserve_debt_and_reprice_its_remaining_service() {
        let now = Instant::now();
        for (rate, deadline) in [(160_000, 60), (40_000, 180)] {
            let mut pacer = Pacer::default();
            pacer.commit(now, 1_000, 80_000);
            pacer.reprice(now + Duration::from_millis(20), rate);
            let expected = now + Duration::from_millis(deadline);
            assert_eq!(pacer.next_deadline(), Some(expected));
            assert!(!pacer.eligible(expected - Duration::from_nanos(1)));
            assert!(pacer.eligible(expected));
            assert_eq!(pacer.charged_bytes(), 1_000);
            pacer.commit(expected, 100, rate);
            assert_eq!(pacer.charged_bytes(), 1_100);
        }
    }

    #[test]
    fn repricing_cannot_recreate_spent_idle_schedule_credit() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.commit(now, 100, 80_000);
        pacer.reprice(now + Duration::from_millis(20), 160_000);
        pacer.commit(now + Duration::from_millis(20), 100, 160_000);
        assert_eq!(pacer.next_deadline(), Some(now + Duration::from_millis(25)));
        assert_eq!(pacer.charged_bytes(), 200);
    }

    #[test]
    fn pacer_charges_exact_emitted_transport_bytes() {
        let now = Instant::now();
        let mut pacer = Pacer::default();
        pacer.commit(now, 1_000, 80_000);
        assert_eq!(pacer.charged_bytes(), 1_000);
        assert_eq!(
            pacer.next_deadline(),
            now.checked_add(Duration::from_millis(100))
        );
        assert!(!pacer.eligible(now + Duration::from_millis(99)));
        assert!(pacer.eligible(now + Duration::from_millis(100)));
        pacer.reset();
        assert!(pacer.eligible(now));
        assert_eq!(pacer.next_deadline(), None);
    }
}
