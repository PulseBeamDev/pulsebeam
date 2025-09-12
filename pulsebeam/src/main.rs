use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::net::{IpAddr, SocketAddr};

use pulsebeam::node;
use pulsebeam_runtime::{net, rt};
use systemstat::{Platform, System};
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

    let io_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("io")
        .build()
        .unwrap();
    io_rt.block_on(run(cpu_rt));
}

pub async fn run(cpu_rt: rt::Runtime) {
    let external_ip = select_host_address();
    let external_addr: SocketAddr = format!("{}:3478", external_ip).parse().unwrap();
    let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
    let unified_socket = net::UnifiedSocket::bind(local_addr, net::Transport::Udp)
        .await
        .expect("bind to udp socket");
    let http_socket: SocketAddr = "0.0.0.0:3000".parse().unwrap();
    tracing::info!(
        "✅ Signaling server listening. Clients should connect to https://{}:3000 or https://localhost:3000",
        external_ip,
    );

    node::run(cpu_rt, external_addr, unified_socket, http_socket, true)
        .await
        .unwrap();
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
