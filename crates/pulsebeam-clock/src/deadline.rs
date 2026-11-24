//! Stateless timing evidence, not a routing or drop decision.

use crate::{
    Error,
    domain::{Projection, Quality},
    time::{GlobalDuration, GlobalInstant, narrow},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timing {
    pub instant: GlobalInstant,
    /// A conservative radius if known. Heuristic errors must not be used here.
    pub absolute_uncertainty_ns: Option<u64>,
    pub quality: Quality,
}

impl From<Projection> for Timing {
    fn from(value: Projection) -> Self {
        Self {
            instant: value.global,
            absolute_uncertainty_ns: value.absolute_uncertainty_ns,
            quality: value.quality,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interval {
    pub earliest: GlobalDuration,
    pub latest: GlobalDuration,
}

impl Interval {
    fn around(center: i128, radius: i128) -> Result<Self, Error> {
        Ok(Self {
            earliest: GlobalDuration::from_nanos(narrow(
                center.checked_sub(radius).ok_or(Error::Overflow)?,
            )?),
            latest: GlobalDuration::from_nanos(narrow(
                center.checked_add(radius).ok_or(Error::Overflow)?,
            )?),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Expiry {
    Definite,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineEvidence {
    pub age: Option<Interval>,
    pub remaining: Option<Interval>,
    pub expiry: Expiry,
}

/// Conservative capture age at the observation instant. Missing bounds stay unknown.
pub fn capture_age(capture: Timing, now: Timing) -> Result<Option<Interval>, Error> {
    age_terms(capture, now)?
        .map(|(center, radius)| Interval::around(center, radius))
        .transpose()
}

fn age_terms(capture: Timing, now: Timing) -> Result<Option<(i128, i128)>, Error> {
    let (Some(capture_radius), Some(now_radius)) =
        (capture.absolute_uncertainty_ns, now.absolute_uncertainty_ns)
    else {
        return Ok(None);
    };
    let radius = i128::from(capture_radius)
        .checked_add(i128::from(now_radius))
        .ok_or(Error::Overflow)?;
    let center = i128::from(now.instant.as_nanos())
        .checked_sub(i128::from(capture.instant.as_nanos()))
        .ok_or(Error::Overflow)?;
    Ok(Some((center, radius)))
}

/// Evaluate one subscription's nonnegative target without changing capture
/// time. Endpoints are inclusive; even exact equality is not definite expiry.
pub fn evaluate(
    capture: Timing,
    now: Timing,
    target: GlobalDuration,
) -> Result<DeadlineEvidence, Error> {
    if target.as_nanos() < 0 {
        return Err(Error::Invalid);
    }
    let Some((center, radius)) = age_terms(capture, now)? else {
        return Ok(DeadlineEvidence {
            age: None,
            remaining: None,
            expiry: Expiry::Inconclusive,
        });
    };
    let age = Interval::around(center, radius)?;
    let remaining = Interval::around(
        i128::from(target.as_nanos())
            .checked_sub(center)
            .ok_or(Error::Overflow)?,
        radius,
    )?;
    let expiry = if remaining.latest.as_nanos() < 0
        && capture.quality == Quality::Synchronized
        && now.quality == Quality::Synchronized
    {
        Expiry::Definite
    } else {
        Expiry::Inconclusive
    };
    Ok(DeadlineEvidence {
        age: Some(age),
        remaining: Some(remaining),
        expiry,
    })
}
