use std::{io, net::SocketAddr, sync::Arc};

use crate::net::PacketSocket;

#[derive(Clone)]
pub struct VirtualUdpSocket(Arc<turmoil::net::UdpSocket>);

impl From<Arc<turmoil::net::UdpSocket>> for VirtualUdpSocket {
    fn from(value: Arc<turmoil::net::UdpSocket>) -> Self {
        VirtualUdpSocket(value)
    }
}

impl PacketSocket for VirtualUdpSocket {
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> {
        self.0.recv_from(buf)
    }

    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> impl Future<Output = io::Result<usize>> {
        self.0.send_to(buf, addr)
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.0.local_addr()
    }
}
