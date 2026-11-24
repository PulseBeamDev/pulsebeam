use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU16;

use noq_proto::{EcnCodepoint, Transmit};
use pulsebeam_udp::{BindOptions, DetachedUdpSocket, SendDatagram, SendStop, UdpSocket};

use super::UDP_PAYLOAD_BYTES;

/// Owns the UDP descriptor and the sole socket-bound transmit, including
/// across reactor moves. No packet-rate work is delegated across shards.
pub struct SocketIo<S = UdpSocket> {
    socket: S,
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

impl SocketIo {
    pub async fn bind(address: SocketAddr) -> io::Result<Self> {
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
        Ok(Self {
            socket: pulsebeam_udp::bind(options).await?.activate()?,
            bytes: Vec::with_capacity(usize::from(UDP_PAYLOAD_BYTES)),
            pending: None,
        })
    }

    pub fn socket(&mut self) -> &mut UdpSocket {
        &mut self.socket
    }

    /// Only invokes the encoder when egress has space. The closure writes the
    /// datagram directly into the slot used for nonblocking socket submission.
    pub fn stage(
        &mut self,
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
            || tx
                .src_ip
                .is_some_and(|ip| ip != self.socket.local_addr().ip())
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
    pub fn flush(&mut self) -> io::Result<Flush> {
        let Some(tx) = &self.pending else {
            return Ok(Flush::Empty);
        };
        let report = if tx.ecn == Some(EcnCodepoint::Ect0) && self.socket.supports_ect0() {
            self.socket.try_send_ect0(tx.destination, &self.bytes)
        } else {
            self.socket.try_send_batch(&[SendDatagram {
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

    pub fn detach(self) -> SocketIo<DetachedUdpSocket> {
        SocketIo {
            socket: self.socket.detach(),
            bytes: self.bytes,
            pending: self.pending,
        }
    }
}

impl SocketIo<DetachedUdpSocket> {
    pub fn activate(self) -> Result<SocketIo, (Box<Self>, io::Error)> {
        let Self {
            socket,
            bytes,
            pending,
        } = self;
        match socket.activate() {
            Ok(socket) => Ok(SocketIo {
                socket,
                bytes,
                pending,
            }),
            Err((socket, error)) => Err((
                Box::new(Self {
                    socket: *socket,
                    bytes,
                    pending,
                }),
                error,
            )),
        }
    }
}
