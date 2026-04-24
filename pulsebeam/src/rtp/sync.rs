use crate::rtp::RtpPacket;
use metrics::histogram;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
use str0m::{
    media::{Frequency, MediaTime},
    rtp::rtcp::SenderInfo,
};
use tokio::time::Instant;

const SR_HISTORY_CAPACITY: usize = 5;
const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
struct ClockReference {
    rtp_time: MediaTime,
    ntp_time: SystemTime,
}

#[derive(Debug)]
pub struct Synchronizer {
    clock_rate: Frequency,
    sr_history: VecDeque<ClockReference>,
    last_sr_time: Option<Instant>,

    /// The RTP timestamp of the very first packet received.
    base_rtp: Option<MediaTime>,
    /// The absolute server timeline anchor. This shifts backwards as we discover
    /// packets with lower network jitter, ensuring a perfectly smooth timeline.
    base_server_time: Option<Instant>,

    pub estimated_clock_drift_ppm: f64,
}

impl Synchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            sr_history: VecDeque::with_capacity(SR_HISTORY_CAPACITY),
            last_sr_time: None,
            base_rtp: None,
            base_server_time: None,
            estimated_clock_drift_ppm: 0.0,
        }
    }

    /// Compute and stamp `playout_time` on the packet in-place.
    pub fn process(&mut self, packet: &mut RtpPacket, sr: Option<SenderInfo>) {
        if let Some(sr) = sr {
            self.add_sender_report(sr, packet.arrival_ts);
        }

        // Initialize our baseline on the very first packet.
        if self.base_rtp.is_none() {
            self.base_rtp = Some(packet.rtp_ts);
            self.base_server_time = Some(packet.arrival_ts);
        }

        let base_rtp = self.base_rtp.unwrap();
        let mut base_server_time = self.base_server_time.unwrap();

        // Use i64 to prevent overflow on streams that run for >6 hours
        let rtp_delta = (packet.rtp_ts.numer() as i64).wrapping_sub(base_rtp.numer() as i64);

        let drift = self.estimated_clock_drift_ppm / 1_000_000.0;
        let drift_correction = 1.0 / (1.0 + drift);
        let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;

        let mut expected_playout = if seconds_delta >= 0.0 {
            base_server_time + Duration::from_secs_f64(seconds_delta)
        } else {
            base_server_time
                .checked_sub(Duration::from_secs_f64(-seconds_delta))
                .unwrap_or(packet.arrival_ts)
        };

        // Minimum Jitter Filter:
        // If a packet arrives *earlier* than our calculated playout time, it means our initial
        // base_server_time included network jitter. We correct the timeline globally by
        // shifting the base backward. This guarantees standard synchronization without jumps.
        if packet.arrival_ts < expected_playout {
            let error = expected_playout.duration_since(packet.arrival_ts);

            if let Some(new_base) = base_server_time.checked_sub(error) {
                base_server_time = new_base;
                self.base_server_time = Some(base_server_time);
                expected_playout = packet.arrival_ts;
            }
        }

        packet.playout_time = expected_playout;
    }

    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        if let Some(last_time) = self.last_sr_time {
            if now.duration_since(last_time) < MIN_SR_UPDATE_INTERVAL {
                return;
            }
        }

        if let Some(last) = self.sr_history.back() {
            // Reject stale or reordered SRs
            if sr.ntp_time <= last.ntp_time {
                return;
            }
            if sr.rtp_time.numer().wrapping_sub(last.rtp_time.numer()) as i64 <= 0 {
                return;
            }
        }

        self.last_sr_time = Some(now);
        self.sr_history.push_back(ClockReference {
            rtp_time: sr.rtp_time,
            ntp_time: sr.ntp_time,
        });

        if self.sr_history.len() > SR_HISTORY_CAPACITY {
            self.sr_history.pop_front();
        }

        if self.sr_history.len() >= 2 {
            let first = self.sr_history.front().unwrap();
            let current = self.sr_history.back().unwrap();
            self.estimated_clock_drift_ppm = Self::compute_clock_drift(first, current);
            histogram!("rtp_sync_clock_drift_ppm").record(self.estimated_clock_drift_ppm);
        }
    }

    fn compute_clock_drift(first: &ClockReference, current: &ClockReference) -> f64 {
        let sender_rtp_delta = current
            .rtp_time
            .numer()
            .wrapping_sub(first.rtp_time.numer()) as i64;

        // We purely use the Sender's NTP delta to find the exact *speed* of the media clock.
        // This is perfectly safe as it bypasses network jitter entirely, while continuing to
        // ignore the absolute offset constraint.
        let sender_ntp_delta_secs = current
            .ntp_time
            .duration_since(first.ntp_time)
            .unwrap_or_default()
            .as_secs_f64();

        if sender_ntp_delta_secs <= 0.001 {
            return 0.0;
        }

        let expected_rtp_delta = sender_ntp_delta_secs * first.rtp_time.frequency().get() as f64;
        let drift_ratio = (sender_rtp_delta as f64 - expected_rtp_delta) / expected_rtp_delta;

        drift_ratio * 1_000_000.0
    }

    pub fn is_synchronized(&self) -> bool {
        self.sr_history.len() >= 2
    }
}

#[cfg(test)]
impl Synchronizer {
    fn process_owned(&mut self, mut packet: RtpPacket) -> RtpPacket {
        self.process(&mut packet, None);
        packet
    }

    fn process_owned_sr(&mut self, sr: SenderInfo, mut packet: RtpPacket) -> RtpPacket {
        self.process(&mut packet, Some(sr));
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RtpPacket, VIDEO_FREQUENCY};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn test_jitter_filter_and_robust_baseline() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();

