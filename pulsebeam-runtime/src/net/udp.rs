//! Batched UDP transport built directly on Linux's `recvmmsg(2)`/`sendmmsg(2)`
//! plus UDP GRO (receive-side coalescing) and UDP GSO (send-side segmentation
//! offload) — no `quinn-udp` dependency.
//!
//! All actual `unsafe` needed to make these syscalls lives inside `nix`
//! (which wraps libc/the kernel ABI in a reviewed, tested safe API). This
//! module contains none itself; that's enforced below.
//!
//! Linux only: `recvmmsg`/`sendmmsg`, `UDP_GRO`, and `UDP_SEGMENT` are all
//! Linux-specific, so this file simply doesn't compile anywhere else.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use crate::net::{Transport, UdpMode};
use crate::sync::Arc;

use super::{
    BATCH_SIZE, MAX_UDP_GSO_PAYLOAD_SIZE, MAX_UDP_PAYLOAD_SIZE, RecvPacketBatch, SendPacket,
    SendPacketBatch, fmt_bytes,
};

use nix::{
    cmsg_space,
    errno::Errno,
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, MultiHeaders, SockaddrStorage, recvmmsg,
        sendmmsg, setsockopt, sockopt,
    },
};
use std::{
    io::{self, ErrorKind, IoSlice, IoSliceMut},
    net::{IpAddr, SocketAddr},
    os::fd::AsRawFd,
};
use zerocopy::FromZeros;

const MEBIBYTE: usize = 1024 * 1024;
const GSO_PROBE_SEGMENT_SIZE: i32 = 1200;
const DISABLE_GSO_SEGMENT_SIZE: i32 = 0;
const INVALID_GRO_SEGMENT_SIZE: i32 = 0;
const SINGLE_SEGMENT: usize = 1;
const DROP_LOG_INTERVAL: usize = 100;

pub const SOCKET_SEND_SIZE: usize = 2 * MEBIBYTE;
pub const SOCKET_RECV_SIZE: usize = 4 * MEBIBYTE;

const UDP_MAX_GSO_SEGMENTS: usize = 64;
const GRO_SLOT_SIZE: usize = UDP_MAX_GSO_SEGMENTS * MAX_UDP_PAYLOAD_SIZE;

fn normalize_v4_mapped(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| SocketAddr::new(IpAddr::V4(v4), addr.port()))
            .unwrap_or(addr),
        IpAddr::V4(_) => addr,
    }
}

fn sockaddr_to_std(addr: &SockaddrStorage) -> io::Result<SocketAddr> {
    if let Some(v4) = addr.as_sockaddr_in() {
        Ok(SocketAddr::new(IpAddr::V4(v4.ip()), v4.port()))
    } else if let Some(v6) = addr.as_sockaddr_in6() {
        Ok(SocketAddr::new(IpAddr::V6(v6.ip()), v6.port()))
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidData,
            "recvmmsg returned an address family we don't support (expected IPv4/IPv6)",
        ))
    }
}

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
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        self.reader.try_recv_batch(out)
    }

    #[inline]
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<usize> {
        self.writer.try_send_batch(batch)
    }

    pub fn close_peer(&mut self, _peer_addr: &SocketAddr) {
        // UDP has no per-peer connection state to close.
    }
}

pub fn bind_socket(addr: SocketAddr) -> io::Result<socket2::Socket> {
    let socket2_sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;

    if addr.is_ipv6() {
        // Prefer dual-stack listeners so a single IPv6 socket can accept IPv4-mapped peers.
        socket2_sock.set_only_v6(false)?;
    }

    socket2_sock.set_nonblocking(true)?;
    socket2_sock.set_reuse_address(true)?;
    socket2_sock.set_reuse_port(true)?;

    socket2_sock.set_recv_buffer_size(SOCKET_RECV_SIZE)?;
    socket2_sock.set_send_buffer_size(SOCKET_SEND_SIZE)?;
    socket2_sock.bind(&addr.into())?;

    Ok(socket2_sock)
}

