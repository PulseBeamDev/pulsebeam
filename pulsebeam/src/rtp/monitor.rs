use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::RtpPacket;

const MAX_BAD_QUALITY_LOCKOUT_DURATION: Duration = Duration::from_secs(20);
const BASE_BAD_QUALITY_LOCKOUT_DURATION: Duration = Duration::from_secs(6);
const QUALITY_TRANSITION_LOCKOUT_DURATION: Duration = Duration::from_secs(4);
const LOCKOUT_BACKOFF_FACTOR: u32 = 2;
const INACTIVE_TIMEOUT_MULTIPLIER: u32 = 15;
const DELTA_DELTA_WINDOW_SIZE: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum StreamQuality {
    Bad = 0,
    Good = 1,
    Excellent = 2,
}

#[derive(Debug, Clone)]
pub struct StreamState {
    inactive: Arc<AtomicBool>,
    bitrate_bps: Arc<AtomicU64>,
    quality: Arc<AtomicU8>,
}

impl StreamState {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self {
            inactive: Arc::new(AtomicBool::new(inactive)),
            bitrate_bps: Arc::new(AtomicU64::new(bitrate_bps)),
            quality: Arc::new(AtomicU8::new(StreamQuality::Good as u8)),
        }
    }

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps(&self) -> f64 {
        self.bitrate_bps.load(Ordering::Relaxed) as f64
    }

    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good,
        }
    }
}

#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,

    stream_id: String,

    delta_delta: DeltaDeltaState,
    last_packet_at: Instant,
    bwe: BitrateEstimate,

    current_quality: StreamQuality,
    bad_quality_lockout_until: Option<Instant>,
    bad_quality_lockout_duration: Duration,
    quality_transition_lockout_until: Option<Instant>,
}

