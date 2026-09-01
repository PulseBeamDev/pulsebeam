//! The NTP timeline: the only clock representation that crosses a shard or
//! process boundary. `Instant` is local scheduling time and never leaves.
//!
//! Overflow is explicit here, and denied workspace-wide.
//!
//! `overflow-checks` is off in release, so a bare `+` or `-` that goes out of
//! range does not stop — it yields a plausible-looking number that the pacer,
//! the allocator or the jitter estimator then treats as a measurement. This is
//! timestamp and sequence arithmetic, where that number is the whole output, so
//! every operation says which behaviour it wants: `saturating_` to clamp,
//! `checked_` to fall back, `wrapping_` where an era boundary makes wrapping
//! the correct answer.

//!
//! `overflow-checks` is off in release, so a bare `+` or `-` that goes out of
//! range does not stop — it yields a plausible-looking number that the pacer,
//! the allocator or the jitter estimator then treats as a measurement. This is
//! timestamp and sequence arithmetic, where that number is the whole output, so
//! every operation says which behaviour it wants: `saturating_` to clamp,
//! `checked_` to fall back, `wrapping_` where an era boundary makes wrapping
//! the correct answer.
#![deny(clippy::arithmetic_side_effects)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// One second in NTP 32.32 fixed-point units.
const NTP_UNITS_PER_SEC: u64 = 1 << 32;

/// The envelope carries bits 47..16 of an [`NtpTime`], so one middle-32 step is
/// 2^-16 s and the field repeats every 2^16 s.
const MID_SHIFT: u32 = 16;
pub const MID_RESOLUTION_NANOS: u64 = 15_259;
pub const MID_WRAP: Duration = Duration::from_secs(1 << 16);

/// Largest gap [`NtpExpander`] will bridge, as a signed middle-32 distance.
///
/// The theoretical limit is half a wrap (2^31 steps, ~9.1h) — beyond it the
/// nearest-candidate choice is no longer unique. This sits an octave below that
/// so expansion refuses well before it would silently pick the wrong era.
const MAX_EXPANSION_GAP: i64 = 1 << 30;

/// An instant on the NTP timeline, 32.32 fixed point: the upper 32 bits are
/// seconds since 1900, the lower 32 a binary fraction of a second.
///
/// Serializable and portable, unlike `Instant`.
/// A point on the NTP timeline, 32.32 fixed point.
///
/// Deliberately **not** `Ord`. The seconds field is modulo 2^32, so the
/// timeline wraps at the era boundary and raw integer comparison is wrong
/// there: a timestamp just past the rollover compares as older than one just
/// before it. Every comparison must go through [`NtpTime::units_since`], which
/// is modular, so this omission is what keeps that from being optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NtpTime(u64);

impl NtpTime {
    pub const ZERO: Self = Self(0);

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// The NTP seconds field is modulo 2^32 — the era (which 136-year cycle we
    /// are in) is implicit, and rolls over in 2036. Differences and middle-32
    /// expansion both use wrapping arithmetic, so they stay correct across a
    /// rollover; only [`to_system_time`](Self::to_system_time) needs an era, and
    /// it assumes the current one.
    pub fn from_system_time(t: SystemTime) -> Self {
        let unix = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let secs = unix.as_secs().wrapping_add(NTP_UNIX_OFFSET_SECS) & 0xFFFF_FFFF;
        Self((secs << 32) | frac_units(unix.subsec_nanos()))
    }

    pub fn to_system_time(self) -> SystemTime {
        let secs = (self.0 >> 32).saturating_sub(NTP_UNIX_OFFSET_SECS);
        // An NTP era is 136 years, so this cannot leave `SystemTime`'s range;
        // clamping rather than trapping keeps a logging path from ending the node.
        UNIX_EPOCH
            .checked_add(Duration::new(secs, frac_nanos(self.0)))
            .unwrap_or(UNIX_EPOCH)
    }

