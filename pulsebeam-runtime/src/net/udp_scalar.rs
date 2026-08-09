use crate::{net::SendPacket, sync::Arc};
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
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> std::io::Result<usize> {
        self.writer.try_send_batch(batch)
    }

    pub fn close_peer(&mut self, _peer_addr: &SocketAddr) {
        // UDP scalar has no per-peer connection finish.
    }
}

pub fn from_socket(
    socket: UdpSocket,
    external_addr: Option<SocketAddr>,
) -> io::Result<UdpTransport> {
    from_shared(Arc::new(socket), external_addr)
}

/// Build a transport over a socket that may already have other transports on
/// it. Several readers on one socket is what `SO_REUSEPORT` looks like from
/// above: a datagram goes to whichever is ready for it.
pub fn from_shared(
    socket: Arc<UdpSocket>,
    external_addr: Option<SocketAddr>,
) -> io::Result<UdpTransport> {
    let local_addr = external_addr.unwrap_or(socket.local_addr()?);

    let reader = UdpTransportReader {
        sock: socket.clone(),
        local_addr,
        arena: vec![0; CHUNK_SIZE].into_boxed_slice(),
    };
    #[cfg(feature = "sim")]
    let shaper = crate::net::shaper::Shaper::default();

    // Release queued packets on their own schedule.
    //
    // Draining opportunistically - only when the caller next sends - ties departure times to how
    // often the event loop happens to run, and everything already due then leaves back to back.
    // That destroys the inter-packet spacing the receiver measures, which is the entire signal a
    // probe carries: on a 3 Mbps link a 13712-byte burst was seen arriving in 16ms, which that
    // link cannot physically do (36.6ms of serialisation), yielding a 6.3 Mbps estimate. Other
    // probes stretched the same way and read low. Both directions of error come from here.
    #[cfg(feature = "sim")]
    {
        let shaper = shaper.clone();
        let sock = socket.clone();
        tokio::task::spawn_local(async move {
            loop {
                match shaper.next_release() {
                    Some(at) => tokio::time::sleep_until(at).await,
                    // Nothing queued. Idle briefly rather than spin; a packet offered in the
                    // meantime is picked up on the next pass.
                    None => tokio::time::sleep(std::time::Duration::from_micros(200)).await,
                }
                let mut shaper = shaper.clone();
                for (dst, buf) in shaper.drain_due(tokio::time::Instant::now()) {
                    if !shaper.should_drop_packet(dst.ip()) {
                        let _ = sock.try_send_to(&buf, dst);
                    }
                }
            }
        });
    }

    let writer = UdpTransportWriter {
        sock: socket.clone(),
        local_addr,
        drop_count: 0,
        #[cfg(feature = "sim")]
        shaper,
    };
    Ok(UdpTransport { reader, writer })
}

pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<UdpTransport> {
    from_socket(UdpSocket::bind(addr).await?, external_addr)
}

pub struct UdpTransportReader {
    sock: Arc<UdpSocket>,
    local_addr: SocketAddr,
    arena: Box<[u8]>,
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
        debug_assert_eq!(self.arena.len(), CHUNK_SIZE);
        match self.sock.try_recv_from(&mut self.arena) {
            Ok((n, source)) => {
                debug_assert!(
                    n <= CHUNK_SIZE,
                    "scalar recv length exceeds maximum UDP chunk size"
                );
                debug_assert!(n <= self.arena.len());
                out.push(RecvPacketBatch {
                    transport: Transport::Udp(UdpMode::Scalar),
                    src: source,
                    dst: self.local_addr,
                    buf: self.arena[..n].to_vec(),
                    stride: n,
                    len: n,
                    offset: 0,
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
    drop_count: usize,
    /// Simulated bottleneck on the way out. See [`crate::net::shaper`].
    #[cfg(feature = "sim")]
    shaper: crate::net::shaper::Shaper,
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
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> std::io::Result<usize> {
        for group in batch.packets {
            self.try_send_group(group)?;
        }
        Ok(batch.packets.len())
    }

    pub fn try_send_group(&mut self, batch: &SendPacket) -> std::io::Result<bool> {
        #[cfg(feature = "sim")]
        {
            use crate::net::shaper::Shaped;
            let now = tokio::time::Instant::now();
            // Offer only. Release is the release task's job, so a packet's departure time is set
            // by the link rather than by when this happens to be called next.
            if let Shaped::Absorbed = self.shaper.offer(now, batch.dst, batch.buf) {
                return Ok(true);
            }
            if self.shaper.should_drop_packet(batch.dst.ip()) {
                return Ok(true);
            }
            if self.shaper.should_duplicate_packet(batch.dst.ip()) {
                let _ = self.sock.try_send_to(batch.buf, batch.dst);
            }
        }

        let res = self.sock.try_send_to(batch.buf, batch.dst);

        match res {
            Ok(_) => Ok(true),
            // Lossy: kernel buffer full — drop this batch rather than queue it.
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                // metrics::counter!("udp_egress_packets_dropped_total").increment(1);
                if self.drop_count.is_multiple_of(100) {
                    tracing::warn!("udp_scalar dropped a packet due to full socket");
                }
                self.drop_count += 1;
                Ok(true)
            }
            Err(err) => {
                tracing::warn!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}
