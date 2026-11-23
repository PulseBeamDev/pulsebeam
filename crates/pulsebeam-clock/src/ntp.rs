//! NTP reference conversion with caller-owned UTC rules. Not for publisher clocks
//! that merely use NTP's bit layout.

use crate::{
    Error,
    time::{ClockRate, GlobalInstant, NS_PER_SECOND, RtpTimestamp, UNIX_EPOCH_TAI_NS, narrow},
};

const UNITS: i128 = 4_294_967_296;
const UTC_TO_NTP_SECONDS: i128 = 2_208_988_800;
const UTC_1972_SECONDS: i64 = 63_072_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NtpTimestamp {
    pub seconds: u32,
    pub fraction: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Era(pub i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EraTimestamp {
    pub era: Era,
    pub timestamp: NtpTimestamp,
}

/// Caller qualification of the producer's reference-time convention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NtpScale {
    /// Unsmeared UTC with the last second repeated at a positive leap (RFC 7164).
    UnsmearedRepeatLast,
    Smeared,
    Unknown,
}

/// A UTC second label on the Unix calendar, excluding inserted seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct UtcSecond(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeapTransition {
    /// Midnight immediately following a leap, at the new offset.
    pub utc_after: UtcSecond,
    pub tai_minus_utc: i16,
}

/// A validated, borrowed rule window. The caller attests the table is complete
/// and correct for its coverage, including a transition at the exclusive end
/// if one affects the final covered label. No rules are extrapolated.
#[derive(Clone, Copy, Debug)]
pub struct UtcRules<'a> {
    start: UtcSecond,
    end: UtcSecond,
    initial_offset: i16,
    transitions: &'a [LeapTransition],
}

impl<'a> UtcRules<'a> {
    pub fn new(
        start: UtcSecond,
        end: UtcSecond,
        initial_offset: i16,
        transitions: &'a [LeapTransition],
    ) -> Result<Self, Error> {
        if start.0 < UTC_1972_SECONDS
            || end <= start
            || (start.0 == UTC_1972_SECONDS && initial_offset != 10)
        {
            return Err(Error::Invalid);
        }
        let mut after = start;
        let mut offset = initial_offset;
        for transition in transitions {
            if transition.utc_after <= after
                || transition.utc_after > end
                || transition.utc_after.0.rem_euclid(86_400) != 0
                || transition.tai_minus_utc.abs_diff(offset) != 1
            {
                return Err(Error::Invalid);
            }
            after = transition.utc_after;
            offset = transition.tai_minus_utc;
        }
        Ok(Self {
            start,
            end,
            initial_offset,
            transitions,
        })
    }

    /// Round the binary fraction down to integer nanoseconds without crossing
    /// a second boundary. The era and scale must be supplied, never guessed.
    pub fn decode(self, value: EraTimestamp, scale: NtpScale) -> Result<GlobalInstant, Error> {
        if scale != NtpScale::UnsmearedRepeatLast {
            return Err(Error::Unresolved);
        }
        let utc = i128::from(value.era.0)
            .checked_mul(UNITS)
            .and_then(|n| n.checked_add(i128::from(value.timestamp.seconds)))
            .and_then(|n| n.checked_sub(UTC_TO_NTP_SECONDS))
            .ok_or(Error::Overflow)?;
        let fraction = i128::from(value.timestamp.fraction)
            .checked_mul(NS_PER_SECOND)
            .ok_or(Error::Overflow)?
            .div_euclid(UNITS);
        self.from_utc_parts(utc, fraction)
    }

    /// Convert a qualified unsmeared Unix UTC label without NTP fraction rounding.
    pub fn decode_unix_nanos(self, unix_nanos: i64) -> Result<GlobalInstant, Error> {
        let unix_nanos = i128::from(unix_nanos);
        self.from_utc_parts(
            unix_nanos.div_euclid(NS_PER_SECOND),
            unix_nanos.rem_euclid(NS_PER_SECOND),
        )
    }

