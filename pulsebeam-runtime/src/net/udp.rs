use crate::net::{GroPayload, Transport, UdpMode};

use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch, fmt_bytes};
use quinn_udp::RecvMeta;
use std::{
    io::{self, ErrorKind, IoSliceMut},
    net::SocketAddr,
    sync::Arc,
};

// Up to 8x IO loop latency, a bit of headroom for keyframe bursts.
// With 1ms scheduling delay, this is capped to 8ms latency.
pub const SOCKET_RECV_SIZE: usize = 8 * BATCH_SIZE * CHUNK_SIZE;
// per-client-pacer handles the latency bloat. But, big enough for keyframe bursts to many subscribers
pub const SOCKET_SEND_SIZE: usize = 32 * BATCH_SIZE * CHUNK_SIZE;

pub async fn bind(
    addr: SocketAddr,
    external_addr: Option<SocketAddr>,
) -> io::Result<(UdpTransportReader, UdpTransportWriter)> {
    let socket2_sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket2_sock.set_nonblocking(true)?;
    socket2_sock.set_reuse_address(true)?;

    #[cfg(unix)]
    socket2_sock.set_reuse_port(true)?;

    socket2_sock.set_recv_buffer_size(SOCKET_RECV_SIZE)?;
    socket2_sock.set_send_buffer_size(SOCKET_SEND_SIZE)?;
    socket2_sock.bind(&addr.into())?;

    let send_buf_size = socket2_sock.send_buffer_size()?;
    let recv_buf_size = socket2_sock.recv_buffer_size()?;

    let state = quinn_udp::UdpSocketState::new((&socket2_sock).into())?;
    let state = Arc::new(state);
    let sock = tokio::net::UdpSocket::from_std(socket2_sock.into())?;
    let writer_sock = Arc::new(sock);

    let local_addr = external_addr.unwrap_or(writer_sock.local_addr()?);
    let reader_sock = writer_sock.clone();

    let reader = UdpTransportReader {
        sock: reader_sock,
        state: state.clone(),
        local_addr,
        meta: [RecvMeta::default(); BATCH_SIZE],
        batch_buffer: Vec::with_capacity(BATCH_SIZE * CHUNK_SIZE),
    };

    let writer = UdpTransportWriter {
        sock: writer_sock,
        state,
        local_addr,
    };

    tracing::info!(
        %addr,
        %local_addr,
        recv_buf = fmt_bytes(recv_buf_size),
        send_buf = fmt_bytes(send_buf_size),
        gro_segments = ?reader.gro_segments(),
        gso_segments = ?writer.max_gso_segments(),
        "UDP socket bound"
    );

    Ok((reader, writer))
}

pub struct UdpTransportReader {
    sock: Arc<tokio::net::UdpSocket>,
    state: Arc<quinn_udp::UdpSocketState>,
    local_addr: SocketAddr,

    meta: [RecvMeta; BATCH_SIZE],
    batch_buffer: Vec<u8>,
}

impl UdpTransportReader {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
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
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
        self.sock.try_io(tokio::io::Interest::READABLE, || {
            // Prepare the pointers for the kernel
            // We set len=capacity so we can take mutable slices of the uninitialized memory
            unsafe { self.batch_buffer.set_len(self.batch_buffer.capacity()) };
            let ptr = self.batch_buffer.as_mut_ptr();

            let mut slices: [IoSliceMut; BATCH_SIZE] = std::array::from_fn(|i| {
                let offset = i * CHUNK_SIZE;
                // SAFETY: We know the buffer is BATCH_SIZE * CHUNK_SIZE large
                unsafe {
                    IoSliceMut::new(std::slice::from_raw_parts_mut(ptr.add(offset), CHUNK_SIZE))
                }
            });

            let res = self
                .state
                .recv((&self.sock).into(), &mut slices, &mut self.meta);
            let _ = slices; // slices is no longer safe to use

            match res {
                Ok(count) => {
                    let new_buffer = Vec::with_capacity(BATCH_SIZE * CHUNK_SIZE);
                    let mut filled_buffer = std::mem::replace(&mut self.batch_buffer, new_buffer);

                    for i in 0..count {
                        let m = &self.meta[i];

                        // Safety: Ensure we don't slice past the buffer (e.g., if kernel lied about len)
                        if m.len > filled_buffer.len() {
                            continue;
                        }

                        let next_buf = filled_buffer.split_off(m.len);

                        out.push(RecvPacketBatch {
                            src: m.addr,
                            dst: self.local_addr,
                            payload: GroPayload {
                                buf: filled_buffer,
                                stride: m.stride,
                                len: m.len,
                            },
                            transport: Transport::Udp(UdpMode::Batch),
                        });
                        filled_buffer = next_buf;
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })
    }
}

#[derive(Clone)]
pub struct UdpTransportWriter {
    sock: Arc<tokio::net::UdpSocket>,
    state: Arc<quinn_udp::UdpSocketState>,
    local_addr: SocketAddr,
}

impl UdpTransportWriter {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::WRITABLE).await?;
        Ok(())
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
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(err) => {
                tracing::trace!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}
