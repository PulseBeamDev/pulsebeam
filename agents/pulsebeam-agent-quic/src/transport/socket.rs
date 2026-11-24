use noq_proto::{EcnCodepoint, Transmit};
use pulsebeam_udp::{BindOptions, Mode, SendStop, UdpSocket};
use std::io;
use std::net::SocketAddr;

use super::UDP_PAYLOAD_BYTES;

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

pub async fn bind(address: SocketAddr) -> io::Result<pulsebeam_udp::BoundUdpSocket> {
    if address.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QUIC requires an explicit local address",
        ));
    }
    let mut options = BindOptions::new(address);
    options.max_datagram_bytes = usize::from(UDP_PAYLOAD_BYTES);
    if cfg!(all(target_os = "linux", not(feature = "portable-scalar"))) {
        options.nonfragmenting_max_datagram_bytes = std::num::NonZeroU16::new(UDP_PAYLOAD_BYTES);
        options.receive_ecn = true;
    } else {
        // Without ancillary data or a DF option, cap Noq transmits in Egress
        // and leave fragmentation policy to the OS. Never probe a larger size.
        options.mode = Mode::Scalar;
    }
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
            || (!local_addr.ip().is_unspecified()
                && tx
                    .src_ip
                    .is_some_and(|ip| ip.to_canonical() != local_addr.ip().to_canonical()))
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
    /// the owner must fence the association rather than retry at another size.
    pub fn flush(&mut self, socket: &mut UdpSocket) -> io::Result<Flush> {
        let Some(tx) = &self.pending else {
            return Ok(Flush::Empty);
        };
        let report = socket.try_send_one(
            tx.destination,
            &self.bytes,
            tx.src_ip,
            tx.ecn == Some(EcnCodepoint::Ect0) && socket.supports_ect0(),
        );
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
