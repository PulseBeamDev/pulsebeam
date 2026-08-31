//! Overflow is explicit here, and denied workspace-wide.
//!
//! `overflow-checks` is off in release, so a bare `+` or `-` that goes out of
//! range does not stop — it yields a plausible-looking number that the pacer,
//! the allocator or the jitter estimator then treats as a measurement. This is
//! timestamp and sequence arithmetic, where that number is the whole output, so
//! every operation says which behaviour it wants: `saturating_` to clamp,
//! `checked_` to fall back, `wrapping_` where an era boundary makes wrapping
//! the correct answer.

use crate::clock::NtpTime;
use crate::rtp::{Frequency, MediaTime, PacketForwardingState, SenderReport as SenderInfo, Ssrc};
use ahash::HashMap;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

const MIN_SR_UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const MAX_DRIFT_PPM: f64 = 50_000.0;
const MAX_RTP_GAP_SECS: f64 = 10.0;
const MIN_GUARDED_SR_INTERVAL: Duration = Duration::from_millis(200);
const MAX_GUARDED_SR_INTERVAL: Duration = Duration::from_secs(10);
const GUARDED_RATE_DRIFT_PPM: u128 = 50_000;
const PPM_SCALE: u128 = 1_000_000;
const GUARDED_MAX_FORWARD_GAP_SECS: u64 = 10;
const GUARDED_MAX_BACKWARD_GAP_SECS: u64 = 2;
const GUARDED_CORRECTION_SLEW_PPM: u128 = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochTransition {
    generation: u64,
}

impl EpochTransition {
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketMapping {
    playout_time: SystemTime,
    epoch_transition: Option<EpochTransition>,
}

impl PacketMapping {
    pub const fn playout_time(self) -> SystemTime {
        self.playout_time
    }

    pub const fn epoch_transition(self) -> Option<EpochTransition> {
        self.epoch_transition
    }
}

#[derive(Clone, Copy, Debug)]
struct GuardedSenderSample {
    rtp: u32,
    ntp: NtpTime,
    arrival: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GuardedAnchor {
    ntp: NtpTime,
    cluster_time: SystemTime,
}

#[derive(Clone, Copy, Debug)]
struct DiscontinuityCandidate {
    current: GuardedSenderSample,
    count: u8,
}

#[derive(Debug)]
struct GuardedMapper {
    clock_rate: Frequency,
    latest: Option<GuardedSenderSample>,
    rate_ppm: i64,
    provisional: Option<(u32, SystemTime)>,
    last_assignment: Option<SystemTime>,
    anchor: Option<GuardedAnchor>,
    minimum_delay_anchor: Option<GuardedAnchor>,
    applied_correction_ns: i128,
    target_correction_ns: i128,
    last_slew_time: Option<SystemTime>,
    candidate: Option<DiscontinuityCandidate>,
    pending_epoch: Option<GuardedSenderSample>,
    epoch_generation: u64,
}

impl GuardedMapper {
    fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            latest: None,
            rate_ppm: 0,
            provisional: None,
            last_assignment: None,
            anchor: None,
            minimum_delay_anchor: None,
            applied_correction_ns: 0,
            target_correction_ns: 0,
            last_slew_time: None,
            candidate: None,
            pending_epoch: None,
            epoch_generation: 0,
        }
    }

    fn observe(&mut self, report: pulsebeam_rtc::SenderReport, arrival: SystemTime) {
        let current = GuardedSenderSample {
            rtp: report.rtp_timestamp(),
            ntp: NtpTime::from_raw(report.ntp_timestamp()),
            arrival,
        };

        let Some(previous) = self.latest else {
            self.latest = Some(current);
            self.provisional = Some((current.rtp, arrival));
            self.install_initial_anchor(current);
            return;
        };

        let comparison = self
            .candidate
            .map_or(previous, |candidate| candidate.current);

        let Some(arrival_delta) = arrival.duration_since(comparison.arrival).ok() else {
            return;
        };
        if !(MIN_GUARDED_SR_INTERVAL..=MAX_GUARDED_SR_INTERVAL).contains(&arrival_delta) {
            return;
        }

        let rtp_delta = i64::from(current.rtp.wrapping_sub(comparison.rtp).cast_signed());
        let ntp_delta = current.ntp.saturating_duration_since(comparison.ntp);
        if current.ntp.units_since(comparison.ntp) <= 0 || rtp_delta <= 0 {
            self.candidate = None;
            return;
        }

        let client_rate_is_plausible = self.rate_is_plausible(rtp_delta.unsigned_abs(), ntp_delta);
        if self.candidate.is_none() && client_rate_is_plausible {
            self.rate_ppm = rate_ppm(self.clock_rate, rtp_delta.unsigned_abs(), ntp_delta);
            self.latest = Some(current);
            self.provisional = None;
            self.update_minimum_delay(current);
            return;
        }

        if self.candidate.is_some() && client_rate_is_plausible {
            let count = self
                .candidate
                .map_or(1, |candidate| candidate.count.saturating_add(1));
            self.candidate = Some(DiscontinuityCandidate { current, count });
            if count >= 3 {
                self.pending_epoch = Some(current);
            }
        } else if self.candidate.is_none() {
            self.candidate = Some(DiscontinuityCandidate { current, count: 1 });
        }
    }

