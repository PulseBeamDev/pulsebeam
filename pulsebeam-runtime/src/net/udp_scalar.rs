use crate::sync::Arc;
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
};

use pulsebeam_core::net::UdpSocket;

use crate::net::{CHUNK_SIZE, RecvPacketBatch, SendPacketBatch, Transport, UdpMode};

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
        // UDP scalar has no per-peer connection finish.
    }
}

pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<UdpTransport> {
    let socket = UdpSocket::bind(addr).await?;
    let socket = Arc::new(socket);
    let local_addr = external_addr.unwrap_or(socket.local_addr()?);

    let reader = UdpTransportReader {
        sock: socket.clone(),
        local_addr,
    };
    let writer = UdpTransportWriter {
        sock: socket.clone(),
        local_addr,
    };
    Ok(UdpTransport { reader, writer })
}

pub struct UdpTransportReader {
    sock: Arc<UdpSocket>,
    local_addr: SocketAddr,
}

impl UdpTransportReader {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn gro_segments(&self) -> usize {
        1
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.readable().await?;
        Ok(())
    }

    #[inline]
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<usize> {
        let mut slot = vec![0u8; CHUNK_SIZE];
        match self.sock.try_recv_from(&mut slot) {
            Ok((n, source)) => {
                slot.truncate(n);
                out.push(RecvPacketBatch {
                    transport: Transport::Udp(UdpMode::Scalar),
                    src: source,
                    dst: self.local_addr,
                    buf: slot,
                    offset: 0,
                    stride: n,
                    len: n,
                });
            }
            Err(err) => {
                return Err(err);
            }
        }

        Ok(1)
    }
}

#[derive(Clone)]
pub struct UdpTransportWriter {
    sock: Arc<UdpSocket>,
    local_addr: SocketAddr,
}

impl UdpTransportWriter {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        1
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.writable().await?;
        Ok(())
    }

    #[inline]
    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> std::io::Result<bool> {
        let res = self.sock.try_send_to(batch.buf, batch.dst);

        match res {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(err) => {
                tracing::warn!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}
