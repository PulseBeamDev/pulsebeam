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
/// itself; it calls `pulsebeam_routing::classify::classify_client_for_node` on
/// packet bytes. A bootstrap result is both returned to the caller *and*
/// cached, before anything has authenticated the source — see the security
/// note below for why that is safe.
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
/// shard owns it are separate concerns: `demux` only resolves, and the caller
/// compares `TransportHandle::shard` against its own before doing anything
/// with the result.
///
/// # Security hardening
///
/// The ufrag is a steering hint, not a credential. It is unauthenticated, it
/// names a route directly, and a forged one *does* get an entry here before
/// anything is verified. Three properties make that harmless, and they have to
/// be read together — `docs/routing.md` carries the same argument in prose.
///
/// **Admission grants nothing.** Steering decides which shard looks at a
/// packet, not whether it is honoured: the owning shard still resolves the
/// route in its published view, and str0m still runs ICE, DTLS and SRTP. A
/// forged ufrag buys parser work and a cache slot, nothing else.
///
/// **A flood cannot displace a live flow.** The cache **evicts rather than
/// refuses** — refusing when full would let an attacker lock out new legitimate
/// flows. Every cache *hit* refreshes `used`, so a participant sending media
/// holds the most recently used entry for its route while forged entries are
/// touched once at admission. Eviction takes the minimum, so a flood evicts its
/// own oldest entry and churns its own ring.
///
/// An authenticated entry is marked by the route owner and acknowledged back
/// to the shard that admitted the bootstrap. Authenticated entries are not
/// evicted while an unauthenticated entry for the same scope is available.
///
/// * **Total cache cap** (`MAX_ADDR_ENTRIES`): bounds the fast-path `addr_map`.
/// * **Per-route cap** (`MAX_ADDRS_PER_ROUTE`): bounds how many source
///   addresses one route (real or fabricated) can occupy, so no single route
///   monopolises the budget.
pub struct Demuxer {
    /// Fast-path cache: maps a known remote `SocketAddr` to a route.
    addr_map: HashMap<SocketAddr, CachedRoute>,
    /// Reverse: maps a route to all its known source addresses (for cleanup).
    route_addrs: HashMap<TransportRoute, ArrayVec<SocketAddr, MAX_ADDRS_PER_ROUTE>>,
    /// Monotonic stamp handed to each cache hit, so eviction can pick the
    /// least recently used entry rather than refusing to admit a new one.
    clock: u64,
    cluster_id: u16,
    node_id: u16,
    shard_count: u16,
}

#[derive(Clone, Copy)]
struct CachedRoute {
    handle: TransportHandle,
    authenticated: bool,
    /// `clock` at the last hit. A flood's entries are touched once; a live
    /// call's entry is touched continuously, which is what makes it survive.
    used: u64,
}

impl Demuxer {
    pub fn new() -> Self {
        Self::for_node(
            0,
            0,
            u16::try_from(pulsebeam_routing::steer::MAX_SHARDS).unwrap_or(u16::MAX),
        )
    }

