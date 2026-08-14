use super::{UdpMode, UnifiedSocket, udp_scalar};
use crate::sync::Arc;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_routing::classify::{self, ClientVerdict};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::{io, net::SocketAddr};

pub struct BoundUdpSocket {
    socket: udp_scalar::UdpTransport,
    local_addr: SocketAddr,
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn into_unified_socket(self) -> io::Result<UnifiedSocket> {
        Ok(UnifiedSocket::UdpScalar(self.socket))
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

/// Bound the way a BPF map is bounded: fixed capacity, deterministic eviction.
/// FIFO rather than LRU or anything hash-order-dependent, so eviction never
/// depends on `HashMap` iteration order or wall-clock time.
const FLOW_TABLE_CAPACITY: usize = 4096;

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

/// One member's queue of datagrams the group has already decided belong to it.
#[derive(Default)]
struct Inbox {
    queue: VecDeque<(Vec<u8>, SocketAddr)>,
    /// Woken when `queue` goes from empty to non-empty. `notify_one` stores a
    /// permit when nobody is waiting, so a member that is between `readable`
    /// calls does not miss the wakeup.
    ready: Rc<tokio::sync::Notify>,
}

type FlowTuple = (SocketAddr, SocketAddr);

/// Bounded `(src, dst) -> member index` cache, standing in for the kernel's
/// `SO_REUSEPORT` BPF map. Capacity-bounded with deterministic FIFO eviction —
/// no time, no hashing of the eviction order — so a plan replays identically.
#[derive(Default)]
struct FlowTable {
    members: HashMap<FlowTuple, usize>,
    arrival_order: VecDeque<FlowTuple>,
}

impl FlowTable {
    fn get(&self, tuple: FlowTuple) -> Option<usize> {
        self.members.get(&tuple).copied()
    }

    /// New tuple, previously unseen. NAT rebinding produces a new tuple, which
    /// simply misses here and falls back through STUN bootstrap again — the
    /// old tuple's entry is left to age out via FIFO eviction rather than
    /// actively cleared, matching a kernel map that has no rebind signal
    /// either.
    fn insert(&mut self, tuple: FlowTuple, member: usize) {
        debug_assert!(
            self.get(tuple).is_none(),
            "insert called for a tuple the flow table already has"
        );
        if self.arrival_order.len() >= FLOW_TABLE_CAPACITY
            && let Some(oldest) = self.arrival_order.pop_front()
        {
            self.members.remove(&oldest);
        }
        self.members.insert(tuple, member);
        self.arrival_order.push_back(tuple);
        debug_assert!(self.arrival_order.len() <= FLOW_TABLE_CAPACITY);
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
    cluster_id: u16,
    node_id: u16,
    /// Indexed by shard index, not join order. A slot is `None` until that
    /// shard's socket has bound.
    inboxes: RefCell<Vec<Option<Inbox>>>,
    flow_table: RefCell<FlowTable>,
    /// Test-only: when set, delivery lands on a different member than the
    /// classifier selected, so the userspace defensive drop (the
    /// shard-ownership check downstream of this adapter) has something to
    /// catch. Always off outside tests; never the normal delivery path.
    wrong_owner_injection: Cell<bool>,
}

impl SimSteering {
    fn new(cluster_id: u16, node_id: u16) -> Self {
        Self {
            cluster_id,
            node_id,
            inboxes: RefCell::new(Vec::new()),
            flow_table: RefCell::new(FlowTable::default()),
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
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "reuseport group failed to allocate its shard slot",
                ))
            }
        }
    }

