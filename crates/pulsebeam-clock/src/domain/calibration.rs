use super::{
    Binding, Config, DriftInterval, Error, GlobalInstant, Line, MonotonicTime, PPM, Point, Quality,
    ReferenceLifecycle, ReferenceState, SourceTime,
};
use crate::time::{ceil_div, narrow};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceQuality {
    /// Includes reference and local monotonic transfer error over the exchange.
    Qualified {
        uncertainty_ns: u64,
    },
    Disqualified,
}

#[derive(Clone, Copy, Debug)]
pub struct ReferenceSample {
    pub at: MonotonicTime,
    pub global: GlobalInstant,
    pub quality: ReferenceQuality,
}

/// A validated exchange, not a network message. `source` is the peer's
/// turnaround instant after accounting for processing time in the interval.
/// `asymmetry_bound_ns` bounds the absolute difference of the two path delays;
/// None means there is no bound better than RTT itself.
#[derive(Clone, Copy, Debug)]
pub struct ActiveExchange {
    pub binding: Binding,
    pub source: SourceTime,
    pub sent_at: MonotonicTime,
    pub reference: ReferenceSample,
    pub asymmetry_bound_ns: Option<u64>,
}

impl ActiveExchange {
    pub(super) fn measurement(self, config: Config) -> Result<Measurement, Error> {
        let ReferenceQuality::Qualified { uncertainty_ns } = self.reference.quality else {
            return Err(Error::Unresolved);
        };
        let rtt = self.reference.at.elapsed_since(self.sent_at)?;
        let asymmetry = self.asymmetry_bound_ns.unwrap_or(rtt);
        if rtt == 0
            || rtt > config.max_exchange_rtt_ns
            || asymmetry > rtt
            || uncertainty_ns > config.max_future_ns
        {
            return Err(Error::Invalid);
        }
        let midpoint = i128::from(self.reference.global.as_nanos())
            .checked_sub(i128::from(rtt.div_ceil(2)))
            .ok_or(Error::Overflow)?;
        let uncertainty_ns = uncertainty_ns
            .checked_add(asymmetry.div_ceil(2))
            .and_then(|n| n.checked_add(rtt % 2))
            .ok_or(Error::Overflow)?;
        Ok(Measurement {
            point: Point {
                x: i128::from(self.source.as_nanos()),
                y: narrow(midpoint)?,
            },
            at: self.reference.at,
            uncertainty_ns,
            evidence: Evidence::Exchange,
        })
    }
}

/// An explicitly correlated capture hint. Its error is conditional, not
/// authoritative UTC accuracy, and it cannot replace an active exchange fit.
#[derive(Clone, Copy, Debug)]
pub struct ActHint {
    pub binding: Binding,
    pub source: SourceTime,
    pub global_estimate: GlobalInstant,
    pub observed_at: MonotonicTime,
    pub conditional_error_ns: u64,
}

