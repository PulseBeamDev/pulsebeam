//! Bounded, bounds-checked IPv4/IPv6 + UDP header walk over the bytes
//! `SkReuseportContext` exposes directly (`data()..data_end()`, which for
//! `SK_REUSEPORT` starts at the IP header, not the payload). Every offset
//! here is fixed or bounded by a header field that is itself bounds-checked
//! before use — the eBPF verifier proves termination and in-bounds access
//! from that, the same discipline `pulsebeam-routing` documents for its own
//! parsing.

use aya_ebpf::programs::SkReuseportContext;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86DD;
const IPPROTO_UDP: u8 = 17;

const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV4_MAX_HEADER_LEN: usize = 60;
const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;

/// Bound on how many payload bytes we ever hand to the classifier. STUN
/// bootstrap messages and the fixed Envelope both fit comfortably inside
/// this; anything past it is truncated, not read out of bounds.
pub const MAX_PAYLOAD: usize = 512;

#[derive(Clone, Copy)]
pub struct FlowKey {
    pub src_addr: [u8; 16],
    pub dst_addr: [u8; 16],
    pub src_port: u16,
    pub dst_port: u16,
    pub is_ipv6: u8,
    pub _pad: [u8; 3],
}

const _: () = assert!(core::mem::size_of::<FlowKey>() == 36);

pub struct UdpPacket {
    pub flow: FlowKey,
    payload: [u8; MAX_PAYLOAD],
    payload_len: usize,
}

impl UdpPacket {
    pub fn payload(&self) -> &[u8] {
        match self.payload.get(..self.payload_len) {
            Some(slice) => slice,
            None => {
                debug_assert!(false, "payload_len must never exceed MAX_PAYLOAD");
                &[]
            }
        }
    }
}

fn packet_bytes(ctx: &SkReuseportContext) -> Option<(usize, usize)> {
    let start = ctx.data();
    let end = ctx.data_end();
    if end < start {
        return None;
    }
    Some((start, end))
}

fn read_at(start: usize, end: usize, offset: usize, buf: &mut [u8]) -> Option<()> {
    let read_start = start.checked_add(offset)?;
    let read_end = read_start.checked_add(buf.len())?;
    if read_end > end {
        return None;
    }
    let src = unsafe { core::slice::from_raw_parts(read_start as *const u8, buf.len()) };
    buf.copy_from_slice(src);
    Some(())
}

fn eth_protocol_host_order(ctx: &SkReuseportContext) -> u16 {
    // `eth_protocol()` reports the raw network-byte-order value; normalize
    // to host order once so every comparison below reads naturally.
    u16::from_be(ctx.eth_protocol() as u16)
}

pub fn parse_udp(ctx: &SkReuseportContext) -> Option<UdpPacket> {
    let (start, end) = packet_bytes(ctx)?;
    match eth_protocol_host_order(ctx) {
        ETH_P_IP => parse_udp_v4(start, end),
        ETH_P_IPV6 => parse_udp_v6(start, end),
        _ => None,
    }
}

fn parse_udp_v4(start: usize, end: usize) -> Option<UdpPacket> {
    let mut ver_ihl = [0u8; 1];
    read_at(start, end, 0, &mut ver_ihl)?;
    let ihl = usize::from(ver_ihl[0] & 0x0F) * 4;
    if ihl < IPV4_MIN_HEADER_LEN || ihl > IPV4_MAX_HEADER_LEN {
        return None;
    }

    let mut proto = [0u8; 1];
    read_at(start, end, 9, &mut proto)?;
    if proto[0] != IPPROTO_UDP {
        return None;
    }

    let mut src4 = [0u8; 4];
    let mut dst4 = [0u8; 4];
    read_at(start, end, 12, &mut src4)?;
    read_at(start, end, 16, &mut dst4)?;

    let mut src_addr = [0u8; 16];
    let mut dst_addr = [0u8; 16];
    src_addr.get_mut(..4)?.copy_from_slice(&src4);
    dst_addr.get_mut(..4)?.copy_from_slice(&dst4);

    finish_udp(start, end, ihl, FlowKey {
        src_addr,
        dst_addr,
        src_port: 0,
        dst_port: 0,
        is_ipv6: 0,
        _pad: [0; 3],
    })
}

fn parse_udp_v6(start: usize, end: usize) -> Option<UdpPacket> {
    let mut next_header = [0u8; 1];
    read_at(start, end, 6, &mut next_header)?;
    if next_header[0] != IPPROTO_UDP {
        return None;
    }

    let mut src_addr = [0u8; 16];
    let mut dst_addr = [0u8; 16];
    read_at(start, end, 8, &mut src_addr)?;
    read_at(start, end, 24, &mut dst_addr)?;

    finish_udp(start, end, IPV6_HEADER_LEN, FlowKey {
        src_addr,
        dst_addr,
        src_port: 0,
        dst_port: 0,
        is_ipv6: 1,
        _pad: [0; 3],
    })
}

fn finish_udp(start: usize, end: usize, l4_offset: usize, mut flow: FlowKey) -> Option<UdpPacket> {
    let mut ports = [0u8; 4];
    read_at(start, end, l4_offset, &mut ports)?;
    flow.src_port = u16::from_be_bytes([ports[0], ports[1]]);
    flow.dst_port = u16::from_be_bytes([ports[2], ports[3]]);

    let payload_offset = l4_offset.checked_add(UDP_HEADER_LEN)?;
    let available = end.checked_sub(start.checked_add(payload_offset)?)?;
    let payload_len = core::cmp::min(available, MAX_PAYLOAD);

    let mut payload = [0u8; MAX_PAYLOAD];
    if payload_len > 0 {
        read_at(start, end, payload_offset, payload.get_mut(..payload_len)?)?;
    }

    Some(UdpPacket {
        flow,
        payload,
        payload_len,
    })
}
