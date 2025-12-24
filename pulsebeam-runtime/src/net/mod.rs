mod sim;
mod tcp;
mod udp;

use std::{io, net::SocketAddr};

use bytes::Bytes;

pub const BATCH_SIZE: usize = quinn_udp::BATCH_SIZE;
// Fit allocator page size and Linux GRO limit
pub const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,
    Tcp,
    // Tls,
}

/// A received packet (zero-copy buffer + source address).
#[derive(Debug, Clone)]
pub struct RecvPacket {
    pub src: SocketAddr, // remote peer
    pub dst: SocketAddr,
    pub buf: Bytes, // pre-allocated buffer
}

#[derive(Debug, Clone)]
pub struct RecvPacketBatch {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub buf: Bytes,
    pub stride: usize,
    pub len: usize,
    pub transport: Transport,
}

impl RecvPacketBatch {
    pub fn iter(&self) -> RecvPacketBatchIter<'_> {
        RecvPacketBatchIter {
            batch: self,
            offset: 0,
        }
    }
}

impl<'a> IntoIterator for &'a RecvPacketBatch {
    type Item = Bytes;
    type IntoIter = RecvPacketBatchIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub struct RecvPacketBatchIter<'a> {
    batch: &'a RecvPacketBatch,
    offset: usize,
}

impl<'a> Iterator for RecvPacketBatchIter<'a> {
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.batch.len {
            return None;
        }

        let remaining = self.batch.len - self.offset;
        let seg_len = std::cmp::min(self.batch.stride, remaining);

        if seg_len == 0 {
            return None;
        }

        let packet_buf = self.batch.buf.slice(self.offset..self.offset + seg_len);

        self.offset += seg_len;

        Some(packet_buf)
    }
}

/// A packet to send.
#[derive(Debug, Clone)]
pub struct SendPacket {
    pub dst: SocketAddr, // destination
    pub buf: Bytes,      // zero-copy payload
}

#[derive(Debug, Clone)]
pub struct SendPacketBatch<'a> {
    pub dst: SocketAddr,
    pub buf: &'a [u8],
    pub segment_size: usize,
}

/// Binds a socket to the given address and transport type.
pub async fn bind(
    addr: SocketAddr,
    transport: Transport,
    external_addr: Option<SocketAddr>,
) -> io::Result<(UnifiedSocketReader, UnifiedSocketWriter)> {
    let socks = match transport {
        Transport::Udp => {
            let (reader, writer) = udp::bind(addr, external_addr)?;
            (
                UnifiedSocketReader::Udp(Box::new(reader)),
                UnifiedSocketWriter::Udp(writer),
            )
        }

        Transport::Tcp => {
            let (reader, writer) = tcp::bind(addr, external_addr).await?;
            (
                UnifiedSocketReader::Tcp(reader),
                UnifiedSocketWriter::Tcp(writer),
            )
        }
        _ => todo!(),
    };
    tracing::debug!("bound to {addr} ({transport:?})");
    Ok(socks)
}

pub enum UnifiedSocketReader {
    Udp(Box<udp::UdpTransportReader>),
    Tcp(tcp::TcpTransportReader),
}

impl UnifiedSocketReader {
    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        if let Self::Tcp(inner) = self {
            inner.close_peer(peer_addr);
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
        }
    }

    /// Waits until the socket is readable.
    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::Tcp(inner) => inner.readable().await,
        }
    }

    /// Receives a batch of packets into pre-allocated buffers.
    #[inline]
    pub fn try_recv_batch(&mut self, packets: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets),
            Self::Tcp(inner) => inner.try_recv_batch(packets),
        }
    }
}

#[derive(Clone)]
pub enum UnifiedSocketWriter {
    Udp(udp::UdpTransportWriter),
    Tcp(tcp::TcpTransportWriter),
}

impl UnifiedSocketWriter {
    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::Tcp(inner) => inner.max_gso_segments(),
        }
    }

    /// Waits until the socket is writable.
    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::Tcp(inner) => inner.writable().await,
        }
    }

    /// Sends a batch of packets.
    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::Tcp(inner) => inner.try_send_batch(batch),
        }
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Udp(_) => Transport::Udp,
            Self::Tcp(_) => Transport::Tcp,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
        }
    }
}

