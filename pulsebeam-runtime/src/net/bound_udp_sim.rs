use super::{UdpMode, UnifiedSocket, udp_batch_sim, udp_scalar};
use crate::sync::Arc;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_routing::steer::{self, FlowKey, SteerEnv, Verdict};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::{Mutex, OnceLock};
use std::{
    io,
    net::{IpAddr, SocketAddr},
};

pub struct BoundUdpSocket {
    socket: SimUdpTransport,
    local_addr: SocketAddr,
}

enum SimUdpTransport {
    Batch(udp_batch_sim::UdpTransport),
    Scalar(udp_scalar::UdpTransport),
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn into_unified_socket(self) -> io::Result<UnifiedSocket> {
        Ok(match self.socket {
            SimUdpTransport::Batch(socket) => UnifiedSocket::Udp(socket),
            SimUdpTransport::Scalar(socket) => UnifiedSocket::UdpScalar(socket),
        })
    }
}

type ReuseportKey = (SocketAddr, Option<SocketAddr>);

/// Neither `pulsebeam/src/node.rs` nor anything below it currently threads a
/// cluster/node identity down to bind time — that lives in the control plane
/// (`pulsebeam/src/control/{controller,ufrag}.rs`), assigned after sockets are
/// already bound. Until that is wired through, every simulated group
/// classifies as this identity. A multi-cluster or multi-node simulation that
/// assigns real non-zero ids will see every client STUN packet dropped as
/// `WrongCluster` / `WrongNode` until that wiring lands.
const DEFAULT_SIM_CLUSTER_ID: u16 = 0;
const DEFAULT_SIM_NODE_ID: u16 = 0;

thread_local! {
    /// Reuseport groups already bound on this host, so a second bind to the
    /// same address joins one instead of failing.
    ///
    /// Keyed by the advertised address as well as the bind address: every host
    /// in a simulation binds `0.0.0.0:3478`, and only the advertised address
    /// tells them apart. `Weak`, so a finished simulation's sockets unbind and
    /// the next one starts clean.
    static REUSEPORT_GROUPS: RefCell<HashMap<ReuseportKey, Weak<ReuseportGroup>>> =
        RefCell::new(HashMap::new());
}

