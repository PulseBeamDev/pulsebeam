use super::{MAX_LADDER_TARGETS, RATE_FALL_TIME_CONSTANT, RateFilter};
use pulsebeam_runtime::time::{Duration, Instant};

// Fabric has explicit structure and active-mask epochs. Sample those epochs
// promptly while retaining the same asymmetric smoothing for mature rates.
const FABRIC_SAMPLE_WINDOW: Duration = Duration::from_millis(1500);

/// Payload costs, in the same units as the stream estimator. A complete reactive
/// fall interval avoids treating an individual keyframe as a sustained rate.
/// Missing eligibility invalidates the window rather than counting it as zero.
#[derive(Debug)]
pub(super) struct TargetRates {
    start: Option<Instant>,
    bytes: [u64; MAX_LADDER_TARGETS],
    pictures: [u64; MAX_LADDER_TARGETS],
    pub(super) fps: [u16; MAX_LADDER_TARGETS],
    filters: [RateFilter; MAX_LADDER_TARGETS],
    pub(super) costs: [u32; MAX_LADDER_TARGETS],
}

impl TargetRates {
    pub(super) fn new() -> Self {
        Self {
            start: None,
            bytes: [0; MAX_LADDER_TARGETS],
            pictures: [0; MAX_LADDER_TARGETS],
            fps: [0; MAX_LADDER_TARGETS],
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
        self.record_bytes(
            now,
            eligible.map(|present| if present { bytes } else { 0 }),
            None,
        );
    }

    pub(super) fn record_bytes(
        &mut self,
        now: Instant,
        bytes: [u64; MAX_LADDER_TARGETS],
        pictures: Option<[u64; MAX_LADDER_TARGETS]>,
    ) {
        self.poll_with_source(now, pictures.is_some());
        self.start.get_or_insert(now);
        for (total, bytes) in self.bytes.iter_mut().zip(bytes) {
            *total = total.saturating_add(bytes);
        }
        if let Some(pictures) = pictures {
            for (total, pictures) in self.pictures.iter_mut().zip(pictures) {
                *total = total.saturating_add(pictures);
            }
        }
    }

    #[cfg(test)]
    fn poll(&mut self, now: Instant) {
        self.poll_with_source(now, false);
    }

    pub(super) fn poll_with_source(&mut self, now: Instant, fabric: bool) {
        let Some(start) = self.start else { return };
        let elapsed = now.saturating_duration_since(start);
        let window = if fabric {
            FABRIC_SAMPLE_WINDOW
        } else {
            RATE_FALL_TIME_CONSTANT
        };
        if elapsed < window {
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
        for (fps, pictures) in self.fps.iter_mut().zip(&mut self.pictures) {
            let rate = (*pictures as f64 / elapsed.as_secs_f64()).round();
            *fps = u16::try_from(crate::bitrate::saturating_bps(rate)).unwrap_or(u16::MAX);
            *pictures = 0;
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
