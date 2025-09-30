use crate::entity::ParticipantId;
use crate::gateway::ice;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Demux result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxResult {
    Participant(Arc<ParticipantId>),
    Rejected(RejectionReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    UnknownSource,
    InvalidPacket,
    MalformedStun,
    UnauthorizedIceUfrag,
    UnauthorizedSsrc,
    PacketTooSmall,
}

/// Packet classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketType {
    Stun,
    Rtp,
    Rtcp,
    Dtls,
    Unknown,
}

impl PacketType {
    #[inline(always)]
    fn from_first_byte(byte: u8) -> Self {
        if byte <= 0x01 {
            return PacketType::Stun;
        }
        if byte >= 20 && byte <= 64 {
            return PacketType::Dtls; // handshake, alert, change_cipher_spec, app data
        }
        let version = byte >> 6;
        if version == 2 {
            return PacketType::Rtp; // SSRC distinguishes RTP vs RTCP
        }
        PacketType::Unknown
    }
}

/// Optimized single-threaded demuxer
pub struct Demuxer {
    ice_ufrag_map: HashMap<Box<[u8]>, Arc<ParticipantId>>,
    ssrc_map: HashMap<u32, Arc<ParticipantId>>,
    addr_cache: HashMap<SocketAddr, Arc<ParticipantId>>,
    participant_ufrag: HashMap<Arc<ParticipantId>, Box<[u8]>>,
    participant_ssrcs: HashMap<Arc<ParticipantId>, Vec<u32>>,
}

impl Demuxer {
    #[inline]
    pub fn new() -> Self {
        Self {
            ice_ufrag_map: HashMap::new(),
            ssrc_map: HashMap::new(),
            addr_cache: HashMap::new(),
            participant_ufrag: HashMap::new(),
            participant_ssrcs: HashMap::new(),
        }
    }

    /// Register ICE ufrag (zero-copy: store as bytes)
    #[inline]
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], paticipant_id: Arc<ParticipantId>) {
        let boxed = ufrag.to_vec().into_boxed_slice();
        self.ice_ufrag_map
            .insert(boxed.clone(), paticipant_id.clone());
        self.participant_ufrag.insert(paticipant_id, boxed);
    }

    #[inline]
    pub fn register_ssrc(&mut self, ssrc: u32, participant_id: Arc<ParticipantId>) {
        self.ssrc_map.insert(ssrc, participant_id.clone());
        self.participant_ssrcs
            .entry(participant_id)
            .or_default()
            .push(ssrc);
    }

    #[inline]
    pub fn unregister(&mut self, participant_id: Arc<ParticipantId>) {
        if let Some(ufrag) = self.participant_ufrag.remove(&participant_id) {
            self.ice_ufrag_map.remove(&ufrag);
        }
        if let Some(ssrcs) = self.participant_ssrcs.remove(&participant_id) {
            for s in ssrcs {
                self.ssrc_map.remove(&s);
            }
        }
        self.addr_cache.retain(|_, h| *h != participant_id);
    }

    /// Public entry point: demux with fast-path cache
    #[inline]
    pub fn demux(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        match self.addr_cache.get(&src) {
            Some(handle) => match self.validate_cached(data, handle.clone()) {
                Some(result) => result,
                None => self.inspect(src, data),
            },
            None => self.inspect(src, data),
        }
    }

    #[inline]
    fn validate_cached(
        &self,
        data: &[u8],
        participant_id: Arc<ParticipantId>,
    ) -> Option<DemuxResult> {
        if data.len() < 12 {
            return Some(DemuxResult::Rejected(RejectionReason::PacketTooSmall));
        }

        match PacketType::from_first_byte(data[0]) {
            PacketType::Stun => {
                let ufrag = ice::parse_stun_remote_ufrag_raw(data)?;
                let expected = self.participant_ufrag.get(&participant_id)?;
                if expected.as_ref() == ufrag {
                    Some(DemuxResult::Participant(participant_id))
                } else {
                    None
                }
            }
            PacketType::Rtp | PacketType::Rtcp => {
                let ssrc = extract_ssrc(data)?;
                let ssrcs = self.participant_ssrcs.get(&participant_id)?;
                if ssrcs.contains(&ssrc) {
                    Some(DemuxResult::Participant(participant_id))
                } else {
                    None
                }
            }
            PacketType::Dtls => {
                // if it’s in the cache, we can accept it for this participant
                Some(DemuxResult::Participant(participant_id))
            }
            PacketType::Unknown => Some(DemuxResult::Rejected(RejectionReason::InvalidPacket)),
        }
    }
    #[inline]
    fn inspect(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if data.len() < 12 {
            return DemuxResult::Rejected(RejectionReason::PacketTooSmall);
        }
        match PacketType::from_first_byte(data[0]) {
            PacketType::Stun => self.demux_stun(src, data),
            PacketType::Rtp | PacketType::Rtcp => self.demux_rtp(src, data),
            PacketType::Dtls => {
                if let Some(h) = self.addr_cache.get(&src) {
                    DemuxResult::Participant(h.clone())
                } else {
                    DemuxResult::Rejected(RejectionReason::UnknownSource)
                }
            }
            PacketType::Unknown => DemuxResult::Rejected(RejectionReason::InvalidPacket),
        }
    }

    #[inline]
    fn demux_stun(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ufrag) = ice::parse_stun_remote_ufrag_raw(data) {
            if let Some(h) = self.ice_ufrag_map.get(ufrag) {
                self.addr_cache.insert(src, h.clone());
                return DemuxResult::Participant(h.clone());
            }
            return DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag);
        }
        DemuxResult::Rejected(RejectionReason::MalformedStun)
    }

    #[inline]
    fn demux_rtp(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ssrc) = extract_ssrc(data) {
            if let Some(h) = self.ssrc_map.get(&ssrc) {
                self.addr_cache.insert(src, h.clone());
                return DemuxResult::Participant(h.clone());
            }
            return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc);
        }
        DemuxResult::Rejected(RejectionReason::InvalidPacket)
    }
}