impl StreamMonitor {
    pub fn new(stream_id: String, shared_state: StreamState) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            shared_state,
            last_packet_at: now,
            delta_delta: DeltaDeltaState::new(DELTA_DELTA_WINDOW_SIZE),
            bwe: BitrateEstimate::new(now),
            current_quality: StreamQuality::Good,
            bad_quality_lockout_until: None,
            bad_quality_lockout_duration: BASE_BAD_QUALITY_LOCKOUT_DURATION,
            quality_transition_lockout_until: None,
        }
    }

    pub fn process_packet(&mut self, packet: &RtpPacket, size_bytes: usize) {
        self.last_packet_at = packet.arrival_ts;
        self.bwe.record(size_bytes);

        self.delta_delta.update(packet);
    }

    pub fn shared_state(&self) -> &StreamState {
        &self.shared_state
    }

    pub fn poll(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.bwe.poll(now);
        self.shared_state
            .bitrate_bps
            .store(self.bwe.estimate_bps() as u64, Ordering::Relaxed);

        let metrics: RawMetrics = (&self.delta_delta).into();
        let was_inactive = self.shared_state.is_inactive();
        let is_inactive = self
            .determine_inactive_state(now, metrics.frame_duration * INACTIVE_TIMEOUT_MULTIPLIER);

        self.shared_state
            .inactive
            .store(is_inactive, Ordering::Relaxed);

        if is_inactive {
            if is_any_sibling_active {
                // This stream is inactive, but its siblings are active. This is our
                // heuristic for a sender-side bandwidth limitation.
                // We mark the quality as Bad but don't reset all metrics,
                // as it might come back online shortly.
                if self.current_quality != StreamQuality::Bad {
                    tracing::warn!(
                        stream_id = %self.stream_id,
                        "Stream inactive while siblings are active. Locking quality to Bad for {:?}.",
                        self.bad_quality_lockout_duration,
                    );
                    self.current_quality = StreamQuality::Bad;
                    self.shared_state
                        .quality
                        .store(StreamQuality::Bad as u8, Ordering::Relaxed);
                    self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);

                    // SET THE LOCKOUT TIMER. This prevents sender from flopping
                    self.bad_quality_lockout_until = Some(now + self.bad_quality_lockout_duration);
                    self.bad_quality_lockout_duration = (LOCKOUT_BACKOFF_FACTOR
                        * self.bad_quality_lockout_duration)
                        .min(MAX_BAD_QUALITY_LOCKOUT_DURATION);
                }
            } else {
                // This stream is inactive, and so are all its siblings.
                // This is a legitimate pause. Reset if it was previously active.
                if !was_inactive {
                    self.reset(now);
                }
            }
            return;
        }

        // Enforce the lockout if the timer is active.
        if let Some(lockout_until) = self.bad_quality_lockout_until {
            if now < lockout_until {
                // We are still in the lockout period. Do nothing and keep the quality as Bad.
                // This prevents the quality from flapping back to Good.
                return;
            } else {
                // The lockout has expired. Allow normal quality assessment to resume.
                tracing::info!(stream_id = %self.stream_id, "Bad quality lockout expired.");
                self.bad_quality_lockout_until = None;
            }
        }

        // Enforce the quality transition lockout.
        if let Some(lockout_until) = self.quality_transition_lockout_until {
            if now < lockout_until {
                // We are in a cooldown period after a quality drop. Do not assess for an upgrade.
                return;
            } else {
                // Cooldown expired, clear it and allow normal assessment to proceed.
                self.quality_transition_lockout_until = None;
            }
        }

        let quality_score = metrics.calculate_jitter_score();
        let new_quality = metrics.quality_hysteresis(quality_score, self.current_quality);

        if new_quality == self.current_quality {
            return;
        }
        // A state change is proposed. Now we apply the lockout logic.
        let is_upgrade = new_quality > self.current_quality;

        if is_upgrade {
            // This is an UPGRADE attempt. Check if we are in a quality transition lockout.
            if let Some(lockout_until) = self.quality_transition_lockout_until {
                if now < lockout_until {
                    // YES. We are in a lockout. Block the upgrade and do nothing.
                    return;
                } else {
                    // The lockout has expired. Clear it so the upgrade can proceed.
                    self.quality_transition_lockout_until = None;
                }
            }
        }

        // If we are here, the transition is allowed. It's either:
        // a) A downgrade (which is always permitted).
        // b) An upgrade, but we are not in a lockout period.

        // If this transition is a downgrade to Bad, set the lockout for the *next* upgrade attempt.
        if new_quality < self.current_quality && new_quality == StreamQuality::Bad {
            tracing::warn!(
                stream_id = %self.stream_id,
                "Quality downgraded to Bad. Locking future upgrades for {:?}.",
                QUALITY_TRANSITION_LOCKOUT_DURATION
            );
            self.quality_transition_lockout_until = Some(now + QUALITY_TRANSITION_LOCKOUT_DURATION);
        }

        // Finally, commit the state change.
        tracing::info!(
            stream_id = %self.stream_id,
            "Stream quality transition: {:?} -> {:?} (score: {:.1}, loss: {:.2}%, m_hat: {:.3}, bitrate: {})",
            self.current_quality,
            new_quality,
            quality_score,
            metrics.packet_loss() * 100.0,
            metrics.m_hat,
            Bitrate::from(self.bwe.bwe_bps_ewma),
        );
        self.current_quality = new_quality;
        self.shared_state
            .quality
            .store(new_quality as u8, Ordering::Relaxed);
    }

    fn reset(&mut self, now: Instant) {
        tracing::info!(
            stream_id = %self.stream_id,
            "Stream inactive, resetting all metrics. Quality was: {:?}", self.current_quality);
        self.delta_delta = DeltaDeltaState::new(DELTA_DELTA_WINDOW_SIZE);
        self.bad_quality_lockout_until = None;
        self.bad_quality_lockout_duration = BASE_BAD_QUALITY_LOCKOUT_DURATION;
        self.quality_transition_lockout_until = None;
        self.bwe = BitrateEstimate::new(now);
        self.current_quality = StreamQuality::Good;
        self.shared_state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);

        self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
    }

    fn determine_inactive_state(&self, now: Instant, timeout: Duration) -> bool {
        now.saturating_duration_since(self.last_packet_at) > timeout
    }
}

#[derive(Debug)]
pub struct BitrateEstimate {
    bwe_last_update: Instant,
    bwe_interval_bytes: usize,
    bwe_bps_ewma: f64,
    bwe_bps_peak: f64,
    peak_decay_time: Instant,
}

impl BitrateEstimate {
    pub fn new(now: Instant) -> Self {
        Self {
            bwe_last_update: now,
            bwe_interval_bytes: 0,
            bwe_bps_ewma: 0.0,
            bwe_bps_peak: 0.0,
            peak_decay_time: now,
        }
    }

