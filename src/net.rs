use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

pub trait PacketSocket: Send + Sync + Clone + 'static {
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>>;
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> impl Future<Output = io::Result<usize>>;
    fn local_addr(&self) -> Result<SocketAddr, io::Error>;
}

#[derive(Clone)]
pub struct UdpSocket(Arc<tokio::net::UdpSocket>);

impl From<Arc<tokio::net::UdpSocket>> for UdpSocket {
    fn from(value: Arc<tokio::net::UdpSocket>) -> Self {
        UdpSocket(value)
    }
}

impl PacketSocket for UdpSocket {
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