    pub fn for_node(cluster_id: u16, node_id: u16, shard_count: u16) -> Self {
        debug_assert!(shard_count > 0);
        Self {
            addr_map: HashMap::new(),
            route_addrs: HashMap::new(),
            clock: 0,
            cluster_id,
            node_id,
            shard_count,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Evict the least recently used entry belonging to `route`.
    ///
    /// Bounded by [`MAX_ADDRS_PER_ROUTE`], which is a `const` 16 — this is the
    /// one scan in the packet path, and it runs only when a route's address
    /// list is already full.
    fn evict_lru_for_route(&mut self, route: TransportRoute) -> Option<SocketAddr> {
        let addrs = self.route_addrs.get(&route)?;
        let mut victim: Option<(SocketAddr, bool, u64)> = None;
        for addr in addrs {
            let Some(entry) = self.addr_map.get(addr) else {
                debug_assert!(false, "route address index must point into the cache");
                continue;
            };
            if victim.is_none_or(|(_, authenticated, used)| {
                (!entry.authenticated && authenticated)
                    || (entry.authenticated == authenticated && entry.used < used)
            }) {
                victim = Some((*addr, entry.authenticated, entry.used));
            }
        }
        let (addr, _, _) = victim?;
        self.addr_map.remove(&addr);
        if let Some(addrs) = self.route_addrs.get_mut(&route) {
            addrs.retain(|cached| *cached != addr);
        }
        Some(addr)
    }

    /// Evict the least recently used entry anywhere.
    ///
    /// Only reached when the global cache is full, which on a healthy node
    /// never happens — it is the flood path, and the point is that a flood
    /// degrades into churn instead of locking legitimate clients out.
    fn evict_lru_global(&mut self) {
        let mut victim: Option<(SocketAddr, TransportRoute, bool, u64)> = None;
        for (addr, entry) in &self.addr_map {
            if victim.is_none_or(|(_, _, authenticated, used)| {
                (!entry.authenticated && authenticated)
                    || (entry.authenticated == authenticated && entry.used < used)
            }) {
                victim = Some((*addr, entry.handle.route, entry.authenticated, entry.used));
            }
        }
        let Some((addr, route, _, _)) = victim else {
            return;
        };
        self.addr_map.remove(&addr);
        if let Some(addrs) = self.route_addrs.get_mut(&route) {
            addrs.retain(|cached| *cached != addr);
        }
        metrics::counter!("demux_addr_cache_evicted").increment(1);
    }

    /// Removes a route and all associated address-cache entries.
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
    /// The caller compares the returned shard against its own.
    pub fn demux(&mut self, batch: &net::RecvPacketBatch) -> Option<TransportHandle> {
        let src = batch.src;

        let stamp = self.tick();
        if let pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } =
            pulsebeam_routing::classify::classify_client_for_node(
                batch.data(),
                self.cluster_id,
                self.node_id,
                self.shard_count,
            )
        {
            let addressed = to_local_handle(handle);
            if self
                .addr_map
                .get(&src)
                .is_some_and(|entry| entry.handle == addressed)
            {
                if let Some(entry) = self.addr_map.get_mut(&src) {
                    entry.used = stamp;
                }
                return Some(addressed);
            }
            self.forget(src);
            self.admit(src, addressed, stamp);
            return Some(addressed);
        }

        if let Some(entry) = self.addr_map.get_mut(&src) {
            entry.used = stamp;
            return Some(entry.handle);
        }

        // Slow path: classify the raw bytes through the shared no_std
        // classifier — the same one the eBPF program and simulator use.
        let handle = match pulsebeam_routing::classify::classify_client_for_node(
            batch.data(),
            self.cluster_id,
            self.node_id,
            self.shard_count,
        ) {
            pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } => handle,
            pulsebeam_routing::classify::ClientVerdict::Established
            | pulsebeam_routing::classify::ClientVerdict::Drop(_) => return None,
        };
        let addressed = to_local_handle(handle);
        self.admit(src, addressed, stamp);
        Some(addressed)
    }

    /// Cache an address another shard classified, so this one can route the
    /// flow directly once steering starts delivering it here.
    pub fn learn(&mut self, src: SocketAddr, handle: TransportHandle) {
        let stamp = self.tick();
        match self.addr_map.get_mut(&src) {
            Some(entry) if entry.handle.route == handle.route => {
                if entry.handle.epoch != handle.epoch {
                    entry.authenticated = false;
                }
                entry.used = stamp;
                entry.handle = handle;
            }
            // A rebind of the same address onto a different route has to leave
            // the old route's address list, or unregistering that route later
            // would evict an entry it no longer owns.
            Some(_) => {
                self.forget(src);
                self.admit(src, handle, stamp);
            }
            None => self.admit(src, handle, stamp),
        }
    }

    /// Marks a cached source address as authenticated for this exact route.
    ///
    /// The route owner calls this locally and the controller sends the same
    /// fact to the shard whose tuple hash admitted the bootstrap. A missing
    /// entry is repaired as an authenticated admission: the entry may have
    /// been evicted before ICE completed, but the owner has already verified
    /// this address for this route.
    pub fn authenticate(&mut self, src: SocketAddr, handle: TransportHandle) {
        let stamp = self.tick();
        match self.addr_map.get_mut(&src) {
            Some(entry) if entry.handle == handle => {
                entry.authenticated = true;
                entry.used = stamp;
            }
            Some(_) => {
                self.forget(src);
                self.admit_authenticated(src, handle, stamp);
            }
            None => self.admit_authenticated(src, handle, stamp),
        }
    }

