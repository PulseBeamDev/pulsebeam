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

    pub fn process(&mut self, mut packet: RtpPacket) -> RtpPacket {
        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, packet.arrival_ts);
        }

        // Initialize reference if missing
        if self.rtp_offset.is_none() {
            self.is_provisional = true;
            self.rtp_offset = Some(ClockReference {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH,
                server_time: packet.arrival_ts,
            });
        }

        let playout_time = self.calculate_playout_time(packet.rtp_ts, packet.arrival_ts);
        packet.playout_time = self.validate_playout_time(playout_time, packet.arrival_ts);

        packet
    }

    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        // Reject reordered/stale SR
        if let Some(last) = self.sr_history.back() {
            if sr.ntp_time <= last.ntp_time {
                return;
            }
            // Rate-limit updates to avoid jitter spikes
            if let Some(correction) = &self.correction
                && now.duration_since(correction.start_time) < MIN_SR_UPDATE_INTERVAL
            {
                return;
            }
        }

        let mapping = ClockReference {
            rtp_time: sr.rtp_time,
            ntp_time: sr.ntp_time,
            server_time: now,
        };

        // If the current offset is just a provisional guess, we start a smooth convergence.
        if self.is_provisional
            && let Some(provisional_offset) = self.rtp_offset
        {
            self.is_provisional = false;

            let provisional_playout =
                self.calculate_playout_time_with_offset(mapping.rtp_time, now, provisional_offset);
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
        // This is the number of RTP ticks that the sender's media clock advanced.
        let sender_rtp_delta = current.rtp_time.numer().wrapping_sub(last.rtp_time.numer()) as i64;

        // This is the wall-clock time that the sender claims has passed between the two reports.
        // This is our most reliable measure of the interval duration.
        let sender_ntp_delta_secs = current
            .ntp_time
            .duration_since(last.ntp_time)
            .unwrap_or(Duration::from_secs(0))
            .as_secs_f64();

        if sender_ntp_delta_secs <= 0.0 {
            return 0.0;
        }

        // Based on the wall-clock time passed, this is how many ticks a PERFECT clock should have generated.
        let expected_rtp_delta = sender_ntp_delta_secs * last.rtp_time.frequency().get() as f64;

        // The drift is the difference between what we observed and what we expected,
        // as a fraction of the expected amount.
        let drift_ratio = (sender_rtp_delta as f64 - expected_rtp_delta) / expected_rtp_delta;

        // Clock Drift Unit = PPM (Parts Per Million)
        drift_ratio * 1_000_000.0
    }

    // The change to the more precise inverse formula should also be kept.
    fn calculate_playout_time_with_offset(
        &self,
        rtp_ts: MediaTime,
        now: Instant,
        offset: ClockReference,
    ) -> Instant {
        let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer()) as i32;

        let drift = self.estimated_clock_drift_ppm / 1_000_000.0;
        let drift_correction = 1.0 / (1.0 + drift);

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
                    playout_time.checked_sub(smear_adjustment).unwrap_or(now)
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

    fn validate_playout_time(&self, playout_time: Instant, now: Instant) -> Instant {
        if playout_time > now + MAX_PLAYOUT_FUTURE {
            warn!(
                "Far future playout_time: {:?}. Limiting to {:?}.",
                playout_time,
                now + MAX_PLAYOUT_FUTURE
            );
            return now + MAX_PLAYOUT_FUTURE;
        }
        if let Some(past_limit) = now.checked_sub(MAX_PLAYOUT_PAST)
            && playout_time < past_limit
        {
            return past_limit;
        }
        playout_time
    }

    pub fn is_synchronized(&self) -> bool {
        !self.is_provisional && self.correction.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RtpPacket, VIDEO_FREQUENCY};
    use std::time::{SystemTime, UNIX_EPOCH};

    // NTP epoch is 1900-01-01 00:00:00 UTC.
    // Unix epoch is 1970-01-01 00:00:00 UTC.
    // The difference is 2,208,988,800 seconds.
    const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

    fn create_sr(rtp_ts: MediaTime, ntp_time: SystemTime) -> SenderInfo {
        SenderInfo {
            ssrc: 1.into(),
            rtp_time: rtp_ts,
            ntp_time,
            sender_packet_count: 0,
            sender_octet_count: 0,
        }
    }

    /// This comprehensive test validates the entire lifecycle of the synchronizer:
    /// 1. Starts in a provisional state.
    /// 2. Enters a converging state upon receiving the first SR.
    /// 3. Smoothly adjusts the clock during the convergence period.
    /// 4. Snaps to the final accurate clock and enters the synchronized state.
    #[test]
    fn test_provisional_to_converging_to_synchronized() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();
        let clock_rate = VIDEO_FREQUENCY.get() as f64;

        // STEP 1: First packet arrives. We are now in the PROVISIONAL state.
        let first_packet_ts = MediaTime::from_90khz(1000);
        let packet1 = sync.process(RtpPacket {
            rtp_ts: first_packet_ts,
            arrival_ts: base_time,
            ..Default::default()
        });

        assert_eq!(packet1.playout_time, base_time);
        assert!(sync.is_provisional);
        assert!(!sync.is_synchronized());

        // STEP 2: An SR arrives. We start CONVERGING.
        let sr_arrival_time = base_time + Duration::from_millis(100);
        let sr_rtp_ts = MediaTime::from_90khz(9000); // Corresponds to 100ms of media
        let sr_ntp_time = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 10);
        let sr_info = create_sr(sr_rtp_ts, sr_ntp_time);

        let second_packet_ts = MediaTime::from_90khz(9180); // 2ms after SR
        let packet2 = sync.process(RtpPacket {
            rtp_ts: second_packet_ts,
            last_sender_info: Some(sr_info),
            arrival_ts: sr_arrival_time,
            ..Default::default()
        });

        let expected_provisional_delta = (9180 - 1000) as f64 / clock_rate;
        let expected_provisional_playout =
            base_time + Duration::from_secs_f64(expected_provisional_delta);

        assert_eq!(packet2.playout_time, expected_provisional_playout);
        assert!(!sync.is_provisional);
        assert!(sync.correction.is_some());
        assert!(!sync.is_synchronized());

        // STEP 3: A packet arrives mid-convergence.
        let mid_smear_time = sr_arrival_time + CLOCK_SMEAR_DURATION / 2; // Halfway through
        let third_packet_ts = MediaTime::from_90khz(31500); // Some time later
        let packet3 = sync.process(RtpPacket {
            rtp_ts: third_packet_ts,
            arrival_ts: mid_smear_time,
            ..Default::default()
        });

        let provisional_delta_3 = (31500 - 1000) as f64 / clock_rate;
        let provisional_playout_3 = base_time + Duration::from_secs_f64(provisional_delta_3);

        let accurate_delta_3 = (31500 - 9000) as f64 / clock_rate;
        let accurate_playout_3 = sr_arrival_time + Duration::from_secs_f64(accurate_delta_3);

        let midpoint_playout_3 =
            provisional_playout_3 + (accurate_playout_3 - provisional_playout_3) / 2;

        let playout_diff = if packet3.playout_time > midpoint_playout_3 {
            packet3.playout_time - midpoint_playout_3
        } else {
            midpoint_playout_3 - packet3.playout_time
        };
        assert!(
            playout_diff < Duration::from_millis(1),
            "Playout time is not halfway"
        );

        // STEP 4: A packet arrives AFTER convergence. We are now SYNCHRONIZED.
        let post_smear_time = sr_arrival_time + CLOCK_SMEAR_DURATION + Duration::from_millis(50);
        let fourth_packet_ts = MediaTime::from_90khz(54000);
        let packet4 = sync.process(RtpPacket {
            rtp_ts: fourth_packet_ts,
            arrival_ts: post_smear_time,
            ..Default::default()
        });

        assert!(sync.correction.is_none(), "Correction should be finished");
        assert!(sync.is_synchronized(), "Should now be synchronized");

        let expected_accurate_delta_4 = (54000 - 9000) as f64 / clock_rate;
        let expected_accurate_playout_4 =
            sr_arrival_time + Duration::from_secs_f64(expected_accurate_delta_4);

        assert_eq!(packet4.playout_time, expected_accurate_playout_4);
    }

    /// This is the definitive end-to-end test. It proves that the ultimate goal—producing
    /// synchronized playout times from multiple streams with different clock drifts—is achieved.
    #[test]
    fn test_playout_time_is_synchronized_across_drifting_streams() {
        let base_time = Instant::now();
        let ntp_base = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1_000_000);

        let mut sync_perfect = Synchronizer::new(VIDEO_FREQUENCY);
        let mut sync_drifting = Synchronizer::new(VIDEO_FREQUENCY);

        // Use integer rates to prevent test-side float inaccuracies
        let perfect_ticks_per_sec: u64 = 90_000;
        let drifting_ticks_per_sec: u64 = 90_090; // Exactly +1000 PPM

        // --- Phase 1 & 2: Synchronization and Drift Measurement ---
        let mut last_time = base_time;
        let mut last_ntp = ntp_base;
        let mut last_rtp_perfect: u64 = 0;
        let mut last_rtp_drifting: u64 = 0;

        // Process initial SR to get out of provisional state
        sync_perfect.process(RtpPacket {
            arrival_ts: last_time,
            last_sender_info: Some(create_sr(MediaTime::from_90khz(last_rtp_perfect), last_ntp)),
            ..Default::default()
        });
        sync_drifting.process(RtpPacket {
            arrival_ts: last_time,
            last_sender_info: Some(create_sr(
                MediaTime::from_90khz(last_rtp_drifting),
                last_ntp,
            )),
            ..Default::default()
        });

        // Wait for smear to finish
        last_time += CLOCK_SMEAR_DURATION + Duration::from_millis(100);
        sync_perfect.process(RtpPacket {
            arrival_ts: last_time,
            ..Default::default()
        });
        sync_drifting.process(RtpPacket {
            arrival_ts: last_time,
            ..Default::default()
        });

        // Send a few more SRs to get a stable drift measurement
        for _ in 0..3 {
            let interval = Duration::from_secs(1);
            last_time += interval;
            last_ntp += interval;

            last_rtp_perfect += perfect_ticks_per_sec;
            last_rtp_drifting += drifting_ticks_per_sec;

            sync_perfect.process(RtpPacket {
                arrival_ts: last_time,
                last_sender_info: Some(create_sr(
                    MediaTime::from_90khz(last_rtp_perfect),
                    last_ntp,
                )),
                ..Default::default()
            });
            sync_drifting.process(RtpPacket {
                arrival_ts: last_time,
                last_sender_info: Some(create_sr(
                    MediaTime::from_90khz(last_rtp_drifting),
                    last_ntp,
                )),
                ..Default::default()
            });
        }

        // --- Phase 3: Verification ---
        assert_eq!(sync_perfect.estimated_clock_drift_ppm.round() as i64, 0);
        assert_eq!(sync_drifting.estimated_clock_drift_ppm.round() as i64, 1000);

        // --- Phase 4: The Simultaneous Event ---
        let event_time = base_time + Duration::from_secs(10);
        let elapsed_for_event = event_time.duration_since(base_time).as_secs_f64();

        let rtp_perfect = (elapsed_for_event * perfect_ticks_per_sec as f64) as u64;
        let packet_perfect = sync_perfect.process(RtpPacket {
            rtp_ts: MediaTime::from_90khz(rtp_perfect),
            arrival_ts: event_time,
            ..Default::default()
        });

        let rtp_drifting = (elapsed_for_event * (drifting_ticks_per_sec as f64)) as u64;
        let packet_drifting = sync_drifting.process(RtpPacket {
            rtp_ts: MediaTime::from_90khz(rtp_drifting),
            arrival_ts: event_time,
            ..Default::default()
        });

        let playout_perfect = packet_perfect.playout_time;
        let playout_drifting = packet_drifting.playout_time;
        let diff = if playout_perfect > playout_drifting {
            playout_perfect - playout_drifting
        } else {
            playout_drifting - playout_perfect
        };

        assert!(
            diff < Duration::from_micros(1),
            "Playout times are not synchronized! Diff: {:?}",
            diff
        );
    }

    /// This is the most comprehensive test. It proves that playout time is synchronized
    /// for two independent streams, even when one has a drifting clock AND they use
    /// different NTP time bases (absolute vs. relative). This is the ultimate goal.
    #[test]
    fn test_playout_sync_across_drifting_streams_with_different_ntp_bases() {
        let base_time = Instant::now();

        // --- Setup: Two streams with different characteristics ---
        let mut sync_absolute_perfect = Synchronizer::new(VIDEO_FREQUENCY);
        let mut sync_relative_drifting = Synchronizer::new(VIDEO_FREQUENCY);

        // A realistic modern timestamp vs. a simple uptime-based one
        let absolute_ntp_base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let relative_ntp_base = UNIX_EPOCH + Duration::from_secs(300); // e.g., server was up for 5 mins

        // Perfect clock vs. a clock that is 0.1% fast (+1000 PPM)
        let perfect_ticks_per_sec: u64 = 90_000;
        let drifting_ticks_per_sec: u64 = 90_090;

        // --- Phase 1 & 2: Synchronization and Drift Measurement ---
        let mut last_time = base_time;
        let mut last_ntp_absolute = absolute_ntp_base;
        let mut last_ntp_relative = relative_ntp_base;
        let mut last_rtp_perfect: u64 = 0;
        let mut last_rtp_drifting: u64 = 0;

        // Process initial SRs to get out of provisional state
        sync_absolute_perfect.process(RtpPacket {
            arrival_ts: last_time,
            last_sender_info: Some(create_sr(
                MediaTime::from_90khz(last_rtp_perfect),
                last_ntp_absolute,
            )),
            ..Default::default()
        });
        sync_relative_drifting.process(RtpPacket {
            arrival_ts: last_time,
            last_sender_info: Some(create_sr(
                MediaTime::from_90khz(last_rtp_drifting),
                last_ntp_relative,
            )),
            ..Default::default()
        });

        // Wait for smear to finish
        last_time += CLOCK_SMEAR_DURATION + Duration::from_millis(100);
        sync_absolute_perfect.process(RtpPacket {
            arrival_ts: last_time,
            ..Default::default()
        });
        sync_relative_drifting.process(RtpPacket {
            arrival_ts: last_time,
            ..Default::default()
        });

        // Send a few more SRs to get a stable drift measurement
        for _ in 0..3 {
            let interval = Duration::from_secs(1);
            last_time += interval;
            last_ntp_absolute += interval;
            last_ntp_relative += interval;

            last_rtp_perfect += perfect_ticks_per_sec;
            last_rtp_drifting += drifting_ticks_per_sec;

            sync_absolute_perfect.process(RtpPacket {
                arrival_ts: last_time,
                last_sender_info: Some(create_sr(
                    MediaTime::from_90khz(last_rtp_perfect),
                    last_ntp_absolute,
                )),
                ..Default::default()
            });
            sync_relative_drifting.process(RtpPacket {
                arrival_ts: last_time,
                last_sender_info: Some(create_sr(
                    MediaTime::from_90khz(last_rtp_drifting),
                    last_ntp_relative,
                )),
                ..Default::default()
            });
        }

        // --- Phase 3: Verification of Drift Calculation ---
        assert!(sync_absolute_perfect.is_synchronized());
        assert!(sync_relative_drifting.is_synchronized());
        assert_eq!(
            sync_absolute_perfect.estimated_clock_drift_ppm.round() as i64,
            0
        );
        assert_eq!(
            sync_relative_drifting.estimated_clock_drift_ppm.round() as i64,
            1000
        );

        // --- Phase 4: The Simultaneous Event and Final Assertion ---
        let event_time = base_time + Duration::from_secs(10);
        let elapsed_for_event = event_time.duration_since(base_time).as_secs_f64();

        // Packet from the perfect-clock, absolute-NTP stream
        let rtp_perfect = (elapsed_for_event * perfect_ticks_per_sec as f64) as u64;
        let packet_perfect = sync_absolute_perfect.process(RtpPacket {
            rtp_ts: MediaTime::from_90khz(rtp_perfect),
            arrival_ts: event_time,
            ..Default::default()
        });

        // Packet from the drifting-clock, relative-NTP stream
        let rtp_drifting = (elapsed_for_event * (drifting_ticks_per_sec as f64)) as u64;
        let packet_drifting = sync_relative_drifting.process(RtpPacket {
            rtp_ts: MediaTime::from_90khz(rtp_drifting),
            arrival_ts: event_time,
            ..Default::default()
        });

        // THE CRITICAL ASSERTION:
        // Despite different NTP bases and a drifting clock, the final playout times must be aligned.
        let playout_perfect = packet_perfect.playout_time;
        let playout_drifting = packet_drifting.playout_time;
        let diff = if playout_perfect > playout_drifting {
            playout_perfect - playout_drifting
        } else {
            playout_drifting - playout_perfect
        };

        assert!(
            diff < Duration::from_micros(1),
            "Playout times are not synchronized across different NTP bases! Diff: {:?}",
            diff
        );
    }

    #[test]
    fn test_clock_drift_detection() {
        // This test remains valid as it already tests the differential calculation.
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();
        let ntp_base_time = UNIX_EPOCH + Duration::from_secs(100);

        // First SR establishes the baseline
        sync.process(RtpPacket {
            arrival_ts: base_time,
            last_sender_info: Some(create_sr(MediaTime::from_90khz(0), ntp_base_time)),
            ..Default::default()
        });

        // We need to wait for the smear to finish before the clock is stable
        let after_smear_time = base_time + CLOCK_SMEAR_DURATION + Duration::from_millis(100);
        sync.process(RtpPacket {
            arrival_ts: after_smear_time,
            ..Default::default()
        });
        assert!(sync.is_synchronized());

        // Second SR is used to calculate the first drift estimate
        let sr2_time = base_time + Duration::from_secs(1);
        sync.process(RtpPacket {
            arrival_ts: sr2_time,
            last_sender_info: Some(create_sr(
                MediaTime::from_90khz(90000),
                ntp_base_time + Duration::from_secs(1),
            )),
            ..Default::default()
        });

        // Third SR is used to calculate a more stable drift
        let sr3_time = base_time + Duration::from_secs(2);
        sync.process(RtpPacket {
            arrival_ts: sr3_time,
            last_sender_info: Some(create_sr(
                MediaTime::from_90khz(180090),
                ntp_base_time + Duration::from_secs(2),
            )),
            ..Default::default()
        });

        assert_eq!(sync.estimated_clock_drift_ppm.round() as i64, 1000);
    }
}
