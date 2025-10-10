use std::{
    io::{self, ErrorKind, IoSliceMut},
    net::SocketAddr,
};

use bytes::Bytes;
use quinn_udp::RecvMeta;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,

    // ======================== TCP ========================
    // TODO: Implement TCP Framing:
    // * https://datatracker.ietf.org/doc/html/rfc6544
    // * https://datatracker.ietf.org/doc/html/rfc4571
    //
    // Supported TCP mode:
    // * Passive: Yes
    // * Active: No
    // * SO: No
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

pub struct RecvBatchStorage {
    backend: Vec<u8>,
    chunk_size: usize,
}

impl RecvBatchStorage {
    const MAX_MTU: usize = 1500;

    pub fn new(gro_segments: usize) -> Self {
        let chunk_size = Self::MAX_MTU * gro_segments;
        Self {
            backend: vec![0; quinn_udp::BATCH_SIZE * chunk_size],
            chunk_size,
        }
    }

    pub fn as_io_slices(&mut self) -> Vec<IoSliceMut<'_>> {
        self.backend
            .chunks_mut(self.chunk_size)
            .map(IoSliceMut::new)
            .collect()
    }
}

/// Holds IoSliceMut for recv operations and metadata.
pub struct RecvPacketBatch<'a> {
    slices: Vec<IoSliceMut<'a>>,
    meta: Vec<RecvMeta>,
}

impl<'a> RecvPacketBatch<'a> {
    pub fn new(storage: &'a mut RecvBatchStorage) -> Self {
        let slices = storage.as_io_slices();
        let meta = vec![RecvMeta::default(); quinn_udp::BATCH_SIZE];

        Self { slices, meta }
    }

    /// Extract received packets into user-facing data
    pub fn collect_packets(&self, local_addr: SocketAddr, count: usize, out: &mut Vec<RecvPacket>) {
        // TODO: avoid reallocation here
        // TODO: send as batch instead of individual packets
        for (i, m) in self.meta.iter().take(count).enumerate() {
            let buf = &self.slices[i];
            let segments = m.len / m.stride;
            for seg in 0..segments {
                let start = seg * m.stride;
                let end = start + m.stride;
                out.push(RecvPacket {
                    src: m.addr,
                    dst: local_addr,
                    buf: Bytes::copy_from_slice(&buf[start..end]),
                });
            }
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
            Transport::SimUdp => Self::SimUdp(SimUdpTransport::bind(addr, external_addr).await?),
            _ => todo!(),
        };
        tracing::debug!("bound to {addr} ({transport:?})");
        Ok(sock)
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::SimUdp(inner) => inner.local_addr(),
        }
    }

    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::SimUdp(inner) => inner.max_gso_segments(),
        }
    }

    pub fn gro_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.gro_segments(),
            Self::SimUdp(inner) => inner.gro_segments(),
        }
    }

    /// Waits until the socket is readable.
    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await?,
            Self::SimUdp(inner) => inner.readable().await?,
        }
        Ok(())
    }

    /// Waits until the socket is writable.
    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await?,
            Self::SimUdp(inner) => inner.writable().await?,
        }
        Ok(())
    }

    /// Receives a batch of packets into pre-allocated buffers.
    /// Returns the number of packets received.
    /// Stops on WouldBlock or fatal errors.
    #[inline]
    pub fn try_recv_batch(
        &self,
        batch: &mut RecvPacketBatch,
        packets: &mut Vec<RecvPacket>,
    ) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(batch, packets),
            Self::SimUdp(inner) => inner.try_recv_batch(batch, packets),
        }
    }

    /// Sends a batch of packets.
    /// Returns the number of packets sent.
    /// Stops on WouldBlock or fatal errors; errors for individual packets are logged.
    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> bool {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::SimUdp(inner) => inner.try_send_batch(batch),
        }
    }
}

pub struct UdpTransport {
    sock: tokio::net::UdpSocket,
    state: quinn_udp::UdpSocketState,
    local_addr: SocketAddr,
}

impl UdpTransport {
    pub const MAX_MTU: usize = 1500;
    pub const BUF_SIZE: usize = 16 * 1024 * 1024; // 16MB

