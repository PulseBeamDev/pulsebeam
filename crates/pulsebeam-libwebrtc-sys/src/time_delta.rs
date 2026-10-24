use crate::{TimeValueError, Timestamp, ffi};
use std::fmt;

/// Owned signed native duration, including both infinity states. Constructors
/// and queries share Timestamp's pinned UnitBase conversion semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimeDelta(Timestamp);
impl Default for TimeDelta {
    fn default() -> Self {
        Self::zero()
    }
}
impl TimeDelta {
    pub const fn zero() -> Self {
        Self(Timestamp::zero())
    }
    pub const fn plus_infinity() -> Self {
        Self(Timestamp::plus_infinity())
    }
    pub const fn minus_infinity() -> Self {
        Self(Timestamp::minus_infinity())
    }
    pub const fn from_native_microsecond_bits(value: i64) -> Self {
        Self(Timestamp::from_native_microsecond_bits(value))
    }
    pub const fn native_microsecond_bits(self) -> i64 {
        self.0.native_microsecond_bits()
    }
    pub const fn from_microseconds(value: i64) -> Result<Self, TimeValueError> {
        match Timestamp::from_microseconds(value) {
            Ok(value) => Ok(Self(value)),
            Err(error) => Err(error),
        }
    }
    pub fn from_millis(value: i64) -> Result<Self, TimeValueError> {
        Timestamp::from_millis(value).map(Self)
    }
    pub fn from_seconds(value: i64) -> Result<Self, TimeValueError> {
        Timestamp::from_seconds(value).map(Self)
    }
    pub fn from_minutes(value: i64) -> Result<Self, TimeValueError> {
        Self::from_seconds(value.checked_mul(60).ok_or(TimeValueError::OutOfRange)?)
    }
    pub fn from_microseconds_f64(value: f64) -> Result<Self, TimeValueError> {
        Timestamp::from_microseconds_f64(value).map(Self)
    }
    pub fn from_millis_f64(value: f64) -> Result<Self, TimeValueError> {
        Timestamp::from_millis_f64(value).map(Self)
    }
    pub fn from_seconds_f64(value: f64) -> Result<Self, TimeValueError> {
        Timestamp::from_seconds_f64(value).map(Self)
    }
    pub fn from_minutes_f64(value: f64) -> Result<Self, TimeValueError> {
        Self::from_seconds_f64(value * 60.0)
    }
    pub fn from_microseconds_f32(value: f32) -> Result<Self, TimeValueError> {
        Timestamp::from_microseconds_f32(value).map(Self)
    }
    pub fn from_millis_f32(value: f32) -> Result<Self, TimeValueError> {
        Timestamp::from_millis_f32(value).map(Self)
    }
    pub fn from_seconds_f32(value: f32) -> Result<Self, TimeValueError> {
        Timestamp::from_seconds_f32(value).map(Self)
    }
    pub fn from_minutes_f32(value: f32) -> Result<Self, TimeValueError> {
        Self::from_seconds_f32(value * 60.0)
    }
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
    pub const fn is_finite(self) -> bool {
        self.0.is_finite()
    }
    pub const fn is_infinite(self) -> bool {
        self.0.is_infinite()
    }
    pub const fn is_plus_infinity(self) -> bool {
        self.0.is_plus_infinity()
    }
    pub const fn is_minus_infinity(self) -> bool {
        self.0.is_minus_infinity()
    }
    pub const fn microseconds(self) -> Result<i64, TimeValueError> {
        self.0.microseconds()
    }
    pub fn millis(self) -> Result<i64, TimeValueError> {
        self.0.millis()
    }
    pub fn seconds(self) -> Result<i64, TimeValueError> {
        self.0.seconds()
    }
    pub fn nanoseconds(self) -> Result<i64, TimeValueError> {
        self.0.nanoseconds()
    }
    pub const fn microseconds_or(self, fallback: i64) -> i64 {
        self.0.microseconds_or(fallback)
    }
    pub fn millis_or(self, fallback: i64) -> i64 {
        self.0.millis_or(fallback)
    }
    pub fn seconds_or(self, fallback: i64) -> i64 {
        self.0.seconds_or(fallback)
    }
    pub fn microseconds_f64(self) -> f64 {
        self.0.microseconds_f64()
    }
    pub fn millis_f64(self) -> f64 {
        self.0.millis_f64()
    }
    pub fn seconds_f64(self) -> f64 {
        self.0.seconds_f64()
    }
    pub fn nanoseconds_f64(self) -> f64 {
        self.0.nanoseconds_f64()
    }
    pub fn microseconds_f32(self) -> f32 {
        self.0.microseconds_f32()
    }
    pub fn millis_f32(self) -> f32 {
        self.0.millis_f32()
    }
    pub fn seconds_f32(self) -> f32 {
        self.0.seconds_f32()
    }
    pub fn nanoseconds_f32(self) -> f32 {
        self.0.nanoseconds_f32()
    }
    pub fn abs(self) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        Self::from_microseconds(value.abs())
    }
    /// Unary minus preserves infinities but rejects the asymmetric finite
    /// lower endpoint whose negation would collide with native +infinity.
    pub fn checked_neg(self) -> Result<Self, TimeValueError> {
        if self.native_microsecond_bits() == i64::MIN + 1 {
            return Err(TimeValueError::OutOfRange);
        }
        Ok(Self::from_native_microsecond_bits(ffi::clock_delta_negate(
            self.native_microsecond_bits(),
        )))
    }
    pub fn checked_add(self, other: Self) -> Result<Self, TimeValueError> {
        self.relative(other, false)
    }
    pub fn checked_sub(self, other: Self) -> Result<Self, TimeValueError> {
        self.relative(other, true)
    }
    fn relative(self, other: Self, subtract: bool) -> Result<Self, TimeValueError> {
        validate_relative(
            self.native_microsecond_bits(),
            other.native_microsecond_bits(),
            subtract,
        )?;
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_operation(
                self.native_microsecond_bits(),
                other.native_microsecond_bits(),
                u8::from(subtract),
            ),
        ))
    }
    pub fn checked_add_assign(&mut self, other: Self) -> Result<(), TimeValueError> {
        *self = self.checked_add(other)?;
        Ok(())
    }
    pub fn checked_sub_assign(&mut self, other: Self) -> Result<(), TimeValueError> {
        *self = self.checked_sub(other)?;
        Ok(())
    }
    /// IEEE floating ratio, preserving native infinity and NaN results.
    pub fn ratio(self, other: Self) -> f64 {
        ffi::clock_delta_ratio(
            self.native_microsecond_bits(),
            other.native_microsecond_bits(),
        )
    }
    pub fn checked_mul_i64(self, scalar: i64) -> Result<Self, TimeValueError> {
        self.integer_scalar(scalar, false)
    }
    pub fn checked_div_i64(self, scalar: i64) -> Result<Self, TimeValueError> {
        self.integer_scalar(scalar, true)
    }
    fn integer_scalar(self, scalar: i64, divide: bool) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        if divide && scalar == 0 {
            return Err(TimeValueError::DivisionByZero);
        }
        let result = if divide {
            value.checked_div(scalar)
        } else {
            value.checked_mul(scalar)
        };
        Self::from_microseconds(result.ok_or(TimeValueError::OutOfRange)?)?;
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_operation(value, scalar, if divide { 3 } else { 2 }),
        ))
    }
    /// Match native size_t arithmetic, including its unsigned promotion and
    /// wrapping on 64-bit targets. This is not signed mathematical scaling.
    pub fn checked_mul_usize(self, scalar: usize) -> Result<Self, TimeValueError> {
        self.size_scalar(scalar, false)
    }
    pub fn checked_div_usize(self, scalar: usize) -> Result<Self, TimeValueError> {
        self.size_scalar(scalar, true)
    }
    fn size_scalar(self, scalar: usize, divide: bool) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        if divide && scalar == 0 {
            return Err(TimeValueError::DivisionByZero);
        }
        if usize::BITS == 64 {
            let result = if divide {
                (value as u64) / scalar as u64
            } else {
                (value as u64).wrapping_mul(scalar as u64)
            };
            if result >= i64::MAX as u64 {
                return Err(TimeValueError::OutOfRange);
            }
        } else {
            let result = if divide {
                value.checked_div(scalar as i64)
            } else {
                value.checked_mul(scalar as i64)
            };
            Self::from_microseconds(result.ok_or(TimeValueError::OutOfRange)?)?;
        }
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_size_scalar(value, scalar, divide),
        ))
    }
    pub fn checked_mul_f64(self, scalar: f64) -> Result<Self, TimeValueError> {
        self.float_scalar(scalar, false)
    }
    pub fn checked_div_f64(self, scalar: f64) -> Result<Self, TimeValueError> {
        self.float_scalar(scalar, true)
    }
    fn float_scalar(self, scalar: f64, divide: bool) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        if divide && scalar == 0.0 {
            return Err(TimeValueError::DivisionByZero);
        }
        validate_rounded_float(if divide {
            value as f64 / scalar
        } else {
            value as f64 * scalar
        })?;
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_float_scalar(value, scalar, divide),
        ))
    }
    /// Native division retains float precision before llround. Multiplication
    /// has only a double overload; use checked_mul_f64(scalar.into()) for it.
    pub fn checked_div_f32(self, scalar: f32) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        if scalar == 0.0 {
            return Err(TimeValueError::DivisionByZero);
        }
        validate_rounded_float((value as f32 / scalar) as f64)?;
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_div_float32(value, scalar),
        ))
    }
    /// These preserve native truncating negative rounding, not mathematical
    /// floor/ceiling. Overflowing native intermediate additions are rejected.
    pub fn round_to(self, resolution: Self) -> Result<Self, TimeValueError> {
        self.round(resolution, 4)
    }
    pub fn round_up_to(self, resolution: Self) -> Result<Self, TimeValueError> {
        self.round(resolution, 5)
    }
    pub fn round_down_to(self, resolution: Self) -> Result<Self, TimeValueError> {
        self.round(resolution, 6)
    }
    fn round(self, resolution: Self, operation: u8) -> Result<Self, TimeValueError> {
        let value = self.microseconds()?;
        let r = resolution
            .microseconds()
            .map_err(|_| TimeValueError::InvalidResolution)?;
        if r <= 0 {
            return Err(TimeValueError::InvalidResolution);
        }
        let numerator = match operation {
            4 => value.checked_add(r / 2),
            5 => value.checked_add(r).and_then(|v| v.checked_sub(1)),
            _ => Some(value),
        }
        .ok_or(TimeValueError::OutOfRange)?;
        let result = (numerator / r)
            .checked_mul(r)
            .ok_or(TimeValueError::OutOfRange)?;
        Self::from_microseconds(result)?;
        Ok(Self::from_native_microsecond_bits(
            ffi::clock_delta_operation(value, r, operation),
        ))
    }
}
impl std::ops::Neg for TimeDelta {
    type Output = Result<Self, TimeValueError>;
    fn neg(self) -> Self::Output {
        self.checked_neg()
    }
}
impl fmt::Display for TimeDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ffi::clock_delta_string(self.native_microsecond_bits()))
    }
}

pub(crate) fn validate_relative(
    left: i64,
    right: i64,
    subtract: bool,
) -> Result<(), TimeValueError> {
    let a = Timestamp::from_native_microsecond_bits(left);
    let b = Timestamp::from_native_microsecond_bits(right);
    if a.is_infinite() || b.is_infinite() {
        if a.is_infinite() && b.is_infinite() && ((a == b) == subtract) {
            return Err(TimeValueError::IndeterminateArithmetic);
        }
        return Ok(());
    }
    let result = if subtract {
        left.checked_sub(right)
    } else {
        left.checked_add(right)
    };
    Timestamp::from_microseconds(result.ok_or(TimeValueError::OutOfRange)?)?;
    Ok(())
}
fn validate_rounded_float(value: f64) -> Result<(), TimeValueError> {
    if value.is_nan() {
        return Err(TimeValueError::NotANumber);
    }
    let rounded = value.round();
    let limit = 2.0_f64.powi(63);
    if !rounded.is_finite() || rounded <= -limit || rounded >= limit {
        return Err(TimeValueError::OutOfRange);
    }
    Ok(())
}
