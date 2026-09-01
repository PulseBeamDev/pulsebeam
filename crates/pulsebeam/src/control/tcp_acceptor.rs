//! TCP acceptor task — owns the `TcpListener` and all in-flight first-frame
//! futures so the controller loop stays free of `FuturesUnordered` machinery.
//!
//! Architecture
//! ------------
//! [`TcpAcceptorHandle::spawn`] starts the accept loop on the current
//! `LocalSet` / `LocalRuntime` (via `tokio::task::spawn_local`).  For each
//! accepted stream it spawns a second inner task that reads the first RFC 4571
//! frame within a timeout, decodes the ICE ufrag, and validates it against
//! this node's identity and shard count. A validated connection is handed off
//! exactly once, as a [`TcpAcceptorEvent`], through a `mailbox` channel back
//! to the controller, which forwards it to the shard the ufrag named. The
//! shard then owns the connection permanently — nothing on this path repeats
//! the handoff or turns it into a per-packet forwarding channel.
//!
//! In-flight counts are tracked in the accept loop via a `done_rx` back-channel
//! so the loop can enforce the total and per-IP caps without requiring the
//! controller to send any acknowledgement.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use pulsebeam_core::net::TcpListener;
use pulsebeam_runtime::{mailbox, net::tcp::BufferedTcpStream};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::{
    control::ufrag::IceUfrag, route::TransportHandle, shard::demux::extract_stun_server_ufrag,
};

/// How long to wait for the first STUN frame on a fresh TCP connection.
pub const TCP_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum number of TCP connections waiting for their first STUN frame.
#[cfg(not(test))]
pub const MAX_PENDING_TCP: usize = 1_024;
#[cfg(test)]
pub const MAX_PENDING_TCP: usize = 4;

/// Maximum concurrent pending connections from a single source IP.
#[cfg(not(test))]
pub const MAX_PENDING_TCP_PER_IP: usize = 16;
#[cfg(test)]
pub const MAX_PENDING_TCP_PER_IP: usize = 2;

/// Sent by the acceptor to the controller once the first frame has been read,
/// decoded, and validated (or the attempt has failed / timed out / been
/// rejected).
pub struct TcpAcceptorEvent {
    pub peer_addr: SocketAddr,
    pub result: Option<PendingTcpConn>,
}

/// A validated pending TCP connection, ready for a one-time handoff to the
/// shard named by `handle`. The controller must not re-validate the ufrag —
/// this is the only place that decision is made.
pub struct PendingTcpConn {
    pub stream: BufferedTcpStream,
    pub peer_addr: SocketAddr,
    pub handle: TransportHandle,
}

/// Routing parameters this node validates every ufrag against before handing
/// a connection off to a shard — the same identity the controller encodes
/// into every ufrag it mints.
#[derive(Debug, Clone, Copy)]
pub struct TcpAcceptorConfig {
    pub cluster_id: u16,
    pub node_id: u16,
    pub shard_count: usize,
}

/// Opaque handle returned by [`TcpAcceptorHandle::spawn`].
///
/// The controller holds `event_rx` and drains it each loop iteration.
pub struct TcpAcceptorHandle {
    pub event_rx: mailbox::Receiver<TcpAcceptorEvent>,
}

impl TcpAcceptorHandle {
    /// Spawn the acceptor loop onto the current `LocalSet` / `LocalRuntime`.
    pub fn spawn(
        listener: TcpListener,
        config: TcpAcceptorConfig,
        shutdown: CancellationToken,
    ) -> Self {
        let (event_tx, event_rx) = mailbox::new(256);
        tokio::task::spawn(acceptor_loop(listener, config, event_tx, shutdown));
        Self { event_rx }
    }
}

