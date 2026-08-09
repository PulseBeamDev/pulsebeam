use crate::clock::NtpTime;
use crate::rtp::RtpPacket;
use ahash::HashMap;
use std::time::Duration;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::{Ssrc, rtcp::SenderInfo},
};
use tokio::time::Instant;

const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const MAX_DRIFT_PPM: f64 = 50_000.0;
const MAX_RTP_GAP_SECS: f64 = 10.0;

#[derive(Debug, Clone, Copy)]
struct ClockReference {
    rtp_time: MediaTime,
    ntp_time: NtpTime,
    arrival_ts: Instant,
}

impl ClockReference {
    fn server_time_at_anchor(&self, ntp_delta: Duration) -> Instant {
        self.arrival_ts + ntp_delta
    }

    /// Of two anchors on the same NTP wall clock, the one implying the lower
    /// propagation delay — i.e. whose server arrival is earliest for its NTP
    /// instant. Only `ntp_time`/`arrival_ts` matter here (never `rtp_time`), so
    /// this is meaningful across sibling encodings with unrelated RTP bases.
    fn lower_delay(self, other: Self) -> Self {
        match other.ntp_time.duration_since(self.ntp_time) {
            // other is later on the NTP clock: does it arrive sooner than self predicts?
            Ok(dt) => {
                if other.arrival_ts < self.arrival_ts + dt {
                    other
                } else {
                    self
                }
            }
            // other is earlier on the NTP clock: compare the other way round.
            Err(e) => {
                if other.arrival_ts + e < self.arrival_ts {
                    other
                } else {
                    self
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Synchronizer {
    clock_rate: Frequency,
    first_sr: Option<ClockReference>,
    latest_sr: Option<ClockReference>,
    last_sr_time: Option<Instant>,
    base_rtp: Option<MediaTime>,
    base_server_time: Option<Instant>,
    pub estimated_clock_drift_ppm: f64,
    /// The server Instant that corresponds to a specific NTP time, representing
    /// the minimum observed propagation delay.
    ntp_anchor: Option<ClockReference>,
}

impl Synchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            first_sr: None,
            latest_sr: None,
            last_sr_time: None,
            base_rtp: None,
            base_server_time: None,
            estimated_clock_drift_ppm: 0.0,
            ntp_anchor: None,
        }
    }

    pub fn process(&mut self, packet: &mut RtpPacket, sr: Option<SenderInfo>) {
        if let Some(sr) = sr {
            self.add_sender_report(sr, packet.arrival_ts);
        }

        if self.base_rtp.is_none() {
            self.reset_baseline(packet.rtp_ts, packet.arrival_ts);
        }

        let base_rtp = self.base_rtp.unwrap();
        let mut base_server_time = self.base_server_time.unwrap();

        let rtp_delta = (packet.rtp_ts.numer() as i64).wrapping_sub(base_rtp.numer() as i64);
        let max_ticks = (MAX_RTP_GAP_SECS * self.clock_rate.get() as f64) as i64;

        // Auto-reset on massive RTP leaps to prevent timeline corruption
        if rtp_delta.abs() > max_ticks {
            self.reset_baseline(packet.rtp_ts, packet.arrival_ts);
            packet.playout_time = packet.arrival_ts;
            return;
        }

        let drift = self.estimated_clock_drift_ppm / 1_000_000.0;
        let drift_correction = 1.0 / (1.0 + drift).max(0.001);

        // 1. If we have SR info, we can calculate the NTP time of this packet and use it for alignment.
        let mut ntp_expected_playout = None;
        if let Some(latest) = self.latest_sr {
            let rtp_delta =
                (packet.rtp_ts.numer() as i64).wrapping_sub(latest.rtp_time.numer() as i64);
            let ntp_delta_secs = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;
            let ntp_pkt = if ntp_delta_secs >= 0.0 {
                latest.ntp_time + Duration::from_secs_f64(ntp_delta_secs)
            } else {
                latest.ntp_time - Duration::from_secs_f64(-ntp_delta_secs)
            };

            if let Some(anchor) = self.ntp_anchor {
                let ntp_delta = ntp_pkt
                    .duration_since(anchor.ntp_time)
                    .unwrap_or(Duration::ZERO);
                ntp_expected_playout = Some(anchor.server_time_at_anchor(ntp_delta));
            }
        }

        // 2. Fallback/Standard path: use the local RTP-based baseline
        let seconds_delta = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;
        let mut expected_playout = if seconds_delta >= 0.0 {
            base_server_time + Duration::from_secs_f64(seconds_delta)
        } else {
            base_server_time
                .checked_sub(Duration::from_secs_f64(-seconds_delta))
                .unwrap_or(packet.arrival_ts)
        };

        // 3. Re-align: If the NTP-based estimate is significantly different, or if we just want
        // to sync multiple tracks, we should prioritize the NTP timeline.
        if let Some(ntp_playout) = ntp_expected_playout {
            // We use the NTP playout if it's available, as it's synchronized across all tracks.
            expected_playout = ntp_playout;
        }

        // Minimum envelope filter: absorbs network jitter
        if packet.arrival_ts < expected_playout {
            let error = expected_playout.duration_since(packet.arrival_ts);
            if let Some(new_base) = base_server_time.checked_sub(error) {
                base_server_time = new_base;
                self.base_server_time = Some(base_server_time);
                expected_playout = packet.arrival_ts;

                // Also pull forward the NTP anchor if it exists. This is critical for
                // recovering from an initial NTP anchor that was established with a large
                // network delay (e.g. startup buffering).
                if let Some(anchor) = &mut self.ntp_anchor
                    && let Some(new_arrival) = anchor.arrival_ts.checked_sub(error)
                {
                    anchor.arrival_ts = new_arrival;
                }
            }
        }

        packet.playout_time = expected_playout;
    }

    fn reset_baseline(&mut self, rtp_ts: MediaTime, arrival_ts: Instant) {
        self.base_rtp = Some(rtp_ts);
        self.base_server_time = Some(arrival_ts);
        self.first_sr = None;
        self.latest_sr = None;
        self.ntp_anchor = None;
        self.estimated_clock_drift_ppm = 0.0;
    }

    fn add_sender_report(&mut self, sr: SenderInfo, now: Instant) {
        if let Some(last_time) = self.last_sr_time
            && now.duration_since(last_time) < MIN_SR_UPDATE_INTERVAL
        {
            return;
        }

        let current = ClockReference {
            rtp_time: sr.rtp_time,
            ntp_time: NtpTime::from_system_time(sr.ntp_time),
            arrival_ts: now,
        };

        if let Some(last) = self.latest_sr {
            // Both comparisons are modular: NTP wraps at the era boundary and
            // the RTP timestamp wraps at 2^32, so a report that is genuinely
            // newer can be numerically smaller than its predecessor.
            if current.ntp_time.units_since(last.ntp_time) <= 0
                || current.rtp_time.numer().wrapping_sub(last.rtp_time.numer()) as i64 <= 0
            {
                return;
            }
        } else {
            self.first_sr = Some(current);
        }

        self.latest_sr = Some(current);
        self.last_sr_time = Some(now);

        if let (Some(first), Some(latest)) = (self.first_sr, self.latest_sr) {
            self.estimated_clock_drift_ppm = Self::compute_clock_drift(&first, &latest);
        }

        // Update the NTP anchor with a minimum envelope filter
        if let Some(anchor) = self.ntp_anchor {
            let ntp_delta = current
                .ntp_time
                .duration_since(anchor.ntp_time)
                .unwrap_or(Duration::ZERO);
            let expected_server = anchor.arrival_ts + ntp_delta;
            if now < expected_server {
                // This SR arrived earlier than the previous anchor relative to NTP.
                // It represents a lower propagation delay.
                self.ntp_anchor = Some(current);
            }
        } else {
            self.ntp_anchor = Some(current);
        }
    }

    fn compute_clock_drift(first: &ClockReference, current: &ClockReference) -> f64 {
        let sender_rtp_delta = current
            .rtp_time
            .numer()
            .wrapping_sub(first.rtp_time.numer()) as i64;
        let sender_ntp_delta_secs = current
            .ntp_time
            .duration_since(first.ntp_time)
            .unwrap_or_default()
            .as_secs_f64();

        if sender_ntp_delta_secs <= 0.001 {
            return 0.0;
        }

        let expected_rtp_delta = sender_ntp_delta_secs * first.rtp_time.frequency().get() as f64;

        if expected_rtp_delta <= 0.0 {
            return 0.0;
        }

        let drift_ratio = (sender_rtp_delta as f64 - expected_rtp_delta) / expected_rtp_delta;

        // Clamp drift to physical boundaries (+/- 5%) to avoid infinity during pauses
        (drift_ratio * 1_000_000.0).clamp(-MAX_DRIFT_PPM, MAX_DRIFT_PPM)
    }

    pub fn is_synchronized(&self) -> bool {
        self.latest_sr
            .is_some_and(|l| self.first_sr.is_some_and(|f| f.ntp_time != l.ntp_time))
    }

    /// This stream's current NTP↔server anchor (its estimate of the connection's
    /// minimum propagation delay), for the track composer to reconcile across
    /// sibling encodings.
    fn ntp_anchor(&self) -> Option<ClockReference> {
        self.ntp_anchor
    }

    /// Adopt an anchor chosen by the track composer, so every encoding of the
    /// track maps NTP onto one shared server-time clock rather than each drifting
    /// to its own.
    fn adopt_ntp_anchor(&mut self, anchor: ClockReference) {
        self.ntp_anchor = Some(anchor);
    }
}

/// One synchronized clock for a whole track: it composes a per-SSRC
/// [`Synchronizer`] for each simulcast encoding and orchestrates them onto a
/// single timeline.
///
/// Every encoding is the same source captured by the same encoder over the same
/// connection, so they share a wall clock and a propagation delay. Each keeps its
/// own RTP↔time mapping (the RTP bases are independent per SSRC), but the
/// connection's minimum-delay NTP anchor is reconciled across all of them: the
/// composer keeps the lowest-delay anchor any encoding has observed and feeds it
/// back to each, so their `playout_time`s land on one clock instead of drifting
/// apart and putting a seam step into a layer switch.
#[derive(Debug)]
pub struct TrackSynchronizer {
    clock_rate: Frequency,
    streams: HashMap<Ssrc, Synchronizer>,
    /// The connection-wide anchor: the lowest-delay NTP↔server reference any
    /// encoding has reported. Shared into every per-SSRC synchronizer.
    shared_anchor: Option<ClockReference>,
}

impl TrackSynchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            streams: HashMap::default(),
            shared_anchor: None,
        }
    }

    /// Stamp `packet.playout_time` on the track's shared clock, routing it to its
    /// encoding's synchronizer and reconciling the connection anchor.
    pub fn process(&mut self, packet: &mut RtpPacket, sr: Option<SenderInfo>) {
        let clock_rate = self.clock_rate;
        let shared = self.shared_anchor;

        let sync = self
            .streams
            .entry(packet.ssrc)
            .or_insert_with(|| Synchronizer::new(clock_rate));

        // Pin this encoding to the connection's shared anchor before it maps the
        // packet, so its playout lands on the track clock, not its own.
        if let Some(anchor) = shared {
            sync.adopt_ntp_anchor(anchor);
        }
        sync.process(packet, sr);

        // Fold whatever anchor this encoding now holds back into the shared one,
        // keeping the lowest-delay estimate across the whole track.
        if let Some(anchor) = sync.ntp_anchor() {
            self.shared_anchor = Some(match self.shared_anchor {
                Some(current) => current.lower_delay(anchor),
                None => anchor,
            });
        }
    }

    pub fn is_synchronized(&self) -> bool {
        self.streams.values().any(Synchronizer::is_synchronized)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(clippy::disallowed_types)]
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

        let p1_arrival = base_time + Duration::from_millis(100);
        let mut p1 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000),
            arrival_ts: p1_arrival,
            ..Default::default()
        };
        sync.process(&mut p1, None);
        assert_eq!(p1.playout_time, p1_arrival);

        let p2_arrival = base_time + Duration::from_secs(1);
        let mut p2 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(180_000),
            arrival_ts: p2_arrival,
            ..Default::default()
        };
        sync.process(&mut p2, None);
        assert_eq!(p2.playout_time, p2_arrival);

        let p3_arrival = base_time + Duration::from_secs(2) + Duration::from_millis(50);
        let mut p3 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(270_000),
            arrival_ts: p3_arrival,
            ..Default::default()
        };
        sync.process(&mut p3, None);
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

            let mut pkt_perf = RtpPacket {
                arrival_ts: last_time,
                rtp_ts: MediaTime::from_90khz(last_rtp_perf),
                ..Default::default()
            };
            sync_perfect.process(
                &mut pkt_perf,
                Some(create_sr(MediaTime::from_90khz(last_rtp_perf), last_ntp)),
            );

            let mut pkt_drift = RtpPacket {
                arrival_ts: last_time,
                rtp_ts: MediaTime::from_90khz(last_rtp_drift),
                ..Default::default()
            };
            sync_drifting.process(
                &mut pkt_drift,
                Some(create_sr(MediaTime::from_90khz(last_rtp_drift), last_ntp)),
            );
        }

        assert_eq!(sync_perfect.estimated_clock_drift_ppm.round() as i64, 0);
        assert_eq!(sync_drifting.estimated_clock_drift_ppm.round() as i64, 1000);

        let event_time = base_time + Duration::from_secs(10);

        let mut p_perf = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000 * 10),
            arrival_ts: event_time,
            ..Default::default()
        };
        sync_perfect.process(&mut p_perf, None);

        let mut p_drift = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_090 * 10),
            arrival_ts: event_time,
            ..Default::default()
        };
        sync_drifting.process(&mut p_drift, None);

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

            let mut pkt_perf = RtpPacket {
                arrival_ts: last_time,
                rtp_ts: MediaTime::from_90khz(90_000 * i),
                ..Default::default()
            };
            sync_perfect.process(
                &mut pkt_perf,
                Some(create_sr(MediaTime::from_90khz(90_000 * i), last_ntp_abs)),
            );

            let mut pkt_drift = RtpPacket {
                arrival_ts: last_time,
                rtp_ts: MediaTime::from_90khz(90_090 * i),
                ..Default::default()
            };
            sync_drifting.process(
                &mut pkt_drift,
                Some(create_sr(MediaTime::from_90khz(90_090 * i), last_ntp_rel)),
            );
        }

        let event_time = base_time + Duration::from_secs(10);

        let mut p_perf = RtpPacket {
            rtp_ts: MediaTime::from_90khz(900_000),
            arrival_ts: event_time,
            ..Default::default()
        };
        sync_perfect.process(&mut p_perf, None);

        let mut p_drift = RtpPacket {
            rtp_ts: MediaTime::from_90khz(900_900),
            arrival_ts: event_time,
            ..Default::default()
        };
        sync_drifting.process(&mut p_drift, None);

        let diff = if p_perf.playout_time > p_drift.playout_time {
            p_perf.playout_time - p_drift.playout_time
        } else {
            p_drift.playout_time - p_perf.playout_time
        };
        assert!(diff < Duration::from_micros(1));
    }

    #[test]
    fn test_massive_rtp_gap_resets_baseline() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();

        // 1. Normal packet
        let p1_arrival = base_time;
        let mut p1 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000),
            arrival_ts: p1_arrival,
            ..Default::default()
        };
        sync.process(&mut p1, None);

        // 2. Massive gap (e.g. 15 seconds)
        let p2_arrival = base_time + Duration::from_secs(15);
        let mut p2 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000 + (15 * 90_000)),
            arrival_ts: p2_arrival,
            ..Default::default()
        };
        sync.process(&mut p2, None);

        // Expect the baseline to reset, mapping playout exactly to the new arrival
        assert_eq!(p2.playout_time, p2_arrival);
        assert_eq!(sync.base_rtp.unwrap(), p2.rtp_ts);
        assert_eq!(sync.estimated_clock_drift_ppm, 0.0);
    }

    #[test]
    fn test_ntp_alignment_recovers_from_initial_delay() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let base_time = Instant::now();
        let ntp_base = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1000);

        // 1. First packet arrives with 5s delay. Baseline pins to 5s.
        let mut p1 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(0),
            arrival_ts: base_time + Duration::from_secs(5),
            ..Default::default()
        };
        sync.process(&mut p1, None);
        assert_eq!(p1.playout_time - base_time, Duration::from_secs(5));

        // 2. An SR arrives that reveals the true NTP.
        // Even if the packet carrying the SR is late, the anchor filter
        // will establish a mapping.
        let mut p2 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000), // 1s media later
            arrival_ts: base_time + Duration::from_secs(6), // Still 5s late
            ..Default::default()
        };
        sync.process(&mut p2, Some(create_sr(MediaTime::from_90khz(0), ntp_base)));
        // It's still late because we haven't seen a fast packet yet.
        assert_eq!(p2.playout_time - base_time, Duration::from_secs(6));

        // 3. A fast packet arrives (only 100ms delay).
        // Sent at T=2s (RTP=180,000). Arrives at T=2.1s.
        let mut p3 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(180_000),
            arrival_ts: base_time + Duration::from_secs(2) + Duration::from_millis(100),
            ..Default::default()
        };
        sync.process(&mut p3, None);

        // The playout MUST snap back to the low-latency timeline!
        assert_eq!(p3.playout_time - base_time, Duration::from_millis(2100));
    }

    #[test]
    fn test_independent_streams_align_via_shared_ntp() {
        let base_time = Instant::now();
        let ntp_base = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1000);

        let mut sync1 = Synchronizer::new(VIDEO_FREQUENCY);
        let mut sync2 = Synchronizer::new(VIDEO_FREQUENCY);

        // Stream 1: Low delay (100ms)
        let mut p1 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(0),
            arrival_ts: base_time + Duration::from_millis(100),
            ..Default::default()
        };
        sync1.process(&mut p1, Some(create_sr(MediaTime::from_90khz(0), ntp_base)));

        // Stream 2: High delay (5s)
        let mut p2 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(0),
            arrival_ts: base_time + Duration::from_secs(5),
            ..Default::default()
        };
        sync2.process(&mut p2, Some(create_sr(MediaTime::from_90khz(0), ntp_base)));

        // Now both have SRs. Stream 2 is still "late" because it hasn't seen a fast packet.
        // But if we send a packet that arrives with low delay for Stream 2:
        let mut p3 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000),
            arrival_ts: base_time + Duration::from_secs(1) + Duration::from_millis(100),
            ..Default::default()
        };
        sync2.process(&mut p3, None);

        // And Stream 1 sends its own packet at the same media time:
        let mut p4 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000),
            arrival_ts: base_time + Duration::from_secs(1) + Duration::from_millis(100),
            ..Default::default()
        };
        sync1.process(&mut p4, None);

        // They must be perfectly aligned now!
        assert_eq!(p3.playout_time, p4.playout_time);
    }
    #[test]
    fn test_ntp_anchor_recovery_is_persistent() {
        let base_time = Instant::now();
        let ntp_base = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1000);
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);

        // 1. Initial SR arrives 5s late.
        let mut p1 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(0),
            arrival_ts: base_time + Duration::from_secs(5),
            ..Default::default()
        };
        sync.process(&mut p1, Some(create_sr(MediaTime::from_90khz(0), ntp_base)));
        assert_eq!(p1.playout_time - base_time, Duration::from_secs(5));

        // 2. A "fast" packet arrives (100ms delay) and pulls the anchor forward.
        let mut p2 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(90_000), // 1s media later
            arrival_ts: base_time + Duration::from_secs(1) + Duration::from_millis(100),
            ..Default::default()
        };
        sync.process(&mut p2, None);
        assert_eq!(p2.playout_time - base_time, Duration::from_millis(1100));

        // 3. Subsequent packets should now be correct based on the updated anchor,
        // even without triggering the jitter filter.
        let mut p3 = RtpPacket {
            rtp_ts: MediaTime::from_90khz(180_000), // 2s media later
            arrival_ts: base_time + Duration::from_secs(2) + Duration::from_millis(100),
            ..Default::default()
        };
        sync.process(&mut p3, None);
        assert_eq!(p3.playout_time - base_time, Duration::from_millis(2100));

        // Ensure that for p3, arrival_ts matches expected_playout (no filter triggered)
        // We can't check internal state easily, but if the anchor wasn't updated,
        // p3 would have had an expected_playout of base_time + 7s.
    }

    /// Two simulcast encodings of one track share a wall clock and a connection.
    /// The composed `TrackSynchronizer` must pin both to the connection's
    /// lowest-delay anchor so their `playout_time`s land on one clock — otherwise
    /// a switch between them would step the timestamp by their delay difference.
    #[test]
    fn track_synchronizer_shares_one_clock_across_encodings() {
        const LOW: u32 = 10;
        const HIGH: u32 = 20;
        const LOW_RTP_BASE: u64 = 1_000_000;
        const HIGH_RTP_BASE: u64 = 5_000_000;
        // The high encoding's report and packets consistently arrive later (more
        // path delay on that stream).
        const HIGH_DELAY: Duration = Duration::from_millis(80);

        let base = Instant::now();
        let ntp0 = UNIX_EPOCH + Duration::from_secs(NTP_UNIX_OFFSET_SECS + 1_000);
        let mut track = TrackSynchronizer::new(VIDEO_FREQUENCY);

        let sr = |ssrc: u32, rtp: u64, ntp: SystemTime| SenderInfo {
            ssrc: ssrc.into(),
            rtp_time: MediaTime::from_90khz(rtp),
            ntp_time: ntp,
            sender_packet_count: 0,
            sender_octet_count: 0,
        };
        let frame = |ssrc: u32, rtp: u64, at: Instant| RtpPacket {
            ssrc: ssrc.into(),
            rtp_ts: MediaTime::from_90khz(rtp),
            arrival_ts: at,
            ..Default::default()
        };

        // Warm up both encodings past the first-packet baseline reset so each has an
        // RTP baseline and SR history and the composer has reconciled the anchor.
        for i in 0..4u64 {
            let t = base + Duration::from_secs(i);
            let ntp = ntp0 + Duration::from_secs(i);
            let mut lo = frame(LOW, LOW_RTP_BASE + i * 90_000, t);
            track.process(&mut lo, Some(sr(LOW, LOW_RTP_BASE + i * 90_000, ntp)));
            let mut hi = frame(HIGH, HIGH_RTP_BASE + i * 90_000, t + HIGH_DELAY);
            track.process(&mut hi, Some(sr(HIGH, HIGH_RTP_BASE + i * 90_000, ntp)));
        }

        // A frame from each encoding at the same wall-clock instant (10s in). Both
        // map their own RTP base to that instant; on one shared clock they resolve
        // to the same playout time. Independent clocks would strand the high layer
        // `HIGH_DELAY` behind — the exact seam step a switch must never introduce.
        let event = base + Duration::from_secs(10);
        let mut lo = frame(LOW, LOW_RTP_BASE + 10 * 90_000, event);
        track.process(&mut lo, None);
        let mut hi = frame(HIGH, HIGH_RTP_BASE + 10 * 90_000, event + HIGH_DELAY);
        track.process(&mut hi, None);

        let delta = if hi.playout_time > lo.playout_time {
            hi.playout_time - lo.playout_time
        } else {
            lo.playout_time - hi.playout_time
        };
        assert!(
            delta < Duration::from_millis(10),
            "encodings of one track must share a clock, got {delta:?}"
        );
    }
}
