use std::num::NonZeroU64;

use pulsebeam_runtime::time::Instant;

use super::schedule::PACKETS_PER_FLOW;

const UDP_IPV6_BYTES: usize = 48;
const NANOBITS_PER_BYTE: u128 = 8_000_000_000;
const BURST_BYTES: usize =
    PACKETS_PER_FLOW * (pulsebeam_link::wan::MAX_DATAGRAM_BYTES + UDP_IPV6_BYTES);
const BURST: u128 = BURST_BYTES as u128 * NANOBITS_PER_BYTE;

/// A protected window may burst by one bounded flow queue. Envelopes are in
/// IP wire bits; charge IPv6 plus UDP even if the adjacency uses IPv4.
pub(super) struct Envelope {
    rate: NonZeroU64,
    credit: u128,
    updated: Instant,
}

impl Envelope {
    pub(super) fn new(rate: NonZeroU64, now: Instant) -> Self {
        Self {
            rate,
            credit: BURST,
            updated: now,
        }
    }

    pub(super) fn rate(&self) -> u64 {
        self.rate.get()
    }

    fn cost(udp_bytes: usize) -> u128 {
        udp_bytes.saturating_add(UDP_IPV6_BYTES) as u128 * NANOBITS_PER_BYTE
    }

    pub(super) fn allows(&mut self, now: Instant, udp_bytes: usize) -> bool {
        if let Some(elapsed) = now.checked_duration_since(self.updated) {
            self.credit = self
                .credit
                .saturating_add(
                    elapsed
                        .as_nanos()
                        .saturating_mul(u128::from(self.rate.get())),
                )
                .min(BURST);
            self.updated = now;
        }
        self.credit >= Self::cost(udp_bytes)
    }

    pub(super) fn sent(&mut self, udp_bytes: usize) {
        self.credit = self.credit.saturating_sub(Self::cost(udp_bytes));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn protected_envelope_charges_ip_overhead_and_refills_at_the_reserved_wire_rate() {
        let now = Instant::now();
        let udp = pulsebeam_link::wan::MAX_DATAGRAM_BYTES;
        let rate = NonZeroU64::new(((udp + UDP_IPV6_BYTES) * 8) as u64).unwrap();
        let mut envelope = Envelope::new(rate, now);
        for _ in 0..PACKETS_PER_FLOW {
            assert!(envelope.allows(now, udp));
            assert!(envelope.allows(now, udp), "a retry is not a departure");
            envelope.sent(udp);
        }
        assert!(!envelope.allows(now, udp));
        assert!(!envelope.allows(now + Duration::from_millis(999), udp));
        let next = now + Duration::from_secs(1);
        assert!(envelope.allows(next, udp));
        envelope.sent(udp);
        assert!(!envelope.allows(next, udp));
    }
}
