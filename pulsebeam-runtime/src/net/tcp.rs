use super::{RecvPacketBatch, SendPacketBatch};
use crate::mailbox;
use crate::net::Transport;
use bytes::{Buf, BufMut, BytesMut};
use pulsebeam_core::net::{TcpReadHalf, TcpStream, TcpWriteHalf, split_tcp};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Maximum RFC 4571 payload length accepted during the initial frame peek.
/// Mirrors `MAX_FRAME_SIZE` below — kept as a separate constant so the method
/// can be used before the rest of the transport is constructed.
const MAX_PEEK_FRAME_SIZE: usize = 1_500;

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

            let mut raw = Vec::with_capacity(2 + len);
            raw.extend_from_slice(&header);
            raw.extend_from_slice(&payload);

            Ok((BufferedTcpStream::with_buffered(stream, raw), payload))
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP first-frame read timed out"))?
    }
}

/// Maximum RFC 4571 payload length accepted on a TCP passive connection.
const MAX_FRAME_SIZE: usize = 1_500;
const MAX_CONNECTIONS: usize = 10_000;
const MAX_CONNS_PER_IP: usize = 20;
/// Overflow guard: `recv_buf` holds at most one partial RFC 4571 frame.
const MAX_RECV_BUF: usize = MAX_FRAME_SIZE + 2 + 64;
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
            if recv_buf.len() < 2 {
                break;
            }
            let len = u16::from_be_bytes([recv_buf[0], recv_buf[1]]) as usize;
            if len == 0 || len > MAX_FRAME_SIZE {
                tracing::warn!(%peer_addr, len, "Invalid TCP frame length, closing connection");
                break 'outer;
            }
            if recv_buf.len() < 2 + len {
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
            Ok(n) => recv_buf.put_slice(&tmp[..n]),
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
    drop_count: usize,
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
            drop_count: 0,
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
        *count += 1;

        if let Err(e) = stream.set_nodelay(true) {
            *self.ip_counts.get_mut(&peer_ip).unwrap() -= 1;
            return Err(e);
        }

        let (raw, mut pending) = stream.into_parts();

        // Synchronously decode any pre-buffered bytes and push to the channel.
        // This keeps the invariant that frames available at add_connection time
        // are immediately visible to try_recv_batch, without needing the task
        // to execute first.
        if !pending.is_empty() {
            let mut pos = BytesMut::from(pending.as_ref());
            pending.clear();
            loop {
                if pos.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([pos[0], pos[1]]) as usize;
                if len == 0 || len > MAX_FRAME_SIZE || pos.len() < 2 + len {
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
        }

        let (read, write) = split_tcp(raw);
        let cancel = CancellationToken::new();

        // Spawn the read task on the same thread.  It owns `read` and forwards
        // decoded frames through `event_tx` until cancelled or the stream closes.
        tokio::task::spawn(tcp_read_task(
            peer_addr,
            read,
            pending, // empty at this point
            self.event_tx.clone(),
            cancel.clone(),
        ));

        self.conns.insert(peer_addr, TcpConn { write, cancel });
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
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => self.handle_event(event, out, local_addr),
                Err(_) => break,
            }
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
    /// Frame `batch` per RFC 4571 and write to the peer's stream.
    ///
    /// **Lossy**: if the kernel send buffer is full (`WouldBlock`) the batch is
    /// dropped rather than queued.  This keeps memory bounded and ensures the
    /// caller's batch queue is always drained to empty on every tick.
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<bool> {
        debug_assert!(batch.segment_size != 0);
        let Some(conn) = self.conns.get_mut(&batch.dst) else {
            return Ok(true); // peer gone — treat as sent
        };

        // Frame all segments into a local buffer (not stored on the connection).
        let mut buf = BytesMut::new();
        let mut offset = 0;
        while offset < batch.buf.len() {
            let end = (offset + batch.segment_size).min(batch.buf.len());
            let seg = &batch.buf[offset..end];
            buf.put_u16(seg.len() as u16);
            buf.put_slice(seg);
            offset = end;
        }

        // Drain to kernel; whatever doesn't fit is dropped (lossy).
        while !buf.is_empty() {
            match conn.write.try_write(&buf) {
                Ok(0) => {
                    self.remove_conn(&batch.dst);
                    return Ok(true);
                }
                Ok(n) => buf.advance(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let dropped = count_rfc4571_frames(&buf);
                    if dropped > 0 {
                        // metrics::counter!("tcp_egress_packets_dropped_total").increment(dropped);
                        if self.drop_count % 100 == 0 {
                            tracing::warn!("udp dropped a packet due to full socket");
                        }
                        self.drop_count += dropped as usize;
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(peer_addr = %batch.dst, error = ?e, "TCP write error");
                    self.remove_conn(&batch.dst);
                    return Ok(true);
                }
            }
        }
        Ok(true)
    }
}

fn count_rfc4571_frames(buf: &[u8]) -> u64 {
    let mut count = 0;
    let mut offset = 0;
    while offset + 2 <= buf.len() {
        let len = u16::from_be_bytes([buf[offset], buf[offset + 1]]) as usize;
        if len == 0 {
            count += 1;
            break;
        }
        let total = 2 + len;
        if offset + total > buf.len() {
            count += 1;
            break;
        }
        count += 1;
        offset += total;
    }
    if offset < buf.len() {
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
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
                frame_bytes.extend_from_slice(&(p.len() as u16).to_be_bytes());
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
            let batch = SendPacketBatch {
                dst: peer_addr,
                buf: payload,
                segment_size: 8,
            };
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
            wire.push((payload.len() >> 8) as u8);
            wire.push(payload.len() as u8);
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

    /// A peer that claims a frame length > MAX_FRAME_SIZE should be disconnected.
    /// We use 0xFFFF (65535) as the length prefix which always exceeds MAX_FRAME_SIZE.
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

            // Send a 2-byte header claiming a 65535-byte payload — always > MAX_FRAME_SIZE.
            // The connection must be dropped the moment we decode the length prefix.
            let junk = vec![0xFF, 0xFF];
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
                "malicious connection should be dropped"
            );
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
                let batch = SendPacketBatch {
                    dst: peer_addr,
                    buf: &payload,
                    segment_size: MAX_FRAME_SIZE,
                };
                assert_eq!(
                    sock.try_send_batch(&batch).unwrap(),
                    true,
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
