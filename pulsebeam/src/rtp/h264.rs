//! H.264 RTP payload classification.
//!
//! The SFU forwards payloads opaquely, but a stream switch is only decodable if
//! the first frame the subscriber sees carries the parameter sets (SPS/PPS) that
//! describe the new stream. Simulcast layers have different resolutions and thus
//! different SPS, so an IDR forwarded without its parameter sets renders as
//! garbage. This module extracts just enough NAL structure to guarantee that.

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
        let mut parts = [""; 3];
        let mut n = 0;
        for (present, name) in [
            (self.sps(), "sps"),
            (self.pps(), "pps"),
            (self.idr(), "idr"),
        ] {
            if present {
                parts[n] = name;
                n += 1;
            }
        }
        if n == 0 {
            f.write_str("NalFlags(-)")
        } else {
            write!(f, "NalFlags({})", parts[..n].join("+"))
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
        // Single NAL unit packet.
        1..=23 => flags.note_nalu_type(first & NALU_TYPE_MASK),

        STAPA_NALU_TYPE => {
            let mut offset = STAPA_HEADER_SIZE;
            while offset + STAPA_NALU_LENGTH_SIZE <= payload.len() {
                let size = ((payload[offset] as usize) << 8) | payload[offset + 1] as usize;
                offset += STAPA_NALU_LENGTH_SIZE;
                if size == 0 || offset + size > payload.len() {
                    break;
                }
                flags.note_nalu_type(payload[offset] & NALU_TYPE_MASK);
                offset += size;
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
            let fu_header = payload[1];
            if fu_header & FU_START_MASK != 0 {
                flags.note_nalu_type(fu_header & NALU_TYPE_MASK);
            }
        }

        _ => {}
    }

    flags
}

#[cfg(test)]
pub mod test_utils {
    use super::*;

    /// A single NAL unit packet of the given type, padded to `len` bytes.
    pub fn single_nalu(nalu_type: u8, len: usize) -> Vec<u8> {
        assert!(len >= 1);
        let mut payload = vec![0u8; len];
        payload[0] = 0x60 | (nalu_type & NALU_TYPE_MASK);
        payload
    }

    pub fn sps(len: usize) -> Vec<u8> {
        single_nalu(SPS_NALU_TYPE, len)
    }

    pub fn pps(len: usize) -> Vec<u8> {
        single_nalu(PPS_NALU_TYPE, len)
    }

    /// A non-IDR coded slice — an ordinary P-frame packet.
    pub fn non_idr(len: usize) -> Vec<u8> {
        single_nalu(1, len)
    }

    /// A STAP-A aggregating the given NAL units, each described as (type, len).
    pub fn stap_a(nalus: &[(u8, usize)]) -> Vec<u8> {
        let mut payload = vec![STAPA_NALU_TYPE];
        for &(ty, len) in nalus {
            assert!(len >= 1);
            payload.push((len >> 8) as u8);
            payload.push(len as u8);
            payload.push(ty & NALU_TYPE_MASK);
            payload.extend(std::iter::repeat_n(0u8, len - 1));
        }
        payload
    }

    /// One fragment of a fragmented NAL unit.
    pub fn fu_a(nalu_type: u8, start: bool, end: bool, len: usize) -> Vec<u8> {
        assert!(len >= FUA_HEADER_SIZE);
        let mut payload = vec![0u8; len];
        payload[0] = FUA_NALU_TYPE;
        let mut fu_header = nalu_type & NALU_TYPE_MASK;
        if start {
            fu_header |= FU_START_MASK;
        }
        if end {
            fu_header |= 0x40;
        }
        payload[1] = fu_header;
        payload
    }

    pub fn idr_fu_a(start: bool, end: bool, len: usize) -> Vec<u8> {
        fu_a(IDR_NALU_TYPE, start, end, len)
    }
}

#[cfg(test)]
mod test {
    use super::test_utils::*;
    use super::*;

    #[test]
    fn empty_payload_carries_nothing() {
        assert_eq!(classify(&[]), NalFlags::empty());
    }

    #[test]
    fn parameter_sets_are_detected_where_str0m_keyframe_detection_is_blind() {
        // The regression this whole module exists for: str0m's keyframe detector
        // reports `false` for these, so a cache keyed on it alone loses them.
        assert!(classify(&sps(20)).sps());
        assert!(!str0m::format::detect_h264_keyframe(&sps(20)));

        assert!(classify(&pps(8)).pps());
        assert!(!str0m::format::detect_h264_keyframe(&pps(8)));
    }

    #[test]
    fn single_nalu_classification() {
        assert!(classify(&non_idr(100)) == NalFlags::empty());
        assert!(classify(&single_nalu(IDR_NALU_TYPE, 100)).idr());
    }

    #[test]
    fn stap_a_reports_every_aggregated_unit() {
        let flags = classify(&stap_a(&[(SPS_NALU_TYPE, 12), (PPS_NALU_TYPE, 5)]));
        assert!(flags.sps() && flags.pps() && !flags.idr());

        let flags = classify(&stap_a(&[
            (SPS_NALU_TYPE, 12),
            (PPS_NALU_TYPE, 5),
            (IDR_NALU_TYPE, 900),
        ]));
        assert!(flags.sps() && flags.pps() && flags.idr());
    }

    #[test]
    fn fragmented_unit_is_attributed_to_its_start_fragment() {
        assert!(classify(&idr_fu_a(true, false, 1200)).idr());
        assert!(!classify(&idr_fu_a(false, false, 1200)).idr());
        assert!(!classify(&idr_fu_a(false, true, 400)).idr());
    }

    #[test]
    fn malformed_payloads_do_not_panic() {
        for payload in [
            vec![STAPA_NALU_TYPE],
            vec![STAPA_NALU_TYPE, 0xFF],
            vec![STAPA_NALU_TYPE, 0xFF, 0xFF, 0x65],
            vec![STAPA_NALU_TYPE, 0x00, 0x00],
            vec![FUA_NALU_TYPE],
            vec![FUB_NALU_TYPE, 0x85],
            vec![0x00],
            vec![0x1F],
        ] {
            let _ = classify(&payload);
        }
    }

    #[test]
    fn classification_agrees_with_str0m_on_idr_presence() {
        for payload in [
            single_nalu(IDR_NALU_TYPE, 300),
            non_idr(300),
            stap_a(&[(SPS_NALU_TYPE, 12), (PPS_NALU_TYPE, 5), (IDR_NALU_TYPE, 40)]),
            stap_a(&[(SPS_NALU_TYPE, 12), (PPS_NALU_TYPE, 5)]),
            idr_fu_a(true, false, 800),
            idr_fu_a(false, false, 800),
        ] {
            assert_eq!(
                classify(&payload).idr(),
                str0m::format::detect_h264_keyframe(&payload),
                "IDR detection must match str0m for payload {:02x?}",
                &payload[..payload.len().min(4)]
            );
        }
    }
}
