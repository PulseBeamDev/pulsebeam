use super::{UdpMode, UnifiedSocket, bind_scalar_socket, udp, udp_scalar};
use std::{io, net::SocketAddr};

pub struct BoundUdpSocket {
    socket: socket2::Socket,
    mode: UdpMode,
    external_addr: Option<SocketAddr>,
    local_addr: SocketAddr,
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn into_unified_socket(self) -> io::Result<UnifiedSocket> {
        match self.mode {
            UdpMode::Batch => Ok(UnifiedSocket::Udp(udp::from_socket(
                self.socket,
                self.external_addr,
            )?)),
            UdpMode::Scalar => Ok(UnifiedSocket::UdpScalar(udp_scalar::from_socket(
                tokio::net::UdpSocket::from_std(self.socket.into())?,
                self.external_addr,
            )?)),
        }
    }
}

pub async fn bind_udp_socket(
    addr: SocketAddr,
    mode: UdpMode,
    external_addr: Option<SocketAddr>,
) -> io::Result<BoundUdpSocket> {
    let socket = match mode {
        UdpMode::Batch => udp::bind_socket(addr)?,
        UdpMode::Scalar => bind_scalar_socket(addr)?,
    };
    let local_addr = match external_addr {
        Some(addr) => addr,
        None => match socket.local_addr()?.as_socket() {
            Some(addr) => addr,
            None => crate::fatal!("a bound UDP socket reported a non-IP local address"),
        },
    };
    Ok(BoundUdpSocket {
        socket,
        mode,
        external_addr,
        local_addr,
    })
}
