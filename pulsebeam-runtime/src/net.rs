use std::{
    io::{self, ErrorKind, IoSliceMut},
    mem::MaybeUninit,
    net::SocketAddr,
};

use bytes::Bytes;
use quinn_udp::RecvMeta;

pub const BATCH_SIZE: usize = 32;
// Fit Mimalloc page size and Linux GRO limit
pub const CHUNK_SIZE: usize = u16::MAX as usize;

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

pub struct RecvPacketBatcher {
    meta: [RecvMeta; BATCH_SIZE],
    buffers: [Box<[MaybeUninit<u8>]>; BATCH_SIZE],
}

impl RecvPacketBatcher {
    pub fn new() -> Self {
        let buffers: [Box<[MaybeUninit<u8>]>; BATCH_SIZE] =
            std::array::from_fn(|_| Box::new_uninit_slice(CHUNK_SIZE));

        Self {
            meta: [RecvMeta::default(); BATCH_SIZE],
            buffers,
        }
    }

    fn collect(&mut self, local_addr: SocketAddr, count: usize, out: &mut Vec<RecvPacketBatch>) {
        metrics::histogram!("network_recv_batch_counts", "transport" => "udp").record(count as f64);
        for i in 0..count {
            let m = &self.meta[i];
            metrics::histogram!("network_recv_batch_sizes", "transport" => "udp")
                .record(m.len as f64);

            // This is fairly cheap for mimalloc as the allocation size <= page size
            // It should be mostly O(1) here
            let new_buf = Box::new_uninit_slice(CHUNK_SIZE);
            let old_buf = std::mem::replace(&mut self.buffers[i], new_buf);
            let old_buf: Box<[u8]> = unsafe { old_buf.assume_init() };
            let full_bytes = Bytes::from(old_buf);
            let buf = full_bytes.slice(0..m.len);

            out.push(RecvPacketBatch {
                src: m.addr,
                dst: local_addr,
                buf,
                stride: m.stride,
                len: m.len,
            });
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
        self.sock.try_io(tokio::io::Interest::READABLE, || {
            let buffers = &mut batcher.buffers;
            let mut slices: [IoSliceMut; BATCH_SIZE] = std::array::from_fn(|i| {
                let slice: &mut [u8] =
                    unsafe { &mut *(buffers[i].as_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
                IoSliceMut::new(slice)
            });
            let res = self
                .state
                .recv((&self.sock).into(), &mut slices, &mut batcher.meta);
            let _ = slices; // slices is no longer safe to use

            match res {
                Ok(count) => {
                    batcher.collect(self.local_addr, count, out);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })
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
        _batch: &mut RecvPacketBatcher,
        packets: &mut Vec<RecvPacketBatch>,
    ) -> std::io::Result<()> {
        let mut buf = [0u8; Self::MTU];
        let mut total_bytes = 0;
        let mut read_count = 0; // Track count to ensure we signal WouldBlock if empty

        loop {
            match self.sock.try_recv_from(&mut buf) {
                Ok((len, src)) => {
                    read_count += 1;
                    total_bytes += len;
                    packets.push(RecvPacketBatch {
                        buf: Bytes::copy_from_slice(&buf[..len]),
                        src,
                        dst: self.local_addr,
                        len,
                        stride: len,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("Receive packet failed: {}", e);
                    return Err(e);
                }
            }
        }

        if read_count > 0 {
            if total_bytes > 0 {
                metrics::histogram!("network_recv_batch_bytes", "transport" => "sim_udp")
                    .record(total_bytes as f64);
            }
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

        sock.writable().await.unwrap();
        let payload = b"hello";
        let send_batch = SendPacketBatch {
            dst: sock.local_addr(),
            buf: payload,
            segment_size: payload.len(),
        };
        assert!(sock.try_send_batch(&send_batch).unwrap());

        sock.readable().await.unwrap();
        let mut batcher = RecvPacketBatcher::new();
        let mut buf = Vec::with_capacity(1);
        sock.try_recv_batch(&mut batcher, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        let mut batch = buf[0].iter();
        assert_eq!(&batch.next().unwrap(), payload.as_slice());

        buf.clear();
        let payload = b"bla";
        let send_batch = SendPacketBatch {
            dst: sock.local_addr(),
            buf: payload,
            segment_size: payload.len(),
        };
        assert!(sock.try_send_batch(&send_batch).unwrap());
        sock.try_recv_batch(&mut batcher, &mut buf).unwrap();
        assert_eq!(buf.len(), 1);
        let mut batch = buf[0].iter();
        assert_eq!(&batch.next().unwrap(), payload.as_slice());
    }
}