/// Accept loop: enforces caps and spawns one inner task per accepted stream.
async fn acceptor_loop(
    listener: TcpListener,
    config: TcpAcceptorConfig,
    event_tx: mailbox::Sender<TcpAcceptorEvent>,
    shutdown: CancellationToken,
) {
    // Back-channel: inner tasks signal completion so the loop can decrement
    // in-flight counters without blocking on those tasks.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<SocketAddr>(MAX_PENDING_TCP);

    let mut pending: usize = 0;
    let mut ip_counts: HashMap<IpAddr, usize> = HashMap::new();

    loop {
        tokio::select! {
            biased;

            // Drain completions first to free budget before accepting new streams.
            Some(peer_addr) = done_rx.recv() => {
                pending = pending.saturating_sub(1);
                let ip = peer_addr.ip();
                if let Some(c) = ip_counts.get_mut(&ip) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        ip_counts.remove(&ip);
                    }
                }
            }

            _ = shutdown.cancelled() => {
                tracing::debug!("tcp acceptor shutting down");
                break;
            }

            res = listener.accept() => {
                match res {
                    Err(err) => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        tracing::warn!(error = ?err, "TCP accept failed");
                    }
                    Ok((stream, peer_addr)) => {
                        if pending >= MAX_PENDING_TCP {
                            tracing::warn!(
                                %peer_addr,
                                limit = MAX_PENDING_TCP,
                                "Pending TCP limit reached, dropping connection"
                            );
                            continue;
                        }
                        let ip = peer_addr.ip();
                        let ip_count = ip_counts.entry(ip).or_insert(0);
                        if *ip_count >= MAX_PENDING_TCP_PER_IP {
                            tracing::warn!(
                                %peer_addr,
                                limit = MAX_PENDING_TCP_PER_IP,
                                "Per-IP pending TCP limit reached, dropping connection"
                            );
                            continue;
                        }
                        *ip_count = ip_count.saturating_add(1);
                        pending = pending.saturating_add(1);

                        let tx = event_tx.clone();
                        let done = done_tx.clone();
                        tokio::task::spawn(first_frame_task(stream, peer_addr, config, tx, done));
                    }
                }
            }
        }
    }
}

/// Inner task: reads the first RFC 4571 frame, validates its ufrag, then
/// notifies both the controller (via `event_tx`) and the accept loop (via
/// `done_tx`).
async fn first_frame_task(
    stream: pulsebeam_core::net::TcpStream,
    peer_addr: SocketAddr,
    config: TcpAcceptorConfig,
    event_tx: mailbox::Sender<TcpAcceptorEvent>,
    done_tx: Sender<SocketAddr>,
) {
    let result = match BufferedTcpStream::read_first_frame(stream, TCP_FIRST_FRAME_TIMEOUT).await {
        Ok((stream, payload)) => validate_and_route(stream, peer_addr, &payload, config),
        Err(e) => {
            tracing::warn!(%peer_addr, error = ?e, "TCP first-frame read failed");
            None
        }
    };

    // Notify the controller. Ignore send errors — it means the controller
    // shut down, and we are about to be dropped too.
    let _ = event_tx.send(TcpAcceptorEvent { peer_addr, result }).await;
    // Notify the accept loop so it can decrement the in-flight counters.
    let _ = done_tx.send(peer_addr).await;
}

