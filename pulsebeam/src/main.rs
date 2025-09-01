use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use mimalloc::MiMalloc;
use pulsebeam::{controller, signaling, system};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::{actor, net};
use systemstat::{Platform, System};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .pretty()
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(run());
}

async fn run() {
    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));

    let external_addr = select_host_address();
    let local_addr: SocketAddr = format!("{external_addr}:3478")
        .parse()
        .expect("valid bind addr");
    let socket = net::UnifiedSocket::bind(local_addr, net::Transport::Udp)
        .await
        .expect("bind to udp socket");

    // TODO: spawn IO actors in a separate single-threaded runtime
    let mut join_set = FuturesUnordered::new();

    let (system_ctx, system_join) = system::SystemContext::spawn(local_addr, socket);
    join_set.push(system_join.map(|_| ()).boxed());
    let controller_actor = controller::ControllerActor::new(
        system_ctx,
        vec![local_addr],
        Arc::new("root".to_string()),
    );

    // TODO: handle join
    let (controller_handle, controller_join) =
        actor::spawn(controller_actor, actor::RunnerConfig::default());
    join_set.push(controller_join.map(|_| ()).boxed());

    let router = signaling::router(controller_handle).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let signaling = async move {
        let _ = axum::serve(listener, router).await;
    };
    let signaling_handle = tokio::spawn(signaling);
    join_set.push(signaling_handle.map(|_| ()).boxed());

    while (join_set.next().await).is_some() {}
}

pub fn select_host_address() -> IpAddr {
    let system = System::new();
    let networks = system.networks().unwrap();

    for net in networks.values() {
        for n in &net.addrs {
            if let systemstat::IpAddr::V4(v) = n.addr
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
            {
                return IpAddr::V4(v);
            }
        }
    }

    panic!("Found no usable network interface");
}
