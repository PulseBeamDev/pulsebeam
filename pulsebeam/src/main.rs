use mimalloc::MiMalloc;
use pulsebeam_runtime::system;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroUsize,
};

use pulsebeam::node;
use systemstat::{IpAddr as SysIpAddr, Platform, System};
use tracing_subscriber::EnvFilter;

fn main() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_ansi(false)
        .compact()
        .init();

    let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    tracing::info!("using {} worker threads", workers);

    // let rt = tokio::runtime::Builder::new_multi_thread()
    //     .enable_all()
    //     .worker_threads(workers)
    //     .build()
    //     .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let shutdown = CancellationToken::new();
    rt.block_on(run(shutdown.clone(), workers));
    shutdown.cancel();
}

pub async fn run(shutdown: CancellationToken, workers: usize) {
    let external_ip = select_host_address();
    let external_addr: SocketAddr = format!("{}:3478", external_ip).parse().unwrap();

    let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
    let http_addr: SocketAddr = "0.0.0.0:3000".parse().unwrap();
    let internal_http_addr: SocketAddr = "0.0.0.0:6060".parse().unwrap();
    tracing::info!(
        "server listening at {}:3000 (signaling), {}:3478 (webrtc), and 0.0.0.0:6060 (metrics/pprof)",
        external_ip,
        external_ip
    );

    // Run the main logic and signal handler concurrently
    tokio::select! {
        Err(err) = node::run(shutdown.clone(), workers, external_addr, local_addr, http_addr, internal_http_addr) => {
            tracing::warn!("node exited with error: {err}");
        }
        _ = system::wait_for_signal() => {
            tracing::info!("shutting down gracefully...");
            shutdown.cancel();
        }
    }
}

pub fn select_host_address() -> IpAddr {
    let system = System::new();
    let networks = match system.networks() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("could not get network interfaces: {e}");
            return IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        }
    };

    let mut external_candidates = vec![];
    let mut lan_candidates = vec![];

    for (name, net) in &networks {
        // skip virtual / docker / bridge interfaces
        if name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
            || name.starts_with("virbr")
        {
            tracing::debug!("skipping virtual interface {}", name);
            continue;
        }

        // optionally restrict to known LAN interface patterns
        if !(name.starts_with("en") || name.starts_with("eth") || name.starts_with("wlp")) {
            tracing::debug!("skipping non-lan interface {}", name);
            continue;
        }

        for n in &net.addrs {
            if let SysIpAddr::V4(ipv4) = n.addr {
                if ipv4.is_loopback() {
                    tracing::debug!("skipping loopback {}: {}", name, ipv4);
                    continue;
                }

                if !ipv4.is_private() {
                    external_candidates.push(IpAddr::V4(ipv4));
                    tracing::info!("found candidate external ip on {}: {}", name, ipv4);
                } else {
                    lan_candidates.push(IpAddr::V4(ipv4));
                    tracing::info!("found candidate lan ip on {}: {}", name, ipv4);
                }
            }
        }
    }

    if let Some(ip) = external_candidates.first() {
        tracing::info!("selecting external ip: {}", ip);
        *ip
    } else if let Some(ip) = lan_candidates.first() {
        tracing::info!("selecting lan ip: {}", ip);
        *ip
    } else {
        tracing::warn!("falling back to localhost");
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    }
}
