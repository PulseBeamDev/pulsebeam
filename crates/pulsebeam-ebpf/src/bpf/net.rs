//! Bounded, bounds-checked IPv4/IPv6 + UDP header walk over the bytes
//! `SkReuseportContext` exposes directly (`data()..data_end()`, which for
//! `SK_REUSEPORT` starts at the IP header, not the payload). Every offset
//! here is fixed or bounded by a header field that is itself bounds-checked
//! before use — the eBPF verifier proves termination and in-bounds access
//! from that, the same discipline `pulsebeam-routing` documents for its own
//! parsing.

use aya_ebpf::programs::SkReuseportContext;
use pulsebeam_routing::steer::FlowKey;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86DD;
const IPPROTO_UDP: u8 = 17;
const IPPROTO_HOPOPT: u8 = 0;
const IPPROTO_ROUTING: u8 = 43;
const IPPROTO_FRAGMENT: u8 = 44;
const IPPROTO_AH: u8 = 51;
const IPPROTO_DEST: u8 = 60;

const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV4_MAX_HEADER_LEN: usize = 60;
const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;
const MAX_IPV6_EXTENSION_HEADERS: usize = 8;

/// Bound on how many payload bytes we ever hand to the classifier. STUN
/// bootstrap messages and the fixed Envelope both fit comfortably inside
/// this; anything past it is truncated, not read out of bounds.
pub const MAX_PAYLOAD: usize = 512;

pub struct UdpPacket<'a> {
    pub flow: FlowKey,
    payload: &'a [u8],
}

impl UdpPacket<'_> {
    pub fn payload(&self) -> &[u8] {
        self.payload
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
    let read_start = start + offset;
    let read_end = read_start + buf.len();

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

pub fn parse_udp<'a>(ctx: &'a SkReuseportContext) -> Option<UdpPacket<'a>> {
    let (start, end) = packet_bytes(ctx)?;
    match eth_protocol_host_order(ctx) {
        ETH_P_IP => parse_udp_v4(ctx, start, end),
        ETH_P_IPV6 => parse_udp_v6(ctx, start, end),
        _ => None,
    }
}

fn parse_udp_v4<'a>(
    ctx: &'a SkReuseportContext,
    start: usize,
    end: usize,
) -> Option<UdpPacket<'a>> {
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

    let mut fragment = [0u8; 2];
    read_at(start, end, 6, &mut fragment)?;
    if u16::from_be_bytes(fragment) & 0x3FFF != 0 {
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

    finish_udp(
        ctx,
        start,
        end,
        ihl,
        FlowKey {
            src_addr,
            dst_addr,
            src_port: 0,
            dst_port: 0,
            is_ipv6: 0,
            _pad: [0; 3],
        },
    )
}

fn parse_udp_v6<'a>(
    ctx: &'a SkReuseportContext,
    start: usize,
    end: usize,
) -> Option<UdpPacket<'a>> {
    let mut next_header = [0u8; 1];
    read_at(start, end, 6, &mut next_header)?;

    let mut src_addr = [0u8; 16];
    let mut dst_addr = [0u8; 16];
    read_at(start, end, 8, &mut src_addr)?;
    read_at(start, end, 24, &mut dst_addr)?;

    let mut l4_offset = IPV6_HEADER_LEN;
    let mut extensions = 0;
    while next_header[0] != IPPROTO_UDP {
        if extensions >= MAX_IPV6_EXTENSION_HEADERS {
            return None;
        }
        extensions += 1;

        let header_len = match next_header[0] {
            IPPROTO_HOPOPT | IPPROTO_ROUTING | IPPROTO_DEST => {
                let mut length = [0u8; 1];
                read_at(start, end, l4_offset + 1, &mut length)?;
                (usize::from(length[0]) + 1).checked_mul(8)?
            }
            IPPROTO_AH => {
                let mut length = [0u8; 1];
                read_at(start, end, l4_offset + 1, &mut length)?;
                (usize::from(length[0]) + 2).checked_mul(4)?
            }
            IPPROTO_FRAGMENT => return None,
            _ => return None,
        };
        let mut next = [0u8; 1];
        read_at(start, end, l4_offset, &mut next)?;
        l4_offset = l4_offset.checked_add(header_len)?;
        debug_assert!(l4_offset <= end.saturating_sub(start));
        next_header = next;
    }

    finish_udp(
        ctx,
        start,
        end,
        l4_offset,
        FlowKey {
            src_addr,
            dst_addr,
            src_port: 0,
            dst_port: 0,
            is_ipv6: 1,
            _pad: [0; 3],
        },
    )
}

fn finish_udp<'a>(
    _ctx: &'a SkReuseportContext,
    start: usize,
    end: usize,
    l4_offset: usize,
    mut flow: FlowKey,
) -> Option<UdpPacket<'a>> {
    let mut ports = [0u8; 4];
    read_at(start, end, l4_offset, &mut ports)?;

    flow.src_port = u16::from_be_bytes([ports[0], ports[1]]);
    flow.dst_port = u16::from_be_bytes([ports[2], ports[3]]);

    if end < start {
        return None;
    }

    // From this point onward, keep everything as scalar offsets/lengths.
    let packet_len = end - start;

    let payload_offset = l4_offset.checked_add(UDP_HEADER_LEN)?;
    if payload_offset > packet_len {
        return None;
    }

    let available = packet_len - payload_offset;
    let payload_len = core::cmp::min(available, MAX_PAYLOAD);

    // Only after proving the offset is inside the packet do we reconstruct
    // the packet pointer.
    let payload_start = start + payload_offset;

    // Give the verifier an explicit direct-packet-access bound.
    let payload_end = payload_start + payload_len;
    if payload_end > end {
        return None;
    }

    let payload = unsafe { core::slice::from_raw_parts(payload_start as *const u8, payload_len) };

    Some(UdpPacket { flow, payload })
}
