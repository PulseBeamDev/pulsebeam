use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use pulsebeam::{
    actor, controller::ControllerHandle, net::UdpSocket, rng::Rng, signaling, sink::UdpSinkHandle,
    source::UdpSourceHandle,
};
use rand::SeedableRng;
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
    let socket = tokio::net::UdpSocket::bind(local_addr)
        .await
        .expect("bind to udp socket");
    let socket: UdpSocket = Arc::new(socket).into();

    let rng = Rng::from_os_rng();
    let (source_handle, source_actor) = UdpSourceHandle::new(local_addr, socket.clone());
    let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
    let (controller_handle, controller_actor) = ControllerHandle::new(
        rng,
        source_handle,
        sink_handle,
        vec![local_addr],
        Arc::new("root".to_string()),
    );

    let router = signaling::router(controller_handle).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let signaling = async move {
        let _ = axum::serve(listener, router).await;
    };

    let mut join_set = JoinSet::new();
    join_set.spawn(actor::run(source_actor));
    join_set.spawn(actor::run(sink_actor));
    join_set.spawn(actor::run(controller_actor));
    join_set.spawn(signaling);

    while let Some(_) = join_set.join_next().await {}
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
