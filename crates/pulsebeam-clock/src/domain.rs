//! Caller-owned source domains and track projections. Binding and packet-lifetime
//! validation are external; NTP layout, a shared transport or a matching name
//! never establish domain membership.

use crate::{
    Error,
    time::{
        Binding, ClockRate, GlobalInstant, MonotonicTime, NS_PER_SECOND, RtpBinding, RtpTimestamp,
        SourceTime, narrow,
    },
};

mod affine;
mod calibration;
use affine::{DriftInterval, affine_scale};
mod timeline;
pub use calibration::{
    ActHint, ActiveExchange, CalibrationStatus, Evidence, ReferenceQuality, ReferenceSample,
};
use calibration::{Calibration, Measurement, SourceEstimate, drift_error};
use timeline::Timeline;

const MAX_TRACKS: u32 = 64;
const MAX_WINDOW_NS: u64 = 3_600_000_000_000;
const PPM: i128 = 1_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub track_capacity: u32,
    pub nominal_delay_ns: u64,
    pub max_past_ns: u64,
    pub max_future_ns: u64,
    pub observation_max_age_ns: u64,
    /// Physical frequency bound in each coordinate (RTP/source, source/global).
    pub max_drift_ppm: u32,
    pub max_exchange_rtt_ns: u64,
    pub min_fit_span_ns: u64,
    pub lock_uncertainty_ns: u64,
    /// Guaranteed look-back in nominal RTP time and in source time.
    pub reorder_ns: u64,
    pub history_capacity: usize,
    /// Maximum correction rate relative to the estimated affine slope.
    pub slew_ppm: u32,
    /// Phase limit for steady-state corrections and RTP/source updates.
    /// Precise exchanges can recover from weak or expired calibration when
    /// the evidence passes physical consistency and timestamp envelopes.
    pub max_phase_error_ns: u64,
}

