use mimalloc::MiMalloc;
use pulsebeam::{controller, signaling, sink, source, system};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::{actor, prelude::*};
use pulsebeam_runtime::{net, rand};
use systemstat::{Platform, System};
use tokio::task::JoinSet;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing_subscriber::EnvFilter;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(run());
}

async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .pretty()
        .init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));

    let ip = select_host_address();
    let local_addr: SocketAddr = format!("{ip}:3478").parse().expect("valid bind addr");
    let socket = net::UnifiedSocket::bind(local_addr, net::Transport::Udp)
        .await
        .expect("bind to udp socket");

    let mut join_set = JoinSet::new();
    let rng = rand::Rng::from_os_rng();
    let source_actor = source::SourceActor::new(local_addr, socket.clone());
    let sink_actor = sink::SinkActor::new(socket.clone());
    let system_ctx = system::SystemContext {
        rng: rng.clone(),
        source_handle: actor::spawn(&mut join_set, source_actor, 1024, 1024),
        sink_handle: actor::spawn(&mut join_set, sink_actor, 1024, 1024),
    };
    let controller_actor = controller::ControllerActor::new(
        system_ctx,
        vec![local_addr],
        Arc::new("root".to_string()),
    );
    let controller_handle = actor::spawn(&mut join_set, controller_actor, 1024, 1024);

    let router = signaling::router(rng, controller_handle).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let signaling = async move {
        let _ = axum::serve(listener, router).await;
    };

    while (join_set.join_next().await).is_some() {}
}

pub fn select_host_address() -> IpAddr {
    let system = System::new();
    let networks = system.networks().unwrap();

    for net in networks.values() {
        for n in &net.addrs {
            if let systemstat::IpAddr::V4(v) = n.addr {
                if !v.is_loopback() && !v.is_link_local() && !v.is_broadcast() {
                    return IpAddr::V4(v);
                }
            }
        }
    }

    panic!("Found no usable network interface");
}