        // 1. First packet establishes baseline (simulated with 100ms jitter delay)
        let p1_arrival = base_time + Duration::from_millis(100);
        let p1 = sync.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000),
            arrival_ts: p1_arrival,
            ..Default::default()
        });
        assert_eq!(p1.playout_time, p1_arrival);

        // 2. Second packet arrives exactly 1 second of media later, but with NO jitter.
        // This packet arrives 900ms after the first packet instead of 1000ms.
        let p2_arrival = base_time + Duration::from_secs(1);
        let p2 = sync.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(180_000),
            arrival_ts: p2_arrival,
            ..Default::default()
        });

        // The baseline should shift backward seamlessly! The playout matches arrival.
        assert_eq!(p2.playout_time, p2_arrival);

        // 3. Third packet arrives 2 seconds of media later, but with 50ms of jitter again.
        let p3_arrival = base_time + Duration::from_secs(2) + Duration::from_millis(50);
        let p3 = sync.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(270_000),
            arrival_ts: p3_arrival,
            ..Default::default()
        });

        // The expected playout time MUST strip the jitter and map exactly to the updated server timeline.
        let expected_p3_playout = base_time + Duration::from_secs(2);
        assert_eq!(p3.playout_time, expected_p3_playout);
    }

    #[test]
    fn test_playout_time_is_synchronized_across_drifting_streams() {
        let base_time = Instant::now();
        let ntp_base = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1_000_000);

        let mut sync_perfect = Synchronizer::new(VIDEO_FREQUENCY);
        let mut sync_drifting = Synchronizer::new(VIDEO_FREQUENCY);

        let perfect_ticks: u64 = 90_000;
        let drifting_ticks: u64 = 90_090; // Exactly +1000 PPM

        let mut last_time = base_time;
        let mut last_ntp = ntp_base;
        let mut last_rtp_perf: u64 = 0;
        let mut last_rtp_drift: u64 = 0;

        for _ in 0..4 {
            let interval = Duration::from_secs(1);
            last_time += interval;
            last_ntp += interval;
            last_rtp_perf += perfect_ticks;
            last_rtp_drift += drifting_ticks;

            sync_perfect.process_owned_sr(
                create_sr(MediaTime::from_90khz(last_rtp_perf), last_ntp),
                RtpPacket {
                    arrival_ts: last_time,
                    rtp_ts: MediaTime::from_90khz(last_rtp_perf),
                    ..Default::default()
                },
            );
            sync_drifting.process_owned_sr(
                create_sr(MediaTime::from_90khz(last_rtp_drift), last_ntp),
                RtpPacket {
                    arrival_ts: last_time,
                    rtp_ts: MediaTime::from_90khz(last_rtp_drift),
                    ..Default::default()
                },
            );
        }

        assert_eq!(sync_perfect.estimated_clock_drift_ppm.round() as i64, 0);
        assert_eq!(sync_drifting.estimated_clock_drift_ppm.round() as i64, 1000);

        // Verify alignment 10 seconds in.
        let event_time = base_time + Duration::from_secs(10);

        let p_perf = sync_perfect.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000 * 10),
            arrival_ts: event_time,
            ..Default::default()
        });
        let p_drift = sync_drifting.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_090 * 10),
            arrival_ts: event_time,
            ..Default::default()
        });

        // The absolute offset perfectly aligns both playout clocks to the exact same Instant base
        let diff = if p_perf.playout_time > p_drift.playout_time {
            p_perf.playout_time - p_drift.playout_time
        } else {
            p_drift.playout_time - p_perf.playout_time
        };
        assert!(diff < Duration::from_micros(1));
    }

    #[test]
    fn test_playout_sync_across_drifting_streams_with_different_ntp_bases() {
        let base_time = Instant::now();
        let absolute_ntp_base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let relative_ntp_base = UNIX_EPOCH + Duration::from_secs(300);

        let mut sync_perfect = Synchronizer::new(VIDEO_FREQUENCY);
        let mut sync_drifting = Synchronizer::new(VIDEO_FREQUENCY);

        let mut last_time = base_time;
        let mut last_ntp_abs = absolute_ntp_base;
        let mut last_ntp_rel = relative_ntp_base;

        for i in 1..5 {
            last_time += Duration::from_secs(1);
            last_ntp_abs += Duration::from_secs(1);
            last_ntp_rel += Duration::from_secs(1);

            sync_perfect.process_owned_sr(
                create_sr(MediaTime::from_90khz(90_000 * i), last_ntp_abs),
                RtpPacket {
                    arrival_ts: last_time,
                    rtp_ts: MediaTime::from_90khz(90_000 * i),
                    ..Default::default()
                },
            );
            sync_drifting.process_owned_sr(
                create_sr(MediaTime::from_90khz(90_090 * i), last_ntp_rel),
                RtpPacket {
                    arrival_ts: last_time,
                    rtp_ts: MediaTime::from_90khz(90_090 * i),
                    ..Default::default()
                },
            );
        }

        // Test 10s into the future
        let event_time = base_time + Duration::from_secs(10);

        let p_perf = sync_perfect.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(900_000),
            arrival_ts: event_time,
            ..Default::default()
        });
        let p_drift = sync_drifting.process_owned(RtpPacket {
            rtp_ts: MediaTime::from_90khz(900_900),
            arrival_ts: event_time,
            ..Default::default()
        });

        // Even with fully decoupled NTP uptime bases, the single shared server baseline aligns flawlessly.
        let diff = if p_perf.playout_time > p_drift.playout_time {
            p_perf.playout_time - p_drift.playout_time
        } else {
            p_drift.playout_time - p_perf.playout_time
        };
        assert!(diff < Duration::from_micros(1));
    }
}
