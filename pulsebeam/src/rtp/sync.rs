use crate::rtp::RtpPacket;
use metrics::histogram;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
use str0m::{
    media::{Frequency, MediaTime},
    rtp::rtcp::SenderInfo,
};
use tokio::time::Instant;
use tracing::warn;

const SR_HISTORY_CAPACITY: usize = 5;
const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SR_CLOCK_DEVIATION: f64 = 0.10; // 10% tolerance
const MIN_SR_ERROR_TOLERANCE: i64 = 9000; // ~100ms @ 90kHz
const MAX_PLAYOUT_FUTURE: Duration = Duration::from_millis(1000);
const MAX_PLAYOUT_PAST: Duration = Duration::from_millis(500);
const CLOCK_SMEAR_DURATION: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
struct ClockReference {
    rtp_time: MediaTime,
    ntp_time: SystemTime, // used to compute relative clock drift
    server_time: Instant, // authoritative server monotonic clock
}

/// State for managing smooth clock convergence.
#[derive(Debug, Clone, Copy)]
struct Correction {
    /// The server time when the correction period started.
    start_time: Instant,
    /// The total adjustment that needs to be applied over the smear duration.
    adjustment: Duration,
    /// Whether the adjustment is positive (speed up) or negative (slow down).
    is_negative: bool,
    /// The final, accurate clock reference to switch to after the smear is complete.
    final_offset: ClockReference,
}

#[derive(Debug)]
pub struct Synchronizer {
    clock_rate: Frequency,
    sr_history: VecDeque<ClockReference>,
    rtp_offset: Option<ClockReference>,
    is_provisional: bool,
    correction: Option<Correction>,
    estimated_clock_drift_ppm: f64,
}