pub fn from_socket(
    socket2_sock: socket2::Socket,
    external_addr: Option<SocketAddr>,
) -> io::Result<UdpTransport> {
    let send_buf_size = socket2_sock.send_buffer_size()?;
    let recv_buf_size = socket2_sock.recv_buffer_size()?;

    let gro_enabled = setsockopt(&socket2_sock, sockopt::UdpGroSegment, &true).is_ok();

    let gso_capable = if setsockopt(
        &socket2_sock,
        sockopt::UdpGsoSegment,
        &GSO_PROBE_SEGMENT_SIZE,
    )
    .is_ok()
    {
        let _ = setsockopt(
            &socket2_sock,
            sockopt::UdpGsoSegment,
            &DISABLE_GSO_SEGMENT_SIZE,
        );
        true
    } else {
        false
    };

    let state_fd = socket2_sock.try_clone()?;
    let sock = tokio::net::UdpSocket::from_std(socket2_sock.into())?;
    let writer_sock = Arc::new(sock);

    let local_addr = external_addr.unwrap_or(writer_sock.local_addr()?);
    let reader_sock = writer_sock.clone();
    drop(state_fd);

    let buffer = <[[u8; GRO_SLOT_SIZE]; BATCH_SIZE]>::new_box_zeroed()
        .expect("failed to allocate zeroed UDP receive buffer");

    let reader = UdpTransportReader {
        sock: reader_sock,
        local_addr,
        gro_enabled,
        buffer,
    };

    let writer = UdpTransportWriter {
        sock: writer_sock,
        local_addr,
        gso_capable,
        drop_count: 0,
        send_batch_limit: send_buf_size / 2,
    };

    tracing::info!(
        %local_addr,
        recv_buf = fmt_bytes(recv_buf_size),
        send_buf = fmt_bytes(send_buf_size),
        gro = gro_enabled,
        gso = gso_capable,
        "UDP socket bound"
    );

    Ok(UdpTransport { reader, writer })
}

pub async fn bind(addr: SocketAddr, external_addr: Option<SocketAddr>) -> io::Result<UdpTransport> {
    from_socket(bind_socket(addr)?, external_addr)
}

pub struct UdpTransportReader {
    sock: Arc<tokio::net::UdpSocket>,
    local_addr: SocketAddr,
    gro_enabled: bool,

    buffer: Box<[[u8; GRO_SLOT_SIZE]; BATCH_SIZE]>,
}

impl UdpTransportReader {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn gro_enabled(&self) -> bool {
        self.gro_enabled
    }

    #[inline]
    pub async fn readable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::READABLE).await?;
        Ok(())
    }

    #[inline]
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        let Self {
            sock,
            local_addr,
            gro_enabled,
            buffer,
        } = self;
        let local_addr = *local_addr;
        let gro_enabled = *gro_enabled;

        sock.try_io(tokio::io::Interest::READABLE, || {
            let mut slot_iter = buffer.iter_mut();
            let mut iovs: [[IoSliceMut; 1]; BATCH_SIZE] = std::array::from_fn(|_| {
                let slot = slot_iter
                    .next()
                    .expect("buffer has exactly BATCH_SIZE slots");
                [IoSliceMut::new(slot.as_mut_slice())]
            });
            drop(slot_iter);

            let fd = sock.as_raw_fd();
            let mut headers = MultiHeaders::preallocate(BATCH_SIZE, Some(cmsg_space!(i32)));

            let mut received = [None; BATCH_SIZE];
            match recvmmsg(fd, &mut headers, iovs.iter_mut(), MsgFlags::empty(), None) {
                Ok(results) => {
                    for (slot_idx, item) in results.enumerate() {
                        debug_assert!(slot_idx < BATCH_SIZE);
                        if item
                            .flags
                            .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
                        {
                            continue;
                        }
                        let total_len = item.bytes;
                        debug_assert!(total_len <= GRO_SLOT_SIZE);
                        if total_len == 0 {
                            continue;
                        }

                        let src = match item.address.as_ref().map(sockaddr_to_std).transpose()? {
                            Some(addr) => normalize_v4_mapped(addr),
                            None => continue, // no source address, can't attribute this datagram
                        };

                        // Default: this slot holds exactly one datagram. If GRO
                        // coalesced several same-size datagrams into it, the
                        // kernel tells us via the UdpGroSegments cmsg.
                        let mut stride = total_len;
                        if gro_enabled && let Ok(cmsgs) = item.cmsgs() {
                            for cmsg in cmsgs {
                                if let ControlMessageOwned::UdpGroSegments(seg) = cmsg {
                                    if seg > INVALID_GRO_SEGMENT_SIZE {
                                        stride = (seg as usize).min(total_len);
                                    }
                                    break;
                                }
                            }
                        }

                        received[slot_idx] = Some((src, stride, total_len));
                        debug_assert_ne!(stride, 0);
                        debug_assert!(stride <= total_len);
                    }
                }
                Err(Errno::EWOULDBLOCK) => return Err(io::Error::from(ErrorKind::WouldBlock)),
                Err(e) => return Err(io::Error::from(e)),
            }
            let prev_len = out.len();
            for (slot_idx, entry) in received.into_iter().enumerate() {
                let Some((src, stride, total_len)) = entry else {
                    continue;
                };
                let buf = buffer[slot_idx][..total_len].to_vec();
                out.push(RecvPacketBatch {
                    src,
                    dst: local_addr,
                    buf,
                    stride,
                    len: total_len,
                    transport: Transport::Udp(UdpMode::Batch),
                    offset: 0,
                });
            }
            Ok(out.len() - prev_len)
        })
    }
}

