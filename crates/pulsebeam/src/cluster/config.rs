use std::collections::BTreeSet;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use pulsebeam_core::auth::{ApiPublicKey, ApiSigningKey};
use pulsebeam_link::{NetworkClass, NetworkPeer, PeerFingerprint, PeerRole, SecretCredential};
use pulsebeam_membership::NodeId;
use serde::Deserialize;

const MAX_PROVISIONED_PEERS: usize = 16;

/// Identities pinned for one ownership Link. Never reuse a DTLS
/// private key across committed incarnations or between nodes.
#[derive(Clone)]
pub struct ProvisionedPeer {
    pub listen: SocketAddr,
    pub local: (NodeId, NonZeroU64),
    pub remote: (NodeId, NonZeroU64),
    pub transport: NetworkPeer,
    /// Explicit programmatic selection of the WAN carrier, with no fallback.
    pub fabric_wan: bool,
    /// Both pinned peers must support revision-1 Opus windows before enabling this.
    /// Explicitly allow WAN sources from this pinned upstream on this outgoing edge.
    pub relay_from: Option<NodeId>,
    pub authority_epoch: [u8; 16],
    pub receipt_key: ApiSigningKey,
    pub peer_receipt_key: ApiPublicKey,
}

impl std::fmt::Debug for ProvisionedPeer {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("ProvisionedPeer")
            .field("listen", &self.listen)
            .field("local", &self.local)
            .field("remote", &self.remote)
            .field("endpoint", &self.transport.endpoint)
            .finish_non_exhaustive()
    }
}

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
        let ownership_peer = match cluster.ownership_peer {
            Some(peer) => {
                ensure!(
                    cluster.topology == Topology::MultiNode,
                    "[cluster.ownership_peer] requires multi_node topology"
                );
                Some(
                    pulsebeam_runtime::task::spawn_blocking(move || peer.resolve())
                        .await
                        .context("ownership peer credential worker failed")??,
                )
            }
            None => None,
        };
        let options = Options {
            cluster_id: cluster.id,
            topology: cluster.topology,
            listen,
            advertise,
            seeds: seeds.into_iter().collect(),
            data_dir,
            ownership_peers: ownership_peer.into_iter().collect(),
        };
        options.validate_topology()?;
        Ok(Some(options))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    id: String,
    advertise_host: String,
    #[serde(default)]
    topology: Topology,
    #[serde(default)]
    seeds: Vec<String>,
    #[serde(default)]
    gossip: GossipConfig,
    ownership_peer: Option<PeerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerConfig {
    listen: SocketAddr,
    local_node: NodeId,
    local_generation: NonZeroU64,
    certificate_der: PathBuf,
    key_pkcs8_der: PathBuf,
    peer_node: NodeId,
    peer_generation: NonZeroU64,
    peer_endpoint: SocketAddr,
    peer_sha256: String,
    authority_epoch: String,
    receipt_key_file: PathBuf,
    peer_receipt_key: ApiPublicKey,
}

impl PeerConfig {
    fn resolve(self) -> Result<ProvisionedPeer> {
        let expected_peer = parse_hex(&self.peer_sha256)
            .map(PeerFingerprint::from_bytes)
            .context("cluster.ownership_peer.peer_sha256 must be 64 hexadecimal digits")?;
        let authority_epoch = parse_hex(&self.authority_epoch)
            .context("cluster.ownership_peer.authority_epoch must be 32 hexadecimal digits")?;
        let signing_key =
            read_identity_file(&self.receipt_key_file).context("reading receipt signing key")?;
        let receipt_key = std::str::from_utf8(&signing_key)
            .ok()
            .and_then(|key| key.trim().parse().ok())
            .context("receipt signing key must be base64url PKCS#8")?;
        let certificate =
            read_identity_file(&self.certificate_der).context("reading Link certificate")?;
        let key = read_identity_file(&self.key_pkcs8_der).context("reading Link private key")?;
        let credential = SecretCredential::from_der(certificate, key)
            .map_err(|_| anyhow::anyhow!("invalid Link certificate or PKCS#8 key"))?;
        let peer = ProvisionedPeer {
            listen: self.listen,
            local: (self.local_node, self.local_generation),
            remote: (self.peer_node, self.peer_generation),
            authority_epoch,
            receipt_key,
            peer_receipt_key: self.peer_receipt_key,
            fabric_wan: false,
            relay_from: None,
            transport: NetworkPeer {
                endpoint: self.peer_endpoint,
                credential,
                expected_peer,
                role: if self.local_node < self.peer_node {
                    PeerRole::Initiator
                } else {
                    PeerRole::Responder
                },
                class: NetworkClass::Wan,
            },
        };
        Ok(peer)
    }
}