impl Synchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            sr_history: VecDeque::with_capacity(SR_HISTORY_CAPACITY),
            rtp_offset: None,
            is_provisional: false,
            correction: None,
            estimated_clock_drift_ppm: 0.0,
        }
    }

    pub fn process(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, now);
        }

        // Initialize reference if missing
        if self.rtp_offset.is_none() {
            self.is_provisional = true;
            self.rtp_offset = Some(ClockReference {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH,
                server_time: now,
            });
        }

        let playout_time = self.calculate_playout_time(packet.rtp_ts, now);
        packet.playout_time = self.validate_playout_time(playout_time, now);

        packet
    }

    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        // Reject reordered/stale SR
        if let Some(last) = self.sr_history.back() {
            if sr.ntp_time <= last.ntp_time {
                return;
            }
            // Rate-limit updates to avoid jitter spikes
            if let Some(correction) = &self.correction {
                if now.duration_since(correction.start_time) < MIN_SR_UPDATE_INTERVAL {
                    return;
                }
            }
        }

        let mapping = ClockReference {
            rtp_time: sr.rtp_time,
            ntp_time: sr.ntp_time,
            server_time: now,
        };

        // If the current offset is just a provisional guess, we start a smooth convergence.
        if self.is_provisional {
            if let Some(provisional_offset) = self.rtp_offset {
                self.is_provisional = false;

                let provisional_playout = self.calculate_playout_time_with_offset(
                    mapping.rtp_time,
                    now,
                    provisional_offset,
                );
                let accurate_playout =
                    self.calculate_playout_time_with_offset(mapping.rtp_time, now, mapping);

                let (adjustment, is_negative) = if accurate_playout > provisional_playout {
                    (accurate_playout - provisional_playout, false)
                } else {
                    (provisional_playout - accurate_playout, true)
                };

                self.correction = Some(Correction {
                    start_time: now,
                    adjustment,
                    is_negative,
                    final_offset: mapping,
                });
            }
        }

        if let Some(last) = self.sr_history.back() {
            self.estimated_clock_drift_ppm = Self::compute_clock_drift(last, &mapping);
            histogram!("rtp_sync_clock_drift_ppm").record(self.estimated_clock_drift_ppm);
        }

        if self.sr_history.len() == SR_HISTORY_CAPACITY {
            self.sr_history.pop_front();
        }
        self.sr_history.push_back(mapping);

        // Only update the main offset if we are not in a correction phase.
        if self.correction.is_none() {
            self.rtp_offset = Some(mapping);
        }
    }

    fn compute_clock_drift(last: &ClockReference, current: &ClockReference) -> f64 {
        let server_delta = current
            .server_time
            .duration_since(last.server_time)
            .as_secs_f64();
        let sender_rtp_delta = current.rtp_time.numer().wrapping_sub(last.rtp_time.numer()) as i64;
        let sender_ntp_delta = current
            .ntp_time
            .duration_since(last.ntp_time)
            .unwrap_or(Duration::from_secs(0))
            .as_secs_f64();

        if sender_ntp_delta == 0.0 || server_delta == 0.0 {
            return 0.0;
        }

        // Expected RTP ticks based on server time
        let expected_rtp_delta = (server_delta * last.rtp_time.frequency().get() as f64) as i64;
        let delta_diff = (sender_rtp_delta - expected_rtp_delta).abs();
        let acceptable_error = ((expected_rtp_delta.abs() as f64 * MAX_SR_CLOCK_DEVIATION) as i64)
            .max(MIN_SR_ERROR_TOLERANCE);

        // Correct small drift automatically
        if delta_diff > acceptable_error {
            warn!(
                "Large clock deviation detected: {} ticks, correcting",
                delta_diff
            );
        }

        // Clock Drift Unit = PPM, aka microseconds faster per second
        ((sender_rtp_delta as f64 - expected_rtp_delta as f64) / expected_rtp_delta as f64)
            * 1_000_000.0
    }

    fn calculate_playout_time(&mut self, rtp_ts: MediaTime, now: Instant) -> Instant {
        // 1. Calculate the base playout time using the *current* offset,
        // which might be provisional or the last known good one.
        let mut playout_time = self
            .rtp_offset
            .map(|offset| self.calculate_playout_time_with_offset(rtp_ts, now, offset))
            .unwrap_or(now);

        // 2. Check if we are in a smooth correction phase.
        if let Some(correction) = self.correction {
            let elapsed = now.saturating_duration_since(correction.start_time);

            // 3. If the correction period is still active, apply a partial adjustment.
            if elapsed < CLOCK_SMEAR_DURATION {
                // Calculate how far along we are in the convergence period (0.0 to 1.0).
                let progress = elapsed.as_secs_f64() / CLOCK_SMEAR_DURATION.as_secs_f64();

                // Determine how much of the total adjustment to apply to this specific packet.
                let smear_adjustment = correction.adjustment.mul_f64(progress);

                // Apply the smear: either add or subtract the adjustment from the base time.
                playout_time = if correction.is_negative {
                    playout_time.saturating_sub(smear_adjustment)
                } else {
                    playout_time + smear_adjustment
                };
            } else {
                // 4. The correction period has just ended. Finalize the state.
                // Snap to the final, accurate offset provided by the first Sender Report.
                self.rtp_offset = Some(correction.final_offset);

                // Clear the correction state so we don't run this logic again.
                self.correction = None;

                // Recalculate the playout time one last time with the new, fully-correct offset.
                // This ensures perfect accuracy from this packet onwards.
                playout_time =
                    self.calculate_playout_time_with_offset(rtp_ts, now, correction.final_offset);
            }
        }

        playout_time
    }

    /// Helper to calculate playout time based on a specific clock reference.
    fn calculate_playout_time_with_offset(
        &self,
        rtp_ts: MediaTime,
        now: Instant,
        offset: ClockReference,
    ) -> Instant {
        let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer()) as i32;

        // Apply drift correction: scale RTP delta by measured clock drift
        // Note: Drift is only applied once we are fully synchronized, not during provisional/correction phases.
        let drift_correction = if self.is_synchronized() {
            1.0 - self.estimated_clock_drift_ppm / 1_000_000.0
        } else {
            1.0
        };
        let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;

        if seconds_delta >= 0.0 {
            offset.server_time + Duration::from_secs_f64(seconds_delta)
        } else {
            offset
                .server_time
                .checked_sub(Duration::from_secs_f64(-seconds_delta))
                .unwrap_or(now)
        }
    }

    fn validate_playout_time(&self, playout_time: Instant, now: Instant) -> Option<Instant> {
        if playout_time > now + MAX_PLAYOUT_FUTURE {
            warn!(
                "Far future playout_time: {:?}. Limiting to {:?}.",
                playout_time,
                now + MAX_PLAYOUT_FUTURE
            );
            return Some(now + MAX_PLAYOUT_FUTURE);
        }
        if let Some(past_limit) = now.checked_sub(MAX_PLAYOUT_PAST) {
            if playout_time < past_limit {
                return Some(past_limit);
            }
        }
        Some(playout_time)
    }

    pub fn is_synchronized(&self) -> bool {
        !self.is_provisional && self.correction.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RtpPacket, VIDEO_FREQUENCY};
    use std::time::UNIX_EPOCH;

    fn create_sr(rtp_ts: MediaTime, ntp_secs_since_epoch: u64) -> SenderInfo {
        let ntp_time = UNIX_EPOCH + Duration::from_secs(ntp_secs_since_epoch);
        SenderInfo {
            ssrc: 1.into(),
            rtp_time: rtp_ts,
            ntp_time,
            sender_packet_count: 0,
            sender_octet_count: 0,
        }
    }

    #[test]
    fn test_initialization_and_first_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();

        let mut packet = RtpPacket::default();
        packet.rtp_ts = MediaTime::new(1000, VIDEO_FREQUENCY);
        packet = sync.process(packet, base_time);

        assert_eq!(packet.playout_time, Some(base_time));
        assert!(!sync.is_synchronized());

        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(2000, VIDEO_FREQUENCY);
        packet2.last_sender_info = Some(create_sr(MediaTime::from_90khz(1800), 10));
        let sr_time = base_time + Duration::from_millis(300);
        packet2 = sync.process(packet2, sr_time);

        let expected_delta = (2000 - 1800) as f64 / VIDEO_FREQUENCY.get() as f64;
        assert_eq!(
            packet2.playout_time,
            Some(sr_time + Duration::from_secs_f64(expected_delta))
        );
        assert!(sync.is_synchronized());
    }

    #[test]
    fn test_clock_drift_detection() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();

        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(MediaTime::from_90khz(0), 100)),
                ..Default::default()
            },
            base_time,
        );
        let sr2_time = base_time + Duration::from_secs(1);
        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(MediaTime::from_90khz(90000), 101)),
                ..Default::default()
            },
            sr2_time,
        );

        let sr3_time = base_time + Duration::from_secs(2);
        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(MediaTime::from_90khz(180090), 102)),
                ..Default::default()
            },
            sr3_time,
        );

        assert_eq!(sync.estimated_clock_drift_ppm.round() as i64, 1000);
    }
}
