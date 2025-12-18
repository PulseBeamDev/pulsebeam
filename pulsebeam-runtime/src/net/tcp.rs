use crate::net::Transport;

use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch};
use bytes::{Buf, Bytes, BytesMut};
use dashmap::DashMap;
use std::sync::Arc;
use std::{io, net::SocketAddr};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

pub struct TcpTransport {
    local_addr: SocketAddr,
    // Channel where background TCP tasks send framed packets to the main loop
    packet_rx: async_channel::Receiver<RecvPacketBatch>,
    // Map to find the outbound channel for a specific remote peer
    // Using Arc<DashMap> for O(1) lookups during send without blocking the main loop
    conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
    // A notifier to wake up the `readable()` future
    readable_notifier: Arc<tokio::sync::Notify>,
}

impl TcpTransport {
    pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = external_addr.unwrap_or(listener.local_addr()?);

        let (packet_tx, packet_rx) = async_channel::bounded(BATCH_SIZE * 8);
        let conns = Arc::new(DashMap::new());
        let readable_notifier = Arc::new(tokio::sync::Notify::new());

        // Spawn the Passive Listener Task
        let conns_clone = conns.clone();
        let notifier_clone = readable_notifier.clone();
        tokio::spawn(async move {
            while let Ok((stream, peer_addr)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                Self::handle_new_connection(
                    stream,
                    peer_addr,
                    local_addr,
                    packet_tx.clone(),
                    conns_clone.clone(),
                    notifier_clone.clone(),
                );
            }
        });

        Ok(Self {
            local_addr,
            packet_rx,
            conns,
            readable_notifier,
        })
    }

    fn handle_new_connection(
        stream: TcpStream,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        packet_tx: async_channel::Sender<RecvPacketBatch>,
        conns: Arc<DashMap<SocketAddr, async_channel::Sender<Bytes>>>,
        notifier: Arc<tokio::sync::Notify>,
    ) {
        let (send_tx, send_rx) = async_channel::bounded::<Bytes>(256); // Per-peer egress buffer
        conns.insert(peer_addr, send_tx);

        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(stream);
            let mut recv_buf = BytesMut::with_capacity(CHUNK_SIZE);

            loop {
                tokio::select! {
                    // Outbound: SFU -> TCP Socket
                    Ok(pkt) = send_rx.recv() => {
                        let len_header = (pkt.len() as u16).to_be_bytes();
                        let bufs = [std::io::IoSlice::new(&len_header), std::io::IoSlice::new(&pkt)];
                        if writer.write_vectored(&bufs).await.is_err() { break; }
                    }
                    // Inbound: TCP Socket -> SFU (RFC 4571)
                    res = reader.read_buf(&mut recv_buf) => {
                        match res {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                while recv_buf.len() >= 2 {
                                    let len = u16::from_be_bytes([recv_buf[0], recv_buf[1]]) as usize;
                                    if recv_buf.len() < 2 + len { break; }

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

                                    if packet_tx.send(pkt).await.is_err() { return; }
                                    notifier.notify_one(); // Wake up the main loop
                                }
                            }
                        }
                    }
                }
            }
            conns.remove(&peer_addr);
        });
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    // Ingress Readiness: Wakes up when the channel has packets
    pub async fn readable(&self) -> io::Result<()> {
        self.readable_notifier.notified().await;
        Ok(())
    }

    // Egress Readiness: TCP is always ready to accept into the MPSC channel
    pub async fn writable(&self) -> io::Result<()> {
        Ok(())
    }

    /// TCP does not use application-level GSO. The kernel handles
    /// segmentation (TSO) automatically for the byte stream.
    pub fn max_gso_segments(&self) -> usize {
        1
    }

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

    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        if let Some(peer_tx) = self.conns.get(&batch.dst) {
            // Non-blocking try_send. If the channel is full, we drop the packet
            // to prevent a slow TCP client from blocking the whole SFU.
            match peer_tx.try_send(Bytes::copy_from_slice(batch.buf)) {
                Ok(_) => Ok(true),
                Err(async_channel::TrySendError::Full(_)) => Ok(false), // Backpressure: drop
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "TCP Peer gone",
                )),
            }
        } else {
            // Passive-only: We don't initiate connections.
            // If the peer isn't here, we can't send.
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Peer not connected via TCP",
            ))
        }
    }
}