fn fmt_bytes(b: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    if b >= MB {
        format!("{}MB", b / MB)
    } else if b >= KB {
        format!("{}KB", b / KB)
    } else {
        format!("{}B", b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpStream, UdpSocket},
    };

    async fn test_transport(transport_type: Transport) {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // 1. Bind now returns a split Reader and Writer
        let (mut reader, writer) = bind(bind_addr, transport_type, None).await.unwrap();

        // We can get the address from either, but writer is usually the "identity"
        let actual_server_addr = writer.local_addr();

        // --- 2. External Client Setup ---
        let mut tcp_client: Option<TcpStream> = None;
        let mut udp_client: Option<UdpSocket> = None;

        if matches!(transport_type, Transport::Tcp) {
            tcp_client = Some(TcpStream::connect(actual_server_addr).await.unwrap());
            tcp_client.as_ref().unwrap().set_nodelay(true).unwrap();
        } else {
            udp_client = Some(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        }

        // --- 3. Handshake: Client -> Server ---
        let handshake_payload = b"hello-sfu";
        if let Some(ref mut tcp) = tcp_client {
            let mut buf = Vec::new();
            buf.put_u16(handshake_payload.len() as u16);
            buf.put_slice(handshake_payload);
            tcp.write_all(&buf).await.unwrap();
        } else {
            udp_client
                .as_ref()
                .unwrap()
                .send_to(handshake_payload, actual_server_addr)
                .await
                .unwrap();
        }

        // Server: Wait for handshake using the Reader
        reader.readable().await.unwrap();

        // Note: RecvPacketBatcher is now internal to the reader and not seen here
        let mut out = Vec::new();

        // Retry loop for UDP loopback jitter
        let remote_peer_addr = loop {
            out.clear();
            // try_recv_batch now takes mut self and handles its own batcher
            if reader.try_recv_batch(&mut out).is_ok() && !out.is_empty() {
                break out[0].src;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // --- 4. Data Transfer: Server -> Client ---
        let num_packets = 100;
        let packet_payload = b"important-media-data";

        // Spawn a client-side receiver task
        let rx_handle = tokio::spawn(async move {
            let mut count = 0;
            if let Some(mut tcp) = tcp_client {
                let mut buf = vec![0u8; 1024];
                while count < num_packets {
                    let len = tcp.read_u16().await.unwrap() as usize;
                    tcp.read_exact(&mut buf[..len]).await.unwrap();
                    assert_eq!(&buf[..len], packet_payload);
                    count += 1;
                }
            } else {
                let mut buf = [0u8; 1024];
                while count < num_packets {
                    let (len, _) = udp_client
                        .as_ref()
                        .unwrap()
                        .recv_from(&mut buf)
                        .await
                        .unwrap();
                    assert_eq!(&buf[..len], packet_payload);
                    count += 1;
                }
            }
            count
        });

        // Server: Send packets using the Writer
        // We can even clone the writer to show multi-owner capability
        let writer_tx = writer.clone();
        let mut sent = 0;
        while sent < num_packets {
            writer_tx.writable().await.unwrap();
            let batch = SendPacketBatch {
                dst: remote_peer_addr,
                buf: packet_payload,
                segment_size: packet_payload.len(),
            };

            match writer_tx.try_send_batch(&batch) {
                Ok(true) => sent += 1,
                Ok(false) => {
                    // Handle backpressure/WouldBlock
                    tokio::task::yield_now().await;
                }
                Err(e) => {
                    panic!("Send failed: {e}");
                }
            }
        }

        // Verify all packets arrived
        let received = tokio::time::timeout(Duration::from_secs(2), rx_handle)
            .await
            .expect("Test timed out")
            .expect("Client task failed");

        assert_eq!(
            received, num_packets,
            "Transport {:?} lost packets",
            transport_type
        );
    }

    #[tokio::test]
    async fn test_udp() {
        test_transport(Transport::Udp).await;
    }

    #[tokio::test]
    async fn test_tcp() {
        test_transport(Transport::Tcp).await;
    }
}
