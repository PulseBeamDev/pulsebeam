extern crate jemallocator;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use pulsebeam::{
    controller::ControllerHandle, egress::EgressHandle, ingress::IngressHandle, signaling,
};
use systemstat::{Platform, System};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::level_filters::LevelFilter;

#[tokio::main]
async fn main() {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(env_filter)
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
    let socket = Arc::new(socket);

    let (ingress, ingress_join) = IngressHandle::spawn(local_addr, socket.clone());
    let (egress, egress_join) = EgressHandle::spawn(socket.clone());
    let (controller, controller_join) = ControllerHandle::spawn(ingress, egress);

    let router = signaling::router(controller).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let signaling_join = tokio::spawn(async move { axum::serve(listener, router) });

    let res = tokio::try_join!(ingress_join, egress_join, controller_join, signaling_join);
    if let Err(err) = res {
        tracing::error!("pipeline ended with an error: {err}")
    }
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
