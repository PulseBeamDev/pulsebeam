use crate::sync::Arc;
use crate::sync::pool_buf::net_recv_pool;
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
};

use pulsebeam_core::net::UdpSocket;

use crate::net::{RecvPacketBatch, SendPacketBatch, Transport, UdpMode};

pub async fn bind(
    addr: SocketAddr,
    external_addr: Option<SocketAddr>,
) -> io::Result<(UdpTransportReader, UdpTransportWriter)> {
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
    Ok((reader, writer))
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
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<()> {
        // Checkout an uninitialised pool slot and hand its storage directly
        // to the kernel via recv_from.  Zero intermediate Vec, zero extra copy.
        let mut slot = net_recv_pool().checkout_uninit();
        match self.sock.try_recv_from(slot.as_uninit_slice()) {
            Ok((n, source)) => {
                out.push(RecvPacketBatch {
                    transport: Transport::Udp(UdpMode::Scalar),
                    src: source,
                    dst: self.local_addr,
                    buf: slot.freeze(n),
                    offset: 0,
                    stride: n,
                    len: n,
                });
            }
            Err(err) => {
                // slot drops here — returns silently to pool.
                return Err(err);
            }
        }

        Ok(())
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
