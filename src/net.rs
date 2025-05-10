use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

pub trait PacketSocket: Send + Sync + Clone + 'static {
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>>;
    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> impl Future<Output = io::Result<usize>>;
}

#[derive(Clone)]
pub struct UdpSocket(Arc<tokio::net::UdpSocket>);

impl Into<UdpSocket> for Arc<tokio::net::UdpSocket> {
    fn into(self) -> UdpSocket {
        UdpSocket(self)
    }
}

impl PacketSocket for UdpSocket {
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> {
        self.0.recv_from(buf)
    }

    fn send_to(&self, buf: &[u8], addr: SocketAddr) -> impl Future<Output = io::Result<usize>> {
        self.0.send_to(buf, addr)
    }
}

pub type Packet = (SocketAddr, Vec<u8>);

#[derive(Clone)]
pub struct SimulatedSocket {
    send_ch: async_channel::Sender<Packet>,
    recv_ch: async_channel::Receiver<Packet>,
}

impl SimulatedSocket {
    pub fn new(cap: usize) -> Self {
        let (tx, rx) = async_channel::bounded(cap);
        Self {
            send_ch: tx,
            recv_ch: rx,
        }
    }
}

impl PacketSocket for SimulatedSocket {
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (src, packet) = self
            .recv_ch
            .recv()
            .await
            .map_err(|_| io::Error::other("closed"))?;

        let size = buf.len().min(packet.len());
        let sliced = &mut buf[..size];
        sliced.copy_from_slice(&packet);
        Ok((size, src))
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.send_ch
            .send((addr, buf.to_vec()))
            .await
            .map_err(|_| io::Error::other("closed"))?;
        Ok(buf.len())
    }
}