    pub fn record(&mut self, packet_len: usize) {
        self.bwe_interval_bytes = self.bwe_interval_bytes.saturating_add(packet_len);
    }

    pub fn poll(&mut self, now: Instant) {
        const ALPHA_UP: f64 = 0.9;
        const ALPHA_DOWN: f64 = 0.1;
        const PEAK_DECAY_INTERVAL: Duration = Duration::from_secs(5);
        const PEAK_DECAY_FACTOR: f64 = 0.95;

        let elapsed = now.saturating_duration_since(self.bwe_last_update);
        let elapsed_secs = elapsed.as_secs_f64();

        if elapsed_secs > 0.0 && self.bwe_interval_bytes > 0 {
            let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;

            // Update EWMA
            if self.bwe_bps_ewma == 0.0 {
                self.bwe_bps_ewma = bps;
            } else {
                let alpha = if bps > self.bwe_bps_ewma {
                    ALPHA_UP
                } else {
                    ALPHA_DOWN
                };
                self.bwe_bps_ewma = (1.0 - alpha) * self.bwe_bps_ewma + alpha * bps;
            }

            // Update peak
            if bps > self.bwe_bps_peak {
                self.bwe_bps_peak = bps;
                self.peak_decay_time = now;
            }

            self.bwe_interval_bytes = 0;
            self.bwe_last_update = now;
        }

        // Decay peak slowly over time
        let peak_age = now.saturating_duration_since(self.peak_decay_time);
        if peak_age > PEAK_DECAY_INTERVAL {
            self.bwe_bps_peak *= PEAK_DECAY_FACTOR;
            self.peak_decay_time = now;
        }
    }

    pub fn estimate_bps(&self) -> f64 {
        // Use max of EWMA and decayed peak for pacing
        self.bwe_bps_ewma.max(self.bwe_bps_peak)
    }
}

struct RawMetrics {
    pub m_hat: f64, // The Kalman-filtered queue delay trend
    pub frame_duration: Duration,
    pub packets_actual: u64,
    pub packets_expected: u64,
}

impl From<&DeltaDeltaState> for RawMetrics {
    fn from(value: &DeltaDeltaState) -> Self {
        Self {
            m_hat: value.m_hat,
            frame_duration: Duration::from_millis(value.frame_duration_ms_ewma as u64),
            packets_actual: value.packets_actual,
            packets_expected: value.packets_expected,
        }
    }
}

impl RawMetrics {
    fn packet_loss(&self) -> f64 {
        if self.packets_expected == 0 {
            return 0.0;
        }
        self.packets_expected.saturating_sub(self.packets_actual) as f64
            / self.packets_expected as f64
    }

    pub fn calculate_jitter_score(&self) -> f64 {
        // We use its absolute value to penalize both overuse (positive)
        // and underuse (negative, which can also indicate instability).
        // The midpoint will need to be re-tuned. The paper RECOMMENDS
        // a threshold of 12.5ms to detect overuse.
        //
        // midpoint=10.0ms to make it a bit more sensitive to measurement.
        sigmoid(self.m_hat.abs(), 100.0, -0.2, 10.0)
    }

    pub fn calculate_loss_score(&self) -> f64 {
        let loss_ratio = self.packet_loss();

        // This is a highly punitive linear penalty.
        // 1% loss = 25 point deduction.
        // 2% loss = 50 point deduction.
        // 4% loss or more = score of 0.
        let loss_penalty_per_percent = 25.0;
        let score = 100.0 - (loss_ratio * 100.0 * loss_penalty_per_percent);

        score.max(0.0)
    }