    pub fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let socket2_sock = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket2_sock.set_nonblocking(true)?;
        socket2_sock.set_reuse_address(true)?;

        #[cfg(unix)]
        socket2_sock.set_reuse_port(true)?;

        socket2_sock.set_recv_buffer_size(Self::BUF_SIZE)?;
        socket2_sock.set_send_buffer_size(Self::BUF_SIZE)?;
        socket2_sock.bind(&addr.into())?;

        let state = quinn_udp::UdpSocketState::new((&socket2_sock).into())?;
        let sock = tokio::net::UdpSocket::from_std(socket2_sock.into())?;

        // TODO: set qos class?
        let local_addr = external_addr.unwrap_or(sock.local_addr()?);

        Ok(Self {
            sock,
            local_addr,
            state,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    pub fn gro_segments(&self) -> usize {
        self.state.gro_segments()
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::READABLE).await?;
        Ok(())
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::WRITABLE).await?;
        Ok(())
    }

    #[inline]
    pub fn try_recv_batch(
        &self,
        batch: &mut RecvPacketBatch,
        out: &mut Vec<RecvPacket>,
    ) -> std::io::Result<()> {
        let res = self.sock.try_io(tokio::io::Interest::READABLE, || {
            self.state
                .recv((&self.sock).into(), &mut batch.slices, &mut batch.meta)
        });

        match res {
            Ok(count) => {
                batch.collect_packets(self.local_addr, count, out);
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> bool {
        debug_assert!(batch.segment_size != 0);
        let transmit = quinn_udp::Transmit {
            destination: batch.dst,
            ecn: None,
            contents: batch.buf,
            segment_size: Some(batch.segment_size),
            src_ip: None,
        };
        tracing::trace!("try_send_batch: {}", transmit.contents.len());

        self.sock
            .try_io(tokio::io::Interest::WRITABLE, || {
                self.state.send((&self.sock).into(), &transmit)
            })
            .is_ok()
    }
}

pub struct SimUdpTransport {
    sock: turmoil::net::UdpSocket,
    local_addr: SocketAddr,
}

impl SimUdpTransport {
    pub const MTU: usize = 1500;

    pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let sock = turmoil::net::UdpSocket::bind(addr).await?;
        let local_addr = external_addr.unwrap_or(sock.local_addr()?);

        Ok(SimUdpTransport { sock, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        1
    }

    pub fn gro_segments(&self) -> usize {
        1
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.readable().await
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.writable().await
    }

    #[inline]
    pub fn try_recv_batch(
        &self,
        batch: &mut RecvPacketBatch,
        packets: &mut Vec<RecvPacket>,
    ) -> std::io::Result<()> {
        let mut buf = [0u8; Self::MTU];

        loop {
            match self.sock.try_recv_from(&mut buf) {
                Ok((len, src)) => {
                    packets.push(RecvPacket {
                        buf: Bytes::copy_from_slice(&buf[..len]),
                        src,
                        dst: self.local_addr,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Receive packet failed: {}", e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> bool {
        assert!(batch.segment_size != 0);
        assert!(batch.buf.len() == batch.segment_size);

        match self.sock.try_send_to(batch.buf, batch.dst) {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => {
                tracing::warn!("Receive packet failed: {}", e);
                false
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_loopback() {
        let sock = UnifiedSocket::bind("127.0.0.1:3000".parse().unwrap(), Transport::Udp, None)
            .await
            .unwrap();

        let payload = b"hello";
        sock.writable().await.unwrap();
        let packet = SendPacket {
            buf: Bytes::from_static(payload),
            dst: sock.local_addr(),
        };
        let batch = SendPacketBatch {
            dst: packet.dst,
            buf: &packet.buf,
            segment_size: packet.buf.len(),
        };
        assert!(sock.try_send_batch(&batch));

        sock.readable().await.unwrap();
        let mut storage = RecvBatchStorage::new(1);
        let mut batch = RecvPacketBatch::new(&mut storage);
        let mut buf = Vec::with_capacity(1);
        sock.try_recv_batch(&mut batch, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(&buf[0].buf, payload.as_slice());
    }
}