impl ActHint {
    pub(super) fn measurement(self) -> Measurement {
        Measurement {
            point: Point {
                x: i128::from(self.source.as_nanos()),
                y: self.global_estimate.as_nanos(),
            },
            at: self.observed_at,
            uncertainty_ns: self.conditional_error_ns,
            evidence: Evidence::Act,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Evidence {
    PassiveReceipt,
    Act,
    Exchange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalibrationStatus {
    pub evidence: Evidence,
    pub observation_age_ns: u64,
    pub fitted_drift_ppm: i32,
    pub active_drift_ppm: i32,
    /// Difference between the assigned global instant and the estimated fit.
    pub phase_correction_ns: i64,
    pub reference_state: ReferenceState,
    pub reference_state_age_ns: u64,
}

#[derive(Clone, Copy)]
pub(super) struct SourceEstimate {
    pub(super) time: SourceTime,
    pub(super) uncertainty_ns: u64,
}

#[derive(Clone, Copy)]
pub(super) struct Measurement {
    pub(super) point: Point,
    pub(super) at: MonotonicTime,
    uncertainty_ns: u64,
    pub(super) evidence: Evidence,
}

impl Measurement {
    pub(super) fn is_precise(self, config: Config) -> bool {
        self.uncertainty_ns <= config.lock_uncertainty_ns
    }
}

#[derive(Clone, Copy)]
pub(super) struct Calibration {
    pub(super) fit: Line,
    latest: Measurement,
    paired: bool,
    feasible: Option<DriftInterval>,
}

impl Calibration {
    pub(super) fn enforces_phase_limit(
        self,
        latest: Measurement,
        config: Config,
    ) -> Result<bool, Error> {
        if !latest.is_precise(config) {
            return Ok(true);
        }
        // During acquisition or holdover, a precise observation can recover
        // using the physical pair gate. A fixed phase limit does not account
        // for accumulated drift or the uncertainty of either measurement.
        Ok(self.paired
            && self.latest.is_precise(config)
            && latest.at.elapsed_since(self.latest.at)? <= config.observation_max_age_ns)
    }

    pub(super) fn observed_at(self) -> MonotonicTime {
        self.latest.at
    }

    pub(super) fn evidence(self) -> Evidence {
        self.latest.evidence
    }
    pub(super) fn bootstrap(fit: Line, at: MonotonicTime) -> Self {
        Self {
            fit,
            latest: Measurement {
                point: fit.origin,
                at,
                uncertainty_ns: 0,
                evidence: Evidence::PassiveReceipt,
            },
            paired: false,
            feasible: None,
        }
    }

    pub(super) fn observe(
        previous: Option<Self>,
        latest: Measurement,
        config: Config,
    ) -> Result<Self, Error> {
        if latest.uncertainty_ns > config.max_past_ns {
            return Err(Error::Invalid);
        }
        let mut fit = Line {
            origin: latest.point,
            units: PPM,
            per: PPM,
        };
        let mut paired = false;
        let measured = DriftInterval::measured(latest.point.y, latest.uncertainty_ns, PPM)?;
        let mut feasible = measured;
        if let Some(previous) = previous {
            if latest.at < previous.latest.at {
                return Err(Error::Stale);
            }
            if previous.latest.evidence == Evidence::Exchange
                && latest.evidence != Evidence::Exchange
            {
                return Ok(previous);
            }
            if previous.latest.evidence != Evidence::PassiveReceipt
                && previous.latest.evidence == latest.evidence
            {
                let dx = latest
                    .point
                    .x
                    .checked_sub(previous.latest.point.x)
                    .ok_or(Error::Overflow)?;
                if dx < i128::from(config.min_fit_span_ns) || latest.at == previous.latest.at {
                    return Err(Error::Stale);
                }
                let dy = i128::from(latest.point.y)
                    .checked_sub(i128::from(previous.latest.point.y))
                    .ok_or(Error::Overflow)?;
                let drift = i128::from(config.max_drift_ppm);
                let displacement = DriftInterval::displacement(dx, config.max_drift_ppm)?;
                feasible = previous
                    .feasible
                    .ok_or(Error::Invalid)?
                    .advance(displacement)?
                    .intersect(measured)?;
                let ppm = dy.checked_mul(PPM).ok_or(Error::Overflow)?.div_euclid(dx);
                fit.units = if latest.is_precise(config) && previous.latest.is_precise(config) {
                    ppm.clamp(
                        PPM.checked_sub(drift).ok_or(Error::Overflow)?,
                        PPM.checked_add(drift).ok_or(Error::Overflow)?,
                    )
                } else {
                    previous.fit.units
                };
                paired = true;
            }
        }
        Ok(Self {
            fit,
            latest,
            paired,
            feasible: Some(feasible),
        })
    }

    pub(super) fn assess(
        self,
        config: Config,
        source: SourceEstimate,
        global: GlobalInstant,
        now: MonotonicTime,
        reference: ReferenceLifecycle,
        active: Line,
    ) -> Result<(Option<CalibrationStatus>, Option<u64>, Quality), Error> {
        let age = now.elapsed_since(self.latest.at)?;
        let fit = self.fit.wide_at(i128::from(source.time.as_nanos()))?;
        let gap = narrow(
            i128::from(global.as_nanos())
                .checked_sub(fit)
                .ok_or(Error::Overflow)?,
        )?;
        let status = Some(CalibrationStatus {
            evidence: self.latest.evidence,
            observation_age_ns: age,
            fitted_drift_ppm: drift_ppm(self.fit)?,
            active_drift_ppm: drift_ppm(active)?,
            phase_correction_ns: gap,
            reference_state: reference.state,
            reference_state_age_ns: now.elapsed_since(reference.changed)?,
        });
        if reference
            .invalidated_at
            .is_some_and(|at| self.latest.at <= at)
        {
            let quality = if reference.state == ReferenceState::Usable {
                Quality::Recovering
            } else {
                Quality::Degraded
            };
            return Ok((status, None, quality));
        }
        if self.latest.evidence != Evidence::Exchange {
            let quality = if reference.state == ReferenceState::Usable {
                Quality::Acquiring
            } else {
                Quality::Degraded
            };
            return Ok((status, None, quality));
        }
        let delta = i128::from(source.time.as_nanos())
            .checked_sub(self.latest.point.x)
            .ok_or(Error::Overflow)?;
        let nominal = i128::from(self.latest.point.y)
            .checked_add(delta)
            .ok_or(Error::Overflow)?;
        let radius = self
            .latest
            .uncertainty_ns
            .checked_add(drift_error(
                delta
                    .checked_abs()
                    .ok_or(Error::Overflow)?
                    .max(i128::from(age)),
                config.max_drift_ppm,
            )?)
            .and_then(|n| n.checked_add(source.uncertainty_ns))
            .and_then(|n| {
                n.checked_add(
                    drift_error(i128::from(source.uncertainty_ns), config.max_drift_ppm).ok()?,
                )
            })
            .and_then(|n| {
                n.checked_add(u64::try_from(nominal.abs_diff(i128::from(global.as_nanos()))).ok()?)
            })
            .ok_or(Error::Overflow)?;
        let quality =
            if age > config.observation_max_age_ns || reference.state == ReferenceState::Lost {
                Quality::Holdover
            } else if gap.unsigned_abs() > config.lock_uncertainty_ns
                || self.latest.at < reference.changed
                || (!self.paired && reference.invalidated_at.is_some())
            {
                Quality::Recovering
            } else if !self.paired {
                Quality::Acquiring
            } else if radius > config.lock_uncertainty_ns {
                Quality::Degraded
            } else {
                Quality::Synchronized
            };
        Ok((status, Some(radius), quality))
    }
}

fn drift_ppm(line: Line) -> Result<i32, Error> {
    let absolute = line
        .units
        .checked_mul(PPM)
        .ok_or(Error::Overflow)?
        .div_euclid(line.per);
    i32::try_from(absolute.checked_sub(PPM).ok_or(Error::Overflow)?).map_err(|_| Error::Overflow)
}

pub(super) fn drift_error(span: i128, ppm: u32) -> Result<u64, Error> {
    let product = span.checked_mul(i128::from(ppm)).ok_or(Error::Overflow)?;
    u64::try_from(ceil_div(product, PPM)?).map_err(|_| Error::Overflow)
}
