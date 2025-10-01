use crate::entity::ParticipantId;
use crate::gateway::ice;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

const PROBE_THRESHOLD: u8 = 2;

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
            return PacketType::Dtls;
        }
        let version = byte >> 6;
        if version == 2 {
            return PacketType::Rtp;
        }
        PacketType::Unknown
    }
}

pub struct Demuxer {
    ice_ufrag_map: HashMap<Box<[u8]>, Arc<ParticipantId>>,
    ssrc_map: HashMap<u32, Arc<ParticipantId>>,
    addr_cache: HashMap<SocketAddr, Arc<ParticipantId>>,
    participant_ufrag: HashMap<Arc<ParticipantId>, Box<[u8]>>,
    participant_ssrcs: HashMap<Arc<ParticipantId>, Vec<u32>>,
    authenticated: HashSet<Arc<ParticipantId>>,
    ssrc_probe: HashMap<(SocketAddr, u32), u8>,
    ssrc_probe_owner: HashMap<u32, Arc<ParticipantId>>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            ice_ufrag_map: HashMap::new(),
            ssrc_map: HashMap::new(),
            addr_cache: HashMap::new(),
            participant_ufrag: HashMap::new(),
            participant_ssrcs: HashMap::new(),
            authenticated: HashSet::new(),
            ssrc_probe: HashMap::new(),
            ssrc_probe_owner: HashMap::new(),
        }
    }

    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], participant_id: Arc<ParticipantId>) {
        let boxed = ufrag.to_vec().into_boxed_slice();
        self.ice_ufrag_map
            .insert(boxed.clone(), participant_id.clone());
        self.participant_ufrag.insert(participant_id, boxed);
    }

    pub fn register_ssrc(&mut self, ssrc: u32, participant_id: Arc<ParticipantId>) {
        self.ssrc_map.insert(ssrc, participant_id.clone());
        self.participant_ssrcs
            .entry(participant_id)
            .or_default()
            .push(ssrc);
    }

    pub fn unregister(&mut self, participant_id: Arc<ParticipantId>) {
        if let Some(ufrag) = self.participant_ufrag.remove(&participant_id) {
            self.ice_ufrag_map.remove(&ufrag);
        }
        if let Some(ssrcs) = self.participant_ssrcs.remove(&participant_id) {
            for s in ssrcs {
                self.ssrc_map.remove(&s);
                self.ssrc_probe_owner.remove(&s);
            }
        }
        self.addr_cache.retain(|_, h| *h != participant_id);
        self.authenticated.remove(&participant_id);
        self.ssrc_probe.retain(|(_, _), _| true);
    }

    pub fn mark_transport_authenticated(&mut self, participant: Arc<ParticipantId>) {
        self.authenticated.insert(participant);
    }

    pub fn demux(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if data.len() < 12 {
            return DemuxResult::Rejected(RejectionReason::PacketTooSmall);
        }

        match PacketType::from_first_byte(data[0]) {
            PacketType::Stun => self.demux_stun(src, data),
            PacketType::Rtp | PacketType::Rtcp => self.demux_rtp(src, data),
            PacketType::Dtls => {
                if let Some(participant) = self.addr_cache.get(&src) {
                    DemuxResult::Participant(participant.clone())
                } else {
                    DemuxResult::Rejected(RejectionReason::UnknownSource)
                }
            }
            PacketType::Unknown => DemuxResult::Rejected(RejectionReason::InvalidPacket),
        }
    }

    fn demux_stun(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ufrag) = ice::parse_stun_remote_ufrag_raw(data) {
            if let Some(participant) = self.ice_ufrag_map.get(ufrag) {
                let participant = participant.clone();
                self.addr_cache.insert(src, participant.clone());
                return DemuxResult::Participant(participant);
            }
            return DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag);
        }
        DemuxResult::Rejected(RejectionReason::MalformedStun)
    }

    fn demux_rtp(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        let is_rtcp = data[1] >= 200 && data[1] <= 204;

        let ssrc = match extract_ssrc_variant(data, is_rtcp) {
            Some(s) => s,
            None => return DemuxResult::Rejected(RejectionReason::InvalidPacket),
        };

        // Collision check with registered SSRCs
        if let Some(owner) = self.ssrc_map.get(&ssrc) {
            let participant = match self.addr_cache.get(&src) {
                Some(p) => p.clone(),
                None => return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc),
            };
            if Arc::ptr_eq(owner, &participant) {
                self.addr_cache.insert(src, participant.clone());
                return DemuxResult::Participant(participant);
            } else {
                return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc);
            }
        }

        // Participant from address cache
        let participant = match self.addr_cache.get(&src) {
            Some(p) => p.clone(),
            None => return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc),
        };

        if !self.authenticated.contains(&participant) {
            return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc);
        }

        // Collision check with probe owner
        if let Some(probe_owner) = self.ssrc_probe_owner.get(&ssrc) {
            if !Arc::ptr_eq(probe_owner, &participant) {
                return DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc);
            }
        }

        // Immediate binding for RTCP Sender Report
        if is_rtcp && data[1] == 200 {
            self.register_ssrc(ssrc, participant.clone());
            self.addr_cache.insert(src, participant.clone());
            return DemuxResult::Participant(participant);
        }

        // Probe logic for RTP
        let key = (src, ssrc);
        let count = self.ssrc_probe.entry(key).or_insert(0);
        *count = count.saturating_add(1);
        self.ssrc_probe_owner
            .entry(ssrc)
            .or_insert_with(|| participant.clone());

        if *count >= PROBE_THRESHOLD {
            self.register_ssrc(ssrc, participant.clone());
            self.ssrc_probe.remove(&(src, ssrc));
            self.ssrc_probe_owner.remove(&ssrc);
            self.addr_cache.insert(src, participant.clone());
            return DemuxResult::Participant(participant);
        }

        DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
    }
}

