use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
};

use bytes::{Bytes, BytesMut};

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
        packets: &mut Vec<RecvPacket>,
        batch_size: usize,
    ) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets, batch_size),
            Self::SimUdp(inner) => inner.try_recv_batch(packets, batch_size),
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
    pub const MTU: usize = 1500;
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
        packets: &mut Vec<RecvPacket>,
        batch_size: usize,
    ) -> std::io::Result<usize> {
        let mut count = 0;

        while count < batch_size {
            // This is done for simplicity. In the future, we may want to evaluate
            // contiguous memory for cache locality.
            let mut buf = BytesMut::with_capacity(Self::MTU);
            // Try to receive a packet
            match self.sock.try_recv_buf_from(&mut buf) {
                Ok((len, src)) => {
                    let packet_data = buf.split_to(len).freeze();
                    packets.push(RecvPacket {
                        buf: packet_data,
                        src,
                        dst: self.local_addr,
                    });
                    count += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Receive packet failed: {}", e);
                    return Err(e);
                }
            }
        }
        Ok(count)
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
        let _ = self.state.send((&self.sock).into(), &transmit);
        if let Ok(_) = self.state.send((&self.sock).into(), &transmit) {
            true
        } else {
            false
        }
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
        packets: &mut Vec<RecvPacket>,
        batch_size: usize,
    ) -> std::io::Result<usize> {
        let mut count = 0;
        let mut buf = [0u8; Self::MTU];

        while count < batch_size {
            match self.sock.try_recv_from(&mut buf) {
                Ok((len, src)) => {
                    packets.push(RecvPacket {
                        buf: Bytes::copy_from_slice(&buf[..len]),
                        src,
                        dst: self.local_addr,
                    });
                    count += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Receive packet failed: {}", e);
                    return Err(e);
                }
            }
        }
        Ok(count)
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
        let mut buf = Vec::with_capacity(1);
        let buf_cap = buf.capacity();
        let count = sock.try_recv_batch(&mut buf, buf_cap).unwrap();
        assert_eq!(count, 1);
        assert_eq!(buf.len(), 1);
        assert_eq!(&buf[0].buf, payload.as_slice());
    }
}
