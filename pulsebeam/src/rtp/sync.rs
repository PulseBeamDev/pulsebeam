use crate::rtp::RtpPacket;
use metrics::{counter, gauge, histogram};
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
use str0m::{
    media::{Frequency, MediaTime},
    rtp::rtcp::SenderInfo,
};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// The maximum number of sender reports to store for clock drift analysis.
const SR_HISTORY_CAPACITY: usize = 5;

/// Maximum acceptable clock drift between the sender and receiver clocks.
/// A 10% tolerance is allowed between sender reports to account for network jitter and
/// minor clock speed differences.
const MAX_SR_CLOCK_DEVIATION: f64 = 0.10;

/// The minimum difference in RTP ticks between two consecutive sender reports to be considered valid.
/// This corresponds to ~100ms at a 90kHz clock rate and prevents spurious updates from closely timed reports.
const MIN_SR_ERROR_TOLERANCE: i64 = 9000;

/// The minimum time that must elapse before the synchronizer updates its time reference.
/// This prevents timeline instability caused by network jitter affecting sender report arrival times.
const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

/// The maximum duration in the future a calculated playout time can be.
/// Playout times beyond this threshold are considered invalid to prevent erroneous timeline jumps.
const MAX_PLAYOUT_FUTURE: Duration = Duration::from_secs(10);

/// The maximum duration in the past a calculated playout time can be.
/// This prevents processing excessively delayed or out-of-order packets.
const MAX_PLAYOUT_PAST: Duration = Duration::from_secs(5);

/// A mapping that represents a stable anchor point between the sender's clock domain
/// (RTP and NTP timestamps) and the receiver's local monotonic clock domain (`Instant`).
///
/// This anchor is created from a validated Sender Report and serves as the reference
/// for calculating the playout time of all subsequent RTP packets.
#[derive(Debug, Clone, Copy)]
struct RtpWallClockMapping {
    /// The RTP timestamp from the Sender Report.
    rtp_time: MediaTime,
    /// The NTP timestamp from the Sender Report. Used to validate the order of reports.
    ntp_time: SystemTime,
    /// The local monotonic time when the Sender Report was received.
    wall_clock: Instant,
}

/// The `Synchronizer` is responsible for translating RTP timestamps from a sender
/// into a consistent playout timeline on the local monotonic clock. It is designed
/// to be resilient to network jitter, packet reordering, and sender clock drift.
///
/// It works by creating a mapping between the sender's clock and the local clock using
/// RTCP Sender Reports (SRs). The NTP timestamp in the SR is used as a monotonic clock
/// to ensure that only newer reports can update the timeline, making the system work correctly
/// even if the sender's clock is not synchronized to an absolute time source (i.e., it is a relative clock).
#[derive(Debug)]
pub struct Synchronizer {
    /// The clock rate of the media stream (e.g., 90000 Hz for video).
    clock_rate: Frequency,
    /// A history of recent, validated sender report mappings for clock drift analysis.
    sr_history: VecDeque<RtpWallClockMapping>,
    /// The current reference mapping used to calculate the playout timeline. This is the
    /// single source of truth for time translation.
    rtp_offset: Option<RtpWallClockMapping>,
    /// The NTP timestamp from the latest validated Sender Report. This is used to
    /// reject stale or reordered reports, ensuring the timeline only moves forward.
    last_sr_ntp: Option<SystemTime>,
    /// The local time when the `rtp_offset` was last updated, used for rate-limiting updates.
    last_offset_update: Option<Instant>,
    /// The estimated clock drift between the sender and receiver, in parts-per-million (PPM).
    estimated_clock_drift_ppm: f64,
    /// The identifier for the stream being synchronized, used for metrics.
    stream_id: String,
}