    /// Bits 47..16 — what the envelope carries.
    pub const fn middle32(self) -> u32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the middle 32 bits are the wire format; the truncation is the encoding"
        )]
        {
            (self.0 >> MID_SHIFT) as u32
        }
    }

    /// Signed distance to `earlier`, in NTP 32.32 units. Positive when `self` is
    /// later.
    pub const fn units_since(self, earlier: Self) -> i64 {
        // NTP differences are modular; the wrap is what carries the sign.
        self.0.wrapping_sub(earlier.0).cast_signed()
    }

    /// Elapsed time since `earlier`, or zero when `self` precedes it.
    ///
    /// Built on [`units_since`](Self::units_since) so ordering is modular:
    /// raw integer comparison would read a timestamp just past the era rollover
    /// as older than one just before it, and produce a 136-year interval.
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        match self.units_since(earlier) {
            units if units > 0 => units_to_duration(units as u64),
            _ => Duration::ZERO,
        }
    }

    /// Shift forward by `d`, clamping at the end of the NTP range.
    ///
    /// Deliberately *not* modular, unlike the comparisons: those wrap because a
    /// timestamp either side of an era boundary is still a real instant, while
    /// an offset large enough to overflow came from a bad input, and wrapping
    /// it would turn that into a plausible-looking time in the distant past.
    pub fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(duration_to_units(d)))
    }

    /// Shift back by `d`, clamping at the start of the NTP range.
    pub fn saturating_sub(self, d: Duration) -> Self {
        Self(self.0.saturating_sub(duration_to_units(d)))
    }

    /// `Ok` with the elapsed time when `self` is at or after `earlier`, `Err`
    /// with the magnitude when it is before — mirroring `SystemTime`.
    ///
    /// Modular, like every other comparison here: "after" means within the
    /// half-era window ahead, not numerically greater.
    pub fn duration_since(self, earlier: Self) -> Result<Duration, Duration> {
        let units = self.units_since(earlier);
        if units >= 0 {
            Ok(units_to_duration(units as u64))
        } else {
            Err(units_to_duration(units.unsigned_abs()))
        }
    }

    pub fn checked_add(self, d: Duration) -> Option<Self> {
        self.0.checked_add(duration_to_units(d)).map(Self)
    }

    pub fn checked_sub(self, d: Duration) -> Option<Self> {
        self.0.checked_sub(duration_to_units(d)).map(Self)
    }

    /// Wrapping, not saturating: the NTP timeline is modular at the era
    /// boundary, and saturating there would put a discontinuity into playout
    /// scheduling in 2036.
    pub fn wrapping_add(self, d: Duration) -> Self {
        Self(self.0.wrapping_add(duration_to_units(d)))
    }

    pub fn wrapping_sub(self, d: Duration) -> Self {
        Self(self.0.wrapping_sub(duration_to_units(d)))
    }
}

impl std::ops::Add<Duration> for NtpTime {
    type Output = Self;
    fn add(self, d: Duration) -> Self {
        self.wrapping_add(d)
    }
}

impl std::ops::Sub<Duration> for NtpTime {
    type Output = Self;
    fn sub(self, d: Duration) -> Self {
        self.wrapping_sub(d)
    }
}

// Both conversions round to nearest rather than truncating. 32.32 resolves to
// ~233ps, finer than a nanosecond, so round-to-nearest makes ns -> NTP -> ns
// exact. Truncating instead loses 1ns per conversion, which is enough to shift
// every playout timestamp and reshuffle a deterministic simulation.
fn frac_units(nanos: u32) -> u64 {
    debug_assert!(nanos < 1_000_000_000, "not a subsecond value: {nanos}");
    ((u64::from(nanos) << 32).saturating_add(500_000_000)) / 1_000_000_000
}

fn frac_nanos(raw: u64) -> u32 {
    (((raw & 0xFFFF_FFFF)
        .saturating_mul(1_000_000_000)
        .saturating_add(1 << 31))
        >> 32) as u32
}

