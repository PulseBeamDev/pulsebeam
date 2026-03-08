pub mod tcp;
pub mod udp;
pub mod udp_scalar;

use std::{io, net::SocketAddr};

use crate::sync::pool_buf::PoolBuf;

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
    /// Backing buffer for this packet's bytes (pool-backed, refcounted).
    /// Always index via `data()` — `offset` addresses within it.
    pub buf: PoolBuf,
    /// Byte offset into `buf` where this packet's data begins.
    pub offset: usize,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,
}

impl RecvPacketBatch {
    /// Returns the exact byte slice for this packet (accounts for `offset`).
    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.buf[self.offset..self.offset + self.len]
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
        self.offset += seg_len;
        Some(&self.batch.buf[abs_start..abs_start + seg_len])
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
) -> io::Result<(UnifiedSocketReader, UnifiedSocketWriter)> {
    let socks = match transport {
        Transport::Udp(UdpMode::Batch) => {
            let (reader, writer) = udp::bind(addr, external_addr).await?;
            (
                UnifiedSocketReader::Udp(Box::new(reader)),
                UnifiedSocketWriter::Udp(writer),
            )
        }
        Transport::Udp(UdpMode::Scalar) => {
            let (reader, writer) = udp_scalar::bind(addr, external_addr).await?;
            (
                UnifiedSocketReader::UdpScalar(reader),
                UnifiedSocketWriter::UdpScalar(writer),
            )
        }
        Transport::Tcp => {
            let (reader, writer) = tcp::bind(addr, external_addr).await?;
            (
                UnifiedSocketReader::Tcp(reader),
                UnifiedSocketWriter::Tcp(writer),
            )
        }
    };
    tracing::debug!("bound to {addr} ({transport:?})");
    Ok(socks)
}
pub enum UnifiedSocketReader {
    Udp(Box<udp::UdpTransportReader>),
    UdpScalar(udp_scalar::UdpTransportReader),
    Tcp(tcp::TcpTransportReader),
}

impl UnifiedSocketReader {
    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        match self {
            Self::Tcp(inner) => inner.close_peer(peer_addr),
            Self::Udp(_) => {}
            Self::UdpScalar(_) => {}
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::UdpScalar(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
        }
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::UdpScalar(inner) => inner.readable().await,
            Self::Tcp(inner) => inner.readable().await,
        }
    }

    #[inline]
    pub fn try_recv_batch(&mut self, packets: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets),
            Self::UdpScalar(inner) => inner.try_recv_batch(packets),
            Self::Tcp(inner) => inner.try_recv_batch(packets),
        }
    }
}

#[derive(Clone)]
pub enum UnifiedSocketWriter {
    Udp(udp::UdpTransportWriter),
    UdpScalar(udp_scalar::UdpTransportWriter),
    Tcp(tcp::TcpTransportWriter),
}

impl UnifiedSocketWriter {
    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::UdpScalar(inner) => inner.max_gso_segments(),
            Self::Tcp(inner) => inner.max_gso_segments(),
        }
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::UdpScalar(inner) => inner.writable().await,
            Self::Tcp(inner) => inner.writable().await,
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::UdpScalar(inner) => inner.try_send_batch(batch),
            Self::Tcp(inner) => inner.try_send_batch(batch),
        }
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Udp(_) => Transport::Udp(UdpMode::Batch),
            Self::UdpScalar(_) => Transport::Udp(UdpMode::Scalar),
            Self::Tcp(_) => Transport::Tcp,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::UdpScalar(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
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
