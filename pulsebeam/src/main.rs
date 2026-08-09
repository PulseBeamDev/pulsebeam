use clap::Parser;
use pulsebeam::control::auth::{JwtAlg, JwtKeyBytes};
use pulsebeam::node::NodeBuilder;
use pulsebeam_runtime::rand;
use std::{net::SocketAddr, num::NonZeroUsize};
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

    /// Public key for verifying access tokens, as `kid:alg:base64`.
    ///
    /// `alg` is `ed25519` or `es256`. The base64 is the raw public key: 32 bytes for Ed25519,
    /// a 65-byte uncompressed point for ES256. Repeatable, so keys can be rotated.
    #[arg(long = "jwt-key", env = "PULSEBEAM_JWT_KEY", value_delimiter = ',')]
    jwt_key: Vec<String>,

    /// Accepted `aud`. Required before the JSON API will serve: without it a token minted for
    /// another service would verify here.
    #[arg(
        long = "jwt-audience",
        env = "PULSEBEAM_JWT_AUDIENCE",
        value_delimiter = ','
    )]
    jwt_audience: Vec<String>,

    /// Accepted `iss`. When empty, any issuer is accepted.
    #[arg(
        long = "jwt-issuer",
        env = "PULSEBEAM_JWT_ISSUER",
        value_delimiter = ','
    )]
    jwt_issuer: Vec<String>,

    /// Cluster-wide resume-token key, as `kid:base64` over 32 bytes. Must match across every node
    /// and survive restarts, or resumption cannot outlive a restart. Repeatable; the first signs.
    #[arg(
        long = "resume-key",
        env = "PULSEBEAM_RESUME_KEY",
        value_delimiter = ','
    )]
    resume_key: Vec<String>,

    /// Also require a bearer token on the legacy `application/sdp` endpoints.
    #[arg(long = "require-auth", env = "PULSEBEAM_REQUIRE_AUTH")]
    require_auth: bool,
}

fn parse_jwt_key(spec: &str) -> anyhow::Result<(String, JwtAlg, JwtKeyBytes)> {
    use base64::Engine;
    let mut parts = spec.splitn(3, ':');
    let kid = parts.next().unwrap_or_default();
    let alg = parts.next().unwrap_or_default();
    let encoded = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected kid:alg:base64"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|e| anyhow::anyhow!("key for {kid} is not valid base64: {e}"))?;

    let (alg, key) = match alg {
        "ed25519" | "EdDSA" => (JwtAlg::Ed25519, JwtKeyBytes::Ed25519Raw(bytes)),
        "es256" | "ES256" => (JwtAlg::Es256, JwtKeyBytes::Es256Raw(bytes)),
        other => anyhow::bail!("unsupported alg {other:?}, expected ed25519 or es256"),
    };
    Ok((kid.to_string(), alg, key))
}

fn parse_resume_key(spec: &str) -> anyhow::Result<(String, [u8; 32])> {
    use base64::Engine;
    let (kid, encoded) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected kid:base64"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|e| anyhow::anyhow!("resume key for {kid} is not valid base64: {e}"))?;
    let secret: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("resume key for {kid} must be exactly 32 bytes"))?;
    Ok((kid.to_string(), secret))
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
    let auth = args.auth();
    rt.block_on(run(shutdown.clone(), workers, rtc_port, args.iface, auth));
    shutdown.cancel();
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    rtc_port: u16,
    network_interface: Option<String>,
    auth: AuthArgs,
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

    let node_builder = match apply_auth(node_builder, auth) {
        Ok(builder) => builder,
        Err(err) => {
            // Refusing to start beats starting with authentication silently disabled.
            tracing::error!("invalid authentication configuration: {err:#}");
            return;
        }
    };

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

/// Parsed authentication flags, kept separate so `run` stays testable without clap.
pub struct AuthArgs {
    jwt_keys: Vec<String>,
    audiences: Vec<String>,
    issuers: Vec<String>,
    resume_keys: Vec<String>,
    require_auth: bool,
}

impl Args {
    fn auth(&self) -> AuthArgs {
        AuthArgs {
            jwt_keys: self.jwt_key.clone(),
            audiences: self.jwt_audience.clone(),
            issuers: self.jwt_issuer.clone(),
            resume_keys: self.resume_key.clone(),
            require_auth: self.require_auth,
        }
    }
}

fn apply_auth(mut builder: NodeBuilder, auth: AuthArgs) -> anyhow::Result<NodeBuilder> {
    for spec in &auth.jwt_keys {
        let (kid, alg, key) = parse_jwt_key(spec)?;
        builder = builder.with_jwt_key(&kid, alg, key)?;
    }
    for aud in auth.audiences {
        builder = builder.with_jwt_audience(aud);
    }
    for iss in auth.issuers {
        builder = builder.with_jwt_issuer(iss);
    }
    for spec in &auth.resume_keys {
        let (kid, secret) = parse_resume_key(spec)?;
        builder = builder.with_resume_key(&kid, secret)?;
    }
    Ok(builder.with_require_auth(auth.require_auth))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_jwt_key_spec_parses_for_both_algorithms() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;

        let ed = b64.encode([7u8; 32]);
        let (kid, alg, key) = parse_jwt_key(&format!("key-1:ed25519:{ed}")).unwrap();
        assert_eq!(kid, "key-1");
        assert_eq!(alg, JwtAlg::Ed25519);
        assert!(matches!(key, JwtKeyBytes::Ed25519Raw(b) if b.len() == 32));

        let mut point = vec![0x04u8];
        point.extend_from_slice(&[9u8; 64]);
        let (_, alg, key) = parse_jwt_key(&format!("key-2:es256:{}", b64.encode(&point))).unwrap();
        assert_eq!(alg, JwtAlg::Es256);
        assert!(matches!(key, JwtKeyBytes::Es256Raw(b) if b.len() == 65));
    }

    #[test]
    fn a_malformed_jwt_key_spec_is_rejected_rather_than_silently_ignored() {
        // Starting with authentication quietly disabled is the failure mode worth preventing.
        for spec in [
            "",
            "key-1",
            "key-1:ed25519",
            "key-1:rsa:AAAA",
            "key-1:ed25519:not!base64!",
        ] {
            assert!(parse_jwt_key(spec).is_err(), "{spec:?} must be rejected");
        }
    }

    #[test]
    fn a_resume_key_must_be_exactly_thirty_two_bytes() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;

        let (kid, secret) = parse_resume_key(&format!("rk-1:{}", b64.encode([3u8; 32]))).unwrap();
        assert_eq!(kid, "rk-1");
        assert_eq!(secret, [3u8; 32]);

        assert!(parse_resume_key(&format!("rk-1:{}", b64.encode([3u8; 31]))).is_err());
        assert!(parse_resume_key(&format!("rk-1:{}", b64.encode([3u8; 33]))).is_err());
        assert!(parse_resume_key("no-separator").is_err());
    }

    #[test]
    fn base64_keys_are_accepted_in_standard_and_url_safe_forms() {
        use base64::Engine;
        // 0xFB 0xFF produce '+' and '/' in standard alphabet, '-' and '_' in url-safe.
        let raw = [0xfbu8; 32];
        let standard = base64::engine::general_purpose::STANDARD.encode(raw);
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert_ne!(standard.trim_end_matches('='), url_safe);

        assert_eq!(parse_resume_key(&format!("rk:{standard}")).unwrap().1, raw);
        assert_eq!(parse_resume_key(&format!("rk:{url_safe}")).unwrap().1, raw);
    }
}
