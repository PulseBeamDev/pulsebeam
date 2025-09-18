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
    pub buf: Bytes,      // pre-allocated buffer
    pub src: SocketAddr, // remote peer
    pub dst: SocketAddr,
}

/// A packet to send.
#[derive(Debug, Clone)]
pub struct SendPacket {
    pub buf: Bytes,      // zero-copy payload
    pub dst: SocketAddr, // destination
}

/// UnifiedSocket enum for different transport types
pub enum UnifiedSocket<'a> {
    Udp(UdpTransport<'a>),
    SimUdp(SimUdpTransport),
}

impl<'a> UnifiedSocket<'a> {
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
        &mut self,
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
    pub fn try_send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(packets),
            Self::SimUdp(inner) => inner.try_send_batch(packets),
        }
    }
}

/// tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
/// TODO:
/// * Evaluate quinn-udp
/// * Evaluate XDP
/// * Evaluate io-uring
pub struct UdpTransport<'a> {
    sock: tokio::net::UdpSocket,
    local_addr: SocketAddr,

    // Leaving a marker with lifetime here for future interactions with system
    // optimizations.
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> UdpTransport<'a> {
    pub const MTU: usize = 1500;
    pub const BUF_SIZE: usize = 16 * 1024 * 1024; // 16MB

    pub fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let sock = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sock.set_nonblocking(true)?;
        sock.set_reuse_address(true)?;

        sock.set_recv_buffer_size(Self::BUF_SIZE)?;
        sock.set_send_buffer_size(Self::BUF_SIZE)?;
        sock.bind(&addr.into())?;

        // TODO: set qos class?

        let sock = tokio::net::UdpSocket::from_std(sock.into())?;
        let local_addr = external_addr.unwrap_or(sock.local_addr()?);

        Ok(UdpTransport {
            sock,
            local_addr,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
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
        &mut self,
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
    pub fn try_send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        let mut count = 0;
        for packet in packets.iter() {
            match self.sock.try_send_to(&packet.buf, packet.dst) {
                Ok(_) => count += 1,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Send packet to {} failed: {}", packet.dst, e); // Internal logging
                    continue; // Skip and try next packet
                }
            }
        }
        Ok(count)
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
    pub fn try_send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        let mut count = 0;
        for packet in packets.iter() {
            match self.sock.try_send_to(&packet.buf, packet.dst) {
                Ok(_) => count += 1,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Send packet to {} failed: {}", packet.dst, e);
                    continue;
                }
            }
        }
        Ok(count)
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

        let payload = b"hello";
        sock.writable().await.unwrap();
        let packet = SendPacket {
            buf: Bytes::from_static(payload),
            dst: sock.local_addr(),
        };
        sock.try_send_batch(&[packet]).unwrap();

        sock.readable().await.unwrap();
        let mut buf = Vec::with_capacity(1);
        let buf_cap = buf.capacity();
        let count = sock.try_recv_batch(&mut buf, buf_cap).unwrap();
        assert_eq!(count, 1);
        assert_eq!(buf.len(), 1);
        assert_eq!(&buf[0].buf, payload.as_slice());
    }
}
