//! RFC 4571 framing over TCP.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`. The
//! length prefix is 16 bits and peer-controlled, so every `offset + len` is an
//! offset a peer chose against a buffer it did not. With `overflow-checks` off
//! in release a wrap would not stop — it would index somewhere else in the
//! stream or report a frame that is not there.

use super::{ReceiveTime, RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use crate::{mailbox, net::SendPacket, sync::Arc};
use bytes::{Buf, BufMut, BytesMut};
use pulsebeam_core::net::{TcpReadHalf, TcpStream, TcpWriteHalf, split_tcp};
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// RFC 4571 carries a 16-bit unsigned payload length. TCP does not inherit
/// UDP's MTU boundary, so a valid frame may be larger than one datagram.
const MAX_PEEK_FRAME_SIZE: usize = u16::MAX as usize;

/// A TCP stream wrapper that replays bytes already read from the socket before
/// delegating further reads to the kernel.
///
/// When the controller peeks the first RFC 4571 frame to extract the ICE ufrag
/// for shard routing, it stores those wire bytes here.  The shard then sees
/// the frame through the normal `try_read` / `try_recv_batch` path, with no
/// knowledge that the bytes were pre-read.
#[derive(Debug)]
pub struct BufferedTcpStream {
    stream: TcpStream,
    pending: BytesMut,
    received_at: ReceiveTime,
}

impl BufferedTcpStream {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            pending: BytesMut::new(),
            received_at: ReceiveTime::now(),
        }
    }

    /// Wrap `stream` and pre-fill the read buffer with `bytes` — the exact
    /// bytes that were already read from the socket (wire-format, including any
    /// RFC 4571 length prefix).  Subsequent `try_read` calls drain these bytes
    /// first before touching the kernel socket.
    pub fn with_buffered(stream: TcpStream, bytes: Vec<u8>, received_at: ReceiveTime) -> Self {
        Self {
            stream,
            pending: BytesMut::from(bytes.as_slice()),
            received_at,
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.stream.set_nodelay(nodelay)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream.local_addr()
    }

    /// Consume the stream, returning the raw `TcpStream` and any pre-buffered
    /// bytes.  Used by `TcpTransport::add_connection` to split the stream and
    /// synchronously drain pending frames before spawning the read task.
    pub fn into_parts(self) -> (TcpStream, BytesMut, ReceiveTime) {
        (self.stream, self.pending, self.received_at)
    }

    /// Read the first RFC 4571 frame from a freshly accepted `stream`, within
    /// `timeout`, and return a `BufferedTcpStream` that will replay those bytes
    /// plus the decoded payload for ICE ufrag extraction.
    ///
    /// This is the only place that knows the wire format of the framing.  The
    /// caller only sees a ready-to-use stream and the raw payload bytes.
    ///
    /// Returns an error on I/O failure, malformed framing, or timeout.
    pub async fn read_first_frame(
        mut stream: TcpStream,
        timeout: Duration,
    ) -> io::Result<(BufferedTcpStream, Vec<u8>)> {
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(timeout, async {
            let mut header = [0u8; 2];
            stream.read_exact(&mut header).await?;
            let len = u16::from_be_bytes(header) as usize;

            if len == 0 || len > MAX_PEEK_FRAME_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("first TCP frame length {len} out of range"),
                ));
            }

            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).await?;
            let received_at = ReceiveTime::now();

            let mut raw = Vec::with_capacity(len.saturating_add(2));
            raw.extend_from_slice(&header);
            raw.extend_from_slice(&payload);

            Ok((
                BufferedTcpStream::with_buffered(stream, raw, received_at),
                payload,
            ))
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP first-frame read timed out"))?
    }
}

/// Maximum RFC 4571 payload length accepted on a TCP passive connection.
const MAX_FRAME_SIZE: usize = u16::MAX as usize;
const MAX_CONNECTIONS: usize = 10_000;
const MAX_CONNS_PER_IP: usize = 20;
/// Overflow guard: `recv_buf` holds at most one partial RFC 4571 frame.
const MAX_RECV_BUF: usize = MAX_FRAME_SIZE + 2 + 64;
const MAX_TCP_WRITE_BUF: usize = 256 * 1024;
/// How long a connection may be idle before the read task reaps it.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Channel capacity for decoded frames, write readiness and close notifications.
const TCP_EVENT_CAPACITY: usize = 8_192;

/// A stream incarnation, unique for the lifetime of its owning TCP transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TcpFlowId(u64);

impl TcpFlowId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Events produced by per-connection IO tasks and consumed by `TcpTransport`.
enum TcpEvent {
    /// A fully-decoded RFC 4571 payload ready to hand to the shard.
    Frame {
        src: SocketAddr,
        flow: TcpFlowId,
        payload: Vec<u8>,
        received_at: ReceiveTime,
    },
    /// A stalled stream can resume its original pending write buffer.
    Writable { src: SocketAddr, flow: TcpFlowId },
    /// The read task exited (EOF, error, idle timeout, or cancellation).
    Closed { src: SocketAddr, flow: TcpFlowId },
}