    fn forget(&mut self, src: SocketAddr) {
        let Some(entry) = self.addr_map.remove(&src) else {
            return;
        };
        if let Some(addrs) = self.route_addrs.get_mut(&entry.handle.route) {
            addrs.retain(|cached| *cached != src);
        }
    }

    /// Cache `src -> handle`, evicting to make room rather than refusing.
    ///
    /// Admission is not conditional on this shard owning the route, and that is
    /// the point: `SO_REUSEPORT` hashes the 4-tuple, so the shard that sees a
    /// flow's STUN sees its DTLS and its media too. Caching here is what lets a
    /// shard keep forwarding a flow it does not own — the rest of a handshake
    /// carries no ufrag, so an uncached address is an undeliverable one.
    fn admit(&mut self, src: SocketAddr, handle: TransportHandle, stamp: u64) {
        self.admit_with_authentication(src, handle, stamp, false);
    }

    fn admit_authenticated(&mut self, src: SocketAddr, handle: TransportHandle, stamp: u64) {
        self.admit_with_authentication(src, handle, stamp, true);
    }

    fn admit_with_authentication(
        &mut self,
        src: SocketAddr,
        handle: TransportHandle,
        stamp: u64,
        authenticated: bool,
    ) {
        if self.addr_map.len() >= MAX_ADDR_ENTRIES {
            self.evict_lru_global();
        }
        if self
            .route_addrs
            .get(&handle.route)
            .is_some_and(|addrs| addrs.len() >= MAX_ADDRS_PER_ROUTE)
        {
            self.evict_lru_for_route(handle.route);
            metrics::counter!("demux_route_addrs_evicted").increment(1);
        }

        let addrs = self.route_addrs.entry(handle.route).or_default();
        if addrs.len() >= MAX_ADDRS_PER_ROUTE {
            debug_assert!(false, "eviction must free a slot before admission");
            return;
        }
        addrs.push(src);
        self.addr_map.insert(
            src,
            CachedRoute {
                handle,
                authenticated,
                used: stamp,
            },
        );
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

    /// A flood of fabricated ufrags naming one route must not lock the real
    /// client out of that route.
    ///
    /// The caches bound memory, and a full table used to mean "stop caching".
    /// That made filling it a denial of service: an uncached source address
    /// has its non-STUN traffic dropped (`demux` returns `None` for
    /// `Established`), so 16 spoofed packets naming a predictable route were
    /// enough to sink a participant's media. Routes are enumerable — slots
    /// issue sequentially from 0 and epochs start at 0 — so this needed no
    /// knowledge of the victim at all.
    ///
    /// Eviction is still the answer rather than gating admission on
    /// authentication: the forwarding shard must cache the bootstrap before
    /// the owner can send its authenticated acknowledgment, and it must route
    /// the DTLS that follows the bootstrap while that acknowledgment is in
    /// flight.
    #[test]
    fn a_flood_on_one_route_cannot_lock_out_its_real_client() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(3, 1);
        let encoded = ice.encode();

        // The attacker fills every per-route address slot.
        for port in 0..u16::try_from(MAX_ADDRS_PER_ROUTE).expect("cap fits a u16") {
            let batch = make_batch(src(40000 + port), stun_with_ufrag(&encoded));
            assert_eq!(d.demux(&batch), Some(handle));
        }

        // The legitimate client arrives afterwards and must still be admitted.
        let victim = src(1000);
        let batch = make_batch(victim, stun_with_ufrag(&encoded));
        assert_eq!(d.demux(&batch), Some(handle));
        assert!(
            d.addr_map.contains_key(&victim),
            "the real client must be cached; an uncached address has its media dropped"
        );

        // And its cache entry must survive continued attacker churn, because
        // it is the one being used.
        for port in 0..64u16 {
            let noise = make_batch(src(50000 + port), stun_with_ufrag(&encoded));
            let _ = d.demux(&noise);
            let _ = d.demux(&make_batch(victim, stun_with_ufrag(&encoded)));
        }
        assert!(
            d.addr_map.contains_key(&victim),
            "an actively used entry must not be evicted by single-touch flood entries"
        );
    }

