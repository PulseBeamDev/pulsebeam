/// Exact signed native data-rate state in bits per second. Infinity remains
/// distinct from finite values instead of saturating to an integer bitrate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataRate {
    BitsPerSecond(i64),
    PositiveInfinity,
    NegativeInfinity,
}
impl DataRate {
    pub(crate) fn from_native(value: i64, kind: i8) -> Self {
        match kind {
            1 => Self::PositiveInfinity,
            -1 => Self::NegativeInfinity,
            _ => Self::BitsPerSecond(value),
        }
    }
}
