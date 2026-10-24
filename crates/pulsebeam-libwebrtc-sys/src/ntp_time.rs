use crate::ffi;

/// Owned native NTP time, including the invalid all-zero value and all 64 bits
/// of the SDK's unsigned Q32.32 representation. It does not infer an NTP era.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct NtpTime(u64);

impl NtpTime {
    pub const FRACTIONS_PER_SECOND: u64 = 0x1_0000_0000;
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }
    pub const fn from_parts(seconds: u32, fractions: u32) -> Self {
        Self((seconds as u64) * Self::FRACTIONS_PER_SECOND + fractions as u64)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub const fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }
    pub const fn fractions(self) -> u32 {
        self.0 as u32
    }
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
    pub fn set(&mut self, seconds: u32, fractions: u32) {
        *self = Self::from_parts(seconds, fractions);
    }
    pub fn reset(&mut self) {
        self.0 = 0;
    }
    /// Native nearest-millisecond conversion, including fractional carry.
    pub fn millis(self) -> i64 {
        ffi::ntp_time_to_ms(self.0)
    }
    /// Pinned SDK Clock::NtpToUtc semantics, not an independent era-aware
    /// conversion. Native unsigned seconds subtraction wraps before scaling
    /// for pre-1970 values. Invalid NTP zero maps to native minus infinity,
    /// represented here as None rather than a fabricated finite microsecond.
    pub fn utc_microseconds(self) -> Option<i64> {
        self.utc_timestamp().microseconds().ok()
    }
    /// Exact native conversion, including minus infinity for invalid NTP zero.
    /// The native unsigned epoch-subtraction behavior is preserved.
    pub fn utc_timestamp(self) -> crate::Timestamp {
        crate::Timestamp::from_native_microsecond_bits(ffi::ntp_time_to_utc_us(self.0))
    }
}

/// Signed native Q32.32 fixed-point seconds, with native floating-point
/// rounding and saturation when constructing from milliseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Q32x32(i64);
impl Q32x32 {
    pub const fn from_raw(value: i64) -> Self {
        Self(value)
    }
    pub const fn raw(self) -> i64 {
        self.0
    }
    pub fn from_millis(value: i64) -> Self {
        Self(ffi::clock_ms_to_q32x32(value))
    }
    pub fn millis(self) -> i64 {
        ffi::clock_q32x32_to_ms(self.0)
    }
    pub fn microseconds(self) -> i64 {
        ffi::clock_q32x32_to_us(self.0)
    }
}

/// Unsigned native Q32.32 fixed-point seconds. Negative milliseconds clamp to
/// zero; oversized milliseconds clamp to u64::MAX in the native helper.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Uq32x32(u64);
impl Uq32x32 {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub fn from_millis(value: i64) -> Self {
        Self(ffi::clock_ms_to_uq32x32(value))
    }
    pub fn millis(self) -> i64 {
        ffi::clock_uq32x32_to_ms(self.0)
    }
    pub fn microseconds(self) -> i64 {
        ffi::clock_uq32x32_to_us(self.0)
    }
}
