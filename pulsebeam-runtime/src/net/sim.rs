use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
};

use crate::net::Transport;

use super::{RecvPacketBatch, SendPacketBatch};
use bytes::Bytes;

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
    pub fn try_recv_batch(&self, packets: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
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
                        transport: Transport::Udp,
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