fn duration_to_units(d: Duration) -> u64 {
    let secs = d.as_secs();
    debug_assert!(
        secs < u64::from(u32::MAX),
        "duration too large for NTP 32.32: {secs}s"
    );
    secs.saturating_mul(NTP_UNITS_PER_SEC)
        .saturating_add(frac_units(d.subsec_nanos()))
}

fn units_to_duration(raw: u64) -> Duration {
    Duration::new(raw >> 32, frac_nanos(raw))
}

/// Maps between the portable NTP timeline and local `Instant` scheduling time.
///
/// Captured once per node at startup and shared by every shard, so all of a
/// node's shards place the same `Instant` at the same NTP instant. It is never
/// refreshed: the wall clock is read exactly once in the process lifetime.
#[derive(Debug, Clone, Copy)]
pub struct WallAnchor {
    ntp: NtpTime,
    mono: Instant,
}

impl WallAnchor {
    pub fn new(wall: SystemTime, mono: Instant) -> Self {
        Self {
            ntp: NtpTime::from_system_time(wall),
            mono,
        }
    }

    pub fn ntp(&self) -> NtpTime {
        self.ntp
    }

    pub fn to_ntp(&self, t: Instant) -> NtpTime {
        if t >= self.mono {
            self.ntp
                .wrapping_add(t.saturating_duration_since(self.mono))
        } else {
            self.ntp
                .wrapping_sub(self.mono.saturating_duration_since(t))
        }
    }

