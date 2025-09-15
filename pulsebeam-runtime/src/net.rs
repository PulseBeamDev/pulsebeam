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
#[derive(Debug)]
pub struct RecvPacket {
    pub buf: BytesMut,   // pre-allocated buffer
    pub src: SocketAddr, // remote peer
    pub dst: SocketAddr,
    pub len: usize,
}

/// A packet to send.
#[derive(Debug)]
pub struct SendPacket {
    pub buf: Bytes,      // zero-copy payload
    pub dst: SocketAddr, // destination
}

/// UnifiedSocket enum for different transport types
pub enum UnifiedSocket<'a> {
    Udp(UdpTransport<'a>),
}

impl<'a> UnifiedSocket<'a> {
    /// Binds a socket to the given address and transport type.
    pub async fn bind(addr: SocketAddr, transport: Transport) -> io::Result<Self> {
        let sock = match transport {
            Transport::Udp => Self::Udp(UdpTransport::bind(addr)?),
            _ => todo!(),
        };
        Ok(sock)
    }

    /// Waits until the socket is readable.
    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await?,
        }
        Ok(())
    }

    /// Waits until the socket is writable.
    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await?,
        }
        Ok(())
    }

    /// Receives a batch of packets into pre-allocated buffers.
    /// Returns the number of packets received.
    /// Stops on WouldBlock or fatal errors.
    #[inline]
    pub fn try_recv_batch(&self, packets: &mut [RecvPacket]) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets),
        }
    }

    /// Sends a batch of packets.
    /// Returns the number of packets sent.
    /// Stops on WouldBlock or fatal errors; errors for individual packets are logged.
    #[inline]
    pub fn try_send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(packets),
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
    pub const BUF_SIZE: usize = 16 * 1024 * 1024; // 16MB

    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let sock = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sock.set_nonblocking(true)?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;

        sock.set_recv_buffer_size(Self::BUF_SIZE)?;
        sock.set_send_buffer_size(Self::BUF_SIZE)?;

        // TODO: set qos class?

        let sock = tokio::net::UdpSocket::from_std(sock.into())?;
        let local_addr = sock.local_addr()?;

        Ok(UdpTransport {
            sock,
            local_addr,
            _marker: std::marker::PhantomData,
        })
    }

    pub async fn readable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::READABLE).await?;
        Ok(())
    }

    pub async fn writable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::WRITABLE).await?;
        Ok(())
    }

    pub fn try_recv_batch(&self, packets: &mut [RecvPacket]) -> std::io::Result<usize> {
        let mut count = 0;
        for packet in packets.iter_mut() {
            match self.sock.try_recv_from(&mut packet.buf) {
                Ok((len, src)) => {
                    packet.src = src;
                    // TODO: verify if this is a good value for dst here.
                    packet.dst = self.local_addr;
                    packet.len = len;
                    count += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

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