/// Extract SSRC (RTP=bytes 8..12, RTCP=bytes 4..8)
#[inline(always)]
fn extract_ssrc(data: &[u8]) -> Option<u32> {
    if data.len() >= 12 {
        Some(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils;

    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn make_rtp_packet(ssrc: u32) -> Vec<u8> {
        let mut pkt = vec![0x80, 0x00]; // V=2, PT=0
        pkt.extend_from_slice(&[0, 0]); // seq
        pkt.extend_from_slice(&[0, 0, 0, 0]); // ts
        pkt.extend_from_slice(&ssrc.to_be_bytes()); // SSRC
        pkt.extend_from_slice(&[0; 12]); // small payload
        pkt
    }

    #[test]
    fn test_cache_fast_path() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        d.register_ssrc(12345, h.clone());

        let pkt = make_rtp_packet(12345);
        let addr = make_addr(5000);

        // First: miss, inspects SSRC
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h.clone()));
        // Second: hit, uses cache
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h));
        assert_eq!(d.addr_cache.len(), 1);
    }

    #[test]
    fn test_nat_rebinding() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        d.register_ssrc(42, h.clone());

        let pkt = make_rtp_packet(42);
        let addr1 = make_addr(5000);
        let addr2 = make_addr(6000);

        assert_eq!(d.demux(addr1, &pkt), DemuxResult::Participant(h.clone()));
        assert_eq!(d.demux(addr2, &pkt), DemuxResult::Participant(h.clone()));

        // both addresses now cached
        assert_eq!(d.addr_cache.len(), 2);
    }

    #[test]
    fn test_unauthorized_ssrc() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        d.register_ssrc(12345, h);

        let pkt = make_rtp_packet(99999); // not registered
        let addr = make_addr(5000);

        assert_eq!(
            d.demux(addr, &pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );
        assert!(d.addr_cache.is_empty());
    }

    #[test]
    fn test_unregister() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        d.register_ssrc(11111, h.clone());

        let pkt = make_rtp_packet(11111);
        let addr = make_addr(5000);

        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h.clone()));
        d.unregister(h);

        assert_eq!(
            d.demux(addr, &pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );
        assert!(d.addr_cache.is_empty());
    }
}
