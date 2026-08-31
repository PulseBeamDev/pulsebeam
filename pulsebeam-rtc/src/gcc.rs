use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    time::{Duration, Instant},
};

use crate::{CompoundRtcpView, SendId};

const TWCC_PACKET_TYPE: u8 = 205;
const TWCC_FORMAT: u8 = 15;
const MIN_BITRATE_BPS: u64 = 30_000;
const MAX_BITRATE_BPS: u64 = 50_000_000;
const DEFAULT_MAX_PROBE_BITRATE_BPS: u64 = 5_000_000;
pub const DEFAULT_INITIAL_BITRATE_BPS: u64 = 300_000;
const OUTAGE: Duration = Duration::from_secs(2);
const TWCC_FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);
const TWCC_REORDER_HOLD: Duration = Duration::from_millis(40);
const TWCC_HISTORY_CAPACITY: usize = 8192;
const TWCC_MAX_STATUS_COUNT: usize = 512;
const SEND_TIME_GROUP: Duration = Duration::from_millis(5);
const INITIAL_ACKNOWLEDGED_RATE_WINDOW: Duration = Duration::from_millis(500);
const ACKNOWLEDGED_RATE_WINDOW: Duration = Duration::from_millis(150);
const TRENDLINE_WINDOW: usize = 20;
const LOSS_DECREASE_THRESHOLD: usize = 20;
const DELAY_DECREASE_INTERVAL: Duration = Duration::from_millis(200);
const LOSS_DECREASE_INTERVAL: Duration = Duration::from_secs(1);
const AIMD_RESPONSE_TIME: Duration = Duration::from_millis(200);
const MIN_ADDITIVE_INCREASE_BPS_PER_SECOND: u64 = 4_000;
const PROBE_VALIDATION_WINDOW: Duration = Duration::from_secs(2);
const INITIAL_PROBE_DURATION: Duration = Duration::from_millis(15);
const ALLOCATION_PROBE_SCALE_NUMERATOR: u64 = 3;
const ALLOCATION_PROBE_SCALE_DENOMINATOR: u64 = 2;
const ALR_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const ALR_PROBE_SCALE_NUMERATOR: u64 = 11;
const ALR_PROBE_SCALE_DENOMINATOR: u64 = 10;
const MIN_RECEIVED_PROBE_PACKETS: usize = 4;
const MIN_RECEIVED_PROBE_PERCENT: u64 = 80;
const MIN_UNSATURATED_PROBE_PERCENT: u64 = 90;
const PROBE_CAPACITY_UTILIZATION_PERCENT: u64 = 95;
const MAX_PROBE_RECEIVE_SEND_RATIO: u64 = 2;
const ALR_BANDWIDTH_USAGE_PERCENT: u64 = 65;
const ALR_BUDGET_WINDOW: Duration = Duration::from_millis(500);
const ALR_START_BUDGET_PERCENT: i64 = 80;
const ALR_STOP_BUDGET_PERCENT: i64 = 50;
const PACING_RATE_NUMERATOR: u64 = 5;
const PACING_RATE_DENOMINATOR: u64 = 2;

pub(crate) struct TwccReceiver {
    epoch: Instant,
    base_sequence: Option<u64>,
    highest_sequence: Option<u64>,
    received: VecDeque<Option<Instant>>,
    next_feedback: Option<Instant>,
    feedback_count: u8,
    media_ssrc: Option<u32>,
    symbols: Vec<u8>,
    deltas: Vec<i16>,
    encoded: Vec<u8>,
}