    fn install_initial_anchor(&mut self, sample: GuardedSenderSample) {
        self.anchor = Some(GuardedAnchor {
            ntp: sample.ntp,
            cluster_time: sample.arrival,
        });
        self.minimum_delay_anchor = self.anchor;
        self.last_slew_time = Some(sample.arrival);
    }

    fn update_minimum_delay(&mut self, sample: GuardedSenderSample) {
        let Some(anchor) = self.anchor else {
            self.install_initial_anchor(sample);
            return;
        };
        let raw = project_ntp(anchor, sample.ntp, 0);
        let offset = signed_nanos(sample.arrival, raw);
        if offset < self.target_correction_ns {
            self.target_correction_ns = offset;
            self.minimum_delay_anchor = Some(GuardedAnchor {
                ntp: sample.ntp,
                cluster_time: sample.arrival,
            });
        }
        debug_assert_ne!(self.target_correction_ns, i128::MIN);
    }

    fn slew(&mut self, now: SystemTime) {
        let Some(last) = self.last_slew_time else {
            self.last_slew_time = Some(now);
            return;
        };
        let Some(elapsed) = now.duration_since(last).ok() else {
            return;
        };
        let max_step = i128::try_from(
            elapsed
                .as_nanos()
                .saturating_mul(GUARDED_CORRECTION_SLEW_PPM)
                .saturating_div(PPM_SCALE),
        )
        .unwrap_or(i128::MAX);
        let delta = self
            .target_correction_ns
            .saturating_sub(self.applied_correction_ns);
        let step = delta.clamp(-max_step, max_step);
        self.applied_correction_ns = self.applied_correction_ns.saturating_add(step);
        self.last_slew_time = Some(now);
        debug_assert_ne!(self.applied_correction_ns, i128::MIN);
        debug_assert_ne!(self.target_correction_ns, i128::MIN);
    }

    fn anchor(&self) -> Option<GuardedAnchor> {
        self.minimum_delay_anchor
    }

    fn adopt_anchor(&mut self, anchor: GuardedAnchor) {
        if self.anchor == Some(anchor) {
            return;
        }
        let Some(old_anchor) = self.anchor else {
            self.anchor = Some(anchor);
            self.minimum_delay_anchor = Some(anchor);
            self.applied_correction_ns = 0;
            self.target_correction_ns = 0;
            self.last_slew_time = Some(anchor.cluster_time);
            return;
        };
        if self.last_assignment.is_none() {
            self.anchor = Some(anchor);
            self.minimum_delay_anchor = Some(anchor);
            self.applied_correction_ns = 0;
            self.target_correction_ns = 0;
            self.last_slew_time = Some(anchor.cluster_time);
            return;
        }
        let Some(reference) = self.latest else {
            self.anchor = Some(anchor);
            self.minimum_delay_anchor = Some(anchor);
            return;
        };
        let old = project_ntp(old_anchor, reference.ntp, self.applied_correction_ns);
        let raw = project_ntp(anchor, reference.ntp, 0);
        self.anchor = Some(anchor);
        self.minimum_delay_anchor = Some(anchor);
        self.applied_correction_ns = signed_nanos(old, raw);
        self.target_correction_ns = 0;
        debug_assert_ne!(self.applied_correction_ns, i128::MIN);
    }

    fn rate_is_plausible(&self, rtp_delta: u64, ntp_delta: Duration) -> bool {
        let expected = u128::from(self.clock_rate.get())
            .saturating_mul(ntp_delta.as_nanos())
            .saturating_div(1_000_000_000);
        if expected == 0 {
            return false;
        }
        let lower = expected.saturating_mul(PPM_SCALE.saturating_sub(GUARDED_RATE_DRIFT_PPM));
        let upper = expected.saturating_mul(PPM_SCALE.saturating_add(GUARDED_RATE_DRIFT_PPM));
        let measured = u128::from(rtp_delta).saturating_mul(PPM_SCALE);
        measured >= lower && measured <= upper
    }