/// Per-connection write-side state owned by the shard.
struct TcpConn {
    flow: TcpFlowId,
    /// Write half — `try_write` is `&self` so no locking needed.
    write: TcpWriteHalf,
    /// Bytes already framed but not yet accepted by the kernel. A partial TCP
    /// write must be resumed before a later frame can be sent.
    pending: BytesMut,
    /// Cancels the associated IO task when the connection is removed.
    cancel: CancellationToken,
    // Coalesced pressure-edge notification only; write bytes remain shard-owned.
    write_ready: Arc<Notify>,
    write_watch_armed: bool,
}

impl TcpConn {
    fn flush_pending(&mut self) -> io::Result<()> {
        for _ in 0..super::BATCH_SIZE {
            if self.pending.is_empty() {
                break;
            }
            match self.write.try_write(&self.pending) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                Ok(count) => {
                    if count > self.pending.len() {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    self.pending.advance(count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn arm_write_ready(&mut self) {
        if !self.pending.is_empty() && !self.write_watch_armed {
            self.write_watch_armed = true;
            self.write_ready.notify_one();
        }
    }
}

/// Read and writable-readiness task for a single TCP connection.
///
/// Runs on the shard runtime and owns the read half, so the shard's hot path
/// never touches it. Decodes RFC 4571
/// frames and forwards them through `tx`.  Exits on EOF, I/O error, idle
/// timeout, or cancellation. EOF, error and timeout produce `TcpEvent::Closed`;
/// the owning transport reports local cancellation.
struct TcpIoTask {
    peer_addr: SocketAddr,
    flow: TcpFlowId,
    read: TcpReadHalf,
    pending: BytesMut,
    received_at: ReceiveTime,
    tx: mailbox::Sender<TcpEvent>,
    cancel: CancellationToken,
    write_ready: Arc<Notify>,
}

impl TcpIoTask {
    async fn run(self) {
        let Self {
            peer_addr,
            flow,
            read,
            mut pending,
            mut received_at,
            tx,
            cancel,
            write_ready,
        } = self;
        let mut recv_buf = BytesMut::with_capacity(MAX_FRAME_SIZE + 2);
        // Carry over any bytes already decoded by the controller's first-frame read.
        if !pending.is_empty() {
            recv_buf.extend_from_slice(&pending);
            pending.clear();
        }

        let mut watch_write = false;
        'outer: loop {
            // Decode all complete frames currently in the buffer before waiting.
            while let Some(len) = frame_len_at(&recv_buf, 0) {
                if len == 0 || len > MAX_FRAME_SIZE {
                    tracing::warn!(%peer_addr, len, "Invalid TCP frame length, closing connection");
                    break 'outer;
                }
                if recv_buf.len() < len.saturating_add(2) {
                    break; // partial frame
                }
                recv_buf.advance(2);
                let payload = recv_buf.split_to(len).to_vec();
                let event = TcpEvent::Frame {
                    src: peer_addr,
                    flow,
                    payload,
                    received_at,
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    result = tx.send(event) => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }

            if recv_buf.len() > MAX_RECV_BUF {
                tracing::warn!(%peer_addr, "TCP recv_buf overflow, closing connection");
                break;
            }

            let read_ready = tokio::time::timeout(READ_TIMEOUT, read.readable());
            tokio::pin!(read_ready);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break 'outer,
                    _ = write_ready.notified(), if !watch_write => watch_write = true,
                    result = pulsebeam_core::net::tcp_write_ready(&read), if watch_write => {
                        if let Err(error) = result {
                            tracing::warn!(%peer_addr, %error, "TCP writable error");
                            break 'outer;
                        }
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            result = tx.send(TcpEvent::Writable { src: peer_addr, flow }) => {
                                if result.is_err() {
                                    return;
                                }
                            }
                        }
                        watch_write = false;
                    }
                    result = &mut read_ready => {
                        match result {
                            Err(_) => {
                                tracing::warn!(%peer_addr, "TCP connection idle timeout");
                                break 'outer;
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(%peer_addr, %error, "TCP readable error");
                                break 'outer;
                            }
                            Ok(Ok(())) => break,
                        }
                    }
                }
            }

            // Non-blocking read from the kernel.
            let mut tmp = [0u8; 4096];
            match read.try_read(&mut tmp) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    received_at = ReceiveTime::now();
                    match tmp.get(..n) {
                        Some(chunk) => recv_buf.put_slice(chunk),
                        None => break,
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    tracing::warn!(%peer_addr, error = ?e, "TCP read error");
                    break;
                }
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = tx.send(TcpEvent::Closed { src: peer_addr, flow }) => {}
        }
    }
}

/// TCP transport for a single shard — **passive (accept-only) per RFC 6544**.
///
/// ## Thread-per-core design
///
/// One IO task is created per accepted connection. Each task owns the read
/// half and watches readiness; all pending writes remain shard-owned. The
/// shard's hot path only consumes a single mailbox, independent of the number
/// of open streams, so TCP
/// has zero overhead on the UDP forwarding path when no TCP data is arriving.
///
/// ## Complexity
///
/// | Operation | Complexity |
/// |-----------|------------|
/// | `readable()` | O(1) — single channel recv |
/// | `try_recv_batch()` | O(frames ready) |
/// | `try_send_batch()` | O(1) — single HashMap lookup |
/// | `add_connection()` | O(1) — split + spawn |
/// | `remove_conn()` | O(1) — cancel + HashMap remove |
pub struct TcpTransport {
    local_addr: SocketAddr,
    /// Write halves and send buffers, keyed by peer address.
    conns: HashMap<SocketAddr, TcpConn>,
    /// Per-IP connection counts for `MAX_CONNS_PER_IP` enforcement.
    ip_counts: HashMap<IpAddr, usize>,
    /// Sender cloned into each read task.
    event_tx: mailbox::Sender<TcpEvent>,
    /// Receiver drained by `try_recv_batch`.
    event_rx: mailbox::Receiver<TcpEvent>,
    /// One event stashed by `readable()` so `try_recv_batch` can always find it.
    peeked: Option<TcpEvent>,
    closed: VecDeque<(SocketAddr, TcpFlowId)>,
    next_flow: Option<u64>,
}

impl TcpTransport {
    pub fn new(local_addr: SocketAddr) -> Self {
        let (event_tx, event_rx) = mailbox::new(TCP_EVENT_CAPACITY);
        Self {
            local_addr,
            conns: HashMap::new(),
            ip_counts: HashMap::new(),
            event_tx,
            event_rx,
            peeked: None,
            closed: VecDeque::new(),
            next_flow: Some(1),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        64
    }

    pub fn active_connections(&self) -> usize {
        self.conns.len()
    }

    /// Accept a new TCP connection that the controller has already validated.
    ///
    /// Pre-buffered bytes (the first STUN frame read by the controller for ufrag
    /// routing) are decoded synchronously and pushed into the event channel so
    /// `try_recv_batch` can deliver them without awaiting task execution.  The
    /// read task handles all subsequent bytes.
    pub fn add_connection(
        &mut self,
        stream: BufferedTcpStream,
        peer_addr: SocketAddr,
    ) -> io::Result<TcpFlowId> {
        let raw_flow = self
            .next_flow
            .ok_or_else(|| io::Error::other("TCP flow identity space exhausted"))?;
        let flow = TcpFlowId(raw_flow);
        self.next_flow = raw_flow.checked_add(1);
        // Retired flows keep their admission slot until the caller drains the notice.
        if self.conns.len().saturating_add(self.closed.len()) >= MAX_CONNECTIONS {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "TCP connection limit reached",
            ));
        }
        if self.conns.contains_key(&peer_addr) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "TCP peer already connected",
            ));
        }

        let peer_ip = peer_addr.ip();
        let count = self.ip_counts.entry(peer_ip).or_insert(0);
        if *count >= MAX_CONNS_PER_IP {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "TCP per-IP connection limit reached",
            ));
        }
        *count = count.saturating_add(1);

        if let Err(e) = stream.set_nodelay(true) {
            if let Some(count) = self.ip_counts.get_mut(&peer_ip) {
                // Wrapping here would pin the per-IP count near u32::MAX and
                // lock that address out of the node for good.
                *count = count.saturating_sub(1);
            } else {
                debug_assert!(false, "per-IP count vanished between insert and rollback");
            }
            return Err(e);
        }

        let (raw, mut pending, received_at) = stream.into_parts();

        // Synchronously decode any pre-buffered bytes and push to the channel.
        // This keeps the invariant that frames available at add_connection time
        // are immediately visible to try_recv_batch, without needing the task
        // to execute first.
        if !pending.is_empty() {
            let mut pos = pending;
            while let Some(len) = frame_len_at(&pos, 0) {
                if len == 0 || len > MAX_FRAME_SIZE || pos.len() < len.saturating_add(2) {
                    break;
                }
                let payload = pos
                    .get(2..len.saturating_add(2))
                    .unwrap_or_default()
                    .to_vec();
                if self
                    .event_tx
                    .try_send(TcpEvent::Frame {
                        src: peer_addr,
                        flow,
                        payload,
                        received_at,
                    })
                    .is_err()
                {
                    break;
                }
                pos.advance(len.saturating_add(2));
            }
            pending = pos;
        }

        let (read, write) = split_tcp(raw);
        let cancel = CancellationToken::new();
        let write_ready = Arc::new(Notify::new());

        // The IO task owns only the read half and readiness watches. Pending
        // framed writes remain exclusively owned and drained by the shard.
        tokio::task::spawn(
            TcpIoTask {
                peer_addr,
                flow,
                read,
                pending,
                received_at,
                tx: self.event_tx.clone(),
                cancel: cancel.clone(),
                write_ready: Arc::clone(&write_ready),
            }
            .run(),
        );

        self.conns.insert(
            peer_addr,
            TcpConn {
                flow,
                write,
                pending: BytesMut::new(),
                cancel,
                write_ready,
                write_watch_armed: false,
            },
        );
        tracing::debug!(%peer_addr, "TCP connection added to shard");
        Ok(flow)
    }

    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        if let Some(flow) = self.remove_conn(peer_addr) {
            self.closed.push_back((*peer_addr, flow));
        }
    }

    pub fn flow_for_peer(&self, peer_addr: &SocketAddr) -> Option<TcpFlowId> {
        self.conns.get(peer_addr).map(|conn| conn.flow)
    }

    pub fn close_flow(&mut self, peer_addr: &SocketAddr, flow: TcpFlowId) {
        if self.flow_for_peer(peer_addr) == Some(flow) {
            self.close_peer(peer_addr);
        }
    }

    fn remove_conn(&mut self, peer_addr: &SocketAddr) -> Option<TcpFlowId> {
        let conn = self.conns.remove(peer_addr)?;
        conn.cancel.cancel();
        let ip = peer_addr.ip();
        if let Some(c) = self.ip_counts.get_mut(&ip) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.ip_counts.remove(&ip);
            }
        }
        tracing::info!(%peer_addr, "TCP connection removed from shard");
        Some(conn.flow)
    }

    /// Resolves when a decoded frame, write readiness or close notice is
    /// available.  Parks forever when no events are queued, letting the shard's
    /// `select!` fall through to other arms (e.g. UDP).
    ///
    /// O(1): waits on a single channel regardless of the number of connections.
    pub async fn readable(&mut self) -> io::Result<()> {
        if self.peeked.is_some() || !self.closed.is_empty() {
            return Ok(());
        }
        match self.event_rx.recv().await {
            Some(event) => {
                self.peeked = Some(event);
                Ok(())
            }
            // All senders dropped — only possible if TcpTransport is being torn down.
            None => std::future::pending().await,
        }
    }

    /// Drain at most one bounded batch of immediately available TCP events into `out`.
    ///
    /// Returns `WouldBlock` when no frames were produced (closed-connection
    /// events are processed but produce no output frames).
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        let start = out.len();
        let _ = self.try_recv_events(out, &mut Vec::new());
        let received = out.len().saturating_sub(start);
        if received == 0 {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(received)
        }
    }

    /// Drain one bounded batch, retaining exact EOF, error and local retirement
    /// identities alongside frames. The result counts consumed events, including
    /// stale frames. Retirement notices are moved out, not retained as history.
    pub fn try_recv_events(
        &mut self,
        out: &mut Vec<RecvPacketBatch>,
        closed: &mut Vec<(SocketAddr, TcpFlowId)>,
    ) -> io::Result<usize> {
        let local_addr = self.local_addr;
        let mut consumed = 0usize;
        for _ in 0..super::BATCH_SIZE {
            if let Some(flow) = self.closed.pop_front() {
                closed.push(flow);
                consumed = consumed.saturating_add(1);
                continue;
            }
            let event = self.peeked.take().or_else(|| self.event_rx.try_recv().ok());
            let Some(event) = event else {
                break;
            };
            consumed = consumed.saturating_add(1);
            self.handle_event(event, out, local_addr, closed);
        }
        if consumed == 0 {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Ok(consumed)
        }
    }

    fn handle_event(
        &mut self,
        event: TcpEvent,
        out: &mut Vec<RecvPacketBatch>,
        local_addr: SocketAddr,
        closed: &mut Vec<(SocketAddr, TcpFlowId)>,
    ) {
        match event {
            TcpEvent::Frame {
                src,
                flow,
                payload,
                received_at,
            } => {
                // An address can already belong to a replacement stream.
                let Some(conn) = self.conns.get(&src) else {
                    return;
                };
                if conn.flow != flow {
                    return;
                }
                let len = payload.len();
                out.push(RecvPacketBatch {
                    src,
                    dst: local_addr,
                    buf: payload,
                    stride: len,
                    len,
                    transport: Transport::Tcp,
                    tcp_flow: Some(flow),
                    received_at,
                    offset: 0,
                });
            }
            TcpEvent::Writable { src, flow } => {
                let Some(conn) = self.conns.get_mut(&src).filter(|conn| conn.flow == flow) else {
                    return;
                };
                conn.write_watch_armed = false;
                let result = conn.flush_pending();
                conn.arm_write_ready();
                if let Err(error) = result {
                    tracing::warn!(peer_addr = %src, %error, "TCP pending write error");
                    self.close_flow(&src, flow);
                }
            }
            TcpEvent::Closed { src, flow } => {
                if self.flow_for_peer(&src) == Some(flow) {
                    let _ = self.remove_conn(&src);
                }
                closed.push((src, flow));
            }
        }
    }

    /// Frame `batch` per RFC 4571 and write to the peer's stream.
    ///
    /// A batch is accepted only after it has been written or retained in the
    /// bounded per-connection egress buffer. `WouldBlock` leaves it with the
    /// caller so a full egress buffer cannot silently discard valid frames.
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<usize> {
        for (index, group) in batch.packets.iter().enumerate() {
            if !self.try_send_group(group)? {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("TCP egress buffer is full before batch item {index}"),
                ));
            }
        }
        Ok(batch.packets.len())
    }

    /// Submit committed output only to its original stream incarnation.
    pub fn try_send_on_flow(&mut self, flow: TcpFlowId, packet: &SendPacket) -> io::Result<bool> {
        if self.flow_for_peer(&packet.dst) != Some(flow) {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        self.try_send_group(packet)
    }

    pub fn try_send_group(&mut self, batch: &SendPacket) -> io::Result<bool> {
        debug_assert!(batch.segment_size != 0);
        if !self.conns.contains_key(&batch.dst) {
            return Ok(true); // peer gone — treat as sent
        }

        // Frame all segments into a local buffer (not stored on the connection).
        let mut buf = BytesMut::new();
        let mut offset = 0;
        while offset < batch.buf.len() {
            let end = offset
                .saturating_add(batch.segment_size)
                .min(batch.buf.len());
            let Some(seg) = batch.buf.get(offset..end) else {
                debug_assert!(false, "segment {offset}..{end} escapes the batch");
                break;
            };
            let Ok(seg_len) = u16::try_from(seg.len()) else {
                debug_assert!(false, "segment of {} bytes has no 16-bit length", seg.len());
                break;
            };
            buf.put_u16(seg_len);
            buf.put_slice(seg);
            offset = end;
        }

        let outcome = (|| -> io::Result<bool> {
            let Some(conn) = self.conns.get_mut(&batch.dst) else {
                return Ok(true);
            };

            conn.flush_pending()?;
            if !conn.pending.is_empty() {
                if conn.pending.len().saturating_add(buf.len()) > MAX_TCP_WRITE_BUF {
                    return Ok(false);
                }
                conn.pending.extend_from_slice(&buf);
                debug_assert!(conn.pending.len() <= MAX_TCP_WRITE_BUF);
                return Ok(true);
            }

            if buf.len() > MAX_TCP_WRITE_BUF {
                tracing::debug!(
                    peer_addr = %batch.dst,
                    bytes = buf.len(),
                    "TCP batch exceeds bounded write buffer"
                );
                return Ok(true);
            }

            for _ in 0..super::BATCH_SIZE {
                if buf.is_empty() {
                    break;
                }
                match conn.write.try_write(&buf) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "TCP write accepted zero bytes",
                        ));
                    }
                    Ok(n) => {
                        debug_assert!(n <= buf.len());
                        if n > buf.len() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "TCP write returned more bytes than supplied",
                            ));
                        }
                        buf.advance(n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        conn.pending.extend_from_slice(&buf);
                        debug_assert!(conn.pending.len() <= MAX_TCP_WRITE_BUF);
                        return Ok(true);
                    }
                    Err(e) => return Err(e),
                }
            }
            if !buf.is_empty() {
                conn.pending.extend_from_slice(&buf);
            }
            Ok(true)
        })();

        match outcome {
            Ok(accepted) => {
                if let Some(conn) = self.conns.get_mut(&batch.dst) {
                    conn.arm_write_ready();
                }
                return Ok(accepted);
            }
            Err(error) => {
                tracing::warn!(peer_addr = %batch.dst, %error, "TCP write error");
                self.close_peer(&batch.dst);
            }
        }
        Ok(true)
    }
}

