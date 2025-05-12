use pulsebeam::net::PacketSocket;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::{io, sync::Mutex, time::Instant};

pub type Packet = (SocketAddr, Vec<u8>, Instant);

/// Hub that holds all inboxes and injects uniform latency
#[derive(Clone)]
pub struct VirtualNetwork {
    latency: Duration,
    inboxes: Arc<Mutex<HashMap<SocketAddr, flume::Sender<Packet>>>>,
}

impl VirtualNetwork {
    pub fn new(latency: Duration) -> Self {
        Self {
            latency,
            inboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a socket under `addr`, returning its receiver
    async fn register(&self, addr: SocketAddr) -> flume::Receiver<Packet> {
        let (tx, rx) = flume::unbounded();
        self.inboxes.lock().await.insert(addr, tx);
        rx
    }

    /// Enqueue a packet into the target’s queue
    async fn deliver(&self, from: SocketAddr, to: SocketAddr, data: Vec<u8>) {
        if let Some(tx) = self.inboxes.lock().await.get(&to) {
            let when = Instant::now() + self.latency;
            let _ = tx.send((from, data, when));
        }
    }
}

/// Simulated UDP socket backed by the Hub
#[derive(Clone)]
pub struct VirtualSocket {
    vn: VirtualNetwork,
    addr: SocketAddr,
    rx: flume::Receiver<Packet>,
}

impl VirtualSocket {
    /// Create & register a new socket at `addr`
    pub async fn register(vn: VirtualNetwork, addr: SocketAddr) -> Self {
        let rx = vn.register(addr).await;
        VirtualSocket { vn, addr, rx }
    }
}

impl PacketSocket for VirtualSocket {
    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        Ok(self.addr)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.vn.deliver(self.addr, target, buf.to_vec()).await;
        Ok(buf.len())
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let (src, pkt, when) = self.rx.recv_async().await.unwrap();
            if when > Instant::now() {
                tokio::time::sleep_until(when).await;
            }
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            return Ok((n, src));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::time::{Duration, Instant, timeout};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[tokio::test]
    async fn test_socket_registration() {
        let vn = VirtualNetwork::new(Duration::from_millis(10));
        let sock = VirtualSocket::register(vn.clone(), addr(10000)).await;
        assert_eq!(sock.local_addr().unwrap(), addr(10000));
    }

    #[tokio::test]
    async fn test_send_and_receive_packet() {
        let vn = VirtualNetwork::new(Duration::from_millis(0));
        let a = VirtualSocket::register(vn.clone(), addr(10001)).await;
        let b = VirtualSocket::register(vn.clone(), addr(10002)).await;

        let msg = b"hello world";
        a.send_to(msg, b.local_addr().unwrap()).await.unwrap();

        let mut buf = [0u8; 1024];
        let (len, from) = b.recv_from(&mut buf).await.unwrap();

        assert_eq!(&buf[..len], msg);
        assert_eq!(from, a.local_addr().unwrap());
    }

    #[tokio::test]
    async fn test_uniform_latency() {
        let latency = Duration::from_millis(100);
        let vn = VirtualNetwork::new(latency);
        let a = VirtualSocket::register(vn.clone(), addr(10003)).await;
        let b = VirtualSocket::register(vn.clone(), addr(10004)).await;

        let msg = b"latency test";
        let start = Instant::now();
        a.send_to(msg, b.local_addr().unwrap()).await.unwrap();

        let mut buf = [0u8; 1024];
        let (_len, _from) = b.recv_from(&mut buf).await.unwrap();
        let elapsed = start.elapsed();

        assert!(elapsed >= latency, "Elapsed: {:?}", elapsed);
    }

    #[tokio::test]
    async fn test_delivery_to_unregistered_socket() {
        let vn = VirtualNetwork::new(Duration::from_millis(10));
        let sock = VirtualSocket::register(vn.clone(), addr(10005)).await;

        // Sending to an unregistered address should not panic
        let result = sock.send_to(b"no target", addr(9999)).await;
        assert_eq!(result.unwrap(), b"no target".len());
    }

    #[tokio::test]
    async fn test_receive_blocks_until_packet_arrives() {
        let vn = VirtualNetwork::new(Duration::from_millis(50));
        let a = VirtualSocket::register(
            vn.clone(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 3478),
        )
        .await;
        let b = VirtualSocket::register(
            vn.clone(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 2)), 3478),
        )
        .await;

        tokio::spawn({
            let a = a.clone();
            let b = b.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let _ = a.send_to(b"delayed", b.local_addr().unwrap()).await;
            }
        });

        let mut buf = [0u8; 1024];
        let timeout_result = timeout(Duration::from_millis(100), b.recv_from(&mut buf)).await;
        assert!(timeout_result.is_ok());
        assert_eq!(&buf[..7], b"delayed");
    }
}

