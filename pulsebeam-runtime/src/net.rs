use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;

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

#[derive(Clone)]
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

#[derive(Clone)]
pub struct SimUdpSocketImpl {
    socket: Arc<turmoil::net::UdpSocket>,
}

impl SimUdpSocketImpl {
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        let socket = Arc::new(turmoil::net::UdpSocket::bind(local_addr).await?);

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

// ======================== UnifiedSocket ========================
#[derive(Clone)]
pub enum UnifiedSocket {
    Udp(UdpSocketImpl),
    SimUdp(SimUdpSocketImpl),
}

impl UnifiedSocket {
    pub async fn bind(local_addr: SocketAddr, transport: Transport) -> io::Result<Self> {
        Ok(match transport {
            Transport::Udp => UnifiedSocket::Udp(UdpSocketImpl::new(local_addr).await?),
            Transport::SimUdp => UnifiedSocket::SimUdp(SimUdpSocketImpl::new(local_addr).await?),
            Transport::Tcp | Transport::Tls => todo!(),
        })
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            UnifiedSocket::Udp(s) => s.recv_from(buf).await,
            UnifiedSocket::SimUdp(s) => s.recv_from(buf).await,
        }
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        match self {
            UnifiedSocket::Udp(s) => s.send_to(buf, addr).await,
            UnifiedSocket::SimUdp(s) => s.send_to(buf, addr).await,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            UnifiedSocket::Udp(s) => s.local_addr(),
            UnifiedSocket::SimUdp(s) => s.local_addr(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use turmoil::Builder;

    // Test binding a SimUdp socket
    #[test]
    fn bind_sim_udp_socket() {
        let mut sim = Builder::new().build();

        sim.client("client", async {
            let socket = UnifiedSocket::bind((Ipv4Addr::UNSPECIFIED, 0).into(), Transport::SimUdp)
                .await
                .unwrap();
            assert!(matches!(socket, UnifiedSocket::SimUdp(_)));
            let addr = socket.local_addr().unwrap();
            assert_eq!(addr.ip(), Ipv4Addr::UNSPECIFIED);
            assert!(addr.port() > 0);
            Ok(())
        });

        sim.run().unwrap();
    }

    // Test local address retrieval
    #[test]
    fn sim_udp_local_addr() {
        let mut sim = Builder::new().build();
        let bind_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 54321).into();

        sim.client("client", async move {
            let socket = UnifiedSocket::bind(bind_addr, Transport::SimUdp)
                .await
                .unwrap();
            let local_addr = socket.local_addr().unwrap();
            assert_eq!(local_addr, bind_addr);
            Ok(())
        });

        sim.run().unwrap();
    }
}
