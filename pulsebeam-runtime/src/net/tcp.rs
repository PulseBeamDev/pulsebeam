use super::{BATCH_SIZE, GroPayload, RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use bytes::{BufMut, Bytes};
use dashmap::DashMap;
use pulsebeam_core::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const MAX_FRAME_SIZE: usize = 1500;
const MAX_CONNECTIONS: usize = 10_000;
const MAX_CONNS_PER_IP: usize = 20;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state for connection management
type ConnMap = Arc<DashMap<SocketAddr, (async_channel::Sender<Bytes>, CancellationToken)>>;

pub async fn bind(
    addr: SocketAddr,
    external_addr: Option<SocketAddr>,
) -> io::Result<(TcpTransportReader, TcpTransportWriter)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = external_addr.unwrap_or(listener.local_addr()?);

    let (packet_tx, packet_rx) = async_channel::bounded(BATCH_SIZE * 128);
    let conns: ConnMap = Arc::new(DashMap::new());
    let ip_counts = Arc::new(DashMap::<IpAddr, usize>::new());

    let readable_notifier = Arc::new(tokio::sync::Notify::new());
    let writable_notifier = Arc::new(tokio::sync::Notify::new());
    let conn_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    let conns_clone = conns.clone();
    let r_notify = readable_notifier.clone();
    let w_notify = writable_notifier.clone();
    let ip_counts_clone = ip_counts.clone();
    let semaphore_clone = conn_semaphore.clone();

    tracing::info!(
        %addr,
        %local_addr,
        "TCP socket bound"
    );

    // Passive Listener Task
    tokio::spawn(async move {
        while let Ok((stream, peer_addr)) = listener.accept().await {
            let ip = peer_addr.ip();

            // 1. Global Connection Limit
            let permit = match semaphore_clone.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(%peer_addr, "Rejecting: Global limit reached");
                    continue;
                }
            };

            // 2. Per-IP Limit
            {
                let mut count = ip_counts_clone.entry(ip).or_insert(0);
                if *count >= MAX_CONNS_PER_IP {
                    tracing::warn!(%peer_addr, "Rejecting: Per-IP limit reached");
                    continue;
                }
                *count += 1;
            }

            let _ = stream.set_nodelay(true);

            handle_new_connection(
                stream,
                peer_addr,
                local_addr,
                packet_tx.clone(),
                conns_clone.clone(),
                ip_counts_clone.clone(),
                r_notify.clone(),
                w_notify.clone(),
                permit,
            );
            w_notify.notify_waiters();
        }
    });

    let reader = TcpTransportReader {
        local_addr,
        packet_rx,
        readable_notifier,
        conns: conns.clone(), // Reader now has access to conns
    };

    let writer = TcpTransportWriter {
        local_addr,
        conns,
        writable_notifier,
    };

    Ok((reader, writer))
}

pub struct TcpTransportReader {
    packet_rx: async_channel::Receiver<RecvPacketBatch>,
    readable_notifier: Arc<tokio::sync::Notify>,
    local_addr: SocketAddr,
    /// Shared connection map to allow forced disconnects
    conns: ConnMap,
}