    pub fn to_instant(&self, t: NtpTime) -> Instant {
        match t.units_since(self.ntp) {
            units if units >= 0 => self
                .mono
                .checked_add(units_to_duration(units as u64))
                .unwrap_or(self.mono),
            units => self
                .mono
                .checked_sub(units_to_duration(units.unsigned_abs()))
                .unwrap_or(self.mono),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandError {
    /// The route has been idle long enough that the middle-32 value no longer
    /// identifies a unique instant. The caller must re-establish a full NTP
    /// reference or tear the route down — it must not guess.
    Ambiguous { gap: i64 },
}

/// Recovers full [`NtpTime`] values from the envelope's middle-32 field.
///
/// The reference chains: the first expansion resolves against the full NTP64
/// delivered at route installation, and every one after that against the
/// previously expanded value. The receiving host's own wall clock is
/// consulted exactly once — at route installation, via `wall.ntp()`, to seed
/// that initial reference — and never again: every later expansion chains off
/// the previous expanded value instead of a fresh clock read, so it stays
/// correct under clock skew that develops after installation.
#[derive(Debug, Clone, Copy)]
pub struct NtpExpander {
    reference: NtpTime,
}

impl NtpExpander {
    pub fn new(reference: NtpTime) -> Self {
        Self { reference }
    }

    pub fn reference(&self) -> NtpTime {
        self.reference
    }

    pub fn expand(&mut self, mid: u32) -> Result<NtpTime, ExpandError> {
        // An era boundary must read as a negative gap, not a huge positive one.
        let gap = i64::from(mid.wrapping_sub(self.reference.middle32()).cast_signed());
        if gap.abs() >= MAX_EXPANSION_GAP {
            return Err(ExpandError::Ambiguous { gap });
        }

        let steps = (self.reference.as_raw() >> MID_SHIFT)
            .cast_signed()
            .saturating_add(gap);
        debug_assert!(steps >= 0, "expansion underflowed the NTP epoch: {steps}");
        let expanded = NtpTime::from_raw((steps as u64) << MID_SHIFT);
        debug_assert_eq!(
            expanded.middle32(),
            mid,
            "expansion must preserve the transmitted middle-32"
        );

        self.reference = expanded;
        Ok(expanded)
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
    use super::*;

    fn ntp(secs: u64, frac: u64) -> NtpTime {
        NtpTime::from_raw((secs << 32) | frac)
    }

    #[test]
    fn system_time_round_trips_exactly() {
        // Round-to-nearest makes ns -> NTP -> ns lossless. Anything less and
        // every playout timestamp shifts, which a deterministic sim will notice.
        for nanos in [0, 1, 499_999_999, 500_000_000, 999_999_998, 999_999_999] {
            let t = UNIX_EPOCH + Duration::new(1_700_000_000, nanos);
            assert_eq!(
                NtpTime::from_system_time(t).to_system_time(),
                t,
                "nanos={nanos}"
            );
        }
    }

    #[test]
    fn duration_arithmetic_matches_system_time_exactly() {
        let anchor = UNIX_EPOCH + Duration::new(1_700_000_000, 0);
        let latest = UNIX_EPOCH + Duration::new(1_700_000_001, 123_456_789);
        let (a, l) = (
            NtpTime::from_system_time(anchor),
            NtpTime::from_system_time(latest),
        );
        for i in 0..2_000u32 {
            let d = Duration::from_secs_f64(f64::from(i) * 0.033_366_7);
            let via_system = (latest + d).duration_since(anchor).unwrap();
            let via_ntp = (l + d).saturating_duration_since(a);
            assert_eq!(via_system, via_ntp, "diverged at i={i}");
        }
    }

    #[test]
    fn middle32_has_documented_resolution_and_wrap() {
        let base = ntp(1_000, 0);
        let one_step = NtpTime::from_raw(base.as_raw() + (1 << MID_SHIFT));
        assert_eq!(one_step.middle32(), base.middle32() + 1);
        let step = one_step.saturating_duration_since(base);
        assert!(
            step.abs_diff(Duration::from_nanos(MID_RESOLUTION_NANOS)) <= Duration::from_nanos(1),
            "one middle-32 step should be ~{MID_RESOLUTION_NANOS}ns, got {step:?}"
        );
        assert_eq!(base.wrapping_add(MID_WRAP).middle32(), base.middle32());
    }

    #[test]
    fn differences_survive_an_era_rollover() {
        // Straddle the 2036 boundary: the seconds field wraps, but the delta
        // between two instants a second apart must still read as one second.
        let before = NtpTime::from_raw(0xFFFF_FFFF << 32);
        let after = before + Duration::from_secs(1);
        assert!(
            after.as_raw() < before.as_raw(),
            "test must cross the rollover"
        );
        assert_eq!(after.units_since(before), i64::from(1i32) << 32);
        assert_eq!(
            units_to_duration(after.units_since(before).unsigned_abs()),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn expansion_chains_from_the_route_reference() {
        let reference = ntp(3_900_000_000, 0);
        let mut exp = NtpExpander::new(reference);
        let mut expected = reference;
        for _ in 0..1_000 {
            expected = expected.wrapping_add(Duration::from_millis(20));
            let got = exp.expand(expected.middle32()).unwrap();
            assert!(
                got.saturating_duration_since(expected)
                    < Duration::from_nanos(MID_RESOLUTION_NANOS)
                    || expected.saturating_duration_since(got)
                        < Duration::from_nanos(MID_RESOLUTION_NANOS)
            );
        }
    }

    #[test]
    fn expansion_crosses_the_wrap_boundary() {
        // Start just under a middle-32 wrap so the next step rolls it over.
        let reference =
            NtpTime::from_raw(((1u64 << 48) - (1 << MID_SHIFT)) | (3_900_000_000 << 32));
        let mut exp = NtpExpander::new(reference);
        let target = NtpTime::from_raw(reference.as_raw() + (4 << MID_SHIFT));
        assert!(target.middle32() < reference.middle32(), "test must wrap");

        let got = exp.expand(target.middle32()).unwrap();
        assert_eq!(got.as_raw() >> MID_SHIFT, target.as_raw() >> MID_SHIFT);
    }

    /// Everything that compares two `NtpTime`s must be modular. The seconds
    /// field wraps in 2036, and a raw comparison there reads a fresh timestamp
    /// as 136 years old — which turns a millisecond interval into an enormous
    /// one and sends `to_instant` the wrong way.
    #[test]
    fn comparisons_stay_correct_across_the_era_rollover() {
        // One second either side of the wrap.
        let before = ntp(u32::MAX as u64, 0);
        let after = before.wrapping_add(Duration::from_secs(2));

        assert!(
            after.units_since(before) > 0,
            "a timestamp past the rollover must read as later"
        );
        assert_eq!(
            after.saturating_duration_since(before),
            Duration::from_secs(2),
            "the interval across the rollover must be the real one"
        );
        assert_eq!(before.saturating_duration_since(after), Duration::ZERO);
        assert_eq!(after.duration_since(before), Ok(Duration::from_secs(2)));
        assert_eq!(before.duration_since(after), Err(Duration::from_secs(2)));
    }

    /// `to_instant` must follow the same modular ordering; branching on raw
    /// integer comparison would subtract a whole era from the anchor.
    #[test]
    fn anchor_conversion_stays_correct_across_the_era_rollover() {
        let before = ntp(u32::MAX as u64, 0);
        let mono = Instant::now();
        let anchor = WallAnchor { ntp: before, mono };

        let after = before.wrapping_add(Duration::from_secs(2));
        assert_eq!(
            anchor.to_instant(after),
            mono + Duration::from_secs(2),
            "a post-rollover timestamp must map ahead of the anchor"
        );
        assert_eq!(
            anchor.to_instant(before.wrapping_sub(Duration::from_secs(2))),
            mono - Duration::from_secs(2),
            "a pre-rollover timestamp must map behind it"
        );
    }

    #[test]
    fn expansion_handles_reordering_backwards() {
        let reference = ntp(3_900_000_000, 0);
        let mut exp = NtpExpander::new(reference);
        let ahead = reference.wrapping_add(Duration::from_millis(100));
        exp.expand(ahead.middle32()).unwrap();

        let reordered = reference.wrapping_add(Duration::from_millis(80));
        let got = exp.expand(reordered.middle32()).unwrap();
        assert!(
            got.units_since(ahead) < 0,
            "a reordered packet must expand to an earlier instant"
        );
    }

    #[test]
    fn expansion_refuses_an_ambiguous_gap_instead_of_guessing() {
        let reference = ntp(3_900_000_000, 0);
        let mut exp = NtpExpander::new(reference);
        // Half a wrap away: the nearest candidate is no longer unique.
        let far = reference.wrapping_add(MID_WRAP / 2);
        assert!(matches!(
            exp.expand(far.middle32()),
            Err(ExpandError::Ambiguous { .. })
        ));
        assert_eq!(
            exp.reference(),
            reference,
            "a refused expansion must not advance the chain"
        );
    }

    #[test]
    fn expansion_ignores_receiver_wall_clock_skew() {
        // Two expanders on the same route reference agree regardless of what
        // their hosts think the time is — the reference is the only input.
        let reference = ntp(3_900_000_000, 0);
        let target = reference.wrapping_add(Duration::from_millis(40));
        let mut a = NtpExpander::new(reference);
        let mut b = NtpExpander::new(reference);
        assert_eq!(a.expand(target.middle32()), b.expand(target.middle32()));
    }

    #[tokio::test(start_paused = true)]
    async fn anchor_round_trips_instants() {
        let mono = Instant::now();
        let wall = UNIX_EPOCH + Duration::new(1_700_000_000, 0);
        let anchor = WallAnchor::new(wall, mono);

        let later = mono + Duration::from_millis(250);
        assert_eq!(anchor.to_instant(anchor.to_ntp(later)), later);

        let earlier = mono - Duration::from_millis(250);
        assert_eq!(anchor.to_instant(anchor.to_ntp(earlier)), earlier);
    }
}