    fn map(&mut self, rtp: MediaTime, arrival: SystemTime) -> PacketMapping {
        debug_assert_eq!(rtp.frequency(), self.clock_rate);
        let rtp = {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "RTP packet timestamps are the low 32-bit sender clock"
            )]
            {
                rtp.numer() as u32
            }
        };
        let mut transition = None;
        if let Some(epoch) = self.pending_epoch.take() {
            self.latest = Some(epoch);
            self.rate_ppm = 0;
            self.candidate = None;
            self.provisional = None;
            self.anchor = Some(GuardedAnchor {
                ntp: epoch.ntp,
                cluster_time: epoch.arrival,
            });
            self.minimum_delay_anchor = self.anchor;
            self.applied_correction_ns = 0;
            self.target_correction_ns = 0;
            self.last_slew_time = Some(epoch.arrival);
            self.epoch_generation = self.epoch_generation.saturating_add(1);
            transition = Some(EpochTransition {
                generation: self.epoch_generation,
            });
        }

        self.slew(arrival);
        let max_forward = i64::from(
            self.clock_rate
                .get()
                .saturating_mul(u32::try_from(GUARDED_MAX_FORWARD_GAP_SECS).unwrap_or(u32::MAX)),
        );
        let max_backward = i64::from(
            self.clock_rate
                .get()
                .saturating_mul(u32::try_from(GUARDED_MAX_BACKWARD_GAP_SECS).unwrap_or(u32::MAX)),
        );
        debug_assert!(max_forward > 0);
        debug_assert!(max_backward > 0);
        let mut mapped = if let Some(reference) = self.latest {
            let delta = i64::from(rtp.wrapping_sub(reference.rtp).cast_signed());
            if delta > max_forward || delta < -max_backward {
                arrival
            } else if let Some(anchor) = self.anchor {
                let ntp_delta = rtp_duration(delta.unsigned_abs(), self.clock_rate, self.rate_ppm);
                let packet_ntp = if delta.is_positive() {
                    reference.ntp.wrapping_add(ntp_delta)
                } else {
                    reference.ntp.wrapping_sub(ntp_delta)
                };
                project_ntp(anchor, packet_ntp, self.applied_correction_ns)
            } else {
                arrival
            }
        } else if let Some((reference_rtp, reference_time)) = self.provisional {
            let delta = i64::from(rtp.wrapping_sub(reference_rtp).cast_signed());
            if delta > max_forward || delta < -max_backward {
                arrival
            } else {
                shift_system_time(reference_time, delta, self.clock_rate, 0)
            }
        } else {
            self.provisional = Some((rtp, arrival));
            arrival
        };

        if mapped < arrival {
            mapped = arrival;
        }
        debug_assert!(mapped >= arrival);
        if let Some(last) = self.last_assignment {
            if mapped < last {
                mapped = last;
            }
            debug_assert!(mapped >= last);
        }
        self.last_assignment = Some(mapped);
        PacketMapping {
            playout_time: mapped,
            epoch_transition: transition,
        }
    }
}