impl Config {
    fn validate(self) -> Result<(), Error> {
        if !(1..=MAX_TRACKS).contains(&self.track_capacity)
            || !(1..=MAX_WINDOW_NS).contains(&self.max_past_ns)
            || !(1..=self.max_past_ns).contains(&self.max_future_ns)
            || !(1..=self.max_past_ns).contains(&self.observation_max_age_ns)
            || self.nominal_delay_ns >= self.max_past_ns
            || !(1..=200_000).contains(&self.max_drift_ppm)
            || !(1..=self.observation_max_age_ns).contains(&self.max_exchange_rtt_ns)
            || !(1..=self.max_past_ns).contains(&self.min_fit_span_ns)
            || !(1..=self.max_future_ns).contains(&self.lock_uncertainty_ns)
            || !(1..=self.max_past_ns).contains(&self.reorder_ns)
            || !(2..=32).contains(&self.history_capacity)
            || !(1..=100_000).contains(&self.slew_ppm)
            || !(self.lock_uncertainty_ns..=self.max_past_ns).contains(&self.max_phase_error_ns)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Initial estimate for a local monotonic origin, not a claim of UTC authority.
#[derive(Clone, Copy, Debug)]
pub struct LocalOrigin {
    pub at: MonotonicTime,
    pub global: GlobalInstant,
}

/// An extended tick with a caller-validated packet incarnation and lifetime.
/// Use the rate-qualified unwrap helper when starting from a residue.
#[derive(Clone, Copy, Debug)]
pub struct RtpSample {
    pub binding: RtpBinding,
    pub tick: i128,
}

#[derive(Clone, Copy, Debug)]
pub struct SenderReport {
    pub sample: RtpSample,
    pub source: SourceTime,
    /// Source error at the exact RTP tick boundary, including the sender
    /// report's own quantization unless it samples an exact tick.
    pub uncertainty_ns: u64,
    pub received_at: MonotonicTime,
}

#[derive(Clone, Copy, Debug)]
pub struct PassiveReceipt {
    pub binding: Binding,
    pub source: SourceTime,
    pub received_at: MonotonicTime,
}

#[derive(Clone, Copy, Debug)]
pub struct DataCapture {
    pub binding: Binding,
    pub source: SourceTime,
    pub uncertainty_ns: u64,
}

/// Trusted receiver metadata, never derived from the packet's capture stamp.
#[derive(Clone, Copy, Debug)]
pub struct DataReceipt {
    pub at: MonotonicTime,
    /// Latest possible time of this packet's receipt at `at`, in the shared
    /// global coordinate. The caller qualifies it independently of the
    /// capture stamp and includes local clock and transfer uncertainty.
    /// With no qualified bound, DATA cannot authorize expiry.
    pub latest_global: Option<GlobalInstant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provenance {
    ArrivalEstimate,
    SenderReport,
    DataCapture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quality {
    Acquiring,
    Synchronized,
    Degraded,
    Holdover,
    Recovering,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Projection {
    pub global: GlobalInstant,
    pub rtp: RtpTimestamp,
    pub binding: Binding,
    pub provenance: Provenance,
    /// Present only when the track/sample has a qualified source association.
    pub source: Option<SourceTime>,
    /// Absolute uncertainty, not a confidence score.
    pub absolute_uncertainty_ns: Option<u64>,
    pub quality: Quality,
    pub calibration: Option<CalibrationStatus>,
}

#[derive(Clone, Copy)]
struct Point {
    x: i128,
    y: i64,
}

#[derive(Clone, Copy)]
struct Line {
    origin: Point,
    units: i128,
    per: i128,
}

impl Line {
    fn at(self, x: i128) -> Result<i64, Error> {
        narrow(self.wide_at(x)?)
    }

    fn wide_at(self, x: i128) -> Result<i128, Error> {
        let delta = x.checked_sub(self.origin.x).ok_or(Error::Overflow)?;
        let (scaled, _) = affine_scale(delta, self.units, self.per)?;
        i128::from(self.origin.y)
            .checked_add(scaled)
            .ok_or(Error::Overflow)
    }
}

#[derive(Clone, Copy)]
struct ReportFit {
    line: Line,
    received_at: MonotonicTime,
    uncertainty_ns: u64,
    feasible: DriftInterval,
}

impl ReportFit {
    fn observe(
        previous: Option<Self>,
        report: SenderReport,
        rate: ClockRate,
        config: Config,
    ) -> Result<Self, Error> {
        if report.uncertainty_ns > config.max_past_ns {
            return Err(Error::Invalid);
        }
        let mut line = nominal_rtp(report.sample.tick, report.source.as_nanos(), rate);
        let scale = line.per.checked_mul(PPM).ok_or(Error::Overflow)?;
        let mut feasible = DriftInterval::measured(line.origin.y, report.uncertainty_ns, scale)?;
        if let Some(previous) = previous {
            if report.received_at <= previous.received_at || line.origin.x <= previous.line.origin.x
            {
                return Err(Error::Stale);
            }
            let dx = line
                .origin
                .x
                .checked_sub(previous.line.origin.x)
                .ok_or(Error::Overflow)?;
            let dy = i128::from(line.origin.y)
                .checked_sub(i128::from(previous.line.origin.y))
                .ok_or(Error::Overflow)?;
            let nominal = dx.checked_mul(line.units).ok_or(Error::Overflow)?;
            let displacement = DriftInterval::displacement(nominal, config.max_drift_ppm)?;
            feasible = previous
                .feasible
                .advance(displacement)?
                .intersect(feasible)?;

            let observed = dy.checked_mul(scale).ok_or(Error::Overflow)?;
            let clamped = displacement.clamp(observed);
            (line.units, line.per) = if clamped == observed {
                (dy, dx)
            } else {
                // The displacement edge is an exact multiple of dx.
                (clamped.div_euclid(dx), scale)
            };
        }
        Ok(Self {
            line,
            received_at: report.received_at,
            uncertainty_ns: report.uncertainty_ns,
            feasible,
        })
    }

    fn estimate(
        self,
        tick: i128,
        rate: ClockRate,
        max_drift_ppm: u32,
        source: i64,
    ) -> Result<SourceEstimate, Error> {
        let nominal = nominal_rtp(self.line.origin.x, self.line.origin.y, rate).at(tick)?;
        let tick_interval = crate::time::ceil_div(NS_PER_SECOND, i128::from(rate.hz()))?;
        let span = i128::from(nominal.abs_diff(self.line.origin.y))
            .checked_add(tick_interval)
            .ok_or(Error::Overflow)?;
        let drift = drift_error(span, max_drift_ppm)?;
        let quantization = u64::try_from(tick_interval).map_err(|_| Error::Overflow)?;
        Ok(SourceEstimate {
            time: SourceTime::from_nanos(source),
            uncertainty_ns: self
                .uncertainty_ns
                .checked_add(drift)
                .and_then(|n| n.checked_add(source.abs_diff(nominal)))
                .and_then(|n| n.checked_add(quantization))
                .and_then(|n| n.checked_add(2))
                .ok_or(Error::Overflow)?,
        })
    }
}

#[derive(Clone, Copy)]
struct Bridge {
    tick: i128,
    residual_ns: i64,
}

struct Track {
    rate: ClockRate,
    arrival: Option<Line>,
    report: Option<ReportFit>,
    source: Timeline,
    bridge: Option<Bridge>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceState {
    Usable,
    Lost,
    Stepped,
}

#[derive(Clone, Copy)]
struct ReferenceLifecycle {
    state: ReferenceState,
    changed: MonotonicTime,
    last_event: Option<MonotonicTime>,
    invalidated_at: Option<MonotonicTime>,
}

pub struct ClockDomain {
    config: Config,
    binding: Binding,
    local: LocalOrigin,
    tracks: Box<[Option<Track>]>,
    source: Timeline,
    calibration: Option<Calibration>,
    reference: ReferenceLifecycle,
    generation_started_at: MonotonicTime,
}

impl ClockDomain {
    pub fn new(config: Config, binding: Binding, local: LocalOrigin) -> Result<Self, Error> {
        config.validate()?;
        let capacity = usize::try_from(config.track_capacity).map_err(|_| Error::Overflow)?;
        Ok(Self {
            config,
            binding,
            local,
            tracks: std::iter::repeat_with(|| None).take(capacity).collect(),
            source: Timeline::new(config.history_capacity),
            calibration: None,
            generation_started_at: local.at,
            reference: ReferenceLifecycle {
                state: ReferenceState::Usable,
                changed: local.at,
                last_event: None,
                invalidated_at: None,
            },
        })
    }

    /// Start a caller-validated new clock incarnation, never a route change.
    pub fn reset_generation(&mut self, binding: Binding, now: MonotonicTime) -> Result<(), Error> {
        if binding.domain != self.binding.domain || binding.generation <= self.binding.generation {
            return Err(Error::WrongBinding);
        }
        self.check_transition_time(now)?;
        self.binding = binding;
        self.generation_started_at = now;
        self.source.clear();
        self.calibration = None;
        self.tracks.fill_with(|| None);
        Ok(())
    }

    /// Publish the receiver's reference state in notification order, using
    /// the current local monotonic time for `at`, not a backdated estimate
    /// of when a discontinuity first occurred. A step invalidates earlier
    /// exchange evidence for future projections, not already-issued results.
    pub fn set_reference_state(
        &mut self,
        binding: Binding,
        at: MonotonicTime,
        state: ReferenceState,
    ) -> Result<(), Error> {
        self.check_binding(binding)?;
        self.check_transition_time(at)?;
        if self.reference.last_event == Some(at) {
            return if self.reference.state == state {
                Ok(())
            } else {
                Err(Error::Stale)
            };
        }
        self.reference.last_event = Some(at);
        if self.reference.state != state || state == ReferenceState::Stepped {
            self.reference.state = state;
            self.reference.changed = at;
        }
        if state == ReferenceState::Stepped {
            self.reference.invalidated_at = Some(at);
        }
        Ok(())
    }

    fn check_transition_time(&self, at: MonotonicTime) -> Result<(), Error> {
        at.elapsed_since(self.generation_started_at)?;
        if let Some(last) = self.reference.last_event {
            at.elapsed_since(last)?;
        }
        if let Some(calibration) = self.calibration {
            at.elapsed_since(calibration.observed_at())?;
        }
        Ok(())
    }

    pub const fn binding(&self) -> Binding {
        self.binding
    }

    /// A slot cannot be reused within this generation.
    pub fn bind_track(&mut self, slot: u32, rate: ClockRate) -> Result<RtpBinding, Error> {
        let index = usize::try_from(slot).map_err(|_| Error::OutOfWindow)?;
        let entry = self.tracks.get_mut(index).ok_or(Error::OutOfWindow)?;
        if entry.is_some() {
            return Err(Error::Invalid);
        }
        *entry = Some(Track {
            rate,
            arrival: None,
            report: None,
            source: Timeline::new(self.config.history_capacity),
            bridge: None,
        });
        Ok(RtpBinding {
            source: self.binding,
            track: slot,
            rate,
        })
    }

    pub fn bootstrap_track(
        &mut self,
        sample: RtpSample,
        received_at: MonotonicTime,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_observation_age(received_at, now)?;
        let arrival = self.arrival_estimate(received_at)?;
        self.check_envelope(arrival, now)?;
        let track = self.track_mut(sample.binding)?;
        if track.arrival.is_some() || track.report.is_some() {
            return Err(Error::Invalid);
        }
        track.arrival = Some(nominal_rtp(sample.tick, arrival.as_nanos(), track.rate));
        Ok(())
    }

    pub fn observe_passive(
        &mut self,
        receipt: PassiveReceipt,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_binding(receipt.binding)?;
        self.check_observation_age(receipt.received_at, now)?;
        let source = self.passive_line(receipt.source, receipt.received_at)?;
        self.check_receipt(source, receipt.source, receipt.received_at)?;
        self.bootstrap_source(source, receipt.received_at);
        Ok(())
    }

    /// `frontier` is at or beyond every previously assigned tick on this track.
    /// The value at it is preserved exactly; earlier ticks use retained segments.
    pub fn observe_sender_report(
        &mut self,
        report: SenderReport,
        frontier: i128,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_observation_age(report.received_at, now)?;
        let track = self.track(report.sample.binding)?;
        let next = ReportFit::observe(track.report, report, track.rate, self.config)?;
        let line = next.line;
        let source = self.passive_line(report.source, report.received_at)?;
        self.check_receipt(source, report.source, report.received_at)?;
        let rewind = i128::from(self.config.reorder_ns)
            .checked_mul(i128::from(track.rate.hz()))
            .ok_or(Error::Overflow)?
            .div_euclid(NS_PER_SECOND);
        let ppm = self.config.slew_ppm;
        let phase_limit = self.config.max_phase_error_ns;
        let previous = if !track.source.is_empty() {
            Some((track.source.at(frontier)?, track.bridge))
        } else if let Some(arrival) = track.arrival {
            let global = arrival.at(frontier)?;
            self.check_envelope(GlobalInstant::from_nanos(global), now)?;
            let (source_at_frontier, residual_ns) = self.source.source_at_global(global, source)?;
            Some((
                source_at_frontier,
                Some(Bridge {
                    tick: frontier,
                    residual_ns,
                }),
            ))
        } else {
            None
        };
        let basis = previous.map_or_else(|| line.at(frontier), |(at, _)| Ok(at))?;
        let reference_at_frontier = if self.source.is_empty() {
            GlobalInstant::from_nanos(source.at(i128::from(basis))?)
        } else {
            self.source_global(SourceTime::from_nanos(basis))?
        };
        self.check_envelope(reference_at_frontier, now)?;

        let track = self.track_mut(report.sample.binding)?;
        if let Some((previous, bridge)) = previous {
            if previous.abs_diff(line.at(frontier)?) > phase_limit {
                return Err(Error::OutOfWindow);
            }
            track.source.update(line, frontier, previous, rewind, ppm)?;
            track.bridge = bridge;
        } else {
            track.source.start(line);
        }
        track.report = Some(next);
        self.bootstrap_source(source, report.received_at);
        Ok(())
    }

    /// Read-only; identical extended ticks use the same line. The caller
    /// retains the first-assigned output for retry/repair identity.
    pub fn project_rtp(&self, sample: RtpSample, now: MonotonicTime) -> Result<Projection, Error> {
        let track = self.track(sample.binding)?;
        if !track.source.includes(sample.tick) {
            return Err(Error::Unresolved);
        }
        if let Some(report) = track.report
            && track.bridge.is_none_or(|bridge| sample.tick >= bridge.tick)
        {
            let time = track.source.at(sample.tick)?;
            let source =
                report.estimate(sample.tick, track.rate, self.config.max_drift_ppm, time)?;
            let global = self.source_global(source.time)?;
            let residual = track.bridge.map_or(0, |bridge| bridge.residual_ns);
            let global = global.checked_add(crate::time::GlobalDuration::from_nanos(residual))?;
            self.project(
                global,
                track.rate,
                now,
                Provenance::SenderReport,
                Some(source),
            )
        } else {
            let line = track.arrival.ok_or(Error::Unresolved)?;
            let global = GlobalInstant::from_nanos(line.at(sample.tick)?);
            self.project(global, track.rate, now, Provenance::ArrivalEstimate, None)
        }
    }

    /// Sender-clock correlation from an accepted report fit, without the
    /// continuity bridge used by `project_rtp`. Not a receiver wall clock.
    pub fn sender_report_time(
        &self,
        sample: RtpSample,
        now: MonotonicTime,
    ) -> Result<SourceTime, Error> {
        let track = self.track(sample.binding)?;
        if !track.source.includes(sample.tick)
            || track.bridge.is_some_and(|bridge| sample.tick < bridge.tick)
        {
            return Err(Error::Unresolved);
        }
        let report = track.report.ok_or(Error::Unresolved)?;
        self.check_observation_age(report.received_at, now)?;
        report.line.at(sample.tick).map(SourceTime::from_nanos)
    }

    pub fn project_data(
        &self,
        capture: Option<DataCapture>,
        receipt: DataReceipt,
        now: MonotonicTime,
    ) -> Result<Projection, Error> {
        self.check_observation_age(receipt.at, now)?;
        if let Some(stamp) = capture {
            self.check_binding(stamp.binding)?;
            if stamp.uncertainty_ns > self.config.max_past_ns {
                return Err(Error::Invalid);
            }
            if !self.source.is_empty() {
                let global = self.source_global(stamp.source)?;
                let mut projection = self.project(
                    global,
                    ClockRate::DATA,
                    now,
                    Provenance::DataCapture,
                    Some(SourceEstimate {
                        time: stamp.source,
                        uncertainty_ns: stamp.uncertainty_ns,
                    }),
                )?;
                match (receipt.latest_global, projection.absolute_uncertainty_ns) {
                    (Some(latest), Some(radius)) => {
                        let earliest = i128::from(global.as_nanos())
                            .checked_sub(i128::from(radius))
                            .ok_or(Error::Overflow)?;
                        if earliest > i128::from(latest.as_nanos()) {
                            return Err(Error::OutOfWindow);
                        }
                    }
                    (None, _) if projection.quality == Quality::Synchronized => {
                        projection.quality = Quality::Degraded;
                    }
                    _ => {}
                }
                return Ok(projection);
            }
        }
        self.project(
            self.arrival_estimate(receipt.at)?,
            ClockRate::DATA,
            now,
            Provenance::ArrivalEstimate,
            None,
        )
    }

    fn project(
        &self,
        global: GlobalInstant,
        rate: ClockRate,
        now: MonotonicTime,
        provenance: Provenance,
        source: Option<SourceEstimate>,
    ) -> Result<Projection, Error> {
        now.elapsed_since(self.generation_started_at)?;
        self.check_envelope(global, now)?;
        let (calibration, absolute_uncertainty_ns, quality) = match (source, self.calibration) {
            (Some(source), Some(model)) => {
                let (_, active) = self.source.evaluate(i128::from(source.time.as_nanos()))?;
                model.assess(self.config, source, global, now, self.reference, active)?
            }
            (Some(_), None) => (None, None, Quality::Acquiring),
            (None, _) => (None, None, Quality::Degraded),
        };
        Ok(Projection {
            global,
            rtp: rate.timestamp(global)?,
            binding: self.binding,
            provenance,
            source: source.map(|estimate| estimate.time),
            absolute_uncertainty_ns,
            quality,
            calibration,
        })
    }

    fn bootstrap_source(&mut self, line: Line, at: MonotonicTime) {
        if self.source.is_empty() {
            self.source.start(line);
            self.calibration = Some(Calibration::bootstrap(line, at));
        }
    }

    /// `frontier` is no earlier than any already assigned source time in this
    /// domain, including those assigned via tracks. It is not a packet timestamp.
    pub fn observe_exchange(
        &mut self,
        exchange: ActiveExchange,
        frontier: SourceTime,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_binding(exchange.binding)?;
        self.check_observation_age(exchange.reference.at, now)?;
        exchange.sent_at.elapsed_since(self.generation_started_at)?;
        if self.reference.state != ReferenceState::Usable
            || exchange.sent_at < self.reference.changed
        {
            return Err(Error::Unresolved);
        }
        let measurement = exchange.measurement(self.config)?;
        self.observe_calibration(measurement, frontier, now)
    }

    /// Uses the same sealed source frontier as an active exchange.
    pub fn observe_act(
        &mut self,
        hint: ActHint,
        frontier: SourceTime,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_binding(hint.binding)?;
        self.check_observation_age(hint.observed_at, now)?;
        if self.reference.state != ReferenceState::Usable
            || hint.observed_at < self.reference.changed
        {
            return Err(Error::Unresolved);
        }
        self.observe_calibration(hint.measurement(), frontier, now)
    }

    fn observe_calibration(
        &mut self,
        measurement: Measurement,
        frontier: SourceTime,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        self.check_envelope(GlobalInstant::from_nanos(measurement.point.y), now)?;
        let prior = self.calibration.filter(|model| {
            self.reference
                .invalidated_at
                .is_none_or(|step| model.observed_at() > step)
        });
        let next = Calibration::observe(prior, measurement, self.config)?;
        if next.evidence() != measurement.evidence {
            return Ok(());
        }
        if self.source.is_empty() {
            self.source.start(next.fit);
        } else {
            let x = i128::from(frontier.as_nanos());
            let previous = self.source.at(x)?;
            let target = next.fit.at(x)?;
            self.check_envelope(GlobalInstant::from_nanos(target), now)?;
            let prior_exchange = prior.filter(|fit| {
                measurement.evidence == Evidence::Exchange && fit.evidence() == Evidence::Exchange
            });
            let recovers_holdover = if let Some(fit) = prior_exchange {
                measurement.is_precise(self.config)
                    && measurement.at.elapsed_since(fit.observed_at())?
                        > self.config.observation_max_age_ns
            } else {
                false
            };
            // A precise refresh has already passed the same-epoch drift gate.
            // The old holdover estimate is a continuity anchor, not evidence
            // for admission. Projections still apply the timestamp envelope.
            if !recovers_holdover {
                self.check_envelope(GlobalInstant::from_nanos(previous), now)?;
            }
            let baseline = if measurement.evidence != Evidence::Exchange {
                Some(previous)
            } else if let Some(fit) = prior_exchange
                && fit.enforces_phase_limit(measurement, self.config)?
            {
                Some(fit.fit.at(x)?)
            } else {
                None
            };
            if baseline
                .is_some_and(|baseline| baseline.abs_diff(target) > self.config.max_phase_error_ns)
            {
                return Err(Error::OutOfWindow);
            }
            self.source.update(
                next.fit,
                x,
                previous,
                self.source_rewind()?,
                self.config.slew_ppm,
            )?;
        }
        self.calibration = Some(next);
        Ok(())
    }

    fn source_rewind(&self) -> Result<i128, Error> {
        // Keep a nominal RTP reorder interval even when the track-to-source
        // projection stretches it by the maximum fitted drift and slew.
        let drift = PPM
            .checked_add(i128::from(self.config.max_drift_ppm))
            .ok_or(Error::Overflow)?;
        let slew = PPM
            .checked_add(i128::from(self.config.slew_ppm))
            .ok_or(Error::Overflow)?;
        let scale = PPM.checked_mul(PPM).ok_or(Error::Overflow)?;
        let stretched = i128::from(self.config.reorder_ns)
            .checked_mul(drift)
            .and_then(|n| n.checked_mul(slew))
            .ok_or(Error::Overflow)?;
        crate::time::ceil_div(stretched, scale)?
            .checked_add(1)
            .ok_or(Error::Overflow)
    }

    fn track(&self, binding: RtpBinding) -> Result<&Track, Error> {
        let index = self.track_index(binding)?;
        self.tracks
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(Error::WrongBinding)
    }

    fn track_mut(&mut self, binding: RtpBinding) -> Result<&mut Track, Error> {
        let index = self.track_index(binding)?;
        self.tracks
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Error::WrongBinding)
    }

    fn track_index(&self, binding: RtpBinding) -> Result<usize, Error> {
        self.check_binding(binding.source)?;
        let index = usize::try_from(binding.track).map_err(|_| Error::WrongBinding)?;
        if self
            .tracks
            .get(index)
            .and_then(Option::as_ref)
            .is_none_or(|track| track.rate != binding.rate)
        {
            return Err(Error::WrongBinding);
        }
        Ok(index)
    }

    fn check_binding(&self, binding: Binding) -> Result<(), Error> {
        if self.binding != binding {
            return Err(Error::WrongBinding);
        }
        Ok(())
    }

    fn check_observation_age(
        &self,
        measured_at: MonotonicTime,
        now: MonotonicTime,
    ) -> Result<(), Error> {
        measured_at.elapsed_since(self.generation_started_at)?;
        if now.elapsed_since(measured_at)? > self.config.observation_max_age_ns {
            return Err(Error::Stale);
        }
        Ok(())
    }

    fn local_time(&self, at: MonotonicTime) -> Result<i128, Error> {
        i128::from(self.local.global.as_nanos())
            .checked_add(i128::from(at.elapsed_since(self.local.at)?))
            .ok_or(Error::Overflow)
    }

    fn arrival_estimate(&self, at: MonotonicTime) -> Result<GlobalInstant, Error> {
        let estimate = self
            .local_time(at)?
            .checked_sub(i128::from(self.config.nominal_delay_ns))
            .ok_or(Error::Overflow)?;
        Ok(GlobalInstant::from_nanos(narrow(estimate)?))
    }

    fn check_envelope(&self, capture: GlobalInstant, now: MonotonicTime) -> Result<(), Error> {
        let age = self
            .local_time(now)?
            .checked_sub(i128::from(capture.as_nanos()))
            .ok_or(Error::Overflow)?;
        if age > i128::from(self.config.max_past_ns)
            || age.checked_neg().ok_or(Error::Overflow)? > i128::from(self.config.max_future_ns)
        {
            return Err(Error::OutOfWindow);
        }
        Ok(())
    }

    fn passive_line(&self, source: SourceTime, at: MonotonicTime) -> Result<Line, Error> {
        if let Some(calibration) = self.calibration {
            return Ok(calibration.fit);
        }
        Ok(Line {
            origin: Point {
                x: i128::from(source.as_nanos()),
                y: self.arrival_estimate(at)?.as_nanos(),
            },
            units: PPM,
            per: PPM,
        })
    }

    fn check_receipt(
        &self,
        line: Line,
        source: SourceTime,
        at: MonotonicTime,
    ) -> Result<(), Error> {
        let capture = GlobalInstant::from_nanos(line.at(i128::from(source.as_nanos()))?);
        self.check_envelope(capture, at)
    }

    fn source_global(&self, source: SourceTime) -> Result<GlobalInstant, Error> {
        Ok(GlobalInstant::from_nanos(
            self.source.at(i128::from(source.as_nanos()))?,
        ))
    }
}

fn nominal_rtp(tick: i128, at: i64, rate: ClockRate) -> Line {
    Line {
        origin: Point { x: tick, y: at },
        units: NS_PER_SECOND,
        per: i128::from(rate.hz()),
    }
}