/// Extract SSRC from RTP or RTCP
fn extract_ssrc_variant(data: &[u8], is_rtcp: bool) -> Option<u32> {
    if is_rtcp {
        if data.len() >= 8 {
            Some(u32::from_be_bytes([data[4], data[5], data[6], data[7]]))
        } else {
            None
        }
    } else if data.len() >= 12 {
        Some(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn make_rtp_packet(ssrc: u32) -> Vec<u8> {
        let mut pkt = vec![0x80, 0x00];
        pkt.extend_from_slice(&[0, 0]);
        pkt.extend_from_slice(&[0, 0, 0, 0]);
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(&[0; 12]);
        pkt
    }

    fn make_rtcp_sr(ssrc: u32) -> Vec<u8> {
        let mut pkt = vec![0x80, 200]; // version=2, PT=200 SR
        pkt.extend_from_slice(&[0, 6]); // length field
        pkt.extend_from_slice(&ssrc.to_be_bytes()); // SSRC
        pkt.extend_from_slice(&[0; 24]);
        pkt
    }

    #[test]
    fn test_rtp_learning_with_auth() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        let addr = make_addr(5000);
        d.addr_cache.insert(addr, h.clone());
        d.mark_transport_authenticated(h.clone());

        let pkt = make_rtp_packet(1234);

        // first packet rejected (probe)
        assert_eq!(
            d.demux(addr, &pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );

        // second packet accepted & bound
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h.clone()));

        // third packet accepted immediately
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h));
    }

    #[test]
    fn test_rtcp_sr_immediate_learning() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        let addr = make_addr(5001);
        d.addr_cache.insert(addr, h.clone());
        d.mark_transport_authenticated(h.clone());

        let pkt = make_rtcp_sr(4242);

        // should be accepted immediately
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h.clone()));
        // subsequent RTP with same SSRC should also pass
        let rtp = make_rtp_packet(4242);
        assert_eq!(d.demux(addr, &rtp), DemuxResult::Participant(h));
    }

    #[test]
    fn test_collision_rejected() {
        let mut d = Demuxer::new();
        let h1 = test_utils::create_participant_id();
        let h2 = test_utils::create_participant_id();
        let addr1 = make_addr(5002);
        let addr2 = make_addr(5003);

        d.addr_cache.insert(addr1, h1.clone());
        d.addr_cache.insert(addr2, h2.clone());
        d.mark_transport_authenticated(h1.clone());
        d.mark_transport_authenticated(h2.clone());

        let pkt1 = make_rtp_packet(5555);
        let pkt2 = make_rtp_packet(5555);

        // h1 learns SSRC first
        d.demux(addr1, &pkt1);
        d.demux(addr1, &pkt1); // bound now

        // h2 tries to claim same SSRC
        assert_eq!(
            d.demux(addr2, &pkt2),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );
    }

    #[test]
    fn test_reject_without_auth() {
        let mut d = Demuxer::new();
        let h = test_utils::create_participant_id();
        let addr = make_addr(5004);
        d.addr_cache.insert(addr, h.clone());

        let pkt = make_rtp_packet(9999);

        assert_eq!(
            d.demux(addr, &pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );
    }
}