    /// Resolve the member a datagram belongs to, without delivering it.
    /// `None` means the packet is dropped: an unknown tuple with no valid
    /// STUN bootstrap, exactly as an unrecognized flow with no matching
    /// kernel map entry is dropped rather than guessed at.
    fn classify(&self, tuple: FlowTuple, payload: &[u8], shard_count: u16) -> Option<usize> {
        if let Some(member) = self.flow_table.borrow().get(tuple) {
            return Some(member);
        }
        match classify::classify_client_for_node(
            payload,
            self.cluster_id,
            self.node_id,
            shard_count,
        ) {
            ClientVerdict::Bootstrap { handle, .. } => {
                let member = usize::from(handle.route.shard());
                debug_assert!(
                    member < usize::from(shard_count),
                    "classify_client_for_node returned an out-of-range shard"
                );
                self.flow_table.borrow_mut().insert(tuple, member);
                Some(member)
            }
            ClientVerdict::Established | ClientVerdict::Drop(_) => None,
        }
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

        let Some(mut idx) = self.classify((src, dst), &payload, shard_count) else {
            return;
        };

        if self.wrong_owner_injection.get() && members > 1 {
            idx = idx.saturating_add(1).checked_rem(members).unwrap_or(0);
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
    _mode: UdpMode,
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
    let socket = udp_scalar::from_reuseport_member(socket, external_addr, member)?;
    let local_addr = socket.local_addr();
    Ok(BoundUdpSocket { socket, local_addr })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_routing::TransportRoute;
    use pulsebeam_routing::ufrag::IceUfrag;

    fn addr(port: u16) -> SocketAddr {
        format!("192.168.1.10:{port}").parse().unwrap()
    }

    fn dst_addr() -> SocketAddr {
        "192.168.1.99:3478".parse().unwrap()
    }

    fn new_steering(shard_count: u16) -> SimSteering {
        let steering = SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID);
        for shard in 0..shard_count {
            steering.bind_shard(shard).unwrap();
        }
        steering
    }

    const BINDING_REQUEST: u16 = 0x0001;
    const MAGIC_COOKIE_BYTES: [u8; 4] = pulsebeam_routing::stun::MAGIC_COOKIE.to_be_bytes();
    const USERNAME_ATTRIBUTE_TYPE: u16 = 0x0006;
    const MIN_STUN_HEADER_SIZE: usize = 20;

    fn build_stun_with_ufrag(u: &IceUfrag) -> Vec<u8> {
        let mut value = Vec::from(u.encode_ascii());
        value.push(b':');
        value.extend_from_slice(b"client");

        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&MAGIC_COOKIE_BYTES);
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(buf.len(), MIN_STUN_HEADER_SIZE);

        let padded_len = (value.len() + 3) & !3;
        let padding = padded_len - value.len();
        buf.extend_from_slice(&USERNAME_ATTRIBUTE_TYPE.to_be_bytes());
        buf.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(&value);
        buf.extend_from_slice(&std::vec![0u8; padding]);

        let total_attr_len = u16::try_from(4 + padded_len).unwrap();
        buf[2..4].copy_from_slice(&total_attr_len.to_be_bytes());
        buf
    }

    fn ufrag_for_shard(shard: u16) -> IceUfrag {
        IceUfrag::new(
            DEFAULT_SIM_CLUSTER_ID,
            DEFAULT_SIM_NODE_ID,
            TransportRoute::new(shard, 1),
            7,
        )
    }

    #[test]
    fn stun_bootstrap_selects_the_shard_named_by_the_ufrag() {
        let steering = new_steering(4);
        let u = ufrag_for_shard(2);
        let msg = build_stun_with_ufrag(&u);
        let tuple = (addr(1024), dst_addr());
        assert_eq!(steering.classify(tuple, &msg, 4), Some(2));
    }

    #[test]
    fn a_second_packet_on_the_same_tuple_reuses_the_member_without_reparsing_stun() {
        let steering = new_steering(4);
        let u = ufrag_for_shard(3);
        let msg = build_stun_with_ufrag(&u);
        let tuple = (addr(1024), dst_addr());
        assert_eq!(steering.classify(tuple, &msg, 4), Some(3));

        // Garbage this time — if this were reclassified it would be dropped.
        let garbage = std::vec![0xAAu8; 32];
        assert_eq!(steering.classify(tuple, &garbage, 4), Some(3));
    }

    #[test]
    fn non_stun_packet_from_an_unknown_tuple_is_dropped() {
        let steering = new_steering(4);
        let tuple = (addr(1024), dst_addr());
        let payload = std::vec![0xAAu8; 32];
        assert_eq!(steering.classify(tuple, &payload, 4), None);
    }

