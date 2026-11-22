use std::collections::VecDeque;

use super::{Error, Line, PPM, Point, affine_scale};
use crate::time::narrow;

#[derive(Clone, Copy)]
struct Correction {
    line: Line,
    below_target: bool,
    finished_after: i128,
}

impl Correction {
    fn finished_at(self, x: i128) -> bool {
        let start = self.line.origin.x;
        x >= start && x.abs_diff(start) >= self.finished_after.unsigned_abs()
    }

    fn build(target: Line, frontier: i128, previous: i64, ppm: u32) -> Result<Option<Self>, Error> {
        let delta = frontier
            .checked_sub(target.origin.x)
            .ok_or(Error::Overflow)?;
        let (scaled, residue) = affine_scale(delta, target.units, target.per)?;
        let target_floor = i128::from(target.origin.y)
            .checked_add(scaled)
            .ok_or(Error::Overflow)?;
        if i128::from(previous) == target_floor {
            return Ok(None);
        }
        let below_target = i128::from(previous) < target_floor;
        let ppm = i128::from(ppm);
        let factor = if below_target {
            PPM.checked_add(ppm)
        } else {
            PPM.checked_sub(ppm)
        };
        let units = target
            .units
            .checked_mul(factor.ok_or(Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let gap = i128::from(previous)
            .checked_sub(target_floor)
            .and_then(|n| n.checked_mul(target.per))
            .and_then(|n| n.checked_sub(residue))
            .and_then(i128::checked_abs)
            .and_then(|n| n.checked_mul(PPM))
            .ok_or(Error::Overflow)?;
        let slew_units = target.units.checked_mul(ppm).ok_or(Error::Overflow)?;
        Ok(Some(Self {
            line: Line {
                origin: Point {
                    x: frontier,
                    y: previous,
                },
                units,
                per: target.per.checked_mul(PPM).ok_or(Error::Overflow)?,
            },
            below_target,
            // Rounded outputs can agree before the two exact lines cross.
            finished_after: crate::time::ceil_div(gap, slew_units)?,
        }))
    }
}

#[derive(Clone, Copy)]
struct Segment {
    start: Option<i128>,
    target: Line,
    correction: Option<Correction>,
}

impl Segment {
    fn build(target: Line, frontier: i128, previous: i64, ppm: u32) -> Result<Self, Error> {
        Ok(Self {
            start: Some(frontier),
            target,
            correction: Correction::build(target, frontier, previous, ppm)?,
        })
    }

    fn starts_by(self, x: i128) -> bool {
        self.start.is_none_or(|start| start <= x)
    }

    fn continues(self, target: Line, frontier: i128, previous: i64) -> Result<bool, Error> {
        if target.at(frontier)? != previous {
            return Ok(false);
        }
        if let Some(correction) = self.correction {
            let distance = frontier
                .checked_sub(correction.line.origin.x)
                .ok_or(Error::Overflow)?;
            if distance < correction.finished_after {
                return Ok(false);
            }
        }
        let (units, per) = reduced_slope(self.target);
        // With equal slopes and an equal integer output at the frontier,
        // matching fractional phases prove equality of the exact lines.
        Ok((units, per) == reduced_slope(target)
            && self.target.origin.x.rem_euclid(per) == target.origin.x.rem_euclid(per))
    }

    fn at(self, x: i128) -> Result<(i128, Line), Error> {
        let target = self.target.wide_at(x)?;
        if let Some(correction) = self.correction
            && !correction.finished_at(x)
        {
            let candidate = correction.line.wide_at(x)?;
            // Both lines are monotone. Selecting their min/max preserves that
            // property after integer rounding, unlike subtracting rounded slew.
            if (correction.below_target && candidate < target)
                || (!correction.below_target && candidate > target)
            {
                return Ok((candidate, correction.line));
            }
        }
        Ok((target, self.target))
    }
}

#[cfg(test)]
#[test]
fn rounded_equality_does_not_finish_slew() {
    let target = Line {
        origin: Point { x: 0, y: 0 },
        units: 1,
        per: 2,
    };
    let mut timeline = Timeline::new(2);
    timeline.start(Line {
        origin: Point { x: 0, y: 1 },
        ..target
    });
    timeline.update(target, 0, 1, 100, 100_000).unwrap();
    assert_eq!(timeline.at(2), Ok(1));
    // The rounded values agree, but the exact correction is still above
    // target and will change the output at x=3.
    assert_eq!(
        timeline.update(target, 2, 1, 100, 100_000),
        Err(Error::Capacity)
    );
    assert_eq!(timeline.at(3), Ok(2));
    timeline.update(target, 20, 10, 100, 100_000).unwrap();
    assert_eq!(timeline.at(3), Ok(2));
}

fn reduced_slope(line: Line) -> (i128, i128) {
    let (mut a, mut b) = (line.units, line.per);
    while b != 0 {
        (a, b) = (b, a.rem_euclid(b));
    }
    (line.units.div_euclid(a), line.per.div_euclid(a))
}

pub(super) struct Timeline {
    segments: VecDeque<Segment>,
    cap: usize,
    cutoff: i128,
    sealed: Option<i128>,
}

impl Timeline {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            segments: VecDeque::with_capacity(cap),
            cap,
            cutoff: i128::MIN,
            sealed: None,
        }
    }

