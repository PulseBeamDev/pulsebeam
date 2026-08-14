use arrayvec::ArrayVec;
use pulsebeam_runtime::net;

use crate::route::{TransportHandle, TransportRoute};

use ahash::{HashMap, HashMapExt};
use std::net::SocketAddr;

/// Maximum number of distinct source addresses cached for a single route.
/// Prevents one forged ufrag from consuming the entire addr_map budget.
const MAX_ADDRS_PER_ROUTE: usize = 16;

/// Hard upper bound on the total number of (src_addr → route) cache entries.
/// Prevents memory exhaustion from a flood of STUN packets with fabricated ufrags.
const MAX_ADDR_ENTRIES: usize = MAX_ADDRS_PER_ROUTE * 4096;

/// A UDP demuxer that maps packets to routes based on source address and STUN ufrag.
///
/// This is a validation/cache layer on top of `pulsebeam-routing`'s shared,
/// `no_std` classifier — the same classifier the Aya eBPF program and the
/// simulator's steering adapter use. `Demuxer` never parses STUN or the ufrag
/// itself; it calls `pulsebeam_routing::classify::classify_client` on the
/// packet bytes and caches the result by source address so repeat packets
/// skip the parse.
///
/// Resolving a route does not imply this shard owns it — see the note on
/// cross-shard arrivals below.
///
/// Routing uses two mechanisms:
/// 1. A fast-path map from `SocketAddr` to a `TransportHandle` (`addr_map`): efficient
///    routing for known addresses (DTLS, RTP, RTCP).
/// 2. For STUN packets from an unknown address, the shared classifier decodes
///    the ufrag in the USERNAME attribute to read `(route, epoch)` directly —
///    no registration or lookup table is required, and no semantic id is
///    ever hashed to get there.
///
/// Non-STUN packets from unknown addresses are rejected.
///
/// A ufrag naming another shard is *not* rejected here. `SO_REUSEPORT` picks
/// the receiving socket by hashing the 4-tuple, which has nothing to do with
/// which shard owns the route, so arriving on the wrong one is ordinary
/// rather than suspicious. Resolving the route and deciding whether this
/// shard owns it are separate concerns: `demux` only resolves; the caller
/// checks ownership (see [`Demuxer::owns`]) and decides whether to forward,
/// process, or drop.
///
/// # Security hardening
///
/// * **Total cache cap** (`MAX_ADDR_ENTRIES`): the fast-path `addr_map` is bounded.
///   Once full, packets are still decoded and forwarded but the source address is not
///   cached, limiting memory under a flood of distinct source IPs.
/// * **Per-route cap** (`MAX_ADDRS_PER_ROUTE`): limits how many source
///   addresses a single route (real or fabricated) can occupy in the cache,
///   preventing one route from monopolising the budget.
pub struct Demuxer {
    /// Fast-path cache: maps a known remote `SocketAddr` to a route.
    addr_map: HashMap<SocketAddr, TransportHandle>,
    /// Reverse: maps a route to all its known source addresses (for cleanup).
    route_addrs: HashMap<TransportRoute, ArrayVec<SocketAddr, MAX_ADDRS_PER_ROUTE>>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            addr_map: HashMap::new(),
            route_addrs: HashMap::new(),
        }
    }

    /// Removes a route and all associated address-cache entries.
    /// Returns the previously-cached addresses (used to close TCP connections).
    pub fn unregister(&mut self, route: TransportRoute) -> Vec<SocketAddr> {
        if let Some(addrs) = self.route_addrs.remove(&route) {
            for addr in &addrs {
                self.addr_map.remove(addr);
            }
            addrs.into_iter().collect()
        } else {
            vec![]
        }
    }

    /// Routes a packet to the transport association it addresses.
    /// Returns `None` if dropped.
    ///
    /// This resolves a route; it does not decide whether this shard owns it.
    /// See [`Demuxer::owns`].
    pub fn demux(&mut self, batch: &net::RecvPacketBatch) -> Option<TransportHandle> {
        let src = batch.src;

        if let Some(&addressed) = self.addr_map.get(&src) {
            return Some(addressed);
        }

        // Slow path: classify the raw bytes through the shared no_std
        // classifier — the same one the eBPF program and simulator use — and
        // cache the resolved (route, epoch) by source address.
        let handle = match pulsebeam_routing::classify::classify_client(batch.data()) {
            pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } => handle,
            pulsebeam_routing::classify::ClientVerdict::Established
            | pulsebeam_routing::classify::ClientVerdict::Drop(_) => return None,
        };
        let addressed = to_local_handle(handle);

        // Populate the fast-path cache only when within the safety bounds, to
        // prevent memory exhaustion from floods of distinct fabricated source IPs.
        if self.addr_map.len() < MAX_ADDR_ENTRIES {
            let route_entry = self.route_addrs.entry(addressed.route).or_default();
            if route_entry.len() < MAX_ADDRS_PER_ROUTE {
                debug_assert!(route_entry.len() < MAX_ADDRS_PER_ROUTE);
                route_entry.push(src);
                self.addr_map.insert(src, addressed);
            }
        }

        Some(addressed)
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a `pulsebeam-routing` transport handle — a bare `u16`-shard
/// `TransportRoute` — into `pulsebeam`'s own `TransportRoute`, which is over
/// `ShardId`. Both wrap the same `shard(12) | slot(20)` bit layout, so the
/// conversion is a raw round trip through `get()`/`from_raw()`.
fn to_local_handle(handle: pulsebeam_routing::TransportHandle) -> TransportHandle {
    let route = TransportRoute::from_raw(handle.route.get());
    debug_assert_eq!(
        route.shard().index(),
        usize::from(handle.route.shard()),
        "shard must survive the pulsebeam-routing <-> pulsebeam TransportRoute conversion"
    );
    TransportHandle::new(route, handle.epoch)
}

