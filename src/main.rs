extern crate jemallocator;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use pulsebeam::{
    controller::ControllerHandle, egress::EgressHandle, ingress::IngressHandle,
    message::ActorError, signaling,
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

    let (ingress_handle, ingress_actor) = IngressHandle::new(local_addr, socket.clone());
    let (egress_handle, egress_actor) = EgressHandle::new(socket.clone());
    let (controller_handle, controller_actor) =
        ControllerHandle::new(ingress_handle, egress_handle);

    let router = signaling::router(controller_handle).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let signaling = async move {
        axum::serve(listener, router)
            .await
            .map_err(|err| ActorError::Unknown(err.to_string()))
    };

    let res = tokio::try_join!(
        ingress_actor.run(),
        egress_actor.run(),
        controller_actor.run(),
        signaling,
    );
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
