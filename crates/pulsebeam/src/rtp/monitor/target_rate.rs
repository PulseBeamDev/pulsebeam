use super::{MAX_LADDER_TARGETS, RATE_FALL_TIME_CONSTANT, RateFilter};
use tokio::time::Instant;

/// Payload costs, in the same units as the stream estimator. A complete reactive
/// fall interval avoids treating an individual keyframe as a sustained rate.
/// Missing eligibility invalidates the window rather than counting it as zero.
#[derive(Debug)]
pub(super) struct TargetRates {
    start: Option<Instant>,
    bytes: [u64; MAX_LADDER_TARGETS],
    filters: [RateFilter; MAX_LADDER_TARGETS],
    pub(super) costs: [u32; MAX_LADDER_TARGETS],
}

impl TargetRates {
    pub(super) fn new() -> Self {
        Self {
            start: None,
            bytes: [0; MAX_LADDER_TARGETS],
            filters: std::array::from_fn(|_| RateFilter::new()),
            costs: [0; MAX_LADDER_TARGETS],
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(super) fn record(
        &mut self,
        now: Instant,
        bytes: u64,
        eligible: [bool; MAX_LADDER_TARGETS],
    ) {
        self.poll(now);
        self.start.get_or_insert(now);
        for (total, present) in self.bytes.iter_mut().zip(eligible) {
            if present {
                *total = total.saturating_add(bytes);
            }
        }
    }

    pub(super) fn poll(&mut self, now: Instant) {
        let Some(start) = self.start else { return };
        let elapsed = now.saturating_duration_since(start);
        if elapsed < RATE_FALL_TIME_CONSTANT {
            return;
        }
        for ((bytes, filter), cost) in self
            .bytes
            .iter_mut()
            .zip(&mut self.filters)
            .zip(&mut self.costs)
        {
            if *bytes == 0 {
                filter.reset();
                *cost = 0;
            } else {
                let rate = *bytes as f64 * 8.0 / elapsed.as_secs_f64();
                filter.update(now, rate);
                *cost = u32::try_from(crate::bitrate::saturating_bps(filter.current()))
                    .unwrap_or(u32::MAX);
            }
            *bytes = 0;
        }
        self.start = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_eligible_fragments_not_declared_temporal_fractions() {
        let start = Instant::now();
        let mut rates = TargetRates::new();
        rates.record(start, 100, [true, true, true]);
        rates.record(start, 100, [true, true, true]);
        rates.record(start, 900, [false, false, true]);
        assert_eq!(rates.costs, [0; MAX_LADDER_TARGETS]);
        rates.poll(start + RATE_FALL_TIME_CONSTANT);
        assert_eq!(rates.costs, [533, 533, 2933]);
    }

    #[test]
    fn unknown_eligibility_reset_and_empty_windows_cannot_publish_zero_cost() {
        let start = Instant::now();
        let mut rates = TargetRates::new();
        rates.record(start, 1200, [true; MAX_LADDER_TARGETS]);
        rates.poll(start + RATE_FALL_TIME_CONSTANT);
        assert!(rates.costs.iter().all(|cost| *cost > 0));
        rates.reset();
        assert_eq!(rates.costs, [0; MAX_LADDER_TARGETS]);
        rates.record(start + RATE_FALL_TIME_CONSTANT, 1200, [false, true, true]);
        rates.poll(start + RATE_FALL_TIME_CONSTANT * 2);
        assert_eq!(rates.costs[0], 0);
        assert!(rates.costs[1] > 0);
        rates.poll(start + RATE_FALL_TIME_CONSTANT * 3);
        assert_eq!(rates.costs, [0; MAX_LADDER_TARGETS]);
    }

    #[test]
    fn packet_byte_overflow_is_saturating() {
        let start = Instant::now();
        let mut rates = TargetRates::new();
        rates.record(start, u64::MAX, [true; MAX_LADDER_TARGETS]);
        rates.record(start, 1, [true; MAX_LADDER_TARGETS]);
        rates.poll(start + RATE_FALL_TIME_CONSTANT);
        assert_eq!(rates.costs, [u32::MAX; MAX_LADDER_TARGETS]);
    }
}
