use crate::ffi;
use std::fmt;

/// A pinned native time value cannot be converted or constructed safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeValueError {
    InfiniteValue,
    DivisionByZero,
    OutOfRange,
    NotANumber,
    IndeterminateArithmetic,
    InvalidResolution,
}
impl fmt::Display for TimeValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InfiniteValue => "this native time operation requires a finite value",
            Self::DivisionByZero => "native time scalar division requires a nonzero divisor",
            Self::OutOfRange => "time value is outside the native operation's safe range",
            Self::NotANumber => "native time values cannot represent NaN",
            Self::IndeterminateArithmetic => "native infinity arithmetic is indeterminate",
            Self::InvalidResolution => "native time rounding requires a positive finite resolution",
        })
    }
}
impl std::error::Error for TimeValueError {}

/// Owned native Timestamp, with signed microseconds and both infinity states.
/// The pinned implementation accepts finite negative values, despite its epoch
/// comment. Integer constructors reject sentinel values; raw representation
/// decoding and explicit infinity constructors preserve them instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn zero() -> Self {
        Self(0)
    }
    pub const fn plus_infinity() -> Self {
        Self(i64::MAX)
    }
    pub const fn minus_infinity() -> Self {
        Self(i64::MIN)
    }
    pub const fn from_native_microsecond_bits(value: i64) -> Self {
        Self(value)
    }
    pub const fn native_microsecond_bits(self) -> i64 {
        self.0
    }
    pub const fn from_microseconds(value: i64) -> Result<Self, TimeValueError> {
        if value == i64::MIN || value == i64::MAX {
            Err(TimeValueError::OutOfRange)
        } else {
            Ok(Self(value))
        }
    }
    pub fn from_millis(value: i64) -> Result<Self, TimeValueError> {
        Self::from_integer(value, 1)
    }
    pub fn from_seconds(value: i64) -> Result<Self, TimeValueError> {
        Self::from_integer(value, 2)
    }
    fn from_integer(value: i64, unit: u8) -> Result<Self, TimeValueError> {
        let result = ffi::clock_timestamp_from_int(value, unit);
        if result.valid {
            Ok(Self(result.value))
        } else {
            Err(TimeValueError::OutOfRange)
        }
    }
    pub fn from_microseconds_f64(value: f64) -> Result<Self, TimeValueError> {
        Self::from_float(value, 0)
    }
    pub fn from_millis_f64(value: f64) -> Result<Self, TimeValueError> {
        Self::from_float(value, 1)
    }
    pub fn from_seconds_f64(value: f64) -> Result<Self, TimeValueError> {
        Self::from_float(value, 2)
    }
    fn from_float(value: f64, unit: u8) -> Result<Self, TimeValueError> {
        if value.is_nan() {
            return Err(TimeValueError::NotANumber);
        }
        let result = ffi::clock_timestamp_from_float(value, unit);
        if result.valid {
            Ok(Self(result.value))
        } else {
            Err(TimeValueError::OutOfRange)
        }
    }
    pub fn from_microseconds_f32(value: f32) -> Result<Self, TimeValueError> {
        Self::from_float32(value, 0)
    }
    pub fn from_millis_f32(value: f32) -> Result<Self, TimeValueError> {
        Self::from_float32(value, 1)
    }
    pub fn from_seconds_f32(value: f32) -> Result<Self, TimeValueError> {
        Self::from_float32(value, 2)
    }
    fn from_float32(value: f32, unit: u8) -> Result<Self, TimeValueError> {
        if value.is_nan() {
            return Err(TimeValueError::NotANumber);
        }
        let result = ffi::clock_timestamp_from_float32(value, unit);
        if result.valid {
            Ok(Self(result.value))
        } else {
            Err(TimeValueError::OutOfRange)
        }
    }
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
    pub const fn is_plus_infinity(self) -> bool {
        self.0 == i64::MAX
    }
    pub const fn is_minus_infinity(self) -> bool {
        self.0 == i64::MIN
    }
    pub const fn is_infinite(self) -> bool {
        self.is_plus_infinity() || self.is_minus_infinity()
    }
    pub const fn is_finite(self) -> bool {
        !self.is_infinite()
    }
    pub const fn finite(self) -> Result<FiniteTimestamp, TimeValueError> {
        if self.is_finite() {
            Ok(FiniteTimestamp(self))
        } else {
            Err(TimeValueError::InfiniteValue)
        }
    }
    pub const fn microseconds(self) -> Result<i64, TimeValueError> {
        if self.is_finite() {
            Ok(self.0)
        } else {
            Err(TimeValueError::InfiniteValue)
        }
    }
    pub fn millis(self) -> Result<i64, TimeValueError> {
        self.integer_query(1)
    }
    pub fn seconds(self) -> Result<i64, TimeValueError> {
        self.integer_query(2)
    }
    pub fn nanoseconds(self) -> Result<i64, TimeValueError> {
        self.integer_query(3)
    }
    fn integer_query(self, unit: u8) -> Result<i64, TimeValueError> {
        if self.is_infinite() {
            return Err(TimeValueError::InfiniteValue);
        }
        let result = ffi::clock_timestamp_convert(self.0, unit);
        if result.valid {
            Ok(result.value)
        } else {
            Err(TimeValueError::OutOfRange)
        }
    }
    pub const fn microseconds_or(self, fallback: i64) -> i64 {
        if self.is_finite() { self.0 } else { fallback }
    }
    pub fn millis_or(self, fallback: i64) -> i64 {
        self.millis().unwrap_or(fallback)
    }
    pub fn seconds_or(self, fallback: i64) -> i64 {
        self.seconds().unwrap_or(fallback)
    }
    pub fn checked_add(self, delta: crate::TimeDelta) -> Result<Self, TimeValueError> {
        self.relative(delta, false)
    }
    pub fn checked_sub(self, delta: crate::TimeDelta) -> Result<Self, TimeValueError> {
        self.relative(delta, true)
    }
    fn relative(self, delta: crate::TimeDelta, subtract: bool) -> Result<Self, TimeValueError> {
        crate::time_delta::validate_relative(self.0, delta.native_microsecond_bits(), subtract)?;
        Ok(Self(ffi::clock_timestamp_operation(
            self.0,
            delta.native_microsecond_bits(),
            u8::from(subtract),
        )))
    }
    pub fn duration_since(self, other: Self) -> Result<crate::TimeDelta, TimeValueError> {
        crate::time_delta::validate_relative(self.0, other.0, true)?;
        Ok(crate::TimeDelta::from_native_microsecond_bits(
            ffi::clock_timestamp_operation(self.0, other.0, 2),
        ))
    }
    pub fn checked_add_assign(&mut self, delta: crate::TimeDelta) -> Result<(), TimeValueError> {
        *self = self.checked_add(delta)?;
        Ok(())
    }
    pub fn checked_sub_assign(&mut self, delta: crate::TimeDelta) -> Result<(), TimeValueError> {
        *self = self.checked_sub(delta)?;
        Ok(())
    }
    pub fn microseconds_f32(self) -> f32 {
        ffi::clock_timestamp_float32(self.0, 0)
    }
    pub fn millis_f32(self) -> f32 {
        ffi::clock_timestamp_float32(self.0, 1)
    }
    pub fn seconds_f32(self) -> f32 {
        ffi::clock_timestamp_float32(self.0, 2)
    }
    pub fn nanoseconds_f32(self) -> f32 {
        ffi::clock_timestamp_float32(self.0, 3)
    }
    pub fn microseconds_f64(self) -> f64 {
        ffi::clock_timestamp_float(self.0, 0)
    }
    pub fn millis_f64(self) -> f64 {
        ffi::clock_timestamp_float(self.0, 1)
    }
    pub fn seconds_f64(self) -> f64 {
        ffi::clock_timestamp_float(self.0, 2)
    }
    pub fn nanoseconds_f64(self) -> f64 {
        ffi::clock_timestamp_float(self.0, 3)
    }
}
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ffi::clock_timestamp_string(self.0))
    }
}

/// Proof that a native Timestamp supports integral microsecond/millisecond
/// clock queries. Negative finite values are preserved. This does not imply
/// that the nanosecond query or an arbitrary clock's NTP conversion is valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FiniteTimestamp(Timestamp);
impl FiniteTimestamp {
    pub const fn from_microseconds(value: i64) -> Result<Self, TimeValueError> {
        match Timestamp::from_microseconds(value) {
            Ok(timestamp) => Ok(Self(timestamp)),
            Err(error) => Err(error),
        }
    }
    pub const fn timestamp(self) -> Timestamp {
        self.0
    }
    pub const fn microseconds(self) -> i64 {
        self.0.0
    }
}
impl From<FiniteTimestamp> for Timestamp {
    fn from(value: FiniteTimestamp) -> Self {
        value.timestamp()
    }
}
impl TryFrom<Timestamp> for FiniteTimestamp {
    type Error = TimeValueError;
    fn try_from(value: Timestamp) -> Result<Self, Self::Error> {
        value.finite()
    }
}
