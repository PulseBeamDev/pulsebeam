use axum::serve::Listener;
use pulsebeam::net::PacketSocket;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
pub struct VirtualUdpSocket(pub Arc<turmoil::net::UdpSocket>);

impl PacketSocket for VirtualUdpSocket {
    fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.0.local_addr()
    }

    fn send_to(
        &self,
        buf: &[u8],
        addr: SocketAddr,
    ) -> impl Future<Output = std::io::Result<usize>> {
        self.0.send_to(buf, addr)
    }

    fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> impl Future<Output = std::io::Result<(usize, SocketAddr)>> {
        self.0.recv_from(buf)
    }
}

pub struct VirtualTcpListener(pub turmoil::net::TcpListener);

impl Listener for VirtualTcpListener {
    type Io = turmoil::net::TcpStream;

    type Addr = SocketAddr;

    fn accept(&mut self) -> impl std::future::Future<Output = (Self::Io, Self::Addr)> + Send {
        async move { self.0.accept().await.unwrap() }
    }

    fn local_addr(&self) -> tokio::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
