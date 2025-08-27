use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,
    Tcp,
    Tls,
}

// ======================== UDP ========================
pub struct UdpSocketImpl {
    socket: Arc<UdpSocket>,
}

impl UdpSocketImpl {
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(local_addr).await?);

        Ok(Self { socket })
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(buf, addr).await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

// ======================== TCP ========================
// TODO: Implement TCP Framing:
// * https://datatracker.ietf.org/doc/html/rfc6544
// * https://datatracker.ietf.org/doc/html/rfc4571
//
// Supported TCP mode:
// * Passive: Yes
// * Active: No
// * SO: No
pub struct TcpSocketImpl {}

struct TcpConnection {
    tx: mpsc::Sender<Bytes>,
}

impl TcpSocketImpl {
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        todo!()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        todo!()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        todo!()
    }
}

// ======================== TLS ========================
pub struct TlsSocketImpl {}

struct TlsConnection {}

impl TlsSocketImpl {
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        todo!()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        todo!()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        todo!()
    }
}

// ======================== UnifiedSocket ========================
pub enum UnifiedSocket {
    Udp(UdpSocketImpl),
    Tcp(TcpSocketImpl),
    Tls(TlsSocketImpl),
}

impl UnifiedSocket {
    pub async fn bind(local_addr: SocketAddr, transport: Transport) -> io::Result<Self> {
        Ok(match transport {
            Transport::Udp => UnifiedSocket::Udp(UdpSocketImpl::new(local_addr).await?),
            Transport::Tcp => UnifiedSocket::Tcp(TcpSocketImpl::new(local_addr).await?),
            Transport::Tls => UnifiedSocket::Tls(TlsSocketImpl::new(local_addr).await?),
        })
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            UnifiedSocket::Udp(s) => s.recv_from(buf).await,
            UnifiedSocket::Tcp(s) => s.recv_from(buf).await,
            UnifiedSocket::Tls(s) => s.recv_from(buf).await,
        }
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        match self {
            UnifiedSocket::Udp(s) => s.send_to(buf, addr).await,
            UnifiedSocket::Tcp(s) => s.send_to(buf, addr).await,
            UnifiedSocket::Tls(s) => s.send_to(buf, addr).await,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            UnifiedSocket::Udp(s) => s.local_addr(),
            UnifiedSocket::Tcp(s) => s.local_addr(),
            UnifiedSocket::Tls(s) => s.local_addr(),
        }
    }
}