/// Extract the server-side ICE ufrag (the first token before `:` in the STUN
/// USERNAME attribute) from a raw STUN binding-request payload.
///
/// Returns `None` if `data` does not look like a valid STUN message or does not
/// carry a USERNAME attribute.
pub(crate) fn extract_stun_server_ufrag(data: &[u8]) -> Option<String> {
    pulsebeam_routing::stun::server_ufrag(data)
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod demux_tests {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::{control::ufrag::IceUfrag, id::ShardId};
    use pulsebeam_runtime::net::{RecvPacketBatch, Transport};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // ── STUN helpers ─────────────────────────────────────────────────────────

    const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
    const BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
    const USERNAME_TYPE: [u8; 2] = [0x00, 0x06];

    /// Build a minimal STUN Binding-Request carrying a USERNAME attribute whose
    /// value is `"{server_ufrag}:client"`.
    fn stun_with_ufrag(server_ufrag: &str) -> Vec<u8> {
        let username = format!("{server_ufrag}:client");
        let value = username.as_bytes();
        let value_len = value.len();
        let padded_len = (value_len + 3) & !3;
        // attr header (4) + padded value
        let attr_total = 4 + padded_len;

        let mut buf = Vec::with_capacity(20 + attr_total);
        buf.extend_from_slice(&BINDING_REQUEST);
        buf.extend_from_slice(
            &u16::try_from(attr_total)
                .expect("fixture attr total fits")
                .to_be_bytes(),
        ); // message length
        buf.extend_from_slice(&MAGIC_COOKIE);
        buf.extend_from_slice(&[0u8; 12]); // transaction ID
        // USERNAME attribute
        buf.extend_from_slice(&USERNAME_TYPE);
        buf.extend_from_slice(
            &u16::try_from(value_len)
                .expect("fixture value fits")
                .to_be_bytes(),
        );
        buf.extend_from_slice(value);
        buf.extend_from_slice(&vec![0u8; padded_len - value_len]); // padding
        buf
    }

    fn make_batch(src: SocketAddr, data: Vec<u8>) -> RecvPacketBatch {
        let len = data.len();
        RecvPacketBatch {
            src,
            dst: "0.0.0.0:0".parse().unwrap(),
            buf: data,
            stride: len,
            len,
            transport: Transport::Udp(pulsebeam_runtime::net::UdpMode::Scalar),
            offset: 0,
        }
    }

    fn src(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), port)
    }

    fn ufrag(shard: usize, slot: u32) -> (IceUfrag, TransportHandle) {
        let route = TransportRoute::new(ShardId::new(shard), slot);
        let epoch = 7;
        (
            IceUfrag::new(0, 0, route, epoch),
            TransportHandle::new(route, epoch),
        )
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn valid_ufrag_matching_shard_routes_and_caches() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(3, 1);
        let encoded = ice.encode();
        let batch = make_batch(src(1000), stun_with_ufrag(&encoded));

        assert_eq!(d.demux(&batch), Some(handle));
        // Fast-path entry created
        assert_eq!(d.addr_map.len(), 1);
        // Second packet uses fast path
        assert_eq!(d.demux(&batch), Some(handle));
        assert_eq!(d.addr_map.len(), 1); // no duplicate
    }

    /// A ufrag for another shard resolves rather than being dropped:
    /// resolving a route and deciding whether this shard owns it are
    /// separate concerns. Which socket steering chose says nothing about
    /// which shard owns the route, so `demux` decodes it either way; the
    /// caller compares `handle.shard()` against its own and drops a
    /// misdelivery.
    #[test]
    fn a_ufrag_for_another_shard_still_resolves_so_the_caller_can_drop_it() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(4, 2);
        let batch = make_batch(src(1000), stun_with_ufrag(&ice.encode()));

        assert_eq!(d.demux(&batch), Some(handle));
        assert_eq!(
            handle.shard(),
            ShardId::new(4),
            "the route names its owner, which is all the caller needs"
        );
    }

    #[test]
    fn oversized_ufrag_is_dropped() {
        let mut d = Demuxer::new();
        // one byte over the encoded length
        let oversized = "A".repeat(IceUfrag::ENCODED_LEN + 1);
        let batch = make_batch(src(1000), stun_with_ufrag(&oversized));
        assert_eq!(d.demux(&batch), None);
        assert!(d.addr_map.is_empty());
    }

    #[test]
    fn garbage_ufrag_is_dropped() {
        let mut d = Demuxer::new();
        let batch = make_batch(src(1000), stun_with_ufrag("notavalidufrag!"));
        assert_eq!(d.demux(&batch), None);
        assert!(d.addr_map.is_empty());
    }

    #[test]
    fn non_stun_from_unknown_addr_is_dropped() {
        let mut d = Demuxer::new();
        let batch = make_batch(src(1000), b"RTP not STUN".to_vec());
        assert_eq!(d.demux(&batch), None);
    }

    #[test]
    fn per_route_addr_cap_limits_cache_growth() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(0, 0);
        let encoded = ice.encode();

        // Fill up to the cap
        for port in 0..u16::try_from(MAX_ADDRS_PER_ROUTE).expect("addr cap fits a u16") {
            let batch = make_batch(src(port), stun_with_ufrag(&encoded));
            assert_eq!(d.demux(&batch), Some(handle), "port {port} should route");
        }
        assert_eq!(d.addr_map.len(), MAX_ADDRS_PER_ROUTE);

        // One more distinct source address: must still ROUTE but must NOT cache
        let extra = make_batch(src(9999), stun_with_ufrag(&encoded));
        assert_eq!(d.demux(&extra), Some(handle), "must still route after cap");
        assert_eq!(d.addr_map.len(), MAX_ADDRS_PER_ROUTE, "cache must not grow");
    }

    #[test]
    fn unregister_clears_all_cached_addrs() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(0, 0);
        let encoded = ice.encode();

        for port in 0..4u16 {
            let batch = make_batch(src(port), stun_with_ufrag(&encoded));
            d.demux(&batch);
        }
        assert_eq!(d.addr_map.len(), 4);

        let freed = d.unregister(handle.route);
        assert_eq!(freed.len(), 4);
        assert!(d.addr_map.is_empty());
        assert!(d.route_addrs.is_empty());
    }

    /// Phase-9 acceptance criterion: the shared classifier and this userspace
    /// path must make the *same* decision for the same bytes — the kernel
    /// parser is an optimization boundary, not a second security model.
    #[test]
    fn userspace_demux_agrees_with_the_shared_classifier() {
        let (ice, handle) = ufrag(5, 42);
        let bytes = stun_with_ufrag(&ice.encode());

        let mut d = Demuxer::new();
        let batch = make_batch(src(1000), bytes.clone());
        let userspace_result = d.demux(&batch);

        let shared_result = match pulsebeam_routing::classify::classify_client(&bytes) {
            pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } => {
                Some(to_local_handle(handle))
            }
            _ => None,
        };

        assert_eq!(userspace_result, shared_result);
        assert_eq!(userspace_result, Some(handle));
    }

    #[test]
    fn userspace_demux_agrees_with_the_shared_classifier_on_drops() {
        for bytes in [
            b"RTP not STUN".to_vec(),
            stun_with_ufrag("notavalidufrag!"),
            stun_with_ufrag(&"A".repeat(IceUfrag::ENCODED_LEN + 1)),
        ] {
            let mut d = Demuxer::new();
            let batch = make_batch(src(2000), bytes.clone());
            assert_eq!(d.demux(&batch), None);
            assert!(matches!(
                pulsebeam_routing::classify::classify_client(&bytes),
                pulsebeam_routing::classify::ClientVerdict::Drop(_)
                    | pulsebeam_routing::classify::ClientVerdict::Established
            ));
        }
    }
}
