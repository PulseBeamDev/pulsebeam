use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use std::sync::Arc;
use std::{io, net::SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

/// TcpTransport handles multiple remote peers over a single "socket" abstraction.
/// It implements RFC 4571 (framing) and is optimized for SFU media forwarding.
pub struct TcpTransport {
    local_addr: SocketAddr,
    // Shared queue for all incoming packets from all TCP peers
    packet_rx: async_channel::Receiver<RecvPacketBatch>,
    // Map of remote address to the individual bounded channel for that peer
    conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
    // Signal for the main SFU loop when any packet arrives
    readable_notifier: Arc<tokio::sync::Notify>,
    // Signal for the main SFU loop when any peer channel has cleared space
    writable_notifier: Arc<tokio::sync::Notify>,
}

impl TcpTransport {
    pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = external_addr.unwrap_or(listener.local_addr()?);

        // Global ingress queue. Sized to handle bursts from many peers.
        let (packet_tx, packet_rx) = async_channel::bounded(BATCH_SIZE * 64);
        let conns = Arc::new(DashMap::new());
        let readable_notifier = Arc::new(tokio::sync::Notify::new());
        let writable_notifier = Arc::new(tokio::sync::Notify::new());

        let conns_clone = conns.clone();
        let r_notify = readable_notifier.clone();
        let w_notify = writable_notifier.clone();

        // Passive Listener Task
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

                // Allow the SFU loop to start sending to this new peer immediately
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
        // Individual egress channel per peer.
        // 8192 is large enough to buffer a high-bitrate video keyframe burst.
        let (send_tx, send_rx) = async_channel::bounded::<Bytes>(8192);
        conns.insert(peer_addr, send_tx);

        let (reader, writer) = stream.into_split();

        // Task: Receiver (Remote Socket -> SFU Ingress)
        tokio::spawn(async move {
            let _ = Self::read_loop(reader, peer_addr, local_addr, packet_tx, r_notify).await;
            conns.remove(&peer_addr);
        });

        // Task: Sender (SFU Egress -> Remote Socket)
        tokio::spawn(async move {
            let _ = Self::write_loop(writer, send_rx, w_notify).await;
        });
    }

    async fn read_loop(
        mut reader: OwnedReadHalf,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        packet_tx: async_channel::Sender<RecvPacketBatch>,
        notifier: Arc<tokio::sync::Notify>,
    ) -> io::Result<()> {
        let mut recv_buf = BytesMut::with_capacity(CHUNK_SIZE * 4);
        loop {
            let n = reader.read_buf(&mut recv_buf).await?;
            if n == 0 {
                return Ok(());
            }

            let mut packet_added = false;
            while recv_buf.len() >= 2 {
                let len = u16::from_be_bytes([recv_buf[0], recv_buf[1]]) as usize;
                if recv_buf.len() < 2 + len {
                    break;
                }

                recv_buf.advance(2);
                let data = recv_buf.split_to(len).freeze();

                let pkt = RecvPacketBatch {
                    src: peer_addr,
                    dst: local_addr,
                    buf: data,
                    stride: len,
                    len,
                    transport: Transport::Tcp,
                };

                // Send to the shared ingress channel
                if packet_tx.send(pkt).await.is_err() {
                    return Ok(());
                }
                packet_added = true;
            }

            if packet_added {
                notifier.notify_waiters();
            }
        }
    }

    async fn write_loop(
        mut writer: OwnedWriteHalf,
        send_rx: async_channel::Receiver<Bytes>,
        notifier: Arc<tokio::sync::Notify>,
    ) -> io::Result<()> {
        let mut write_buf = Vec::with_capacity(CHUNK_SIZE * 2);

        loop {
            // Wait for media packets
            let first_pkt = send_rx
                .recv()
                .await
                .map_err(|_| io::ErrorKind::BrokenPipe)?;

            write_buf.clear();
            write_buf.put_u16(first_pkt.len() as u16);
            write_buf.put_slice(&first_pkt);

            // SYSCALL OPTIMIZATION: Aggressively drain the channel to batch
            // multiple RTP frames into a single TCP write.
            while let Ok(next_pkt) = send_rx.try_recv() {
                write_buf.put_u16(next_pkt.len() as u16);
                write_buf.put_slice(&next_pkt);
                // Keep the buffer within a reasonable MTU/GSO limit (64KB)
                if write_buf.len() > 65535 {
                    break;
                }
            }

            // Perform the actual TCP write
            writer.write_all(&write_buf).await?;

            // Wake up anyone waiting on writable() (SFU loop)
            notifier.notify_waiters();
        }
    }

    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[inline]
    pub fn max_gso_segments(&self) -> usize {
        1
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        // Fast path: if packets are already in the ingress channel, return
        while self.packet_rx.is_empty() {
            self.readable_notifier.notified().await;
        }
        Ok(())
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        loop {
            // If no connections exist yet, wait for one
            if self.conns.is_empty() {
                self.writable_notifier.notified().await;
                continue;
            }

            // Writable if AT LEAST ONE connection is not full.
            // This prevents a single slow TCP client from deadlocking the SFU.
            let mut any_available = false;
            for conn in self.conns.iter() {
                if !conn.is_full() {
                    any_available = true;
                    break;
                }
            }

            if any_available {
                return Ok(());
            }

            // All TCP connections are currently congested; wait for a flush
            self.writable_notifier.notified().await;
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

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        if let Some(peer_tx) = self.conns.get(&batch.dst) {
            // Send into the per-peer bounded buffer
            match peer_tx.try_send(Bytes::copy_from_slice(batch.buf)) {
                Ok(_) => Ok(true),
                Err(async_channel::TrySendError::Full(_)) => {
                    // Signal to the SFU that this specific peer is congested.
                    // The SFU should usually drop the packet for this peer.
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "TCP Peer gone",
                )),
            }
        } else {
            // Ignore packets for addresses not connected via TCP (Passive-only)
            Ok(false)
        }
    }
}
