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
const MAX_PLAYOUT_FUTURE: Duration = Duration::from_secs(10);
const MAX_PLAYOUT_PAST: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
struct ClockReference {
    rtp_time: MediaTime,
    ntp_time: SystemTime, // used to compute relative clock drift
    server_time: Instant, // authoritative server monotonic clock
}

#[derive(Debug)]
pub struct Synchronizer {
    clock_rate: Frequency,
    sr_history: VecDeque<ClockReference>,
    rtp_offset: Option<ClockReference>,
    last_offset_update: Option<Instant>,
    estimated_clock_drift_ppm: f64,
}

impl Synchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            sr_history: VecDeque::with_capacity(SR_HISTORY_CAPACITY),
            rtp_offset: None,
            last_offset_update: None,
            estimated_clock_drift_ppm: 0.0,
        }
    }

    pub fn process(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        if let Some(sr) = packet.last_sender_info {
            self.add_sender_report(sr, now);
        }

        // Initialize reference if missing
        if self.rtp_offset.is_none() {
            self.rtp_offset = Some(ClockReference {
                rtp_time: packet.rtp_ts,
                ntp_time: SystemTime::UNIX_EPOCH,
                server_time: now,
            });
            self.last_offset_update = Some(now);
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
            if let Some(last_update) = self.last_offset_update {
                if now.duration_since(last_update) < MIN_SR_UPDATE_INTERVAL {
                    return;
                }
            }
        }

        let mapping = ClockReference {
            rtp_time: sr.rtp_time,
            ntp_time: sr.ntp_time,
            server_time: now,
        };

        if let Some(last) = self.sr_history.back() {
            self.estimated_clock_drift_ppm = Self::compute_clock_drift(last, &mapping);
            histogram!("rtp_sync_clock_drift_ppm").record(self.estimated_clock_drift_ppm);
        }

        if self.sr_history.len() == SR_HISTORY_CAPACITY {
            self.sr_history.pop_front();
        }
        self.sr_history.push_back(mapping);

        self.rtp_offset = Some(mapping);
        self.last_offset_update = Some(now);
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

    fn calculate_playout_time(&self, rtp_ts: MediaTime, now: Instant) -> Instant {
        if let Some(offset) = self.rtp_offset {
            let rtp_delta = rtp_ts.numer().wrapping_sub(offset.rtp_time.numer()) as i32;
            // Apply drift correction: scale RTP delta by measured clock drift
            let drift_correction = 1.0 - self.estimated_clock_drift_ppm / 1_000_000.0;
            let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;
            if seconds_delta >= 0.0 {
                offset.server_time + Duration::from_secs_f64(seconds_delta)
            } else {
                offset
                    .server_time
                    .checked_sub(Duration::from_secs_f64(-seconds_delta))
                    .unwrap_or(now)
            }
        } else {
            now
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
        self.rtp_offset.is_some()
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
