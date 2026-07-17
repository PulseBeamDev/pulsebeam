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
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,

    pub offset: usize,
}

impl RecvPacketBatch {
    /// Returns the exact byte slice for this packet (accounts for `offset`).
    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    // https://stackoverflow.com/questions/68606470/how-to-return-a-reference-when-implementing-an-iterator
    // GAT is not stabilized, so it's not possible to return a reference in an iterator yet.
    pub fn next_packet(&mut self) -> Option<&[u8]> {
        assert!(self.stride != 0, "stride must not be zero in iteration");
        let tail = self.len.min(self.offset + self.stride);
        if self.offset >= tail {
            return None;
        }

        let buf = &self.buf[self.offset..tail];
        self.offset = tail;
        Some(buf)
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
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> std::io::Result<bool> {
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
}
