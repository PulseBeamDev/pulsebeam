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

#[derive(clap::ValueEnum, Clone, Debug, Copy, PartialEq)]
pub enum RtMode {
    /// Thread-per-core (default)
    Tpc,
    /// Work-stealing
    Stealing,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Enable development mode preset
    #[arg(short, long)]
    dev: bool,

    /// Runtime mode
    #[arg(
        long = "runtime",
        visible_alias = "rt",
        value_enum,
        default_value_t = RtMode::Tpc
    )]
    pub rt: RtMode,
}

trait Runtime {
    fn block_on<F: Future>(&self, future: F) -> F::Output;
}

enum PulsebeamRuntime {
    LocalRuntime(tokio::runtime::LocalRuntime),
    MultiThreadedRuntime(tokio::runtime::Runtime),
}

impl Runtime for PulsebeamRuntime {
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        match self {
            Self::LocalRuntime(rt) => rt.block_on(future),
            Self::MultiThreadedRuntime(rt) => rt.block_on(future),
        }
    }
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

    // Control thread is floating between threads
    let total_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let workers = total_cores;
    tracing::info!(
        "using {} data plane worker threads ({} total cores)",
        workers,
        total_cores
    );

    let rt = match args.rt {
        RtMode::Tpc => {
            let mut rt_builder = tokio::runtime::Builder::new_current_thread();
            let rt = rt_builder
                .enable_all()
                // .worker_threads(workers)
                // .disable_lifo_slot()
                // https://github.com/tokio-rs/tokio/issues/7745
                .enable_alt_timer()
                .build_local(LocalOptions::default())
                .unwrap();
            PulsebeamRuntime::LocalRuntime(rt)
        }
        RtMode::Stealing => {
            let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
            let rt = rt_builder
                .enable_all()
                // .worker_threads(workers)
                // .disable_lifo_slot()
                // https://github.com/tokio-rs/tokio/issues/7745
                .enable_alt_timer()
                .build()
                .unwrap();
            PulsebeamRuntime::MultiThreadedRuntime(rt)
        }
    };

    let rtc_port: u16 = if args.dev { 3478 } else { 443 };
    let shutdown = CancellationToken::new();
    let shared_runtime = matches!(args.rt, RtMode::Stealing);
    rt.block_on(run(shutdown.clone(), workers, rtc_port, shared_runtime));
    shutdown.cancel();
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    rtc_port: u16,
    use_shared_runtime: bool,
) {
    let external_ip = pulsebeam_runtime::system::select_host_address();
    let external_addr: SocketAddr = format!("{}:{}", external_ip, rtc_port).parse().unwrap();
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse().unwrap();
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse().unwrap();
    let metrics_addr: SocketAddr = "0.0.0.0:6060".parse().unwrap();

    tracing::info!("Starting node on {external_addr} (RTC), {http_api_addr} (API)");
    let rng = rand::os_rng();
    let mut node_builder = NodeBuilder::new()
        .workers(workers)
        .local_addr(local_addr)
        .external_addr(external_addr)
        .rng(rng)
        .with_http_api(http_api_addr)
        .with_internal_metrics(metrics_addr);

    if use_shared_runtime {
        node_builder = node_builder.with_current_runtime();
    }
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
