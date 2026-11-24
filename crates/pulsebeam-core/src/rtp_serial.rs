//! Extend wrapping RTP coordinates relative to a known position.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionError {
    Ambiguous,
    BeforeOrigin,
    Overflow,
}

/// Extend a 32-bit RTP timestamp or canonical sequence to the position nearest
/// the anchor. An exact half-cycle has no unique extension.
pub fn extend_u32(raw: u32, anchor: u64) -> Result<u64, ExtensionError> {
    extend(u64::from(raw), anchor, 1 << 32)
}

pub(crate) fn extend_u16(raw: u16, anchor: u64) -> Result<u64, ExtensionError> {
    extend(u64::from(raw), anchor, 1 << 16)
}

fn extend(raw: u64, anchor: u64, cycle: u64) -> Result<u64, ExtensionError> {
    let mask = cycle.saturating_sub(1);
    let ahead = raw.wrapping_sub(anchor & mask) & mask;
    match ahead.cmp(&(cycle / 2)) {
        std::cmp::Ordering::Equal => Err(ExtensionError::Ambiguous),
        std::cmp::Ordering::Less => anchor.checked_add(ahead).ok_or(ExtensionError::Overflow),
        std::cmp::Ordering::Greater => anchor
            .checked_sub(cycle.saturating_sub(ahead))
            .ok_or(ExtensionError::BeforeOrigin),
    }
}
