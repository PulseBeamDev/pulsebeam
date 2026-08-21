//! RFC 4571 framing over TCP.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`. The
//! length prefix is 16 bits and peer-controlled, so every `offset + len` is an
//! offset a peer chose against a buffer it did not. With `overflow-checks` off
//! in release a wrap would not stop — it would index somewhere else in the
//! stream or report a frame that is not there.

use super::{RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use crate::{mailbox, net::SendPacket};
use bytes::{Buf, BufMut, BytesMut};
use pulsebeam_core::net::{TcpReadHalf, TcpStream, TcpWriteHalf, split_tcp};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
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
}

impl BufferedTcpStream {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            pending: BytesMut::new(),
        }
    }

    /// Wrap `stream` and pre-fill the read buffer with `bytes` — the exact
    /// bytes that were already read from the socket (wire-format, including any
    /// RFC 4571 length prefix).  Subsequent `try_read` calls drain these bytes
    /// first before touching the kernel socket.
    pub fn with_buffered(stream: TcpStream, bytes: Vec<u8>) -> Self {
        Self {
            stream,
            pending: BytesMut::from(bytes.as_slice()),
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
    pub fn into_parts(self) -> (TcpStream, BytesMut) {
        (self.stream, self.pending)
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

            let mut raw = Vec::with_capacity(len.saturating_add(2));
            raw.extend_from_slice(&header);
            raw.extend_from_slice(&payload);

            Ok((BufferedTcpStream::with_buffered(stream, raw), payload))
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
/// Channel capacity for TCP events (frames + close notifications).
const TCP_EVENT_CAPACITY: usize = 8_192;

/// Events produced by per-connection read tasks and consumed by `TcpTransport`.
enum TcpEvent {
    /// A fully-decoded RFC 4571 payload ready to hand to the shard.
    Frame { src: SocketAddr, payload: Vec<u8> },
    /// The read task exited (EOF, error, idle timeout, or cancellation).
    Closed(SocketAddr),
}

/// Per-connection write-side state owned by the shard.
struct TcpConn {
    /// Write half — `try_write` is `&self` so no locking needed.
    write: TcpWriteHalf,
    /// Bytes already framed but not yet accepted by the kernel. A partial TCP
    /// write must be resumed before a later frame can be sent.
    pending: BytesMut,
    /// Cancels the associated read task when the connection is removed.
    cancel: CancellationToken,
}

/// Read task for a single TCP connection.
///
/// Runs on the same thread as the shard (`spawn_local`).  Owns the read half
/// of the stream so the shard's hot-path never touches it.  Decodes RFC 4571
/// frames and forwards them through `tx`.  Exits on EOF, I/O error, idle
/// timeout, or cancellation; always sends `TcpEvent::Closed` before exiting.
async fn tcp_read_task(
    peer_addr: SocketAddr,
    read: TcpReadHalf,
    mut pending: BytesMut,
    tx: mailbox::Sender<TcpEvent>,
    cancel: CancellationToken,
) {
    let mut recv_buf = BytesMut::with_capacity(MAX_FRAME_SIZE + 2);
    // Carry over any bytes already decoded by the controller's first-frame read.
    if !pending.is_empty() {
        recv_buf.extend_from_slice(&pending);
        pending.clear();
    }

    'outer: loop {
        // Decode all complete frames currently in the buffer before waiting.
        loop {
            let Some(len) = frame_len_at(&recv_buf, 0) else {
                break;
            };
            if len == 0 || len > MAX_FRAME_SIZE {
                tracing::warn!(%peer_addr, len, "Invalid TCP frame length, closing connection");
                break 'outer;
            }
            if recv_buf.len() < len.saturating_add(2) {
                break; // partial frame
            }
            recv_buf.advance(2);
            let payload = recv_buf.split_to(len).to_vec();
            if tx
                .send(TcpEvent::Frame {
                    src: peer_addr,
                    payload,
                })
                .await
                .is_err()
            {
                return; // transport dropped its receiver
            }
        }

        if recv_buf.len() > MAX_RECV_BUF {
            tracing::warn!(%peer_addr, "TCP recv_buf overflow, closing connection");
            break;
        }

        // Wait for readability, cancellation, or idle timeout.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = tokio::time::timeout(READ_TIMEOUT, read.readable()) => {
                match result {
                    Err(_) => {
                        tracing::warn!(%peer_addr, "TCP connection idle timeout");
                        break;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(%peer_addr, error = ?e, "TCP readable error");
                        break;
                    }
                    Ok(Ok(())) => {}
                }
            }
        }

        // Non-blocking read from the kernel.
        let mut tmp = [0u8; 4096];
        match read.try_read(&mut tmp) {
            Ok(0) => break, // EOF
            Ok(n) => match tmp.get(..n) {
                Some(chunk) => recv_buf.put_slice(chunk),
                None => break,
            },
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                tracing::warn!(%peer_addr, error = ?e, "TCP read error");
                break;
            }
        }
    }

    let _ = tx.send(TcpEvent::Closed(peer_addr)).await;
}