pub struct UdpTransportWriter {
    sock: Arc<tokio::net::UdpSocket>,
    local_addr: SocketAddr,
    gso_capable: bool,
    drop_count: usize,
    send_batch_limit: usize,
}

impl UdpTransportWriter {
    /// Builds another independent writer handle over the same underlying
    /// socket. Each write creates its syscall scratch locally, so this can be
    /// used by another task without sharing mutable header state.
    pub fn fork(&self) -> Self {
        Self {
            sock: self.sock.clone(),
            local_addr: self.local_addr,
            gso_capable: self.gso_capable,
            drop_count: 0,
            send_batch_limit: self.send_batch_limit,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        if self.gso_capable {
            UDP_MAX_GSO_SEGMENTS
        } else {
            SINGLE_SEGMENT
        }
    }

    #[inline]
    pub async fn writable(&self) -> io::Result<()> {
        self.sock.ready(tokio::io::Interest::WRITABLE).await?;
        Ok(())
    }

    /// Sends every packet in `batch`. GSO's segment-size cmsg applies to an
    /// entire `sendmmsg()` call, not per-message, so packets are grouped into
    /// contiguous runs sharing a `segment_size` and one `sendmmsg()` is
    /// issued per run (and per BATCH_SIZE chunk within a run).
    ///
    /// Lossy by design: if the kernel send buffer is full, the offending
    /// group is dropped (counted, rate-limited-logged) rather than queued or
    /// retried, matching UDP's unreliable-delivery contract.
    #[inline]
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<usize> {
        let mut sent = 0usize;
        let packets = &batch.packets;
        let mut i = 0;

        while i < packets.len() {
            // UDP_SEGMENT is encoded in every mmsghdr in a `sendmmsg` call.
            // Keep ordinary datagrams out of a GSO run: attaching a segment
            // cmsg to a one-segment message is rejected by some kernels.
            let gso_seg = self
                .gso_capable
                .then_some(packets[i].segment_size)
                .filter(|&seg| seg < packets[i].buf.len());
            let mut j = i + 1;
            let mut bytes = packets[i].buf.len();
            while j < packets.len()
                && self
                    .gso_capable
                    .then_some(packets[j].segment_size)
                    .filter(|&seg| seg < packets[j].buf.len())
                    == gso_seg
                && (j - i) < BATCH_SIZE
                && bytes + packets[j].buf.len() <= self.send_batch_limit
            {
                bytes += packets[j].buf.len();
                j += 1;
            }
            sent += self.send_group(&packets[i..j], gso_seg)?;
            i = j;
        }

        Ok(sent)
    }

    fn send_group(&mut self, group: &[SendPacket], gso_seg: Option<usize>) -> io::Result<usize> {
        debug_assert!(!group.is_empty());
        debug_assert!(group.len() <= BATCH_SIZE);
        if group.is_empty() {
            return Ok(0);
        }

        for p in group {
            debug_assert!(!p.buf.is_empty(), "SendPacket buffer must not be empty");
            debug_assert_ne!(p.segment_size, 0);
            debug_assert!(p.segment_size <= p.buf.len());
            debug_assert!(p.buf.len() <= MAX_UDP_GSO_PAYLOAD_SIZE);
            debug_assert!(
                p.segment_size >= p.buf.len()
                    || p.buf.len().div_ceil(p.segment_size) <= UDP_MAX_GSO_SEGMENTS
            );
            debug_assert_eq!(
                gso_seg,
                self.gso_capable
                    .then_some(p.segment_size)
                    .filter(|&seg| seg < p.buf.len())
            );
            if p.buf.is_empty()
                || p.segment_size == 0
                || p.segment_size > p.buf.len()
                || p.segment_size > u16::MAX as usize
                || p.buf.len() > MAX_UDP_GSO_PAYLOAD_SIZE
                || (p.segment_size < p.buf.len()
                    && p.buf.len().div_ceil(p.segment_size) > UDP_MAX_GSO_SEGMENTS)
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "invalid UDP packet segment size",
                ));
            }
        }

        // sendmmsg is capped at BATCH_SIZE, so keep its transient pointer
        // lists on the stack. Allocating these Vecs on every submission was
        // a significant part of CPU time for small RTP packets.
        let mut iovs: [[IoSlice<'_>; 1]; BATCH_SIZE] = std::array::from_fn(|_| [IoSlice::new(&[])]);
        let mut addrs: [Option<SockaddrStorage>; BATCH_SIZE] = std::array::from_fn(|_| None);
        for (index, packet) in group.iter().enumerate() {
            iovs[index] = [IoSlice::new(packet.buf)];
            addrs[index] = Some(SockaddrStorage::from(packet.dst));
        }

        // GSO only makes sense (and is only legal) when segment_size is
        // strictly smaller than at least one packet in the group, and only
        // when we've confirmed the kernel supports UDP_SEGMENT.
        let gso_seg = gso_seg.map(|size| size as u16);
        let cmsg = gso_seg.as_ref().map(ControlMessage::UdpGsoSegments);

        let sock = &self.sock;
        let result = sock.try_io(tokio::io::Interest::WRITABLE, || {
            let fd = sock.as_raw_fd();
            let cmsgs = cmsg.as_slice();
            let mut headers = if cmsg.is_some() {
                MultiHeaders::preallocate(BATCH_SIZE, Some(cmsg_space!(u16)))
            } else {
                MultiHeaders::preallocate(BATCH_SIZE, None)
            };
            match sendmmsg(
                fd,
                &mut headers,
                &iovs[..group.len()],
                &addrs[..group.len()],
                cmsgs,
                MsgFlags::empty(),
            ) {
                Ok(results) => Ok(results.count()),
                Err(Errno::EWOULDBLOCK) => Err(io::Error::from(ErrorKind::WouldBlock)),
                Err(e) => Err(io::Error::from(e)),
            }
        });

        match result {
            Ok(count) => {
                self.record_drop(group.len() - count);
                Ok(count)
            }
            // Lossy: kernel buffer full — drop this group rather than queue it.
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                self.record_drop(group.len());
                Ok(group.len())
            }
            Err(err) => {
                tracing::trace!("try_send_batch failed with {err}");
                Err(err)
            }
        }
    }

    fn record_drop(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        if self.drop_count.is_multiple_of(DROP_LOG_INTERVAL) {
            tracing::warn!(dropped = count, "udp dropped packets during sendmmsg");
        }
        self.drop_count += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{SendPacket, SendPacketBatch};
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
        let mut transport = bind(addr, None).await.unwrap();
        let send_addr = transport.local_addr();

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let payload = (0..1500).map(|i| (i % 256) as u8).collect::<Vec<_>>();
        let packets = [SendPacket {
            dst: recv_addr,
            buf: &payload,
            segment_size: 500,
        }];
        let batch = SendPacketBatch { packets: &packets };

        transport.writable().await.unwrap();
        let sent = transport.try_send_batch(&batch).unwrap();
        assert_eq!(sent, 1, "UDP transport should accept the batch for egress");

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

    #[tokio::test]
    async fn udp_transport_writer_fans_out_to_multiple_destinations() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut transport = bind(addr, None).await.unwrap();

        let r1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let p1 = b"hello-r1".to_vec();
        let p2 = b"hello-r2-longer".to_vec();

        let packets = [
            SendPacket {
                dst: r1.local_addr().unwrap(),
                buf: &p1,
                segment_size: p1.len(),
            },
            SendPacket {
                dst: r2.local_addr().unwrap(),
                buf: &p2,
                segment_size: p2.len(),
            },
        ];
        let batch = SendPacketBatch { packets: &packets };

        transport.writable().await.unwrap();
        let sent = transport.try_send_batch(&batch).unwrap();
        assert_eq!(sent, 2);

        let mut buf = vec![0u8; 64];
        let (n1, _) = tokio::time::timeout(Duration::from_millis(250), r1.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n1], &p1[..]);

        let (n2, _) = tokio::time::timeout(Duration::from_millis(250), r2.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n2], &p2[..]);
    }

    #[tokio::test]
    async fn gso_gro_preserves_a_full_segment_batch() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut sender = bind(addr, None).await.unwrap();
        let mut receiver = bind(addr, None).await.unwrap();
        if sender.max_gso_segments() == SINGLE_SEGMENT || !receiver.reader.gro_enabled() {
            return;
        }

        let segment_size = MAX_UDP_PAYLOAD_SIZE;
        let segment_count = sender
            .max_gso_segments()
            .min(MAX_UDP_GSO_PAYLOAD_SIZE / segment_size);
        let mut payload = vec![0; segment_size * segment_count];
        for (index, segment) in payload.chunks_exact_mut(segment_size).enumerate() {
            segment.fill(index as u8);
        }
        let packets = [SendPacket {
            dst: receiver.local_addr(),
            buf: &payload,
            segment_size,
        }];

        sender.writable().await.unwrap();
        assert_eq!(
            sender
                .try_send_batch(&SendPacketBatch { packets: &packets })
                .unwrap(),
            1
        );

        receiver.readable().await.unwrap();
        let mut batches = Vec::new();
        receiver.try_recv_batch(&mut batches).unwrap();
        let mut received = Vec::new();
        for batch in &mut batches {
            while let Some(packet) = batch.next_packet() {
                received.push(packet.to_vec());
            }
        }

        assert_eq!(received.len(), segment_count);
        for (index, packet) in received.iter().enumerate() {
            assert_eq!(packet, &vec![index as u8; segment_size]);
        }
    }

    #[tokio::test]
    async fn receiver_drains_a_burst_across_multiple_recvmmsg_calls() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut receiver = bind(addr, None).await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet_count = BATCH_SIZE * 2;

        for index in 0..packet_count {
            let payload = vec![index as u8; MAX_UDP_PAYLOAD_SIZE - index % 2];
            sender
                .send_to(&payload, receiver.local_addr())
                .await
                .unwrap();
        }

        let mut received = Vec::with_capacity(packet_count);
        while received.len() < packet_count {
            tokio::time::timeout(Duration::from_millis(250), receiver.readable())
                .await
                .unwrap()
                .unwrap();
            let mut batches = Vec::new();
            receiver.try_recv_batch(&mut batches).unwrap();
            for batch in &mut batches {
                while let Some(packet) = batch.next_packet() {
                    received.push(packet.to_vec());
                }
            }
        }

        assert_eq!(received.len(), packet_count);
        for (index, packet) in received.iter().enumerate() {
            assert_eq!(packet.len(), MAX_UDP_PAYLOAD_SIZE - index % 2);
            assert!(packet.iter().all(|&byte| byte == index as u8));
        }
    }

    #[tokio::test]
    async fn gro_preserves_a_full_receive_aggregate() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut receiver = bind(addr, None).await.unwrap();
        if !receiver.reader.gro_enabled() {
            return;
        }
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        for index in 0..UDP_MAX_GSO_SEGMENTS {
            let payload = vec![index as u8; MAX_UDP_PAYLOAD_SIZE];
            sender
                .send_to(&payload, receiver.local_addr())
                .await
                .unwrap();
        }

        let mut received = Vec::with_capacity(UDP_MAX_GSO_SEGMENTS);
        while received.len() < UDP_MAX_GSO_SEGMENTS {
            tokio::time::timeout(Duration::from_millis(250), receiver.readable())
                .await
                .unwrap()
                .unwrap();
            let mut batches = Vec::new();
            receiver.try_recv_batch(&mut batches).unwrap();
            for batch in &mut batches {
                while let Some(packet) = batch.next_packet() {
                    received.push(packet.to_vec());
                }
            }
        }

        assert_eq!(received.len(), UDP_MAX_GSO_SEGMENTS);
        for (index, packet) in received.iter().enumerate() {
            assert_eq!(packet, &vec![index as u8; MAX_UDP_PAYLOAD_SIZE]);
        }
    }
}
