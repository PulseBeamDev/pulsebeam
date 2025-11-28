use std::{
    io::{self, ErrorKind, IoSliceMut},
    net::SocketAddr,
};

use bytes::{Bytes, BytesMut};
use quinn_udp::RecvMeta;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Udp,
    Tcp,
    Tls,
    SimUdp,
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

pub struct RecvBatchStorage {
    buffer: BytesMut,
    chunk_size: usize,
}

impl RecvBatchStorage {
    const MAX_MTU: usize = 1500;

    pub fn new(gro_segments: usize) -> Self {
        let chunk_size = Self::MAX_MTU * gro_segments;
        let capacity = quinn_udp::BATCH_SIZE * chunk_size;
        Self {
            buffer: BytesMut::with_capacity(capacity),
            chunk_size,
        }
    }

    /// Prepares the storage for receiving packets.
    /// Returns a vector of mutable slices pointing into the internal buffer.
    pub fn prepare(&mut self) -> Vec<IoSliceMut<'_>> {
        let capacity = quinn_udp::BATCH_SIZE * self.chunk_size;

        // Ensure we have enough capacity
        if self.buffer.capacity() < capacity {
            self.buffer = BytesMut::with_capacity(capacity);
        }

        // SAFETY: We are setting the length to the full capacity to allow the kernel to write to it.
        // We must ensure that we do not read from uninitialized parts of this buffer.
        // The `finalize` method uses `RecvMeta` (populated by `recv_mmsg`) to slice only the written parts.
        unsafe {
            self.buffer.set_len(capacity);
        }

        self.buffer
            .chunks_mut(self.chunk_size)
            .map(IoSliceMut::new)
            .collect()
    }

    /// Finalizes the batch, extracting received packets as zero-copy Bytes.
    /// This consumes the current buffer and allocates a new one for the next round.
    pub fn finalize(
        &mut self,
        count: usize,
        meta: &[RecvMeta],
        local_addr: SocketAddr,
        out: &mut Vec<RecvPacketBatch>,
    ) {
        if count == 0 {
            return;
        }

        // Record metrics
        let total_bytes: usize = meta.iter().take(count).map(|m| m.len).sum();
        if total_bytes > 0 {
            metrics::histogram!("network_recv_batch_bytes", "transport" => "udp")
                .record(total_bytes as f64);
        }

        // Take the buffer and freeze it to make it immutable and shareable (zero-copy)
        let capacity = quinn_udp::BATCH_SIZE * self.chunk_size;
        let chunk = std::mem::replace(&mut self.buffer, BytesMut::with_capacity(capacity));
        let bytes = chunk.freeze();

        for (i, m) in meta.iter().take(count).enumerate() {
            metrics::histogram!("network_recv_batch_io_slices", "transport" => "udp")
                .record((m.len / m.stride) as f64);

            let offset = i * self.chunk_size;
            // Ensure we don't read out of bounds (should not happen if prepare/recv logic is correct)
            let end = offset + m.len;

            let packet_buf = bytes.slice(offset..end);

            let batch = RecvPacketBatch {
                src: m.addr,
                dst: local_addr,
                buf: packet_buf,
                stride: m.stride,
                len: m.len,
            };
            out.push(batch);
        }
    }

    // Helper for SimUdp which doesn't use RecvMeta
    pub fn finalize_sim(
        &mut self,
        count: usize,
        lens: &[(usize, SocketAddr)], // (len, src)
        local_addr: SocketAddr,
        out: &mut Vec<RecvPacketBatch>,
    ) {
        if count == 0 {
            return;
        }

        let total_bytes: usize = lens.iter().map(|(l, _)| *l).sum();
        if total_bytes > 0 {
            metrics::histogram!("network_recv_batch_bytes", "transport" => "sim_udp")
                .record(total_bytes as f64);
        }

        let capacity = quinn_udp::BATCH_SIZE * self.chunk_size;
        let chunk = std::mem::replace(&mut self.buffer, BytesMut::with_capacity(capacity));
        let bytes = chunk.freeze();

        for (i, (len, src)) in lens.iter().enumerate() {
            let offset = i * self.chunk_size;
            let end = offset + len;
            let packet_buf = bytes.slice(offset..end);

            out.push(RecvPacketBatch {
                src: *src,
                dst: local_addr,
                buf: packet_buf,
                stride: *len,
                len: *len,
            });
        }
    }
}