/// TCP transport for a single shard — **passive (accept-only) per RFC 6544**.
///
/// ## Thread-per-core design
///
/// One `spawn_local` task is created per accepted connection.  Each task owns
/// the read half of the stream and runs on the **same OS thread** as the shard
/// (Tokio `LocalSet`).  The shard's hot-path only touches a single
/// `mpsc::Receiver` — O(1) regardless of how many streams are open — so TCP
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
    ) -> io::Result<()> {
        if self.conns.len() >= MAX_CONNECTIONS {
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

        let (raw, mut pending) = stream.into_parts();

        // Synchronously decode any pre-buffered bytes and push to the channel.
        // This keeps the invariant that frames available at add_connection time
        // are immediately visible to try_recv_batch, without needing the task
        // to execute first.
        if !pending.is_empty() {
            let mut pos = pending;
            loop {
                let Some(len) = frame_len_at(&pos, 0) else {
                    break;
                };
                if len == 0 || len > MAX_FRAME_SIZE || pos.len() < len.saturating_add(2) {
                    break;
                }
                pos.advance(2);
                let payload = pos.split_to(len).to_vec();
                // try_send never blocks; the channel has capacity for this.
                let _ = self.event_tx.try_send(TcpEvent::Frame {
                    src: peer_addr,
                    payload,
                });
            }
            pending = pos;
        }

        let (read, write) = split_tcp(raw);
        let cancel = CancellationToken::new();

        // Spawn the read task on the same thread.  It owns `read` and forwards
        // decoded frames through `event_tx` until cancelled or the stream closes.
        tokio::task::spawn(tcp_read_task(
            peer_addr,
            read,
            pending,
            self.event_tx.clone(),
            cancel.clone(),
        ));

        self.conns.insert(
            peer_addr,
            TcpConn {
                write,
                pending: BytesMut::new(),
                cancel,
            },
        );
        tracing::debug!(%peer_addr, "TCP connection added to shard");
        Ok(())
    }

    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        self.remove_conn(peer_addr);
    }

    fn remove_conn(&mut self, peer_addr: &SocketAddr) {
        if let Some(conn) = self.conns.remove(peer_addr) {
            // Signal the read task to exit.  It will send TcpEvent::Closed which
            // try_recv_batch will see and call remove_conn again — that is a no-op
            // because the entry is already gone.
            conn.cancel.cancel();
            let ip = peer_addr.ip();
            if let Some(c) = self.ip_counts.get_mut(&ip) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.ip_counts.remove(&ip);
                }
            }
            tracing::info!(%peer_addr, "TCP connection removed from shard");
        }
    }

    /// Resolves when at least one TCP event (frame or close notification) is
    /// available.  Parks forever when no events are queued, letting the shard's
    /// `select!` fall through to other arms (e.g. UDP).
    ///
    /// O(1): waits on a single channel regardless of the number of connections.
    pub async fn readable(&mut self) -> io::Result<()> {
        if self.peeked.is_some() {
            return Ok(()); // previous readable() result not yet consumed
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

    /// Drain all immediately available TCP events into `out`.
    ///
    /// Returns `WouldBlock` when no frames were produced (closed-connection
    /// events are processed but produce no output frames).
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        let local_addr = self.local_addr;

        // Process the event stashed by readable() first.
        if let Some(event) = self.peeked.take() {
            self.handle_event(event, out, local_addr);
        }

        // Drain any further events that arrived since readable() was called.
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_event(event, out, local_addr);
        }

        if out.is_empty() {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(out.len())
        }
    }

    fn handle_event(
        &mut self,
        event: TcpEvent,
        out: &mut Vec<RecvPacketBatch>,
        local_addr: SocketAddr,
    ) {
        match event {
            TcpEvent::Frame { src, payload } => {
                // Discard stale events from already-removed connections.
                if !self.conns.contains_key(&src) {
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
                    offset: 0,
                });
            }
            TcpEvent::Closed(src) => {
                self.remove_conn(&src);
            }
        }
    }

    /// Frame `batch` per RFC 4571 and write to the peer's stream.
    ///
    /// **Lossy**: a new batch is dropped when a previous partial frame is still
    /// waiting or the kernel send buffer is full. A partial frame itself is
    /// retained so later frames cannot corrupt RFC 4571 alignment.
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<usize> {
        for group in batch.packets {
            self.try_send_group(group)?;
        }
        Ok(batch.packets.len())
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

            while !conn.pending.is_empty() {
                match conn.write.try_write(&conn.pending) {
                    Ok(0) => return Ok(false),
                    Ok(n) => {
                        debug_assert!(n <= conn.pending.len());
                        if n > conn.pending.len() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "TCP write returned more bytes than supplied",
                            ));
                        }
                        conn.pending.advance(n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                    Err(e) => return Err(e),
                }
            }

            if buf.len() > MAX_TCP_WRITE_BUF {
                tracing::debug!(
                    peer_addr = %batch.dst,
                    bytes = buf.len(),
                    "TCP batch exceeds bounded write buffer"
                );
                return Ok(true);
            }

            while !buf.is_empty() {
                match conn.write.try_write(&buf) {
                    Ok(0) => return Ok(false),
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
            Ok(true)
        })();

        match outcome {
            Ok(true) => {}
            Ok(false) => self.remove_conn(&batch.dst),
            Err(error) => {
                tracing::warn!(peer_addr = %batch.dst, %error, "TCP write error");
                self.remove_conn(&batch.dst);
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
        crate::testing::test_host_ip("192.168.250.12")
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
            sock.add_connection(BufferedTcpStream::new(server_stream), peer_addr)
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

            sock.add_connection(
                BufferedTcpStream::with_buffered(server_stream, wire),
                peer_addr,
            )
            .unwrap();

            // Must be immediately available — no bytes sent on the wire yet.
            let mut out = Vec::new();
            let _ = sock.try_recv_batch(&mut out);
            assert_eq!(out.len(), 1);
            assert_eq!(&out[0].buf[..], payload.as_slice());
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

    /// Verify that try_send_batch is lossy: flooding a peer that has stopped
    /// reading never panics and always returns Ok(true) (packets are dropped,
    /// not buffered), keeping memory bounded.
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

            // Flood: every call must return Ok(true) (lossy drop), never buffering.
            let payload = vec![0u8; MAX_FRAME_SIZE];
            for _ in 0..200 {
                let packets = [SendPacket {
                    dst: peer_addr,
                    buf: &payload,
                    segment_size: MAX_FRAME_SIZE,
                }];
                let batch = SendPacketBatch { packets: &packets };
                assert_eq!(
                    sock.try_send_batch(&batch).unwrap(),
                    1,
                    "try_send_batch must always return Ok(true) under back-pressure"
                );
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
