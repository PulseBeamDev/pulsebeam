use super::{Error, PPM};

/// Feasible coordinates in an exact scaled space. Advancing the prior
/// intersection keeps older constraints without retaining observations.
#[derive(Clone, Copy)]
pub(super) struct DriftInterval {
    lo: i128,
    hi: i128,
}

impl DriftInterval {
    pub(super) fn measured(y: i64, radius: u64, scale: i128) -> Result<Self, Error> {
        let y = i128::from(y);
        let radius = i128::from(radius);
        let scale_edge =
            |n: Option<i128>| n.and_then(|n| n.checked_mul(scale)).ok_or(Error::Overflow);
        Ok(Self {
            lo: scale_edge(y.checked_sub(radius))?,
            hi: scale_edge(y.checked_add(radius))?,
        })
    }

    // For source/global the nominal slope is 1, scaled by PPM.
    // For RTP/source it is 1e9 / rate, scaled by rate * PPM.
    pub(super) fn displacement(nominal: i128, drift_ppm: u32) -> Result<Self, Error> {
        let drift = i128::from(drift_ppm);
        let edge = |factor: Option<i128>| {
            factor
                .and_then(|n| n.checked_mul(nominal))
                .ok_or(Error::Overflow)
        };
        Ok(Self {
            lo: edge(PPM.checked_sub(drift))?,
            hi: edge(PPM.checked_add(drift))?,
        })
    }

    pub(super) fn advance(self, displacement: Self) -> Result<Self, Error> {
        Ok(Self {
            lo: self
                .lo
                .checked_add(displacement.lo)
                .ok_or(Error::Overflow)?,
            hi: self
                .hi
                .checked_add(displacement.hi)
                .ok_or(Error::Overflow)?,
        })
    }

    pub(super) fn intersect(self, other: Self) -> Result<Self, Error> {
        let lo = self.lo.max(other.lo);
        let hi = self.hi.min(other.hi);
        if lo > hi {
            return Err(Error::OutOfWindow);
        }
        Ok(Self { lo, hi })
    }

    pub(super) fn clamp(self, value: i128) -> i128 {
        value.clamp(self.lo, self.hi)
    }
}

/// Signed Euclidean quotient and residue with a positive ratio. The fallback
/// keeps quotient and remainder separately rather than storing a 256-bit
/// product. It has a fixed limit of 128 iterations regardless of stream length.
pub(super) fn affine_scale(delta: i128, units: i128, per: i128) -> Result<(i128, i128), Error> {
    if let Some(value) = delta.checked_mul(units) {
        return Ok((value.div_euclid(per), value.rem_euclid(per)));
    }
    let divisor = u128::try_from(per).map_err(|_| Error::Invalid)?;
    let units = u128::try_from(units).map_err(|_| Error::Invalid)?;
    let magnitude = delta.unsigned_abs();
    let whole = magnitude
        .checked_mul(units.div_euclid(divisor))
        .ok_or(Error::Overflow)?;
    let factor = units.rem_euclid(divisor);
    let mut quotient = 0_u128;
    let mut residue = 0_u128;
    for bit in (0..u128::BITS).rev() {
        let doubled = residue.checked_mul(2).ok_or(Error::Overflow)?;
        quotient = quotient
            .checked_mul(2)
            .and_then(|q| q.checked_add(doubled.div_euclid(divisor)))
            .ok_or(Error::Overflow)?;
        residue = doubled.rem_euclid(divisor);
        if (magnitude >> bit) & 1 != 0 {
            let added = residue.checked_add(factor).ok_or(Error::Overflow)?;
            quotient = quotient
                .checked_add(added.div_euclid(divisor))
                .ok_or(Error::Overflow)?;
            residue = added.rem_euclid(divisor);
        }
    }
    let mut quotient = quotient.checked_add(whole).ok_or(Error::Overflow)?;
    if delta < 0 && residue != 0 {
        quotient = quotient.checked_add(1).ok_or(Error::Overflow)?;
        residue = divisor.checked_sub(residue).ok_or(Error::Overflow)?;
    }
    let signed = if delta < 0 && quotient == i128::MIN.unsigned_abs() {
        i128::MIN
    } else {
        let q = i128::try_from(quotient).map_err(|_| Error::Overflow)?;
        if delta < 0 {
            q.checked_neg().ok_or(Error::Overflow)?
        } else {
            q
        }
    };
    Ok((
        signed,
        i128::try_from(residue).map_err(|_| Error::Overflow)?,
    ))
}

#[cfg(test)]
#[test]
fn exact_floor_and_residue_when_product_is_wider_than_i128() {
    let divisor = i128::MAX;
    let units = 2;
    assert_eq!(affine_scale(i128::MAX, units, divisor), Ok((2, 0)));
    assert_eq!(
        affine_scale(i128::MIN, units, divisor),
        Ok((-3, i128::MAX - 2))
    );
    assert_eq!(
        affine_scale(i128::MIN, divisor, divisor),
        Ok((i128::MIN, 0))
    );
    assert_eq!(
        affine_scale(i128::MAX, divisor, units),
        Err(Error::Overflow)
    );
}