    pub(super) fn clear(&mut self) {
        self.segments.clear();
        self.cutoff = i128::MIN;
        self.sealed = None;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub(super) fn includes(&self, x: i128) -> bool {
        x >= self.cutoff
    }

    pub(super) fn start(&mut self, target: Line) {
        debug_assert!(self.is_empty());
        self.segments.push_back(Segment {
            start: None,
            target,
            correction: None,
        });
    }

    pub(super) fn update(
        &mut self,
        target: Line,
        frontier: i128,
        previous: i64,
        rewind: i128,
        ppm: u32,
    ) -> Result<(), Error> {
        if self.sealed.is_some_and(|sealed| frontier < sealed) {
            return Err(Error::Stale);
        }
        let cutoff = frontier.saturating_sub(rewind);
        let prune = self
            .segments
            .partition_point(|segment| segment.starts_by(cutoff))
            .saturating_sub(1);
        let last = self.segments.back();
        let continues = last
            .map(|segment| segment.continues(target, frontier, previous))
            .transpose()?
            .unwrap_or(false);
        if continues {
            self.segments.drain(..prune);
        } else {
            let replace = last.is_some_and(|last| last.start == Some(frontier));
            let retained = self
                .segments
                .len()
                .checked_sub(prune)
                .ok_or(Error::Overflow)?;
            if retained >= self.cap && !replace {
                return Err(Error::Capacity);
            }
            let segment = Segment::build(target, frontier, previous, ppm)?;
            self.segments.drain(..prune);
            if replace {
                self.segments.pop_back();
            }
            self.segments.push_back(segment);
        }
        self.cutoff = self.cutoff.max(cutoff);
        self.sealed = Some(frontier);
        Ok(())
    }

    pub(super) fn at(&self, x: i128) -> Result<i64, Error> {
        narrow(self.evaluate(x)?.0)
    }

    pub(super) fn evaluate(&self, x: i128) -> Result<(i128, Line), Error> {
        if !self.includes(x) {
            return Err(Error::Unresolved);
        }
        let i = self
            .segments
            .partition_point(|segment| segment.starts_by(x))
            .saturating_sub(1);
        self.segments.get(i).ok_or(Error::Unresolved)?.at(x)
    }

    /// Invert at a control-path transition from arrival time to source time.
    /// Keep the residual when a quantized global value has no exact preimage.
    pub(super) fn source_at_global(
        &self,
        global: i64,
        bootstrap: Line,
    ) -> Result<(i64, i64), Error> {
        let mut low = self.cutoff.max(i128::from(i64::MIN));
        let mut high = i128::from(i64::MAX);
        let evaluate = |x| {
            if self.is_empty() {
                bootstrap.wide_at(x)
            } else {
                Ok(self.evaluate(x)?.0)
            }
        };
        let target = i128::from(global);
        if evaluate(low)? > target || evaluate(high)? < target {
            return Err(Error::Unresolved);
        }
        while low < high {
            let mid = low
                .checked_add(high)
                .and_then(|n| n.checked_add(1))
                .ok_or(Error::Overflow)?
                .div_euclid(2);
            if evaluate(mid)? <= target {
                low = mid;
            } else {
                high = mid.checked_sub(1).ok_or(Error::Overflow)?;
            }
        }
        Ok((
            narrow(low)?,
            narrow(target.checked_sub(evaluate(low)?).ok_or(Error::Overflow)?)?,
        ))
    }
}
