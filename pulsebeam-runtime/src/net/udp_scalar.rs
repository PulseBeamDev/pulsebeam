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

/// Build a transport over a socket shared with the rest of a `SO_REUSEPORT`
/// group, reading this member's share of the arrivals.
///
/// The group decides which member a datagram belongs to, by hashing its
/// 4-tuple; see `bound_udp_sim`. Sending is unaffected — every member writes to
/// the one socket, which is what the kernel does too.
#[cfg(feature = "sim")]
pub(crate) fn from_reuseport_member(
    socket: Arc<UdpSocket>,
    external_addr: Option<SocketAddr>,
    member: crate::net::bound_udp::ReuseportMember,
) -> io::Result<UdpTransport> {
    build(socket, external_addr, Some(member))
}

/// Build a transport over a socket that may already have other transports on
/// it. Several readers on one socket is what `SO_REUSEPORT` looks like from
/// above: a datagram goes to whichever is ready for it.
pub fn from_shared(
    socket: Arc<UdpSocket>,
    external_addr: Option<SocketAddr>,
) -> io::Result<UdpTransport> {
    build(
        socket,
        external_addr,
        #[cfg(feature = "sim")]
        None,
    )
}

fn build(
    socket: Arc<UdpSocket>,
    external_addr: Option<SocketAddr>,
    #[cfg(feature = "sim")] member: Option<crate::net::bound_udp::ReuseportMember>,
) -> io::Result<UdpTransport> {
    let local_addr = external_addr.unwrap_or(socket.local_addr()?);

    let reader = UdpTransportReader {
        sock: socket.clone(),
        local_addr,
        arena: vec![0; CHUNK_SIZE].into_boxed_slice(),
        #[cfg(feature = "sim")]
        member,
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
        sock: socket,
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
    /// Set when this socket is one of several bound to the same address. Its
    /// arrivals then come from the group rather than straight off the socket,
    /// because which member a datagram belongs to is the group's decision.
    #[cfg(feature = "sim")]
    member: Option<crate::net::bound_udp::ReuseportMember>,
}

impl UdpTransportReader {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn gro_segments(&self) -> usize {
        #[cfg(feature = "sim")]
        if crate::net::shaper::gro_enabled(self.local_addr.ip()) {
            return crate::net::UDP_MAX_GSO_SEGMENTS;
        }
        1
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        #[cfg(feature = "sim")]
        if let Some(member) = &self.member {
            return member.readable().await;
        }
        self.sock.readable().await?;
        Ok(())
    }

    #[inline]
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> std::io::Result<usize> {
        debug_assert_eq!(self.arena.len(), CHUNK_SIZE);
        let start = out.len();
        let gro_enabled = self.gro_segments() > 1;
        #[cfg(feature = "sim")]
        if let Some(member) = &self.member {
            for index in 0..crate::net::BATCH_SIZE {
                match member.try_recv() {
                    Ok((buf, src)) => {
                        debug_assert!(
                            buf.len() <= CHUNK_SIZE,
                            "reuseport datagram exceeds the chunk size"
                        );
                        Self::append_datagram(out, self.local_addr, src, buf, gro_enabled);
                    }
                    Err(err) if index > 0 && err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) => return Err(err),
                }
            }
            self.record_gro(out, start);
            return Ok(out.len().saturating_sub(start));
        }

        for index in 0..crate::net::BATCH_SIZE {
            match self.sock.try_recv_from(&mut self.arena) {
                Ok((n, source)) => {
                    debug_assert!(
                        n <= CHUNK_SIZE,
                        "scalar recv length exceeds maximum UDP chunk size"
                    );
                    debug_assert!(n <= self.arena.len());
                    let buf = self.arena.get(..n).unwrap_or_default().to_vec();
                    Self::append_datagram(out, self.local_addr, source, buf, gro_enabled);
                }
                Err(err) if index > 0 && err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        self.record_gro(out, start);
        Ok(out.len().saturating_sub(start))
    }

    fn append_datagram(
        out: &mut Vec<RecvPacketBatch>,
        dst: SocketAddr,
        src: SocketAddr,
        buf: Vec<u8>,
        gro_enabled: bool,
    ) {
        let len = buf.len();
        debug_assert_ne!(len, 0);
        if gro_enabled {
            if let Some(previous) = out.last_mut() {
                let datagrams = previous.len.div_ceil(previous.stride);
                if previous.src == src
                    && previous.stride == len
                    && datagrams < crate::net::UDP_MAX_GSO_SEGMENTS
                {
                    previous.buf.extend_from_slice(&buf);
                    previous.len = previous.len.saturating_add(len);
                    debug_assert_eq!(previous.buf.len(), previous.len);
                    return;
                }
            }
        }
        out.push(RecvPacketBatch {
            transport: Transport::Udp(UdpMode::Scalar),
            src,
            dst,
            buf,
            stride: len,
            len,
            received_at: Some(tokio::time::Instant::now()),
            offset: 0,
        });
    }

    fn record_gro(&self, out: &[RecvPacketBatch], start: usize) {
        #[cfg(not(feature = "sim"))]
        let _ = (out, start);
        #[cfg(feature = "sim")]
        for batch in out.get(start..).unwrap_or_default() {
            debug_assert_ne!(batch.stride, 0);
            let datagrams = batch.len.div_ceil(batch.stride);
            if datagrams > 1 {
                crate::net::shaper::record_gro_batch(self.local_addr.ip(), datagrams);
            }
        }
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
        if cfg!(feature = "sim") {
            crate::net::UDP_MAX_GSO_SEGMENTS
        } else {
            1
        }
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
            debug_assert!(!batch.buf.is_empty());
            debug_assert_ne!(batch.segment_size, 0);
            debug_assert!(batch.segment_size <= batch.buf.len());
            let segment_count = batch.buf.len().div_ceil(batch.segment_size);
            debug_assert!(segment_count <= crate::net::UDP_MAX_GSO_SEGMENTS);
            if batch.buf.is_empty()
                || batch.segment_size == 0
                || batch.segment_size > batch.buf.len()
                || segment_count > crate::net::UDP_MAX_GSO_SEGMENTS
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "invalid simulated UDP packet segment size",
                ));
            }
            if segment_count > 1 {
                crate::net::shaper::record_gso_batch(batch.dst.ip(), segment_count);
            }
            let now = tokio::time::Instant::now();
            for segment in batch.buf.chunks(batch.segment_size) {
                self.try_send_datagram(now, batch.dst, segment)?;
            }
            return Ok(true);
        }

        #[cfg(not(feature = "sim"))]
        {
            let res = self.sock.try_send_to(batch.buf, batch.dst);

            match res {
                Ok(_) => Ok(true),
                // Lossy: kernel buffer full — drop this batch rather than queue it.
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    // metrics::counter!("udp_egress_packets_dropped_total").increment(1);
                    if self.drop_count.is_multiple_of(100) {
                        tracing::warn!("udp_scalar dropped a packet due to full socket");
                    }
                    self.drop_count = self.drop_count.saturating_add(1);
                    Ok(true)
                }
                Err(err) => {
                    tracing::warn!("try_send_batch failed with {err}");
                    Err(err)
                }
            }
        }
    }

    #[cfg(feature = "sim")]
    fn try_send_datagram(
        &mut self,
        now: tokio::time::Instant,
        dst: SocketAddr,
        buf: &[u8],
    ) -> io::Result<()> {
        use crate::net::shaper::Shaped;

        debug_assert!(!buf.is_empty());
        if let Shaped::Absorbed = self.shaper.offer(now, dst, buf) {
            return Ok(());
        }
        if self.shaper.should_drop_packet(dst.ip()) {
            return Ok(());
        }
        if self.shaper.should_duplicate_packet(dst.ip()) {
            let _ = self.sock.try_send_to(buf, dst);
        }
        match self.sock.try_send_to(buf, dst) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if self.drop_count.is_multiple_of(100) {
                    tracing::warn!("udp_scalar dropped a packet due to full socket");
                }
                self.drop_count = self.drop_count.saturating_add(1);
                Ok(())
            }
            Err(err) => {
                tracing::warn!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }
}
