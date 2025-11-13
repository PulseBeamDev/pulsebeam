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

/// The maximum number of sender reports to maintain in the history for analysis or future heuristics.
const SR_HISTORY_CAPACITY: usize = 5;

/// Maximum acceptable deviation between predicted and actual SR timing (10% tolerance for clock drift)
const MAX_SR_CLOCK_DEVIATION: f64 = 0.10;

/// Minimum absolute error tolerance in RTP ticks (100ms at 90kHz)
const MIN_SR_ERROR_TOLERANCE: i64 = 9000;

/// Minimum time between SR offset updates to avoid jitter (200ms)
const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

/// Maximum reasonable playout time in the future (10 seconds)
const MAX_PLAYOUT_FUTURE: Duration = Duration::from_secs(10);

/// Maximum reasonable playout time in the past (5 seconds)
const MAX_PLAYOUT_PAST: Duration = Duration::from_secs(5);

/// Represents a validated mapping between a sender's clock (RTP/NTP) and the SFU's wall-clock.
#[derive(Debug, Clone, Copy)]
struct RtpWallClockMapping {
    rtp_time: MediaTime,
    ntp_time: SystemTime,
    wall_clock: Instant,
}

#[derive(Debug)]
pub struct Synchronizer {
    /// The clock rate of the RTP stream (e.g., 90000 for video).
    clock_rate: Frequency,
    /// A history of the most recent, validated sender reports.
    sr_history: VecDeque<RtpWallClockMapping>,
    /// The current reference mapping used to calculate the playout timeline.
    rtp_offset: Option<RtpWallClockMapping>,
    /// The NTP timestamp from the latest valid Sender Report seen. This is the key to
    /// preventing timeline corruption from reordered packets.
    last_sr_ntp: Option<SystemTime>,
    /// Last time we updated rtp_offset (for jitter protection)
    last_offset_update: Option<Instant>,
    /// Track estimated clock drift for monitoring
    estimated_clock_drift_ppm: f64,
    /// Metric labels for this stream
    stream_id: String,
}

impl Synchronizer {
    /// Creates a new synchronizer for a stream with a given clock rate.
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