    fn from_utc_parts(self, utc: i128, fraction: i128) -> Result<GlobalInstant, Error> {
        let offset = self.offset_at_utc(utc)?;
        let global = utc
            .checked_add(i128::from(offset))
            .and_then(|n| n.checked_mul(NS_PER_SECOND))
            .and_then(|n| n.checked_add(fraction))
            .and_then(|n| n.checked_sub(i128::from(UNIX_EPOCH_TAI_NS)))
            .ok_or(Error::Overflow)?;
        Ok(GlobalInstant::from_nanos(narrow(global)?))
    }

    /// Round the binary fraction down. Encode then decode differs by at most
    /// one nanosecond, toward the past, within representable coverage.
    pub fn encode(self, instant: GlobalInstant) -> Result<EraTimestamp, Error> {
        let tai = i128::from(instant.as_nanos())
            .checked_add(i128::from(UNIX_EPOCH_TAI_NS))
            .ok_or(Error::Overflow)?;
        let tai_second = tai.div_euclid(NS_PER_SECOND);
        let count = self.transitions.partition_point(|leap| {
            i128::from(leap.utc_after.0)
                .checked_add(i128::from(leap.tai_minus_utc))
                .is_some_and(|after| tai_second >= after)
        });
        let offset = self.preceding_offset(count);
        let utc = tai_second
            .checked_sub(i128::from(offset))
            .ok_or(Error::Overflow)?;
        if self.offset_at_utc(utc)? != offset {
            // A positive leap's inserted second has no distinct Unix label.
            return Err(Error::Ambiguous);
        }
        let ntp = utc.checked_add(UTC_TO_NTP_SECONDS).ok_or(Error::Overflow)?;
        let fraction = tai
            .rem_euclid(NS_PER_SECOND)
            .checked_mul(UNITS)
            .ok_or(Error::Overflow)?
            .div_euclid(NS_PER_SECOND);
        Ok(EraTimestamp {
            era: Era(i32::try_from(ntp.div_euclid(UNITS)).map_err(|_| Error::Overflow)?),
            timestamp: NtpTimestamp {
                seconds: u32::try_from(ntp.rem_euclid(UNITS)).map_err(|_| Error::Overflow)?,
                fraction: u32::try_from(fraction).map_err(|_| Error::Overflow)?,
            },
        })
    }

    fn preceding_offset(self, count: usize) -> i16 {
        count
            .checked_sub(1)
            .and_then(|i| self.transitions.get(i))
            .map_or(self.initial_offset, |leap| leap.tai_minus_utc)
    }

    fn offset_at_utc(self, utc: i128) -> Result<i16, Error> {
        if utc < i128::from(self.start.0) || utc >= i128::from(self.end.0) {
            return Err(Error::OutOfWindow);
        }
        let count = self
            .transitions
            .partition_point(|leap| i128::from(leap.utc_after.0) <= utc);
        let offset = self.preceding_offset(count);
        if let Some(next) = self.transitions.get(count)
            && utc.checked_add(1) == Some(i128::from(next.utc_after.0))
        {
            return Err(if next.tai_minus_utc > offset {
                Error::Ambiguous
            } else {
                Error::Unresolved
            });
        }
        Ok(offset)
    }
}

/// One continuous outgoing RTP coordinate, not an epoch-switch policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutgoingRtp {
    pub rate: ClockRate,
    pub modular_offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderReportPair {
    pub ntp: EraTimestamp,
    pub rtp: RtpTimestamp,
}

impl OutgoingRtp {
    pub fn timestamp(self, instant: GlobalInstant) -> Result<RtpTimestamp, Error> {
        Ok(RtpTimestamp(
            self.rate
                .timestamp(instant)?
                .0
                .wrapping_add(self.modular_offset),
        ))
    }

    pub fn sender_report_pair(
        self,
        instant: GlobalInstant,
        utc: UtcRules<'_>,
    ) -> Result<SenderReportPair, Error> {
        Ok(SenderReportPair {
            ntp: utc.encode(instant)?,
            rtp: self.timestamp(instant)?,
        })
    }
}
