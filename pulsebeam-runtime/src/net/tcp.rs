use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use std::sync::Arc;
use std::{io, net::SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Creates a TCP Reader and Writer pair.
pub async fn bind(
    addr: SocketAddr,
    external_addr: Option<SocketAddr>,
) -> io::Result<(TcpTransportReader, TcpTransportWriter)> {
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

            handle_new_connection(
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

    let reader = TcpTransportReader {
        local_addr,
        packet_rx,
        readable_notifier,
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
}

impl TcpTransportReader {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Waits until the socket is readable.
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

    /// Pulls packets from the internal channel into the provided vector.
    /// Takes `&mut self` as requested for the Reader API.
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
    conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
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
                if !c.is_full() {
                    any_available = true;
                    break;
                }
            }
            if any_available {
                return Ok(());
            }
            let wait = self.writable_notifier.notified();

            // Re-check logic to prevent race conditions
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
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        let Some(peer_tx) = self.conns.get(&batch.dst) else {
            // If the peer is gone, we drop the packet (consistent with UDP behavior)
            return Ok(true);
        };

        let required_slots = (batch.buf.len() + batch.segment_size - 1) / batch.segment_size;
        if peer_tx.capacity().unwrap() - peer_tx.len() < required_slots {
            return Ok(false);
        }

        let mut offset = 0;
        let total_len = batch.buf.len();
        while offset < total_len {
            let end = std::cmp::min(offset + batch.segment_size, total_len);
            let segment = &batch.buf[offset..end];
            let _ = peer_tx.try_send(Bytes::copy_from_slice(segment));
            offset = end;
        }
        Ok(true)
    }
}

/// Background connection handler
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
        assert_eq!(&out[0].buf[..], p1);
        assert_eq!(&out[1].buf[..], p2);
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
