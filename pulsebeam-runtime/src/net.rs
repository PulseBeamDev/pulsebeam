use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
};

use bytes::{Bytes, BytesMut};
use tokio::io::Interest;

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
pub struct RecvPacket {
    pub buf: BytesMut,    // pre-allocated buffer
    pub len: usize,       // actual data length
    pub addr: SocketAddr, // remote peer
}

/// A packet to send.
pub struct SendPacket {
    pub buf: Bytes,       // zero-copy payload
    pub addr: SocketAddr, // destination
}

/// tokio/mio doesn't support batching: https://github.com/tokio-rs/mio/issues/185
/// TODO:
/// * Evaluate quinn-udp
/// * Evaluate XDP
/// * Evaluate io-uring
pub struct UdpTransport<'a> {
    sock: tokio::net::UdpSocket,

    // Leaving a marker with lifetime here for future interactions with system
    // optimizations.
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> UdpTransport<'a> {
    pub const BUF_SIZE: usize = 16 * 1024 * 1024; // 16MB

    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let sock = std::net::UdpSocket::bind(addr)?;
        let sock: socket2::Socket = sock.into();
        sock.set_nonblocking(true)?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;

        sock.set_recv_buffer_size(Self::BUF_SIZE)?;
        sock.set_send_buffer_size(Self::BUF_SIZE)?;

        // TODO: set qos class?

        sock.bind(&addr.into())?;

        let sock = tokio::net::UdpSocket::from_std(sock.into())?;

        Ok(UdpTransport {
            sock,
            _marker: std::marker::PhantomData,
        })
    }

    pub async fn recv_batch(&self, packets: &mut [RecvPacket]) -> std::io::Result<usize> {
        self.sock.ready(Interest::READABLE).await?;

        let mut count = 0;
        while count < packets.len() {
            // Use try_recv_from to non-blockingly receive (zero alloc)
            match self.sock.try_recv_from(&mut packets[count].buf) {
                Ok((len, addr)) => {
                    packets[count].len = len;
                    packets[count].addr = addr;
                    count += 1;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // No more data available right now; stop batch
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(count)
    }

    pub async fn send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        if packets.is_empty() {
            return Ok(0);
        }

        let mut count = 0;
        let mut last_error = None;

        // Loop over the packets, attempting to send each one
        while count < packets.len() {
            // First, try to send non-blockingly
            match self
                .sock
                .try_send_to(&packets[count].buf, packets[count].addr)
            {
                Ok(_) => {
                    count += 1;
                    last_error = None; // Reset error if successful
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // Socket not writable; await writability
                    self.sock.ready(Interest::WRITABLE).await?;
                    // Continue trying (don't count as sent yet)
                }
                Err(e) => {
                    // Other error; store and continue to try next, but we'll return error if any
                    last_error = Some(e);
                    count += 1; // Skip this packet, consider "processed" but error will be returned
                }
            }
        }

        if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(count)
        }
    }
}

/// UnifiedSocket enum for different transport types
pub enum UnifiedSocket<'a> {
    Udp(UdpTransport<'a>),
}

impl<'a> UnifiedSocket<'a> {
    pub async fn bind(addr: SocketAddr, transport: Transport) -> io::Result<Self> {
        let sock = match transport {
            Transport::Udp => Self::Udp(UdpTransport::bind(addr)?),
            _ => todo!(),
        };
        Ok(sock)
    }

    /// Receive a batch of packets (up to `packets.len()`).
    /// Returns number of packets actually received.
    /// Uses pre-allocated buffers in `packets`, awaits readability once, then drains available datagrams
    /// with zero additional allocations.
    #[inline]
    pub async fn recv_batch(&self, packets: &mut [RecvPacket]) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.recv_batch(packets).await,
        }
    }

    /// Send a batch of packets.
    /// Returns number of packets actually sent.
    /// Uses provided buffers, awaits writability if needed, then sends as many as possible
    /// with zero additional allocations.
    #[inline]
    pub async fn send_batch(&self, packets: &[SendPacket]) -> std::io::Result<usize> {
        match self {
            Self::Udp(inner) => inner.send_batch(packets).await,
        }
    }
}
