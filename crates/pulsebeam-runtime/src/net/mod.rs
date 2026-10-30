#[cfg(feature = "sim")]
#[path = "bound_udp_sim.rs"]
mod bound_udp;
#[cfg(feature = "sim")]
pub mod shaper;
#[cfg(feature = "sim")]
mod udp_sim;
#[cfg(feature = "sim")]
pub use bound_udp::{bind_udp_socket, install_steering_flow, set_wrong_owner_injection};

pub use pulsebeam_udp::{BATCH_SIZE, ReceiveTime};
use std::net::SocketAddr;

pub const CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_UDP_PAYLOAD_SIZE: usize = 1500;
pub const MAX_UDP_GSO_PAYLOAD_SIZE: usize = u16::MAX as usize;
pub const UDP_MAX_GSO_SEGMENTS: usize = pulsebeam_udp::MAX_GSO_SEGMENTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UdpMode {
    /// Standard `send_to` (Low latency, lower throughput)
    Scalar,
    /// Batch `sendmmsg`/GSO (Higher throughput, slight buffering latency)
    Batch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    Udp(UdpMode),
}

#[derive(Debug, Clone)]
pub struct RecvPacketBatch {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub buf: Vec<u8>,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,
    pub received_at: ReceiveTime,

    pub offset: usize,
}

impl RecvPacketBatch {
    /// Returns the exact byte slice for this packet (accounts for `offset`).
    #[inline]
    pub fn data(&self) -> &[u8] {
        debug_assert!(self.len <= self.buf.len());
        self.buf.get(..self.len).unwrap_or_default()
    }

    // https://stackoverflow.com/questions/68606470/how-to-return-a-reference-when-implementing-an-iterator
    // GAT is not stabilized, so it's not possible to return a reference in an iterator yet.
    #[inline]
    pub fn next_packet(&mut self) -> Option<&[u8]> {
        debug_assert_ne!(self.stride, 0);
        debug_assert!(self.len <= self.buf.len());
        debug_assert!(self.stride <= self.len.max(1));
        debug_assert!(self.offset <= self.len);
        if self.offset == self.len {
            return None;
        }
        let tail = self.len.min(self.offset.saturating_add(self.stride));
        debug_assert!(tail > self.offset);

        let buf = self.buf.get(self.offset..tail)?;
        self.offset = tail;
        Some(buf)
    }
}

/// A single outbound packet entry inside a batch.
#[derive(Debug, Clone, Copy)]
pub struct SendPacket<'a> {
    pub dst: SocketAddr,
    pub buf: &'a [u8],
    /// GSO segment size. If `segment_size < buf.len()`, the kernel splits
    /// `buf` into `segment_size`-sized datagrams to `dst` via UDP_SEGMENT.
    /// If `segment_size == buf.len()` (or `buf` is empty), no GSO is applied
    /// and this is a single ordinary datagram.
    pub segment_size: usize,
}

/// A multi-packet, multi-destination batch for sendmmsg.
///
/// Packets do not need to share a destination or segment size — the backend
/// groups contiguous runs that share a `segment_size` into individual
/// `sendmmsg()` calls (GSO segment size is applied per-call, not per-packet).
/// For best syscall efficiency, callers should sort/group packets by
/// `segment_size` before building a batch.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendPacketBatch<'a> {
    pub packets: &'a [SendPacket<'a>],
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;
    use std::net::SocketAddr;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:1234".parse().unwrap()
    }

    #[test]
    fn recv_packet_batch_iter_yields_multiple_segments_without_off_by_one() {
        let mut batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: (0u8..20).collect(),
            stride: 6,
            len: 20,
            transport: Transport::Udp(UdpMode::Scalar),
            received_at: ReceiveTime::now(),
            offset: 0,
        };

        assert_eq!(batch.next_packet(), Some(&[0, 1, 2, 3, 4, 5][..]));
        assert_eq!(batch.next_packet(), Some(&[6, 7, 8, 9, 10, 11][..]));
        assert_eq!(batch.next_packet(), Some(&[12, 13, 14, 15, 16, 17][..]));
        assert_eq!(batch.next_packet(), Some(&[18, 19][..]));
        assert_eq!(batch.next_packet(), None);
    }

    #[test]
    fn recv_packet_batch_iter_yields_multiple_segments_with_exact_len() {
        let mut batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: (0u8..20).collect(),
            stride: 6,
            len: 18,
            transport: Transport::Udp(UdpMode::Scalar),
            received_at: ReceiveTime::now(),
            offset: 0,
        };

        assert_eq!(batch.next_packet(), Some(&[0, 1, 2, 3, 4, 5][..]));
        assert_eq!(batch.next_packet(), Some(&[6, 7, 8, 9, 10, 11][..]));
        assert_eq!(batch.next_packet(), Some(&[12, 13, 14, 15, 16, 17][..]));
        assert_eq!(batch.next_packet(), None);
    }

    #[test]
    fn recv_packet_batch_handles_receive_arena_tail_and_empty_payload() {
        let mut batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: vec![1, 2, 3, 4],
            stride: 2,
            len: 2,
            transport: Transport::Udp(UdpMode::Batch),
            received_at: ReceiveTime::now(),
            offset: 0,
        };

        assert_eq!(batch.data(), &[1, 2]);
        batch.buf.clear();
        batch.len = 0;
        batch.stride = 1;
        assert!(batch.data().is_empty());
        assert_eq!(batch.next_packet(), None);
        assert_eq!(batch.next_packet(), None);
    }
}