#[allow(
    clippy::disallowed_types,
    reason = "one-shot simulator control must cross turmoil host threads; it is not shard state"
)]
fn wrong_owner_injection_state() -> &'static Mutex<Option<IpAddr>> {
    static STATE: OnceLock<Mutex<Option<IpAddr>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn wrong_owner_injection_state_enabled() -> Option<IpAddr> {
    // A poisoned lock means a test thread panicked while holding it; that test
    // is already failing, and steering must not take the process down with it.
    match wrong_owner_injection_state().lock() {
        Ok(state) => *state,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// One member's queue of datagrams the group has already decided belong to it.
#[derive(Default)]
struct Inbox {
    queue: VecDeque<(Vec<u8>, SocketAddr)>,
    /// Woken when `queue` goes from empty to non-empty. `notify_one` stores a
    /// permit when nobody is waiting, so a member that is between `readable`
    /// calls does not miss the wakeup.
    ready: Rc<tokio::sync::Notify>,
}

type FlowTuple = FlowKey;

/// Bounded `(src, dst) -> member index` cache, standing in for the kernel's
/// `SO_REUSEPORT` BPF map. It uses deterministic LRU eviction: a hot call is
/// touched by every established packet and therefore survives churn just as
/// it does in the kernel's LRU map.
#[derive(Default)]
struct FlowTable {
    members: HashMap<FlowTuple, u16>,
    arrival_order: VecDeque<FlowTuple>,
}

impl FlowTable {
    fn get(&mut self, tuple: FlowTuple) -> Option<u16> {
        let member = self.members.get(&tuple).copied()?;
        self.arrival_order.retain(|known| *known != tuple);
        self.arrival_order.push_back(tuple);
        debug_assert_eq!(self.arrival_order.len(), self.members.len());
        Some(member)
    }

    /// New tuple, previously unseen. NAT rebinding produces a new tuple, which
    /// simply misses here and falls back through STUN bootstrap again — the
    /// old tuple's entry is left to age out rather than actively cleared,
    /// matching a kernel map that has no rebind signal either.
    fn insert(&mut self, tuple: FlowTuple, member: u16) {
        if self.members.insert(tuple, member).is_some() {
            self.arrival_order.retain(|known| *known != tuple);
            self.arrival_order.push_back(tuple);
            debug_assert_eq!(self.arrival_order.len(), self.members.len());
            return;
        }
        if self.arrival_order.len() >= steer::SIM_FLOW_CAPACITY
            && let Some(oldest) = self.arrival_order.pop_front()
        {
            self.members.remove(&oldest);
        }
        self.arrival_order.push_back(tuple);
        debug_assert!(self.arrival_order.len() <= steer::SIM_FLOW_CAPACITY);
        debug_assert_eq!(self.members.len(), self.arrival_order.len());
    }
}

/// The deterministic simulation adapter for `SO_REUSEPORT` steering: makes the
/// exact same delivery decision the Aya program makes on Linux, by calling the
/// shared `pulsebeam-routing` classifier on the datagram bytes rather than
/// extracting a route from a Rust object the BPF path would never see on the
/// wire.
///
/// Holds no socket, so it is directly unit-testable: classification and
/// delivery are pure bookkeeping over `(cluster_id, node_id)`, a bounded flow
/// table, and one queue per shard.
struct SimSteering {
    /// Indexed by shard index, not join order. A slot is `None` until that
    /// shard's socket has bound.
    inboxes: RefCell<Vec<Option<Inbox>>>,
    flow_table: RefCell<FlowTable>,
    counters: RefCell<[u64; steer::counters::COUNT as usize]>,
    /// Test-only: when set, delivery lands on a different member than the
    /// classifier selected, so the userspace defensive drop (the
    /// shard-ownership check downstream of this adapter) has something to
    /// catch. Always off outside tests; never the normal delivery path.
    wrong_owner_injection: Cell<bool>,
}

impl SimSteering {
    fn new(cluster_id: u16, node_id: u16) -> Self {
        let _ = (cluster_id, node_id);
        Self {
            inboxes: RefCell::new(Vec::new()),
            flow_table: RefCell::new(FlowTable::default()),
            counters: RefCell::new([0; steer::counters::COUNT as usize]),
            wrong_owner_injection: Cell::new(false),
        }
    }

    #[cfg(test)]
    fn set_wrong_owner_injection(&self, enabled: bool) {
        self.wrong_owner_injection.set(enabled);
    }

    /// Reserve `shard_index`'s inbox. Called once per socket at bind time,
    /// before any datagram can arrive for it.
    fn bind_shard(&self, shard_index: u16) -> io::Result<()> {
        let index = usize::from(shard_index);
        let mut inboxes = self.inboxes.borrow_mut();
        if inboxes.len() <= index {
            inboxes.resize_with(index.saturating_add(1), || None);
        }
        match inboxes.get_mut(index) {
            Some(slot @ None) => {
                *slot = Some(Inbox::default());
                Ok(())
            }
            Some(Some(_)) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("shard index {shard_index} is already bound in this reuseport group"),
            )),
            None => {
                debug_assert!(false, "resize did not grow inboxes to index {index}");
                Err(io::Error::other(
                    "reuseport group failed to allocate its shard slot",
                ))
            }
        }
    }

    /// Resolve the member a datagram belongs to, without delivering it. A
    /// known flow is selected from the map; a miss follows the socket group's
    /// ordinary tuple hash so userspace can perform bootstrap classification.
    fn classify(&self, tuple: FlowTuple, _payload: &[u8], shard_count: u16) -> Option<usize> {
        let mut env = SimSteerEnv {
            flow_table: &self.flow_table,
            counters: &self.counters,
        };
        let verdict = steer::steer_client(&mut env, tuple, shard_count, steer::MAX_SHARDS);
        match verdict {
            Verdict::Select { shard } => {
                env.bump(steer::counters::SELECTED);
                Some(usize::from(shard))
            }
            Verdict::Pass | Verdict::Drop(_) => Some(default_member(tuple, shard_count)),
        }
    }

    #[cfg(test)]
    fn classify_node(&self, payload: &[u8], shard_count: u16) -> Option<usize> {
        let mut env = SimSteerEnv {
            flow_table: &self.flow_table,
            counters: &self.counters,
        };
        match steer::steer_node(&mut env, payload, shard_count, steer::MAX_SHARDS) {
            Verdict::Select { shard } => {
                env.bump(steer::counters::SELECTED);
                Some(usize::from(shard))
            }
            // A node-lane datagram the program will not steer is not
            // delivered here either: `Pass` means "no opinion", and there is no
            // default hash to fall back to on this lane the way there is for
            // clients.
            Verdict::Pass | Verdict::Drop(_) => None,
        }
    }

    #[cfg(test)]
    fn counter(&self, counter: u32) -> u64 {
        self.counters
            .borrow()
            .get(counter as usize)
            .copied()
            .unwrap_or_default()
    }

    fn deliver(&self, src: SocketAddr, dst: SocketAddr, payload: Vec<u8>) {
        let mut inboxes = self.inboxes.borrow_mut();
        let members = inboxes.len();
        if members == 0 {
            return;
        }
        let Ok(shard_count) = u16::try_from(members) else {
            debug_assert!(false, "reuseport group has more members than shards fit");
            return;
        };

        let Some(mut idx) = self.classify(flow_key(src, dst), &payload, shard_count) else {
            return;
        };

        // Only a datagram that names its own route can be diverted to a shard
        // that has never seen the flow. Steering delivers to the tuple hash or
        // to the route's owner, and both of those have classified the flow
        // already; a stranger shard has nothing to go on but the ufrag, so
        // diverting media there would be testing a delivery production never
        // performs. Naming the route is also what makes the diversion land
        // somewhere that is definitely not the owner, rather than one shard
        // over from whatever the hash happened to pick.
        if members > 1
            && let Some(owner) = route_owner(&payload)
            && (self.wrong_owner_injection.get()
                || wrong_owner_injection_state_enabled().is_some_and(|source| source == src.ip()))
        {
            self.wrong_owner_injection.set(false);
            match wrong_owner_injection_state().lock() {
                Ok(mut state) => *state = None,
                Err(poisoned) => *poisoned.into_inner() = None,
            }
            idx = usize::from(owner)
                .saturating_add(1)
                .checked_rem(members)
                .unwrap_or(0);
            debug_assert_ne!(
                idx,
                usize::from(owner),
                "the injected datagram must land on a shard that does not own its route"
            );
        }

        debug_assert!(idx < members, "resolved member {idx} of {members}");
        let Some(Some(inbox)) = inboxes.get_mut(idx) else {
            debug_assert!(
                false,
                "resolved member {idx} of {members} has no bound socket"
            );
            return;
        };
        inbox.queue.push_back((payload, src));
        let ready = inbox.ready.clone();
        // Release the borrow before waking: the woken member reads the same
        // RefCell as soon as it runs.
        drop(inboxes);
        ready.notify_one();
    }
}

