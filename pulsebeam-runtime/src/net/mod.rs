pub mod tcp;
pub mod udp;
pub mod udp_scalar;
use std::{io, net::SocketAddr};

pub const BATCH_SIZE: usize = quinn_udp::BATCH_SIZE;
pub const CHUNK_SIZE: usize = 16 * 1024;

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

#[derive(Debug)]
pub struct RecvPacketBatch {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub transport: Transport,
    pub payload: GroPayload,
}

#[derive(Debug)]
pub struct GroPayload {
    pub buf: Vec<u8>,
    pub stride: usize,
    pub len: usize,
}

impl IntoIterator for GroPayload {
    type Item = Vec<u8>;
    type IntoIter = RecvPacketBatchIter;

    fn into_iter(self) -> Self::IntoIter {
        RecvPacketBatchIter {
            batch: self,
            offset: 0,
        }
    }
}

pub struct RecvPacketBatchIter {
    batch: GroPayload,
    offset: usize,
}

impl Iterator for RecvPacketBatchIter {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.batch.len {
            return None;
        }
        let remaining = self.batch.len - self.offset;
        let seg_len = std::cmp::min(self.batch.stride, remaining);
        if seg_len == 0 {
            return None;
        }
        // split_off returns the tail [seg_len..], we want the head [..seg_len]
        let tail = self.batch.buf.split_off(seg_len);
        let segment = std::mem::replace(&mut self.batch.buf, tail);
        self.offset += seg_len;
        Some(segment)
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
