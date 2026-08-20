//! Simulation of the production batched UDP contract.
//!
//! Turmoil provides a datagram socket, not Linux's `recvmmsg`/`sendmmsg` and
//! ancillary-data APIs. The simulator therefore uses the shared batched
//! receive/send representation and the same GSO/GRO decisions as the scalar
//! simulation adapter, while keeping the backend selection explicit as
//! `UdpMode::Batch`. Production code above this boundary sees no simulator
//! feature branches.

use super::{RecvPacketBatch, SendPacketBatch, UdpMode};
use crate::sync::Arc;
use pulsebeam_core::net::UdpSocket;
use std::{io, net::SocketAddr};

pub struct UdpTransport {
    inner: super::udp_scalar::UdpTransport,
}

impl UdpTransport {
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    pub fn max_gso_segments(&self) -> usize {
        self.inner.max_gso_segments()
    }

    pub async fn readable(&self) -> io::Result<()> {
        self.inner.readable().await
    }

    pub async fn writable(&self) -> io::Result<()> {
        self.inner.writable().await
    }

    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        self.inner.try_recv_batch(out)
    }

    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<usize> {
        self.inner.try_send_batch(batch)
    }

    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        self.inner.close_peer(peer_addr);
    }
}

pub(crate) fn from_reuseport_member(
    socket: Arc<UdpSocket>,
    external_addr: Option<SocketAddr>,
    member: super::bound_udp::ReuseportMember,
) -> io::Result<UdpTransport> {
    let inner = super::udp_scalar::from_reuseport_member_with_mode(
        socket,
        external_addr,
        member,
        UdpMode::Batch,
    )?;
    Ok(UdpTransport { inner })
}
