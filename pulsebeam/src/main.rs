use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::net::{IpAddr, SocketAddr};

use pulsebeam::node;
use pulsebeam_runtime::net;
use systemstat::{Platform, System};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .pretty()
        .init();

    // https://github.com/tokio-rs/tokio/discussions/6831
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.spawn(run());
    rt.block_on(handle).unwrap();
    // let rt = tokio::runtime::Builder::new_current_thread()
    //     .enable_all()
    //     .build()
    //     .unwrap();
    // rt.block_on(run());
}

pub async fn run() {
    let external_addr: SocketAddr = format!("{}:3478", select_host_address()).parse().unwrap();
    let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
    let unified_socket = net::UnifiedSocket::bind(local_addr, net::Transport::Udp)
        .await
        .expect("bind to udp socket");
    let http_socket: SocketAddr = "0.0.0.0:3000".parse().unwrap();

    let node = node::Node::new(external_addr, unified_socket, http_socket);
    node.run().await;
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
