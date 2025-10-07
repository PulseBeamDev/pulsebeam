use crate::{api, controller, gateway};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::collections::double_buffer;
use pulsebeam_runtime::{actor, net, rand};
use pulsebeam_runtime::{mailbox, prelude::*};
use std::sync::atomic::AtomicUsize;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub gateway: gateway::GatewayHandle,
    pub sockets: Vec<mailbox::Sender<net::SendPacket>>,
    egress_counter: Arc<AtomicUsize>,
}

impl NodeContext {
    pub fn single(socket: net::UnifiedSocket) -> Self {
        let socket = Arc::new(socket);

        let (gw, _) = actor::spawn_default(gateway::GatewayActor::new(vec![socket.clone()]));

        Self {
            rng: rand::Rng::from_os_rng(),
            gateway: gw,
            sockets: vec![],
            egress_counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn allocate_egress(&self) -> mailbox::Sender<net::SendPacket> {
        let seq = self
            .egress_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sockets.get(seq % self.sockets.len()).unwrap().clone()
    }
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    external_addr: SocketAddr,
    local_addr: SocketAddr,
    http_addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut sockets: Vec<Arc<net::UnifiedSocket>> = Vec::new();
    for _ in 0..256 {
        let socket =
            match net::UnifiedSocket::bind(local_addr, net::Transport::Udp, Some(external_addr))
                .await
            {
                Ok(socket) => socket,
                Err(err) if sockets.is_empty() => {
                    return Err(anyhow::Error::new(err).context("failed to bind udp"));
                }
                Err(err) => {
                    tracing::warn!("SO_REUSEPORT is not supported, fallback to 1 socket: {err}");
                    break;
                }
            };

        let socket = Arc::new(socket);
        sockets.push(socket);
    }
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    let mut join_set = FuturesUnordered::new();
    let (gateway, gateway_join) = actor::spawn_default(gateway::GatewayActor::new(sockets.clone()));
    join_set.push(gateway_join.map(|_| ()).boxed());
    let rng = rand::Rng::from_os_rng();

    let mut egress = Vec::new();
    for socket in &sockets {
        let socket = socket.clone();
        let (tx, mut rx) = mailbox::new(128);
        tokio::task::spawn(async move {
            let mut db = double_buffer::DoubleBuffer::new(1024);
            loop {
                tokio::select! {
                    Some(msg) = rx.recv() => {
                tracing::trace!("received packet for egress");
                        db.push(msg);
                    }

                    Ok(_) = socket.writable(), if !db.is_empty() => {
                        let Some(buffer) = db.prepare_send() else {
                            continue;
                        };

                        let sent_count = socket.try_send_batch(buffer).unwrap();
                        tracing::trace!("sent {sent_count} packets to socket");
                        db.mark_sent(sent_count);

                        let Some(buffer) = db.prepare_send() else {
                            continue;
                        };

                        let sent_count = socket.try_send_batch(buffer).unwrap();
                        tracing::trace!("sent {sent_count} packets to socket");
                        db.mark_sent(sent_count);
                    }
                }
            }
        });

        egress.push(tx);
    }
    let node_ctx = NodeContext {
        rng,
        gateway,
        sockets: egress,
        egress_counter: Arc::new(AtomicUsize::new(0)),
    };

    let controller_actor = controller::ControllerActor::new(
        node_ctx,
        vec![external_addr],
        Arc::new("root".to_string()),
    );
    let (controller_handle, controller_join) = actor::spawn_default(controller_actor);
    join_set.push(controller_join.map(|_| ()).boxed());

    // HTTP API
    let api_cfg = api::ApiConfig {
        base_path: "/api/v1".to_string(),
        default_host: http_addr.to_string(),
    };
    let router = api::router(controller_handle, api_cfg).layer(cors);

    let shutdown_for_http = shutdown.clone();
    let signaling = async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        tracing::debug!("listening on {http_addr}");
        tokio::select! {
            res = axum::serve(listener, router) => {
                if let Err(e) = res {
                    tracing::error!("http server error: {e}");
                }
            }
            _ = shutdown_for_http.cancelled() => {
                tracing::info!("http server shutting down");
            }
        }
    };

    join_set.push(tokio::spawn(signaling).map(|_| ()).boxed());

    // Wait for all tasks to complete OR shutdown
    tokio::select! {
        _ = async { while join_set.next().await.is_some() {} } => {}
        _ = shutdown.cancelled() => {
            tracing::info!("node received shutdown");
        }
    }

    Ok(())
}
