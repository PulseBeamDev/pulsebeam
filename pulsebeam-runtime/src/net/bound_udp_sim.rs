use super::{UdpMode, UnifiedSocket, udp_scalar};
use std::{io, net::SocketAddr};

pub struct BoundUdpSocket {
    socket: udp_scalar::UdpTransport,
    local_addr: SocketAddr,
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn into_unified_socket(self) -> io::Result<UnifiedSocket> {
        Ok(UnifiedSocket::UdpScalar(self.socket))
    }
}

pub async fn bind_udp_socket(
    addr: SocketAddr,
    mode: UdpMode,
    external_addr: Option<SocketAddr>,
) -> io::Result<BoundUdpSocket> {
    if mode == UdpMode::Batch {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "batched UDP is unavailable in simulation",
        ));
    }
    let socket = udp_scalar::bind(addr, external_addr).await?;
    let local_addr = socket.local_addr();
    Ok(BoundUdpSocket { socket, local_addr })
}