    /// A shard that does not own a route must still be able to route the rest
    /// of that flow's handshake.
    ///
    /// `SO_REUSEPORT` hashes the 4-tuple, so a flow's STUN and its DTLS land on
    /// the same shard whether or not that shard owns the route. Caching on
    /// classification is what makes the second one deliverable: DTLS carries no
    /// ufrag, so an uncached address has nothing to classify and is dropped.
    ///
    /// This is why admission must not be gated on authentication. The
    /// forwarding shard must carry the handshake until the owner can send its
    /// acknowledgment back.
    #[test]
    fn a_shard_that_does_not_own_a_route_still_routes_the_rest_of_the_flow() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(3, 1);
        let client = src(1234);

        // Bootstrap: classifiable, and resolves to a route this shard does not
        // own. Nothing here says "mine".
        let bootstrap = make_batch(client, stun_with_ufrag(&ice.encode()));
        assert_eq!(d.demux(&bootstrap), Some(handle));

        // Everything after it carries no ufrag. It must still resolve while the
        // owner-side authentication acknowledgment is in flight.
        let dtls = make_batch(
            client,
            std::vec![0x16, 0xfe, 0xfd, 0x00, 0x01, 0x02, 0x03, 0x04],
        );
        assert_eq!(
            d.demux(&dtls),
            Some(handle),
            "a non-STUN packet on a known flow must resolve on the shard that saw its bootstrap"
        );

        let media = make_batch(
            client,
            std::vec![0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
        );
        assert_eq!(d.demux(&media), Some(handle));
    }

    #[test]
    fn authenticated_entries_survive_unauthenticated_route_churn() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(3, 1);
        let client = src(1234);

        assert_eq!(
            d.demux(&make_batch(client, stun_with_ufrag(&ice.encode()))),
            Some(handle)
        );
        d.authenticate(client, handle);
        assert!(
            d.addr_map
                .get(&client)
                .is_some_and(|entry| entry.authenticated)
        );

        for port in 40000..(40000 + u16::try_from(MAX_ADDRS_PER_ROUTE * 3).unwrap()) {
            let noise = src(port);
            let _ = d.demux(&make_batch(noise, stun_with_ufrag(&ice.encode())));
        }

