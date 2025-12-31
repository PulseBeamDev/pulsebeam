use std::io::Result;
use std::net::SocketAddr;
#[cfg(not(feature = "sim"))]
use tokio::net;
#[cfg(feature = "sim")]
use turmoil::net;

pub use net::*;

// pub trait UdpSocket: Send + Sync + 'static {
//     fn local_addr(&self) -> Result<SocketAddr>;
//     fn recv_from(&self, buf: &mut [u8])
//     -> impl Future<Output = Result<(usize, SocketAddr)>> + Send;
//     fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize>;
// }
//
// #[cfg(feature = "sim")]
// impl UdpSocket for net::UdpSocket {
//     fn local_addr(&self) -> Result<SocketAddr> {
//         self.local_addr()
//     }
//
//     async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
//         self.recv_from(buf).await
//     }
//
//     fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
//         self.try_send_to(buf, target)
//     }
// }
