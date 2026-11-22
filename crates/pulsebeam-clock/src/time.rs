//! Canonical time coordinates and rate-qualified RTP arithmetic.

use crate::Error;

pub(crate) const NS_PER_SECOND: i128 = 1_000_000_000;
const WRAP: i128 = 4_294_967_296;
const HALF_WRAP: i128 = 2_147_483_648;

/// The UTC Unix-epoch instant, expressed in nanoseconds from 1970 TAI midnight.
pub const UNIX_EPOCH_TAI_NS: i64 = 8_000_082_000;
/// The first instant of 1972 UTC, with TAI-UTC = 10 seconds.
pub const UTC_1972_START: GlobalInstant = GlobalInstant::from_nanos(63_072_001_999_918_000);

macro_rules! nanos {
    ($(#[$doc:meta])* $name:ident, $int:ty) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name($int);

        impl $name {
            pub const fn from_nanos(nanos: $int) -> Self {
                Self(nanos)
            }

            pub const fn as_nanos(self) -> $int {
                self.0
            }
        }
    };
}

nanos!(
    /// Continuous TAI elapsed nanoseconds from the UTC Unix-epoch instant.
    GlobalInstant, i64
);
nanos!(
    /// A signed difference on the global timescale.
    GlobalDuration, i64
);
nanos!(
    /// Elapsed time on the caller's local monotonic clock, from a stable local origin.
    MonotonicTime, u64
);
nanos!(
    /// Nanoseconds in a caller-established source reference coordinate, not UTC.
    SourceTime, i64
);

impl GlobalInstant {
    pub fn from_tai_1970_nanos(nanos: i64) -> Result<Self, Error> {
        Ok(Self(
            nanos
                .checked_sub(UNIX_EPOCH_TAI_NS)
                .ok_or(Error::Overflow)?,
        ))
    }

    pub fn to_tai_1970_nanos(self) -> Result<i64, Error> {
        self.0.checked_add(UNIX_EPOCH_TAI_NS).ok_or(Error::Overflow)
    }

    pub fn checked_add(self, duration: GlobalDuration) -> Result<Self, Error> {
        Ok(Self(self.0.checked_add(duration.0).ok_or(Error::Overflow)?))
    }

    pub fn duration_since(self, earlier: Self) -> Result<GlobalDuration, Error> {
        Ok(GlobalDuration(
            self.0.checked_sub(earlier.0).ok_or(Error::Overflow)?,
        ))
    }
}

impl MonotonicTime {
    pub fn elapsed_since(self, earlier: Self) -> Result<u64, Error> {
        self.0.checked_sub(earlier.0).ok_or(Error::Stale)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ClockRate(u32);

impl ClockRate {
    pub const OPUS: Self = Self(48_000);
    pub const VIDEO: Self = Self(90_000);
    pub const DATA: Self = Self(90_000);

    pub const fn new(hz: u32) -> Option<Self> {
        if hz == 0 { None } else { Some(Self(hz)) }
    }

    pub const fn hz(self) -> u32 {
        self.0
    }

    pub fn ticks(self, instant: GlobalInstant) -> Result<i128, Error> {
        Ok(i128::from(instant.0)
            .checked_mul(i128::from(self.0))
            .ok_or(Error::Overflow)?
            .div_euclid(NS_PER_SECOND))
    }

    pub fn timestamp(self, instant: GlobalInstant) -> Result<RtpTimestamp, Error> {
        RtpTimestamp::from_ticks(self.ticks(instant)?)
    }

    /// Inclusive nanosecond endpoints corresponding to an extended tick.
    /// At rates above 1 GHz not every tick contains an integer nanosecond.
    pub fn instant_interval(self, tick: i128) -> Result<(GlobalInstant, GlobalInstant), Error> {
        let boundary = |tick: i128| {
            ceil_div(
                tick.checked_mul(NS_PER_SECOND).ok_or(Error::Overflow)?,
                i128::from(self.0),
            )
        };
        let lower = boundary(tick)?;
        let upper = boundary(tick.checked_add(1).ok_or(Error::Overflow)?)?
            .checked_sub(1)
            .ok_or(Error::Overflow)?;
        if lower > upper {
            return Err(Error::Unresolved);
        }
        Ok((GlobalInstant(narrow(lower)?), GlobalInstant(narrow(upper)?)))
    }
}

pub(crate) fn ceil_div(numerator: i128, positive_denominator: i128) -> Result<i128, Error> {
    numerator
        .div_euclid(positive_denominator)
        .checked_add(i128::from(numerator.rem_euclid(positive_denominator) != 0))
        .ok_or(Error::Overflow)
}

pub(crate) fn narrow(value: i128) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RtpTimestamp(pub u32);

impl RtpTimestamp {
    pub fn from_ticks(ticks: i128) -> Result<Self, Error> {
        Ok(Self(
            u32::try_from(ticks.rem_euclid(WRAP)).map_err(|_| Error::Overflow)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DomainId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Generation(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Binding {
    pub domain: DomainId,
    pub generation: Generation,
}

/// An unwrap binding includes the individual RTP coordinate, not just its rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RtpBinding {
    pub source: Binding,
    pub track: u32,
    pub rate: ClockRate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnwrapWindow {
    past: u32,
    future: u32,
    max_anchor_age_ns: u64,
}

impl UnwrapWindow {
    pub fn new(past: u32, future: u32, max_anchor_age_ns: u64) -> Result<Self, Error> {
        if i128::from(past) >= HALF_WRAP
            || i128::from(future) >= HALF_WRAP
            || max_anchor_age_ns == 0
        {
            return Err(Error::Invalid);
        }
        // Do not let a nearest-wrap anchor remain fresh for a half-period at any
        // supported rate by default; the rate-dependent limit is checked at use.
        Ok(Self {
            past,
            future,
            max_anchor_age_ns,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnwrapAnchor {
    binding: RtpBinding,
    tick: i128,
    measured_at: MonotonicTime,
}

impl UnwrapAnchor {
    pub const fn new(binding: RtpBinding, tick: i128, measured_at: MonotonicTime) -> Self {
        Self {
            binding,
            tick,
            measured_at,
        }
    }

    /// The caller must fence packet/incarnation lifetime independently. Two
    /// packets separated by whole wraps can have identical residues.
    pub fn unwrap(
        self,
        binding: RtpBinding,
        residue: RtpTimestamp,
        now: MonotonicTime,
        window: UnwrapWindow,
    ) -> Result<i128, Error> {
        if binding != self.binding {
            return Err(Error::WrongBinding);
        }
        let age = now.elapsed_since(self.measured_at)?;
        let rate = i128::from(binding.rate.0);
        let half_period = HALF_WRAP
            .checked_mul(NS_PER_SECOND)
            .ok_or(Error::Overflow)?;
        if i128::from(window.max_anchor_age_ns)
            .checked_mul(rate)
            .ok_or(Error::Overflow)?
            >= half_period
        {
            return Err(Error::Invalid);
        }
        if age > window.max_anchor_age_ns {
            return Err(Error::Stale);
        }
        let mut delta = i128::from(residue.0)
            .checked_sub(self.tick.rem_euclid(WRAP))
            .ok_or(Error::Overflow)?
            .rem_euclid(WRAP);
        if delta == HALF_WRAP {
            return Err(Error::Ambiguous);
        }
        if delta > HALF_WRAP {
            delta = delta.checked_sub(WRAP).ok_or(Error::Overflow)?;
        }
        if delta
            < i128::from(window.past)
                .checked_neg()
                .ok_or(Error::Overflow)?
            || delta > i128::from(window.future)
        {
            return Err(Error::OutOfWindow);
        }
        self.tick.checked_add(delta).ok_or(Error::Overflow)
    }
}