impl TcpTransportReader {
    /// Closes the connection for a specific peer.
    /// Useful when high-level demux or auth fails.
    pub fn close_peer(&self, peer_addr: &SocketAddr) {
        if let Some((_, (tx, cancel))) = self.conns.remove(peer_addr) {
            cancel.cancel(); // Stops the background reader task
            tx.close(); // Stops the background writer task
            tracing::info!(%peer_addr, "Reader forced connection close");
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn readable(&self) -> io::Result<()> {
        loop {
            if !self.packet_rx.is_empty() {
                return Ok(());
            }
            let wait = self.readable_notifier.notified();
            if !self.packet_rx.is_empty() {
                return Ok(());
            }
            wait.await;
        }
    }

    #[inline]
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<()> {
        let mut count = 0;
        while let Ok(pkt) = self.packet_rx.try_recv() {
            out.push(pkt);
            count += 1;
            if count >= BATCH_SIZE {
                break;
            }
        }
        if out.is_empty() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct TcpTransportWriter {
    local_addr: SocketAddr,
    conns: ConnMap,
    writable_notifier: Arc<tokio::sync::Notify>,
}

impl TcpTransportWriter {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        64
    }

    pub async fn writable(&self) -> io::Result<()> {
        loop {
            if self.conns.is_empty() {
                self.writable_notifier.notified().await;
                continue;
            }
            let mut any_available = false;
            for c in self.conns.iter() {
                if !c.value().0.is_full() {
                    any_available = true;
                    break;
                }
            }
            if any_available {
                return Ok(());
            }
            let wait = self.writable_notifier.notified();
            wait.await;
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        let Some(peer_entry) = self.conns.get(&batch.dst) else {
            return Ok(true);
        };
        let (peer_tx, _) = peer_entry.value();

        let required_slots = batch.buf.len().div_ceil(batch.segment_size);
        if peer_tx.capacity().unwrap() - peer_tx.len() < required_slots {
            return Ok(false);
        }

        let mut offset = 0;
        while offset < batch.buf.len() {
            let end = std::cmp::min(offset + batch.segment_size, batch.buf.len());
            let _ = peer_tx.try_send(Bytes::copy_from_slice(&batch.buf[offset..end]));
            offset = end;
        }
        Ok(true)
    }
}

fn handle_new_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    packet_tx: async_channel::Sender<RecvPacketBatch>,
    conns: ConnMap,
    ip_counts: Arc<DashMap<IpAddr, usize>>,
    r_notify: Arc<tokio::sync::Notify>,
    w_notify: Arc<tokio::sync::Notify>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let (send_tx, send_rx) = async_channel::bounded::<Bytes>(1024);
    let cancel_token = CancellationToken::new();

    conns.insert(peer_addr, (send_tx, cancel_token.clone()));

    let (mut tcp_reader, tcp_writer) = stream.into_split();
    let peer_ip = peer_addr.ip();

    // Task: Receiver (RFC 4571 Un-framing)
    let r_cancel = cancel_token.clone();
    let r_conns = conns.clone();
    let r_ip_counts = ip_counts.clone();
    tokio::spawn(async move {
        // Guard to release semaphore and cleanup DashMap on task exit
        let _guard = (permit, r_cancel);
        let mut recv_buf = Vec::with_capacity(MAX_FRAME_SIZE + 2);

        loop {
            tokio::select! {
                _ = _guard.1.cancelled() => break,
                res = tokio::time::timeout(READ_TIMEOUT, tcp_reader.read_buf(&mut recv_buf)) => {
                    let _n = match res {
                        Ok(Ok(n)) if n > 0 => n,
                        _ => break, // Timeout, Error, or EOF
                    };

                    while recv_buf.len() >= 2 {
                        let len = u16::from_be_bytes([recv_buf[0], recv_buf[1]]) as usize;

                        if len > MAX_FRAME_SIZE || len == 0 {
                            tracing::warn!(%peer_addr, len, "Invalid TCP frame size, dropping connection");
                            return;
                        }

                        if recv_buf.len() < 2 + len { break; }

                        let mut without_header = recv_buf.split_off(2); // [2..] — skip length prefix
                        recv_buf = without_header.split_off(len);

                        // Use try_send to prevent reader task from blocking if SFU logic lags
                        if let Err(_) = packet_tx.try_send(RecvPacketBatch {
                            src: peer_addr,
                            dst: local_addr,
                                payload: GroPayload {
                                buf: without_header,
                                stride: len,
                                len,
                            },
                            transport: Transport::Tcp,
                        }) {
                            tracing::debug!("TCP packet dropped: Global queue full");
                        } else {
                            r_notify.notify_waiters();
                        }
                    }
                }
            }
        }

        // Final cleanup
        r_conns.remove(&peer_addr);
        if let Some(mut count) = r_ip_counts.get_mut(&peer_ip) {
            *count = count.saturating_sub(1);
        }
    });

    // Task: Sender
    tokio::spawn(async move {
        let mut write_buf = Vec::with_capacity(MAX_FRAME_SIZE + 2);
        let mut writer = tcp_writer;

        while let Ok(first) = send_rx.recv().await {
            write_buf.clear();
            write_buf.put_u16(first.len() as u16);
            write_buf.put_slice(&first);

            while let Ok(next) = send_rx.try_recv() {
                write_buf.put_u16(next.len() as u16);
                write_buf.put_slice(&next);
                if write_buf.len() > 16384 {
                    break;
                }
            }

            if writer.write_all(&write_buf).await.is_err() {
                break;
            }
            w_notify.notify_waiters();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn test_tcp_rfc_compliance() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        // 1. Setup: Bind returns the split pair
        let (mut reader, writer) = bind(addr, None).await.unwrap();
        let server_addr = writer.local_addr();

        // 2. Test RFC 6544 (Passive Connection Acceptance)
        let mut client = TcpStream::connect(server_addr).await.unwrap();
        client.set_nodelay(true).unwrap();

        // 3. Test RFC 4571 Ingress (Client -> Server)
        let p1 = b"packet1";
        let p2 = b"packet2-longer";
        let mut stream = Vec::new();
        for p in [p1.as_slice(), p2.as_slice()] {
            stream.extend_from_slice(&(p.len() as u16).to_be_bytes());
            stream.extend_from_slice(p);
        }
        client.write_all(&stream).await.unwrap();

        // Server Reader: Wait and receive
        reader.readable().await.unwrap();
        let mut out = Vec::new();
        reader.try_recv_batch(&mut out).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(&out[0].payload.buf[..], p1);
        assert_eq!(&out[1].payload.buf[..], p2);
        let client_addr = out[0].src;

        // 4. Test RFC 4571 Egress (Server -> Client)
        let jumbo_payload = b"segment1segment2";
        let batch = SendPacketBatch {
            dst: client_addr,
            buf: jumbo_payload,
            segment_size: 8,
        };

        // Server Writer: Wait and send
        writer.writable().await.unwrap();
        writer.try_send_batch(&batch).unwrap();

        // Client must see two separate framed packets
        for expected in [b"segment1", b"segment2"] {
            let len = client.read_u16().await.unwrap();
            assert_eq!(len as usize, expected.len());
            let mut buf = vec![0u8; len as usize];
            client.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, expected);
        }
    }

    #[tokio::test]
    async fn test_tcp_multi_peer_isolation() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (_reader, writer) = bind(addr, None).await.unwrap();

        // Connect two separate clients
        let _c1 = TcpStream::connect(writer.local_addr()).await.unwrap();
        let _c2 = TcpStream::connect(writer.local_addr()).await.unwrap();

        // Small sleep to let the background listener task process the accepts
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Accessing internal 'conns' map (permitted in internal test module)
        assert_eq!(writer.conns.len(), 2);
    }

    #[tokio::test]
    async fn test_writer_cloning() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (_reader, writer) = bind(addr, None).await.unwrap();

        // Verify writer can be cloned and moved to another task
        let writer_clone = writer.clone();
        let handle = tokio::spawn(async move {
            let _ = writer_clone.local_addr();
            // In a real test we'd perform a send here
        });

        handle.await.unwrap();
    }
}
