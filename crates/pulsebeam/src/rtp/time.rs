use std::{cmp::Ordering, num::NonZeroU32};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frequency(NonZeroU32);

impl Frequency {
    #[allow(
        clippy::unwrap_used,
        reason = "nonzero literal checked during constant evaluation"
    )]
    pub const NINETY_KHZ: Self = Self(NonZeroU32::new(90_000).unwrap());
    #[allow(
        clippy::unwrap_used,
        reason = "nonzero literal checked during constant evaluation"
    )]
    pub const FORTY_EIGHT_KHZ: Self = Self(NonZeroU32::new(48_000).unwrap());
    #[allow(
        clippy::unwrap_used,
        reason = "nonzero literal checked during constant evaluation"
    )]
    const MILLIS: Self = Self(NonZeroU32::new(1_000).unwrap());
    #[allow(
        clippy::unwrap_used,
        reason = "nonzero literal checked during constant evaluation"
    )]
    const HUNDREDTHS: Self = Self(NonZeroU32::new(100).unwrap());

    pub const fn from_nonzero(value: NonZeroU32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    pub const fn nonzero(self) -> NonZeroU32 {
        self.0
    }
}

/// Extended RTP ticks and their negotiated clock rate, independent of transport time.
#[derive(Clone, Copy, Debug)]
pub struct MediaTime {
    ticks: u64,
    frequency: Frequency,
}

impl MediaTime {
    pub const fn new(ticks: u64, frequency: Frequency) -> Self {
        Self { ticks, frequency }
    }

    pub const fn from_90khz(ticks: u64) -> Self {
        Self::new(ticks, Frequency::NINETY_KHZ)
    }

    pub const fn from_millis(millis: u64) -> Self {
        Self::new(millis, Frequency::MILLIS)
    }

    pub const fn from_hundredths(ticks: u64) -> Self {
        Self::new(ticks, Frequency::HUNDREDTHS)
    }

    pub const fn numer(self) -> u64 {
        self.ticks
    }

    pub const fn frequency(self) -> Frequency {
        self.frequency
    }
}

impl PartialEq for MediaTime {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MediaTime {}

impl PartialOrd for MediaTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MediaTime {
    fn cmp(&self, other: &Self) -> Ordering {
        let left = u128::from(self.ticks).saturating_mul(u128::from(other.frequency.get()));
        let right = u128::from(other.ticks).saturating_mul(u128::from(self.frequency.get()));
        left.cmp(&right)
    }
}