/// The RFC 4571 big-endian length prefix at `offset`, or `None` if fewer than
/// two bytes remain there.
fn frame_len_at(buf: &[u8], offset: usize) -> Option<usize> {
    let hi = *buf.get(offset)?;
    let lo = *buf.get(offset.saturating_add(1))?;
    Some(usize::from(u16::from_be_bytes([hi, lo])))
}

/// How many RFC 4571 frames a buffer holds, counting a trailing incomplete one.
///
/// Feeds the dropped-packet counter, so an over-count reports congestion that
/// did not happen. The early exits `return` rather than `break`: they have
/// already counted the frame they stopped on, and falling through to the
/// trailing-remainder check counted it a second time.
#[cfg(test)]
fn count_rfc4571_frames(buf: &[u8]) -> u64 {
    let mut count = 0u64;
    let mut offset = 0usize;
    while offset.saturating_add(2) <= buf.len() {
        let Some(len) = frame_len_at(buf, offset) else {
            return count.saturating_add(1);
        };
        if len == 0 {
            return count.saturating_add(1);
        }
        let total = len.saturating_add(2);
        if offset.saturating_add(total) > buf.len() {
            return count.saturating_add(1);
        }
        count = count.saturating_add(1);
        offset = offset.saturating_add(total);
    }
    if offset < buf.len() {
        count = count.saturating_add(1);
    }
    count
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;
    use pulsebeam_core::net::TcpListener;
    use std::{net::IpAddr, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn run_local<Fut>(test: Fut)
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        crate::testing::run_local(test_host_ip(), test);
    }

    /// RFC 4571 framing: a 2-byte big-endian length, then that many bytes.
    ///
    /// These are boundary cases, not plausible ones. The arithmetic here is
    /// `offset + 2` and `offset + 2 + len` against a buffer the peer controls,
    /// so what matters is the buffer ending exactly at, one before, and one
    /// after each of those, plus a length field large enough to run past the
    /// end on its own.
    mod rfc4571_framing {
        use super::super::{MAX_FRAME_SIZE, count_rfc4571_frames};

        fn frame(len: usize) -> Vec<u8> {
            let mut v = u16::try_from(len)
                .unwrap_or(u16::MAX)
                .to_be_bytes()
                .to_vec();
            v.extend(std::iter::repeat_n(0xab, len));
            v
        }

        #[test]
        fn an_empty_buffer_has_no_frames() {
            assert_eq!(count_rfc4571_frames(&[]), 0);
        }

        #[test]
        fn a_lone_length_byte_is_not_yet_a_frame() {
            assert_eq!(count_rfc4571_frames(&[0x00]), 1, "counted as a partial");
        }

        #[test]
        fn whole_frames_are_counted_exactly() {
            for n in [1usize, 2, 7, MAX_FRAME_SIZE] {
                assert_eq!(count_rfc4571_frames(&frame(n)), 1, "single frame of {n}");
                let mut two = frame(n);
                two.extend(frame(n));
                assert_eq!(count_rfc4571_frames(&two), 2, "two frames of {n}");
            }
        }

        #[test]
        fn a_zero_length_frame_terminates_the_scan() {
            let mut buf = vec![0x00, 0x00];
            buf.extend(frame(4));
            assert_eq!(count_rfc4571_frames(&buf), 1);
        }

        #[test]
        fn a_payload_one_byte_short_counts_as_partial() {
            let mut buf = frame(8);
            buf.pop();
            assert_eq!(count_rfc4571_frames(&buf), 1);
        }

        /// The length field is 16 bits and the buffer is not, so a peer can
        /// always claim more than it sent. `offset + 2 + len` must not run past
        /// the end or wrap.
        #[test]
        fn a_length_larger_than_the_buffer_does_not_run_off_the_end() {
            for claimed in [1u16, 1024, u16::MAX] {
                let mut buf = claimed.to_be_bytes().to_vec();
                buf.push(0xab);
                assert_eq!(count_rfc4571_frames(&buf), 1, "claimed {claimed}");
            }
        }

        /// Every truncation of a well-formed two-frame stream, which walks the
        /// buffer end across both header boundaries and both payload interiors.
        #[test]
        fn every_prefix_of_a_valid_stream_is_counted_without_panicking() {
            let mut full = frame(3);
            full.extend(frame(5));
            for cut in 0..=full.len() {
                let n = count_rfc4571_frames(&full[..cut]);
                assert!(
                    usize::try_from(n).expect("count fits usize") <= full.len(),
                    "prefix of {cut} reported {n} frames"
                );
            }
            assert_eq!(count_rfc4571_frames(&full), 2);
        }

        /// Adversarial bytes rather than well-formed ones: the scan must
        /// terminate and stay in bounds whatever the length fields say.
        #[test]
        fn arbitrary_bytes_never_panic_or_hang() {
            let mut state = 0x2545_f491_4f6c_dd1du64;
            for _ in 0..2_000 {
                let len = (state % 40) as usize;
                let buf: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        u8::try_from((state >> 24) & 0xff).expect("masked to a byte")
                    })
                    .collect();
                let n = count_rfc4571_frames(&buf);
                assert!(usize::try_from(n).expect("count fits usize") <= buf.len().max(1));
            }
        }
    }

    fn test_host_ip() -> IpAddr {
        std::net::Ipv4Addr::LOCALHOST.into()
    }

    /// Connect a client to `listener`, accept the server-side stream, return both.
    async fn make_pair(
        listener: &TcpListener,
    ) -> (pulsebeam_core::net::TcpStream, TcpStream, SocketAddr) {
        let server_addr = listener.local_addr().unwrap();
        let (cli, srv) = tokio::join!(
            pulsebeam_core::net::TcpStream::connect(server_addr),
            listener.accept()
        );
        let client = cli.unwrap();
        let (server_stream, peer_addr) = srv.unwrap();
        (client, server_stream, peer_addr)
    }

    #[test]
    fn test_tcp_rfc_compliance() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let server_addr = listener.local_addr().unwrap();
            let mut sock = TcpTransport::new(server_addr);

            let (mut client, server_stream, peer_addr) = make_pair(&listener).await;
            client.set_nodelay(true).unwrap();
            let flow = sock
                .add_connection(BufferedTcpStream::new(server_stream), peer_addr)
                .unwrap();

            // Ingress: client → server, RFC 4571 framing
            let p1 = b"packet1";
            let p2 = b"packet2-longer";
            let mut frame_bytes = Vec::new();
            for p in [p1.as_slice(), p2.as_slice()] {
                let plen = u16::try_from(p.len()).expect("test frame fits a length prefix");
                frame_bytes.extend_from_slice(&plen.to_be_bytes());
                frame_bytes.extend_from_slice(p);
            }
            client.write_all(&frame_bytes).await.unwrap();

            sock.readable().await.unwrap();
            let mut out = Vec::new();
            // Retry loop: data may arrive piecemeal in tests
            for _ in 0..20 {
                let _ = sock.try_recv_batch(&mut out);
                if out.len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(out.len(), 2);
            assert_eq!(&out[0].buf[..], p1);
            assert_eq!(&out[1].buf[..], p2);

            // Egress: server → client, two segments
            let payload = b"segment1segment2";
            let packets = [SendPacket {
                dst: peer_addr,
                buf: payload,
                segment_size: 8,
            }];
            let batch = SendPacketBatch { packets: &packets };
            sock.try_send_batch(&batch).unwrap();

            for expected in [b"segment1".as_slice(), b"segment2".as_slice()] {
                let len = client.read_u16().await.unwrap();
                assert_eq!(len as usize, expected.len());
                let mut buf = vec![0u8; len as usize];
                client.read_exact(&mut buf).await.unwrap();
                assert_eq!(buf, expected);
            }

            client.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), sock.readable())
                .await
                .unwrap()
                .unwrap();
            out.clear();
            let mut closed = Vec::new();
            assert_eq!(sock.try_recv_events(&mut out, &mut closed).unwrap(), 1);
            assert!(out.is_empty());
            assert_eq!(closed, vec![(peer_addr, flow)]);
            assert_eq!(sock.active_connections(), 0);
            assert_eq!(
                sock.try_recv_events(&mut out, &mut closed)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        });
    }

    /// Verifies that bytes pre-buffered via `with_buffered` are delivered on the
    /// next `try_recv_batch` call — this is the path used when the controller
    /// pre-reads the first STUN frame for ufrag-based routing.
    #[test]
    fn test_initial_payload_delivery() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let server_addr = listener.local_addr().unwrap();
            let mut sock = TcpTransport::new(server_addr);

            let (_client, server_stream, peer_addr) = make_pair(&listener).await;
            let payload = b"stun-request-bytes".to_vec();

            // Build the RFC 4571-framed wire bytes (length header + payload) exactly
            // as read_first_tcp_frame would produce them.
            let mut wire = Vec::with_capacity(2 + payload.len());
            let plen = u16::try_from(payload.len()).expect("test frame fits a length prefix");
            wire.extend_from_slice(&plen.to_be_bytes());
            wire.extend_from_slice(&payload);

            let received_at = ReceiveTime {
                monotonic: tokio::time::Instant::now() - Duration::from_secs(1),
                wall: std::time::UNIX_EPOCH,
            };
            let flow = sock
                .add_connection(
                    BufferedTcpStream::with_buffered(server_stream, wire, received_at),
                    peer_addr,
                )
                .unwrap();

            // Must be immediately available — no bytes sent on the wire yet.
            let mut out = Vec::new();
            let _ = sock.try_recv_batch(&mut out);
            assert_eq!(out.len(), 1);
            assert_eq!(&out[0].buf[..], payload.as_slice());
            assert_eq!(out[0].tcp_flow, Some(flow));
            assert_eq!(out[0].received_at, received_at);

            for _ in 0..=super::super::BATCH_SIZE {
                sock.event_tx
                    .try_send(TcpEvent::Frame {
                        src: peer_addr,
                        flow,
                        payload: vec![2],
                        received_at,
                    })
                    .unwrap_or_else(|_| panic!("test inbox has capacity"));
            }
            assert_eq!(
                sock.try_recv_batch(&mut out).unwrap(),
                super::super::BATCH_SIZE
            );
            assert_eq!(out.len(), super::super::BATCH_SIZE + 1);
            assert!(out.iter().all(|packet| packet.received_at == received_at));
            out.clear();
            assert_eq!(sock.try_recv_batch(&mut out).unwrap(), 1);

            sock.close_flow(&peer_addr, flow);
            let (mut replacement_client, replacement_stream, _) = make_pair(&listener).await;
            let replacement = sock
                .add_connection(BufferedTcpStream::new(replacement_stream), peer_addr)
                .unwrap();
            assert_ne!(flow, replacement);

            out.clear();
            let mut closed = Vec::new();
            sock.try_recv_events(&mut out, &mut closed).unwrap();
            assert_eq!(closed, vec![(peer_addr, flow)]);
            closed.clear();
            sock.handle_event(
                TcpEvent::Frame {
                    src: peer_addr,
                    flow,
                    payload: vec![1],
                    received_at,
                },
                &mut out,
                server_addr,
                &mut closed,
            );
            sock.handle_event(
                TcpEvent::Closed {
                    src: peer_addr,
                    flow,
                },
                &mut out,
                server_addr,
                &mut closed,
            );
            assert_eq!(closed, vec![(peer_addr, flow)]);
            assert!(out.is_empty());
            sock.handle_event(
                TcpEvent::Writable {
                    src: peer_addr,
                    flow,
                },
                &mut out,
                server_addr,
                &mut closed,
            );
            assert_eq!(sock.flow_for_peer(&peer_addr), Some(replacement));
            assert!(!sock.conns.get(&peer_addr).unwrap().write_watch_armed);
            closed.clear();
            for _ in 0..=super::super::BATCH_SIZE {
                sock.event_tx
                    .try_send(TcpEvent::Closed {
                        src: peer_addr,
                        flow,
                    })
                    .unwrap_or_else(|_| panic!("test inbox has capacity"));
            }
            assert_eq!(
                sock.try_recv_events(&mut out, &mut closed).unwrap(),
                super::super::BATCH_SIZE
            );
            assert_eq!(closed.len(), super::super::BATCH_SIZE);
            assert_eq!(sock.try_recv_events(&mut out, &mut closed).unwrap(), 1);
            assert_eq!(closed.len(), super::super::BATCH_SIZE + 1);
            assert_eq!(sock.flow_for_peer(&peer_addr), Some(replacement));

            let packet = SendPacket {
                dst: peer_addr,
                buf: b"replacement",
                segment_size: 11,
            };
            assert_eq!(
                sock.try_send_on_flow(flow, &packet).unwrap_err().kind(),
                io::ErrorKind::NotConnected,
            );
            assert!(sock.try_send_on_flow(replacement, &packet).unwrap());
            assert_eq!(replacement_client.read_u16().await.unwrap(), 11);
            let mut received = [0; 11];
            replacement_client.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"replacement");

            sock.close_flow(&peer_addr, replacement);
            closed.clear();
            assert_eq!(sock.try_recv_events(&mut out, &mut closed).unwrap(), 1);
            assert_eq!(closed, vec![(peer_addr, replacement)]);

            // Unconsumed local retirements cannot allow unbounded connection churn.
            sock.closed
                .resize(MAX_CONNECTIONS, (peer_addr, replacement));
            let (_client, stream, peer) = make_pair(&listener).await;
            assert_eq!(
                sock.add_connection(BufferedTcpStream::new(stream), peer)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::ConnectionRefused
            );
            closed.clear();
            assert_eq!(
                sock.try_recv_events(&mut out, &mut closed).unwrap(),
                super::super::BATCH_SIZE
            );
            assert_eq!(
                sock.closed.len(),
                MAX_CONNECTIONS - super::super::BATCH_SIZE
            );
            let (_client, stream, peer) = make_pair(&listener).await;
            sock.add_connection(BufferedTcpStream::new(stream), peer)
                .unwrap();
        });
    }

    /// A zero-length RFC 4571 frame is not an RTP/RTCP datagram and must not
    /// leave the decoder waiting forever for a payload that cannot arrive.
    #[test]
    fn test_recv_buf_overflow_drops_connection() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let server_addr = listener.local_addr().unwrap();
            let mut sock = TcpTransport::new(server_addr);

            let (mut client, server_stream, peer_addr) = make_pair(&listener).await;
            sock.add_connection(BufferedTcpStream::new(server_stream), peer_addr)
                .unwrap();

            let junk = vec![0x00, 0x00];
            client.write_all(&junk).await.unwrap();
            // Follow with body bytes so the kernel buffer has data to read.
            let filler = vec![0u8; 256];
            let _ = client.write_all(&filler).await; // may fail if server already closed

            // Drain: the shard should detect overflow and remove the connection.
            for _ in 0..20 {
                let mut out = Vec::new();
                let _ = sock.try_recv_batch(&mut out);
                if sock.active_connections() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(
                sock.active_connections(),
                0,
                "zero-length connection should be dropped"
            );
        });
    }

    #[test]
    fn a_valid_large_rfc4571_frame_is_not_rejected_as_an_invalid_length() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let (mut client, server_stream, _) = make_pair(&listener).await;
            let payload = vec![0x5a; 5_635];
            let len = u16::try_from(payload.len()).unwrap();
            client.write_all(&len.to_be_bytes()).await.unwrap();
            client.write_all(&payload).await.unwrap();

            let (_, decoded) =
                BufferedTcpStream::read_first_frame(server_stream, Duration::from_secs(1))
                    .await
                    .unwrap();
            assert_eq!(decoded, payload);
        });
    }

    /// Flooding a peer that has stopped reading never panics and reports
    /// back-pressure once the bounded egress buffer is full.
    #[test]
    fn test_send_to_slow_reader_is_lossy() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let server_addr = listener.local_addr().unwrap();
            let mut sock = TcpTransport::new(server_addr);

            let (client, server_stream, peer_addr) = make_pair(&listener).await;
            sock.add_connection(BufferedTcpStream::new(server_stream), peer_addr)
                .unwrap();

            // Stop reading — kernel TX buffer will fill quickly.
            drop(client);

            // Flood until the bounded egress buffer reports back-pressure.
            let payload = vec![0u8; MAX_FRAME_SIZE];
            for _ in 0..200 {
                let packets = [SendPacket {
                    dst: peer_addr,
                    buf: &payload,
                    segment_size: MAX_FRAME_SIZE,
                }];
                let batch = SendPacketBatch { packets: &packets };
                match sock.try_send_batch(&batch) {
                    Ok(sent) => assert_eq!(sent, 1),
                    Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                }
            }
            // Connection may or may not still be present (depends on whether the
            // kernel signalled a hard error), but we must not have panicked.
        });
    }

    #[test]
    fn test_tcp_multi_peer_isolation() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let server_addr = listener.local_addr().unwrap();
            let mut sock = TcpTransport::new(server_addr);

            let (_c1, s1, p1) = make_pair(&listener).await;
            sock.add_connection(BufferedTcpStream::new(s1), p1).unwrap();

            let (_c2, s2, p2) = make_pair(&listener).await;
            sock.add_connection(BufferedTcpStream::new(s2), p2).unwrap();

            assert_eq!(sock.active_connections(), 2);
        });
    }

    #[test]
    fn test_access_local_addr() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let local_addr = listener.local_addr().unwrap();
            let sock = TcpTransport::new(local_addr);

            // TcpTransport is Send; it can be moved to another thread.
            let handle = tokio::spawn(async move { sock.local_addr() });
            assert_eq!(handle.await.unwrap(), local_addr);
        });
    }
}
