use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU16;

use noq_proto::{EcnCodepoint, Transmit};
#[cfg(not(feature = "sim"))]
use pulsebeam_udp::DetachedUdpSocket;
use pulsebeam_udp::{BindOptions, SendDatagram, SendStop, UdpSocket};

use super::UDP_PAYLOAD_BYTES;

/// Owns the UDP descriptor and the sole socket-bound transmit, including
/// across reactor moves. No packet-rate work is delegated across shards.
#[cfg(not(feature = "sim"))]
pub struct SocketIo<S = UdpSocket> {
    socket: S,
    egress: Egress,
}

/// One committed Noq transmit, stored with the logical owner rather than
/// reactor-affine socket registration.
pub struct Egress {
    bytes: Vec<u8>,
    pending: Option<Transmit>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Flush {
    Empty,
    Sent,
    Blocked,
    PathMtu,
}

#[cfg(not(feature = "sim"))]
impl SocketIo {
    pub async fn bind(address: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: bind(address).await?.activate()?,
            egress: Egress::default(),
        })
    }

    pub fn socket(&mut self) -> &mut UdpSocket {
        &mut self.socket
    }

    pub fn stage(
        &mut self,
        encode: impl FnOnce(&mut Vec<u8>) -> Option<Transmit>,
    ) -> io::Result<bool> {
        self.egress.stage(self.socket.local_addr(), encode)
    }

    pub fn flush(&mut self) -> io::Result<Flush> {
        self.egress.flush(&mut self.socket)
    }

    pub fn detach(self) -> SocketIo<DetachedUdpSocket> {
        SocketIo {
            socket: self.socket.detach(),
            egress: self.egress,
        }
    }
}

pub async fn bind(address: SocketAddr) -> io::Result<pulsebeam_udp::BoundUdpSocket> {
    if address.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QUIC requires an explicit local address",
        ));
    }
    let mut options = BindOptions::new(address);
    options.max_datagram_bytes = usize::from(UDP_PAYLOAD_BYTES);
    options.nonfragmenting_max_datagram_bytes = NonZeroU16::new(UDP_PAYLOAD_BYTES);
    options.receive_ecn = true;
    pulsebeam_udp::bind(options).await
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            bytes: Vec::with_capacity(usize::from(UDP_PAYLOAD_BYTES)),
            pending: None,
        }
    }
}

impl Egress {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn discard(&mut self) {
        self.pending = None;
    }

    /// Only invokes the encoder when egress has space. The closure writes the
    /// datagram directly into the slot used for nonblocking socket submission.
    pub fn stage(
        &mut self,
        local_addr: SocketAddr,
        encode: impl FnOnce(&mut Vec<u8>) -> Option<Transmit>,
    ) -> io::Result<bool> {
        if self.pending.is_some() {
            return Ok(false);
        }
        self.bytes.clear();
        let Some(tx) = encode(&mut self.bytes) else {
            return Ok(false);
        };
        if tx.segment_size.is_some()
            || tx.size == 0
            || tx.size > self.bytes.len()
            || tx.size > usize::from(UDP_PAYLOAD_BYTES)
            || tx.src_ip.is_some_and(|ip| ip != local_addr.ip())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid QUIC transmit for bound socket",
            ));
        }
        self.bytes.truncate(tx.size);
        self.pending = Some(tx);
        Ok(true)
    }

    /// A refusal at the IPv6/QUIC floor leaves the committed packet staged;
    /// the owner must fence the association rather than truncate or fragment.
    pub fn flush(&mut self, socket: &mut UdpSocket) -> io::Result<Flush> {
        let Some(tx) = &self.pending else {
            return Ok(Flush::Empty);
        };
        let report = if tx.ecn == Some(EcnCodepoint::Ect0) && socket.supports_ect0() {
            socket.try_send_ect0(tx.destination, &self.bytes)
        } else {
            socket.try_send_batch(&[SendDatagram {
                destination: tx.destination,
                payload: &self.bytes,
                gso_segment_size: None,
            }])
        };
        match report.stop {
            None if report.accepted == 1 => {
                self.pending = None;
                Ok(Flush::Sent)
            }
            Some(SendStop::Blocked) => Ok(Flush::Blocked),
            Some(ref stop) if stop.is_path_mtu_error() => Ok(Flush::PathMtu),
            Some(SendStop::Error(error)) => Err(error),
            None => Err(io::Error::other(
                "UDP backend did not account for QUIC transmit",
            )),
        }
    }
}

#[cfg(not(feature = "sim"))]
impl SocketIo<DetachedUdpSocket> {
    pub fn activate(self) -> Result<SocketIo, (Box<Self>, io::Error)> {
        let Self { socket, egress } = self;
        match socket.activate() {
            Ok(socket) => Ok(SocketIo { socket, egress }),
            Err((socket, error)) => Err((
                Box::new(Self {
                    socket: *socket,
                    egress,
                }),
                error,
            )),
        }
    }
}
