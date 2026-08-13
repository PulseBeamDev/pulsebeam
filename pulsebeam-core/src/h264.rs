//! Classifying H.264 RTP payloads by the NAL units they carry.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`. A
//! STAP-A aggregate carries 16-bit lengths chosen by the sender, so every
//! `offset + size` is an offset a peer picked against a buffer it did not. With
//! `overflow-checks` off in release a wrap would not stop — it would read a
//! NAL type from the wrong place and the switcher would treat a payload as a
//! keyframe that is not one.

const NALU_TYPE_MASK: u8 = 0x1F;

const IDR_NALU_TYPE: u8 = 5;
const SPS_NALU_TYPE: u8 = 7;
const PPS_NALU_TYPE: u8 = 8;
const STAPA_NALU_TYPE: u8 = 24;
const FUA_NALU_TYPE: u8 = 28;
const FUB_NALU_TYPE: u8 = 29;

const STAPA_HEADER_SIZE: usize = 1;
const STAPA_NALU_LENGTH_SIZE: usize = 2;
const FUA_HEADER_SIZE: usize = 2;
const FUB_HEADER_SIZE: usize = 4;

const FU_START_MASK: u8 = 0x80;

const FLAG_SPS: u8 = 1 << 0;
const FLAG_PPS: u8 = 1 << 1;
const FLAG_IDR: u8 = 1 << 2;

/// Which of the NAL units relevant to stream switching a payload carries.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NalFlags(u8);

impl NalFlags {
    pub const fn empty() -> Self {
        Self(0)
    }

    #[inline]
    pub const fn sps(self) -> bool {
        self.0 & FLAG_SPS != 0
    }

    #[inline]
    pub const fn pps(self) -> bool {
        self.0 & FLAG_PPS != 0
    }

    #[inline]
    pub const fn idr(self) -> bool {
        self.0 & FLAG_IDR != 0
    }

    #[inline]
    fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }

    #[inline]
    fn note_nalu_type(&mut self, nalu_type: u8) {
        match nalu_type {
            IDR_NALU_TYPE => self.set(FLAG_IDR),
            SPS_NALU_TYPE => self.set(FLAG_SPS),
            PPS_NALU_TYPE => self.set(FLAG_PPS),
            _ => {}
        }
    }
}

impl std::fmt::Debug for NalFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<&str> = [
            (self.sps(), "sps"),
            (self.pps(), "pps"),
            (self.idr(), "idr"),
        ]
        .into_iter()
        .filter_map(|(present, name)| present.then_some(name))
        .collect();
        if parts.is_empty() {
            f.write_str("NalFlags(-)")
        } else {
            write!(f, "NalFlags({})", parts.join("+"))
        }
    }
}