fn signed_nanos(later: SystemTime, earlier: SystemTime) -> i128 {
    match later.duration_since(earlier) {
        Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
        Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

fn duration_from_nanos(nanos: i128) -> Duration {
    let magnitude = u128::try_from(nanos.unsigned_abs()).unwrap_or(u128::MAX);
    Duration::from_nanos(u64::try_from(magnitude).unwrap_or(u64::MAX))
}

fn project_ntp(anchor: GuardedAnchor, ntp: NtpTime, correction_ns: i128) -> SystemTime {
    let delta = ntp.duration_since(anchor.ntp);
    let projected = match delta {
        Ok(duration) => anchor.cluster_time.checked_add(duration),
        Err(duration) => anchor.cluster_time.checked_sub(duration),
    }
    .unwrap_or(anchor.cluster_time);
    if correction_ns.is_negative() {
        projected
            .checked_sub(duration_from_nanos(correction_ns))
            .unwrap_or(projected)
    } else {
        projected
            .checked_add(duration_from_nanos(correction_ns))
            .unwrap_or(projected)
    }
}

fn rtp_duration(rtp_delta: u64, clock_rate: Frequency, rate_ppm: i64) -> Duration {
    let denominator = u128::from(clock_rate.get())
        .saturating_mul(u128::try_from(rate_ppm.saturating_add(1_000_000)).unwrap_or(1));
    let nanos = u128::from(rtp_delta)
        .saturating_mul(1_000_000_000)
        .saturating_mul(PPM_SCALE)
        .checked_div(denominator.max(1))
        .unwrap_or(u128::MAX);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

impl GuardedAnchor {
    fn lower_delay(self, other: Self) -> Self {
        match other.ntp.duration_since(self.ntp) {
            Ok(delta) => {
                if other.cluster_time
                    < self
                        .cluster_time
                        .checked_add(delta)
                        .unwrap_or(self.cluster_time)
                {
                    other
                } else {
                    self
                }
            }
            Err(delta) => {
                if other
                    .cluster_time
                    .checked_add(delta)
                    .unwrap_or(other.cluster_time)
                    < self.cluster_time
                {
                    other
                } else {
                    self
                }
            }
        }
    }
}

fn rate_ppm(clock_rate: Frequency, rtp_delta: u64, ntp_delta: Duration) -> i64 {
    let expected = u128::from(clock_rate.get())
        .saturating_mul(ntp_delta.as_nanos())
        .saturating_div(1_000_000_000);
    if expected == 0 {
        return 0;
    }
    let measured = u128::from(rtp_delta);
    let signed = if measured >= expected {
        i128::try_from(measured.saturating_sub(expected)).unwrap_or(i128::MAX)
    } else {
        -i128::try_from(expected.saturating_sub(measured)).unwrap_or(i128::MAX)
    };
    let value = signed
        .saturating_mul(i128::try_from(PPM_SCALE).unwrap_or(i128::MAX))
        .checked_div(i128::try_from(expected).unwrap_or(1))
        .unwrap_or(0);
    i64::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

fn shift_system_time(
    base: SystemTime,
    rtp_delta: i64,
    clock_rate: Frequency,
    rate_ppm: i64,
) -> SystemTime {
    if rtp_delta == 0 {
        return base;
    }
    let ppm = u128::try_from(rate_ppm.saturating_add(1_000_000)).unwrap_or(1);
    let nanos = u128::from(rtp_delta.unsigned_abs())
        .saturating_mul(1_000_000_000)
        .saturating_mul(PPM_SCALE)
        .checked_div(u128::from(clock_rate.get()).saturating_mul(ppm))
        .unwrap_or(u128::MAX);
    let nanos = u64::try_from(nanos).unwrap_or(u64::MAX);
    let duration = Duration::from_nanos(nanos);
    if rtp_delta.is_positive() {
        base.checked_add(duration).unwrap_or(base)
    } else {
        base.checked_sub(duration).unwrap_or(base)
    }
}

#[derive(Debug, Clone, Copy)]
struct ClockReference {
    rtp_time: MediaTime,
    ntp_time: NtpTime,
    arrival_ts: Instant,
}

/// Shift an NTP wall time by a signed number of seconds.
///
/// Saturating rather than trapping: `ntp_delta_secs` comes from a sender
/// report's RTP delta, which a misbehaving or malicious publisher controls. A
/// clamped timestamp produces a bad playout estimate that the rest of the
/// pipeline already bounds; an overflow would end the process.
fn offset_ntp(base: NtpTime, delta_secs: f64) -> NtpTime {
    if !delta_secs.is_finite() {
        return base;
    }
    let magnitude = Duration::from_secs_f64(delta_secs.abs());
    if delta_secs >= 0.0 {
        base.saturating_add(magnitude)
    } else {
        base.saturating_sub(magnitude)
    }
}

impl ClockReference {
    fn server_time_at_anchor(&self, ntp_delta: Duration) -> Instant {
        self.arrival_ts
            .checked_add(ntp_delta)
            .unwrap_or(self.arrival_ts)
    }

    /// Of two anchors on the same NTP wall clock, the one implying the lower
    /// propagation delay — i.e. whose server arrival is earliest for its NTP
    /// instant. Only `ntp_time`/`arrival_ts` matter here (never `rtp_time`), so
    /// this is meaningful across sibling encodings with unrelated RTP bases.
    fn lower_delay(self, other: Self) -> Self {
        match other.ntp_time.duration_since(self.ntp_time) {
            // other is later on the NTP clock: does it arrive sooner than self predicts?
            Ok(dt) => {
                if other.arrival_ts < self.arrival_ts.checked_add(dt).unwrap_or(self.arrival_ts) {
                    other
                } else {
                    self
                }
            }
            // other is earlier on the NTP clock: compare the other way round.
            Err(e) => {
                if other.arrival_ts.checked_add(e).unwrap_or(other.arrival_ts) < self.arrival_ts {
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
    guarded: GuardedMapper,
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
            guarded: GuardedMapper::new(clock_rate),
            first_sr: None,
            latest_sr: None,
            last_sr_time: None,
            base_rtp: None,
            base_server_time: None,
            estimated_clock_drift_ppm: 0.0,
            ntp_anchor: None,
        }
    }

    pub fn observe_sender_report(
        &mut self,
        report: pulsebeam_rtc::SenderReport,
        arrival: SystemTime,
    ) {
        self.guarded.observe(report, arrival);
    }

    pub fn map_packet(&mut self, rtp: MediaTime, arrival: SystemTime) -> PacketMapping {
        self.guarded.map(rtp, arrival)
    }

    fn guarded_anchor(&self) -> Option<GuardedAnchor> {
        self.guarded.anchor()
    }

    fn adopt_guarded_anchor(&mut self, anchor: GuardedAnchor) {
        self.guarded.adopt_anchor(anchor);
    }

    pub fn process(&mut self, packet: &mut PacketForwardingState, sr: Option<SenderInfo>) {
        if let Some(sr) = sr {
            self.add_sender_report(sr, packet.arrival_ts);
        }

        // The two move together, so read them together: a baseline half-set is
        // a bug rather than a state to recover from.
        let (base_rtp, mut base_server_time) = match (self.base_rtp, self.base_server_time) {
            (Some(rtp), Some(server_time)) => (rtp, server_time),
            _ => self.reset_baseline(packet.rtp_ts, packet.arrival_ts),
        };

        let rtp_delta = packet
            .rtp_ts
            .numer()
            .cast_signed()
            .wrapping_sub(base_rtp.numer().cast_signed());
        let max_ticks =
            crate::bitrate::saturating_bps(MAX_RTP_GAP_SECS * f64::from(self.clock_rate.get()))
                .cast_signed();

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
            let rtp_delta = packet
                .rtp_ts
                .numer()
                .cast_signed()
                .wrapping_sub(latest.rtp_time.numer().cast_signed());
            let ntp_delta_secs = rtp_delta as f64 / self.clock_rate.get() as f64 * drift_correction;
            let ntp_pkt = offset_ntp(latest.ntp_time, ntp_delta_secs);

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
            base_server_time
                .checked_add(Duration::from_secs_f64(seconds_delta))
                .unwrap_or(base_server_time)
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

    fn reset_baseline(&mut self, rtp_ts: MediaTime, arrival_ts: Instant) -> (MediaTime, Instant) {
        self.base_rtp = Some(rtp_ts);
        self.base_server_time = Some(arrival_ts);
        self.first_sr = None;
        self.latest_sr = None;
        self.ntp_anchor = None;
        self.estimated_clock_drift_ppm = 0.0;
        (rtp_ts, arrival_ts)
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
                || current
                    .rtp_time
                    .numer()
                    .wrapping_sub(last.rtp_time.numer())
                    .cast_signed()
                    <= 0
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
            let expected_server = anchor
                .arrival_ts
                .checked_add(ntp_delta)
                .unwrap_or(anchor.arrival_ts);
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
            .wrapping_sub(first.rtp_time.numer())
            .cast_signed();
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
    guarded_shared_anchor: Option<GuardedAnchor>,
}

impl TrackSynchronizer {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            streams: HashMap::default(),
            shared_anchor: None,
            guarded_shared_anchor: None,
        }
    }

    /// Stamp `packet.playout_time` on the track's shared clock, routing it to its
    /// encoding's synchronizer and reconciling the connection anchor.
    pub fn process(
        &mut self,
        packet: &mut PacketForwardingState,
        ssrc: Ssrc,
        sr: Option<SenderInfo>,
    ) {
        let clock_rate = self.clock_rate;
        let shared = self.shared_anchor;

        let sync = self
            .streams
            .entry(ssrc)
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

    pub fn observe_sender_report(
        &mut self,
        report: pulsebeam_rtc::SenderReport,
        arrival: SystemTime,
    ) {
        let ssrc = Ssrc::from(report.ssrc());
        let anchor = {
            let sync = self
                .streams
                .entry(ssrc)
                .or_insert_with(|| Synchronizer::new(self.clock_rate));
            sync.observe_sender_report(report, arrival);
            sync.guarded_anchor()
        };
        if let Some(anchor) = anchor {
            self.guarded_shared_anchor = Some(match self.guarded_shared_anchor {
                Some(current) => current.lower_delay(anchor),
                None => anchor,
            });
        }
        self.reconcile_guarded_anchor();
    }

    pub fn map_packet(&mut self, ssrc: Ssrc, rtp: MediaTime, arrival: SystemTime) -> PacketMapping {
        self.reconcile_guarded_anchor();
        let sync = self
            .streams
            .entry(ssrc)
            .or_insert_with(|| Synchronizer::new(self.clock_rate));
        sync.map_packet(rtp, arrival)
    }

    fn reconcile_guarded_anchor(&mut self) {
        let Some(anchor) = self.guarded_shared_anchor else {
            return;
        };
        for sync in self.streams.values_mut() {
            sync.adopt_guarded_anchor(anchor);
        }
    }

    pub fn is_synchronized(&self) -> bool {
        self.streams.values().any(Synchronizer::is_synchronized)
    }
}

#[cfg(test)]
mod tests {
    use crate::rtp::RtpPacket;
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::rtp::VIDEO_FREQUENCY;
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

        assert_eq!(
            crate::bitrate::saturating_bps(sync_perfect.estimated_clock_drift_ppm.round())
                .cast_signed(),
            0
        );
        assert_eq!(
            crate::bitrate::saturating_bps(sync_drifting.estimated_clock_drift_ppm.round())
                .cast_signed(),
            1000
        );

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
            track.process(
                &mut lo,
                LOW.into(),
                Some(sr(LOW, LOW_RTP_BASE + i * 90_000, ntp)),
            );
            let mut hi = frame(HIGH, HIGH_RTP_BASE + i * 90_000, t + HIGH_DELAY);
            track.process(
                &mut hi,
                HIGH.into(),
                Some(sr(HIGH, HIGH_RTP_BASE + i * 90_000, ntp)),
            );
        }

        // A frame from each encoding at the same wall-clock instant (10s in). Both
        // map their own RTP base to that instant; on one shared clock they resolve
        // to the same playout time. Independent clocks would strand the high layer
        // `HIGH_DELAY` behind — the exact seam step a switch must never introduce.
        let event = base + Duration::from_secs(10);
        let mut lo = frame(LOW, LOW_RTP_BASE + 10 * 90_000, event);
        track.process(&mut lo, LOW.into(), None);
        let mut hi = frame(HIGH, HIGH_RTP_BASE + 10 * 90_000, event + HIGH_DELAY);
        track.process(&mut hi, HIGH.into(), None);

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

#[cfg(test)]
mod hostile_clock_contract_tests {
    use super::*;
    use crate::rtp::VIDEO_FREQUENCY;
    use pulsebeam_rtc::SenderReport;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

    fn sender_report(ssrc: u32, rtp: u32, ntp_secs: u64) -> SenderReport {
        sender_report_with_ntp(ssrc, rtp, ntp_secs << 32)
    }

    fn sender_report_with_ntp(ssrc: u32, rtp: u32, ntp_timestamp: u64) -> SenderReport {
        SenderReport::new(
            ssrc,
            (NTP_UNIX_OFFSET_SECS << 32) + ntp_timestamp,
            rtp,
            0,
            0,
        )
    }

    fn wall(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + seconds)
    }

    fn wall_ms(seconds: u64, millis: u64) -> SystemTime {
        wall(seconds) + Duration::from_millis(millis)
    }

    fn as_system_time(value: SystemTime) -> SystemTime {
        value
    }

    #[test]
    fn no_sender_report_uses_a_conservative_ingress_anchor() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        let first = sync.map_packet(MediaTime::from_90khz(10_000), wall(1));
        let later = sync.map_packet(MediaTime::from_90khz(100_000), wall(3));

        assert!(first.playout_time() >= wall(1));
        assert!(later.playout_time() >= wall(3));
        assert!(first.epoch_transition().is_none());
    }

    #[test]
    fn sender_report_cadence_does_not_depend_on_client_absolute_wall_origin() {
        let mut left = Synchronizer::new(VIDEO_FREQUENCY);
        let mut right = Synchronizer::new(VIDEO_FREQUENCY);
        let _ = left.map_packet(MediaTime::from_90khz(0), wall(0));
        let _ = right.map_packet(MediaTime::from_90khz(0), wall(0));

        for step in 0..4u32 {
            let rtp = (step + 1) * 90_090;
            let arrival = wall(u64::from(step + 1));
            left.observe_sender_report(sender_report(1, rtp, 10_000 + u64::from(step)), arrival);
            right.observe_sender_report(sender_report(2, rtp, 40_000 + u64::from(step)), arrival);
        }

        let left_mapping = left.map_packet(MediaTime::from_90khz(450_450), wall(6));
        let right_mapping = right.map_packet(MediaTime::from_90khz(450_450), wall(6));
        assert_eq!(left_mapping.playout_time(), right_mapping.playout_time());
    }

    #[test]
    fn sender_rate_uses_rtp_ntp_cadence_when_arrival_delay_changes() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        sync.observe_sender_report(sender_report(1, 0, 10_000), wall_ms(1, 20));
        sync.observe_sender_report(sender_report(1, 90_090, 10_001), wall(2));

        let mapping = sync.map_packet(MediaTime::from_90khz(450_450), wall(5));
        let expected = wall(6);
        let error = mapping
            .playout_time()
            .duration_since(expected)
            .unwrap_or_else(|error| error.duration());
        assert!(
            error <= Duration::from_millis(2),
            "arrival jitter became clock drift: {error:?}, mapped={:?}",
            mapping.playout_time()
        );
    }

    #[test]
    fn stale_regressing_repeated_implausible_and_outlier_reports_are_inert() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        for step in 0..4u32 {
            let rtp = (step + 1) * 90_000;
            sync.observe_sender_report(
                sender_report(1, rtp, 20_000 + u64::from(step)),
                wall(u64::from(step + 1)),
            );
        }

        let historical = sync
            .map_packet(MediaTime::from_90khz(360_000), wall(5))
            .playout_time();
        let historical_saved = historical;
        let expected_future = sync
            .map_packet(MediaTime::from_90khz(540_000), wall(7))
            .playout_time();
        let attacks = [
            (270_000, 20_002),
            (360_000, 20_003),
            (450_000, 20_003),
            (900_000, 20_004),
            (180_000, 80_000),
        ];
        for (rtp, ntp_secs) in attacks {
            sync.observe_sender_report(sender_report(1, rtp, ntp_secs), wall(6));
        }

        let future = sync
            .map_packet(MediaTime::from_90khz(540_000), wall(7))
            .playout_time();
        assert!(future >= historical);
        assert_eq!(future, expected_future);
        assert_eq!(historical, historical_saved);

        let bounded = sync.map_packet(MediaTime::from_90khz(u64::from(u32::MAX)), wall(8));
        assert!(bounded.playout_time() <= wall(8) + Duration::from_secs(1));
    }

    #[test]
    fn small_sender_report_corrections_are_continuous_and_future_only() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        sync.observe_sender_report(sender_report(1, 0, 30_000), wall(1));
        sync.observe_sender_report(
            sender_report_with_ntp(1, 90_000, (31_000 << 32) + (1 << 22)),
            wall(2),
        );
        let historical = sync
            .map_packet(MediaTime::from_90khz(90_000), wall(2))
            .playout_time();

        sync.observe_sender_report(sender_report(1, 180_000, 32_000), wall(3));
        sync.observe_sender_report(sender_report(1, 270_000, 33_000), wall(4));
        let future_mapping = sync.map_packet(MediaTime::from_90khz(360_000), wall(5));
        let future = future_mapping.playout_time();

        assert!(future >= historical);
        assert!(future_mapping.epoch_transition().is_none());
        let elapsed = future.duration_since(historical).unwrap();
        assert!(elapsed >= Duration::from_millis(2_980));
        assert!(elapsed <= Duration::from_millis(3_020));
    }

    #[test]
    fn large_startup_delay_converges_without_revising_assignments() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        sync.observe_sender_report(sender_report(1, 0, 10_000), wall_ms(0, 500));

        let historical = sync
            .map_packet(MediaTime::from_90khz(0), wall_ms(0, 500))
            .playout_time();
        sync.observe_sender_report(sender_report(1, 90_000, 10_001), wall(1));
        let mut previous = sync
            .map_packet(MediaTime::from_90khz(90_000), wall(1))
            .playout_time();
        let max_correction_step_ms = 1_000u128
            .saturating_mul(GUARDED_CORRECTION_SLEW_PPM)
            .saturating_div(PPM_SCALE);
        let min_advance = Duration::from_millis(
            u64::try_from(1_000u128.saturating_sub(max_correction_step_ms)).unwrap_or(0),
        );

        for second in 2..=16u32 {
            sync.observe_sender_report(
                sender_report(1, second * 90_000, 10_000 + u64::from(second)),
                wall(u64::from(second)),
            );
            let current = sync
                .map_packet(
                    MediaTime::from_90khz(u64::from(second) * 90_000),
                    wall(u64::from(second)),
                )
                .playout_time();
            let advance = current.duration_since(previous).unwrap();
            assert!(advance >= min_advance);
            assert!(advance <= Duration::from_secs(1));
            previous = current;
        }

        assert!(previous >= historical);
        assert!(previous <= wall(16) + Duration::from_millis(20));
    }

    #[test]
    fn large_validated_discontinuity_returns_an_explicit_epoch_transition() {
        let mut sync = Synchronizer::new(VIDEO_FREQUENCY);
        sync.observe_sender_report(sender_report(1, 0, 50_000), wall(1));
        sync.observe_sender_report(sender_report(1, 90_000, 50_001), wall(2));
        let established = sync.map_packet(MediaTime::from_90khz(180_000), wall(3));
        assert!(established.epoch_transition().is_none());

        sync.observe_sender_report(sender_report(1, 2_000_000, 60_000), wall(4));
        let first_candidate = sync.map_packet(MediaTime::from_90khz(2_000_000), wall(4));
        assert!(first_candidate.epoch_transition().is_none());

        sync.observe_sender_report(sender_report(1, 2_090_000, 60_001), wall(5));
        let second_candidate = sync.map_packet(MediaTime::from_90khz(2_090_000), wall(5));
        assert!(second_candidate.epoch_transition().is_none());

        sync.observe_sender_report(sender_report(1, 2_180_000, 60_002), wall(6));
        let discontinuity = sync.map_packet(MediaTime::from_90khz(2_180_000), wall(6));
        assert!(discontinuity.epoch_transition().is_some());
    }

    #[test]
    fn sibling_encodings_align_with_independent_rtp_bases() {
        let mut track = TrackSynchronizer::new(VIDEO_FREQUENCY);
        for step in 0..3u32 {
            let low_arrival = wall_ms(u64::from(step + 1), 20);
            let high_arrival = wall_ms(u64::from(step + 1), 180);
            track.observe_sender_report(
                sender_report(10, 1_000_000 + step * 90_000, 60_000 + u64::from(step)),
                low_arrival,
            );
            track.observe_sender_report(
                sender_report(20, 7_000_000 + step * 90_000, 60_000 + u64::from(step)),
                high_arrival,
            );
        }

        let low = track.map_packet(10.into(), MediaTime::from_90khz(1_360_000), wall(5));
        let high = track.map_packet(20.into(), MediaTime::from_90khz(7_360_000), wall(5));
        let low_time = as_system_time(low.playout_time());
        let high_time = as_system_time(high.playout_time());
        let delta = low_time
            .duration_since(high_time)
            .unwrap_or_else(|error| error.duration());
        assert!(delta <= Duration::from_millis(10));
    }

    #[test]
    fn later_minimum_delay_observations_align_mapped_siblings() {
        let mut track = TrackSynchronizer::new(VIDEO_FREQUENCY);

        for step in 0..2u32 {
            let ntp = 10_000 + u64::from(step);
            let rtp = step * 90_000;
            track.observe_sender_report(sender_report(10, rtp, ntp), wall_ms(u64::from(step), 500));
            track.observe_sender_report(
                sender_report(20, rtp + 7_000_000, ntp),
                wall_ms(u64::from(step), 500),
            );
        }

        let initial_low =
            track.map_packet(10.into(), MediaTime::from_90khz(90_000), wall_ms(1, 500));
        let initial_high =
            track.map_packet(20.into(), MediaTime::from_90khz(7_090_000), wall_ms(1, 500));

        track.observe_sender_report(sender_report(10, 180_000, 10_002), wall_ms(2, 100));
        track.observe_sender_report(sender_report(20, 7_180_000, 10_002), wall_ms(2, 500));

        let event = wall_ms(2, 600);
        let low = track.map_packet(10.into(), MediaTime::from_90khz(270_000), event);
        let high = track.map_packet(20.into(), MediaTime::from_90khz(7_270_000), event);
        let delta = low
            .playout_time()
            .duration_since(high.playout_time())
            .unwrap_or_else(|error| error.duration());

        assert!(initial_low.playout_time() >= wall_ms(1, 500));
        assert!(initial_high.playout_time() >= wall_ms(1, 500));
        assert!(low.playout_time() >= initial_low.playout_time());
        assert!(high.playout_time() >= initial_high.playout_time());
        assert!(low.playout_time() > event);
        assert!(high.playout_time() > event);
        assert!(delta <= Duration::from_millis(1));
    }
}
