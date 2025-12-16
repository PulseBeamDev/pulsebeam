use str0m::{
    error::PacketError,
    format::{CodecExtra, H264CodecExtra},
};

pub const STAPA_NALU_TYPE: u8 = 24;
pub const FUA_NALU_TYPE: u8 = 28;
pub const FUB_NALU_TYPE: u8 = 29;
pub const IDR_NALU_TYPE: u8 = 5;
pub const SPS_NALU_TYPE: u8 = 7;
pub const PPS_NALU_TYPE: u8 = 8;
pub const AUD_NALU_TYPE: u8 = 9;
pub const FILLER_NALU_TYPE: u8 = 12;

pub const FUA_HEADER_SIZE: usize = 2;
pub const STAPA_HEADER_SIZE: usize = 1;
pub const STAPA_NALU_LENGTH_SIZE: usize = 2;

pub const NALU_TYPE_BITMASK: u8 = 0x1F;
pub const NALU_REF_IDC_BITMASK: u8 = 0x60;
pub const FU_START_BITMASK: u8 = 0x80;
pub const FU_END_BITMASK: u8 = 0x40;

pub const OUTPUT_STAP_AHEADER: u8 = 0x78;

pub static ANNEXB_NALUSTART_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

#[derive(PartialEq, Eq, Debug, Default, Clone)]
pub struct H264Depacketizer {
    /// Whether to output in AVC format (length-prefixed NALUs) instead of Annex B format
    pub is_avc: bool,
    fua_buffer: Option<Vec<u8>>,
}

impl H264Depacketizer {
    /// depacketize parses the passed byte slice and stores the result in the
    /// H264Packet this method is called upon
    pub fn depacketize(
        &mut self,
        packet: &[u8],
        out: &mut Vec<u8>,
        extra: &mut CodecExtra,
    ) -> Result<(), PacketError> {
        if packet.len() == 0 {
            return Err(PacketError::ErrShortPacket);
        }

        // NALU Types
        // https://tools.ietf.org/html/rfc6184#section-5.4
        let b0 = packet[0];
        let nalu_type = b0 & NALU_TYPE_BITMASK;

        match nalu_type {
            t @ 1..=23 => {
                let is_keyframe = if let CodecExtra::H264(e) = extra {
                    (t == IDR_NALU_TYPE) | e.is_keyframe
                } else {
                    t == IDR_NALU_TYPE
                };
                *extra = CodecExtra::H264(H264CodecExtra { is_keyframe });

                if self.is_avc {
                    out.extend_from_slice(&(packet.len() as u32).to_be_bytes());
                } else {
                    out.extend_from_slice(ANNEXB_NALUSTART_CODE);
                }
                out.extend_from_slice(packet);
                Ok(())
            }
            STAPA_NALU_TYPE => {
                let mut curr_offset = STAPA_HEADER_SIZE;
                while curr_offset + 1 < packet.len() {
                    let nalu_size =
                        ((packet[curr_offset] as usize) << 8) | packet[curr_offset + 1] as usize;
                    curr_offset += STAPA_NALU_LENGTH_SIZE;

                    if curr_offset + nalu_size > packet.len() {
                        return Err(PacketError::StapASizeLargerThanBuffer(
                            nalu_size,
                            packet.len() - curr_offset,
                        ));
                    }

                    let Some(b0) = packet.get(curr_offset) else {
                        continue;
                    };
                    let t = b0 & NALU_TYPE_BITMASK;
                    let is_keyframe = if let CodecExtra::H264(e) = extra {
                        (t == IDR_NALU_TYPE) | e.is_keyframe
                    } else {
                        t == IDR_NALU_TYPE
                    };
                    *extra = CodecExtra::H264(H264CodecExtra { is_keyframe });

                    if self.is_avc {
                        out.extend_from_slice(&(nalu_size as u32).to_be_bytes());
                    } else {
                        out.extend_from_slice(ANNEXB_NALUSTART_CODE);
                    }
                    out.extend_from_slice(&packet[curr_offset..curr_offset + nalu_size]);
                    curr_offset += nalu_size;
                }

                Ok(())
            }
            FUA_NALU_TYPE => {
                if packet.len() < FUA_HEADER_SIZE as usize {
                    return Err(PacketError::ErrShortPacket);
                }

                if self.fua_buffer.is_none() {
                    self.fua_buffer = Some(Vec::new());
                }

                if let Some(fua_buffer) = &mut self.fua_buffer {
                    fua_buffer.extend_from_slice(&packet[FUA_HEADER_SIZE as usize..]);
                }

                let b1 = packet[1];
                if b1 & FU_END_BITMASK != 0 {
                    let nalu_ref_idc = b0 & NALU_REF_IDC_BITMASK;
                    let fragmented_nalu_type = b1 & NALU_TYPE_BITMASK;

                    let is_keyframe = if let CodecExtra::H264(e) = extra {
                        (fragmented_nalu_type == IDR_NALU_TYPE) | e.is_keyframe
                    } else {
                        fragmented_nalu_type == IDR_NALU_TYPE
                    };
                    *extra = CodecExtra::H264(H264CodecExtra { is_keyframe });

                    if let Some(fua_buffer) = self.fua_buffer.take() {
                        if self.is_avc {
                            out.extend_from_slice(&((fua_buffer.len() + 1) as u32).to_be_bytes());
                        } else {
                            out.extend_from_slice(ANNEXB_NALUSTART_CODE);
                        }
                        out.push(nalu_ref_idc | fragmented_nalu_type);
                        out.extend_from_slice(&fua_buffer);
                    }

                    Ok(())
                } else {
                    Ok(())
                }
            }
            _ => Err(PacketError::NaluTypeIsNotHandled(nalu_type)),
        }
    }

    /// is_partition_head checks if this is the head of a packetized nalu stream.
    fn is_partition_head(&self, packet: &[u8]) -> bool {
        if packet.len() < 2 {
            return false;
        }

        if packet[0] & NALU_TYPE_BITMASK == FUA_NALU_TYPE
            || packet[0] & NALU_TYPE_BITMASK == FUB_NALU_TYPE
        {
            (packet[1] & FU_START_BITMASK) != 0
        } else {
            true
        }
    }

    fn is_partition_tail(&self, marker: bool, _packet: &[u8]) -> bool {
        marker
    }
}