    pub fn quality_hysteresis(&self, score: f64, current: StreamQuality) -> StreamQuality {
        match current {
            StreamQuality::Excellent => {
                // Must drop below 75 to go down to Good
                if score < 75.0 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Excellent
                }
            }
            StreamQuality::Good => {
                if score >= 85.0 {
                    StreamQuality::Excellent
                } else if score < 55.0 {
                    StreamQuality::Bad
                } else {
                    StreamQuality::Good
                }
            }
            StreamQuality::Bad => {
                // Must rise above 65 to go up to Good
                if score >= 65.0 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct PacketStatus {
    arrival: Instant,
    rtp_ts: MediaTime,
}

#[inline]
fn sigmoid(value: f64, range_max: f64, k: f64, midpoint: f64) -> f64 {
    range_max / (1.0 + (-k * (value - midpoint)).exp())
}

#[derive(Debug)]
struct DeltaDeltaState {
    head: SeqNo,          // Next expected seq
    tail: SeqNo,          // Oldest seq in buffer
    frequency: Frequency, // Clock rate (e.g., 90000)
    last_rtp_ts: MediaTime,
    last_arrival: Instant,

    m_hat: f64,     // The estimate of the queue delay trend, m_hat(i-1)
    e: f64,         // The variance of the estimate, e(i-1)
    var_v_hat: f64, // The variance of the measurement noise, var_v_hat(i-1)

    packets_actual: u64,
    packets_expected: u64,
    frame_duration_ms_ewma: f64,

    buffer: Vec<Option<PacketStatus>>,
    initialized: bool,
}

impl DeltaDeltaState {
    pub fn new(cap: usize) -> Self {
        Self {
            head: 0.into(),
            tail: 0.into(),
            frequency: Frequency::NINETY_KHZ,
            last_rtp_ts: MediaTime::from_90khz(0),
            last_arrival: Instant::now(),
            m_hat: 0.0,
            e: 0.1,         // Initial value from paper, Table 1: e(0) = 0.1
            var_v_hat: 1.0, // A reasonable starting default (var_v is clamped at 1)
            packets_actual: 0,
            packets_expected: 0,
            frame_duration_ms_ewma: 1000.0,
            buffer: vec![None; cap],
            initialized: false,
        }
    }

    pub fn update(&mut self, packet: &RtpPacket) {
        if !self.initialized {
            self.init(packet);
        }
        let seq = packet.seq_no;

        // Check if packet is older than tail using wrapping comparison
        let tail_val = *self.tail;
        let seq_val = *seq;
        let seq_offset = seq_val.wrapping_sub(tail_val);

        // If seq is more than half the u64 space behind tail, it's considered old
        if seq_offset > (u64::MAX / 2) {
            tracing::warn!(
                "{} is older than the current tail, {}, ignore it",
                seq,
                self.tail
            );
            return;
        }

        let rtp_ts = packet.rtp_ts;
        let arrival = packet.arrival_ts;
        let buffer_capacity = self.buffer.len() as u64;

        // Update head if this packet is beyond it
        let head_val = *self.head;
        let seq_ahead_of_head = seq_val.wrapping_sub(head_val);

        if seq_ahead_of_head < (u64::MAX / 2) || seq == self.head {
            self.head = seq.wrapping_add(1).into();
        }

        // Check if there's space in the buffer for this packet
        // Using wrapping arithmetic: offset < capacity means it's in range
        let offset_from_tail = seq_val.wrapping_sub(tail_val);
        if offset_from_tail >= buffer_capacity {
            // No space - need to slide the window
            // process_until is now optimized to only check each buffer slot once
            let new_tail = self.head.wrapping_sub(buffer_capacity);
            self.process_until(new_tail.into());
        }

        *self.packet_mut(seq) = Some(PacketStatus { arrival, rtp_ts });
        self.process_in_order();
    }

    fn init(&mut self, packet: &RtpPacket) {
        let seq = packet.seq_no;
        let rtp_ts = packet.rtp_ts;
        let arrival = packet.arrival_ts;

        self.head = seq.wrapping_add(1).into();
        self.tail = seq;
        self.frequency = rtp_ts.frequency();
        self.last_rtp_ts = rtp_ts;
        self.last_arrival = arrival;
        self.initialized = true;
    }

    /// Implements https://www.ietf.org/archive/id/draft-ietf-rmcat-gcc-02.txt.
    fn advance(&mut self, pkt: &PacketStatus) {
        let actual_ms = pkt.arrival.duration_since(self.last_arrival).as_secs_f64() * 1000.0;
        let expected_ms = (pkt.rtp_ts.numer().wrapping_sub(self.last_rtp_ts.numer()) as f64)
            * 1000.0
            / self.frequency.get() as f64;

        // This is `d(i)` from the GCC paper, the inter-group delay variation.
        let skew = actual_ms - expected_ms;

        // Kalman gain K is calculated based on the previous estimate's variance `e`
        // and the previous measurement variance `var_v_hat`.
        // Equation: k(i) = (e(i-1) + q(i)) / (var_v_hat(i) + e(i-1) + q(i))
        // Note: The paper simplifies and assumes var_v_hat(i) is used, which is fine.
        // q is the process noise, a constant.
        const Q: f64 = 1e-3; // State noise covariance from paper, q = 10^-3
        //
        let k = (self.e + Q) / (self.var_v_hat + self.e + Q);

        // Update the estimate of the queue delay trend, m_hat.
        // This is the core output of the filter.
        // Equation: m_hat(i) = m_hat(i-1) + z(i) * k(i)
        // where z(i) = d(i) - m_hat(i-1)
        let z = skew - self.m_hat;
        self.m_hat += z * k;

        // Update the variance of the estimate.
        // Equation: e(i) = (1 - k(i)) * (e(i-1) + q(i))
        self.e = (1.0 - k) * (self.e + Q);

        // --- Update the Measurement Noise Variance (var_v_hat) ---
        // This part allows the filter to adapt to changing network conditions.
        // It's a modified EWMA.
        // f_max is not easily available, so we use a simpler fixed alpha.
        // A chi of 0.01 is a reasonable choice for smoothing.
        const CHI: f64 = 0.01;
        let alpha = (1.0 - CHI).powf(30.0 / 1000.0 * 50.0); // Assuming 50fps for f_max

        // Outlier filter: Clamp the input to the variance calculation.
        let z_clamped = z.abs().min(3.0 * self.var_v_hat.sqrt());
        self.var_v_hat = (alpha * self.var_v_hat + (1.0 - alpha) * z_clamped.powi(2)).max(1.0);

        self.last_arrival = pkt.arrival;
        self.last_rtp_ts = pkt.rtp_ts;
        self.packets_actual += 1;
        self.packets_expected += 1;

        if expected_ms != 0.0 {
            const ALPHA_UP: f64 = 0.1;
            const ALPHA_DOWN: f64 = 0.01;

            let new_duration = if expected_ms > self.frame_duration_ms_ewma {
                (1.0 - ALPHA_UP) * self.frame_duration_ms_ewma + ALPHA_UP * expected_ms
            } else {
                (1.0 - ALPHA_DOWN) * self.frame_duration_ms_ewma + ALPHA_DOWN * expected_ms
            };

            if (new_duration - self.frame_duration_ms_ewma).abs() > 0.1 {
                let from = self.frame_duration_ms_ewma * INACTIVE_TIMEOUT_MULTIPLIER as f64;
                let to = new_duration * INACTIVE_TIMEOUT_MULTIPLIER as f64;
                tracing::debug!("new inactivity timeout: {:.3}ms -> {:.3}ms", from, to);
            }
            self.frame_duration_ms_ewma = new_duration;
        }
    }

    fn process_in_order(&mut self) {
        // Process packets in sequence order starting from tail
        loop {
            let tail_val = *self.tail;
            let head_val = *self.head;

            // Check if we've caught up to head using wrapping arithmetic
            let distance = head_val.wrapping_sub(tail_val);
            if distance == 0 || distance > (u64::MAX / 2) {
                // Either we're at head, or head is somehow behind us (shouldn't happen)
                break;
            }

            // Try to get the packet at tail position
            let Some(pkt) = self.packet_mut(self.tail).take() else {
                // No packet at tail, can't continue processing in order
                break;
            };

            self.advance(&pkt);
            self.tail = self.tail.wrapping_add(1).into();
        }
    }

    fn process_until(&mut self, end: SeqNo) {
        let tail_val = *self.tail;
        let end_val = *end;

        // Calculate how many sequence numbers to process
        let count = end_val.wrapping_sub(tail_val);

        // If count is 0 or seems like we're going backwards, something is wrong
        if count == 0 || count > (u64::MAX / 2) {
            if count != 0 {
                tracing::warn!(
                    "process_until: end {} appears to be before tail {}",
                    end,
                    self.tail
                );
            }
            return;
        }

        let buffer_capacity = self.buffer.len() as u64;

        // If the jump is larger than our buffer, we only need to check
        // each buffer slot once. Limit iteration to buffer_capacity.
        // Any packets beyond that are guaranteed to be lost anyway.
        let iterations = count.min(buffer_capacity);

        if count > buffer_capacity {
            // We're skipping more packets than our buffer can hold
            // Count the definitely-lost packets (everything before our buffer range)
            let packets_definitely_lost = count - buffer_capacity;
            self.packets_expected += packets_definitely_lost;

            tracing::debug!(
                "process_until: large jump of {} packets, {} definitely lost, checking last {} slots",
                count,
                packets_definitely_lost,
                iterations
            );
        }

        // Only process the last buffer_capacity worth of packets
        // Start from (end - buffer_capacity) to end
        let start_checking = end_val.wrapping_sub(iterations);

        for i in 0..iterations {
            let current = start_checking.wrapping_add(i);
            let Some(pkt) = self.packet_mut(current.into()).take() else {
                self.packets_expected += 1;
                continue;
            };

            self.advance(&pkt);
        }
        self.tail = end;
    }

    fn as_index(&self, seq: SeqNo) -> usize {
        (*seq % self.buffer.len() as u64) as usize
    }

    fn packet(&mut self, seq: SeqNo) -> &Option<PacketStatus> {
        let index = self.as_index(seq);
        &self.buffer[index]
    }

    fn packet_mut(&mut self, seq: SeqNo) -> &mut Option<PacketStatus> {
        let index = self.as_index(seq);
        &mut self.buffer[index]
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    const TEST_CAP: usize = 10;
    const TEST_FREQ: Frequency = Frequency::NINETY_KHZ;
    const MS_PER_PACKET: u64 = 20; // 20ms per packet
    const RTP_TS_PER_PACKET: u64 = (TEST_FREQ.get() as u64 * MS_PER_PACKET) / 1000; // 1800

    // Helper to create a stream of test packets
    struct PacketFactory {
        start_time: Instant,
        start_seq: u64,
        start_rtp_ts: u64,
    }

    impl PacketFactory {
        fn new() -> Self {
            Self {
                start_time: Instant::now(),
                start_seq: 1,
                start_rtp_ts: 1000,
            }
        }

        fn with_start_seq(start_seq: u64) -> Self {
            Self {
                start_time: Instant::now(),
                start_seq,
                start_rtp_ts: 1000,
            }
        }

        // Creates a packet with a given sequence number and an optional jitter in arrival time
        fn at(&self, seq: u64, arrival_jitter_ms: i64) -> RtpPacket {
            // Use wrapping arithmetic to handle overflow correctly
            let packets_since_start = seq.wrapping_sub(self.start_seq);
            let rtp_ts = self
                .start_rtp_ts
                .wrapping_add(packets_since_start.wrapping_mul(RTP_TS_PER_PACKET));
            let ideal_arrival_ms = packets_since_start.wrapping_mul(MS_PER_PACKET);

            let arrival_time = if arrival_jitter_ms >= 0 {
                self.start_time
                    + Duration::from_millis(ideal_arrival_ms.wrapping_add(arrival_jitter_ms as u64))
            } else {
                self.start_time + Duration::from_millis(ideal_arrival_ms)
                    - Duration::from_millis(arrival_jitter_ms.unsigned_abs())
            };

            RtpPacket {
                seq_no: seq.into(),
                rtp_ts: MediaTime::new(rtp_ts, TEST_FREQ),
                arrival_ts: arrival_time.into(),
                ..Default::default()
            }
        }
    }

    #[test]
    fn test_initialization() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();
        let pkt = factory.at(1, 0);

        monitor.update(&pkt);

        assert_eq!(
            monitor.tail,
            2.into(),
            "Tail should advance past the first packet"
        );
        assert_eq!(monitor.head, 2.into(), "Head should be next expected seq");
        assert_eq!(monitor.last_rtp_ts, pkt.rtp_ts);
        assert_eq!(monitor.last_arrival, pkt.arrival_ts);
    }

    #[test]
    fn test_in_order_processing() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        monitor.update(&pkt1);
        assert_eq!(monitor.tail, 2.into());

        let pkt2 = factory.at(2, 0);
        monitor.update(&pkt2);

        assert_eq!(monitor.tail, 3.into(), "Tail should advance to 3");
        assert_eq!(monitor.head, 3.into(), "Head should advance to 3");
    }

    #[test]
    fn test_out_of_order_within_buffer() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        let pkt3 = factory.at(3, 0);
        let pkt2 = factory.at(2, 0);

        monitor.update(&pkt1);
        assert_eq!(monitor.tail, 2.into());
        assert_eq!(monitor.head, 2.into());

        monitor.update(&pkt3); // Packet 3 arrives
        assert_eq!(monitor.tail, 2.into(), "Tail shouldn't move, waiting for 2");
        assert_eq!(monitor.head, 4.into(), "Head should jump to 4");
        assert!(
            monitor.packet(3.into()).is_some(),
            "Packet 3 should be in buffer"
        );

        monitor.update(&pkt2); // Packet 2 arrives, filling the gap
        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should advance to 4 after processing 2, 3"
        );
        assert_eq!(monitor.head, 4.into(), "Head should remain 4");
    }