/// Pin an authenticated flow to the shard that owns its route, the way the
/// controller pins one in the kernel's `FLOWS` map once str0m authenticates a
/// source address.
///
/// Returns whether a reuseport group serving `destination` was found. Steering
/// is a cache, so a miss is not an error: the flow keeps arriving wherever the
/// tuple hash puts it and userspace keeps forwarding it.
pub fn install_steering_flow(source: SocketAddr, destination: SocketAddr, shard: u16) -> bool {
    REUSEPORT_GROUPS.with(|groups| {
        for ((bind, advertised), group) in groups.borrow().iter() {
            let serves = advertised.map_or(*bind == destination, |addr| addr == destination);
            if !serves {
                continue;
            }
            let Some(group) = group.upgrade() else {
                continue;
            };
            // The receive pump keys arrivals on the socket's own local address,
            // not on the candidate the client dialled. Installing under
            // anything else produces an entry nothing will ever match.
            let Ok(local) = group.socket.local_addr() else {
                continue;
            };
            group
                .steering
                .flow_table
                .borrow_mut()
                .insert(flow_key(source, local), shard);
            return true;
        }
        false
    })
}

fn route_owner(payload: &[u8]) -> Option<u16> {
    match pulsebeam_routing::classify::classify_client(payload) {
        pulsebeam_routing::classify::ClientVerdict::Bootstrap { handle, .. } => {
            Some(handle.route.shard())
        }
        _ => None,
    }
}

