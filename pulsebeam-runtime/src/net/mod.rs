mod sim;
mod tcp;
mod udp;

use std::{io, net::SocketAddr};

use bytes::Bytes;
use quinn_udp::RecvMeta;

use crate::net::{sim::SimUdpTransport, tcp::TcpTransport, udp::UdpTransport};

pub const BATCH_SIZE: usize = quinn_udp::BATCH_SIZE;
// Fit allocator page size and Linux GRO limit
pub const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,
    Tcp,
    Tls,
    SimUdp,
}

/// A received packet (zero-copy buffer + source address).
#[derive(Debug, Clone)]
pub struct RecvPacket {
    pub src: SocketAddr, // remote peer
    pub dst: SocketAddr,
    pub buf: Bytes, // pre-allocated buffer
}

#[derive(Debug, Clone)]
pub struct RecvPacketBatch {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub buf: Bytes,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,
}

impl RecvPacketBatch {
    pub fn iter(&self) -> RecvPacketBatchIter<'_> {
        RecvPacketBatchIter {
            batch: self,
            offset: 0,
        }
    }
}

impl<'a> IntoIterator for &'a RecvPacketBatch {
    type Item = Bytes;
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
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.batch.len {
            return None;
        }

        let remaining = self.batch.len - self.offset;
        let seg_len = std::cmp::min(self.batch.stride, remaining);

        if seg_len == 0 {
            return None;
        }

        let packet_buf = self.batch.buf.slice(self.offset..self.offset + seg_len);

        self.offset += seg_len;

        Some(packet_buf)
    }
}

pub struct RecvPacketBatcher {
    meta: [RecvMeta; BATCH_SIZE],
    batch_buffer: Vec<u8>,
}

impl RecvPacketBatcher {
    pub fn new() -> Self {
        Self {
            meta: [RecvMeta::default(); BATCH_SIZE],
            batch_buffer: Vec::with_capacity(BATCH_SIZE * CHUNK_SIZE),
        }
    }

    fn collect(&mut self, local_addr: SocketAddr, count: usize, out: &mut Vec<RecvPacketBatch>) {
        let new_buffer = Vec::with_capacity(BATCH_SIZE * CHUNK_SIZE);
        let filled_buffer = std::mem::replace(&mut self.batch_buffer, new_buffer);
        let master_bytes = Bytes::from(filled_buffer);

        for i in 0..count {
            let m = &self.meta[i];

            let start = i * CHUNK_SIZE;
            let end = start + m.len;

            // Safety: Ensure we don't slice past the buffer (e.g., if kernel lied about len)
            if end > master_bytes.len() {
                continue;
            }

            let buf = master_bytes.slice(start..end);

            out.push(RecvPacketBatch {
                src: m.addr,
                dst: local_addr,
                buf,
                stride: m.stride,
                len: m.len,
                transport: Transport::Udp,
            });
        }
    }
}

/// A packet to send.
#[derive(Debug, Clone)]
pub struct SendPacket {
    pub dst: SocketAddr, // destination
    pub buf: Bytes,      // zero-copy payload
}

#[derive(Debug, Clone)]
pub struct SendPacketBatch<'a> {
    pub dst: SocketAddr,
    pub buf: &'a [u8],
    pub segment_size: usize,
}

/// UnifiedSocket enum for different transport types
pub enum UnifiedSocket {
    Udp(UdpTransport),
    Tcp(TcpTransport),
    SimUdp(SimUdpTransport),
}

impl UnifiedSocket {
    /// Binds a socket to the given address and transport type.
    pub async fn bind(
        addr: SocketAddr,
        transport: Transport,
        external_addr: Option<SocketAddr>,
    ) -> io::Result<Self> {
        let sock = match transport {
            Transport::Udp => Self::Udp(UdpTransport::bind(addr, external_addr)?),
            Transport::Tcp => Self::Tcp(TcpTransport::bind(addr, external_addr).await?),
            Transport::SimUdp => Self::SimUdp(SimUdpTransport::bind(addr, external_addr).await?),
            _ => todo!(),
        };
        tracing::debug!("bound to {addr} ({transport:?})");
        Ok(sock)
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
            Self::SimUdp(inner) => inner.local_addr(),
        }
    }

    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::Tcp(inner) => inner.max_gso_segments(),
            Self::SimUdp(inner) => inner.max_gso_segments(),
        }
    }

    /// Waits until the socket is readable.
    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::Tcp(inner) => inner.readable().await,
            Self::SimUdp(inner) => inner.readable().await,
        }
    }

    /// Waits until the socket is writable.
    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::Tcp(inner) => inner.writable().await,
            Self::SimUdp(inner) => inner.writable().await,
        }
    }

    /// Receives a batch of packets into pre-allocated buffers.
    #[inline]
    pub fn try_recv_batch(
        &self,
        batch: &mut RecvPacketBatcher,
        packets: &mut Vec<RecvPacketBatch>,
    ) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(batch, packets),
            Self::Tcp(inner) => inner.try_recv_batch(packets),
            Self::SimUdp(inner) => inner.try_recv_batch(batch, packets),
        }
    }

    /// Sends a batch of packets.
    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::Tcp(inner) => inner.try_send_batch(batch),
            Self::SimUdp(inner) => inner.try_send_batch(batch),
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
mod test {
    use super::*;

    #[tokio::test]
    async fn test_loopback() {
        let mut sock = UnifiedSocket::bind("127.0.0.1:3000".parse().unwrap(), Transport::Udp, None)
            .await
            .unwrap();

        sock.writable().await.unwrap();
        let payload = b"hello";
        let send_batch = SendPacketBatch {
            dst: sock.local_addr(),
            buf: payload,
            segment_size: payload.len(),
        };
        assert!(sock.try_send_batch(&send_batch).unwrap());

        sock.readable().await.unwrap();
        let mut batcher = RecvPacketBatcher::new();
        let mut buf = Vec::with_capacity(1);
        sock.try_recv_batch(&mut batcher, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        let mut batch = buf[0].iter();
        assert_eq!(&batch.next().unwrap(), payload.as_slice());

        buf.clear();
        let payload = b"bla";
        let send_batch = SendPacketBatch {
            dst: sock.local_addr(),
            buf: payload,
            segment_size: payload.len(),
        };
        assert!(sock.try_send_batch(&send_batch).unwrap());
        sock.try_recv_batch(&mut batcher, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        let mut batch = buf[0].iter();
        assert_eq!(&batch.next().unwrap(), payload.as_slice());
    }
}
