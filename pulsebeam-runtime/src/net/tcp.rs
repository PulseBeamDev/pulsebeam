use super::{RecvPacketBatch, SendPacketBatch};
use crate::net::Transport;
use bytes::{Buf, BufMut, BytesMut};
use pulsebeam_core::net::TcpStream;
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

/// Maximum RFC 4571 payload length accepted on a TCP passive connection.
///
/// Each RFC 4571 frame wraps exactly one UDP-equivalent datagram (STUN, DTLS record,
/// SRTP/SRTCP packet).  DTLS fragments its handshake messages to the PMTU when running
/// over ICE-TCP, so every record stays within the Ethernet MTU.  Frames larger than 1 500
/// bytes indicate a misbehaving or malicious peer and are rejected immediately.
const MAX_FRAME_SIZE: usize = 1_500;
const MAX_CONNECTIONS: usize = 10_000;
const MAX_CONNS_PER_IP: usize = 20;
/// Maximum bytes the reassembly buffer may hold for a single connection.
///
/// Because decoding is interleaved with each read, `recv_buf` should only ever
/// contain one partial RFC 4571 frame at a time (previous complete frames are
/// flushed before the next read).  The limit therefore only needs to be one
/// max-frame plus the 2-byte length header and a small slack.
const MAX_RECV_BUF: usize = MAX_FRAME_SIZE + 2 + 64;
/// Maximum bytes allowed in the per-connection write-pending buffer.
/// Peers that stop reading will be disconnected once this headroom is consumed.
const MAX_SEND_BUF_PER_CONN: usize = 256 * 1024;
/// How long a connection may be idle (no data received) before it is reaped.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-connection state — owned exclusively by the shard, no sharing across threads.
struct TcpConn {
    stream: TcpStream,
    /// Reassembly buffer for RFC 4571 framing on the read path.
    recv_buf: BytesMut,
    /// RFC 4571-framed bytes waiting to be flushed to the kernel on the write path.
    send_buf: BytesMut,
    /// Wall-clock time of the last successful read. Used for idle-timeout enforcement.
    last_activity: Instant,
}

/// TCP transport for a single shard — **passive (accept-only) per RFC 6544**.
///
/// The SFU only advertises a TCP *passive* ICE candidate.  Connections arrive
/// via the controller's `TcpListener`; the shard is handed the accepted stream
/// through `add_connection`.  This transport never calls `TcpStream::connect`
/// or initiates any outbound connection.
///
/// Designed for thread-per-core: entirely `!Sync`, no `Arc`-shared state, no spawned tasks,
/// no cross-thread channels.  All I/O is driven inline from the shard's event loop.
pub struct TcpTransport {
    local_addr: SocketAddr,
    conns: HashMap<SocketAddr, TcpConn>,
    ip_counts: HashMap<IpAddr, usize>,
}

