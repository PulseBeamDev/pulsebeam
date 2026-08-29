use std::time::Duration;

use tokio::time::Instant;

const MIN_RATE_BPS: u64 = 30_000;
const MAX_BURST_BYTES: u64 = 1_600;

pub(crate) struct PacketPacer {
    rate_bps: u64,
    credit_bytes: u64,
    last_updated: Instant,
}

impl PacketPacer {
    pub(crate) fn new(now: Instant, rate_bps: u64) -> Self {
        Self {
            rate_bps: rate_bps.max(MIN_RATE_BPS),
            credit_bytes: MAX_BURST_BYTES,
            last_updated: now,
        }
    }

    pub(crate) fn set_rate(&mut self, now: Instant, rate_bps: u64) {
        self.replenish(now);
        self.rate_bps = rate_bps.max(MIN_RATE_BPS);
    }

    pub(crate) fn permits(&mut self, now: Instant, bytes: usize) -> bool {
        self.replenish(now);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if bytes > self.credit_bytes {
            return false;
        }
        self.credit_bytes = self.credit_bytes.saturating_sub(bytes);
        true
    }

    pub(crate) fn next_ready(&mut self, now: Instant, bytes: usize) -> Instant {
        self.replenish(now);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if bytes <= self.credit_bytes {
            return now;
        }
        let deficit = bytes.saturating_sub(self.credit_bytes);
        let numerator = u128::from(deficit)
            .saturating_mul(8_000_000_000)
            .saturating_add(u128::from(self.rate_bps.saturating_sub(1)));
        let nanos = numerator
            .checked_div(u128::from(self.rate_bps))
            .unwrap_or_default();
        let wait = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        now.checked_add(wait).unwrap_or(now)
    }

    fn replenish(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_updated);
        let earned = u128::from(self.rate_bps)
            .saturating_mul(elapsed.as_nanos())
            .saturating_div(8_000_000_000);
        self.credit_bytes = self
            .credit_bytes
            .saturating_add(u64::try_from(earned).unwrap_or(u64::MAX))
            .min(MAX_BURST_BYTES);
        self.last_updated = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_pacer_spaces_packets_after_its_burst_budget() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now, 1_000_000);

        assert!(pacer.permits(now, 1_600));
        assert!(!pacer.permits(now, 1_250));
        let ready = pacer.next_ready(now, 1_250);
        assert_eq!(ready.duration_since(now), Duration::from_millis(10));
        assert!(pacer.permits(ready, 1_250));
    }
}