    #[test]
    fn malformed_stun_packet_is_dropped() {
        let steering = new_steering(4);
        let tuple = (addr(1024), dst_addr());
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE_BYTES);
        msg.extend_from_slice(&[0u8; 12]);
        assert_eq!(steering.classify(tuple, &msg, 4), None);
    }

    #[test]
    fn garbage_packet_is_dropped() {
        let steering = new_steering(4);
        let tuple = (addr(1024), dst_addr());
        let garbage = std::vec![0x00u8; 3];
        assert_eq!(steering.classify(tuple, &garbage, 4), None);
    }

    #[test]
    fn rebound_tuple_requires_a_fresh_stun_bootstrap() {
        let steering = new_steering(4);
        let u = ufrag_for_shard(1);
        let msg = build_stun_with_ufrag(&u);
        let old_tuple = (addr(1024), dst_addr());
        assert_eq!(steering.classify(old_tuple, &msg, 4), Some(1));

        let new_tuple = (addr(1025), dst_addr());
        let non_stun = std::vec![0xAAu8; 32];
        assert_eq!(
            steering.classify(new_tuple, &non_stun, 4),
            None,
            "a rebound tuple has no flow entry yet, so non-STUN traffic on it must not be assigned"
        );

        let u2 = ufrag_for_shard(1);
        let msg2 = build_stun_with_ufrag(&u2);
        assert_eq!(steering.classify(new_tuple, &msg2, 4), Some(1));
    }

    #[test]
    fn flow_table_eviction_is_deterministic_fifo() {
        let mut table = FlowTable::default();
        for i in 0..FLOW_TABLE_CAPACITY {
            let tuple = (addr(u16::try_from(1024 + i).unwrap()), dst_addr());
            table.insert(tuple, i % 4);
        }
        let evicted_tuple = (addr(1024), dst_addr());
        assert_eq!(table.get(evicted_tuple), Some(0));

        let overflow_port = u16::try_from(1024 + FLOW_TABLE_CAPACITY).expect("fits a port");
        let overflow_tuple = (addr(overflow_port), dst_addr());
        table.insert(overflow_tuple, 0);
        assert_eq!(
            table.get(evicted_tuple),
            None,
            "oldest entry must be evicted once capacity is exceeded"
        );
        assert_eq!(table.get(overflow_tuple), Some(0));
        assert_eq!(table.members.len(), FLOW_TABLE_CAPACITY);
    }

    #[test]
    fn wrong_owner_injection_delivers_to_a_different_member_than_classification_chose() {
        let steering = new_steering(4);
        steering.set_wrong_owner_injection(true);

        let u = ufrag_for_shard(2);
        let msg = build_stun_with_ufrag(&u);
        let tuple = (addr(1024), dst_addr());
        let selected = steering.classify(tuple, &msg, 4).unwrap();
        assert_eq!(selected, 2);

        steering.deliver(tuple.0, tuple.1, msg);

        let inboxes = steering.inboxes.borrow();
        let landed = inboxes
            .iter()
            .enumerate()
            .find_map(|(i, inbox)| {
                inbox
                    .as_ref()
                    .filter(|inbox| !inbox.queue.is_empty())
                    .map(|_| i)
            })
            .expect("packet must land somewhere");
        assert_ne!(
            landed, selected,
            "wrong-owner injection must not deliver to the classifier's chosen member"
        );
    }

    #[test]
    fn wrong_owner_injection_is_off_by_default() {
        let steering = new_steering(4);
        let u = ufrag_for_shard(2);
        let msg = build_stun_with_ufrag(&u);
        let tuple = (addr(1024), dst_addr());
        steering.deliver(tuple.0, tuple.1, msg);

        let inboxes = steering.inboxes.borrow();
        assert!(!inboxes.get(2).unwrap().as_ref().unwrap().queue.is_empty());
    }

    #[test]
    fn bind_shard_out_of_order_still_indexes_by_shard_not_join_order() {
        let steering = SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID);
        steering.bind_shard(2).unwrap();
        steering.bind_shard(0).unwrap();
        steering.bind_shard(1).unwrap();

        let u = ufrag_for_shard(0);
        let msg = build_stun_with_ufrag(&u);
        let tuple = (addr(1024), dst_addr());
        assert_eq!(steering.classify(tuple, &msg, 3), Some(0));
    }

    #[test]
    fn binding_the_same_shard_twice_fails() {
        let steering = SimSteering::new(DEFAULT_SIM_CLUSTER_ID, DEFAULT_SIM_NODE_ID);
        steering.bind_shard(0).unwrap();
        assert!(steering.bind_shard(0).is_err());
    }
}
