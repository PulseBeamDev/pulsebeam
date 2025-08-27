use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,
    Tcp,
    Tls,
}

// ======================== UDP ========================
pub struct UdpSocketImpl {
    socket: Arc<UdpSocket>,
    incoming: Mutex<mpsc::Receiver<(SocketAddr, Bytes)>>,
}

impl UdpSocketImpl {
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(local_addr).await?);
        let (tx, rx) = mpsc::channel(128);
        let recv_socket = socket.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((len, addr)) => {
                        let _ = tx.send((addr, Bytes::copy_from_slice(&buf[..len]))).await;
                    }
                    Err(e) => eprintln!("UDP recv error: {}", e),
                }
            }
        });

        Ok(Self {
            socket,
            incoming: Mutex::new(rx),
        })
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut rx = self.incoming.lock().await;
        let (addr, data) = rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "Receive channel closed"))?;
        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok((len, addr))
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
