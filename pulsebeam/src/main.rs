use clap::Parser;
use pulsebeam::node::NodeBuilder;
use pulsebeam_runtime::rand;
use std::{net::SocketAddr, num::NonZeroUsize};
use tokio::runtime::LocalOptions;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// References:
//   * https://jemalloc.net/jemalloc.3.html#opt.percpu_arena
//   * https://github.com/jemalloc/jemalloc/blob/dev/TUNING.md
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = concat!(
    "lg_tcache_max:19,", // 512KB limit: buffers GRO/GSO packets & hash expansions lock-free
    "dirty_decay_ms:30000,", // Soft 1s amortization window prevents huge inline purge spikes
    "muzzy_decay_ms:0,", // Bypass the unpredictable kernel muzzy gray-zone entirely
    "abort_conf:true",   // Safely crash on boot if any setting above is invalid
    "\0"                 // Null-terminator required for C-compatibility
)
.as_bytes();

// TODO: disabled heap profiler for now. This keeps causing latency spikes by a few ms.
// #[allow(non_upper_case_globals)]
// #[unsafe(export_name = "malloc_conf")]
// pub static malloc_conf: &[u8] = b"\
//     percpu_arena:percpu,\
//     background_thread:true,\
//     dirty_decay_ms:5000,\
//     muzzy_decay_ms:5000,\
//     metadata_thp:disabled,\
//     prof:true,\
//     prof_active:true,\
//     lg_prof_sample:21,\
//     abort_conf:true\
//     \0";

// use mimalloc::MiMalloc;
//
// #[global_allocator]
// static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Enable development mode preset
    #[arg(short, long)]
    dev: bool,
    /// Pin to a specific network interface name (e.g., enp0s13f0u1u2)
    #[arg(short = 'i', long = "iface")]
    iface: Option<String>,
}

fn main() {
    let args = Args::parse();
    let (non_blocking_writer, _guard) = tracing_appender::non_blocking(std::io::stdout());

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_writer)
        .with_ansi(true)
        .compact();

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();

    // Control thread is floating between threads
    let total_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let workers = total_cores;
    tracing::info!(
        "using {} data plane worker threads ({} total cores)",
        workers,
        total_cores
    );

    let mut rt_builder = tokio::runtime::Builder::new_current_thread();
    let rt = rt_builder
        .enable_all()
        // .worker_threads(workers)
        // .disable_lifo_slot()
        // https://github.com/tokio-rs/tokio/issues/7745
        .enable_alt_timer()
        .build_local(LocalOptions::default())
        .unwrap();
    let rtc_port: u16 = if args.dev { 3478 } else { 443 };
    let shutdown = CancellationToken::new();
    rt.block_on(run(shutdown.clone(), workers, rtc_port, args.iface));
    shutdown.cancel();
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    rtc_port: u16,
    network_interface: Option<String>,
) {
    let external_ips =
        pulsebeam_runtime::system::select_host_addresses(network_interface.as_deref());
    let external_addrs: Vec<SocketAddr> = external_ips
        .iter()
        .copied()
        .map(|ip| SocketAddr::new(ip, rtc_port))
        .collect();
    let local_addr: SocketAddr = format!("[::]:{}", rtc_port).parse().unwrap();
    let http_api_addr: SocketAddr = "[::]:7070".parse().unwrap();
    let metrics_addr: SocketAddr = "[::]:6060".parse().unwrap();

    tracing::info!(
        ?external_addrs,
        "Starting node with advertised RTC addresses"
    );
    tracing::info!("API listening on {http_api_addr}");
    let rng = rand::os_rng();
    let node_builder = NodeBuilder::new()
        .workers(workers)
        .local_addr(local_addr)
        .external_addrs(external_addrs)
        .rng(rng)
        .with_http_api(http_api_addr)
        .with_internal_metrics(metrics_addr);

    let node = node_builder.run(shutdown.child_token());
    let node_handle = tokio::task::spawn(node);

    tracing::info!("server started...");

    tokio::select! {
        Err(err) = node_handle => {
            tracing::warn!("node exited with error: {err}");
        }
        _ = pulsebeam_runtime::system::wait_for_signal() => {
            tracing::info!("shutting down gracefully...");
            shutdown.cancel();
        }
    }
}
