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
