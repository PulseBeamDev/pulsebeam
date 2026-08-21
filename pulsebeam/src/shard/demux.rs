use arrayvec::ArrayVec;
use pulsebeam_runtime::net;

use crate::route::{TransportHandle, TransportRoute};

use ahash::{HashMap, HashMapExt};
use std::net::SocketAddr;

const MAX_ADDRS_PER_ROUTE: usize = 16;
const MAX_ADDR_ENTRIES: usize = MAX_ADDRS_PER_ROUTE * 4096;

#[derive(Debug, Clone, Copy, Default)]
struct LruList {
    head: Option<SocketAddr>,
    tail: Option<SocketAddr>,
    len: usize,
}

#[derive(Debug, Clone, Copy)]
struct CachedRoute {
    handle: TransportHandle,
    authenticated: bool,
    global_prev: Option<SocketAddr>,
    global_next: Option<SocketAddr>,
    route_prev: Option<SocketAddr>,
    route_next: Option<SocketAddr>,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
struct DemuxWork {
    stun_sniffs: usize,
    stun_parses: usize,
    cache_entries_examined: usize,
}

pub struct Demuxer {
    addr_map: HashMap<SocketAddr, CachedRoute>,
    route_addrs: HashMap<TransportRoute, RouteLists>,
    unauthenticated: LruList,
    authenticated: LruList,
    cluster_id: u16,
    node_id: u16,
    shard_count: u16,
    #[cfg(test)]
    work: DemuxWork,
}

#[derive(Debug, Clone, Copy, Default)]
struct RouteLists {
    unauthenticated: LruList,
    authenticated: LruList,
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
            unauthenticated: LruList::default(),
            authenticated: LruList::default(),
            cluster_id,
            node_id,
            shard_count,
            #[cfg(test)]
            work: DemuxWork::default(),
        }
    }

    fn list_mut(&mut self, authenticated: bool) -> &mut LruList {
        if authenticated {
            &mut self.authenticated
        } else {
            &mut self.unauthenticated
        }
    }

    fn route_list_mut(&mut self, route: TransportRoute, authenticated: bool) -> &mut LruList {
        let lists = self.route_addrs.entry(route).or_default();
        if authenticated {
            &mut lists.authenticated
        } else {
            &mut lists.unauthenticated
        }
    }

    fn unlink_global(&mut self, addr: SocketAddr) {
        let Some(entry) = self.addr_map.get(&addr).copied() else {
            debug_assert!(false, "the global LRU must point into the cache");
            return;
        };
        let list = self.list_mut(entry.authenticated);
        if list.head == Some(addr) {
            list.head = entry.global_next;
        }
        if list.tail == Some(addr) {
            list.tail = entry.global_prev;
        }
        list.len = list.len.saturating_sub(1);
        if let Some(prev) = entry.global_prev {
            if let Some(previous) = self.addr_map.get_mut(&prev) {
                previous.global_next = entry.global_next;
            } else {
                debug_assert!(false, "global LRU predecessor must be cached");
            }
        }
        if let Some(next) = entry.global_next {
            if let Some(next_entry) = self.addr_map.get_mut(&next) {
                next_entry.global_prev = entry.global_prev;
            } else {
                debug_assert!(false, "global LRU successor must be cached");
            }
        }
        if let Some(current) = self.addr_map.get_mut(&addr) {
            current.global_prev = None;
            current.global_next = None;
        }
    }

    fn link_global_front(&mut self, addr: SocketAddr, authenticated: bool) {
        let head = self.list_mut(authenticated).head;
        {
            let Some(entry) = self.addr_map.get_mut(&addr) else {
                debug_assert!(false, "a cached entry must exist before LRU linking");
                return;
            };
            entry.global_prev = None;
            entry.global_next = head;
        }
        if let Some(head) = head {
            if let Some(entry) = self.addr_map.get_mut(&head) {
                entry.global_prev = Some(addr);
            } else {
                debug_assert!(false, "global LRU head must be cached");
            }
        }
        let list = self.list_mut(authenticated);
        list.head = Some(addr);
        if list.tail.is_none() {
            list.tail = Some(addr);
        }
        list.len = list.len.saturating_add(1);
    }

    fn unlink_route(&mut self, addr: SocketAddr, route: TransportRoute) {
        let Some(entry) = self.addr_map.get(&addr).copied() else {
            debug_assert!(false, "the route LRU must point into the cache");
            return;
        };
        let list = self.route_list_mut(route, entry.authenticated);
        if list.head == Some(addr) {
            list.head = entry.route_next;
        }
        if list.tail == Some(addr) {
            list.tail = entry.route_prev;
        }
        list.len = list.len.saturating_sub(1);
        if let Some(prev) = entry.route_prev {
            if let Some(previous) = self.addr_map.get_mut(&prev) {
                previous.route_next = entry.route_next;
            } else {
                debug_assert!(false, "route LRU predecessor must be cached");
            }
        }
        if let Some(next) = entry.route_next {
            if let Some(next_entry) = self.addr_map.get_mut(&next) {
                next_entry.route_prev = entry.route_prev;
            } else {
                debug_assert!(false, "route LRU successor must be cached");
            }
        }
        if let Some(current) = self.addr_map.get_mut(&addr) {
            current.route_prev = None;
            current.route_next = None;
        }
        if self
            .route_addrs
            .get(&route)
            .is_some_and(|lists| lists.unauthenticated.len == 0 && lists.authenticated.len == 0)
        {
            self.route_addrs.remove(&route);
        }
    }

    fn link_route_front(&mut self, addr: SocketAddr, route: TransportRoute, authenticated: bool) {
        let head = self.route_list_mut(route, authenticated).head;
        {
            let Some(entry) = self.addr_map.get_mut(&addr) else {
                debug_assert!(false, "a cached entry must exist before route linking");
                return;
            };
            entry.route_prev = None;
            entry.route_next = head;
        }
        if let Some(head) = head {
            if let Some(entry) = self.addr_map.get_mut(&head) {
                entry.route_prev = Some(addr);
            } else {
                debug_assert!(false, "route LRU head must be cached");
            }
        }
        let list = self.route_list_mut(route, authenticated);
        list.head = Some(addr);
        if list.tail.is_none() {
            list.tail = Some(addr);
        }
        list.len = list.len.saturating_add(1);
    }

    fn touch(&mut self, addr: SocketAddr) {
        let Some(entry) = self.addr_map.get(&addr).copied() else {
            return;
        };
        self.unlink_global(addr);
        self.unlink_route(addr, entry.handle.route);
        self.link_global_front(addr, entry.authenticated);
        self.link_route_front(addr, entry.handle.route, entry.authenticated);
    }

    fn remove_entry(&mut self, addr: SocketAddr) -> Option<CachedRoute> {
        let entry = self.addr_map.get(&addr).copied()?;
        self.unlink_global(addr);
        self.unlink_route(addr, entry.handle.route);
        let removed = self.addr_map.remove(&addr);
        debug_assert!(removed.is_some());
        removed
    }

    fn evict_global(&mut self) {
        let victim = self.unauthenticated.tail.or(self.authenticated.tail);
        let Some(victim) = victim else {
            debug_assert!(self.addr_map.is_empty());
            return;
        };
        #[cfg(test)]
        {
            self.work.cache_entries_examined = self.work.cache_entries_examined.saturating_add(1);
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("cache_entries_examined", 1);
        let _ = self.remove_entry(victim);
        metrics::counter!("demux_addr_cache_evicted").increment(1);
    }

    fn evict_route(&mut self, route: TransportRoute) {
        let victim = self
            .route_addrs
            .get(&route)
            .and_then(|lists| lists.unauthenticated.tail.or(lists.authenticated.tail));
        let Some(victim) = victim else {
            debug_assert!(false, "a full route cache must have an entry");
            return;
        };
        #[cfg(test)]
        {
            self.work.cache_entries_examined = self.work.cache_entries_examined.saturating_add(1);
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("cache_entries_examined", 1);
        let _ = self.remove_entry(victim);
        metrics::counter!("demux_route_addrs_evicted").increment(1);
    }

    pub fn unregister(&mut self, route: TransportRoute) -> Vec<SocketAddr> {
        let mut freed = ArrayVec::<SocketAddr, MAX_ADDRS_PER_ROUTE>::new();
        loop {
            let next = self
                .route_addrs
                .get(&route)
                .and_then(|lists| lists.unauthenticated.head.or(lists.authenticated.head));
            let Some(addr) = next else {
                break;
            };
            let removed = self.remove_entry(addr);
            debug_assert!(removed.is_some());
            freed.push(addr);
        }
        freed.into_iter().collect()
    }

    pub fn demux(&mut self, batch: &net::RecvPacketBatch) -> Option<TransportHandle> {
        let src = batch.src;
        #[cfg(test)]
        {
            self.work.stun_sniffs = self.work.stun_sniffs.saturating_add(1);
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("stun_sniffs", 1);
        if !pulsebeam_routing::stun::is_stun(batch.data()) {
            return self.cached(src);
        }

        #[cfg(test)]
        {
            self.work.stun_parses = self.work.stun_parses.saturating_add(1);
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("stun_parses", 1);
        match pulsebeam_routing::classify::classify_client_for_node(
            batch.data(),
            self.cluster_id,
            self.node_id,
            self.shard_count,
        ) {
            pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } => {
                let handle = to_local_handle(handle);
                if self
                    .addr_map
                    .get(&src)
                    .is_some_and(|entry| entry.handle == handle)
                {
                    self.touch(src);
                    return Some(handle);
                }
                self.forget(src);
                self.admit(src, handle, false);
                Some(handle)
            }
            pulsebeam_routing::classify::ClientVerdict::Established
            | pulsebeam_routing::classify::ClientVerdict::Drop(_) => self.cached(src),
        }
    }

    fn cached(&mut self, src: SocketAddr) -> Option<TransportHandle> {
        let handle = self.addr_map.get(&src).map(|entry| entry.handle)?;
        self.touch(src);
        Some(handle)
    }

    pub fn learn(&mut self, src: SocketAddr, handle: TransportHandle) {
        match self.addr_map.get(&src).copied() {
            Some(entry) if entry.handle.route == handle.route => {
                let authenticated = entry.authenticated && entry.handle.epoch == handle.epoch;
                self.unlink_global(src);
                self.unlink_route(src, handle.route);
                if let Some(entry) = self.addr_map.get_mut(&src) {
                    entry.handle = handle;
                    entry.authenticated = authenticated;
                }
                self.link_global_front(src, authenticated);
                self.link_route_front(src, handle.route, authenticated);
            }
            Some(_) => {
                self.forget(src);
                self.admit(src, handle, false);
            }
            None => self.admit(src, handle, false),
        }
    }

    pub fn authenticate(&mut self, src: SocketAddr, handle: TransportHandle) {
        match self.addr_map.get(&src).copied() {
            Some(entry) if entry.handle == handle => {
                if !entry.authenticated {
                    self.unlink_global(src);
                    self.unlink_route(src, handle.route);
                    if let Some(entry) = self.addr_map.get_mut(&src) {
                        entry.authenticated = true;
                    }
                    self.link_global_front(src, true);
                    self.link_route_front(src, handle.route, true);
                } else {
                    self.touch(src);
                }
            }
            Some(_) | None => {
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("demux_authentication_stale");
            }
        }
    }

    fn forget(&mut self, src: SocketAddr) {
        let _ = self.remove_entry(src);
    }

    fn admit(&mut self, src: SocketAddr, handle: TransportHandle, authenticated: bool) {
        if self.addr_map.len() >= MAX_ADDR_ENTRIES {
            self.evict_global();
        }
        let route_len = self.route_addrs.get(&handle.route).map_or(0, |lists| {
            lists
                .unauthenticated
                .len
                .saturating_add(lists.authenticated.len)
        });
        if route_len >= MAX_ADDRS_PER_ROUTE {
            self.evict_route(handle.route);
        }
        let route_len = self.route_addrs.get(&handle.route).map_or(0, |lists| {
            lists
                .unauthenticated
                .len
                .saturating_add(lists.authenticated.len)
        });
        if route_len >= MAX_ADDRS_PER_ROUTE {
            debug_assert!(false, "route eviction must free a cache slot");
            return;
        }
        let previous = self.addr_map.insert(
            src,
            CachedRoute {
                handle,
                authenticated,
                global_prev: None,
                global_next: None,
                route_prev: None,
                route_next: None,
            },
        );
        debug_assert!(previous.is_none());
        self.link_global_front(src, authenticated);
        self.link_route_front(src, handle.route, authenticated);
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

fn to_local_handle(handle: pulsebeam_routing::TransportHandle) -> TransportHandle {
    let route = TransportRoute::from_raw(handle.route.get());
    debug_assert_eq!(
        route.shard().index(),
        usize::from(handle.route.shard()),
        "shard must survive the pulsebeam-routing <-> pulsebeam TransportRoute conversion"
    );
    TransportHandle::new(route, handle.epoch)
}

pub(crate) fn extract_stun_server_ufrag(data: &[u8]) -> Option<String> {
    pulsebeam_routing::stun::server_ufrag(data)
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control::ufrag::IceUfrag, id::ShardId};
    use pulsebeam_runtime::net::{RecvPacketBatch, Transport};
    use std::net::{IpAddr, Ipv4Addr};

    const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

    fn stun_with_ufrag(server: &str) -> Vec<u8> {
        let value = format!("{server}:client");
        let padded = (value.len() + 3) & !3;
        let mut buf = Vec::with_capacity(24 + padded);
        buf.extend_from_slice(&[0, 1]);
        buf.extend_from_slice(&u16::try_from(4 + padded).unwrap().to_be_bytes());
        buf.extend_from_slice(&MAGIC_COOKIE);
        buf.extend_from_slice(&[0; 12]);
        buf.extend_from_slice(&[0, 6]);
        buf.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(value.as_bytes());
        buf.resize(buf.len() + padded - value.len(), 0);
        buf
    }

    fn batch(src: SocketAddr, data: Vec<u8>) -> RecvPacketBatch {
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

    fn handle(slot: u32) -> (IceUfrag, TransportHandle) {
        let route = TransportRoute::new(ShardId::new(3), slot);
        let epoch = 7;
        (
            IceUfrag::new(0, 0, route, epoch),
            TransportHandle::new(route, epoch),
        )
    }

    #[test]
    fn established_cache_hits_do_not_parse_stun() {
        let mut demux = Demuxer::new();
        let (ufrag, expected) = handle(1);
        let client = src(1234);
        assert_eq!(
            demux.demux(&batch(client, stun_with_ufrag(&ufrag.encode()))),
            Some(expected)
        );
        let parses = demux.work.stun_parses;
        assert_eq!(
            demux.demux(&batch(client, vec![0x80, 0x60, 0, 1])),
            Some(expected)
        );
        assert_eq!(demux.work.stun_parses, parses);
        assert_eq!(demux.work.cache_entries_examined, 0);
    }

    #[test]
    fn a_rebound_source_clears_authentication() {
        let mut demux = Demuxer::new();
        let (first_ufrag, first) = handle(1);
        let (_, second) = handle(2);
        let client = src(1234);
        assert_eq!(
            demux.demux(&batch(client, stun_with_ufrag(&first_ufrag.encode()))),
            Some(first)
        );
        demux.authenticate(client, first);
        demux.learn(client, second);
        assert_eq!(
            demux.addr_map.get(&client).map(|entry| entry.handle),
            Some(second)
        );
        assert!(!demux.addr_map.get(&client).unwrap().authenticated);
    }

    #[test]
    fn stale_authentication_does_not_create_an_authenticated_cache_entry() {
        let mut demux = Demuxer::new();
        let (first_ufrag, first) = handle(1);
        let (_, second) = handle(2);
        let client = src(1234);
        let _ = demux.demux(&batch(client, stun_with_ufrag(&first_ufrag.encode())));
        demux.authenticate(client, second);
        assert_eq!(
            demux.addr_map.get(&client).map(|entry| entry.handle),
            Some(first)
        );
        assert!(!demux.addr_map.get(&client).unwrap().authenticated);
    }

    #[test]
    fn cache_eviction_does_not_scan_the_cache() {
        let mut demux = Demuxer::new();
        for slot in 0..MAX_ADDR_ENTRIES / MAX_ADDRS_PER_ROUTE {
            let (ufrag, _) = handle(u32::try_from(slot).unwrap());
            for offset in 0..MAX_ADDRS_PER_ROUTE {
                let address = SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::from(
                        0x0a00_0000 + u32::try_from(slot * 16 + offset).unwrap(),
                    )),
                    4000,
                );
                let _ = demux.demux(&batch(address, stun_with_ufrag(&ufrag.encode())));
            }
        }
        let before = demux.work.cache_entries_examined;
        let (ufrag, _) = handle(9000);
        let _ = demux.demux(&batch(src(65000), stun_with_ufrag(&ufrag.encode())));
        assert_eq!(demux.work.cache_entries_examined, before + 1);
    }

    #[test]
    fn unregister_removes_all_route_entries() {
        let mut demux = Demuxer::new();
        let (ufrag, expected) = handle(1);
        for port in 1000..1004 {
            let _ = demux.demux(&batch(src(port), stun_with_ufrag(&ufrag.encode())));
        }
        assert_eq!(demux.unregister(expected.route).len(), 4);
        assert!(demux.addr_map.is_empty());
        assert!(demux.route_addrs.is_empty());
    }
}
