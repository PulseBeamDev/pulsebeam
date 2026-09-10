use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub(crate) struct Pacer {
    next_send: Option<Instant>,
    charged_bytes: u64,
}

impl Pacer {
    pub(crate) fn eligible(&self, now: Instant) -> bool {
        self.next_send.is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_send
    }

    pub(crate) fn commit(&mut self, now: Instant, wire_bytes: usize, rate_bps: u64) {
        let start = self.next_send.map_or(now, |deadline| deadline.max(now));
        let nanos = (wire_bytes as u128)
            .saturating_mul(8_000_000_000)
            .div_ceil(u128::from(rate_bps.max(1)));
        let interval = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        self.next_send = start.checked_add(interval);
        self.charged_bytes = self.charged_bytes.saturating_add(wire_bytes as u64);
    }

    #[cfg(test)]
    const fn charged_bytes(&self) -> u64 {
        self.charged_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