impl Synchronizer {
    /// Creates a new `Synchronizer` for a stream with a specified clock rate.
    pub fn new(clock_rate: Frequency, stream_id: String) -> Self {
        info!(
            stream_id = %stream_id,
            clock_rate = %clock_rate.get(),
            "Initializing RTP synchronizer"
        );

        gauge!("rtp_sync_clock_rate", "stream_id" => stream_id.clone())
            .set(clock_rate.get() as f64);

        Self {
            clock_rate,
            sr_history: VecDeque::with_capacity(SR_HISTORY_CAPACITY),
            rtp_offset: None,
            last_sr_ntp: None,
            last_offset_update: None,
            estimated_clock_drift_ppm: 0.0,
            stream_id,
        }
    }

    /// Processes an `RtpPacket` to calculate and assign its synchronized `playout_time`.
    ///
    /// The process involves three main steps:
    /// 1. If the packet contains a Sender Report, validate it and potentially update the time reference.
    /// 2. Calculate the packet's playout time based on the current time reference.
    /// 3. Validate that the calculated playout time is within a reasonable range.
    pub fn process(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        counter!("rtp_sync_packets_processed", "stream_id" => self.stream_id.clone()).increment(1);

        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, now);
        }

        if self.rtp_offset.is_none() {
            // This is the first packet for the stream, and no Sender Report has been received yet.
            // We establish a temporary time reference based on the packet's arrival time.
            // This reference will be corrected as soon as the first valid SR arrives.
            warn!(
                stream_id = %self.stream_id,
                rtp_ts = %packet.rtp_ts,
                "First packet seen without a Sender Report. Establishing temporary time reference."
            );

            counter!("rtp_sync_temporary_offset_created", "stream_id" => self.stream_id.clone())
                .increment(1);

            self.rtp_offset = Some(RtpWallClockMapping {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH, // A placeholder as this is an estimate.
                wall_clock: now,
            });
            self.last_offset_update = Some(now);
        }

        let playout_time = self.calculate_playout_time(packet.rtp_ts, now);

        if let Some(validated_playout) = self.validate_playout_time(playout_time, now) {
            packet.playout_time = Some(validated_playout);

            let delta = if validated_playout >= now {
                validated_playout.duration_since(now).as_secs_f64()
            } else {
                -(now.duration_since(validated_playout).as_secs_f64())
            };

            histogram!("rtp_sync_playout_delta_seconds", "stream_id" => self.stream_id.clone())
                .record(delta);
        } else {
            // If the calculated time is unreasonable, fall back to the current time.
            warn!(
                stream_id = %self.stream_id,
                rtp_ts = %packet.rtp_ts,
                calculated_playout = ?playout_time,
                "Playout time validation failed, using current time as fallback."
            );
            packet.playout_time = Some(now);
            counter!("rtp_sync_playout_validation_failed", "stream_id" => self.stream_id.clone())
                .increment(1);
        }

        packet
    }

    /// Validates and processes a new `SenderInfo` report to update the time reference.
    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        debug!(
            stream_id = %self.stream_id,
            rtp_ts = %sr.rtp_time,
            ntp_ts = ?sr.ntp_time,
            "Processing Sender Report"
        );

        counter!("rtp_sync_sr_received", "stream_id" => self.stream_id.clone()).increment(1);

        // Check 1: Reject stale or reordered SRs.
        // The NTP timestamp is treated as a monotonic clock from the sender. This ensures
        // that a delayed, older SR does not rewind our established timeline.
        if let Some(last_ntp) = self.last_sr_ntp {
            if sr.ntp_time <= last_ntp {
                debug!(
                    stream_id = %self.stream_id,
                    current_ntp = ?sr.ntp_time,
                    last_seen_ntp = ?last_ntp,
                    "Ignoring stale/reordered sender report"
                );
                counter!("rtp_sync_sr_rejected_stale", "stream_id" => self.stream_id.clone())
                    .increment(1);
                return;
            }
        }

        // Check 2: Rate-limit SR updates to protect against jitter.
        // This prevents rapid, small adjustments to the timeline caused by fluctuating network delay.
        if let Some(last_update) = self.last_offset_update {
            let time_since_update = now.duration_since(last_update);
            if time_since_update < MIN_SR_UPDATE_INTERVAL {
                debug!(
                    stream_id = %self.stream_id,
                    time_since_update_ms = time_since_update.as_millis(),
                    min_interval_ms = MIN_SR_UPDATE_INTERVAL.as_millis(),
                    "Skipping SR update: too soon after last update"
                );
                counter!("rtp_sync_sr_rejected_jitter", "stream_id" => self.stream_id.clone())
                    .increment(1);
                return;
            }

            histogram!("rtp_sync_sr_interval_seconds", "stream_id" => self.stream_id.clone())
                .record(time_since_update.as_secs_f64());
        }

        // Check 3: Validate that the new SR is consistent with the existing timeline.
        // This checks for significant clock drift that could indicate a problem with the sender.
        if self.sr_history.len() >= 2 {
            match self.validate_sr_consistency(&sr, now) {
                Ok(clock_drift_ppm) => {
                    self.estimated_clock_drift_ppm = clock_drift_ppm;
                    gauge!("rtp_sync_clock_drift_ppm", "stream_id" => self.stream_id.clone())
                        .set(clock_drift_ppm);
                }
                Err(reason) => {
                    error!(
                        stream_id = %self.stream_id,
                        rtp_ts = %sr.rtp_time,
                        ntp_ts = ?sr.ntp_time,
                        reason = %reason,
                        "Sender Report failed consistency check, ignoring."
                    );
                    counter!("rtp_sync_sr_rejected_inconsistent", "stream_id" => self.stream_id.clone()).increment(1);
                    return;
                }
            }
        }

        // The SR has passed all validation checks. Update the timeline reference.
        if self.last_sr_ntp.is_none() {
            info!(
                stream_id = %self.stream_id,
                rtp_ts = %sr.rtp_time,
                ntp_ts = ?sr.ntp_time,
                "Received first valid Sender Report. Timeline established."
            );
            counter!("rtp_sync_timeline_established", "stream_id" => self.stream_id.clone())
                .increment(1);
        }

        self.last_sr_ntp = Some(sr.ntp_time);

        let mapping = RtpWallClockMapping {
            rtp_time: sr.rtp_time,
            ntp_time: sr.ntp_time,
            wall_clock: now,
        };

        if self.sr_history.len() == SR_HISTORY_CAPACITY {
            self.sr_history.pop_front();
        }
        self.sr_history.push_back(mapping);

        // Set the new mapping as the active reference for playout time calculations.
        self.rtp_offset = Some(mapping);
        self.last_offset_update = Some(now);

        counter!("rtp_sync_sr_accepted", "stream_id" => self.stream_id.clone()).increment(1);
    }

    /// Checks if a new Sender Report is consistent with previous reports by analyzing clock drift.
    ///
    /// It compares the time elapsed on the local clock with the time elapsed on the sender's
    /// RTP clock. A significant deviation indicates a potential issue.
    ///
    /// Returns `Ok(clock_drift_ppm)` if consistent, `Err(reason)` otherwise.
    fn validate_sr_consistency(&self, sr: &SenderInfo, now: Instant) -> Result<f64, String> {
        let last_mapping = self
            .sr_history
            .back()
            .ok_or_else(|| "SR history is empty".to_string())?;

        // Calculate time elapsed on the local (receiver's) clock.
        let wall_delta = now.duration_since(last_mapping.wall_clock);
        let expected_rtp_delta = (wall_delta.as_secs_f64() * self.clock_rate.get() as f64) as i64;

        // Calculate time elapsed on the remote (sender's) RTP clock.
        // `wrapping_sub` correctly handles the 32-bit RTP timestamp wraparound.
        let actual_rtp_delta =
            sr.rtp_time
                .numer()
                .wrapping_sub(last_mapping.rtp_time.numer()) as i32 as i64;

        // The difference between expected and actual RTP deltas indicates clock drift.
        let delta_diff = (actual_rtp_delta - expected_rtp_delta).abs();
        let acceptable_error = ((expected_rtp_delta.abs() as f64 * MAX_SR_CLOCK_DEVIATION) as i64)
            .max(MIN_SR_ERROR_TOLERANCE);

        if delta_diff > acceptable_error {
            return Err(format!(
                "RTP delta deviation exceeds tolerance: expected {}, got {}, diff {} (max {})",
                expected_rtp_delta, actual_rtp_delta, delta_diff, acceptable_error
            ));
        }

        // Calculate clock drift in parts-per-million (PPM).
        let clock_drift_ppm = if expected_rtp_delta > 0 {
            ((actual_rtp_delta as f64 - expected_rtp_delta as f64) / expected_rtp_delta as f64)
                * 1_000_000.0
        } else {
            0.0
        };

        Ok(clock_drift_ppm)
    }

    /// Calculates the playout time for an RTP timestamp based on the current reference mapping.
    ///
    /// The formula is:
    /// `playout_time = reference_wall_clock + (packet_rtp_ts - reference_rtp_ts)`
    ///
    /// This calculation is resilient to RTP timestamp wraparound because it uses `wrapping_sub`.
    fn calculate_playout_time(&self, rtp_ts: MediaTime, now: Instant) -> Instant {
        if let Some(offset) = self.rtp_offset {
            // Calculate the difference in RTP ticks between this packet and the reference SR.
            let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer()) as i32;
            let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64;

            // Apply the delta to the reference wall_clock to get the final playout time.
            if seconds_delta >= 0.0 {
                offset.wall_clock + Duration::from_secs_f64(seconds_delta)
            } else {
                // The packet is older than the reference point, so subtract the duration.
                offset
                    .wall_clock
                    .checked_sub(Duration::from_secs_f64(-seconds_delta))
                    .unwrap_or(now) // Fallback to `now` in case of underflow.
            }
        } else {
            // This should rarely happen after the first packet.
            warn!(
                stream_id = %self.stream_id,
                "Calculating playout time without a valid time reference; falling back to packet arrival time."
            );
            counter!("rtp_sync_fallback_to_arrival", "stream_id" => self.stream_id.clone())
                .increment(1);
            now
        }
    }

    /// Validates that a calculated playout time is within a reasonable window relative to the current time.
    fn validate_playout_time(&self, playout_time: Instant, now: Instant) -> Option<Instant> {
        if playout_time > now + MAX_PLAYOUT_FUTURE {
            warn!(
                stream_id = %self.stream_id,
                future_delta_secs = (playout_time - now).as_secs_f64(),
                "Calculated playout time is too far in the future."
            );
            return None;
        }

        if let Some(past_limit) = now.checked_sub(MAX_PLAYOUT_PAST) {
            if playout_time < past_limit {
                warn!(
                    stream_id = %self.stream_id,
                    past_delta_secs = (now - playout_time).as_secs_f64(),
                    "Calculated playout time is too far in the past."
                );
                return None;
            }
        }

        Some(playout_time)
    }

    /// Returns `true` if the synchronizer has received at least one valid Sender Report
    /// and has established a stable timeline.
    pub fn is_synchronized(&self) -> bool {
        self.last_sr_ntp.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::VIDEO_FREQUENCY;
    use rtp_packet::RtpPacket;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Helper to create a `SenderInfo` for testing.
    fn create_sr(rtp_ts_num: u32, ntp_secs_since_epoch: u64) -> SenderInfo {
        let ntp_time = UNIX_EPOCH + Duration::from_secs(ntp_secs_since_epoch);
        SenderInfo {
            rtp_time: MediaTime::new(rtp_ts_num as u64, VIDEO_FREQUENCY),
            ntp_time,
            packet_count: 0,
            octet_count: 0,
        }
    }

    #[test]
    fn test_initialization_and_first_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. First packet arrives with no SR. It should get a temporary playout time equal to its arrival time.
        let mut packet1 = RtpPacket::default();
        packet1.rtp_ts = MediaTime::new(1000, VIDEO_FREQUENCY);
        packet1 = sync.process(packet1, base_time);
        assert_eq!(
            packet1.playout_time,
            Some(base_time),
            "First packet should use arrival time as temporary playout time"
        );
        assert!(!sync.is_synchronized());

        // 2. A packet with the first SR arrives later.
        let sr_time = base_time + Duration::from_millis(300);
        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(2000, VIDEO_FREQUENCY);
        packet2.last_sender_info = Some(create_sr(1800, 10)); // SR corresponds to RTP 1800
        packet2 = sync.process(packet2, sr_time);

        // Playout time should now be based on the SR's reference.
        // RTP delta from reference = 2000 - 1800 = 200 ticks.
        let expected_delta_secs = 200.0 / VIDEO_FREQUENCY.get() as f64;
        let expected_playout_time = sr_time + Duration::from_secs_f64(expected_delta_secs);
        assert_eq!(packet2.playout_time, Some(expected_playout_time));
        assert!(sync.is_synchronized());
    }

    #[test]
    fn test_ignores_stale_reordered_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Process a valid SR, establishing the timeline with NTP time 101.
        let mut packet_new = RtpPacket::default();
        packet_new.last_sender_info = Some(create_sr(90000, 101));
        sync.process(packet_new.clone(), base_time);

        let initial_offset = sync.rtp_offset.unwrap();
        assert_eq!(
            sync.last_sr_ntp.unwrap(),
            UNIX_EPOCH + Duration::from_secs(101)
        );

        // 2. Process an older SR that arrived late (NTP time 100).
        let late_packet_time = base_time + Duration::from_millis(300);
        let mut packet_old = RtpPacket::default();
        packet_old.last_sender_info = Some(create_sr(45000, 100)); // Older NTP time
        sync.process(packet_old, late_packet_time);

        // 3. Assert that the synchronizer ignored the old SR and its offset is unchanged.
        assert_eq!(
            sync.last_sr_ntp.unwrap(),
            UNIX_EPOCH + Duration::from_secs(101),
            "Synchronizer should not update its NTP time with the stale report"
        );
        assert_eq!(sync.rtp_offset.unwrap().rtp_time, initial_offset.rtp_time);
        assert_eq!(
            sync.rtp_offset.unwrap().wall_clock,
            initial_offset.wall_clock
        );
    }

    #[test]
    fn test_timeline_updates_with_new_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Establish initial timeline.
        let mut initial_packet = RtpPacket::default();
        initial_packet.last_sender_info = Some(create_sr(1000, 200));
        sync.process(initial_packet, base_time);

        // 2. 5 seconds later, a new SR arrives.
        let new_time = base_time + Duration::from_secs(5);
        let current_rtp_ts = 1000 + (VIDEO_FREQUENCY.get() * 5); // Expected RTP after 5s
        let sr_rtp_ts = current_rtp_ts - 900; // SR is for a packet 10ms in the past

        let mut drift_packet = RtpPacket::default();
        drift_packet.rtp_ts = MediaTime::new(current_rtp_ts, VIDEO_FREQUENCY);
        drift_packet.last_sender_info = Some(create_sr(sr_rtp_ts as u32, 205));
        drift_packet = sync.process(drift_packet, new_time);

        // 3. The offset should now be updated to the new reference.
        let offset = sync.rtp_offset.unwrap();
        assert_eq!(offset.rtp_time.numer(), sr_rtp_ts);
        assert_eq!(offset.wall_clock, new_time);

        // 4. Playout time should be calculated from this *new* reference.
        let expected_delta_secs =
            (current_rtp_ts - sr_rtp_ts) as f64 / VIDEO_FREQUENCY.get() as f64;
        let expected_playout_time = new_time + Duration::from_secs_f64(expected_delta_secs);
        assert_eq!(drift_packet.playout_time, Some(expected_playout_time));
    }

    #[test]
    fn test_rtp_timestamp_wraparound() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();
        let u32_max = u32::MAX as u64;

        // 1. Establish a reference point very close to the wraparound point.
        let initial_rtp = u32_max - 45000; // 0.5s before wraparound
        let mut packet1 = RtpPacket::default();
        packet1.last_sender_info = Some(create_sr(initial_rtp as u32, 300));
        sync.process(packet1, base_time);

        // 2. 1 second later, process a packet whose timestamp has wrapped around.
        let new_time = base_time + Duration::from_secs(1);
        let wrapped_rtp = initial_rtp.wrapping_add(90000); // Add 1s of ticks
        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(wrapped_rtp, VIDEO_FREQUENCY);
        packet2 = sync.process(packet2, new_time);

        // 3. The calculated playout time should be exactly 1 second after the reference's arrival time.
        let playout_time = packet2.playout_time.unwrap();
        assert_eq!(
            playout_time,
            base_time + Duration::from_secs(1),
            "Playout time should be exactly 1 second after reference"
        );
    }

    #[test]
    fn test_playout_time_of_late_media_packet() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Establish timeline with an SR. Reference is RTP 180000 at base_time.
        let mut sr_packet = RtpPacket::default();
        sr_packet.last_sender_info = Some(create_sr(180000, 402));
        sync.process(sr_packet, base_time);

        // 2. A packet that *should* have arrived 100ms ago arrives now, 500ms after the reference was set.
        let late_arrival_time = base_time + Duration::from_millis(500);
        let mut late_packet = RtpPacket::default();
        let late_packet_rtp = 180000 - 9000; // 0.1s before the SR's RTP time
        late_packet.rtp_ts = MediaTime::new(late_packet_rtp, VIDEO_FREQUENCY);
        late_packet = sync.process(late_packet, late_arrival_time);

        // 3. The calculated playout time should be in the past relative to the reference.
        // It should be 0.1s *before* the reference wall_clock time.
        let expected_playout_time = base_time - Duration::from_millis(100);
        assert_eq!(
            late_packet.playout_time,
            Some(expected_playout_time),
            "Playout time for a late packet should be calculated correctly in the past"
        );
    }

    #[test]
    fn test_sr_jitter_protection() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. First SR establishes timeline
        let mut packet1 = RtpPacket::default();
        packet1.last_sender_info = Some(create_sr(90000, 100));
        sync.process(packet1, base_time);
        let first_offset = sync.rtp_offset.unwrap();

        // 2. Another SR arrives 100ms later (below MIN_SR_UPDATE_INTERVAL), so it should be ignored.
        let sr2_time = base_time + Duration::from_millis(100);
        let mut packet2 = RtpPacket::default();
        packet2.last_sender_info = Some(create_sr(99000, 101));
        sync.process(packet2, sr2_time);

        // 3. Offset should NOT have changed.
        let second_offset = sync.rtp_offset.unwrap();
        assert_eq!(first_offset.rtp_time, second_offset.rtp_time);
        assert_eq!(first_offset.wall_clock, second_offset.wall_clock);

        // 4. A third SR arrives 250ms after the first (above threshold) and should be accepted.
        let sr3_time = base_time + Duration::from_millis(250);
        let mut packet3 = RtpPacket::default();
        packet3.last_sender_info = Some(create_sr(112500, 102));
        sync.process(packet3, sr3_time);

        // 5. The offset should have been updated.
        let third_offset = sync.rtp_offset.unwrap();
        assert_eq!(third_offset.rtp_time.numer(), 112500);
        assert_eq!(third_offset.wall_clock, sr3_time);
    }

    #[test]
    fn test_clock_drift_detection() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // Process three SRs to build up history for drift calculation.
        // SR 1: Baseline
        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(0, 100)),
                ..Default::default()
            },
            base_time,
        );

        // SR 2: 1 second later, perfect clock
        let sr2_time = base_time + Duration::from_secs(1);
        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(90000, 101)),
                ..Default::default()
            },
            sr2_time,
        );

        // SR 3: Another second later, but sender's clock is slightly fast.
        // Expected RTP is 180000, but sender sends 180090 (90 ticks fast).
        let sr3_time = base_time + Duration::from_secs(2);
        sync.process(
            RtpPacket {
                last_sender_info: Some(create_sr(180090, 102)),
                ..Default::default()
            },
            sr3_time,
        );

        // Drift should be detected. Expected drift is (90 / 90000) * 1,000,000 = 1000 PPM.
        assert_eq!(sync.estimated_clock_drift_ppm.round() as i64, 1000);
    }
}
