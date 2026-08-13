//! H.264 RTP payload classification.
//!
//! The SFU forwards payloads opaquely, but a stream switch is only decodable if
//! the first frame the subscriber sees carries the parameter sets (SPS/PPS) that
//! describe the new stream. Simulcast layers have different resolutions and thus
//! different SPS, so an IDR forwarded without its parameter sets renders as
//! garbage. This module extracts just enough NAL structure to guarantee that.

pub use pulsebeam_core::h264::{NalFlags, classify};

// Constants used only by test helpers below.
#[cfg(test)]
const NALU_TYPE_MASK: u8 = 0x1F;
#[cfg(test)]
const IDR_NALU_TYPE: u8 = 5;
#[cfg(test)]
const SPS_NALU_TYPE: u8 = 7;
#[cfg(test)]
const PPS_NALU_TYPE: u8 = 8;
#[cfg(test)]
const STAPA_NALU_TYPE: u8 = 24;
#[cfg(test)]
const FUA_NALU_TYPE: u8 = 28;
#[cfg(test)]
const FUB_NALU_TYPE: u8 = 29;
#[cfg(test)]
const FUA_HEADER_SIZE: usize = 2;
#[cfg(test)]
const FU_START_MASK: u8 = 0x80;

#[cfg(test)]
pub mod test_utils {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
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
            let len16 = u16::try_from(len).expect("fixture NAL fits a length prefix");
            payload.extend_from_slice(&len16.to_be_bytes());
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
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
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
