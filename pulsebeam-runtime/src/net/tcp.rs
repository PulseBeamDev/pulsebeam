use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use std::sync::Arc;
use std::{io, net::SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct TcpTransport {
    local_addr: SocketAddr,
    // Shared ingress for all remote peers
    packet_rx: async_channel::Receiver<RecvPacketBatch>,
    // Per-peer egress channels
    conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
    readable_notifier: Arc<tokio::sync::Notify>,
    writable_notifier: Arc<tokio::sync::Notify>,
}

impl TcpTransport {
    pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = external_addr.unwrap_or(listener.local_addr()?);

        let (packet_tx, packet_rx) = async_channel::bounded(BATCH_SIZE * 64);
        let conns = Arc::new(DashMap::new());
        let readable_notifier = Arc::new(tokio::sync::Notify::new());
        let writable_notifier = Arc::new(tokio::sync::Notify::new());

        let conns_clone = conns.clone();
        let r_notify = readable_notifier.clone();
        let w_notify = writable_notifier.clone();

        // Passive Listener Task (RFC 6544)
        tokio::spawn(async move {
            while let Ok((stream, peer_addr)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_linger(None);

                Self::handle_new_connection(
                    stream,
                    peer_addr,
                    local_addr,
                    packet_tx.clone(),
                    conns_clone.clone(),
                    r_notify.clone(),
                    w_notify.clone(),
                );
                w_notify.notify_waiters();
            }
        });

        Ok(Self {
            local_addr,
            packet_rx,
            conns,
            readable_notifier,
            writable_notifier,
        })
    }

    fn handle_new_connection(
        stream: TcpStream,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        packet_tx: async_channel::Sender<RecvPacketBatch>,
        conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
        r_notify: Arc<tokio::sync::Notify>,
        w_notify: Arc<tokio::sync::Notify>,
    ) {
        let (send_tx, send_rx) = async_channel::bounded::<Bytes>(8192);
        conns.insert(peer_addr, send_tx);

        let (mut reader, writer) = stream.into_split();

        // Task: Receiver (RFC 4571 Un-framing)
        tokio::spawn(async move {
            let mut recv_buf = BytesMut::with_capacity(CHUNK_SIZE * 4);
            while let Ok(n) = reader.read_buf(&mut recv_buf).await {
                if n == 0 {
                    break;
                }
                let mut added = false;
                while recv_buf.len() >= 2 {
                    let len = u16::from_be_bytes([recv_buf[0], recv_buf[1]]) as usize;
                    if recv_buf.len() < 2 + len {
                        break;
                    }
                    recv_buf.advance(2);
                    let data = recv_buf.split_to(len).freeze();
                    let _ = packet_tx
                        .send(RecvPacketBatch {
                            src: peer_addr,
                            dst: local_addr,
                            buf: data,
                            stride: len,
                            len,
                            transport: Transport::Tcp,
                        })
                        .await;
                    added = true;
                }
                if added {
                    r_notify.notify_waiters();
                }
            }
            conns.remove(&peer_addr);
        });

        // Task: Sender (RFC 4571 Framing + Syscall Batching)
        tokio::spawn(async move {
            let mut write_buf = Vec::with_capacity(CHUNK_SIZE * 2);
            let mut writer = writer;
            while let Ok(first) = send_rx.recv().await {
                write_buf.clear();
                write_buf.put_u16(first.len() as u16);
                write_buf.put_slice(&first);

                while let Ok(next) = send_rx.try_recv() {
                    write_buf.put_u16(next.len() as u16);
                    write_buf.put_slice(&next);
                    if write_buf.len() > 65535 {
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

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        let Some(peer_tx) = self.conns.get(&batch.dst) else {
            return Ok(true);
        };

        // Atomic check: Ensure enough capacity exists for the whole jumbo batch
        // to prevent partial frames being stuck in the channel.
        let required_slots = (batch.buf.len() + batch.segment_size - 1) / batch.segment_size;
        if peer_tx.capacity().unwrap() - peer_tx.len() < required_slots {
            return Ok(false);
        }

        let mut offset = 0;
        let total_len = batch.buf.len();
        while offset < total_len {
            let end = std::cmp::min(offset + batch.segment_size, total_len);
            let segment = &batch.buf[offset..end];
            // Each segment gets its own Bytes handle so the writer frames it correctly
            let _ = peer_tx.try_send(Bytes::copy_from_slice(segment));
            offset = end;
        }
        Ok(true)
    }

    #[inline]
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
    pub async fn writable(&self) -> io::Result<()> {
        loop {
            if self.conns.is_empty() {
                self.writable_notifier.notified().await;
                continue;
            }
            let mut any_available = false;
            for c in self.conns.iter() {
                if !c.is_full() {
                    any_available = true;
                    break;
                }
            }
            if any_available {
                return Ok(());
            }
            let wait = self.writable_notifier.notified();
            // Re-check after registering waker to prevent race
            let mut any_available = false;
            for c in self.conns.iter() {
                if !c.is_full() {
                    any_available = true;
                    break;
                }
            }
            if any_available {
                return Ok(());
            }
            wait.await;
        }
    }

    #[inline]
    pub fn try_recv_batch(&self, out: &mut Vec<RecvPacketBatch>) -> io::Result<()> {
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

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn max_gso_segments(&self) -> usize {
        64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn test_tcp_rfc_compliance() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let transport = TcpTransport::bind(addr, None).await.unwrap();
        let server_addr = transport.local_addr();

        // 1. Test RFC 6544 (Passive Connection Acceptance)
        let mut client = TcpStream::connect(server_addr).await.unwrap();
        client.set_nodelay(true).unwrap();

        // 2. Test RFC 4571 Ingress (Client -> Server)
        // Send two RTP packets in one stream burst
        let p1 = b"packet1";
        let p2 = b"packet2-longer";
        let mut stream = Vec::new();
        for p in [p1.as_slice(), p2.as_slice()] {
            stream.extend_from_slice(&(p.len() as u16).to_be_bytes());
            stream.extend_from_slice(p);
        }
        client.write_all(&stream).await.unwrap();

        // Server should receive 2 discrete batches
        transport.readable().await.unwrap();
        let mut out = Vec::new();
        transport.try_recv_batch(&mut out).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(&out[0].buf[..], p1);
        assert_eq!(&out[1].buf[..], p2);
        let client_addr = out[0].src;

        // 3. Test RFC 4571 Egress (Server -> Client)
        // Send a GSO-style batch from Server
        let jumbo_payload = b"segment1segment2";
        let batch = SendPacketBatch {
            dst: client_addr,
            buf: jumbo_payload,
            segment_size: 8,
        };

        transport.writable().await.unwrap();
        transport.try_send_batch(&batch).unwrap();

        // Client must see two separate 2-byte length prefixes
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
        let transport = TcpTransport::bind(addr, None).await.unwrap();

        let _c1 = TcpStream::connect(transport.local_addr()).await.unwrap();
        let _c2 = TcpStream::connect(transport.local_addr()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(transport.conns.len(), 2);
    }
}
