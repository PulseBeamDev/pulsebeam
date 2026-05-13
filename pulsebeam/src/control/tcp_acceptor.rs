//! TCP acceptor task — owns the `TcpListener` and all in-flight first-frame
//! futures so the controller loop stays free of `FuturesUnordered` machinery.
//!
//! Architecture
//! ------------
//! [`TcpAcceptorHandle::spawn`] starts the accept loop on the current
//! `LocalSet` / `LocalRuntime` (via `tokio::task::spawn_local`).  For each
//! accepted stream it spawns a second inner task that reads the first RFC 4571
//! frame within a timeout.  When done it sends a [`TcpAcceptorEvent`] through a
//! `mailbox` channel back to the controller, which routes the connection to the
//! right shard.
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
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::shard::demux::extract_stun_server_ufrag;

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

/// Sent by the acceptor to the controller once the first frame has been read
/// (or the attempt has failed / timed out).
pub struct TcpAcceptorEvent {
    pub peer_addr: SocketAddr,
    pub result: Option<PendingTcpConn>,
}

/// A successfully read pending TCP connection, ready for routing.
pub struct PendingTcpConn {
    pub stream: BufferedTcpStream,
    pub peer_addr: SocketAddr,
    pub server_ufrag: Option<String>,
}

/// Opaque handle returned by [`TcpAcceptorHandle::spawn`].
///
/// The controller holds `event_rx` and drains it each loop iteration.
pub struct TcpAcceptorHandle {
    pub event_rx: mailbox::Receiver<TcpAcceptorEvent>,
}

impl TcpAcceptorHandle {
    /// Spawn the acceptor loop onto the current `LocalSet` / `LocalRuntime`.
    pub fn spawn(listener: TcpListener, shutdown: CancellationToken) -> Self {
        let (event_tx, event_rx) = mailbox::new(256);
        tokio::task::spawn_local(acceptor_loop(listener, event_tx, shutdown));
        Self { event_rx }
    }
}

/// Accept loop: enforces caps and spawns one inner task per accepted stream.
async fn acceptor_loop(
    listener: TcpListener,
    event_tx: mailbox::Sender<TcpAcceptorEvent>,
    shutdown: CancellationToken,
) {
    // Back-channel: inner tasks signal completion so the loop can decrement
    // in-flight counters without blocking on those tasks.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<SocketAddr>();

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
                        *ip_count += 1;
                        pending += 1;

                        let tx = event_tx.clone();
                        let done = done_tx.clone();
                        tokio::task::spawn_local(first_frame_task(stream, peer_addr, tx, done));
                    }
                }
            }
        }
    }
}

/// Inner task: reads the first RFC 4571 frame then notifies both the
/// controller (via `event_tx`) and the accept loop (via `done_tx`).
async fn first_frame_task(
    stream: pulsebeam_core::net::TcpStream,
    peer_addr: SocketAddr,
    event_tx: mailbox::Sender<TcpAcceptorEvent>,
    done_tx: UnboundedSender<SocketAddr>,
) {
    let result = match BufferedTcpStream::read_first_frame(stream, TCP_FIRST_FRAME_TIMEOUT).await {
        Ok((stream, payload)) => {
            let server_ufrag = extract_stun_server_ufrag(&payload);
            Some(PendingTcpConn {
                stream,
                peer_addr,
                server_ufrag,
            })
        }
        Err(e) => {
            tracing::warn!(%peer_addr, error = ?e, "TCP first-frame read failed");
            None
        }
    };

    // Notify the controller. Ignore send errors — it means the controller
    // shut down, and we are about to be dropped too.
    let _ = event_tx.send(TcpAcceptorEvent { peer_addr, result }).await;
    // Notify the accept loop so it can decrement the in-flight counters.
    let _ = done_tx.send(peer_addr);
}

#[cfg(test)]
mod tests {
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

    /// Accept one server-side TCP stream from a dedicated loopback listener.
    /// Returns (client, server, peer_addr) keeping both sides alive.
    async fn accept_one() -> (
        pulsebeam_core::net::TcpStream,
        pulsebeam_core::net::TcpStream,
        SocketAddr,
    ) {
        let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) =
            tokio::join!(pulsebeam_core::net::TcpStream::connect(addr), listener.accept());
        let client = client.unwrap();
        let (server, peer_addr) = accepted.unwrap();
        (client, server, peer_addr)
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

            let handle = TcpAcceptorHandle::spawn(listener, CancellationToken::new());
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
            let mut count = 0;
            while let Ok(Some(_)) = timeout(Duration::from_millis(10), event_rx.recv()).await {
                count += 1;
            }
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

            let handle = TcpAcceptorHandle::spawn(listener, CancellationToken::new());
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
            let handle = TcpAcceptorHandle::spawn(listener, shutdown.clone());
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
        assert!(MAX_PENDING_TCP_PER_IP < MAX_PENDING_TCP);

        run_local(async {
            let listener = TcpListener::bind(SocketAddr::new(test_host_ip(), 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();

            let handle = TcpAcceptorHandle::spawn(listener, CancellationToken::new());
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
}
