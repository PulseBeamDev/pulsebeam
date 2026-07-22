#[cfg_attr(feature = "sim", path = "bound_udp_sim.rs")]
mod bound_udp;
pub mod tcp;
pub mod udp;
pub mod udp_scalar;

pub use bound_udp::{BoundUdpSocket, bind_udp_socket};

use std::{io, net::SocketAddr};

/// Maximum datagrams submitted to one `recvmmsg`/`sendmmsg` syscall.
///
/// This is deliberately the Linux value formerly inherited from `quinn-udp`.
/// Keeping it local makes the runtime independent of Quinn and bounds all
/// per-socket scratch space.
pub const BATCH_SIZE: usize = 32;
pub const CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_UDP_PAYLOAD_SIZE: usize = 1500;
pub const MAX_UDP_GSO_PAYLOAD_SIZE: usize = u16::MAX as usize;

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
    Tcp,
}

#[derive(Debug, Clone)]
pub struct RecvPacketBatch {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub buf: Vec<u8>,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,

    pub offset: usize,
}

impl RecvPacketBatch {
    /// Returns the exact byte slice for this packet (accounts for `offset`).
    #[inline]
    pub fn data(&self) -> &[u8] {
        debug_assert!(self.len <= self.buf.len());
        &self.buf[..self.len]
    }

    // https://stackoverflow.com/questions/68606470/how-to-return-a-reference-when-implementing-an-iterator
    // GAT is not stabilized, so it's not possible to return a reference in an iterator yet.
    #[inline]
    pub fn next_packet(&mut self) -> Option<&[u8]> {
        debug_assert_ne!(self.stride, 0);
        debug_assert!(self.len <= self.buf.len());
        debug_assert!(self.stride <= self.len);
        debug_assert!(self.offset <= self.len);
        if self.offset == self.len {
            return None;
        }
        let tail = self.len.min(self.offset.saturating_add(self.stride));
        debug_assert!(tail > self.offset);

        let buf = &self.buf[self.offset..tail];
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

pub async fn bind(
    addr: SocketAddr,
    transport: Transport,
    external_addr: Option<SocketAddr>,
) -> io::Result<UnifiedSocket> {
    let sock = match transport {
        Transport::Udp(mode) => bind_udp_socket(addr, mode, external_addr)
            .await?
            .into_unified_socket()?,
        Transport::Tcp => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UnifiedSocket does not support TCP transports",
            ));
        }
    };
    tracing::debug!("bound to {addr} ({transport:?})");
    Ok(sock)
}

pub enum UnifiedSocket {
    Udp(udp::UdpTransport),
    UdpScalar(udp_scalar::UdpTransport),
}

impl UnifiedSocket {
    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        match self {
            Self::Udp(inner) => inner.close_peer(peer_addr),
            Self::UdpScalar(inner) => inner.close_peer(peer_addr),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::UdpScalar(inner) => inner.local_addr(),
        }
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::UdpScalar(inner) => inner.readable().await,
        }
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::UdpScalar(inner) => inner.writable().await,
        }
    }

    #[inline]
    pub fn try_recv_batch(&mut self, packets: &mut Vec<RecvPacketBatch>) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets),
            Self::UdpScalar(inner) => inner.try_recv_batch(packets),
        }
    }

    /// Attempts to send every packet in `batch`. Returns the number of
    /// packets accepted by the kernel (packets dropped because the socket
    /// buffer was full count as "accepted" — this transport is lossy by
    /// design and never queues).
    #[inline]
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> std::io::Result<usize> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::UdpScalar(inner) => inner.try_send_batch(batch),
        }
    }

    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::UdpScalar(inner) => inner.max_gso_segments(),
        }
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Udp(_) => Transport::Udp(UdpMode::Batch),
            Self::UdpScalar(_) => Transport::Udp(UdpMode::Scalar),
        }
    }
}

fn fmt_bytes(b: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if b >= MB {
        format!("{}MB", b / MB)
    } else if b >= KB {
        format!("{}KB", b / KB)
    } else {
        format!("{}B", b)
    }
}

#[cfg(test)]
mod tests {
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
            offset: 0,
        };

        assert_eq!(batch.next_packet(), Some(&[0, 1, 2, 3, 4, 5][..]));
        assert_eq!(batch.next_packet(), Some(&[6, 7, 8, 9, 10, 11][..]));
        assert_eq!(batch.next_packet(), Some(&[12, 13, 14, 15, 16, 17][..]));
        assert_eq!(batch.next_packet(), None);
    }

    #[test]
    fn recv_packet_batch_data_excludes_receive_arena_tail() {
        let batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: vec![1, 2, 3, 4],
            stride: 2,
            len: 2,
            transport: Transport::Udp(UdpMode::Batch),
            offset: 0,
        };

        assert_eq!(batch.data(), &[1, 2]);
    }
}