/// Holds metadata for recv operations.
pub struct RecvPacketBatcher<'a> {
    storage: &'a mut RecvBatchStorage,
    meta: Vec<RecvMeta>,
}

impl<'a> RecvPacketBatcher<'a> {
    pub fn new(storage: &'a mut RecvBatchStorage) -> Self {
        Self {
            storage,
            meta: vec![RecvMeta::default(); quinn_udp::BATCH_SIZE],
        }
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

/// UnifiedSocket enum for different transport types
pub enum UnifiedSocket {
    Udp(UdpTransport),
    SimUdp(SimUdpTransport),
}

impl UnifiedSocket {
    /// Binds a socket to the given address and transport type.
    pub async fn bind(
        addr: SocketAddr,
        transport: Transport,
        external_addr: Option<SocketAddr>,
    ) -> io::Result<Self> {
        let sock = match transport {
            Transport::Udp => Self::Udp(UdpTransport::bind(addr, external_addr)?),
            Transport::SimUdp => Self::SimUdp(SimUdpTransport::bind(addr, external_addr).await?),
            _ => todo!(),
        };
        tracing::debug!("bound to {addr} ({transport:?})");
        Ok(sock)
    }

    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Udp(inner) => inner.local_addr(),
            Self::SimUdp(inner) => inner.local_addr(),
        }
    }

    pub fn max_gso_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.max_gso_segments(),
            Self::SimUdp(inner) => inner.max_gso_segments(),
        }
    }

    pub fn gro_segments(&self) -> usize {
        match self {
            Self::Udp(inner) => inner.gro_segments(),
            Self::SimUdp(inner) => inner.gro_segments(),
        }
    }

    /// Waits until the socket is readable.
    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.readable().await,
            Self::SimUdp(inner) => inner.readable().await,
        }
    }

    /// Waits until the socket is writable.
    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Udp(inner) => inner.writable().await,
            Self::SimUdp(inner) => inner.writable().await,
        }
    }

    /// Receives a batch of packets into pre-allocated buffers.
    #[inline]
    pub fn try_recv_batch(
        &self,
        batch: &mut RecvPacketBatcher,
        packets: &mut Vec<RecvPacketBatch>,
    ) -> std::io::Result<()> {
        match self {
            Self::Udp(inner) => inner.try_recv_batch(batch, packets),
            Self::SimUdp(inner) => inner.try_recv_batch(batch, packets),
        }
    }

    /// Sends a batch of packets.
    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        match self {
            Self::Udp(inner) => inner.try_send_batch(batch),
            Self::SimUdp(inner) => inner.try_send_batch(batch),
        }
    }
}

pub struct UdpTransport {
    sock: tokio::net::UdpSocket,
    state: quinn_udp::UdpSocketState,
    local_addr: SocketAddr,
}

impl UdpTransport {
    pub const MAX_MTU: usize = 1500;
    pub const BUF_SIZE: usize = 1024 * 1024 * 1024;

    pub fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let socket2_sock = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket2_sock.set_nonblocking(true)?;
        socket2_sock.set_reuse_address(true)?;

        #[cfg(unix)]
        socket2_sock.set_reuse_port(true)?;

        socket2_sock.set_recv_buffer_size(Self::BUF_SIZE)?;
        socket2_sock.set_send_buffer_size(Self::BUF_SIZE)?;
        socket2_sock.bind(&addr.into())?;

        let state = quinn_udp::UdpSocketState::new((&socket2_sock).into())?;
        let sock = tokio::net::UdpSocket::from_std(socket2_sock.into())?;

        let local_addr = external_addr.unwrap_or(sock.local_addr()?);

        Ok(Self {
            sock,
            local_addr,
            state,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    pub fn gro_segments(&self) -> usize {
        self.state.gro_segments()
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::READABLE).await?;
        Ok(())
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::WRITABLE).await?;
        Ok(())
    }

