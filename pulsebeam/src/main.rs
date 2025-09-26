use mimalloc::MiMalloc;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::net::{IpAddr, SocketAddr};

use pulsebeam::node;
use pulsebeam_runtime::{net, rt};
use systemstat::{IpAddr as SysIpAddr, Platform, System};
use tracing_subscriber::EnvFilter;

fn main() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .pretty()
        .init();

    // Runtime priority strategy for SFU:
    // - IO runtime stays at *default* system priority (normal).
    //   We do NOT boost it above normal to avoid starving CPU workers.
    // - CPU runtime threads are set to a slightly *lower* priority (e.g. 30–40 on
    //   thread-priority’s crossplatform scale). This ensures:
    //     * Signaling / HTTP / control I/O remains responsive under load.
    //     * CPU-heavy media tasks still get full throughput when system is idle.
    //     * No elevated privileges needed (unlike raising priority).
    // In short: keep IO normal, lower CPU pool, let the OS scheduler do the rest.
    //
    // We intentionally avoid CPU core pinning here:
    // - Pinning can reduce scheduler flexibility: if a pinned worker is overloaded
    //   while another core is idle, throughput suffers.
    // - In multi-tenant / VPS / VM / NUMA environments, pinning may backfire by
    //   increasing memory latency or conflicting with the hypervisor.
    // - Letting the OS scheduler float CPU worker threads generally gives better
    //   balance and portability across environments.
    // If ultra-low latency is required on bare metal, prefer isolating a core at
    // the deployment level (e.g. taskset, cgroups) instead of hard pinning in code.

    // https://github.com/tokio-rs/tokio/discussions/6831
    let cpu_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .thread_name("cpu")
        .on_thread_start(|| {
            // lower priority for CPU works so IO gets higher priority
            thread_priority::set_current_thread_priority(
                thread_priority::ThreadPriority::Crossplatform(
                    thread_priority::ThreadPriorityValue::try_from(40).unwrap(),
                ),
            )
            .unwrap();
        })
        .build()
        .unwrap();

    let shutdown = CancellationToken::new();
    let io_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("io")
        .build()
        .unwrap();
    io_rt.block_on(run(shutdown.clone(), &cpu_rt));
    shutdown.cancel();
}

pub async fn run(shutdown: CancellationToken, cpu_rt: &rt::Runtime) {
    let external_ip = select_host_address();
    let external_addr: SocketAddr = format!("{}:3478", external_ip).parse().unwrap();
    let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
    let unified_socket =
        net::UnifiedSocket::bind(local_addr, net::Transport::Udp, Some(external_addr))
            .await
            .expect("bind to udp socket");
    let http_addr: SocketAddr = "0.0.0.0:3000".parse().unwrap();
    tracing::info!(
        "server listening at {}:3000 (signaling) and {}:3478 (webrtc)",
        external_ip,
        external_ip
    );

    // Run the main logic and signal handler concurrently
    tokio::select! {
        Err(err) = node::run(shutdown.clone(), cpu_rt, external_addr, unified_socket, http_addr) => {
            tracing::warn!("node exited with error: {err}");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received SIGINT, shutting down gracefully...");
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