    /// Processes an RTP packet to calculate and assign its synchronized `playout_time`.
    ///
    /// This is the main entry point for the synchronizer. It inspects the packet for a
    /// Sender Report, validates it, updates the internal time reference if necessary, and
    /// then computes the packet's absolute playout time on the SFU timeline.
    pub fn process(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        counter!("rtp_sync_packets_processed", "stream_id" => self.stream_id.clone()).increment(1);

        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, now);
        }

        if self.rtp_offset.is_none() {
            // This is the first packet ever seen for this stream. We have no SR yet,
            // so we must establish a temporary, estimated offset. This will be corrected
            // as soon as the first valid SR arrives.
            warn!(
                stream_id = %self.stream_id,
                rtp_ts = %packet.rtp_ts,
                "First packet arrived without a Sender Report, establishing temporary time reference."
            );

            counter!("rtp_sync_temporary_offset_created", "stream_id" => self.stream_id.clone())
                .increment(1);

            self.rtp_offset = Some(RtpWallClockMapping {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH, // Placeholder NTP as this is an estimate.
                wall_clock: now,
            });
            self.last_offset_update = Some(now);
        }

        let playout_time = self.calculate_playout_time(packet.rtp_ts, now);

        // Validate playout time is reasonable
        if let Some(validated_playout) = self.validate_playout_time(playout_time, now) {
            packet.playout_time = Some(validated_playout);

            // Record playout time delta for monitoring
            let delta = if validated_playout >= now {
                validated_playout.duration_since(now).as_secs_f64()
            } else {
                -(now.duration_since(validated_playout).as_secs_f64())
            };

            histogram!("rtp_sync_playout_delta_seconds", "stream_id" => self.stream_id.clone())
                .record(delta);
        } else {
            // Fallback to current time if validation fails
            warn!(
                stream_id = %self.stream_id,
                rtp_ts = %packet.rtp_ts,
                calculated_playout = ?playout_time,
                "Playout time validation failed, using current time"
            );
            packet.playout_time = Some(now);
            counter!("rtp_sync_playout_validation_failed", "stream_id" => self.stream_id.clone())
                .increment(1);
        }

        packet
    }

    /// Validates and incorporates a new Sender Report into the time reference.
    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        debug!(
            stream_id = %self.stream_id,
            rtp_ts = %sr.rtp_time,
            ntp_ts = ?sr.ntp_time,
            "Processing Sender Report"
        );

        counter!("rtp_sync_sr_received", "stream_id" => self.stream_id.clone()).increment(1);

        // Check 1: Reject reordered/stale SRs based on NTP timestamp
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

        // Check 2: Don't update too frequently (jitter protection)
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

        // Check 3: Validate SR consistency with recent history
        if self.sr_history.len() >= 2 {
            match self.validate_sr_consistency(&sr, now) {
                Ok(clock_drift_ppm) => {
                    self.estimated_clock_drift_ppm = clock_drift_ppm;
                    gauge!("rtp_sync_clock_drift_ppm", "stream_id" => self.stream_id.clone())
                        .set(clock_drift_ppm);

                    if clock_drift_ppm.abs() > 100.0 {
                        warn!(
                            stream_id = %self.stream_id,
                            clock_drift_ppm = clock_drift_ppm,
                            "Significant clock drift detected"
                        );
                    }
                }
                Err(reason) => {
                    error!(
                        stream_id = %self.stream_id,
                        rtp_ts = %sr.rtp_time,
                        ntp_ts = ?sr.ntp_time,
                        reason = %reason,
                        "SR failed consistency check, ignoring"
                    );
                    counter!("rtp_sync_sr_rejected_inconsistent", "stream_id" => self.stream_id.clone()).increment(1);
                    return;
                }
            }
        }

        // SR is valid, update our state
        if self.last_sr_ntp.is_none() {
            info!(
                stream_id = %self.stream_id,
                rtp_ts = %sr.rtp_time,
                ntp_ts = ?sr.ntp_time,
                "Received first Sender Report, timeline established"
            );
            counter!("rtp_sync_timeline_established", "stream_id" => self.stream_id.clone())
                .increment(1);
        } else {
            debug!(
                stream_id = %self.stream_id,
                rtp_ts = %sr.rtp_time,
                ntp_ts = ?sr.ntp_time,
                "Received new Sender Report, updating time reference"
            );
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

        self.rtp_offset = Some(mapping);
        self.last_offset_update = Some(now);

        counter!("rtp_sync_sr_accepted", "stream_id" => self.stream_id.clone()).increment(1);
        gauge!("rtp_sync_sr_history_size", "stream_id" => self.stream_id.clone())
            .set(self.sr_history.len() as f64);
    }

    /// Validates that a new SR is consistent with recent history.
    /// Returns Ok(clock_drift_ppm) if valid, Err(reason) if invalid.
    fn validate_sr_consistency(&self, sr: &SenderInfo, now: Instant) -> Result<f64, String> {
        let last_mapping = self
            .sr_history
            .back()
            .ok_or_else(|| "No previous mapping available".to_string())?;

        // Calculate expected RTP delta based on wall clock time
        let wall_delta = now.duration_since(last_mapping.wall_clock);
        let expected_rtp_delta = (wall_delta.as_secs_f64() * self.clock_rate.get() as f64) as i64;

        // Calculate actual RTP delta (handle wraparound properly)
        let actual_rtp_delta =
            sr.rtp_time
                .numer()
                .wrapping_sub(last_mapping.rtp_time.numer()) as i32 as i64;

        // Reject if the delta is too small (< 10ms) - likely a duplicate or error
        if actual_rtp_delta < (self.clock_rate.get() as i64 / 100) {
            return Err(format!(
                "RTP delta too small: {} ticks (< 10ms)",
                actual_rtp_delta
            ));
        }

        // Check if delta is reasonable (within tolerance for clock drift)
        let delta_diff = (actual_rtp_delta - expected_rtp_delta).abs();
        let acceptable_error = ((expected_rtp_delta.abs() as f64 * MAX_SR_CLOCK_DEVIATION) as i64)
            .max(MIN_SR_ERROR_TOLERANCE);

        if delta_diff > acceptable_error {
            return Err(format!(
                "RTP delta deviation too large: expected {}, got {}, diff {} (max acceptable: {})",
                expected_rtp_delta, actual_rtp_delta, delta_diff, acceptable_error
            ));
        }

        // Calculate clock drift in parts per million (ppm)
        let clock_drift_ppm = if expected_rtp_delta > 0 {
            ((actual_rtp_delta as f64 - expected_rtp_delta as f64) / expected_rtp_delta as f64)
                * 1_000_000.0
        } else {
            0.0
        };

        Ok(clock_drift_ppm)
    }

    /// Validates that a calculated playout time is reasonable.
    /// Returns Some(playout_time) if valid, None if it should be rejected.
    fn validate_playout_time(&self, playout_time: Instant, now: Instant) -> Option<Instant> {
        // Check if playout time is too far in the future
        if playout_time > now + MAX_PLAYOUT_FUTURE {
            warn!(
                stream_id = %self.stream_id,
                future_delta_secs = (playout_time - now).as_secs_f64(),
                max_future_secs = MAX_PLAYOUT_FUTURE.as_secs_f64(),
                "Playout time too far in future"
            );
            return None;
        }

        // Check if playout time is too far in the past
        if playout_time < now.checked_sub(MAX_PLAYOUT_PAST)? {
            warn!(
                stream_id = %self.stream_id,
                past_delta_secs = (now - playout_time).as_secs_f64(),
                max_past_secs = MAX_PLAYOUT_PAST.as_secs_f64(),
                "Playout time too far in past"
            );
            return None;
        }

        Some(playout_time)
    }

    /// Calculates the synchronized playout time for a given RTP timestamp.
    ///
    /// The calculation is resilient to RTP timestamp wraparound and media packet reordering
    /// because it is always relative to the latest stable reference offset.
    fn calculate_playout_time(&self, rtp_ts: MediaTime, now: Instant) -> Instant {
        if let Some(offset) = self.rtp_offset {
            // Use wrapping_sub to handle timestamp wraparound correctly
            let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer()) as i32;
            let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64;

            // Apply drift compensation if we have a good estimate
            let compensated_delta =
                if self.sr_history.len() >= 3 && self.estimated_clock_drift_ppm.abs() > 10.0 {
                    let drift_factor = 1.0 + (self.estimated_clock_drift_ppm / 1_000_000.0);
                    seconds_delta * drift_factor
                } else {
                    seconds_delta
                };

            if compensated_delta >= 0.0 {
                offset.wall_clock + Duration::from_secs_f64(compensated_delta)
            } else {
                offset
                    .wall_clock
                    .checked_sub(Duration::from_secs_f64(-compensated_delta))
                    .unwrap_or(now)
            }
        } else {
            warn!(
                stream_id = %self.stream_id,
                "Calculating playout time without a valid time reference; falling back to packet arrival time"
            );
            counter!("rtp_sync_fallback_to_arrival", "stream_id" => self.stream_id.clone())
                .increment(1);
            now
        }
    }

    /// Returns the current estimated clock drift in parts per million (ppm).
    /// Positive values indicate the sender's clock is faster than expected.
    pub fn get_clock_drift_ppm(&self) -> f64 {
        self.estimated_clock_drift_ppm
    }

    /// Returns the number of sender reports in the history.
    pub fn sr_history_len(&self) -> usize {
        self.sr_history.len()
    }

    /// Returns true if the synchronizer has established a valid timeline.
    pub fn is_synchronized(&self) -> bool {
        self.rtp_offset.is_some() && self.last_sr_ntp.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::VIDEO_FREQUENCY;
    use crate::rtp::rtp_packet::RtpPacket;
    use std::time::{SystemTime, UNIX_EPOCH};
    use str0m::media::MediaTime;

    /// Creates a SenderInfo for testing purposes using std::time::SystemTime.
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

        // 1. First packet arrives with no SR. It should get a temporary playout time.
        let mut packet1 = RtpPacket::default();
        packet1.rtp_ts = MediaTime::new(1000, VIDEO_FREQUENCY);
        packet1 = sync.process(packet1, base_time);
        assert_eq!(
            packet1.playout_time,
            Some(base_time),
            "First packet should use arrival time as temporary playout time"
        );

        // 2. A packet with the first SR arrives 300ms later (above MIN_SR_UPDATE_INTERVAL).
        let sr_time = base_time + Duration::from_millis(300);
        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(2000, VIDEO_FREQUENCY);
        packet2.last_sender_info = Some(create_sr(1800, 10)); // SR corresponds to RTP 1800
        packet2 = sync.process(packet2, sr_time);

        // The playout time should now be based on the SR's reference.
        // RTP delta = 2000 - 1800 = 200
        let expected_delta_secs = 200.0 / VIDEO_FREQUENCY.get() as f64;
        let expected_playout_time = sr_time + Duration::from_secs_f64(expected_delta_secs);
        assert_eq!(packet2.playout_time, Some(expected_playout_time));
        assert!(sync.is_synchronized());
    }

    #[test]
    fn test_ignores_stale_reordered_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Process a valid, new SR. This establishes the timeline.
        let mut packet_new = RtpPacket::default();
        packet_new.last_sender_info = Some(create_sr(90000, 101)); // NTP time 101
        sync.process(packet_new.clone(), base_time);

        let expected_ntp = UNIX_EPOCH + Duration::from_secs(101);
        assert_eq!(
            sync.last_sr_ntp,
            Some(expected_ntp),
            "Synchronizer should have stored the new NTP time"
        );

        // 2. Process an older SR that arrived 300ms late.
        let late_packet_time = base_time + Duration::from_millis(300);
        let mut packet_old = RtpPacket::default();
        packet_old.last_sender_info = Some(create_sr(45000, 100)); // NTP time 100 (older)
        sync.process(packet_old, late_packet_time);

        // 3. Assert that the synchronizer ignored the old SR and its offset is unchanged.
        assert_eq!(
            sync.last_sr_ntp,
            Some(expected_ntp),
            "Synchronizer should not have updated its NTP time with the stale report"
        );

        let offset = sync.rtp_offset.unwrap();
        assert_eq!(offset.rtp_time.numer(), 90000);
        assert_eq!(offset.wall_clock, base_time);
    }

    #[test]
    fn test_drift_correction_with_new_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Establish initial timeline.
        let mut initial_packet = RtpPacket::default();
        initial_packet.rtp_ts = MediaTime::new(1000, VIDEO_FREQUENCY);
        initial_packet.last_sender_info = Some(create_sr(1000, 200));
        sync.process(initial_packet, base_time);

        // 2. 5 seconds later, a new packet with a new SR arrives.
        let new_time = base_time + Duration::from_secs(5);
        let rtp_ts_after_5s = 1000 + (VIDEO_FREQUENCY.get() * 5);
        let sr_rtp_ts = rtp_ts_after_5s - 900;

        let mut drift_packet = RtpPacket::default();
        drift_packet.rtp_ts = MediaTime::new(rtp_ts_after_5s, VIDEO_FREQUENCY);
        drift_packet.last_sender_info = Some(create_sr(sr_rtp_ts as u32, 205)); // NTP time advanced by 5s
        drift_packet = sync.process(drift_packet, new_time);

        // 3. The offset should now be updated to the new reference.
        let offset = sync.rtp_offset.unwrap();
        assert_eq!(offset.rtp_time.numer(), sr_rtp_ts);
        assert_eq!(offset.wall_clock, new_time);

        // 4. Playout time should be calculated from this *new* reference.
        let expected_delta_secs =
            (rtp_ts_after_5s - sr_rtp_ts) as f64 / VIDEO_FREQUENCY.get() as f64;
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
        packet1.rtp_ts = MediaTime::new(initial_rtp, VIDEO_FREQUENCY);
        packet1.last_sender_info = Some(create_sr(initial_rtp as u32, 300));
        sync.process(packet1, base_time);

        // 2. 1 second later, process a packet whose timestamp has wrapped around.
        let new_time = base_time + Duration::from_secs(1);
        let wrapped_rtp = initial_rtp.wrapping_add(90000); // Add 1s of ticks, causing wrap
        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(wrapped_rtp, VIDEO_FREQUENCY);
        packet2 = sync.process(packet2, new_time);

        // 3. Check if the delta was calculated correctly.
        let playout_time = packet2.playout_time.unwrap();
        let duration_since_ref = playout_time.duration_since(base_time);

        // We expect the duration to be exactly 1 second from the reference.
        let expected_duration = Duration::from_secs(1);
        assert_eq!(
            duration_since_ref, expected_duration,
            "Duration should be exactly 1 second after timestamp wraparound"
        );
    }

    #[test]
    fn test_playout_time_of_late_media_packet() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. Establish timeline with an SR. Reference is RTP 180000 at base_time.
        let mut sr_packet = RtpPacket::default();
        sr_packet.rtp_ts = MediaTime::new(180000, VIDEO_FREQUENCY); // 2s into the stream
        sr_packet.last_sender_info = Some(create_sr(180000, 402));
        sync.process(sr_packet, base_time);

        // 2. A packet that *should* have arrived 100ms ago (RTP timestamp is 9000 ticks earlier)
        // arrives now, 500ms after the reference was set.
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
            "Playout time for a late packet should be calculated in the past"
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

        // 2. Another SR arrives 100ms later (below MIN_SR_UPDATE_INTERVAL)
        let sr2_time = base_time + Duration::from_millis(100);
        let mut packet2 = RtpPacket::default();
        packet2.last_sender_info = Some(create_sr(99000, 101));
        sync.process(packet2, sr2_time);

        // 3. Offset should NOT have changed due to jitter protection
        let second_offset = sync.rtp_offset.unwrap();
        assert_eq!(
            first_offset.rtp_time.numer(),
            second_offset.rtp_time.numer()
        );
        assert_eq!(first_offset.wall_clock, second_offset.wall_clock);

        // 4. SR arrives 250ms after first (above threshold), should be accepted
        let sr3_time = base_time + Duration::from_millis(250);
        let mut packet3 = RtpPacket::default();
        packet3.last_sender_info = Some(create_sr(112500, 102));
        sync.process(packet3, sr3_time);

        // 5. This time offset should have updated
        let third_offset = sync.rtp_offset.unwrap();
        assert_eq!(third_offset.rtp_time.numer(), 112500);
        assert_eq!(third_offset.wall_clock, sr3_time);
    }

    #[test]
    fn test_clock_drift_detection() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY, "test_stream".to_string());
        let base_time = Instant::now();

        // 1. First SR
        let mut packet1 = RtpPacket::default();
        packet1.last_sender_info = Some(create_sr(0, 100));
        sync.process(packet1, base_time);

        // 2. Second SR after 1 second - need this to establish history
        let sr2_time = base_time + Duration::from_secs(1);
        let mut packet2 = RtpPacket::default();
        packet2.last_sender_info = Some(create_sr(90000, 101));
        sync.process(packet2, sr2_time);

        // 3. Third SR after another second with slight drift
        // Expected: 90000 more ticks, but sender has 90100 (100 ticks fast = ~11 ppm drift)
        let sr3_time = base_time + Duration::from_secs(2);
        let mut packet3 = RtpPacket::default();
        packet3.last_sender_info = Some(create_sr(180100, 102));
        sync.process(packet3, sr3_time);

        // Should have detected and stored the drift
        assert!(sync.get_clock_drift_ppm().abs() > 0.0);
    }
}