    #[inline]
    pub fn try_recv_batch(
        &self,
        batcher: &mut RecvPacketBatcher,
        out: &mut Vec<RecvPacketBatch>,
    ) -> std::io::Result<()> {
        let mut slices = batcher.storage.prepare();

        match self.sock.try_io(tokio::io::Interest::READABLE, || {
            self.state
                .recv((&self.sock).into(), &mut slices, &mut batcher.meta)
        }) {
            Ok(count) => {
                batcher
                    .storage
                    .finalize(count, &batcher.meta, self.local_addr, out);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        debug_assert!(batch.segment_size != 0);
        let transmit = quinn_udp::Transmit {
            destination: batch.dst,
            ecn: None,
            contents: batch.buf,
            segment_size: Some(batch.segment_size),
            src_ip: None,
        };
        let res = self.sock.try_io(tokio::io::Interest::WRITABLE, || {
            self.state.try_send((&self.sock).into(), &transmit)
        });

        match res {
            Ok(_) => {
                metrics::histogram!("network_send_batch_bytes", "transport" => "udp")
                    .record(batch.buf.len() as f64);
                Ok(true)
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(err) => {
                tracing::warn!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}

pub struct SimUdpTransport {
    sock: turmoil::net::UdpSocket,
    local_addr: SocketAddr,
}

impl SimUdpTransport {
    pub const MTU: usize = 1500;

    pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<Self> {
        let sock = turmoil::net::UdpSocket::bind(addr).await?;
        let local_addr = external_addr.unwrap_or(sock.local_addr()?);

        Ok(SimUdpTransport { sock, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        1
    }

    pub fn gro_segments(&self) -> usize {
        1
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.readable().await
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.writable().await
    }

    #[inline]
    pub fn try_recv_batch(
        &self,
        batcher: &mut RecvPacketBatcher,
        packets: &mut Vec<RecvPacketBatch>,
    ) -> std::io::Result<()> {
        let mut slices = batcher.storage.prepare();
        let mut lens = Vec::with_capacity(slices.len());
        let mut read_count = 0; // Track count to ensure we signal WouldBlock if empty

        for slice in slices.iter_mut() {
            match self.sock.try_recv_from(slice) {
                Ok((len, src)) => {
                    read_count += 1;
                    lens.push((len, src));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Receive packet failed: {}", e);
                    return Err(e);
                }
            }
        }

        if read_count > 0 {
            batcher
                .storage
                .finalize_sim(read_count, &lens, self.local_addr, packets);
            Ok(())
        } else {
            Err(io::Error::from(ErrorKind::WouldBlock))
        }
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        assert!(batch.segment_size != 0);
        assert!(batch.buf.len() == batch.segment_size);

        match self.sock.try_send_to(batch.buf, batch.dst) {
            Ok(_) => {
                metrics::histogram!("network_send_batch_bytes", "transport" => "sim_udp")
                    .record(batch.buf.len() as f64);
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(e) => {
                tracing::warn!("Receive packet failed: {}", e);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_loopback() {
        // NOTE: Metrics are not checked in this test, but their code paths are exercised.
        // A proper test would involve setting up a metrics recorder.
        let sock = UnifiedSocket::bind("127.0.0.1:3000".parse().unwrap(), Transport::Udp, None)
            .await
            .unwrap();

        let payload = b"hello";
        sock.writable().await.unwrap();
        let packet = SendPacket {
            buf: Bytes::from_static(payload),
            dst: sock.local_addr(),
        };
        let batch = SendPacketBatch {
            dst: packet.dst,
            buf: &packet.buf,
            segment_size: packet.buf.len(),
        };
        assert!(sock.try_send_batch(&batch).unwrap());

        sock.readable().await.unwrap();
        let mut storage = RecvBatchStorage::new(1);
        let mut batcher = RecvPacketBatcher::new(&mut storage);
        let mut buf = Vec::with_capacity(1);
        sock.try_recv_batch(&mut batcher, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(&buf[0].buf, payload.as_slice());
    }
}
