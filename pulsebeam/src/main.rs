use clap::Parser;
use pulsebeam::node::NodeBuilder;
use pulsebeam_runtime::system;
use std::{net::SocketAddr, num::NonZeroUsize};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// sampling every 32MB allocations
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = b"background_thread:true,metadata_thp:auto,dirty_decay_ms:30000,muzzy_decay_ms:30000,lg_tcache_max:21,prof:true,prof_active:true,lg_prof_sample:25\0";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Enable development mode preset
    #[arg(short, long)]
    dev: bool,
}

fn main() {
    let args = Args::parse();
    let use_tokio_console = cfg!(feature = "tokio-console");

    if use_tokio_console {
        #[cfg(feature = "tokio-console")]
        console_subscriber::init();
    } else {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_ansi(true);

        let registry = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer);
        registry.init();
    }

    let workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    tracing::info!("using {} worker threads", workers);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(workers)
        // https://github.com/tokio-rs/tokio/issues/7745
        .enable_alt_timer()
        .build()
        .unwrap();

    let rtc_port: u16 = if args.dev { 3478 } else { 443 };
    let shutdown = CancellationToken::new();
    rt.block_on(run(shutdown.clone(), workers, rtc_port));
    shutdown.cancel();
}

pub async fn run(shutdown: CancellationToken, workers: usize, rtc_port: u16) {
    let external_ip = system::select_host_address();
    let external_addr: SocketAddr = format!("{}:{}", external_ip, rtc_port).parse().unwrap();
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse().unwrap();
    let http_api_addr: SocketAddr = "0.0.0.0:3000".parse().unwrap();
    let metrics_addr: SocketAddr = "0.0.0.0:6060".parse().unwrap();

    tracing::info!("Starting node on {external_addr} (RTC), {http_api_addr} (API)");
    let node_handle = NodeBuilder::new()
        .workers(workers)
        .local_addr(local_addr)
        .external_addr(external_addr)
        .with_http_api(http_api_addr)
        .with_internal_metrics(metrics_addr)
        .run(shutdown.child_token());

    tracing::info!("server started...");

    tokio::select! {
        Err(err) = node_handle => {
            tracing::warn!("node exited with error: {err}");
        }
        _ = system::wait_for_signal() => {
            tracing::info!("shutting down gracefully...");
            shutdown.cancel();
        }
    }
}
