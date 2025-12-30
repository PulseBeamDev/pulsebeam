pub mod sim;
pub mod tcp;
pub mod udp;

use bytes::Bytes;
use std::sync::Arc;
use std::{io, net::SocketAddr};

pub const BATCH_SIZE: usize = quinn_udp::BATCH_SIZE;
pub const CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    Udp,
    Tcp,
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

#[derive(Debug, Clone)]
pub struct SendPacketBatch<'a> {
    pub dst: SocketAddr,
    pub buf: &'a [u8],
    pub segment_size: usize,
}

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
    };
    tracing::debug!("bound to {addr} ({transport:?})");
    Ok(socks)
}

/// Helper to bind a pre-constructed simulation pair.
pub fn bind_sim(
    reader: sim::SimSocketReader,
    writer: sim::SimSocketWriter,
) -> (UnifiedSocketReader, UnifiedSocketWriter) {
    (
        UnifiedSocketReader::Sim(Box::new(reader)),
        UnifiedSocketWriter::Sim(writer),
    )
}

/// Creates a paired Reader/Writer wrapping a Turmoil UDP socket.
pub fn create_sim_udp_pair(
    turmoil_socket: Arc<turmoil::net::UdpSocket>,
) -> (UnifiedSocketReader, UnifiedSocketWriter) {
    let reader = sim::SimSocketReader::new_udp(turmoil_socket.clone());
    let writer = sim::SimSocketWriter::new_udp(turmoil_socket);

    bind_sim(reader, writer)
}

/// Creates a paired Reader/Writer wrapping a Turmoil TCP stream.
pub fn create_sim_tcp_pair(
    stream: turmoil::net::TcpStream,
) -> (UnifiedSocketReader, UnifiedSocketWriter) {
    let local_addr = stream.local_addr().unwrap();
    let peer_addr = stream.peer_addr().unwrap();

    // Split the stream so we can own Read/Write halves independently in background tasks
    let (read_half, write_half) = tokio::io::split(stream);

    let reader = sim::SimSocketReader::new_io(read_half, local_addr, peer_addr, Transport::Tcp);
    let writer = sim::SimSocketWriter::new_io(write_half, local_addr, Transport::Tcp);

    bind_sim(reader, writer)
}

pub enum UnifiedSocketReader {
    Udp(Box<udp::UdpTransportReader>),
    Tcp(tcp::TcpTransportReader),
    Sim(Box<sim::SimSocketReader>),
}

impl UnifiedSocketReader {
    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        match self {
            Self::Tcp(inner) => inner.close_peer(peer_addr),
            Self::Sim(inner) => inner.close_peer(peer_addr),
            Self::Udp(_) => {}
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
            Self::Sim(inner) => inner.local_addr(),
        }
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::Tcp(inner) => inner.readable().await,
            Self::Sim(inner) => inner.readable().await,
        }
    }

    #[inline]
    pub fn try_recv_batch(&mut self, packets: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(packets),
            Self::Tcp(inner) => inner.try_recv_batch(packets),
            Self::Sim(inner) => inner.try_recv_batch(packets),
        }
    }
}

#[derive(Clone)]
pub enum UnifiedSocketWriter {
    Udp(udp::UdpTransportWriter),
    Tcp(tcp::TcpTransportWriter),
    Sim(sim::SimSocketWriter),
}

impl UnifiedSocketWriter {
    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::Tcp(inner) => inner.max_gso_segments(),
            Self::Sim(inner) => inner.max_gso_segments(),
        }
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::Tcp(inner) => inner.writable().await,
            Self::Sim(inner) => inner.writable().await,
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::Tcp(inner) => inner.try_send_batch(batch),
            Self::Sim(inner) => inner.try_send_batch(batch),
        }
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Udp(_) => Transport::Udp,
            Self::Tcp(_) => Transport::Tcp,
            Self::Sim(inner) => inner.transport(),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::Tcp(inner) => inner.local_addr(),
            Self::Sim(inner) => inner.local_addr(),
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