/// Classify a single RTP payload (no RTP header) into the NAL units it carries.
///
/// Only the FU start fragment reliably carries the original NAL type, so a
/// fragmented unit is attributed entirely to its first packet.
pub fn classify(payload: &[u8]) -> NalFlags {
    let mut flags = NalFlags::empty();
    let Some(&first) = payload.first() else {
        return flags;
    };

    match first & NALU_TYPE_MASK {
        // Single NAL unit packet or raw Annex-B NALU (types 1-23).
        1..=23 => flags.note_nalu_type(first & NALU_TYPE_MASK),

        STAPA_NALU_TYPE => {
            let mut offset = STAPA_HEADER_SIZE;
            while offset.saturating_add(STAPA_NALU_LENGTH_SIZE) <= payload.len() {
                let (Some(&hi), Some(&lo)) =
                    (payload.get(offset), payload.get(offset.saturating_add(1)))
                else {
                    break;
                };
                let size = (usize::from(hi) << 8) | usize::from(lo);
                offset = offset.saturating_add(STAPA_NALU_LENGTH_SIZE);
                if size == 0 || offset.saturating_add(size) > payload.len() {
                    break;
                }
                let Some(&nalu) = payload.get(offset) else {
                    break;
                };
                flags.note_nalu_type(nalu & NALU_TYPE_MASK);
                offset = offset.saturating_add(size);
            }
        }

        ty @ (FUA_NALU_TYPE | FUB_NALU_TYPE) => {
            let header_size = if ty == FUA_NALU_TYPE {
                FUA_HEADER_SIZE
            } else {
                FUB_HEADER_SIZE
            };
            if payload.len() < header_size {
                return flags;
            }
            let Some(&fu_header) = payload.get(1) else {
                return flags;
            };
            if fu_header & FU_START_MASK != 0 {
                flags.note_nalu_type(fu_header & NALU_TYPE_MASK);
            }
        }

        _ => {}
    }

    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STAP-A aggregate: header byte, then `(u16 length, payload)` pairs.
    fn stapa(units: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![STAPA_NALU_TYPE];
        for u in units {
            let len = u16::try_from(u.len()).expect("test NAL unit fits a STAP-A length field");
            v.extend_from_slice(&len.to_be_bytes());
            v.extend_from_slice(u);
        }
        v
    }

    fn nal(ty: u8) -> Vec<u8> {
        vec![ty & NALU_TYPE_MASK]
    }

    #[test]
    fn an_empty_payload_carries_nothing() {
        assert_eq!(classify(&[]), NalFlags::empty());
    }

    #[test]
    fn single_nal_units_are_classified_by_type() {
        assert!(classify(&nal(IDR_NALU_TYPE)).idr());
        assert!(classify(&nal(SPS_NALU_TYPE)).sps());
        assert!(classify(&nal(PPS_NALU_TYPE)).pps());
        let other = classify(&nal(1));
        assert!(!other.idr() && !other.sps() && !other.pps());
    }

    #[test]
    fn a_stapa_reports_every_unit_it_aggregates() {
        let f = classify(&stapa(&[
            &nal(SPS_NALU_TYPE),
            &nal(PPS_NALU_TYPE),
            &nal(IDR_NALU_TYPE),
        ]));
        assert!(f.sps() && f.pps() && f.idr());
    }

    /// The length field is the sender's, the buffer is ours. Each of these
    /// walks the aggregate off the end a different way.
    #[test]
    fn a_stapa_never_reads_past_its_buffer() {
        // Length field claiming more than remains.
        let mut over = vec![STAPA_NALU_TYPE, 0x00, 0x40];
        over.push(SPS_NALU_TYPE);
        assert_eq!(classify(&over), NalFlags::empty(), "claimed 64, sent 1");

        // Zero length terminates rather than looping.
        let zero = vec![STAPA_NALU_TYPE, 0x00, 0x00, SPS_NALU_TYPE];
        assert_eq!(classify(&zero), NalFlags::empty());

        // Length field itself truncated to one byte.
        assert_eq!(classify(&[STAPA_NALU_TYPE, 0x00]), NalFlags::empty());

        // Header only.
        assert_eq!(classify(&[STAPA_NALU_TYPE]), NalFlags::empty());

        // The maximum a 16-bit field can claim, against a tiny buffer.
        let huge = vec![STAPA_NALU_TYPE, 0xff, 0xff, SPS_NALU_TYPE];
        assert_eq!(classify(&huge), NalFlags::empty());
    }

    /// A unit ending exactly at the buffer end is valid and must be read;
    /// one byte shorter must not be.
    #[test]
    fn a_stapa_unit_ending_exactly_at_the_end_is_still_read() {
        let mut exact = stapa(&[&nal(IDR_NALU_TYPE)]);
        assert!(classify(&exact).idr());

        exact.pop();
        let short = exact;
        assert_eq!(classify(&short), NalFlags::empty());
    }

    #[test]
    fn every_prefix_of_a_valid_stapa_is_safe() {
        let full = stapa(&[&nal(SPS_NALU_TYPE), &nal(PPS_NALU_TYPE)]);
        for cut in 0..=full.len() {
            let _ = classify(&full[..cut]);
        }
        let f = classify(&full);
        assert!(f.sps() && f.pps());
    }

    #[test]
    fn only_the_start_fragment_of_a_fua_is_attributed() {
        let start = [FUA_NALU_TYPE, FU_START_MASK | IDR_NALU_TYPE];
        assert!(classify(&start).idr());

        let middle = [FUA_NALU_TYPE, IDR_NALU_TYPE];
        assert!(!classify(&middle).idr(), "a continuation is not a keyframe");
    }

    #[test]
    fn fragment_headers_shorter_than_their_type_requires_are_ignored() {
        assert_eq!(classify(&[FUA_NALU_TYPE]), NalFlags::empty());
        assert_eq!(classify(&[FUB_NALU_TYPE]), NalFlags::empty());
        // FU-B needs four bytes; three is short even though index 1 exists.
        assert_eq!(
            classify(&[FUB_NALU_TYPE, FU_START_MASK | IDR_NALU_TYPE, 0x00]),
            NalFlags::empty()
        );
    }

    /// Arbitrary bytes: the classifier runs on whatever a peer sends, so it
    /// must terminate and stay in bounds for all of it.
    #[test]
    fn arbitrary_payloads_never_panic() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..4_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = usize::try_from(state % 24).expect("a value below 24 fits usize");
            let buf: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    u8::try_from((state >> 32) & 0xff).expect("masked to one byte")
                })
                .collect();
            let _ = classify(&buf);
        }
    }
}
