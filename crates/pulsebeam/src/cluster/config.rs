use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub cluster: Option<ClusterConfig>,
}

impl ServerConfig {
    pub fn read(path: Option<&Path>) -> Result<Self> {
        match path {
            None => Ok(Self::default()),
            Some(path) => {
                let bytes = std::fs::read_to_string(path)
                    .with_context(|| format!("reading server config {}", path.display()))?;
                toml::from_str(&bytes)
                    .with_context(|| format!("invalid server config {}", path.display()))
            }
        }
    }

    pub async fn resolve(self, data_dir: Option<PathBuf>) -> Result<Option<Options>> {
        let Some(cluster) = self.cluster else {
            return Ok(None);
        };
        let data_dir = data_dir.context("[cluster] requires an explicit --data-dir")?;
        ensure!(
            !data_dir.as_os_str().is_empty(),
            "--data-dir must not be empty"
        );
        ensure!(
            !cluster.id.trim().is_empty()
                && cluster.id.len() <= 128
                && !cluster.id.chars().any(char::is_control),
            "cluster.id must contain 1..128 bytes without control characters"
        );
        ensure!(
            !cluster.advertise_host.trim().is_empty(),
            "cluster.advertise_host is required and must be peer reachable"
        );
        let listen = cluster.gossip.listen;
        ensure!(
            listen.port() != 0 && !listen.ip().is_multicast(),
            "cluster.gossip.listen must have a nonzero port and a unicast or wildcard bind address"
        );
        ensure!(
            !matches!(listen.ip(), IpAddr::V4(ip) if ip.is_broadcast()),
            "cluster.gossip.listen cannot be a broadcast address"
        );
        let advertise = resolve_host(&cluster.advertise_host, listen.port())
            .await
            .context("resolving cluster.advertise_host")?;
        ensure!(
            advertise.is_ipv4() == listen.is_ipv4(),
            "cluster.advertise_host and gossip.listen must use the same IP family"
        );
        let mut seeds = BTreeSet::new();
        for seed in cluster.seeds {
            let resolved = unique_endpoint(
                pulsebeam_runtime::net::lookup_host(&seed)
                    .await
                    .with_context(|| format!("resolving cluster seed {seed}"))?,
            )
            .with_context(|| format!("invalid cluster seed {seed}"))?;
            ensure!(
                resolved.is_ipv4() == listen.is_ipv4(),
                "cluster seed {seed} has a different IP family from gossip.listen"
            );
            seeds.insert(resolved);
        }
        Ok(Some(Options {
            cluster_id: cluster.id,
            listen,
            advertise,
            seeds: seeds.into_iter().collect(),
            data_dir,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    id: String,
    advertise_host: String,
    #[serde(default)]
    seeds: Vec<String>,
    #[serde(default)]
    gossip: GossipConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GossipConfig {
    #[serde(default = "default_listen")]
    listen: SocketAddr,
}

fn default_listen() -> SocketAddr {
    ([0, 0, 0, 0], 7946).into()
}
impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

/// Resolved host-owned configuration. The membership engine never reads paths or DNS.
#[derive(Clone, Debug)]
pub struct Options {
    pub cluster_id: String,
    pub listen: SocketAddr,
    pub advertise: SocketAddr,
    pub seeds: Vec<SocketAddr>,
    pub data_dir: PathBuf,
}

async fn resolve_host(host: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return unique_endpoint(std::iter::once(SocketAddr::new(ip, port)));
    }
    ensure!(
        !host.contains(':') && !host.contains('/') && !host.contains('['),
        "advertise_host must be a host without a port or brackets"
    );
    unique_endpoint(pulsebeam_runtime::net::lookup_host((host, port)).await?)
}

fn unique_endpoint(endpoints: impl Iterator<Item = SocketAddr>) -> Result<SocketAddr> {
    let endpoints: BTreeSet<_> = endpoints.collect();
    ensure!(
        endpoints.len() == 1,
        "endpoint must resolve to exactly one address; configure an explicit IP for ambiguous DNS"
    );
    let addr = endpoints
        .into_iter()
        .next()
        .context("endpoint has no address")?;
    ensure!(
        addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast(),
        "endpoint must be a peer-reachable unicast address with a nonzero port"
    );
    match addr.ip() {
        IpAddr::V4(ip) => ensure!(!ip.is_broadcast(), "broadcast is not a gossip endpoint"),
        IpAddr::V6(ip) => ensure!(
            !ip.is_unicast_link_local(),
            "link-local gossip endpoints without a scope are unsupported"
        ),
    }
    Ok(addr)
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests {
    use super::*;

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn omitted_cluster_is_standalone_and_does_not_touch_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let unused = dir.path().join("unused");
        assert!(
            ServerConfig::read(None)
                .unwrap()
                .resolve(Some(unused.clone()))
                .await
                .unwrap()
                .is_none()
        );
        let empty: ServerConfig = toml::from_str("").unwrap();
        assert!(empty.resolve(None).await.unwrap().is_none());
        assert!(!unused.exists());
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn defaults_explicit_advertise_and_required_data_directory() {
        let text = "[cluster]\nid = 'test'\nadvertise_host = '127.0.0.1'\n";
        assert!(
            toml::from_str::<ServerConfig>(text)
                .unwrap()
                .resolve(None)
                .await
                .is_err()
        );
        let config = toml::from_str::<ServerConfig>(text)
            .unwrap()
            .resolve(Some("state".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config.listen, "0.0.0.0:7946".parse().unwrap());
        assert_eq!(config.advertise, "127.0.0.1:7946".parse().unwrap());
        assert!(config.seeds.is_empty());
        for bad in [
            "[cluster]\nid='test'",
            "[cluster]\nid='test'\nadvertise_host='127.0.0.1'\ngeneration=1",
        ] {
            assert!(toml::from_str::<ServerConfig>(bad).is_err());
        }
        for (id, host, port) in [
            ("", "127.0.0.1", 7946),
            ("test", "0.0.0.0", 7946),
            ("test", "239.1.1.1", 7946),
            ("test", "127.0.0.1:7946", 7946),
            ("test", "127.0.0.1", 0),
        ] {
            let text = format!(
                "[cluster]\nid='{id}'\nadvertise_host='{host}'\n[cluster.gossip]\nlisten='0.0.0.0:{port}'"
            );
            assert!(
                toml::from_str::<ServerConfig>(&text)
                    .unwrap()
                    .resolve(Some("state".into()))
                    .await
                    .is_err()
            );
        }
        assert!(
            unique_endpoint(
                [
                    "127.0.0.1:7946".parse().unwrap(),
                    "127.0.0.2:7946".parse().unwrap()
                ]
                .into_iter()
            )
            .is_err()
        );
    }
}