        assert!(
            d.addr_map
                .get(&client)
                .is_some_and(|entry| entry.authenticated),
            "the sender shard must protect the acknowledged flow from route-local churn"
        );
    }

    #[test]
    fn learning_a_new_epoch_clears_the_old_authentication() {
        let mut d = Demuxer::new();
        let (first_ice, first) = ufrag(3, 1);
        let second = TransportHandle::new(first.route, first.epoch + 1);
        let client = src(1234);

        assert_eq!(
            d.demux(&make_batch(client, stun_with_ufrag(&first_ice.encode()))),
            Some(first)
        );
        d.authenticate(client, first);
        d.learn(client, second);

        assert_eq!(
            d.addr_map.get(&client).map(|entry| entry.handle),
            Some(second)
        );
        assert!(
            !d.addr_map
                .get(&client)
                .is_some_and(|entry| entry.authenticated)
        );
    }

    /// The owner of a route must be able to route a flow it only ever saw
    /// forwarded.
    ///
    /// Steering is a cache, and populating it hands a flow over from the shard
    /// the tuple hash picked to the shard that owns the route. The owner has
    /// never classified that flow — the bootstrap went to the other shard — so
    /// without learning the address while forwarding is still happening, the
    /// first datagram to arrive directly is unclassifiable and the handover
    /// silently drops media until the next consent check repairs it.
    #[test]
    fn an_owner_routes_a_flow_it_only_saw_forwarded() {
        let mut owner = Demuxer::new();
        let (_, handle) = ufrag(3, 1);
        let client = src(1234);

        let media = make_batch(
            client,
            std::vec![0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
        );
        assert_eq!(
            owner.demux(&media),
            None,
            "an address this shard has never seen resolves to nothing"
        );

        owner.learn(client, handle);

        assert_eq!(
            owner.demux(&media),
            Some(handle),
            "once steering delivers the flow here directly, it must resolve"
        );
    }

    /// Learning the same address onto a different route must leave the old
    /// route's address list, or tearing that route down would evict an entry
    /// it no longer owns.
    #[test]
    fn relearning_an_address_moves_it_off_the_old_route() {
        let mut d = Demuxer::new();
        let (_, first) = ufrag(3, 1);
        let (_, second) = ufrag(3, 2);
        let client = src(1234);

        d.learn(client, first);
        d.learn(client, second);

        d.unregister(first.route);
        let media = make_batch(
            client,
            std::vec![0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
        );
        assert_eq!(
            d.demux(&media),
            Some(second),
            "unregistering the abandoned route must not evict the live one"
        );
    }

    #[test]
    fn a_reconnect_reclassifies_an_address_cached_for_the_previous_route() {
        let mut d = Demuxer::new();
        let (first_ice, first) = ufrag(3, 1);
        let (second_ice, second) = ufrag(3, 2);
        let client = src(1234);

        assert_eq!(
            d.demux(&make_batch(client, stun_with_ufrag(&first_ice.encode()))),
            Some(first)
        );
        d.authenticate(client, first);

        assert_eq!(
            d.demux(&make_batch(client, stun_with_ufrag(&second_ice.encode()))),
            Some(second)
        );
        assert_eq!(
            d.addr_map.get(&client).map(|entry| entry.handle),
            Some(second)
        );
        assert!(
            !d.addr_map
                .get(&client)
                .is_some_and(|entry| entry.authenticated)
        );
    }

    /// The same attack aimed at the whole node rather than one route.
    #[test]
    fn a_global_flood_still_admits_a_new_client() {
        let mut d = Demuxer::new();
        let (protected_ice, protected_handle) = ufrag(3, 9999);
        let protected = src(1234);
        assert_eq!(
            d.demux(&make_batch(
                protected,
                stun_with_ufrag(&protected_ice.encode()),
            )),
            Some(protected_handle)
        );
        d.authenticate(protected, protected_handle);

        // Saturate the global budget across many routes. The flood lives on
        // 10.x.x.x so it cannot collide with the newcomer's address.
        fn flood_src(n: usize) -> SocketAddr {
            let n = u32::try_from(n).expect("flood index fits a u32");
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(0x0A00_0000u32.wrapping_add(n))),
                4000,
            )
        }
        let mut i = 0usize;
        for slot in 0..u32::try_from(MAX_ADDR_ENTRIES / MAX_ADDRS_PER_ROUTE)
            .expect("route count fits a u32")
        {
            let (ice, _) = ufrag(0, slot);
            let encoded = ice.encode();
            for _ in 0..MAX_ADDRS_PER_ROUTE {
                let batch = make_batch(flood_src(i), stun_with_ufrag(&encoded));
                let _ = d.demux(&batch);
                i = i.wrapping_add(1);
            }
        }
        assert_eq!(d.addr_map.len(), MAX_ADDR_ENTRIES);

        let (ice, handle) = ufrag(1, 999);
        let newcomer = src(65000);
        let batch = make_batch(newcomer, stun_with_ufrag(&ice.encode()));
        assert_eq!(d.demux(&batch), Some(handle));
        assert!(
            d.addr_map.contains_key(&newcomer),
            "a saturated cache must evict, not refuse: refusing drops the new client's media"
        );
        assert!(
            d.addr_map.len() <= MAX_ADDR_ENTRIES,
            "eviction must keep the cache within its bound"
        );
        assert!(
            d.addr_map
                .get(&protected)
                .is_some_and(|entry| entry.authenticated),
            "global churn must not evict an acknowledged flow before unauthenticated entries"
        );
    }

    #[test]
    fn valid_ufrag_matching_shard_routes_and_caches() {
        let mut d = Demuxer::new();
        let (ice, handle) = ufrag(3, 1);
        let encoded = ice.encode();
        let batch = make_batch(src(1000), stun_with_ufrag(&encoded));

        assert_eq!(d.demux(&batch), Some(handle));
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