pub fn set_wrong_owner_injection(source: IpAddr) {
    match wrong_owner_injection_state().lock() {
        Ok(mut state) => *state = Some(source),
        Err(poisoned) => *poisoned.into_inner() = Some(source),
    }
}

struct SimSteerEnv<'a> {
    flow_table: &'a RefCell<FlowTable>,
    counters: &'a RefCell<[u64; steer::counters::COUNT as usize]>,
}

impl SteerEnv for SimSteerEnv<'_> {
    fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
        self.flow_table.borrow_mut().get(flow)
    }

    fn bump(&mut self, counter: u32) {
        let mut counters = self.counters.borrow_mut();
        let Some(value) = counters.get_mut(counter as usize) else {
            debug_assert!(false, "steering counter index must be valid");
            return;
        };
        *value = value.saturating_add(1);
    }
}

fn default_member(flow: FlowKey, shard_count: u16) -> usize {
    debug_assert!(shard_count > 0);
    let mut hash = 0xcbf29ce484222325u64;
    for byte in flow.src_addr.iter().chain(flow.dst_addr.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= u64::from(flow.src_port);
    hash = hash.wrapping_mul(0x100000001b3);
    hash ^= u64::from(flow.dst_port);
    hash = hash.wrapping_mul(0x100000001b3);
    hash ^= u64::from(flow.is_ipv6);
    // `shard_count` is the number of bound sockets, so it is never zero here;
    // the guard keeps the modulo total rather than trusting that at a distance.
    let members = u64::from(shard_count).max(1);
    usize::try_from(hash.checked_rem(members).unwrap_or(0)).unwrap_or(0)
}

fn flow_key(src: SocketAddr, dst: SocketAddr) -> FlowKey {
    let (src_addr, src_ipv6) = socket_addr_parts(src);
    let (dst_addr, dst_ipv6) = socket_addr_parts(dst);
    debug_assert_eq!(src_ipv6, dst_ipv6, "a UDP flow cannot mix address families");
    FlowKey {
        src_addr,
        dst_addr,
        src_port: src.port(),
        dst_port: dst.port(),
        is_ipv6: u8::from(src_ipv6),
        _pad: [0; 3],
    }
}

fn socket_addr_parts(addr: SocketAddr) -> ([u8; 16], bool) {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let mut bytes = [0; 16];
            bytes[..4].copy_from_slice(&ip.octets());
            (bytes, false)
        }
        std::net::IpAddr::V6(ip) => (ip.octets(), true),
    }
}

/// A set of sockets bound to one address, emulating `SO_REUSEPORT` steered by
/// [`SimSteering`].
///
/// # Why this is not just "share the socket"
///
/// It was, and that was wrong. Several readers polling one socket delivers a
/// datagram to whichever happens to be ready, which is not what `SO_REUSEPORT`
/// does and not something a WebRTC server can work with: a session's ICE and
/// DTLS state lives on exactly one shard, so a client whose datagrams scatter
/// across the group never completes a handshake.
///
/// # Shape
///
/// A single pump task owns reading. It is the only thing that ever touches the
/// socket's receive half, which matters because turmoil's `readable()` takes a
/// lock on it: several members awaiting readability directly would serialise on
/// that lock, and a member blocked there cannot notice its own inbox filling.
/// Members only ever wait on their inbox.
struct ReuseportGroup {
    socket: Arc<UdpSocket>,
    steering: SimSteering,
}

