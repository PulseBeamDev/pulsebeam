use crate::rtp::RtpPacket;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
use str0m::{
    media::{Frequency, MediaTime},
    rtp::rtcp::SenderInfo,
};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// The maximum number of sender reports to maintain in the history for analysis or future heuristics.
const SR_HISTORY_CAPACITY: usize = 5;

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
}

impl Synchronizer {
    /// Creates a new synchronizer for a stream with a given clock rate.
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            sr_history: VecDeque::with_capacity(SR_HISTORY_CAPACITY),
            rtp_offset: None,
            last_sr_ntp: None,
        }
    }

    /// Processes an RTP packet to calculate and assign its synchronized `playout_time`.
    ///
    /// This is the main entry point for the synchronizer. It inspects the packet for a
    /// Sender Report, validates it, updates the internal time reference if necessary, and
    /// then computes the packet's absolute playout time on the SFU timeline.
    pub fn process(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, now);
        }

        if self.rtp_offset.is_none() {
            // This is the first packet ever seen for this stream. We have no SR yet,
            // so we must establish a temporary, estimated offset. This will be corrected
            // as soon as the first valid SR arrives.
            warn!(
                "First packet arrived without a Sender Report, establishing temporary time reference."
            );
            self.rtp_offset = Some(RtpWallClockMapping {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH, // Placeholder NTP as this is an estimate.
                wall_clock: now,
            });
        }

        let playout_time = self.calculate_playout_time(packet.rtp_ts, now);
        packet.playout_time = Some(playout_time);
        packet
    }

    /// Validates and incorporates a new Sender Report into the time reference.
    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        if let Some(last_ntp) = self.last_sr_ntp {
            if sr.ntp_time <= last_ntp {
                debug!(
                    current_ntp = ?sr.ntp_time,
                    last_seen_ntp = ?last_ntp,
                    "Ignoring stale/reordered sender report."
                );
                return;
            }
        }

        if self.last_sr_ntp.is_none() {
            info!(rtp_ts = %sr.rtp_time, ntp_ts = ?sr.ntp_time, "Received first Sender Report, timeline established.");
        } else {
            debug!(rtp_ts = %sr.rtp_time, ntp_ts = ?sr.ntp_time, "Received new Sender Report, updating time reference.");
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
    }

    /// Calculates the synchronized playout time for a given RTP timestamp.
    ///
    /// The calculation is resilient to RTP timestamp wraparound and media packet reordering
    /// because it is always relative to the latest stable reference offset.
    fn calculate_playout_time(&self, rtp_ts: MediaTime, now: Instant) -> Instant {
        if let Some(offset) = self.rtp_offset {
            let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer());
            let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64;
            offset.wall_clock + Duration::from_secs_f64(seconds_delta)
        } else {
            warn!(
                "Calculating playout time without a valid time reference; falling back to packet arrival time."
            );
            now
        }
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
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
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

        // 2. A packet with the first SR arrives 10ms later.
        let sr_time = base_time + Duration::from_millis(10);
        let mut packet2 = RtpPacket::default();
        packet2.rtp_ts = MediaTime::new(2000, VIDEO_FREQUENCY);
        packet2.last_sender_info = Some(create_sr(1800, 10)); // SR corresponds to RTP 1800
        packet2 = sync.process(packet2, sr_time);

        // The playout time should now be based on the SR's reference.
        // RTP delta = 2000 - 1800 = 200
        let expected_delta_secs = 200.0 / VIDEO_FREQUENCY.get() as f64;
        let expected_playout_time = sr_time + Duration::from_secs_f64(expected_delta_secs);
        assert_eq!(packet2.playout_time, Some(expected_playout_time));
    }

    #[test]
    fn test_ignores_stale_reordered_sr() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
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

        // 2. Process an older SR that arrived 20ms late.
        let late_packet_time = base_time + Duration::from_millis(20);
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
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
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
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
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
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
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
}