impl TwccReceiver {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            base_sequence: None,
            highest_sequence: None,
            received: VecDeque::with_capacity(TWCC_HISTORY_CAPACITY),
            next_feedback: None,
            feedback_count: 0,
            media_ssrc: None,
            symbols: Vec::with_capacity(TWCC_MAX_STATUS_COUNT),
            deltas: Vec::with_capacity(TWCC_MAX_STATUS_COUNT),
            encoded: Vec::with_capacity(1200),
        }
    }

    pub(crate) fn observe(&mut self, sequence: u16, received_at: Instant, media_ssrc: u32) {
        self.media_ssrc.get_or_insert(media_ssrc);
        let sequence = self.extend_sequence(sequence);
        let Some(base) = self.base_sequence else {
            self.base_sequence = Some(sequence);
            self.highest_sequence = Some(sequence);
            self.received.push_back(Some(received_at));
            self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
            return;
        };
        if sequence < base {
            return;
        }
        let offset = sequence.saturating_sub(base);
        let Ok(offset) = usize::try_from(offset) else {
            self.reset(sequence, received_at);
            return;
        };
        if offset >= TWCC_HISTORY_CAPACITY {
            self.reset(sequence, received_at);
            return;
        }
        while self.received.len() <= offset {
            self.received.push_back(None);
        }
        let Some(slot) = self.received.get_mut(offset) else {
            debug_assert!(
                false,
                "the bounded TWCC receive history indexes its packet range"
            );
            return;
        };
        if slot.is_some() {
            return;
        }
        *slot = Some(received_at);
        self.highest_sequence = Some(self.highest_sequence.unwrap_or(sequence).max(sequence));
        if self.next_feedback.is_none() {
            self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_feedback
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::expect_used,
        reason = "the fixed twenty-byte header is resized and its bounded fields are asserted before encoding"
    )]
    pub(crate) fn build_feedback(&mut self, now: Instant, sender_ssrc: u32) -> Option<&[u8]> {
        if self.next_feedback.is_some_and(|deadline| now < deadline) {
            return None;
        }
        let safe_cutoff = now.checked_sub(TWCC_REORDER_HOLD).unwrap_or(now);
        let safe_end = self
            .received
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, received)| {
                received
                    .is_some_and(|received_at| received_at <= safe_cutoff)
                    .then_some(index)
            })?;
        let count = safe_end.saturating_add(1).min(TWCC_MAX_STATUS_COUNT);
        if count == 0 {
            self.next_feedback = None;
            return None;
        }
        let first_at = self
            .received
            .iter()
            .take(count)
            .find_map(|received| *received)?;
        let reference_ticks = self.micros_since_epoch(first_at).saturating_div(64_000);
        let reference_at = self.epoch.checked_add(Duration::from_micros(
            reference_ticks.saturating_mul(64_000),
        ))?;
        self.symbols.clear();
        self.deltas.clear();
        let mut previous = reference_at;
        for received_at in self.received.iter().take(count) {
            let Some(received_at) = received_at else {
                self.symbols.push(0);
                continue;
            };
            let delta = signed_delta_250us(*received_at, previous)?;
            let symbol = if (0..=255).contains(&delta) { 1 } else { 2 };
            self.symbols.push(symbol);
            self.deltas.push(delta);
            previous = *received_at;
        }
        self.encoded.clear();
        self.encoded.resize(20, 0);
        self.encoded[0] = 0x8f;
        self.encoded[1] = TWCC_PACKET_TYPE;
        self.encoded[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
        self.encoded[8..12].copy_from_slice(&self.media_ssrc?.to_be_bytes());
        let base = self.base_sequence?;
        self.encoded[12..14].copy_from_slice(&(base as u16).to_be_bytes());
        self.encoded[14..16].copy_from_slice(
            &u16::try_from(count)
                .expect("bounded TWCC feedback status count fits a u16")
                .to_be_bytes(),
        );
        self.encoded[16] = u8::try_from((reference_ticks >> 16) & 0xff).ok()?;
        self.encoded[17] = u8::try_from((reference_ticks >> 8) & 0xff).ok()?;
        self.encoded[18] = u8::try_from(reference_ticks & 0xff).ok()?;
        self.encoded[19] = self.feedback_count;
        self.feedback_count = self.feedback_count.wrapping_add(1);
        for symbols in self.symbols.chunks(7) {
            let mut chunk = 0xc000u16;
            for (index, symbol) in symbols.iter().enumerate() {
                let shift = 12u32.saturating_sub(u32::try_from(index).ok()?.saturating_mul(2));
                chunk |= u16::from(*symbol) << shift;
            }
            self.encoded.extend_from_slice(&chunk.to_be_bytes());
        }
        let mut deltas = self.deltas.iter();
        for symbol in &self.symbols {
            match *symbol {
                0 => {}
                1 => self.encoded.push(u8::try_from(*deltas.next()?).ok()?),
                2 => self
                    .encoded
                    .extend_from_slice(&deltas.next()?.to_be_bytes()),
                _ => {
                    debug_assert!(false, "TWCC status symbols are generated locally");
                    return None;
                }
            }
        }
        let trailing_delta = deltas.next();
        debug_assert!(trailing_delta.is_none());
        let padding = (4usize.saturating_sub(self.encoded.len() % 4)) % 4;
        if padding != 0 {
            self.encoded
                .resize(self.encoded.len().saturating_add(padding), 0);
            let last = self.encoded.last_mut()?;
            *last = u8::try_from(padding).ok()?;
            self.encoded[0] |= 0x20;
        }
        debug_assert!(self.encoded.len().is_multiple_of(4));
        let words = self.encoded.len().checked_div(4)?;
        let length = u16::try_from(words.checked_sub(1)?).ok()?;
        self.encoded[2..4].copy_from_slice(&length.to_be_bytes());
        self.discard(count);
        self.next_feedback = if self.received.is_empty() {
            None
        } else {
            now.checked_add(TWCC_FEEDBACK_INTERVAL)
        };
        Some(&self.encoded)
    }

    fn reset(&mut self, sequence: u64, received_at: Instant) {
        self.base_sequence = Some(sequence);
        self.highest_sequence = Some(sequence);
        self.received.clear();
        self.received.push_back(Some(received_at));
        self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
    }

    fn discard(&mut self, count: usize) {
        let count = count.min(self.received.len());
        self.received.drain(..count);
        self.base_sequence = self
            .base_sequence
            .map(|base| base.saturating_add(u64::try_from(count).unwrap_or(u64::MAX)));
        if self.received.is_empty() {
            self.base_sequence = None;
            self.highest_sequence = None;
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the low word of the extended TWCC sequence is intentionally serialized as u16"
    )]
    fn extend_sequence(&self, sequence: u16) -> u64 {
        let Some(highest) = self.highest_sequence else {
            return u64::from(sequence);
        };
        let highest_low = highest as u16;
        let rollover = highest >> 16;
        let rollover = if sequence < highest_low
            && highest_low.wrapping_sub(sequence) > (u16::MAX / 2)
        {
            rollover.saturating_add(1)
        } else if sequence > highest_low && sequence.wrapping_sub(highest_low) > (u16::MAX / 2) {
            rollover.saturating_sub(1)
        } else {
            rollover
        };
        (rollover << 16) | u64::from(sequence)
    }

    fn micros_since_epoch(&self, instant: Instant) -> u64 {
        u64::try_from(instant.saturating_duration_since(self.epoch).as_micros()).unwrap_or(u64::MAX)
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the negative branch converts a nonnegative duration that was range-checked before negation"
)]
fn signed_delta_250us(received_at: Instant, previous: Instant) -> Option<i16> {
    let micros = if received_at >= previous {
        i64::try_from(received_at.duration_since(previous).as_micros()).ok()?
    } else {
        -i64::try_from(previous.duration_since(received_at).as_micros()).ok()?
    };
    let units = micros / 250;
    i16::try_from(units).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CongestionEstimate {
    bitrate_bps: u64,
    application_limited: bool,
}

impl CongestionEstimate {
    pub const fn bitrate_bps(self) -> u64 {
        self.bitrate_bps
    }

    pub const fn application_limited(self) -> bool {
        self.application_limited
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgressCongestion {
    send_id: SendId,
    transport_sequence: u16,
}

impl EgressCongestion {
    pub const fn send_id(self) -> SendId {
        self.send_id
    }

    pub const fn transport_sequence(self) -> u16 {
        self.transport_sequence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeDecision {
    id: u32,
    target_bitrate_bps: u64,
    packet_count: u8,
    min_duration: Duration,
}

impl ProbeDecision {
    pub const fn id(self) -> u32 {
        self.id
    }

    pub const fn target_bitrate_bps(self) -> u64 {
        self.target_bitrate_bps
    }

    pub const fn packet_count(self) -> u8 {
        self.packet_count
    }

    pub const fn min_duration(self) -> Duration {
        self.min_duration
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GccOutcome {
    estimate: CongestionEstimate,
    pacing_bitrate_bps: u64,
    padding_bitrate_bps: u64,
    state: CongestionState,
    acknowledged: usize,
    lost: usize,
    probe: Option<ProbeDecision>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CongestionState {
    Normal,
    Underusing,
    DelayLimited,
    LossLimited,
    FeedbackOutage,
}

impl GccOutcome {
    pub const fn estimate(self) -> CongestionEstimate {
        self.estimate
    }

    pub const fn pacing_bitrate_bps(self) -> u64 {
        self.pacing_bitrate_bps
    }

    pub const fn padding_bitrate_bps(self) -> u64 {
        self.padding_bitrate_bps
    }

    pub const fn state(self) -> CongestionState {
        self.state
    }

    pub const fn acknowledged(self) -> usize {
        self.acknowledged
    }

    pub const fn lost(self) -> usize {
        self.lost
    }

    pub const fn probe(self) -> Option<ProbeDecision> {
        self.probe
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwccStatus {
    sequence: u16,
    received_at: Option<Duration>,
}

impl TwccStatus {
    pub const fn sequence(self) -> u16 {
        self.sequence
    }

    pub const fn received_at(self) -> Option<Duration> {
        self.received_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwccFeedback {
    statuses: Box<[TwccStatus]>,
}

impl TwccFeedback {
    pub fn statuses(&self) -> &[TwccStatus] {
        &self.statuses
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GccError {
    #[error("TWCC feedback is malformed")]
    MalformedTwcc,
    #[error("send identity {0:?} is already tracked")]
    DuplicateSend(SendId),
    #[error("send identity {0:?} is not tracked")]
    UnknownSend(SendId),
    #[error("send identity {0:?} already has a departure timestamp")]
    DuplicateDeparture(SendId),
}

#[derive(Clone, Copy, Debug)]
struct SendRecord {
    transport_sequence: u16,
    bytes: usize,
    departed_at: Option<Instant>,
    acknowledged: bool,
    probe_id: Option<u32>,
}

#[derive(Clone, Copy)]
struct PacketFeedback {
    departed_at: Instant,
    received_at: Duration,
    bytes: usize,
}

#[derive(Clone, Copy)]
struct PacketGroup {
    first_departure: Instant,
    last_departure: Instant,
    first_arrival: Duration,
    complete_arrival: Duration,
}

impl PacketGroup {
    fn new(packet: PacketFeedback) -> Self {
        Self {
            first_departure: packet.departed_at,
            last_departure: packet.departed_at,
            first_arrival: packet.received_at,
            complete_arrival: packet.received_at,
        }
    }

    fn belongs_to_burst(&self, packet: PacketFeedback) -> bool {
        let arrival_delta = packet.received_at.saturating_sub(self.complete_arrival);
        let send_delta = packet
            .departed_at
            .saturating_duration_since(self.last_departure);
        if send_delta.is_zero() {
            return true;
        }
        packet.received_at >= self.complete_arrival
            && arrival_delta < send_delta
            && arrival_delta <= SEND_TIME_GROUP
            && packet.received_at.saturating_sub(self.first_arrival) < Duration::from_millis(100)
    }

    fn accepts(&self, packet: PacketFeedback) -> bool {
        self.belongs_to_burst(packet)
            || packet
                .departed_at
                .saturating_duration_since(self.first_departure)
                <= SEND_TIME_GROUP
    }

    fn push(&mut self, packet: PacketFeedback) {
        self.last_departure = self.last_departure.max(packet.departed_at);
        self.complete_arrival = self.complete_arrival.max(packet.received_at);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BandwidthUsage {
    Normal,
    Underusing,
    Overusing,
}

struct TrendlineEstimator {
    accumulated_delay_ms: f64,
    smoothed_delay_ms: f64,
    samples: VecDeque<(f64, f64)>,
    delta_count: usize,
    threshold: f64,
    overuse_time: Duration,
    overuse_count: u8,
    previous_trend: f64,
    last_threshold_update: Option<Duration>,
    hypothesis: BandwidthUsage,
}

impl TrendlineEstimator {
    fn new() -> Self {
        Self {
            accumulated_delay_ms: 0.0,
            smoothed_delay_ms: 0.0,
            samples: VecDeque::with_capacity(TRENDLINE_WINDOW),
            delta_count: 0,
            threshold: 12.5,
            overuse_time: Duration::ZERO,
            overuse_count: 0,
            previous_trend: 0.0,
            last_threshold_update: None,
            hypothesis: BandwidthUsage::Normal,
        }
    }

    fn update(
        &mut self,
        send_delta: Duration,
        arrival_delta: Duration,
        arrival_time: Duration,
    ) -> BandwidthUsage {
        let delta_ms = duration_ms(arrival_delta) - duration_ms(send_delta);
        self.delta_count = self.delta_count.saturating_add(1).min(1_000);
        self.accumulated_delay_ms += delta_ms;
        self.smoothed_delay_ms = 0.9 * self.smoothed_delay_ms + 0.1 * self.accumulated_delay_ms;
        self.samples
            .push_back((duration_ms(arrival_time), self.smoothed_delay_ms));
        if self.samples.len() > TRENDLINE_WINDOW {
            let _ = self.samples.pop_front();
        }
        let trend = if self.samples.len() == TRENDLINE_WINDOW {
            linear_fit_slope(&self.samples).unwrap_or(self.previous_trend)
        } else {
            self.previous_trend
        };
        let modified_trend = (self.delta_count.min(60) as f64) * trend * 4.0;
        let delta = send_delta;
        if modified_trend > self.threshold {
            if self.overuse_count == 0 {
                self.overuse_time = delta.checked_div(2).unwrap_or(Duration::ZERO);
            } else {
                self.overuse_time = self.overuse_time.saturating_add(delta);
            }
            self.overuse_count = self.overuse_count.saturating_add(1);
            if self.overuse_time > Duration::from_millis(10)
                && self.overuse_count > 1
                && trend >= self.previous_trend
            {
                self.hypothesis = BandwidthUsage::Overusing;
                self.overuse_time = Duration::ZERO;
                self.overuse_count = 0;
            }
        } else if modified_trend < -self.threshold {
            self.overuse_time = Duration::ZERO;
            self.overuse_count = 0;
            self.hypothesis = BandwidthUsage::Underusing;
        } else {
            self.overuse_time = Duration::ZERO;
            self.overuse_count = 0;
            self.hypothesis = BandwidthUsage::Normal;
        }
        self.previous_trend = trend;
        self.update_threshold(modified_trend, arrival_time);
        self.hypothesis
    }

    fn update_threshold(&mut self, modified_trend: f64, now: Duration) {
        let last = self.last_threshold_update.unwrap_or(now);
        self.last_threshold_update = Some(now);
        if modified_trend.abs() > self.threshold + 15.0 {
            return;
        }
        let elapsed_ms = duration_ms(now.saturating_sub(last)).min(100.0);
        let gain = if modified_trend.abs() < self.threshold {
            0.039
        } else {
            0.0087
        };
        self.threshold = (self.threshold
            + gain * (modified_trend.abs() - self.threshold) * elapsed_ms)
            .clamp(6.0, 600.0);
    }
}

struct DelayBasedBwe {
    previous: Option<PacketGroup>,
    current: Option<PacketGroup>,
    trendline: TrendlineEstimator,
    reordered_packets: u8,
}

impl DelayBasedBwe {
    fn new() -> Self {
        Self {
            previous: None,
            current: None,
            trendline: TrendlineEstimator::new(),
            reordered_packets: 0,
        }
    }

    fn update(&mut self, packet: PacketFeedback) -> Option<BandwidthUsage> {
        let Some(current) = self.current.as_mut() else {
            self.current = Some(PacketGroup::new(packet));
            return None;
        };
        if packet.departed_at < current.first_departure {
            self.reordered_packets = self.reordered_packets.saturating_add(1);
            if self.reordered_packets >= 3 {
                self.reset();
            }
            return None;
        }
        self.reordered_packets = 0;
        if current.accepts(packet) {
            current.push(packet);
            return None;
        }
        let next = PacketGroup::new(packet);
        let Some(completed) = self.current.replace(next) else {
            debug_assert!(
                false,
                "current packet group remains present after initialization"
            );
            return None;
        };
        let previous = self.previous.replace(completed)?;
        let send_delta = completed
            .last_departure
            .saturating_duration_since(previous.last_departure);
        let arrival_delta = completed
            .complete_arrival
            .saturating_sub(previous.complete_arrival);
        if send_delta.is_zero() || arrival_delta.is_zero() {
            return None;
        }
        Some(
            self.trendline
                .update(send_delta, arrival_delta, completed.complete_arrival),
        )
    }

    fn reset(&mut self) {
        self.previous = None;
        self.current = None;
        self.trendline = TrendlineEstimator::new();
        self.reordered_packets = 0;
    }
}

struct AcknowledgedBitrateEstimator {
    window_bytes: u64,
    window_elapsed: Duration,
    previous_received: Option<Duration>,
    estimate_kbps: Option<f64>,
    estimate_variance: f64,
}

impl AcknowledgedBitrateEstimator {
    fn new() -> Self {
        Self {
            window_bytes: 0,
            window_elapsed: Duration::ZERO,
            previous_received: None,
            estimate_kbps: None,
            estimate_variance: 50.0,
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the rate calculation uses saturating arithmetic and a nonzero checked interval"
    )]
    fn update(&mut self, received: Duration, bytes: usize, _alr: bool) -> Option<u64> {
        let rate_window = if self.estimate_kbps.is_some() {
            ACKNOWLEDGED_RATE_WINDOW
        } else {
            INITIAL_ACKNOWLEDGED_RATE_WINDOW
        };
        if let Some(previous) = self.previous_received {
            if received < previous {
                self.window_bytes = 0;
                self.window_elapsed = Duration::ZERO;
            } else {
                let elapsed = received.saturating_sub(previous);
                if elapsed > rate_window {
                    self.window_bytes = 0;
                    self.window_elapsed = Duration::ZERO;
                } else {
                    self.window_elapsed = self.window_elapsed.saturating_add(elapsed);
                }
            }
        }
        self.previous_received = Some(received);
        if self.window_elapsed >= rate_window {
            let elapsed_millis = self.window_elapsed.as_secs_f64() * 1_000.0;
            debug_assert!(elapsed_millis > 0.0);
            let sample_kbps = 8.0 * self.window_bytes as f64 / elapsed_millis;
            self.update_estimate(sample_kbps);
            self.window_elapsed = self.window_elapsed.saturating_sub(rate_window);
            self.window_bytes = 0;
        }
        self.window_bytes = self
            .window_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.estimate_kbps
            .map(|estimate| (estimate * 1_000.0).max(0.0) as u64)
    }

    fn expect_fast_rate_change(&mut self) {
        self.estimate_variance += 200.0;
    }

    fn update_estimate(&mut self, sample_kbps: f64) {
        debug_assert!(sample_kbps >= 0.0);
        let Some(estimate_kbps) = self.estimate_kbps else {
            self.estimate_kbps = Some(sample_kbps);
            return;
        };
        let denominator = estimate_kbps.max(f64::EPSILON);
        let uncertainty = 10.0 * (estimate_kbps - sample_kbps).abs() / denominator;
        let sample_variance = uncertainty * uncertainty;
        let predicted_variance = self.estimate_variance + 5.0;
        let combined_variance = sample_variance + predicted_variance;
        debug_assert!(combined_variance > 0.0);
        self.estimate_kbps = Some(
            (sample_variance * estimate_kbps + predicted_variance * sample_kbps)
                / combined_variance,
        );
        self.estimate_variance = sample_variance * predicted_variance / combined_variance;
    }
}

#[derive(Clone, Copy)]
struct LossEvidence {
    lost: usize,
    total: usize,
}

struct LossBasedBwe {
    samples: VecDeque<(Instant, usize, usize)>,
    last_decrease: Option<Instant>,
}

impl LossBasedBwe {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            last_decrease: None,
        }
    }

    fn update(&mut self, now: Instant, acknowledged: usize, lost: usize) -> Option<LossEvidence> {
        let total = acknowledged.saturating_add(lost);
        if total == 0 {
            return None;
        }
        self.samples.push_back((now, acknowledged, lost));
        let cutoff = now.checked_sub(LOSS_DECREASE_INTERVAL).unwrap_or(now);
        while self.samples.front().is_some_and(|(at, _, _)| *at < cutoff) {
            let _ = self.samples.pop_front();
        }
        if self
            .last_decrease
            .is_some_and(|last| now.saturating_duration_since(last) < LOSS_DECREASE_INTERVAL)
        {
            return None;
        }
        let (acknowledged, lost) = self.samples.iter().fold(
            (0usize, 0usize),
            |(acknowledged, lost), (_, sample_acknowledged, sample_lost)| {
                (
                    acknowledged.saturating_add(*sample_acknowledged),
                    lost.saturating_add(*sample_lost),
                )
            },
        );
        let total = acknowledged.saturating_add(lost);
        let decrease = total >= LOSS_DECREASE_THRESHOLD && lost.saturating_mul(10) > total;
        if decrease {
            self.last_decrease = Some(now);
            Some(LossEvidence { lost, total })
        } else {
            None
        }
    }
}

struct AlrDetector {
    budget_bytes: i64,
    last_sent: Option<Instant>,
    started: Option<Instant>,
}

impl AlrDetector {
    fn new() -> Self {
        Self {
            budget_bytes: 0,
            last_sent: None,
            started: None,
        }
    }

    fn on_bytes_sent(&mut self, now: Instant, bytes: usize, bitrate_bps: u64) {
        let Some(last) = self.last_sent.replace(now) else {
            return;
        };
        let elapsed = now.saturating_duration_since(last);
        if elapsed > OUTAGE {
            self.budget_bytes = 0;
            self.started = None;
            return;
        }
        let target_bitrate_bps = bitrate_bps
            .saturating_mul(ALR_BANDWIDTH_USAGE_PERCENT)
            .saturating_div(100);
        let earned_bytes = target_bitrate_bps
            .saturating_mul(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
            .saturating_div(8_000_000);
        let max_budget_bytes = target_bitrate_bps
            .saturating_mul(u64::try_from(ALR_BUDGET_WINDOW.as_micros()).unwrap_or(u64::MAX))
            .saturating_div(8_000_000);
        let max_budget_bytes = i64::try_from(max_budget_bytes).unwrap_or(i64::MAX);
        self.budget_bytes = self
            .budget_bytes
            .saturating_sub(i64::try_from(bytes).unwrap_or(i64::MAX))
            .saturating_add(i64::try_from(earned_bytes).unwrap_or(i64::MAX))
            .clamp(max_budget_bytes.saturating_neg(), max_budget_bytes);
        let start = max_budget_bytes
            .saturating_mul(ALR_START_BUDGET_PERCENT)
            .saturating_div(100);
        let stop = max_budget_bytes
            .saturating_mul(ALR_STOP_BUDGET_PERCENT)
            .saturating_div(100);
        if self.budget_bytes > start {
            self.started.get_or_insert(now);
        } else if self.budget_bytes < stop {
            self.started = None;
        }
    }

    fn is_alr(&self, now: Instant) -> bool {
        self.started.is_some()
            || self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) > OUTAGE)
    }
}

struct ProbePacket {
    departed_at: Instant,
    received_at: Option<Option<Duration>>,
    bytes: usize,
}

struct ActiveProbe {
    id: u32,
    scale: u64,
    target_bitrate_bps: u64,
    packet_count: u8,
    first_departure_at: Option<Instant>,
    completed: bool,
    packets: BTreeMap<SendId, ProbePacket>,
}

struct ProbeController {
    initial_bitrate_bps: Option<u64>,
    next_initial_probe_scale: Option<u64>,
    max_total_allocated_bitrate_bps: u64,
    pending_allocation_probe_bps: Option<u64>,
    last_alr_probe: Option<Instant>,
    next_id: u32,
    active: Option<ActiveProbe>,
}

impl ProbeController {
    fn new() -> Self {
        Self {
            initial_bitrate_bps: None,
            next_initial_probe_scale: Some(3),
            max_total_allocated_bitrate_bps: 0,
            pending_allocation_probe_bps: None,
            last_alr_probe: None,
            next_id: 0,
            active: None,
        }
    }

    fn set_max_total_allocated_bitrate(&mut self, bitrate_bps: u64) {
        if bitrate_bps > self.max_total_allocated_bitrate_bps {
            self.pending_allocation_probe_bps = Some(bitrate_bps);
        }
        self.max_total_allocated_bitrate_bps = bitrate_bps;
    }

    fn initial_probe_pending(&self) -> bool {
        self.next_initial_probe_scale.is_some()
            || self
                .active
                .as_ref()
                .is_some_and(|active| active.scale == 3 || active.scale == 6)
    }

    fn next(
        &mut self,
        now: Instant,
        estimate_bps: u64,
        application_limited: bool,
        congested: bool,
    ) -> Option<ProbeDecision> {
        if congested {
            return None;
        }
        if self.active.as_ref().is_some_and(|active| {
            active.first_departure_at.is_some_and(|started| {
                now.saturating_duration_since(started) > PROBE_VALIDATION_WINDOW
            })
        }) {
            self.active = None;
        }
        if self.active.is_some() {
            return None;
        }
        if self
            .pending_allocation_probe_bps
            .is_some_and(|target| target <= estimate_bps)
        {
            self.pending_allocation_probe_bps = None;
        }
        let (target_bitrate_bps, scale) = if let Some(scale) = self.next_initial_probe_scale {
            let initial_bitrate_bps = *self.initial_bitrate_bps.get_or_insert(estimate_bps);
            let default_target = initial_bitrate_bps
                .saturating_mul(scale)
                .min(DEFAULT_MAX_PROBE_BITRATE_BPS);
            let allocation_target = self
                .pending_allocation_probe_bps
                .take()
                .unwrap_or_default()
                .saturating_mul(ALLOCATION_PROBE_SCALE_NUMERATOR)
                .saturating_div(ALLOCATION_PROBE_SCALE_DENOMINATOR)
                .min(DEFAULT_MAX_PROBE_BITRATE_BPS);
            let target_bitrate_bps = default_target.max(allocation_target);
            self.next_initial_probe_scale = (scale == 3
                && target_bitrate_bps < initial_bitrate_bps.saturating_mul(6))
            .then_some(6);
            (target_bitrate_bps, scale)
        } else if let Some(target_bitrate_bps) = self.pending_allocation_probe_bps.take() {
            (
                target_bitrate_bps
                    .saturating_mul(ALLOCATION_PROBE_SCALE_NUMERATOR)
                    .saturating_div(ALLOCATION_PROBE_SCALE_DENOMINATOR)
                    .min(DEFAULT_MAX_PROBE_BITRATE_BPS),
                1,
            )
        } else if application_limited
            && self
                .last_alr_probe
                .is_none_or(|last| now.saturating_duration_since(last) >= ALR_PROBE_INTERVAL)
        {
            let allocation_cap = if self.max_total_allocated_bitrate_bps == 0 {
                DEFAULT_MAX_PROBE_BITRATE_BPS
            } else {
                self.max_total_allocated_bitrate_bps
                    .saturating_mul(ALR_PROBE_SCALE_NUMERATOR)
                    .saturating_div(ALR_PROBE_SCALE_DENOMINATOR)
            };
            let target = estimate_bps
                .max(self.max_total_allocated_bitrate_bps)
                .saturating_mul(ALR_PROBE_SCALE_NUMERATOR)
                .saturating_div(ALR_PROBE_SCALE_DENOMINATOR)
                .min(allocation_cap)
                .min(DEFAULT_MAX_PROBE_BITRATE_BPS);
            (target, 0)
        } else {
            return None;
        };
        let decision = ProbeDecision {
            id: self.next_id,
            target_bitrate_bps,
            packet_count: 5,
            min_duration: INITIAL_PROBE_DURATION,
        };
        if application_limited {
            self.last_alr_probe = Some(now);
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.active = Some(ActiveProbe {
            id: decision.id,
            scale,
            target_bitrate_bps: decision.target_bitrate_bps,
            packet_count: decision.packet_count,
            first_departure_at: None,
            completed: false,
            packets: BTreeMap::new(),
        });
        Some(decision)
    }

    fn record_departure(
        &mut self,
        probe_id: u32,
        send_id: SendId,
        departed_at: Instant,
        bytes: usize,
    ) {
        let Some(active) = self.active.as_mut() else {
            debug_assert!(false, "a probe packet must have an active probe");
            return;
        };
        if active.id != probe_id {
            debug_assert_eq!(
                active.id, probe_id,
                "a probe packet must belong to the active probe"
            );
            return;
        }
        active.first_departure_at.get_or_insert(departed_at);
        let previous = active.packets.insert(
            send_id,
            ProbePacket {
                departed_at,
                received_at: None,
                bytes,
            },
        );
        debug_assert!(previous.is_none(), "a probe packet is recorded once");
    }

    fn record_feedback(&mut self, probe_id: u32, send_id: SendId, received_at: Option<Duration>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.id != probe_id {
            return;
        }
        let Some(packet) = active.packets.get_mut(&send_id) else {
            return;
        };
        debug_assert!(
            packet.received_at.is_none(),
            "a probe packet has one feedback status"
        );
        packet.received_at = Some(received_at);
    }

    fn complete(&mut self, probe_id: u32) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.id == probe_id {
            active.completed = true;
        }
    }

    fn result(&mut self, now: Instant) -> Option<u64> {
        let active = self.active.as_ref()?;
        if active
            .first_departure_at
            .is_some_and(|started| now.saturating_duration_since(started) > PROBE_VALIDATION_WINDOW)
        {
            self.active = None;
            return None;
        }
        if !active.completed
            || active.packets.len() < usize::from(active.packet_count)
            || active
                .packets
                .values()
                .any(|packet| packet.received_at.is_none())
        {
            return None;
        }
        let mut packets: Vec<_> = active
            .packets
            .values()
            .filter_map(|packet| {
                packet
                    .received_at
                    .flatten()
                    .map(|received_at| (packet.departed_at, received_at, packet.bytes))
            })
            .collect();
        if packets.len() < MIN_RECEIVED_PROBE_PACKETS {
            self.active = None;
            return None;
        }
        let sent_packets = u64::try_from(active.packets.len()).unwrap_or(u64::MAX);
        let received_packets = u64::try_from(packets.len()).unwrap_or(u64::MAX);
        let sent_bytes = active.packets.values().fold(0u64, |total, packet| {
            total.saturating_add(u64::try_from(packet.bytes).unwrap_or(u64::MAX))
        });
        let received_bytes = packets.iter().fold(0u64, |total, (_, _, bytes)| {
            total.saturating_add(u64::try_from(*bytes).unwrap_or(u64::MAX))
        });
        if received_packets.saturating_mul(100)
            < sent_packets.saturating_mul(MIN_RECEIVED_PROBE_PERCENT)
            || received_bytes.saturating_mul(100)
                < sent_bytes.saturating_mul(MIN_RECEIVED_PROBE_PERCENT)
        {
            self.active = None;
            return None;
        }
        packets.sort_unstable_by_key(|packet| packet.0);
        let (first_departed, _, _) = packets.first().copied()?;
        let (last_departed, _, last_sent_bytes) = packets.last().copied()?;
        let send_bytes =
            received_bytes.saturating_sub(u64::try_from(last_sent_bytes).unwrap_or(u64::MAX));
        packets.sort_unstable_by_key(|packet| packet.1);
        let (_, first_received, first_received_bytes) = packets.first().copied()?;
        let (_, last_received, _) = packets.last().copied()?;
        let receive_bytes =
            received_bytes.saturating_sub(u64::try_from(first_received_bytes).unwrap_or(u64::MAX));
        let send_rate = rate_from(
            send_bytes,
            last_departed.saturating_duration_since(first_departed),
        )?;
        let receive_rate = rate_from(receive_bytes, last_received.saturating_sub(first_received))?;
        if receive_rate > send_rate.saturating_mul(MAX_PROBE_RECEIVE_SEND_RATIO) {
            self.active = None;
            return None;
        }
        let result = if receive_rate.saturating_mul(100)
            < send_rate.saturating_mul(MIN_UNSATURATED_PROBE_PERCENT)
        {
            receive_rate
                .saturating_mul(PROBE_CAPACITY_UTILIZATION_PERCENT)
                .saturating_div(100)
        } else {
            send_rate.min(receive_rate)
        }
        .min(active.target_bitrate_bps);
        if active.scale == 3
            && result.saturating_mul(10) < active.target_bitrate_bps.saturating_mul(7)
        {
            self.next_initial_probe_scale = None;
        }
        self.active = None;
        Some(result)
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the interval is checked nonzero before converting its bounded microseconds"
)]
fn rate_from(bytes: u64, interval: Duration) -> Option<u64> {
    (!interval.is_zero()).then(|| {
        bytes
            .saturating_mul(8)
            .saturating_mul(1_000_000)
            .saturating_div(u64::try_from(interval.as_micros()).unwrap_or(u64::MAX))
    })
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn linear_fit_slope(samples: &VecDeque<(f64, f64)>) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let count = samples.len() as f64;
    let (sum_x, sum_y) = samples
        .iter()
        .fold((0.0, 0.0), |(x, y), (sample_x, sample_y)| {
            (x + sample_x, y + sample_y)
        });
    let mean_x = sum_x / count;
    let mean_y = sum_y / count;
    let (covariance, variance) = samples.iter().fold((0.0, 0.0), |(cov, var), (x, y)| {
        let x = x - mean_x;
        (cov + x * (y - mean_y), var + x * x)
    });
    (variance > f64::EPSILON).then_some(covariance / variance)
}

pub struct Gcc {
    next_transport_sequence: u16,
    records: HashMap<SendId, SendRecord>,
    sequence_index: HashMap<u16, SendId>,
    history_order: VecDeque<SendId>,
    history_capacity: usize,
    bitrate_bps: u64,
    acknowledged_bitrate: AcknowledgedBitrateEstimator,
    loss_based_bwe: LossBasedBwe,
    delay_based_bwe: DelayBasedBwe,
    alr: AlrDetector,
    probe_controller: ProbeController,
    last_delay_decrease: Option<Instant>,
    previously_in_alr: bool,
    last_departure: Option<Instant>,
    last_feedback: Option<Instant>,
    last_outage_decay: Option<Instant>,
}

impl Gcc {
    pub fn new(history_capacity: usize) -> Self {
        Self::with_initial_bitrate(history_capacity, DEFAULT_INITIAL_BITRATE_BPS)
    }

    pub fn with_initial_bitrate(history_capacity: usize, initial_bitrate_bps: u64) -> Self {
        let history_capacity = history_capacity.max(1);
        Self {
            next_transport_sequence: 0,
            records: HashMap::with_capacity(history_capacity),
            sequence_index: HashMap::with_capacity(history_capacity),
            history_order: VecDeque::with_capacity(history_capacity),
            history_capacity,
            bitrate_bps: initial_bitrate_bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS),
            acknowledged_bitrate: AcknowledgedBitrateEstimator::new(),
            loss_based_bwe: LossBasedBwe::new(),
            delay_based_bwe: DelayBasedBwe::new(),
            alr: AlrDetector::new(),
            probe_controller: ProbeController::new(),
            last_delay_decrease: None,
            previously_in_alr: false,
            last_departure: None,
            last_feedback: None,
            last_outage_decay: None,
        }
    }

    pub fn estimate(&self, now: Instant) -> CongestionEstimate {
        CongestionEstimate {
            bitrate_bps: self.bitrate_bps,
            application_limited: self.alr.is_alr(now),
        }
    }

    pub fn pacing_bitrate_bps(&self) -> u64 {
        self.bitrate_bps
            .saturating_mul(PACING_RATE_NUMERATOR)
            .saturating_div(PACING_RATE_DENOMINATOR)
    }

    pub fn has_feedback(&self) -> bool {
        self.last_feedback.is_some()
    }

    pub fn initial_probe_pending(&self) -> bool {
        self.probe_controller.initial_probe_pending()
    }

    pub fn start(&mut self, now: Instant) -> GccOutcome {
        let probe = self.probe_controller.next(
            now,
            self.bitrate_bps,
            self.estimate(now).application_limited(),
            false,
        );
        self.outcome(now, CongestionState::Normal, 0, 0, probe)
    }

    pub fn set_max_total_allocated_bitrate(&mut self, bitrate_bps: u64) {
        self.probe_controller
            .set_max_total_allocated_bitrate(bitrate_bps);
    }

    pub(crate) fn maintenance_probe(
        &mut self,
        now: Instant,
        bitrate_bps: u64,
    ) -> Option<GccOutcome> {
        self.probe_controller
            .set_max_total_allocated_bitrate(bitrate_bps);
        let probe = self
            .probe_controller
            .next(now, self.bitrate_bps, true, false)?;
        Some(self.outcome(now, CongestionState::Normal, 0, 0, Some(probe)))
    }

    pub fn assign(&mut self, send_id: SendId, bytes: usize) -> Result<EgressCongestion, GccError> {
        self.assign_with_probe(send_id, bytes, None)
    }

    pub fn assign_probe(
        &mut self,
        send_id: SendId,
        bytes: usize,
        probe_id: u32,
    ) -> Result<EgressCongestion, GccError> {
        self.assign_with_probe(send_id, bytes, Some(probe_id))
    }

    fn assign_with_probe(
        &mut self,
        send_id: SendId,
        bytes: usize,
        probe_id: Option<u32>,
    ) -> Result<EgressCongestion, GccError> {
        if self.records.contains_key(&send_id) {
            return Err(GccError::DuplicateSend(send_id));
        }
        while self.history_order.len() >= self.history_capacity {
            let Some(expired) = self.history_order.pop_front() else {
                break;
            };
            if let Some(record) = self.records.remove(&expired) {
                self.sequence_index.remove(&record.transport_sequence);
            }
        }
        let sequence = self.next_transport_sequence;
        self.next_transport_sequence = self.next_transport_sequence.wrapping_add(1);
        let record = SendRecord {
            transport_sequence: sequence,
            bytes,
            departed_at: None,
            acknowledged: false,
            probe_id,
        };
        self.records.insert(send_id, record);
        self.sequence_index.insert(sequence, send_id);
        self.history_order.push_back(send_id);
        Ok(EgressCongestion {
            send_id,
            transport_sequence: sequence,
        })
    }

    pub fn record_departure(&mut self, send_id: SendId, now: Instant) -> Result<(), GccError> {
        let record = self
            .records
            .get_mut(&send_id)
            .ok_or(GccError::UnknownSend(send_id))?;
        if record.departed_at.is_some() {
            return Err(GccError::DuplicateDeparture(send_id));
        }
        record.departed_at = Some(now);
        if let Some(probe_id) = record.probe_id {
            self.probe_controller
                .record_departure(probe_id, send_id, now, record.bytes);
        }
        self.last_departure = Some(now);
        self.alr.on_bytes_sent(now, record.bytes, self.bitrate_bps);
        Ok(())
    }

    pub fn complete_probe(&mut self, probe_id: u32) {
        self.probe_controller.complete(probe_id);
    }

    pub fn process_feedback(&mut self, now: Instant, feedback: &TwccFeedback) -> GccOutcome {
        let mut acknowledged = Vec::new();
        let mut lost = 0usize;
        for status in feedback.statuses() {
            let Some(send_id) = self.sequence_index.get(&status.sequence()).copied() else {
                continue;
            };
            let Some(record) = self.records.get_mut(&send_id) else {
                continue;
            };
            let Some(departed_at) = record.departed_at else {
                continue;
            };
            if record.acknowledged {
                continue;
            }
            record.acknowledged = true;
            if let Some(probe_id) = record.probe_id {
                self.probe_controller
                    .record_feedback(probe_id, send_id, status.received_at());
            }
            if let Some(received_at) = status.received_at() {
                acknowledged.push(PacketFeedback {
                    departed_at,
                    received_at,
                    bytes: record.bytes,
                });
            } else {
                lost = lost.saturating_add(1);
            }
        }
        let acknowledged_count = acknowledged.len();
        let total = acknowledged_count.saturating_add(lost);
        let loss_evidence = self.loss_based_bwe.update(now, acknowledged_count, lost);
        let loss_congested = loss_evidence.is_some();
        acknowledged.sort_unstable_by_key(|packet: &PacketFeedback| packet.received_at);
        let mut delay_state = BandwidthUsage::Normal;
        for packet in &acknowledged {
            if let Some(state) = self.delay_based_bwe.update(*packet) {
                delay_state = state;
            }
        }
        let congested = delay_state == BandwidthUsage::Overusing || loss_congested;
        let previous_feedback = self.last_feedback;
        self.last_feedback = Some(now);
        self.last_outage_decay = None;
        if total > 0 {
            let first_feedback = previous_feedback.is_none();
            self.update_estimate(
                now,
                previous_feedback,
                &acknowledged,
                loss_evidence,
                delay_state,
                first_feedback,
            );
        }
        let probe = self.probe_controller.next(
            now,
            self.bitrate_bps,
            self.estimate(now).application_limited(),
            congested,
        );
        let state = if loss_congested {
            CongestionState::LossLimited
        } else {
            match delay_state {
                BandwidthUsage::Normal => CongestionState::Normal,
                BandwidthUsage::Underusing => CongestionState::Underusing,
                BandwidthUsage::Overusing => CongestionState::DelayLimited,
            }
        };
        self.outcome(now, state, acknowledged_count, lost, probe)
    }

    pub fn process_rtcp(
        &mut self,
        now: Instant,
        rtcp: &CompoundRtcpView<'_>,
    ) -> Result<Vec<GccOutcome>, GccError> {
        let feedback = parse_twcc(rtcp)?;
        Ok(feedback
            .iter()
            .map(|feedback| self.process_feedback(now, feedback))
            .collect())
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.estimate(now).application_limited() {
            return None;
        }
        let feedback = self.last_feedback?;
        let base = self.last_outage_decay.unwrap_or(feedback);
        base.checked_add(OUTAGE)
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Option<GccOutcome> {
        if self.estimate(now).application_limited() {
            return None;
        }
        let feedback = self.last_feedback?;
        let first_decay = feedback.checked_add(OUTAGE).unwrap_or(feedback);
        let due = self
            .last_outage_decay
            .and_then(|decay| decay.checked_add(OUTAGE))
            .unwrap_or(first_decay);
        if now < due {
            return None;
        }
        self.bitrate_bps = self
            .bitrate_bps
            .saturating_mul(4)
            .saturating_div(5)
            .max(MIN_BITRATE_BPS);
        self.last_outage_decay = Some(now);
        Some(self.outcome(now, CongestionState::FeedbackOutage, 0, 0, None))
    }

    fn outcome(
        &self,
        now: Instant,
        state: CongestionState,
        acknowledged: usize,
        lost: usize,
        probe: Option<ProbeDecision>,
    ) -> GccOutcome {
        let estimate = self.estimate(now);
        GccOutcome {
            estimate,
            pacing_bitrate_bps: self.pacing_bitrate_bps(),
            padding_bitrate_bps: 0,
            state,
            acknowledged,
            lost,
            probe,
        }
    }

    fn update_estimate(
        &mut self,
        now: Instant,
        previous_feedback: Option<Instant>,
        acknowledged: &[PacketFeedback],
        loss_evidence: Option<LossEvidence>,
        delay_state: BandwidthUsage,
        first_feedback: bool,
    ) {
        let alr = self.estimate(now).application_limited();
        if self.previously_in_alr && !alr {
            self.acknowledged_bitrate.expect_fast_rate_change();
        }
        self.previously_in_alr = alr;
        let acknowledged_rate = acknowledged.iter().fold(None, |rate, packet| {
            self.acknowledged_bitrate
                .update(packet.received_at, packet.bytes, alr)
                .or(rate)
        });
        let probe_bitrate = self.probe_controller.result(now);
        if delay_state == BandwidthUsage::Overusing {
            let reduction_due = self
                .last_delay_decrease
                .is_none_or(|last| now.saturating_duration_since(last) >= DELAY_DECREASE_INTERVAL)
                || acknowledged_rate.is_some_and(|rate| rate < self.bitrate_bps.saturating_div(2));
            if reduction_due {
                let throughput = if alr {
                    self.bitrate_bps
                } else {
                    acknowledged_rate.unwrap_or(self.bitrate_bps)
                };
                self.bitrate_bps = self.bitrate_bps.min(delay_backoff_target(throughput));
                self.last_delay_decrease = Some(now);
            }
        } else if let Some(loss) = loss_evidence {
            self.bitrate_bps = loss_backoff_target(self.bitrate_bps, loss);
        } else if let Some(promoted) = probe_bitrate {
            self.bitrate_bps = self.bitrate_bps.max(promoted);
        } else if first_feedback && let Some(rate) = acknowledged_rate {
            self.bitrate_bps = self
                .bitrate_bps
                .max(rate.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS));
        } else if !alr {
            let elapsed = previous_feedback
                .and_then(|last| now.checked_duration_since(last))
                .unwrap_or(Duration::ZERO)
                .min(Duration::from_secs(1));
            let multiplicative_increase = self
                .bitrate_bps
                .saturating_mul(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                .saturating_div(12_500);
            let packet_bits = acknowledged
                .iter()
                .map(|packet| u64::try_from(packet.bytes).unwrap_or(u64::MAX))
                .sum::<u64>()
                .checked_div(u64::try_from(acknowledged.len()).unwrap_or(u64::MAX).max(1))
                .unwrap_or_default()
                .saturating_mul(8);
            let additive_rate = packet_bits
                .saturating_mul(1_000_000)
                .checked_div(
                    u64::try_from(AIMD_RESPONSE_TIME.as_micros())
                        .unwrap_or(u64::MAX)
                        .max(1),
                )
                .unwrap_or_default()
                .max(MIN_ADDITIVE_INCREASE_BPS_PER_SECOND);
            let additive_increase = additive_rate
                .saturating_mul(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
                .saturating_div(1_000_000);
            let increase = multiplicative_increase.max(additive_increase);
            let ceiling = acknowledged_rate
                .map(|rate| rate.saturating_mul(3).saturating_div(2))
                .unwrap_or(MAX_BITRATE_BPS);
            if self.bitrate_bps < ceiling {
                self.bitrate_bps = self.bitrate_bps.saturating_add(increase).min(ceiling);
            }
        }
        self.bitrate_bps = self.bitrate_bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS);
        debug_assert!((MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&self.bitrate_bps));
    }
}

fn delay_backoff_target(acknowledged_bitrate_bps: u64) -> u64 {
    acknowledged_bitrate_bps
        .saturating_mul(85)
        .saturating_div(100)
}

fn loss_backoff_target(bitrate_bps: u64, evidence: LossEvidence) -> u64 {
    debug_assert!(evidence.total > 0);
    let denominator = u64::try_from(evidence.total)
        .unwrap_or(u64::MAX)
        .saturating_mul(2);
    let numerator = denominator.saturating_sub(u64::try_from(evidence.lost).unwrap_or(u64::MAX));
    bitrate_bps
        .saturating_mul(numerator)
        .saturating_div(denominator.max(1))
}

pub fn parse_twcc(rtcp: &CompoundRtcpView<'_>) -> Result<Vec<TwccFeedback>, GccError> {
    let mut feedback = Vec::new();
    for packet in rtcp.packets() {
        if packet.packet_type() == TWCC_PACKET_TYPE && packet.report_count() == TWCC_FORMAT {
            feedback.push(parse_twcc_packet(packet.bytes())?);
        }
    }
    Ok(feedback)
}

#[cfg(test)]
#[path = "gcc/test_utils.rs"]
mod test_utils;

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    reason = "each TWCC field is obtained from a bounds-checked structural slice"
)]
fn parse_twcc_packet(bytes: &[u8]) -> Result<TwccFeedback, GccError> {
    let fixed = bytes.get(..20).ok_or(GccError::MalformedTwcc)?;
    let base_sequence = u16::from_be_bytes([fixed[12], fixed[13]]);
    let status_count = usize::from(u16::from_be_bytes([fixed[14], fixed[15]]));
    if status_count > TWCC_HISTORY_CAPACITY {
        return Err(GccError::MalformedTwcc);
    }
    let reference_time = u32::from_be_bytes([0, fixed[16], fixed[17], fixed[18]]);
    let mut offset = 20usize;
    let mut symbols = Vec::with_capacity(status_count);
    while symbols.len() < status_count {
        let chunk = bytes
            .get(offset..offset.saturating_add(2))
            .ok_or(GccError::MalformedTwcc)?;
        offset = offset.saturating_add(2);
        let chunk = u16::from_be_bytes([chunk[0], chunk[1]]);
        if chunk & 0x8000 == 0 {
            let symbol = ((chunk >> 13) & 0x03) as u8;
            let run = usize::from(chunk & 0x1fff);
            if run == 0 || symbols.len().saturating_add(run) > status_count {
                return Err(GccError::MalformedTwcc);
            }
            symbols.extend(std::iter::repeat_n(symbol, run));
        } else if chunk & 0x4000 == 0 {
            for shift in (0..14).rev() {
                if symbols.len() == status_count {
                    break;
                }
                symbols.push(((chunk >> shift) & 1) as u8);
            }
        } else {
            for shift in (0..7).rev() {
                if symbols.len() == status_count {
                    break;
                }
                symbols.push(((chunk >> (shift * 2)) & 3) as u8);
            }
        }
    }
    let mut received_at = Duration::from_micros(u64::from(reference_time).saturating_mul(64_000));
    let mut statuses = Vec::with_capacity(status_count);
    for (index, symbol) in symbols.into_iter().enumerate() {
        let sequence =
            base_sequence.wrapping_add(u16::try_from(index).map_err(|_| GccError::MalformedTwcc)?);
        let received = match symbol {
            0 => None,
            1 => {
                let delta = *bytes.get(offset).ok_or(GccError::MalformedTwcc)?;
                offset = offset.saturating_add(1);
                received_at = apply_delta(received_at, i64::from(delta).saturating_mul(250));
                Some(received_at)
            }
            2 => {
                let delta = bytes
                    .get(offset..offset.saturating_add(2))
                    .ok_or(GccError::MalformedTwcc)?;
                offset = offset.saturating_add(2);
                let delta = i16::from_be_bytes([delta[0], delta[1]]);
                received_at = apply_delta(received_at, i64::from(delta).saturating_mul(250));
                Some(received_at)
            }
            _ => return Err(GccError::MalformedTwcc),
        };
        statuses.push(TwccStatus {
            sequence,
            received_at: received,
        });
    }
    if offset > bytes.len() {
        return Err(GccError::MalformedTwcc);
    }
    Ok(TwccFeedback {
        statuses: statuses.into_boxed_slice(),
    })
}

fn apply_delta(at: Duration, delta_micros: i64) -> Duration {
    if delta_micros >= 0 {
        at.saturating_add(Duration::from_micros(delta_micros as u64))
    } else {
        at.saturating_sub(Duration::from_micros(delta_micros.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use super::test_utils::{FeedbackImpairment, GccSimulation};
    use super::*;
    use crate::{IngressPacket, PacketId, PacketProvenance, TransportMetadata, TransportProtocol};

    fn feedback(statuses: &[(u16, Option<Duration>)]) -> TwccFeedback {
        TwccFeedback {
            statuses: statuses
                .iter()
                .map(|(sequence, received_at)| TwccStatus {
                    sequence: *sequence,
                    received_at: *received_at,
                })
                .collect(),
        }
    }

    #[test]
    fn gcc_uses_authoritative_departures_and_ignores_duplicates() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let first = gcc.assign(SendId::new(1), 1200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1200).expect("second send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");
        let report = feedback(&[
            (first.transport_sequence(), Some(Duration::from_millis(1))),
            (second.transport_sequence(), Some(Duration::from_millis(11))),
        ]);

        let first_outcome = gcc.process_feedback(now + Duration::from_millis(20), &report);
        let duplicate = gcc.process_feedback(now + Duration::from_millis(30), &report);

        assert_eq!(first_outcome.acknowledged(), 2);
        assert_eq!(duplicate.acknowledged(), 0);
        assert_eq!(duplicate.lost(), 0);
    }

    #[test]
    fn gcc_reduces_on_sustained_loss_and_recovers_after_outage() {
        let now = Instant::now();
        let mut gcc = Gcc::new(32);
        let mut statuses = Vec::new();
        for index in 0..20 {
            let send = gcc.assign(SendId::new(index + 1), 1_200).expect("send");
            gcc.record_departure(send.send_id(), now + Duration::from_millis(index))
                .expect("departure");
            statuses.push((send.transport_sequence(), None));
        }
        let before = gcc.estimate(now).bitrate_bps();
        let outcome = gcc.process_feedback(now + Duration::from_millis(30), &feedback(&statuses));

        assert!(outcome.estimate().bitrate_bps() < before);
        gcc.handle_timeout(now + OUTAGE + Duration::from_secs(1));
        assert!(
            gcc.estimate(now + OUTAGE + Duration::from_secs(1))
                .application_limited()
        );
    }

    #[test]
    fn gcc_schedules_one_decay_per_feedback_outage() {
        let now = Instant::now();
        let mut gcc = Gcc::new(128);
        let send = gcc.assign(SendId::new(1), 1200).expect("send");
        gcc.record_departure(send.send_id(), now)
            .expect("departure");
        let feedback_at = now + Duration::from_millis(10);
        gcc.process_feedback(
            feedback_at,
            &feedback(&[(send.transport_sequence(), Some(Duration::from_millis(1)))]),
        );

        let first_deadline = feedback_at + OUTAGE;
        assert_eq!(gcc.next_deadline(feedback_at), Some(first_deadline));
        assert!(
            gcc.handle_timeout(first_deadline - Duration::from_nanos(1))
                .is_none()
        );
        for index in 0..60 {
            let active = gcc
                .assign(SendId::new(index + 2), 1_200)
                .expect("active send");
            gcc.record_departure(
                active.send_id(),
                feedback_at + Duration::from_millis((index + 1) * 30),
            )
            .expect("active departure");
        }

        let first = gcc.handle_timeout(first_deadline).expect("outage decay");
        assert!(!first.estimate().application_limited());
        assert!(first.estimate().bitrate_bps() < DEFAULT_INITIAL_BITRATE_BPS);
        assert_eq!(
            gcc.next_deadline(first_deadline),
            Some(first_deadline + OUTAGE)
        );
        assert!(
            gcc.handle_timeout(first_deadline + Duration::from_millis(1))
                .is_none()
        );
    }

    #[test]
    fn gcc_does_not_decay_an_application_limited_sender() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let send = gcc.assign(SendId::new(1), 1200).expect("send");
        gcc.record_departure(send.send_id(), now)
            .expect("departure");
        gcc.process_feedback(
            now + Duration::from_millis(10),
            &feedback(&[(send.transport_sequence(), Some(Duration::from_millis(1)))]),
        );
        let idle = now + OUTAGE + Duration::from_secs(1);

        assert!(gcc.estimate(idle).application_limited());
        assert_eq!(gcc.next_deadline(idle), None);
        assert!(gcc.handle_timeout(idle).is_none());
    }

    #[test]
    fn gcc_accepts_a_policy_selected_initial_bitrate() {
        let now = Instant::now();
        let gcc = Gcc::with_initial_bitrate(8, 2_000_000);

        assert_eq!(gcc.estimate(now).bitrate_bps(), 2_000_000);
    }

    #[test]
    fn gcc_promotes_its_first_clean_throughput_sample() {
        let now = Instant::now();
        let mut gcc = Gcc::new(128);
        let mut statuses = Vec::new();
        for index in 0..=63u64 {
            let send = gcc
                .assign(SendId::new(index + 1), 1_200)
                .expect("clean send");
            let at = Duration::from_millis(index.saturating_mul(8));
            gcc.record_departure(send.send_id(), now + at)
                .expect("clean departure");
            statuses.push((send.transport_sequence(), Some(at)));
        }

        let outcome = gcc.process_feedback(now + Duration::from_millis(550), &feedback(&statuses));

        assert!(outcome.estimate().bitrate_bps() > DEFAULT_INITIAL_BITRATE_BPS);
    }

    #[test]
    fn acknowledged_rate_adapts_quickly_after_application_limited_operation() {
        fn low_rate_estimator() -> AcknowledgedBitrateEstimator {
            let mut estimator = AcknowledgedBitrateEstimator::new();
            for index in 0..=10u64 {
                let _ = estimator.update(Duration::from_millis(index * 50), 1_200, true);
            }
            estimator
        }

        let mut ordinary = low_rate_estimator();
        let mut accelerated = low_rate_estimator();
        accelerated.expect_fast_rate_change();
        for index in 1..=15u64 {
            let at = Duration::from_millis(500 + index * 10);
            let _ = ordinary.update(at, 1_200, false);
            let _ = accelerated.update(at, 1_200, false);
        }

        assert!(
            accelerated.estimate_kbps.expect("accelerated estimate")
                > ordinary.estimate_kbps.expect("ordinary estimate")
        );
    }

    #[test]
    fn loss_backoff_is_proportional_to_observed_loss() {
        assert_eq!(
            loss_backoff_target(1_000_000, LossEvidence { lost: 5, total: 20 },),
            875_000
        );
    }

    #[test]
    fn normal_feedback_never_uses_the_increase_ceiling_as_a_decrease() {
        let now = Instant::now();
        let mut gcc = Gcc::with_initial_bitrate(16, 1_000_000);
        let first = gcc.assign(SendId::new(1), 1_200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1_200).expect("second send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(100))
            .expect("second departure");

        let outcome = gcc.process_feedback(
            now + Duration::from_millis(200),
            &feedback(&[
                (first.transport_sequence(), Some(Duration::ZERO)),
                (
                    second.transport_sequence(),
                    Some(Duration::from_millis(100)),
                ),
            ]),
        );

        assert_eq!(outcome.state(), CongestionState::Normal);
        assert_eq!(outcome.estimate().bitrate_bps(), 1_000_000);
    }

    #[test]
    fn delay_backoff_tracks_measured_throughput() {
        let measured_throughput = 2_400_000;
        let decreased = delay_backoff_target(measured_throughput);

        assert_eq!(decreased, 2_040_000);
    }

    #[test]
    fn pacing_retains_headroom_after_send_side_feedback() {
        let now = Instant::now();
        let mut gcc = Gcc::with_initial_bitrate(8, 2_000_000);

        assert_eq!(gcc.pacing_bitrate_bps(), 5_000_000);

        let send = gcc.assign(SendId::new(1), 1200).expect("unique send");
        gcc.record_departure(send.send_id(), now)
            .expect("known departure");
        let _ = gcc.process_feedback(
            now + Duration::from_millis(20),
            &feedback(&[(send.transport_sequence(), Some(Duration::from_millis(10)))]),
        );

        assert_eq!(gcc.pacing_bitrate_bps(), 5_000_000);
    }

    #[test]
    fn gcc_does_not_treat_a_reordered_feedback_gap_as_congestion() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let mut sequences = Vec::new();
        for index in 0..5 {
            let send = gcc
                .assign(SendId::new(index), 1200)
                .expect("unique send identity");
            gcc.record_departure(send.send_id(), now + Duration::from_millis(index * 10))
                .expect("authoritative departure");
            sequences.push(send.transport_sequence());
        }

        let outcome = gcc.process_feedback(
            now + Duration::from_millis(60),
            &feedback(&[
                (sequences[0], Some(Duration::from_millis(1))),
                (sequences[1], Some(Duration::from_millis(11))),
                (sequences[2], None),
                (sequences[3], Some(Duration::from_millis(31))),
                (sequences[4], Some(Duration::from_millis(41))),
            ]),
        );

        assert_eq!(outcome.lost(), 1);
        assert!(outcome.estimate().bitrate_bps() >= DEFAULT_INITIAL_BITRATE_BPS);
        assert!(outcome.probe().is_some());
    }

    #[test]
    fn gcc_only_promotes_a_measured_probe_cluster() {
        let now = Instant::now();
        let mut gcc = Gcc::new(32);
        let first = gcc.assign(SendId::new(1), 1_200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1_200).expect("second send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");
        let initial = gcc.process_feedback(
            now + Duration::from_millis(20),
            &feedback(&[
                (first.transport_sequence(), Some(Duration::from_millis(1))),
                (second.transport_sequence(), Some(Duration::from_millis(11))),
            ]),
        );
        let probe = initial.probe().expect("initial Google-style probe");
        let before = initial.estimate().bitrate_bps();

        let unrelated = gcc.assign(SendId::new(3), 1_200).expect("unrelated send");
        gcc.record_departure(unrelated.send_id(), now + Duration::from_millis(30))
            .expect("unrelated departure");
        let unrelated_feedback = gcc.process_feedback(
            now + Duration::from_millis(40),
            &feedback(&[(
                unrelated.transport_sequence(),
                Some(Duration::from_millis(31)),
            )]),
        );
        assert!(unrelated_feedback.estimate().bitrate_bps() < probe.target_bitrate_bps());

        let mut statuses = Vec::new();
        for index in 0..u64::from(probe.packet_count()) {
            let send_id = SendId::new(index + 10);
            let send = gcc
                .assign_probe(send_id, 1_200, probe.id())
                .expect("probe send");
            gcc.record_departure(send.send_id(), now + Duration::from_millis(50 + index))
                .expect("probe departure");
            statuses.push((
                send.transport_sequence(),
                Some(Duration::from_millis(51 + index)),
            ));
        }
        gcc.complete_probe(probe.id());
        let result = gcc.process_feedback(now + Duration::from_millis(80), &feedback(&statuses));

        assert!(result.estimate().bitrate_bps() > before);
        assert!(result.estimate().bitrate_bps() <= probe.target_bitrate_bps());
        let second = result.probe().expect("second Google-style initial probe");
        assert!(second.target_bitrate_bps() >= probe.target_bitrate_bps());
        assert!(second.target_bitrate_bps() <= DEFAULT_MAX_PROBE_BITRATE_BPS);
    }

    #[test]
    fn allocation_growth_requests_its_own_probe_cluster() {
        let now = Instant::now();
        let mut controller = ProbeController::new();
        controller.next_initial_probe_scale = None;
        controller.set_max_total_allocated_bitrate(DEFAULT_INITIAL_BITRATE_BPS * 2);
        controller.set_max_total_allocated_bitrate(2_000_000);

        let probe = controller
            .next(now, DEFAULT_INITIAL_BITRATE_BPS, false, false)
            .expect("an allocation above the estimate requests a probe");
        assert_eq!(probe.target_bitrate_bps(), 3_000_000);
        controller.active = None;
        assert!(
            controller
                .next(
                    now + Duration::from_secs(1),
                    DEFAULT_INITIAL_BITRATE_BPS,
                    false,
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn probe_estimate_accounts_for_packet_intervals_at_a_saturated_link() {
        let now = Instant::now();
        let mut controller = ProbeController::new();
        controller.next_initial_probe_scale = None;
        controller.set_max_total_allocated_bitrate(4_000_000);
        let probe = controller
            .next(now, DEFAULT_INITIAL_BITRATE_BPS, false, false)
            .expect("allocation probe");

        for index in 0..u64::from(probe.packet_count()) {
            let send_id = SendId::new(index);
            controller.record_departure(
                probe.id(),
                send_id,
                now + Duration::from_micros(index.saturating_mul(2_400)),
                1_200,
            );
            controller.record_feedback(
                probe.id(),
                send_id,
                Some(Duration::from_micros(index.saturating_mul(3_200))),
            );
        }
        controller.complete(probe.id());

        assert_eq!(
            controller.result(now + Duration::from_millis(20)),
            Some(2_850_000)
        );
    }

    #[test]
    fn delay_overuse_takes_precedence_over_a_probe_result() {
        let now = Instant::now();
        let mut gcc = Gcc::with_initial_bitrate(64, 1_000_000);
        gcc.alr.last_sent = Some(now);
        gcc.probe_controller.next_initial_probe_scale = None;
        gcc.probe_controller
            .set_max_total_allocated_bitrate(4_000_000);
        let probe = gcc
            .probe_controller
            .next(now, 1_000_000, false, false)
            .expect("allocation probe");
        for index in 0..u64::from(probe.packet_count()) {
            let send_id = SendId::new(index);
            gcc.probe_controller.record_departure(
                probe.id(),
                send_id,
                now + Duration::from_micros(index.saturating_mul(2_400)),
                1_200,
            );
            gcc.probe_controller.record_feedback(
                probe.id(),
                send_id,
                Some(Duration::from_micros(index.saturating_mul(3_200))),
            );
        }
        gcc.probe_controller.complete(probe.id());

        gcc.update_estimate(
            now + Duration::from_millis(20),
            None,
            &[],
            None,
            BandwidthUsage::Overusing,
            false,
        );

        assert!(gcc.bitrate_bps < 1_000_000);
    }

    #[test]
    fn initial_probe_includes_known_receiver_demand() {
        let now = Instant::now();
        let mut controller = ProbeController::new();
        controller.set_max_total_allocated_bitrate(1_400_000);

        let probe = controller
            .next(now, DEFAULT_INITIAL_BITRATE_BPS, false, false)
            .expect("initial receiver-demand probe");

        assert_eq!(probe.target_bitrate_bps(), 2_100_000);
        assert!(controller.next_initial_probe_scale.is_none());
    }

    #[test]
    fn a_probe_is_validated_from_its_first_departure_not_when_it_was_scheduled() {
        let scheduled_at = Instant::now();
        let departed_at = scheduled_at + Duration::from_secs(60);
        let mut gcc = Gcc::new(32);
        let outcome = gcc.start(scheduled_at);
        let probe = outcome.probe().expect("initial probe");
        let mut statuses = Vec::new();

        for index in 0..u64::from(probe.packet_count()) {
            let send = gcc
                .assign_probe(SendId::new(index), 1_200, probe.id())
                .expect("probe packet");
            gcc.record_departure(
                send.send_id(),
                departed_at + Duration::from_millis(index.saturating_mul(4)),
            )
            .expect("probe departure");
            statuses.push((
                send.transport_sequence(),
                Some(Duration::from_millis(index.saturating_mul(4))),
            ));
        }
        gcc.complete_probe(probe.id());

        let result = gcc.process_feedback(
            departed_at + Duration::from_millis(50),
            &feedback(&statuses),
        );

        assert!(result.estimate().bitrate_bps() > DEFAULT_INITIAL_BITRATE_BPS);
    }

    #[test]
    fn application_limited_sender_reprobes_after_a_bounded_interval() {
        let now = Instant::now();
        let mut controller = ProbeController::new();
        controller.next_initial_probe_scale = None;
        controller.set_max_total_allocated_bitrate(DEFAULT_INITIAL_BITRATE_BPS * 2);

        let probe = controller
            .next(now, DEFAULT_INITIAL_BITRATE_BPS, true, false)
            .expect("an ALR sender probes immediately");
        assert_eq!(probe.target_bitrate_bps(), DEFAULT_INITIAL_BITRATE_BPS * 3);
        controller.active = None;
        assert!(
            controller
                .next(
                    now + ALR_PROBE_INTERVAL - Duration::from_millis(1),
                    DEFAULT_INITIAL_BITRATE_BPS,
                    true,
                    false,
                )
                .is_none()
        );
        let periodic = controller
            .next(
                now + ALR_PROBE_INTERVAL,
                DEFAULT_INITIAL_BITRATE_BPS,
                true,
                false,
            )
            .expect("periodic ALR probe");
        assert_eq!(
            periodic.target_bitrate_bps(),
            DEFAULT_INITIAL_BITRATE_BPS * 11 / 5
        );
        assert_eq!(periodic.packet_count(), 5);
    }

    #[test]
    fn gcc_parses_twcc_from_structural_rtcp_view() {
        let source = SocketAddr::from(([127, 0, 0, 1], 5000));
        let destination = SocketAddr::from(([127, 0, 0, 1], 6000));
        let bytes = [
            0x8f, 205, 0, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 7, 0, 2, 0, 0, 0, 0, 0x20, 0x02, 1, 2,
        ];
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                Instant::now(),
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(1),
            ),
        )
        .parse()
        .expect("RTCP packet");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("RTCP packet");
        };

        let parsed = parse_twcc(&rtcp).expect("TWCC feedback");

        assert_eq!(parsed[0].statuses()[0].sequence(), 7);
        assert_eq!(parsed[0].statuses()[1].sequence(), 8);
    }

    #[test]
    fn twcc_receiver_reports_reordered_packets_and_loss() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(20, now, 9);
        receiver.observe(22, now + Duration::from_millis(10), 9);
        receiver.observe(21, now + Duration::from_millis(5), 9);
        let bytes = receiver
            .build_feedback(now + TWCC_FEEDBACK_INTERVAL, 7)
            .expect("due TWCC feedback")
            .to_vec();
        let source = SocketAddr::from(([127, 0, 0, 1], 5000));
        let destination = SocketAddr::from(([127, 0, 0, 1], 6000));
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                now,
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(1),
            ),
        )
        .parse()
        .expect("generated RTCP");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("generated TWCC must be RTCP");
        };
        let report = parse_twcc(&rtcp).expect("generated TWCC is structurally valid");
        let statuses = report[0].statuses();

        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].sequence(), 20);
        assert_eq!(statuses[1].sequence(), 21);
        assert_eq!(statuses[2].sequence(), 22);
        assert!(statuses.iter().all(|status| status.received_at().is_some()));

        receiver.observe(23, now + Duration::from_millis(60), 9);
        receiver.observe(25, now + Duration::from_millis(70), 9);
        let bytes = receiver
            .build_feedback(now + Duration::from_millis(110), 7)
            .expect("second TWCC feedback")
            .to_vec();
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                now,
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(2),
            ),
        )
        .parse()
        .expect("generated RTCP");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("generated TWCC must be RTCP");
        };
        let report = parse_twcc(&rtcp).expect("generated TWCC is structurally valid");
        let statuses = report[0].statuses();

        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].sequence(), 23);
        assert!(statuses[0].received_at().is_some());
        assert_eq!(statuses[1].sequence(), 24);
        assert!(statuses[1].received_at().is_none());
        assert_eq!(statuses[2].sequence(), 25);
        assert!(statuses[2].received_at().is_some());
    }

    #[test]
    fn twcc_receiver_does_not_report_a_recent_reordering_gap_as_loss() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(20, now, 9);
        receiver.observe(22, now + Duration::from_millis(49), 9);

        let first = receiver
            .build_feedback(now + TWCC_FEEDBACK_INTERVAL, 7)
            .expect("old packet is ready for feedback")
            .to_vec();
        let first = parse_twcc_packet(&first).expect("first feedback is valid");
        assert_eq!(first.statuses().len(), 1);
        assert_eq!(first.statuses()[0].sequence(), 20);

        receiver.observe(21, now + Duration::from_millis(55), 9);
        let second = receiver
            .build_feedback(now + Duration::from_millis(100), 7)
            .expect("reordered packets become ready")
            .to_vec();
        let second = parse_twcc_packet(&second).expect("second feedback is valid");
        assert_eq!(second.statuses().len(), 2);
        assert_eq!(second.statuses()[0].sequence(), 21);
        assert_eq!(second.statuses()[1].sequence(), 22);
        assert!(
            second
                .statuses()
                .iter()
                .all(|status| status.received_at().is_some())
        );
    }

    #[test]
    fn twcc_small_delta_preserves_the_unsigned_wire_range() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(7, now + Duration::from_millis(50), 9);
        let bytes = receiver
            .build_feedback(now + Duration::from_millis(100), 7)
            .expect("due TWCC feedback")
            .to_vec();
        let parsed = parse_twcc_packet(&bytes).expect("locally generated TWCC is valid");

        assert_eq!(
            parsed.statuses()[0].received_at(),
            Some(Duration::from_millis(50))
        );
        assert_ne!(bytes[0] & 0x20, 0, "TWCC declares its RTCP padding");
        assert_eq!(bytes.last().copied(), Some(1));
        assert_eq!(
            usize::from(u16::from_be_bytes([bytes[2], bytes[3]])).saturating_add(1) * 4,
            bytes.len()
        );
    }

    #[test]
    fn twcc_receiver_extends_transport_sequence_wraparound() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(u16::MAX, now + Duration::from_millis(1), 9);
        receiver.observe(0, now + Duration::from_millis(2), 9);
        let bytes = receiver
            .build_feedback(now + Duration::from_millis(60), 7)
            .expect("due TWCC feedback")
            .to_vec();
        let parsed = parse_twcc_packet(&bytes).expect("wraparound feedback is valid");

        assert_eq!(parsed.statuses().len(), 2);
        assert_eq!(parsed.statuses()[0].sequence(), u16::MAX);
        assert_eq!(parsed.statuses()[1].sequence(), 0);
        assert!(
            parsed
                .statuses()
                .iter()
                .all(|status| status.received_at().is_some())
        );
    }

    #[test]
    fn twcc_receiver_keeps_one_media_ssrc_across_transport_wide_reports() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(1, now, 11);
        receiver.observe(2, now + Duration::from_millis(1), 22);
        let first = receiver
            .build_feedback(now + Duration::from_millis(50), 7)
            .expect("first mixed-SSRC feedback")
            .to_vec();
        receiver.observe(3, now + Duration::from_millis(60), 22);
        let second = receiver
            .build_feedback(now + Duration::from_millis(110), 7)
            .expect("second mixed-SSRC feedback")
            .to_vec();

        assert_eq!(&first[8..12], &11u32.to_be_bytes());
        assert_eq!(&second[8..12], &11u32.to_be_bytes());
    }

    #[test]
    fn twcc_wraparound_survives_reordering_and_a_lost_sequence() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(u16::MAX - 1, now + Duration::from_millis(1), 9);
        receiver.observe(0, now + Duration::from_millis(3), 9);
        receiver.observe(u16::MAX, now + Duration::from_millis(2), 9);
        receiver.observe(2, now + Duration::from_millis(5), 9);
        let bytes = receiver
            .build_feedback(now + Duration::from_millis(60), 7)
            .expect("impaired wraparound feedback")
            .to_vec();
        let parsed = parse_twcc_packet(&bytes).expect("wraparound feedback is valid");
        let statuses = parsed.statuses();

        assert_eq!(statuses.len(), 5);
        assert_eq!(
            statuses
                .iter()
                .map(|status| status.sequence())
                .collect::<Vec<_>>(),
            vec![u16::MAX - 1, u16::MAX, 0, 1, 2]
        );
        assert!(statuses[0].received_at().is_some());
        assert!(statuses[1].received_at().is_some());
        assert!(statuses[2].received_at().is_some());
        assert!(statuses[3].received_at().is_none());
        assert!(statuses[4].received_at().is_some());
    }

    #[test]
    fn twcc_parser_rejects_impossible_status_ranges() {
        let mut bytes = [0u8; 20];
        bytes[0] = 0x8f;
        bytes[1] = TWCC_PACKET_TYPE;
        bytes[14..16].copy_from_slice(&u16::MAX.to_be_bytes());

        assert_eq!(parse_twcc_packet(&bytes), Err(GccError::MalformedTwcc));
    }

    #[test]
    fn gcc_history_eviction_makes_stale_feedback_inert() {
        let now = Instant::now();
        let mut gcc = Gcc::new(2);
        let stale = gcc.assign(SendId::new(1), 1_200).expect("stale send");
        gcc.record_departure(stale.send_id(), now)
            .expect("stale departure");
        for index in 2..=3 {
            let send = gcc.assign(SendId::new(index), 1_200).expect("new send");
            gcc.record_departure(send.send_id(), now + Duration::from_millis(index))
                .expect("new departure");
        }
        let before = gcc.estimate(now).bitrate_bps();

        let outcome = gcc.process_feedback(
            now + Duration::from_millis(10),
            &feedback(&[(stale.transport_sequence(), Some(Duration::from_millis(1)))]),
        );

        assert_eq!(outcome.acknowledged(), 0);
        assert_eq!(outcome.lost(), 0);
        assert_eq!(outcome.estimate().bitrate_bps(), before);
    }

    #[test]
    fn gcc_handles_capacity_change_reordered_and_unrecorded_feedback() {
        let now = Instant::now();
        let mut gcc = Gcc::new(64);
        let first = gcc.assign(SendId::new(1), 1200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1200).expect("second send");
        let pending = gcc.assign(SendId::new(3), 1200).expect("pending send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");
        let increased = gcc.process_feedback(
            now + Duration::from_millis(20),
            &feedback(&[
                (second.transport_sequence(), Some(Duration::from_millis(11))),
                (first.transport_sequence(), Some(Duration::from_millis(1))),
                (
                    pending.transport_sequence(),
                    Some(Duration::from_millis(12)),
                ),
                (u16::MAX, Some(Duration::from_millis(12))),
            ]),
        );
        let mut delayed_statuses = Vec::new();
        for index in 0..25u64 {
            let send = gcc
                .assign(SendId::new(index + 4), 1200)
                .expect("unique send identity");
            gcc.record_departure(
                send.send_id(),
                now + Duration::from_millis(30 + index.saturating_mul(6)),
            )
            .expect("authoritative departure");
            delayed_statuses.push((
                send.transport_sequence(),
                (index % 2 == 0).then_some(Duration::from_millis(30 + index.saturating_mul(15))),
            ));
        }
        let delayed = gcc.process_feedback(
            now + Duration::from_millis(500),
            &feedback(&delayed_statuses),
        );

        assert_eq!(increased.acknowledged(), 2);
        assert!(increased.probe().is_some());
        assert_eq!(delayed.lost(), 12);
        assert!(delayed.estimate().bitrate_bps() <= MAX_BITRATE_BPS);
    }

    #[test]
    fn gcc_network_simulation_replays_identically() {
        fn run() -> test_utils::SimulationSummary {
            let mut simulation = GccSimulation::new(1_500_000)
                .with_loss_every(17)
                .with_feedback_impairment(FeedbackImpairment {
                    drop_every: Some(7),
                    reverse_every: Some(3),
                    duplicate_every: Some(5),
                });
            simulation.run_for(Duration::from_secs(20), 2_000_000);
            simulation.summary().clone()
        }

        assert_eq!(run(), run());
    }

    #[test]
    fn gcc_probes_and_converges_without_persistent_queue() {
        let capacity = 2_000_000;
        let mut simulation = GccSimulation::new(capacity);

        simulation.run_for(Duration::from_secs(20), capacity.saturating_mul(2));
        let summary = simulation.summary();

        assert!(summary.acknowledged > 100);
        assert!(summary.probes.len() >= 2);
        assert!(summary.last_estimate() >= capacity.saturating_mul(70) / 100);
        assert!(
            summary.last_estimate() <= capacity.saturating_mul(125) / 100,
            "estimate overshot a steady link: {summary:?}"
        );
        assert!(summary.max_queue_delay <= Duration::from_millis(100));
    }

    #[test]
    fn gcc_capacity_drop_reduces_then_recovers_the_estimate() {
        let mut simulation = GccSimulation::new(2_000_000);
        simulation.run_for(Duration::from_secs(15), 5_000_000);
        let before_drop = simulation.summary().last_estimate();
        let drop_started = simulation
            .summary()
            .estimates
            .last()
            .map_or(Duration::ZERO, |(at, _)| *at);

        simulation.set_capacity(500_000);
        simulation.run_for(Duration::from_secs(12), 5_000_000);
        let reduced = simulation.summary().minimum_estimate_since(drop_started);
        let recovery_started = simulation
            .summary()
            .estimates
            .last()
            .map_or(Duration::ZERO, |(at, _)| *at);
        simulation.set_capacity(2_000_000);
        simulation.run_for(Duration::from_secs(20), 5_000_000);
        let recovered = simulation
            .summary()
            .maximum_estimate_since(recovery_started);

        assert!(
            reduced < before_drop.saturating_mul(80) / 100,
            "estimate did not respond to capacity drop: before={before_drop}, reduced={reduced}, final={}",
            simulation.summary().last_estimate()
        );
        assert!(recovered > reduced.saturating_mul(3) / 2);
        assert!(simulation.summary().last_estimate() >= 1_000_000);
    }

    #[test]
    fn gcc_survives_loss_and_feedback_impairment() {
        let mut simulation = GccSimulation::new(1_500_000)
            .with_loss_every(10)
            .with_feedback_impairment(FeedbackImpairment {
                drop_every: Some(5),
                reverse_every: Some(2),
                duplicate_every: Some(3),
            });

        simulation.run_for(Duration::from_secs(30), 2_000_000);
        let summary = simulation.summary();

        assert!(summary.acknowledged > 100);
        assert!(summary.lost > 0);
        assert!(summary.dropped_feedback > 0);
        assert!(summary.duplicate_feedback > 0);
        assert!(summary.last_estimate() >= MIN_BITRATE_BPS);
        assert!(summary.last_estimate() <= 1_500_000 * 3 / 2);
    }

    #[test]
    fn gcc_feedback_outage_decays_and_recovers() {
        let mut simulation = GccSimulation::new(2_000_000);
        simulation.run_for(Duration::from_secs(12), 5_000_000);
        let before = simulation.summary().last_estimate();

        simulation.run_feedback_outage_for(Duration::from_secs(7), 5_000_000);
        let during = simulation.summary().last_estimate();
        simulation.run_for(Duration::from_secs(20), 5_000_000);
        let after = simulation.summary().last_estimate();

        assert!(
            during < before,
            "feedback outage did not decay estimate: before={before}, during={during}, after={after}"
        );
        assert!(after > during);
        assert!(after >= 1_000_000);
    }

    #[test]
    fn gcc_application_limited_flow_uses_periodic_probing() {
        let mut simulation = GccSimulation::new(2_000_000);
        simulation.run_for(Duration::from_secs(12), 2_000_000);
        let probes_before_alr = simulation.summary().probes.len();

        simulation.run_for(Duration::from_secs(12), 100_000);

        assert!(simulation.summary().probes.len() > probes_before_alr);
        assert!(simulation.summary().last_estimate() >= 100_000);
    }
}