fn parse_hex<const N: usize>(input: &str) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    if input.len() != bytes.len().saturating_mul(2) {
        return None;
    }
    for (byte, hex) in bytes.iter_mut().zip(input.as_bytes().chunks_exact(2)) {
        let hex = std::str::from_utf8(hex).ok()?;
        if !hex.bytes().all(|part| part.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(bytes)
}

fn read_identity_file(path: &Path) -> Result<Vec<u8>> {
    const MAX_DER_BYTES: u64 = 16 * 1024;
    ensure!(
        path.is_absolute(),
        "Link identity files require absolute paths"
    );
    ensure!(
        std::fs::metadata(path)?.is_file(),
        "Link identity must be a regular file"
    );
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_DER_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= MAX_DER_BYTES,
        "Link identity exceeds the DER size limit"
    );
    Ok(bytes)
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

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    #[default]
    MultiNode,
    SingleNode,
}

/// Resolved host-owned configuration. The membership engine never reads paths or DNS.
#[derive(Clone, Debug)]
pub struct Options {
    pub cluster_id: String,
    pub topology: Topology,
    pub listen: SocketAddr,
    pub advertise: SocketAddr,
    pub seeds: Vec<SocketAddr>,
    pub data_dir: PathBuf,
    pub ownership_peers: Vec<ProvisionedPeer>,
}