    #[test]
    fn test_packet_loss() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        assert_eq!(monitor.tail, 2.into(), "Tail is 2, waiting for packet 2");
        monitor.update(&factory.at(3, 0));
        assert_eq!(
            monitor.tail,
            2.into(),
            "Tail should still be 2, waiting for 2"
        );

        monitor.update(&factory.at(12, 0)); // This packet is outside the window [2, 2+10)

        // The head becomes 13. new_tail = 13 - 10 = 3.
        // process_until(3) is called. It processes from old_tail (2) up to 3 (exclusive).
        // It looks for packet 2, doesn't find it (loss), and sets tail to 3.
        // Then process_in_order() runs and immediately processes packet 3, advancing tail to 4.
        assert_eq!(monitor.head, 13.into());
        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should be at 4 after sliding past lost packet 2 and processing packet 3"
        );
    }

    #[test]
    fn test_buffer_wraparound_and_making_space() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP); // Capacity is 10
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        assert_eq!(monitor.tail, 2.into());
        assert_eq!(monitor.head, 2.into());

        let pkt12 = factory.at(12, 0);
        monitor.update(&pkt12);

        assert_eq!(monitor.head, 13.into());
        assert_eq!(
            monitor.tail,
            3.into(),
            "Tail should be moved forward to make space"
        );
        assert!(
            monitor.packet(1.into()).is_none(),
            "Packet 1 should have been processed and removed"
        );
        assert!(
            monitor.packet(12.into()).is_some(),
            "Packet 12 should now be in the buffer"
        );
    }

    #[test]
    fn test_ignore_old_packet() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(5, 0));
        monitor.update(&factory.at(6, 0));

        assert_eq!(monitor.tail, 7.into());
        let before_last_arrival = monitor.last_arrival;

        // Send a packet older than the current tail
        monitor.update(&factory.at(4, 0));

        // State should not have changed
        assert_eq!(monitor.tail, 7.into(), "Old packet should not change tail");
        assert_eq!(
            monitor.last_arrival, before_last_arrival,
            "State should not be updated by an old packet"
        );
    }

    #[test]
    fn test_duplicate_packet_overwrite() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        // Send packets 1 and 3, creating a gap for packet 2.
        monitor.update(&factory.at(1, 0));
        monitor.update(&factory.at(3, 0));

        assert_eq!(monitor.tail, 2.into());
        assert!(monitor.packet(3.into()).is_some());

        // Send a "duplicate" of 3, but with a different arrival time.
        let pkt3_dup = factory.at(3, 10);
        monitor.update(&pkt3_dup);
        assert_eq!(
            monitor.packet(3.into()).as_ref().unwrap().arrival,
            pkt3_dup.arrival_ts,
            "Duplicate should overwrite existing packet data"
        );

        // Now, fill the gap with packet 2.
        monitor.update(&factory.at(2, 0));

        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should have processed up to packet 3"
        );
        assert_eq!(
            monitor.last_arrival, pkt3_dup.arrival_ts,
            "The monitor should have used the overwritten packet's data"
        );
    }

    #[test]
    fn test_sequence_number_wrap_around() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let start_seq = u64::MAX - 5;
        let factory = PacketFactory::with_start_seq(start_seq);
        let mut current_seq = start_seq;

        // Initialize the monitor
        monitor.update(&factory.at(current_seq, 0));
        assert_eq!(monitor.tail, (current_seq.wrapping_add(1)).into());

        // Process several packets, wrapping around u64::MAX
        for _ in 0..15 {
            current_seq = current_seq.wrapping_add(1);
            monitor.update(&factory.at(current_seq, 0));
        }
        let expected_tail = start_seq.wrapping_add(16);
        let expected_head = start_seq.wrapping_add(16);
        assert_eq!(monitor.tail, expected_tail.into());
        assert_eq!(monitor.head, expected_head.into());
    }
}
