use super::{UdpMode, UnifiedSocket, udp_scalar};
use crate::sync::Arc;
use pulsebeam_core::net::UdpSocket;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
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

/// A set of sockets bound to one address, emulating `SO_REUSEPORT`.
///
/// # Why this is not just "share the socket"
///
/// It was, and that was wrong. Several readers polling one socket delivers a
/// datagram to whichever happens to be ready, which is not what `SO_REUSEPORT`
/// does and not something a WebRTC server can work with: the kernel picks a
/// socket by hashing the packet's 4-tuple, so every datagram of a flow lands on
/// the same one for as long as the group's membership is unchanged. A session's
/// ICE and DTLS state lives on exactly one shard, so a client whose datagrams
/// scatter across the group never completes a handshake.
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
    inboxes: RefCell<Vec<Inbox>>,
}

/// Which member a datagram belongs to.
///
/// Hashing source and destination together is the 4-tuple the kernel uses.
/// Destination is constant within a group, so in practice the source decides —
/// but including it keeps the emulation honest if a group ever spans addresses.
///
/// `DefaultHasher` rather than a `RandomState`: the mapping has to repeat across
/// runs, or a failing plan does not replay.
fn member_for(src: SocketAddr, dst: SocketAddr, members: usize) -> usize {
    debug_assert!(members > 0, "a group always has at least the member asking");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut hasher);
    dst.hash(&mut hasher);
    (hasher.finish() % members as u64) as usize
}

impl ReuseportGroup {
    fn deliver(&self, src: SocketAddr, dst: SocketAddr, payload: Vec<u8>) {
        let mut inboxes = self.inboxes.borrow_mut();
        let members = inboxes.len();
        if members == 0 {
            return;
        }
        let idx = member_for(src, dst, members);
        let inbox = &mut inboxes[idx];
        inbox.queue.push_back((payload, src));
        let ready = inbox.ready.clone();
        // Release the borrow before waking: the woken member reads the same
        // RefCell as soon as it runs.
        drop(inboxes);
        ready.notify_one();
    }
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
                let inboxes = self.group.inboxes.borrow();
                if !inboxes[self.index].queue.is_empty() {
                    return Ok(());
                }
                inboxes[self.index].ready.clone()
            };
            ready.notified().await;
        }
    }

    /// Take the next datagram, or `WouldBlock` — the readiness contract the
    /// caller already expects from a socket.
    pub(crate) fn try_recv(&self) -> io::Result<(Vec<u8>, SocketAddr)> {
        self.group.inboxes.borrow_mut()[self.index]
            .queue
            .pop_front()
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
                    Ok((n, src)) => group.deliver(src, dst, buf[..n].to_vec()),
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
/// Delivery is sticky per 4-tuple, like the real thing. See [`ReuseportGroup`].
pub async fn bind_udp_socket(
    addr: SocketAddr,
    _mode: UdpMode,
    external_addr: Option<SocketAddr>,
) -> io::Result<BoundUdpSocket> {
    let key = (addr, external_addr);
    let existing =
        REUSEPORT_GROUPS.with(|groups| groups.borrow().get(&key).and_then(Weak::upgrade));

    let group = match existing {
        Some(group) => group,
        None => {
            let group = Rc::new(ReuseportGroup {
                socket: Arc::new(UdpSocket::bind(addr).await?),
                inboxes: RefCell::new(Vec::new()),
            });
            REUSEPORT_GROUPS.with(|groups| groups.borrow_mut().insert(key, Rc::downgrade(&group)));
            spawn_pump(&group);
            group
        }
    };

    // Joining renumbers the group, exactly as adding a socket to a real one
    // does. Every bind happens during node startup, before a client has sent
    // anything, so no flow is rehomed mid-flight.
    let index = {
        let mut inboxes = group.inboxes.borrow_mut();
        inboxes.push(Inbox::default());
        inboxes.len() - 1
    };

    let socket = group.socket.clone();
    let member = ReuseportMember { group, index };
    let socket = udp_scalar::from_reuseport_member(socket, external_addr, member)?;
    let local_addr = socket.local_addr();
    Ok(BoundUdpSocket { socket, local_addr })
}

#[cfg(test)]
mod tests {
    use super::member_for;
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        format!("192.168.1.10:{port}").parse().unwrap()
    }

    #[test]
    fn a_flow_always_lands_on_the_same_member() {
        let dst = addr(3478);
        for port in 1024u16..1124 {
            let first = member_for(addr(port), dst, 4);
            for _ in 0..8 {
                assert_eq!(
                    member_for(addr(port), dst, 4),
                    first,
                    "a 4-tuple must map to one member while the group is unchanged"
                );
            }
        }
    }

    /// Stickiness alone is satisfied by sending everything to member 0, which
    /// is also the bug this replaced. Spread is what says it is a hash.
    #[test]
    fn flows_are_spread_across_the_group() {
        let dst = addr(3478);
        let mut hit = [0usize; 4];
        for port in 1024u16..1224 {
            hit[member_for(addr(port), dst, 4)] += 1;
        }
        for (member, count) in hit.iter().enumerate() {
            assert!(
                *count > 0,
                "member {member} received none of 200 flows: {hit:?}"
            );
        }
    }

    /// A plan that fails must fail the same way on replay, which means the
    /// placement cannot come from a randomly seeded hasher.
    #[test]
    fn the_mapping_does_not_vary_between_hashers() {
        let dst = addr(3478);
        let mapping: Vec<usize> = (1024u16..1124)
            .map(|p| member_for(addr(p), dst, 3))
            .collect();
        let again: Vec<usize> = (1024u16..1124)
            .map(|p| member_for(addr(p), dst, 3))
            .collect();
        assert_eq!(mapping, again);
    }
}