impl Options {
    pub(super) fn validate_topology(&self) -> Result<()> {
        if self.topology == Topology::SingleNode {
            ensure!(
                self.seeds.is_empty(),
                "cluster.topology = 'single_node' does not allow seeds"
            );
            ensure!(
                self.listen.ip().is_loopback() && self.listen == self.advertise,
                "cluster.topology = 'single_node' requires identical loopback listen and advertise endpoints"
            );
        }
        ensure!(
            self.ownership_peers.len() <= MAX_PROVISIONED_PEERS,
            "too many provisioned ownership peers"
        );
        let mut listeners = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        let mut nodes = BTreeSet::new();
        let first = self.ownership_peers.first();
        for peer in &self.ownership_peers {
            if let Some(upstream) = peer.relay_from {
                ensure!(
                    peer.fabric_wan
                        && upstream != peer.remote.0
                        && self.ownership_peers.iter().any(|candidate| {
                            candidate.remote.0 == upstream
                                && candidate.fabric_wan
                                && candidate.relay_from.is_none()
                        }),
                    "WAN transit requires a distinct pinned upstream without an export policy"
                );
            }
            if let Some(first) = first {
                ensure!(
                    first.local == peer.local
                        && first.authority_epoch == peer.authority_epoch
                        && first.receipt_key.public_key() == peer.receipt_key.public_key(),
                    "ownership peers must share the local identity and authority epoch"
                );
            }
            if self.ownership_peers.len() > 1 {
                ensure!(
                    peer.fabric_wan
                        && listeners.insert(peer.listen)
                        && endpoints.insert(peer.transport.endpoint)
                        && nodes.insert(peer.remote.0),
                    "multiple ownership peers require distinct WAN listeners, endpoints and nodes"
                );
            }
            ensure!(
                self.topology == Topology::MultiNode,
                "[cluster.ownership_peer] requires multi_node topology"
            );
            ensure!(
                peer.local.0 != peer.remote.0,
                "peer node must be different from the local node"
            );
            ensure!(
                peer.listen.port() != 0
                    && !peer.listen.ip().is_multicast()
                    && !matches!(peer.listen.ip(), IpAddr::V4(ip) if ip.is_broadcast()),
                "ownership Link must listen on a fixed unicast or wildcard address"
            );
            ensure!(
                !peer.fabric_wan || !peer.listen.ip().is_unspecified(),
                "Fabric WAN requires a concrete local address and cannot use legacy Fabric carriage"
            );
            let remote = unique_endpoint(std::iter::once(peer.transport.endpoint))
                .context("invalid ownership peer endpoint")?;
            ensure!(
                remote.is_ipv4() == peer.listen.is_ipv4(),
                "ownership Link endpoints must use the same address family"
            );
            ensure!(
                remote != peer.listen,
                "ownership peer endpoint must differ from the local listener"
            );
            ensure!(
                peer.transport.expected_peer != peer.transport.credential.fingerprint(),
                "peer and local DTLS pins must differ"
            );
            ensure!(
                peer.authority_epoch != [0; 16],
                "ownership authority epoch must be nonzero"
            );
            ensure!(
                peer.receipt_key.public_key() != peer.peer_receipt_key,
                "peer and local receipt keys must differ"
            );
        }
        ensure!(
            listeners.is_disjoint(&endpoints),
            "ownership listeners cannot also be peer endpoints"
        );
        Ok(())
    }
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
        IpAddr::V4(ip) => ensure!(!ip.is_broadcast(), "broadcast endpoints are unsupported"),
        IpAddr::V6(ip) => ensure!(
            !ip.is_unicast_link_local(),
            "link-local endpoints without a scope are unsupported"
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
    async fn single_node_requires_an_explicit_isolated_endpoint() {
        let base = "[cluster]\nid='test'\ntopology='single_node'\nadvertise_host='127.0.0.1'\n";
        let resolve = |suffix: &str| {
            toml::from_str::<ServerConfig>(&format!("{base}{suffix}"))
                .unwrap()
                .resolve(Some("state".into()))
        };
        let listen = "[cluster.gossip]\nlisten='127.0.0.1:7946'";
        assert_eq!(
            resolve(listen).await.unwrap().unwrap().topology,
            Topology::SingleNode
        );
        assert!(resolve("").await.is_err());
        assert!(
            resolve("[cluster.gossip]\nlisten='127.0.0.2:7946'")
                .await
                .is_err()
        );
        assert!(
            resolve(&format!("seeds=['127.0.0.2:7946']\n{listen}"))
                .await
                .is_err()
        );
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn ownership_peer_requires_an_exact_pin_and_multi_node_topology() {
        let local = NodeId::new();
        let remote = NodeId::new();
        let receipt_public = ApiSigningKey::from_seed([27; 32]).public_key();
        let config = |topology: &str, fingerprint: &str| {
            toml::from_str::<ServerConfig>(&format!(
                "[cluster]\nid='test'\ntopology='{topology}'\nadvertise_host='127.0.0.1'\n\
                 [cluster.gossip]\nlisten='127.0.0.1:7946'\n\
                 [cluster.ownership_peer]\nlisten='127.0.0.1:9000'\n\
                 local_node='{local}'\nlocal_generation=2\npeer_node='{remote}'\npeer_generation=2\n\
                 peer_endpoint='127.0.0.1:9001'\npeer_sha256='{fingerprint}'\n\
                 certificate_der='unused'\nkey_pkcs8_der='unused'\n\
                 authority_epoch='0123456789abcdef0123456789abcdef'\n\
                 receipt_key_file='unused'\npeer_receipt_key='{receipt_public}'"
            ))
            .unwrap()
            .resolve(Some("state".into()))
        };
        let valid = "aB".repeat(32);
        assert_eq!(parse_hex::<32>(&valid).unwrap(), [0xab; 32]);
        assert!(parse_hex::<32>(&"+a".repeat(32)).is_none());
        let err = config("multi_node", "invalid").await.unwrap_err();
        assert!(format!("{err:#}").contains("peer_sha256"));
        let err = config("single_node", &valid).await.unwrap_err();
        assert!(format!("{err:#}").contains("requires multi_node"));
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
        assert_eq!(config.topology, Topology::MultiNode);
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