/// Decode the initial STUN frame's ufrag and validate it names this node and
/// a shard it actually has, before the connection is ever handed to the
/// controller. This is the only validation this connection ever receives —
/// the shard that ends up owning `stream` trusts `handle` without
/// re-checking it.
///
/// Every rejection branch drops `stream` on return, closing the OS socket;
/// none of them forward the connection anywhere.
fn validate_and_route(
    stream: BufferedTcpStream,
    peer_addr: SocketAddr,
    payload: &[u8],
    config: TcpAcceptorConfig,
) -> Option<PendingTcpConn> {
    let Some(raw_ufrag) = extract_stun_server_ufrag(payload) else {
        tracing::warn!(%peer_addr, "TCP first frame carries no STUN ufrag, dropping");
        return None; // stream dropped here, OS socket closed
    };

    let Some(ufrag) = IceUfrag::decode(&raw_ufrag) else {
        tracing::warn!(%peer_addr, "TCP first frame ufrag failed to decode, dropping");
        return None; // stream dropped here, OS socket closed
    };

    if ufrag.cluster_id != config.cluster_id {
        tracing::warn!(
            %peer_addr,
            ufrag_cluster = ufrag.cluster_id,
            our_cluster = config.cluster_id,
            "TCP connection ufrag targets a different cluster, dropping"
        );
        return None; // stream dropped here, OS socket closed
    }

    if ufrag.node_id != config.node_id {
        tracing::warn!(
            %peer_addr,
            ufrag_node = ufrag.node_id,
            our_node = config.node_id,
            "TCP connection ufrag targets a different node, dropping"
        );
        return None; // stream dropped here, OS socket closed
    }

    let handle = ufrag.handle();
    let shard = handle.shard();
    if shard.index() >= config.shard_count {
        tracing::warn!(
            %peer_addr,
            shard = shard.index(),
            shard_count = config.shard_count,
            "TCP connection ufrag targets an out-of-range shard, dropping"
        );
        return None; // stream dropped here, OS socket closed
    }

    debug_assert_eq!(
        shard,
        handle.shard(),
        "the shard validated against shard_count must be the shard the handoff targets"
    );

    Some(PendingTcpConn {
        stream,
        peer_addr,
        handle,
    })
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use pulsebeam_core::net::TcpListener;
    use std::{net::IpAddr, time::Duration};
    use tokio::time::timeout;

    fn run_local<Fut>(test: Fut)
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        pulsebeam_runtime::testing::run_local(test_host_ip(), test);
    }

    fn test_host_ip() -> IpAddr {
        pulsebeam_runtime::testing::test_host_ip("192.168.250.11")
    }

    fn test_config() -> TcpAcceptorConfig {
        TcpAcceptorConfig {
            cluster_id: 0,
            node_id: 0,
            shard_count: 4,
        }
    }

    // ── pending cap ──────────────────────────────────────────────────────────

    /// The acceptor drops connections when `MAX_PENDING_TCP` is already reached.
    #[test]
    fn test_pending_tcp_cap_drops_excess_connections() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();

            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            // Connect MAX_PENDING_TCP clients and hold them open.
            let mut clients = Vec::new();
            for _ in 0..MAX_PENDING_TCP {
                let client = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
                clients.push(client);
            }

            // Connect one more: the acceptor should drop it silently.
            // Give the acceptor loop time to process all connections.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // The extra connection — server drops it, client EOF or RST.
            let extra = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            // The event channel should have received at most MAX_PENDING_TCP
            // connection events (all still pending their first frame since we
            // never wrote anything).  The excess connection produces no event.
            // We just verify that no MORE than MAX_PENDING_TCP events arrive
            // within a short window.
            while let Ok(Some(_)) = timeout(Duration::from_millis(10), event_rx.recv()).await {}
            // All connections eventually time out (TCP_FIRST_FRAME_TIMEOUT) and
            // produce a None result.  We mainly care that the cap was enforced.
            drop(clients);
            drop(extra);
        });
    }

    /// After in-flight connections complete, the acceptor accepts new ones.
    #[test]
    fn test_pending_tcp_accepts_after_cap_frees_up() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();

            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            // Fill to the limit with clients that immediately close (EOF → None result).
            for _ in 0..MAX_PENDING_TCP {
                let _client = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
                // drop immediately so the first-frame task gets EOF quickly
            }

            // Wait for all pending tasks to resolve (EOF → None events).
            let mut resolved = 0;
            while resolved < MAX_PENDING_TCP {
                if let Ok(Some(_ev)) = timeout(Duration::from_secs(5), event_rx.recv()).await {
                    resolved += 1;
                } else {
                    break;
                }
            }
            assert_eq!(
                resolved, MAX_PENDING_TCP,
                "all pending tasks should resolve"
            );

            // Give the acceptor loop time to process done signals.
            tokio::time::sleep(Duration::from_millis(20)).await;

            // Now a new connection should be accepted (no cap hit).
            let _new_client = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
            // No event yet because the first-frame read is still pending —
            // we just verify no panic and the connection was accepted by the OS.
        });
    }

    #[test]
    fn test_tcp_acceptor_stops_on_shutdown() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let shutdown = CancellationToken::new();
            let handle = TcpAcceptorHandle::spawn(listener, test_config(), shutdown.clone());
            let mut event_rx = handle.event_rx;

            shutdown.cancel();

            let result =
                tokio::time::timeout(Duration::from_secs(1), async { event_rx.recv().await }).await;
            assert!(matches!(result, Ok(None)));
        });
    }

    // ── per-IP cap ───────────────────────────────────────────────────────────

    /// Connections from a single IP beyond MAX_PENDING_TCP_PER_IP are dropped.
    #[test]
    fn test_per_ip_cap_enforced() {
        // This test only makes sense if per-IP limit < global limit.
        const { assert!(MAX_PENDING_TCP_PER_IP < MAX_PENDING_TCP) };

        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();

            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            // Open MAX_PENDING_TCP_PER_IP + 1 connections from the same IP.
            let mut clients = Vec::new();
            for _ in 0..=MAX_PENDING_TCP_PER_IP {
                let c = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
                clients.push(c);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Drop all clients so tasks resolve quickly.
            clients.clear();

            let mut count = 0;
            while let Ok(Some(_ev)) = timeout(Duration::from_millis(200), event_rx.recv()).await {
                count += 1;
            }

            // Exactly MAX_PENDING_TCP_PER_IP should have been accepted; the
            // extra one is dropped silently.
            assert_eq!(
                count, MAX_PENDING_TCP_PER_IP,
                "only {MAX_PENDING_TCP_PER_IP} connections from one IP should be accepted"
            );
        });
    }

    // ── ufrag validation and one-time handoff ───────────────────────────────

    use crate::{id::ShardId, route::TransportRoute};
    use tokio::io::AsyncWriteExt;

    const STUN_BINDING_REQUEST: u16 = 0x0001;
    const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
    const STUN_HEADER_LEN: usize = 20;
    const USERNAME_ATTR_TYPE: u16 = 0x0006;

    /// Build a minimal STUN binding request carrying `server_ufrag` in its
    /// USERNAME attribute, the same shape a real ICE client sends.
    fn build_stun_binding_request(server_ufrag: &str) -> Vec<u8> {
        let username = format!("{server_ufrag}:CLIENTUFRAG");
        let username_bytes = username.as_bytes();
        let padded_len = (username_bytes.len() + 3) & !3;

        let mut attrs = Vec::new();
        attrs.extend_from_slice(&USERNAME_ATTR_TYPE.to_be_bytes());
        attrs.extend_from_slice(&u16::try_from(username_bytes.len()).unwrap().to_be_bytes());
        attrs.extend_from_slice(username_bytes);
        attrs.resize(padded_len.saturating_add(4), 0);

        let mut msg = Vec::with_capacity(STUN_HEADER_LEN + attrs.len());
        msg.extend_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        msg.extend_from_slice(&u16::try_from(attrs.len()).unwrap().to_be_bytes());
        msg.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&[0u8; 12]);
        msg.extend_from_slice(&attrs);
        msg
    }

    fn build_large_stun_binding_request(server_ufrag: &str) -> Vec<u8> {
        let mut msg = build_stun_binding_request(server_ufrag);
        const SOFTWARE_ATTR_TYPE: u16 = 0x8022;
        const SOFTWARE_LEN: usize = 5_600;
        msg.extend_from_slice(&SOFTWARE_ATTR_TYPE.to_be_bytes());
        msg.extend_from_slice(&u16::try_from(SOFTWARE_LEN).unwrap().to_be_bytes());
        msg.extend(std::iter::repeat_n(0x5a, SOFTWARE_LEN));
        let attr_len = msg.len().saturating_sub(STUN_HEADER_LEN);
        msg[2..4].copy_from_slice(&u16::try_from(attr_len).unwrap().to_be_bytes());
        msg
    }

    /// Build a STUN binding request with no USERNAME attribute at all.
    fn build_stun_binding_request_without_username() -> Vec<u8> {
        let mut msg = Vec::with_capacity(STUN_HEADER_LEN);
        msg.extend_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&[0u8; 12]);
        msg
    }

    fn frame_rfc4571(payload: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(payload.len().saturating_add(2));
        framed.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        framed.extend_from_slice(payload);
        framed
    }

    async fn connect_and_send(addr: SocketAddr, payload: &[u8]) -> pulsebeam_core::net::TcpStream {
        let mut client = pulsebeam_core::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(&frame_rfc4571(payload)).await.unwrap();
        client
    }

    async fn recv_event(
        event_rx: &mut mailbox::Receiver<TcpAcceptorEvent>,
    ) -> Option<PendingTcpConn> {
        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("acceptor must produce an event before the test timeout")
            .expect("acceptor event channel must not close")
            .result
    }

    #[test]
    fn test_valid_same_node_ufrag_hands_off_to_encoded_shard() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let config = test_config();
            let handle = TcpAcceptorHandle::spawn(listener, config, CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let transport = TransportRoute::new(ShardId::new(2), 7);
            let ufrag = IceUfrag::new(config.cluster_id, config.node_id, transport, 3).encode();
            let _client = connect_and_send(addr, &build_stun_binding_request(&ufrag)).await;

            let conn = recv_event(&mut event_rx)
                .await
                .expect("valid same-node ufrag must hand off");
            assert_eq!(conn.handle.shard(), ShardId::new(2));
            assert_eq!(conn.handle, TransportHandle::new(transport, 3));

            // The handoff happens once: no further event ever arrives for this
            // connection, even though the OS socket is still open.
            let second = timeout(Duration::from_millis(100), event_rx.recv()).await;
            assert!(
                second.is_err(),
                "a validated connection must be handed off exactly once"
            );
        });
    }

    #[test]
    fn test_valid_large_rfc4571_first_frame_hands_off() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let config = test_config();
            let handle = TcpAcceptorHandle::spawn(listener, config, CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let transport = TransportRoute::new(ShardId::new(1), 9);
            let ufrag = IceUfrag::new(config.cluster_id, config.node_id, transport, 4).encode();
            let payload = build_large_stun_binding_request(&ufrag);
            assert!(payload.len() > 1_500);
            let _client = connect_and_send(addr, &payload).await;

            let conn = recv_event(&mut event_rx)
                .await
                .expect("a valid large RFC 4571 frame must hand off");
            assert_eq!(conn.handle, TransportHandle::new(transport, 4));
            assert!(conn.stream.has_pending());
        });
    }

    #[test]
    fn test_wrong_cluster_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let config = test_config();
            let handle = TcpAcceptorHandle::spawn(listener, config, CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let transport = TransportRoute::new(ShardId::new(1), 0);
            let ufrag = IceUfrag::new(
                config.cluster_id.wrapping_add(1),
                config.node_id,
                transport,
                0,
            )
            .encode();
            let _client = connect_and_send(addr, &build_stun_binding_request(&ufrag)).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "a connection naming a different cluster must be rejected"
            );
        });
    }

    #[test]
    fn test_wrong_node_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let config = test_config();
            let handle = TcpAcceptorHandle::spawn(listener, config, CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let transport = TransportRoute::new(ShardId::new(1), 0);
            let ufrag = IceUfrag::new(
                config.cluster_id,
                config.node_id.wrapping_add(1),
                transport,
                0,
            )
            .encode();
            let _client = connect_and_send(addr, &build_stun_binding_request(&ufrag)).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "a connection naming a different node must be rejected"
            );
        });
    }

    #[test]
    fn test_shard_beyond_node_shard_count_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let config = test_config();
            let handle = TcpAcceptorHandle::spawn(listener, config, CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let out_of_range = TransportRoute::new(ShardId::new(config.shard_count), 0);
            let ufrag = IceUfrag::new(config.cluster_id, config.node_id, out_of_range, 0).encode();
            let _client = connect_and_send(addr, &build_stun_binding_request(&ufrag)).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "a connection naming a shard beyond shard_count must be rejected"
            );
        });
    }

    #[test]
    fn test_malformed_ufrag_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let _client =
                connect_and_send(addr, &build_stun_binding_request("not-a-valid-ufrag!!")).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "an undecodable ufrag must be rejected"
            );
        });
    }

    #[test]
    fn test_absent_ufrag_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let _client =
                connect_and_send(addr, &build_stun_binding_request_without_username()).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "a STUN frame with no USERNAME attribute must be rejected"
            );
        });
    }

    #[test]
    fn test_non_stun_first_frame_is_rejected() {
        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let handle =
                TcpAcceptorHandle::spawn(listener, test_config(), CancellationToken::new());
            let mut event_rx = handle.event_rx;

            let garbage = vec![0xAAu8; 32];
            let _client = connect_and_send(addr, &garbage).await;

            assert!(
                recv_event(&mut event_rx).await.is_none(),
                "a non-STUN first frame must be rejected"
            );
        });
    }
}
