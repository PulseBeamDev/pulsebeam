use crate::rtp::RtpPacket;
use std::collections::BTreeMap;
use str0m::rtp::SeqNo;

// --- Constants adapted from your H264Packetizer ---
const STAPA_NALU_TYPE: u8 = 24;
const FUA_NALU_TYPE: u8 = 28;
const IDR_NALU_TYPE: u8 = 5;
const SPS_NALU_TYPE: u8 = 7;
const PPS_NALU_TYPE: u8 = 8;
const AUD_NALU_TYPE: u8 = 9; // Access Unit Delimiter
const SEI_NALU_TYPE: u8 = 6; // Supplemental Enhancement Information
const NALU_TYPE_BITMASK: u8 = 0x1F;
const FU_START_BITMASK: u8 = 0x80;
const STAPA_HEADER_SIZE: usize = 1;
const STAPA_NALU_LENGTH_SIZE: usize = 2;

#[derive(Debug)]
pub struct H264KeyframeDetector {
    marker_seqs: BTreeMap<SeqNo, ()>,
}

impl H264KeyframeDetector {
    const MAX_SEQ_DISTANCE: u64 = 128;

    pub fn new() -> Self {
        Self {
            marker_seqs: BTreeMap::new(),
        }
    }

    pub fn process_packet(&mut self, packet: &RtpPacket) -> bool {
        let seq = packet.seq_no;

        // 1. Mark phase
        if packet.raw_header.marker {
            tracing::debug!("marker found: {}", seq);
            self.marker_seqs.insert(seq, ());
        }

        // 2. Cleanup phase
        let cutoff = seq.saturating_sub(Self::MAX_SEQ_DISTANCE).into();
        self.marker_seqs.retain(|&s, _| s >= cutoff);

        // 3. Detection phase
        // We check if this looks like a keyframe start.
        if !self.is_keyframe_start(packet) {
            return false;
        }

        // 4. Verification phase
        // Strict check: Is this packet immediately following a packet with a Marker?
        let prev_seq = seq.wrapping_sub(1).into();
        if self.marker_seqs.contains_key(&prev_seq) {
            tracing::debug!("idr start found (after marker): {}", seq);
            return true;
        }

        // OPTIONAL: Heuristic fallback for WebRTC UDP reordering.
        // If we found a raw SPS or PPS, it is *almost certainly* the start of a keyframe,
        // even if the UDP packet with the previous marker arrived out of order.
        // Uncomment the lines below if you want to be even more aggressive against False Negatives.
        /*
        if self.contains_sps_or_pps(packet) {
             tracing::debug!("idr start found (forced by SPS/PPS): {}", seq);
             return true;
        }
        */

        false
    }

    fn is_keyframe_start(&self, packet: &RtpPacket) -> bool {
        if packet.payload.is_empty() {
            return false;
        }

        let b0 = packet.payload[0];
        let nalu_type = b0 & NALU_TYPE_BITMASK;

        match nalu_type {
            // SINGLE NAL UNIT CASE
            // We accept IDR, SPS, PPS, AUD, and SEI as valid starts.
            IDR_NALU_TYPE | SPS_NALU_TYPE | PPS_NALU_TYPE | AUD_NALU_TYPE | SEI_NALU_TYPE => true,

            // STAP-A CASE (Aggregation)
            STAPA_NALU_TYPE => {
                // We use the logic from H264Depacketizer to peek at the first NAL.
                // STAP-A format: [Header (1)] [Size (2)] [NAL Header (1)] ...
                let first_nal_offset = STAPA_HEADER_SIZE + STAPA_NALU_LENGTH_SIZE;

                if packet.payload.len() <= first_nal_offset {
                    return false;
                }

                // Check the header of the FIRST aggregated NAL unit
                let first_nal_header = packet.payload[first_nal_offset]; // Index 3
                let first_type = first_nal_header & NALU_TYPE_BITMASK;

                matches!(
                    first_type,
                    IDR_NALU_TYPE | SPS_NALU_TYPE | PPS_NALU_TYPE | AUD_NALU_TYPE | SEI_NALU_TYPE
                )
            }

            // FU-A CASE (Fragmentation)
            FUA_NALU_TYPE => {
                if packet.payload.len() < 2 {
                    return false;
                }

                let b1 = packet.payload[1];
                let is_start = (b1 & FU_START_BITMASK) != 0;

                if !is_start {
                    return false;
                }

                let original_type = b1 & NALU_TYPE_BITMASK;

                matches!(
                    original_type,
                    IDR_NALU_TYPE | SPS_NALU_TYPE | PPS_NALU_TYPE | AUD_NALU_TYPE | SEI_NALU_TYPE
                )
            }

            _ => false,
        }
    }

    /// Helper if you want to force-accept SPS/PPS regardless of marker history
    fn contains_sps_or_pps(&self, packet: &RtpPacket) -> bool {
        let b0 = packet.payload[0];
        let nalu_type = b0 & NALU_TYPE_BITMASK;

        match nalu_type {
            SPS_NALU_TYPE | PPS_NALU_TYPE => true,
            STAPA_NALU_TYPE => {
                // Scan the STAP-A for ANY SPS/PPS
                let mut curr_offset = STAPA_HEADER_SIZE;
                while curr_offset + 1 < packet.payload.len() {
                    let nalu_size = ((packet.payload[curr_offset] as usize) << 8)
                        | packet.payload[curr_offset + 1] as usize;
                    curr_offset += STAPA_NALU_LENGTH_SIZE;
                    if curr_offset >= packet.payload.len() {
                        break;
                    }

                    let t = packet.payload[curr_offset] & NALU_TYPE_BITMASK;
                    if t == SPS_NALU_TYPE || t == PPS_NALU_TYPE {
                        return true;
                    }

                    curr_offset += nalu_size;
                }
                false
            }
            _ => false,
        }
    }
}
