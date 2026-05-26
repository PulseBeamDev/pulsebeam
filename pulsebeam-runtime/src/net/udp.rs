use crate::net::{Transport, UdpMode};
use crate::sync::Arc;

use super::{BATCH_SIZE, CHUNK_SIZE, RecvPacketBatch, SendPacketBatch, fmt_bytes};
use quinn_udp::RecvMeta;
use std::{
    io::{self, ErrorKind, IoSliceMut},
    net::SocketAddr,
};

pub const SOCKET_SEND_SIZE: usize = 2 * 1024 * 1024;
pub const SOCKET_RECV_SIZE: usize = 4 * 1024 * 1024;

pub struct UdpTransport {
    reader: UdpTransportReader,
    writer: UdpTransportWriter,
}

impl UdpTransport {
    pub fn local_addr(&self) -> SocketAddr {
        self.reader.local_addr()
    }

    pub fn max_gso_segments(&self) -> usize {
        self.writer.max_gso_segments()
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.reader.readable().await
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.writer.writable().await
    }

    #[inline]
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<usize> {
        self.reader.try_recv_batch(out)
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        self.writer.try_send_batch(batch)
    }

    pub fn close_peer(&mut self, _peer_addr: &SocketAddr) {
        // UDP has no per-peer connection state to close.
    }
}

pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<UdpTransport> {
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
        batch_buffer: vec![0u8; BATCH_SIZE * CHUNK_SIZE],
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

    Ok(UdpTransport { reader, writer })
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
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<usize> {
        self.sock.try_io(tokio::io::Interest::READABLE, || {
            let mut chunks = self.batch_buffer.chunks_exact_mut(CHUNK_SIZE);
            let mut slices: [IoSliceMut; BATCH_SIZE] = std::array::from_fn(|_| {
                let chunk = chunks.next().expect("batch_buffer is sized correctly");
                IoSliceMut::new(chunk)
            });

            let res = self
                .state
                .recv((&*self.sock).into(), &mut slices, &mut self.meta);

            match res {
                Ok(count) => {
                    let prev_len = out.len();
                    for i in 0..count {
                        let m = &self.meta[i];
                        let base = i * CHUNK_SIZE;
                        assert!(m.stride != 0, "gro stride can't be zero");
                        let tail = self.batch_buffer.len().min(base + m.len);
                        let buf = &self.batch_buffer[base..tail];

                        out.push(RecvPacketBatch {
                            src: m.addr,
                            dst: self.local_addr,
                            buf: buf.to_vec(), // Contains the entire GRO block
                            stride: m.stride,  // Downstream will use this to skip through buf
                            len: m.len,        // Total length of the GRO batch
                            transport: Transport::Udp(UdpMode::Batch),
                        });
                    }
                    Ok(out.len() - prev_len)
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
        debug_assert!(
            !batch.buf.is_empty(),
            "SendPacketBatch buffer must not be empty"
        );
        debug_assert!(
            batch.segment_size <= batch.buf.len(),
            "SendPacketBatch segment_size must not exceed total buffer length"
        );
        let transmit = quinn_udp::Transmit {
            destination: batch.dst,
            ecn: None,
            contents: batch.buf,
            segment_size: Some(batch.segment_size),
            src_ip: None,
        };
        let res = self.sock.try_io(tokio::io::Interest::WRITABLE, || {
            self.state.try_send((&*self.sock).into(), &transmit)
        });

        match res {
            Ok(_) => Ok(true),
            // Lossy: kernel buffer full — drop this batch rather than queue it.
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                let dropped = batch.buf.len().div_ceil(batch.segment_size);
                metrics::counter!("udp_egress_packets_dropped_total").increment(dropped as u64);
                Ok(true)
            }
            Err(err) => {
                tracing::trace!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::SocketAddr, time::Duration};
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn udp_transport_reader_receives_packet() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut transport = bind(addr, None).await.unwrap();
        let local_addr = transport.local_addr();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(b"test-udp-payload", &local_addr)
            .await
            .unwrap();

        transport.readable().await.unwrap();
        let mut out = Vec::new();
        let count = transport.try_recv_batch(&mut out).unwrap();

        assert_eq!(count, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data(), b"test-udp-payload");
        assert_eq!(out[0].src, sender.local_addr().unwrap());
        assert_eq!(out[0].dst, local_addr);
    }

    #[tokio::test]
    async fn udp_transport_writer_sends_payload_without_corruption() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let transport = bind(addr, None).await.unwrap();
        let send_addr = transport.local_addr();

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let payload = (0..1500).map(|i| (i % 256) as u8).collect::<Vec<_>>();
        let batch = SendPacketBatch {
            dst: recv_addr,
            buf: &payload,
            segment_size: 500,
        };

        transport.writable().await.unwrap();
        let sent = transport.try_send_batch(&batch).unwrap();
        assert!(sent, "UDP transport should accept the batch for egress");

        let mut buf = vec![0u8; 2048];
        let mut received = Vec::with_capacity(payload.len());

        while received.len() < payload.len() {
            let (n, peer) =
                tokio::time::timeout(Duration::from_millis(250), receiver.recv_from(&mut buf))
                    .await
                    .expect("timed out waiting for UDP packet")
                    .expect("failed to receive UDP packet");
            assert_eq!(peer, send_addr);
            received.extend_from_slice(&buf[..n]);
        }

        assert_eq!(received, payload);
    }
}
