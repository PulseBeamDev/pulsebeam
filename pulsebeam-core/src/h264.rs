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
const FU_END_MASK: u8 = 0x40;

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

    pub const fn from_parts(sps: bool, pps: bool, idr: bool) -> Self {
        Self(
            (if sps { FLAG_SPS } else { 0 })
                | (if pps { FLAG_PPS } else { 0 })
                | (if idr { FLAG_IDR } else { 0 }),
        )
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketizedChunk {
    pub payload: Vec<u8>,
    pub start_of_frame: bool,
    pub end_of_frame: bool,
}

#[derive(Debug, Clone)]
pub struct Packetizer {
    mtu: usize,
}

impl Packetizer {
    pub fn new(mtu: usize) -> Self {
        debug_assert!(
            mtu > FUA_HEADER_SIZE,
            "H.264 RTP payload budget must fit FU-A headers"
        );
        Self {
            mtu: mtu.max(FUA_HEADER_SIZE.saturating_add(1)),
        }
    }

    pub fn packetize(&self, access_unit: &[u8]) -> Vec<PacketizedChunk> {
        let nalus = annex_b_nalus(access_unit);
        debug_assert!(
            !nalus.is_empty(),
            "H.264 access unit must contain at least one NAL unit"
        );
        let mut chunks = Vec::new();
        for nalu in nalus {
            if nalu.len() <= self.mtu {
                chunks.push(PacketizedChunk {
                    payload: nalu.to_vec(),
                    start_of_frame: false,
                    end_of_frame: false,
                });
                continue;
            }

            let Some((&header, body)) = nalu.split_first() else {
                debug_assert!(false, "Annex-B parser must not return empty NAL units");
                continue;
            };
            debug_assert!(
                !body.is_empty(),
                "only a NAL larger than the MTU is fragmented"
            );
            let fragment_payload = self.mtu.saturating_sub(FUA_HEADER_SIZE).max(1);
            let fragments = body.len().div_ceil(fragment_payload);
            for fragment_index in 0..fragments {
                let start = fragment_index.saturating_mul(fragment_payload);
                let end = fragment_index
                    .saturating_add(1)
                    .saturating_mul(fragment_payload)
                    .min(body.len());
                debug_assert!(start < end && end <= body.len());
                let start_fragment = fragment_index == 0;
                let end_fragment = fragment_index.saturating_add(1) == fragments;
                let mut payload =
                    Vec::with_capacity(end.saturating_sub(start).saturating_add(FUA_HEADER_SIZE));
                payload.push((header & 0xe0) | FUA_NALU_TYPE);
                payload.push(
                    (header & NALU_TYPE_MASK)
                        | if start_fragment { FU_START_MASK } else { 0 }
                        | if end_fragment { FU_END_MASK } else { 0 },
                );
                let Some(fragment) = body.get(start..end) else {
                    debug_assert!(false, "fragment bounds were checked above");
                    continue;
                };
                payload.extend_from_slice(fragment);
                chunks.push(PacketizedChunk {
                    payload,
                    start_of_frame: false,
                    end_of_frame: false,
                });
            }
        }
        if let Some(first) = chunks.first_mut() {
            first.start_of_frame = true;
        }
        if let Some(last) = chunks.last_mut() {
            last.end_of_frame = true;
        }
        chunks
    }
}

fn annex_b_nalus(access_unit: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i.saturating_add(3) < access_unit.len() {
        let start = match (
            access_unit.get(i),
            access_unit.get(i.saturating_add(1)),
            access_unit.get(i.saturating_add(2)),
            access_unit.get(i.saturating_add(3)),
        ) {
            (Some(0), Some(0), Some(1), _) => Some(i.saturating_add(3)),
            (Some(0), Some(0), Some(0), Some(1)) => Some(i.saturating_add(4)),
            _ => None,
        };
        if let Some(start) = start {
            starts.push((i, start));
            i = start;
        } else {
            i = i.saturating_add(1);
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(index, &(_, start))| {
            let end = starts
                .get(index.saturating_add(1))
                .map(|(next, _)| *next)
                .unwrap_or(access_unit.len());
            access_unit.get(start..end).filter(|nalu| !nalu.is_empty())
        })
        .collect()
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
    fn annex_b_access_units_become_mode_one_h264_rtp_payloads() {
        let mut access_unit = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f];
        access_unit.extend_from_slice(&[0, 0, 1, 0x68, 0xce, 0x06]);
        access_unit.extend_from_slice(&[0, 0, 1, 0x65]);
        access_unit.extend(std::iter::repeat_n(0x55, 2_500));

        let chunks = Packetizer::new(1_100).packetize(&access_unit);
        assert!(chunks.first().is_some_and(|chunk| chunk.start_of_frame));
        assert!(chunks.last().is_some_and(|chunk| chunk.end_of_frame));
        assert!(chunks.iter().all(|chunk| chunk.payload.len() <= 1_100));
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.payload.starts_with(&[0, 0, 1]))
        );
        assert!(classify(&chunks[0].payload).sps());
        assert!(classify(&chunks[1].payload).pps());

        let idr = &chunks[2..];
        assert!(
            idr.first()
                .is_some_and(|chunk| classify(&chunk.payload).idr())
        );
        assert!(
            idr.iter()
                .all(|chunk| chunk.payload[0] & NALU_TYPE_MASK == FUA_NALU_TYPE)
        );
        assert!(
            idr.first()
                .is_some_and(|chunk| chunk.payload[1] & FU_START_MASK != 0)
        );
        assert!(
            idr.last()
                .is_some_and(|chunk| chunk.payload[1] & FU_END_MASK != 0)
        );
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
