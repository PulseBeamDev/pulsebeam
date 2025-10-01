use crate::entity::ParticipantId;
use crate::gateway::ice;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxResult {
    Participant(Arc<ParticipantId>),
    Rejected(RejectionReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// Packet received from an unknown source address that was not a STUN binding request.
    UnknownSource,
    /// Packet is not a valid STUN, DTLS, RTP, or RTCP packet.
    InvalidPacket,
    /// STUN packet could not be parsed.
    MalformedStun,
    /// STUN packet had a valid format but an unknown USERNAME attribute.
    UnauthorizedIceUfrag,
    /// Packet was too small to be processed.
    PacketTooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketType {
    Stun,
    Dtls,
    RtpOrRtcp,
    Unknown,
}

impl PacketType {
    /// Determines the packet type from the first byte of the packet.
    #[inline(always)]
    fn from_first_byte(byte: u8) -> Self {
        if byte <= 0x01 {
            // STUN messages have 0b00 as the first two bits.
            return PacketType::Stun;
        }
        if byte >= 20 && byte <= 64 {
            // DTLS messages occupy this range.
            return PacketType::Dtls;
        }
        let version = byte >> 6;
        if version == 2 {
            // RTP/RTCP messages have 0b10 as the first two bits (version 2).
            return PacketType::RtpOrRtcp;
        }
        PacketType::Unknown
    }
}

/// A UDP demuxer that maps packets to participants based on source address and STUN ufrag.
///
/// This implementation uses two primary mechanisms for routing incoming UDP packets:
/// 1. A fast-path map from `SocketAddr` to `ParticipantId` (`addr_map`). This provides
///    efficient routing for known addresses.
/// 2. For packets from unknown addresses, it inspects STUN messages to learn the `ufrag`.
///    The `ufrag` is used to identify the participant, and the source address is then
///    added to the address map for future fast-path lookups.
///
/// Non-STUN packets from unknown addresses are rejected.
pub struct Demuxer {
    /// Maps a remote ICE username fragment (ufrag) to a participant.
    ufrag_map: HashMap<Box<[u8]>, Arc<ParticipantId>>,
    /// A cache mapping a remote `SocketAddr` to a known participant. This is the fast path.
    addr_map: HashMap<SocketAddr, Arc<ParticipantId>>,
    /// A reverse map to efficiently clean up `ufrag_map` when a participant is unregistered.
    participant_ufrag: HashMap<Arc<ParticipantId>, Box<[u8]>>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            ufrag_map: HashMap::new(),
            addr_map: HashMap::new(),
            participant_ufrag: HashMap::new(),
        }
    }

    /// Registers a participant with their ICE username fragment.
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], participant_id: Arc<ParticipantId>) {
        let boxed_ufrag = ufrag.to_vec().into_boxed_slice();
        self.ufrag_map
            .insert(boxed_ufrag.clone(), participant_id.clone());
        self.participant_ufrag.insert(participant_id, boxed_ufrag);
    }

    /// Removes a participant and all associated state (ufrag and address mappings).
    pub fn unregister(&mut self, participant_id: &Arc<ParticipantId>) {
        if let Some(ufrag) = self.participant_ufrag.remove(participant_id) {
            self.ufrag_map.remove(&ufrag);
        }
        // Remove all address entries pointing to this participant.
        self.addr_map.retain(|_, p| !Arc::ptr_eq(p, participant_id));
    }

    /// Determines the owner of an incoming UDP packet.
    pub fn demux(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if data.is_empty() {
            return DemuxResult::Rejected(RejectionReason::PacketTooSmall);
        }

        // STUN packets must always be processed by the slow path to allow for address re-mapping.
        // Other packet types can use the fast path if the address is already known.
        match PacketType::from_first_byte(data[0]) {
            PacketType::Stun => self.demux_stun(src, data),
            PacketType::Dtls | PacketType::RtpOrRtcp => {
                // This is the fast path for non-STUN packets.
                if let Some(participant) = self.addr_map.get(&src) {
                    DemuxResult::Participant(participant.clone())
                } else {
                    DemuxResult::Rejected(RejectionReason::UnknownSource)
                }
            }
            PacketType::Unknown => DemuxResult::Rejected(RejectionReason::InvalidPacket),
        }
    }

    /// Handles STUN packets to identify participants and learn their address.
    fn demux_stun(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ufrag) = ice::parse_stun_remote_ufrag_raw(data) {
            if let Some(participant) = self.ufrag_map.get(ufrag) {
                let participant = participant.clone();
                // Address learned or re-assigned, add it to the map for the fast path next time.
                self.addr_map.insert(src, participant.clone());
                return DemuxResult::Participant(participant);
            }
            // Valid STUN but ufrag is not registered with us.
            return DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag);
        }
        // Not a valid STUN packet with a USERNAME attribute.
        DemuxResult::Rejected(RejectionReason::MalformedStun)
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils; // Assuming a test utility for creating participant IDs.
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    /// Manually constructs a STUN binding request packet with a USERNAME attribute.
    /// This avoids the need for an external STUN crate in the tests.
    fn make_stun_packet(ufrag: &str) -> Vec<u8> {
        // The USERNAME attribute value, typically formatted as "remote_ufrag:local_ufrag".
        // For testing, we only need the remote ufrag part that the demuxer expects.
        let username_value = format!("{}:remote", ufrag);
        let value_bytes = username_value.as_bytes();
        let value_len = value_bytes.len();

        // Attribute value must be padded to a multiple of 4 bytes.
        let padded_value_len = (value_len + 3) & !3;
        let padding_len = padded_value_len - value_len;

        // Total attribute length (header + padded value).
        let attr_total_len = 4 + padded_value_len;

        // STUN Message Header (20 bytes).
        let msg_type: u16 = 0x0001; // Binding Request
        let msg_len: u16 = attr_total_len as u16; // Length of attributes section
        let magic_cookie: u32 = 0x2112A442;
        let transaction_id: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

        // USERNAME Attribute (Type-Length-Value).
        let attr_type: u16 = 0x0006; // USERNAME
        let attr_value_len: u16 = value_len as u16;

        // Assemble the packet.
        let mut packet = Vec::with_capacity(20 + attr_total_len);
        packet.extend_from_slice(&msg_type.to_be_bytes());
        packet.extend_from_slice(&msg_len.to_be_bytes());
        packet.extend_from_slice(&magic_cookie.to_be_bytes());
        packet.extend_from_slice(&transaction_id);
        packet.extend_from_slice(&attr_type.to_be_bytes());
        packet.extend_from_slice(&attr_value_len.to_be_bytes());
        packet.extend_from_slice(value_bytes);
        packet.extend_from_slice(&vec![0; padding_len]);

        packet
    }

    fn make_rtp_packet(ssrc: u32) -> Vec<u8> {
        let mut pkt = vec![0x80, 0x00, 0, 0, 0, 0, 0, 0];
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(&[0; 4]); // Payload
        pkt
    }

    #[test]
    fn test_demux_by_stun_then_rtp() {
        let mut d = Demuxer::new();
        let p1 = test_utils::create_participant_id();
        let addr1 = make_addr(5000);
        let ufrag1 = "ufrag-1";

        d.register_ice_ufrag(ufrag1.as_bytes(), p1.clone());

        // 1. Packet from unknown source, but it's STUN. Should be accepted and address learned.
        let stun_pkt = make_stun_packet(ufrag1);
        assert_eq!(
            d.demux(addr1, &stun_pkt),
            DemuxResult::Participant(p1.clone())
        );

        // 2. Now that the address is learned, a subsequent RTP packet should be accepted on the fast path.
        let rtp_pkt = make_rtp_packet(12345);
        assert_eq!(
            d.demux(addr1, &rtp_pkt),
            DemuxResult::Participant(p1.clone())
        );
    }

    #[test]
    fn test_reject_rtp_from_unknown_source() {
        let mut d = Demuxer::new();
        let p1 = test_utils::create_participant_id();
        let addr1 = make_addr(5001);
        let ufrag1 = "ufrag-2";

        d.register_ice_ufrag(ufrag1.as_bytes(), p1.clone());

        // An RTP packet arrives from an address we haven't learned via STUN yet.
        // It should be rejected.
        let rtp_pkt = make_rtp_packet(54321);
        assert_eq!(
            d.demux(addr1, &rtp_pkt),
            DemuxResult::Rejected(RejectionReason::UnknownSource)
        );
    }

    #[test]
    fn test_reject_unauthorized_ufrag() {
        let mut d = Demuxer::new();
        let addr = make_addr(5002);
        let stun_pkt = make_stun_packet("unregistered-ufrag");

        assert_eq!(
            d.demux(addr, &stun_pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag)
        );
    }

    #[test]
    fn test_unregister_participant() {
        let mut d = Demuxer::new();
        let p1 = test_utils::create_participant_id();
        let addr1 = make_addr(5003);
        let ufrag1 = "ufrag-3";

        d.register_ice_ufrag(ufrag1.as_bytes(), p1.clone());

        // Learn the address via STUN.
        let stun_pkt = make_stun_packet(ufrag1);
        assert_eq!(
            d.demux(addr1, &stun_pkt),
            DemuxResult::Participant(p1.clone())
        );
        assert_eq!(d.addr_map.len(), 1);
        assert_eq!(d.ufrag_map.len(), 1);

        // Unregister the participant.
        d.unregister(&p1);

        // Both the ufrag and the address mapping should be gone.
        assert!(d.ufrag_map.is_empty());
        assert!(d.addr_map.is_empty());
        assert!(d.participant_ufrag.is_empty());

        // A new packet from the same address should now be rejected.
        assert_eq!(
            d.demux(addr1, &stun_pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag)
        );
    }

    #[test]
    fn test_address_reassignment() {
        let mut d = Demuxer::new();
        let p1 = test_utils::create_participant_id();
        let p2 = test_utils::create_participant_id();
        let addr = make_addr(5004);
        let ufrag1 = "ufrag-p1";
        let ufrag2 = "ufrag-p2";

        d.register_ice_ufrag(ufrag1.as_bytes(), p1.clone());
        d.register_ice_ufrag(ufrag2.as_bytes(), p2.clone());

        // P1 sends a STUN packet, its address is mapped.
        let stun1 = make_stun_packet(ufrag1);
        assert_eq!(d.demux(addr, &stun1), DemuxResult::Participant(p1.clone()));
        assert_eq!(d.addr_map.get(&addr).unwrap(), &p1);

        // Now, P2 sends a STUN packet from the *same* address (e.g., behind the same NAT).
        // The demuxer should re-map the address to P2.
        let stun2 = make_stun_packet(ufrag2);
        assert_eq!(d.demux(addr, &stun2), DemuxResult::Participant(p2.clone()));

        // Verify that the map was updated.
        assert_eq!(d.addr_map.get(&addr).unwrap(), &p2);

        // An RTP packet should now be routed to P2.
        let rtp = make_rtp_packet(999);
        assert_eq!(d.demux(addr, &rtp), DemuxResult::Participant(p2));
    }
}
