pub mod tcp;
pub mod udp;
pub mod udp_scalar;

use std::{io, net::SocketAddr};

pub const BATCH_SIZE: usize = quinn_udp::BATCH_SIZE;
pub const CHUNK_SIZE: usize = 64 * 1024;

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
    pub offset: usize,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,
}

impl RecvPacketBatch {
    /// Returns the exact byte slice for this packet (accounts for `offset`).
    #[inline]
    pub fn data(&self) -> &[u8] {
        debug_assert!(
            self.offset <= self.buf.len(),
            "RecvPacketBatch.offset is out of bounds"
        );
        debug_assert!(
            self.len <= self.buf.len().saturating_sub(self.offset),
            "RecvPacketBatch.len is out of bounds"
        );
        match self.offset.checked_add(self.len) {
            Some(end) => self.buf.get(self.offset..end).unwrap_or(&[]),
            None => &[],
        }
    }

    pub fn iter(&self) -> RecvPacketBatchIter<'_> {
        RecvPacketBatchIter {
            batch: self,
            offset: 0,
        }
    }
}

impl<'a> IntoIterator for &'a RecvPacketBatch {
    type Item = &'a [u8];
    type IntoIter = RecvPacketBatchIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub struct RecvPacketBatchIter<'a> {
    batch: &'a RecvPacketBatch,
    offset: usize,
}

impl<'a> Iterator for RecvPacketBatchIter<'a> {
    // Zero-copy: yields borrowed slices directly into the shared Bytes buffer.
    // No atomic operations — the refcount on `batch.buf` is already held by
    // the RecvPacketBatch owner for the duration of the iteration.
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.batch.len {
            return None;
        }
        let remaining = self.batch.len - self.offset;
        // stride == 0 means a single non-GRO datagram: treat as one segment.
        let stride = if self.batch.stride == 0 {
            self.batch.len
        } else {
            self.batch.stride
        };
        let seg_len = std::cmp::min(stride, remaining);
        if seg_len == 0 {
            return None;
        }
        let abs_start = self.batch.offset + self.offset;
        let abs_end = abs_start.checked_add(seg_len)?;
        if abs_end > self.batch.buf.len() {
            return None;
        }

        self.offset += seg_len;
        Some(&self.batch.buf[abs_start..abs_end])
    }
}

#[derive(Debug, Clone)]
pub struct SendPacketBatch<'a> {
    pub dst: SocketAddr,
    pub buf: &'a [u8],
    pub segment_size: usize,
}

pub async fn bind(
    addr: SocketAddr,
    transport: Transport,
    external_addr: Option<SocketAddr>,
) -> io::Result<UnifiedSocket> {
    let sock = match transport {
        Transport::Udp(UdpMode::Batch) => {
            let transport = udp::bind(addr, external_addr).await?;
            UnifiedSocket::Udp(transport)
        }
        Transport::Udp(UdpMode::Scalar) => {
            let transport = udp_scalar::bind(addr, external_addr).await?;
            UnifiedSocket::UdpScalar(transport)
        }
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

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
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
    fn recv_packet_batch_data_returns_exact_slice() {
        let batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: vec![1, 2, 3, 4, 5],
            offset: 1,
            stride: 0,
            len: 3,
            transport: Transport::Udp(UdpMode::Scalar),
        };

        assert_eq!(batch.data(), &[2, 3, 4]);
    }

    #[test]
    fn recv_packet_batch_iter_yields_multiple_segments_without_off_by_one() {
        let batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: (0u8..20).collect(),
            offset: 0,
            stride: 6,
            len: 20,
            transport: Transport::Udp(UdpMode::Scalar),
        };

        let chunks: Vec<&[u8]> = batch.iter().collect();
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(chunks[1], &[6, 7, 8, 9, 10, 11]);
        assert_eq!(chunks[2], &[12, 13, 14, 15, 16, 17]);
        assert_eq!(chunks[3], &[18, 19]);
    }

    #[test]
    fn recv_packet_batch_iter_single_segment_stride_zero() {
        let batch = RecvPacketBatch {
            src: test_addr(),
            dst: test_addr(),
            buf: (0u8..10).collect(),
            offset: 0,
            stride: 0,
            len: 10,
            transport: Transport::Udp(UdpMode::Scalar),
        };

        let chunks: Vec<&[u8]> = batch.iter().collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }
}