/// A handle one bound socket holds on its slot in a group.
pub(crate) struct ReuseportMember {
    group: Rc<ReuseportGroup>,
    index: usize,
}

impl ReuseportMember {
    /// Resolve once the member's inbox has something in it.
    pub(crate) async fn readable(&self) -> io::Result<()> {
        loop {
            let ready = {
                let inboxes = self.group.steering.inboxes.borrow();
                let Some(Some(inbox)) = inboxes.get(self.index) else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "this socket's reuseport slot is gone",
                    ));
                };
                if !inbox.queue.is_empty() {
                    return Ok(());
                }
                inbox.ready.clone()
            };
            ready.notified().await;
        }
    }

    /// Take the next datagram, or `WouldBlock` — the readiness contract the
    /// caller already expects from a socket.
    pub(crate) fn try_recv(&self) -> io::Result<(Vec<u8>, SocketAddr)> {
        self.group
            .steering
            .inboxes
            .borrow_mut()
            .get_mut(self.index)
            .and_then(Option::as_mut)
            .and_then(|inbox| inbox.queue.pop_front())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "reuseport inbox is empty"))
    }
}

/// How often the pump re-checks whether its group still has members.
///
/// It is otherwise parked in `readable()`, which never resolves once a
/// simulation stops sending. Without this the last group of a plan would keep
/// its socket bound and the next plan on this thread would fail to bind.
const PUMP_LIVENESS_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Read from the shared socket and file each datagram under its member.
///
/// Holds only a `Weak` on the group between datagrams, so the group drops when
/// its last member does rather than being kept alive by its own pump.
fn spawn_pump(group: &Rc<ReuseportGroup>) {
    let weak = Rc::downgrade(group);
    let socket = group.socket.clone();
    tokio::task::spawn_local(async move {
        let mut buf = vec![0u8; super::CHUNK_SIZE];
        loop {
            if weak.strong_count() == 0 {
                return;
            }
            tokio::select! {
                res = socket.readable() => {
                    if res.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(PUMP_LIVENESS_POLL) => continue,
            }

            let Some(group) = weak.upgrade() else {
                return;
            };
            let Ok(dst) = socket.local_addr() else {
                return;
            };
            // Drain to WouldBlock: readability is edge-triggered, so leaving a
            // datagram behind can park the pump with work outstanding.
            loop {
                match socket.try_recv_from(&mut buf) {
                    Ok((n, src)) => match buf.get(..n) {
                        Some(datagram) => group.steering.deliver(src, dst, datagram.to_vec()),
                        None => return,
                    },
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
        }
    });
}

/// Bind a socket, emulating `SO_REUSEPORT`.
///
/// Turmoil allows one bind per address, but a shard-per-core SFU binds the same
/// port once per worker and lets the kernel spread arrivals. Without that, a
/// simulation asking for several workers silently gets one — and a single-shard
/// node never executes a cross-shard path, so every plan that believes it is
/// testing one is testing something else.
///
/// `shard_index` is the shard this socket serves; delivery for that shard's
/// flows lands here regardless of bind order. Callers must bind every shard of
/// a group before traffic starts — the group treats its current member count
/// as the classifier's `shard_count`, so a socket that joins after a peer has
/// already resolved a route for it would be a race the real kernel map doesn't
/// have either.
///
/// Delivery is steered by [`SimSteering`], the same classifier the Aya program
/// runs. See [`ReuseportGroup`].
pub async fn bind_udp_socket(
    addr: SocketAddr,
    mode: UdpMode,
    external_addr: Option<SocketAddr>,
    shard_index: u16,
) -> io::Result<BoundUdpSocket> {
    let key = (addr, external_addr);
    let existing =
        REUSEPORT_GROUPS.with(|groups| groups.borrow().get(&key).and_then(Weak::upgrade));

    let group = match existing {
        Some(group) => group,
        None => {
            let group = Rc::new(ReuseportGroup {
                socket: Arc::new(UdpSocket::bind(addr).await?),
                steering: SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID),
            });
            REUSEPORT_GROUPS.with(|groups| groups.borrow_mut().insert(key, Rc::downgrade(&group)));
            spawn_pump(&group);
            group
        }
    };

    group.steering.bind_shard(shard_index)?;

    let socket = group.socket.clone();
    let member = ReuseportMember {
        group,
        index: usize::from(shard_index),
    };
    let socket = match mode {
        UdpMode::Batch => SimUdpTransport::Batch(udp_batch_sim::from_reuseport_member(
            socket,
            external_addr,
            member,
        )?),
        UdpMode::Scalar => SimUdpTransport::Scalar(udp_scalar::from_reuseport_member(
            socket,
            external_addr,
            member,
        )?),
    };
    let local_addr = match &socket {
        SimUdpTransport::Batch(socket) => socket.local_addr(),
        SimUdpTransport::Scalar(socket) => socket.local_addr(),
    };
    Ok(BoundUdpSocket { socket, local_addr })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_routing::RouteId;
    use pulsebeam_routing::envelope::{Envelope, EnvelopeType};

    fn addr(port: u16) -> SocketAddr {
        format!("192.168.1.10:{port}").parse().unwrap()
    }

    fn dst_addr() -> SocketAddr {
        "192.168.1.99:3478".parse().unwrap()
    }

    /// A STUN binding request whose USERNAME names `shard`'s route, which is
    /// the only kind of datagram a shard can classify without prior state.
    fn bootstrap_for(shard: u16, slot: u32) -> Vec<u8> {
        let ufrag = pulsebeam_routing::ufrag::IceUfrag::new(
            DEFAULT_SIM_CLUSTER_ID,
            DEFAULT_SIM_NODE_ID,
            pulsebeam_routing::TransportRoute::new(shard, slot),
            0,
        );
        let mut username = ufrag.encode_ascii().to_vec();
        username.extend_from_slice(b":client");
        let padded = (username.len() + 3) & !3;

        let mut buf = Vec::with_capacity(24 + padded);
        buf.extend_from_slice(&0x0001u16.to_be_bytes());
        buf.extend_from_slice(
            &u16::try_from(4 + padded)
                .expect("fixture length fits")
                .to_be_bytes(),
        );
        buf.extend_from_slice(&pulsebeam_routing::stun::MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(&0x0006u16.to_be_bytes());
        buf.extend_from_slice(
            &u16::try_from(username.len())
                .expect("fixture username fits")
                .to_be_bytes(),
        );
        buf.extend_from_slice(&username);
        buf.resize(buf.len() + padded - username.len(), 0);
        buf
    }

    fn new_steering(shard_count: u16) -> SimSteering {
        let steering = SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID);
        for shard in 0..shard_count {
            steering.bind_shard(shard).unwrap();
        }
        steering
    }

    #[test]
    fn an_unknown_tuple_uses_default_hash_without_reading_payload() {
        let steering = new_steering(4);
        let tuple = flow_key(addr(1024), dst_addr());
        let first = steering.classify(tuple, b"not-stun", 4);
        assert_eq!(first, steering.classify(tuple, b"garbage", 4));
        assert_eq!(steering.counter(steer::counters::UNKNOWN_FLOW), 2);
    }

    #[test]
    fn an_authenticated_flow_map_hit_selects_its_owner() {
        let steering = new_steering(4);
        let tuple = flow_key(addr(1024), dst_addr());
        steering.flow_table.borrow_mut().insert(tuple, 3);
        assert_eq!(steering.classify(tuple, b"media", 4), Some(3));
        assert_eq!(steering.counter(steer::counters::SELECTED), 1);
    }

    #[test]
    fn malformed_and_non_stun_bootstrap_payloads_still_fall_through() {
        let steering = new_steering(4);
        let tuple = flow_key(addr(1024), dst_addr());
        let selected = steering.classify(tuple, &[0; 3], 4);
        assert_eq!(selected, steering.classify(tuple, b"media", 4));
        assert_eq!(steering.counter(steer::counters::MALFORMED_STUN), 0);
    }

    #[test]
    fn stale_map_entries_fall_through_and_are_counted() {
        let steering = new_steering(4);
        let tuple = flow_key(addr(1024), dst_addr());
        steering.flow_table.borrow_mut().insert(tuple, 3);
        assert_eq!(
            steering.classify(tuple, b"media", 3),
            Some(default_member(tuple, 3))
        );
        assert_eq!(steering.counter(steer::counters::STALE_ROUTE), 1);
    }

    #[test]
    fn flow_table_eviction_preserves_a_hot_flow() {
        let mut table = FlowTable::default();
        for i in 0..steer::SIM_FLOW_CAPACITY {
            let tuple = flow_key(addr(u16::try_from(1024 + i).unwrap()), dst_addr());
            table.insert(tuple, u16::try_from(i % 4).unwrap());
        }
        let hot_tuple = flow_key(addr(1024), dst_addr());
        let cold_tuple = flow_key(addr(1025), dst_addr());
        assert_eq!(table.get(hot_tuple), Some(0));
        let overflow_port = u16::try_from(1024 + steer::SIM_FLOW_CAPACITY).expect("fits a port");
        table.insert(flow_key(addr(overflow_port), dst_addr()), 0);
        assert_eq!(table.get(hot_tuple), Some(0));
        assert_eq!(table.get(cold_tuple), None);
        assert_eq!(table.members.len(), steer::SIM_FLOW_CAPACITY);
    }

    /// A bootstrap datagram delivered to a shard that does not own its route.
    ///
    /// This is the one delivery userspace forwarding exists for, and the test
    /// pins the property that makes it exercisable: the datagram lands
    /// somewhere other than the shard the ufrag names, whatever the tuple hash
    /// would have chosen.
    #[test]
    fn wrong_owner_injection_lands_on_a_shard_that_does_not_own_the_route() {
        let steering = new_steering(4);
        steering.set_wrong_owner_injection(true);
        let owner = 2u16;
        steering.deliver(addr(1024), dst_addr(), bootstrap_for(owner, 7));

        let inboxes = steering.inboxes.borrow();
        let landed = inboxes
            .iter()
            .enumerate()
            .find_map(|(index, inbox)| {
                inbox
                    .as_ref()
                    .filter(|inbox| !inbox.queue.is_empty())
                    .map(|_| index)
            })
            .expect("packet must land somewhere");
        assert_ne!(landed, usize::from(owner));
    }

    /// A datagram that names no route cannot be diverted: a shard that has
    /// never classified the flow has nothing to resolve it with, so production
    /// never delivers one there.
    #[test]
    fn injection_does_not_divert_a_datagram_that_names_no_route() {
        let steering = new_steering(4);
        steering.set_wrong_owner_injection(true);
        let tuple = flow_key(addr(1024), dst_addr());
        steering.deliver(addr(1024), dst_addr(), b"media".to_vec());

        let inboxes = steering.inboxes.borrow();
        let landed = inboxes
            .iter()
            .enumerate()
            .find_map(|(index, inbox)| {
                inbox
                    .as_ref()
                    .filter(|inbox| !inbox.queue.is_empty())
                    .map(|_| index)
            })
            .expect("packet must land somewhere");
        assert_eq!(landed, default_member(tuple, 4));
    }

    #[test]
    fn node_envelope_drop_taxonomy_remains_kernel_owned() {
        let steering = new_steering(4);
        assert_eq!(steering.classify_node(&[0; 3], 4), None);
        assert_eq!(steering.counter(steer::counters::MALFORMED_ENVELOPE), 1);
        let envelope = Envelope {
            ty: EnvelopeType::Media,
            epoch: 1,
            route: RouteId::new(1, 1),
            extension: 0,
        }
        .encode();
        assert_eq!(steering.classify_node(&envelope, 4), Some(1));
    }

    #[test]
    fn binding_the_same_shard_twice_fails() {
        let steering = SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID);
        steering.bind_shard(0).unwrap();
        assert!(steering.bind_shard(0).is_err());
    }
}
