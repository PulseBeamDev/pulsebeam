use super::{UdpMode, udp_scalar};

#[cfg(not(feature = "sim"))]
use super::bind_scalar_socket;
#[cfg(not(feature = "sim"))]
use std::{io, net::SocketAddr};

pub use udp_scalar::UdpTransport;

pub const MODE: UdpMode = UdpMode::Scalar;

#[cfg(not(feature = "sim"))]
pub fn bind_socket(addr: SocketAddr) -> io::Result<socket2::Socket> {
    bind_scalar_socket(addr)
}

#[cfg(not(feature = "sim"))]
pub fn from_socket(
    socket: socket2::Socket,
    external_addr: Option<SocketAddr>,
) -> io::Result<UdpTransport> {
    udp_scalar::from_socket(
        tokio::net::UdpSocket::from_std(socket.into())?,
        external_addr,
    )
}