impl TcpTransport {
    pub fn new(local_addr: SocketAddr) -> Self {
        Self {
            local_addr,
            conns: HashMap::new(),
            ip_counts: HashMap::new(),
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

    /// Accept a new TCP stream that has already been accepted by the controller.
    ///
    /// `initial_payload` is the decoded payload of the first RFC 4571 frame that
    /// the controller already read to extract the ICE ufrag for routing.  It is
    /// re-enqueued so that `try_recv_batch` delivers it on the next call.
    pub fn add_connection(
        &mut self,
        stream: TcpStream,
        peer_addr: SocketAddr,
        initial_payload: Vec<u8>,
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

        // Pre-populate recv_buf with the already-decoded first frame so that
        // try_recv_batch delivers it on the next call without re-reading the wire.
        let mut recv_buf = BytesMut::with_capacity(MAX_FRAME_SIZE + 2);
        if !initial_payload.is_empty() && initial_payload.len() <= MAX_FRAME_SIZE {
            recv_buf.put_u16(initial_payload.len() as u16);
            recv_buf.put_slice(&initial_payload);
        }

        self.conns.insert(
            peer_addr,
            TcpConn {
                stream,
                recv_buf,
                send_buf: BytesMut::new(),
                last_activity: Instant::now(),
            },
        );
        tracing::debug!(%peer_addr, "TCP connection added to shard");
        Ok(())
    }

    pub fn close_peer(&mut self, peer_addr: &SocketAddr) {
        self.remove_conn(peer_addr);
    }

    fn remove_conn(&mut self, peer_addr: &SocketAddr) {
        if self.conns.remove(peer_addr).is_some() {
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

    /// Returns a future that resolves when at least one connection has data to read.
    ///
    /// Parks forever when there are no connections, letting the caller's `select!` fall
    /// through to other branches (e.g. UDP).  Allocation in this path only occurs while
    /// the shard is idle — the hot UDP forwarding path never reaches it.
    pub async fn readable(&mut self) -> io::Result<()> {
        if self.conns.is_empty() {
            // No TCP connections: park so the select can use its other arms.
            return std::future::pending().await;
        }
        // Race every connection: the first readable one wins.  Boxing is bounded by the
        // number of active TCP connections (typically very small per shard).
        let futures: Vec<_> = self
            .conns
            .values_mut()
            .map(|c| Box::pin(c.stream.readable()))
            .collect();
        futures::future::select_all(futures).await.0
    }

    /// Drain all available TCP data into `out` without blocking.
    ///
    /// Returns `WouldBlock` when nothing was received.
    ///
    /// # Passive-only invariant
    ///
    /// `TcpTransport` only holds connections that were *accepted* by the
    /// controller (TCP passive role per RFC 6544).  It never initiates outgoing
    /// connections.  Decoding is interleaved with each kernel read so that
    /// `recv_buf` never accumulates more than one partial RFC 4571 frame,
    /// preventing legitimate DTLS/SRTP traffic from triggering the overflow guard.
    pub fn try_recv_batch(&mut self, out: &mut Vec<RecvPacketBatch>) -> io::Result<usize> {
        let local_addr = self.local_addr;
        let now = Instant::now();
        let mut to_remove: Vec<SocketAddr> = Vec::new();

        'conn: for (&peer_addr, conn) in &mut self.conns {
            // Idle-timeout: drop connections that have sent nothing recently.
            if now.duration_since(conn.last_activity) > READ_TIMEOUT {
                tracing::warn!(%peer_addr, "TCP connection idle timeout, dropping");
                to_remove.push(peer_addr);
                continue;
            }

            // Interleaved read-decode loop.
            //
            // We decode ALL complete frames that are already in recv_buf BEFORE
            // issuing the next try_read.  This means recv_buf at the start of
            // each try_read contains only a single partial frame (or nothing),
            // keeping the buffer small regardless of how many frames the kernel
            // has queued up.  The pattern that previously triggered the overflow
            // (reading N complete frames in one try_read call before the decode
            // loop could consume them) no longer occurs.
            loop {
                // 1. Decode all complete frames currently in the buffer.
                loop {
                    if conn.recv_buf.len() < 2 {
                        break;
                    }
                    let len =
                        u16::from_be_bytes([conn.recv_buf[0], conn.recv_buf[1]]) as usize;
                    if len == 0 || len > MAX_FRAME_SIZE {
                        tracing::warn!(
                            %peer_addr,
                            len,
                            "Invalid TCP frame length, dropping connection"
                        );
                        to_remove.push(peer_addr);
                        continue 'conn;
                    }
                    if conn.recv_buf.len() < 2 + len {
                        break; // partial frame — wait for more data
                    }
                    conn.recv_buf.advance(2);
                    let data = conn.recv_buf.split_to(len);
                    out.push(RecvPacketBatch {
                        src: peer_addr,
                        dst: local_addr,
                        buf: data.to_vec(),
                        offset: 0,
                        stride: len,
                        len,
                        transport: Transport::Tcp,
                    });
                }

                // 2. Overflow guard: recv_buf now holds at most one partial frame.
                //    Growth beyond MAX_RECV_BUF means the declared frame length
                //    exceeds MAX_FRAME_SIZE or the peer is sending garbage.
                if conn.recv_buf.len() > MAX_RECV_BUF {
                    tracing::warn!(
                        %peer_addr,
                        buf_len = conn.recv_buf.len(),
                        "TCP recv_buf overflow (malformed framing), dropping connection"
                    );
                    to_remove.push(peer_addr);
                    continue 'conn;
                }

                // 3. Read the next chunk from the kernel.
                let mut tmp = [0u8; 4096];
                match conn.stream.try_read(&mut tmp) {
                    Ok(0) => {
                        to_remove.push(peer_addr);
                        continue 'conn;
                    }
                    Ok(n) => {
                        conn.recv_buf.put_slice(&tmp[..n]);
                        conn.last_activity = now;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(%peer_addr, error = ?e, "TCP read error");
                        to_remove.push(peer_addr);
                        continue 'conn;
                    }
                }
            }
        }

        for addr in to_remove {
            self.remove_conn(&addr);
        }

        if out.is_empty() {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(out.len())
        }
    }

    /// Frame `batch` per RFC 4571, write directly to the peer's stream.
    ///
    /// Returns `Ok(true)` when the send buffer is fully flushed to the kernel,
    /// `Ok(false)` under send-buffer back-pressure, and `Ok(true)` when the peer
    /// has gone (nothing left to do).
    pub fn try_send_batch(&mut self, batch: &SendPacketBatch) -> io::Result<bool> {
        let Some(conn) = self.conns.get_mut(&batch.dst) else {
            return Ok(true); // peer gone — treat as sent
        };

        // Append RFC 4571-framed segments to the per-connection write buffer.
        let mut offset = 0;
        while offset < batch.buf.len() {
            let end = (offset + batch.segment_size).min(batch.buf.len());
            let seg = &batch.buf[offset..end];
            conn.send_buf.put_u16(seg.len() as u16);
            conn.send_buf.put_slice(seg);
            offset = end;
        }

        // Slow-reader guard: drop connections whose write buffer has grown too large.
        // This prevents a misbehaving or intentionally slow peer from exhausting memory.
        let should_drop = conn.send_buf.len() > MAX_SEND_BUF_PER_CONN;

        // Flush as much as the kernel will accept right now.
        let mut remove = should_drop;
        if !should_drop {
            while !conn.send_buf.is_empty() {
                match conn.stream.try_write(&conn.send_buf) {
                    Ok(0) => break,
                    Ok(n) => conn.send_buf.advance(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(peer_addr = %batch.dst, error = ?e, "TCP write error");
                        remove = true;
                        break;
                    }
                }
            }
        }
        // NLL: last use of `conn` is above; the borrow ends here.
        let fully_flushed = !remove && conn.send_buf.is_empty();

        if remove {
            tracing::warn!(
                peer_addr = %batch.dst,
                "dropping TCP connection (write error or send_buf overflow)"
            );
            self.remove_conn(&batch.dst);
            return Ok(true);
        }
        Ok(fully_flushed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::net::TcpListener;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Connect a client to `listener`, accept the server-side stream, return both.
    async fn make_pair(listener: &TcpListener) -> (tokio::net::TcpStream, TcpStream, SocketAddr) {
        let server_addr = listener.local_addr().unwrap();
        let (cli, srv) = tokio::join!(
            tokio::net::TcpStream::connect(server_addr),
            listener.accept()
        );
        let client = cli.unwrap();
        let (server_stream, peer_addr) = srv.unwrap();
        (client, server_stream, peer_addr)
    }

    #[tokio::test]
    async fn test_tcp_rfc_compliance() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mut sock = TcpTransport::new(server_addr);

        let (mut client, server_stream, peer_addr) = make_pair(&listener).await;
        client.set_nodelay(true).unwrap();
        sock.add_connection(server_stream, peer_addr, vec![]).unwrap();

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
    }

    /// Verifies that an initial_payload passed to add_connection is delivered on
    /// the next try_recv_batch call — this is the path used when the controller
    /// pre-reads the first STUN frame for ufrag-based routing.
    #[tokio::test]
    async fn test_initial_payload_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mut sock = TcpTransport::new(server_addr);

        let (_client, server_stream, peer_addr) = make_pair(&listener).await;
        let payload = b"stun-request-bytes".to_vec();
        sock.add_connection(server_stream, peer_addr, payload.clone())
            .unwrap();

        // Must be immediately available — no bytes sent on the wire yet.
        let mut out = Vec::new();
        let _ = sock.try_recv_batch(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0].buf[..], payload.as_slice());
    }

    /// A peer that claims a frame length > MAX_FRAME_SIZE should be disconnected.
    /// We use 0xFFFF (65535) as the length prefix which always exceeds MAX_FRAME_SIZE.
    #[tokio::test]
    async fn test_recv_buf_overflow_drops_connection() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mut sock = TcpTransport::new(server_addr);

        let (mut client, server_stream, peer_addr) = make_pair(&listener).await;
        sock.add_connection(server_stream, peer_addr, vec![]).unwrap();

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
        assert_eq!(sock.active_connections(), 0, "malicious connection should be dropped");
    }

    /// A peer that stops reading should be dropped once MAX_SEND_BUF_PER_CONN is hit.
    #[tokio::test]
    async fn test_send_buf_overflow_drops_connection() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mut sock = TcpTransport::new(server_addr);

        let (client, server_stream, peer_addr) = make_pair(&listener).await;
        sock.add_connection(server_stream, peer_addr, vec![]).unwrap();

        // Stop reading from the client side so the kernel TX buffer fills up.
        drop(client);

        // Flood the send path with MAX_SEND_BUF_PER_CONN + 1 bytes across many calls.
        let payload = vec![0u8; MAX_FRAME_SIZE];
        let segments_needed = (MAX_SEND_BUF_PER_CONN / (MAX_FRAME_SIZE + 2)) + 2;
        for _ in 0..segments_needed {
            let batch = SendPacketBatch {
                dst: peer_addr,
                buf: &payload,
                segment_size: MAX_FRAME_SIZE,
            };
            let _ = sock.try_send_batch(&batch);
            if sock.active_connections() == 0 {
                break;
            }
        }
        assert_eq!(
            sock.active_connections(),
            0,
            "slow-reader connection should be dropped after send_buf overflow"
        );
    }

    #[tokio::test]
    async fn test_tcp_multi_peer_isolation() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mut sock = TcpTransport::new(server_addr);

        let (_c1, s1, p1) = make_pair(&listener).await;
        sock.add_connection(s1, p1, vec![]).unwrap();

        let (_c2, s2, p2) = make_pair(&listener).await;
        sock.add_connection(s2, p2, vec![]).unwrap();

        assert_eq!(sock.active_connections(), 2);
    }

    #[tokio::test]
    async fn test_access_local_addr() {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let local_addr = listener.local_addr().unwrap();
        let sock = TcpTransport::new(local_addr);

        // TcpTransport is Send; it can be moved to another thread.
        let handle = tokio::spawn(async move { sock.local_addr() });
        assert_eq!(handle.await.unwrap(), local_addr);
    }
}
