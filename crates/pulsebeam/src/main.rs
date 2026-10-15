#![cfg_attr(not(test), forbid(unsafe_code))]

use anyhow::{Context, Result};
use clap::Parser;
use pulsebeam::node::NodeBuilder;
use pulsebeam_core::auth::{
    DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID, KeyRegistry,
    KeyRegistryEntry,
};
use pulsebeam_runtime::rand;
use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    path::Path,
};
use tokio::runtime::LocalOptions;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

// #[cfg(not(target_env = "msvc"))]
// #[global_allocator]
// static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
//
// // References:
// //   * https://jemalloc.net/jemalloc.3.html#opt.percpu_arena
// //   * https://github.com/jemalloc/jemalloc/blob/dev/TUNING.md
// #[allow(non_upper_case_globals)]
// #[unsafe(export_name = "malloc_conf")]
// pub static malloc_conf: &[u8] = concat!(
//     "lg_tcache_max:19,", // 512KB limit: buffers GRO/GSO packets & hash expansions lock-free
//     "dirty_decay_ms:30000,", // Soft 1s amortization window prevents huge inline purge spikes
//     "muzzy_decay_ms:0,", // Bypass the unpredictable kernel muzzy gray-zone entirely
//     "abort_conf:true",   // Safely crash on boot if any setting above is invalid
//     "\0"                 // Null-terminator required for C-compatibility
// )
// .as_bytes();

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

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Enable development mode preset
    #[arg(short, long)]
    dev: bool,
    /// Pin to a specific network interface name (e.g., enp0s13f0u1u2)
    #[arg(short = 'i', long = "iface")]
    iface: Option<String>,
    #[arg(short, long, default_value_t = 16)]
    shards: usize,
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

    let registry =
        configured_key_registry(&args, Path::new("key-registry.json")).unwrap_or_else(|error| {
            pulsebeam_runtime::fatal!("invalid authentication configuration: {error:#}")
        });
    tracing::info!(project_id = %registry.project_id(), "configured authentication project");

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
        .unwrap_or_else(|err| pulsebeam_runtime::fatal!("cannot build the node runtime: {err}"));
    let rtc_port: u16 = if args.dev { 3478 } else { 443 };
    let shutdown = CancellationToken::new();
    if let Err(err) = rt.block_on(run(
        shutdown.clone(),
        workers,
        rtc_port,
        args.iface,
        args.shards,
        registry,
    )) {
        pulsebeam_runtime::fatal!("server failed: {err:#}");
    }
    shutdown.cancel();
}

fn configured_key_registry(args: &Args, path: &Path) -> Result<KeyRegistry> {
    if args.dev {
        return Ok(KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )?);
    }
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read key registry {}", path.display()))?;
    Ok(KeyRegistry::parse_json(&json)?)
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    rtc_port: u16,
    network_interface: Option<String>,
    shards_per_worker: usize,
    key_registry: KeyRegistry,
) -> Result<()> {
    let external_ips =
        pulsebeam_runtime::system::select_host_addresses(network_interface.as_deref());
    let external_addrs: Vec<SocketAddr> = external_ips
        .iter()
        .copied()
        .map(|ip| SocketAddr::new(ip, rtc_port))
        .collect();
    let unspecified_v6 = |port| SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port);
    let local_addr = unspecified_v6(rtc_port);
    let http_api_addr = unspecified_v6(7070);
    let metrics_addr = unspecified_v6(6060);

    tracing::info!(
        ?external_addrs,
        "Starting node with advertised RTC addresses"
    );
    let rng = rand::os_rng();
    let node_builder = NodeBuilder::new()
        .workers(workers)
        .local_addr(local_addr)
        .external_addrs(external_addrs)
        .rng(rng)
        .work_stealing(shards_per_worker)
        .with_http_api(http_api_addr)
        .with_key_registry(key_registry)
        .with_internal_metrics(metrics_addr);

    let node = node_builder.run(shutdown.child_token());
    // Not `spawn`: under `--features sim` a bound socket belongs to a
    // thread-local `SO_REUSEPORT` group, which makes the node future `!Send`.
    // It runs on this thread either way, so nothing is lost by saying so.
    let node_handle = tokio::task::spawn_local(node);

    tracing::info!("starting server...");

    tokio::select! {
        result = node_handle => {
            result.context("node task failed")??;
            tracing::warn!("node stopped");
        }
        _ = pulsebeam_runtime::system::wait_for_signal() => {
            tracing::info!("shutting down gracefully...");
            shutdown.cancel();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;
    use pulsebeam_core::{
        auth::ApiSigningKey,
        identity::{ApiKeyId, ProjectId},
    };
    use std::path::PathBuf;

    fn project(last: u8) -> ProjectId {
        ProjectId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, last])
    }

    fn key_id(last: u8) -> ApiKeyId {
        ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, last])
    }

    fn entry(last: u8) -> KeyRegistryEntry {
        KeyRegistryEntry {
            key_id: key_id(last),
            public_key: ApiSigningKey::from_seed([last; 32]).public_key(),
        }
    }

    fn registry_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "pulsebeam-server-key-registry-{}.json",
            ApiKeyId::new()
        ))
    }

    #[test]
    fn authentication_only_needs_explicit_dev_mode() {
        assert!(Args::try_parse_from(["pulsebeam", "--dev"]).is_ok());
        assert!(Args::try_parse_from(["pulsebeam"]).is_ok());
        assert!(
            Args::try_parse_from(["pulsebeam", "--project-registry", "registry.json"]).is_err()
        );
    }

    #[test]
    fn development_configuration_uses_the_well_known_project() {
        let args = Args::try_parse_from(["pulsebeam", "--dev"]).unwrap();
        let registry = configured_key_registry(&args, Path::new("nonexistent.json")).unwrap();
        assert_eq!(registry.project_id(), *DEVELOPMENT_PROJECT_ID);
        assert_eq!(registry.keys().len(), 1);
        assert_eq!(registry.keys()[0].key_id, *DEVELOPMENT_API_KEY_ID);
    }

    #[test]
    fn production_configuration_requires_valid_key_registry() {
        let path = registry_path();
        let args = Args::try_parse_from(["pulsebeam"]).unwrap();
        assert!(configured_key_registry(&args, &path).is_err());

        let registry = KeyRegistry::new(project(1), vec![entry(1)]).unwrap();
        std::fs::write(&path, registry.to_pretty_json().unwrap()).unwrap();
        let configured = configured_key_registry(&args, &path).unwrap();
        assert_eq!(configured.project_id(), project(1));
        assert_eq!(configured.keys(), registry.keys());

        std::fs::write(&path, "{\"projectId\":\"invalid\",\"keys\":[]}").unwrap();
        assert!(configured_key_registry(&args, &path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
