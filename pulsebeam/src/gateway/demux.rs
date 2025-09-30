use std::collections::HashMap;
use std::net::SocketAddr;

/// Participant handle type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParticipantHandle(u64);

impl ParticipantHandle {
    #[inline(always)]
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    #[inline(always)]
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

/// Demux result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemuxResult {
    Participant(ParticipantHandle),
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
    Unknown,
}

impl PacketType {
    #[inline(always)]
    fn from_first_byte(byte: u8) -> Self {
        if byte <= 0x01 {
            return PacketType::Stun;
        }
        let version = byte >> 6;
        if version != 2 {
            return PacketType::Unknown;
        }
        PacketType::Rtp // cheap, RTCP handled by SSRC extractor
    }
}

/// Optimized single-threaded demuxer
pub struct Demuxer {
    ice_ufrag_map: HashMap<Box<[u8]>, ParticipantHandle>,
    ssrc_map: HashMap<u32, ParticipantHandle>,
    addr_cache: HashMap<SocketAddr, ParticipantHandle>,
    participant_ufrag: HashMap<ParticipantHandle, Box<[u8]>>,
    participant_ssrcs: HashMap<ParticipantHandle, Vec<u32>>,
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
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], handle: ParticipantHandle) {
        let boxed = ufrag.to_vec().into_boxed_slice();
        self.ice_ufrag_map.insert(boxed.clone(), handle);
        self.participant_ufrag.insert(handle, boxed);
    }

    #[inline]
    pub fn register_ssrc(&mut self, ssrc: u32, handle: ParticipantHandle) {
        self.ssrc_map.insert(ssrc, handle);
        self.participant_ssrcs.entry(handle).or_default().push(ssrc);
    }

    #[inline]
    pub fn unregister(&mut self, handle: ParticipantHandle) {
        if let Some(ufrag) = self.participant_ufrag.remove(&handle) {
            self.ice_ufrag_map.remove(&ufrag);
        }
        if let Some(ssrcs) = self.participant_ssrcs.remove(&handle) {
            for s in ssrcs {
                self.ssrc_map.remove(&s);
            }
        }
        self.addr_cache.retain(|_, &mut h| h != handle);
    }

    /// Public entry point: demux with fast-path cache
    #[inline]
    pub fn demux(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(&h) = self.addr_cache.get(&src) {
            if let Some(r) = self.validate_cached(data, h) {
                return r;
            }
        }
        self.inspect(src, data)
    }

    #[inline]
    fn validate_cached(&self, data: &[u8], handle: ParticipantHandle) -> Option<DemuxResult> {
        if data.len() < 12 {
            return Some(DemuxResult::Rejected(RejectionReason::PacketTooSmall));
        }
        match PacketType::from_first_byte(data[0]) {
            PacketType::Stun => {
                if let Some(ufrag) = extract_stun_username(data) {
                    if let Some(exp) = self.participant_ufrag.get(&handle) {
                        if exp.as_ref() == ufrag {
                            return Some(DemuxResult::Participant(handle));
                        }
                    }
                }
                None
            }
            PacketType::Rtp | PacketType::Rtcp => {
                if let Some(ssrc) = extract_ssrc(data) {
                    if self
                        .participant_ssrcs
                        .get(&handle)
                        .map_or(false, |v| v.contains(&ssrc))
                    {
                        return Some(DemuxResult::Participant(handle));
                    }
                }
                None
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
            PacketType::Unknown => DemuxResult::Rejected(RejectionReason::InvalidPacket),
        }
    }

    #[inline]
    fn demux_stun(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ufrag) = extract_stun_username(data) {
            if let Some(&h) = self.ice_ufrag_map.get(ufrag) {
                self.addr_cache.insert(src, h);
                return DemuxResult::Participant(h);
            }
            return DemuxResult::Rejected(RejectionReason::UnauthorizedIceUfrag);
        }
        DemuxResult::Rejected(RejectionReason::MalformedStun)
    }

    #[inline]
    fn demux_rtp(&mut self, src: SocketAddr, data: &[u8]) -> DemuxResult {
        if let Some(ssrc) = extract_ssrc(data) {
            if let Some(&h) = self.ssrc_map.get(&ssrc) {
                self.addr_cache.insert(src, h);
                return DemuxResult::Participant(h);
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

/// Extract ICE ufrag from STUN (username attr 0x0006)
#[inline]
fn extract_stun_username<'a>(data: &'a [u8]) -> Option<&'a [u8]> {
    if data.len() < 20 {
        return None;
    }
    if &data[4..8] != &[0x21, 0x12, 0xA4, 0x42] {
        return None;
    }
    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let mut off = 20;
    let end = 20 + msg_len.min(data.len() - 20);
    while off + 4 <= end {
        let attr_type = u16::from_be_bytes([data[off], data[off + 1]]);
        let attr_len = u16::from_be_bytes([data[off + 2], data[off + 3]]) as usize;
        if attr_type == 0x0006 {
            let s = off + 4;
            let e = s + attr_len;
            if e <= data.len() {
                let v = &data[s..e];
                return v.split(|&b| b == b':').next();
            }
            return None;
        }
        off += 4 + ((attr_len + 3) & !3);
    }
    None
}

#[cfg(test)]
mod tests {
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
        let h = ParticipantHandle::new(1);
        d.register_ssrc(12345, h);

        let pkt = make_rtp_packet(12345);
        let addr = make_addr(5000);

        // First: miss, inspects SSRC
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h));
        // Second: hit, uses cache
        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h));
        assert_eq!(d.addr_cache.len(), 1);
    }

    #[test]
    fn test_nat_rebinding() {
        let mut d = Demuxer::new();
        let h = ParticipantHandle::new(1);
        d.register_ssrc(42, h);

        let pkt = make_rtp_packet(42);
        let addr1 = make_addr(5000);
        let addr2 = make_addr(6000);

        assert_eq!(d.demux(addr1, &pkt), DemuxResult::Participant(h));
        assert_eq!(d.demux(addr2, &pkt), DemuxResult::Participant(h));

        // both addresses now cached
        assert_eq!(d.addr_cache.len(), 2);
    }

    #[test]
    fn test_unauthorized_ssrc() {
        let mut d = Demuxer::new();
        let h = ParticipantHandle::new(1);
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
        let h = ParticipantHandle::new(1);
        d.register_ssrc(11111, h);

        let pkt = make_rtp_packet(11111);
        let addr = make_addr(5000);

        assert_eq!(d.demux(addr, &pkt), DemuxResult::Participant(h));
        d.unregister(h);

        assert_eq!(
            d.demux(addr, &pkt),
            DemuxResult::Rejected(RejectionReason::UnauthorizedSsrc)
        );
        assert!(d.addr_cache.is_empty());
    }
}
