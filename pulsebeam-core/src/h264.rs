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
        // Single NAL unit packet or raw Annex-B NALU (types 1-23).
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
