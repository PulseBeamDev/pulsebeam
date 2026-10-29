//! Low-priority passive ICE-TCP I/O. Only owned messages cross into shards.

#[cfg(test)]
mod tests;

use crate::{
    control::ufrag::IceUfrag, route::NodeTransportAddress, shard::demux::extract_stun_server_ufrag,
};
use pulsebeam_core::net::{TcpListener, TcpStream};
use pulsebeam_queue as mailbox;
use pulsebeam_runtime::net::ReceiveTime;
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// Finite resource limits for the optional passive fallback.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_connections: usize,
    pub max_pending: usize,
    pub max_per_ip: usize,
    pub max_frame_bytes: usize,
    pub inbox_capacity: usize,
    pub outbound_capacity: usize,
    pub first_stun_timeout: Duration,
    pub stalled_io_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_pending: 64,
            max_per_ip: 16,
            max_frame_bytes: 1500,
            inbox_capacity: 32,
            outbound_capacity: 32,
            first_stun_timeout: Duration::from_secs(2),
            stalled_io_timeout: Duration::from_secs(2),
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self, shards: usize) -> io::Result<()> {
        let invalid = self.max_connections == 0
            || self.max_pending == 0
            || self.max_per_ip == 0
            || self.max_frame_bytes < 20
            || self.max_frame_bytes > usize::from(u16::MAX)
            || self.inbox_capacity == 0
            || self.outbound_capacity == 0
            || self.first_stun_timeout.is_zero()
            || self.stalled_io_timeout.is_zero();
        let bounds = self
            .max_connections
            .checked_add(self.max_pending)
            .filter(|tasks| *tasks <= Semaphore::MAX_PERMITS)
            .and_then(|_| self.outbound_capacity.checked_mul(self.max_connections))
            .and_then(|outbound| {
                self.inbox_capacity
                    .checked_mul(shards)?
                    .checked_add(outbound)
            })
            .and_then(|frames| frames.checked_mul(self.max_frame_bytes));
        if invalid
            || bounds.is_none()
            || self.max_pending > Semaphore::MAX_PERMITS
            || self.inbox_capacity > Semaphore::MAX_PERMITS
            || self.outbound_capacity > Semaphore::MAX_PERMITS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid ICE-TCP resource limits",
            ));
        }
        Ok(())
    }
}

struct AdmissionPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    gauge: metrics::Gauge,
}

impl AdmissionPermit {
    fn new(permit: tokio::sync::OwnedSemaphorePermit, metric: &'static str) -> Self {
        let gauge = metrics::gauge!(metric);
        gauge.increment(1.0);
        Self {
            _permit: permit,
            gauge,
        }
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.gauge.decrement(1.0);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TcpFlowId(u64);

#[allow(
    clippy::disallowed_types,
    reason = "process-wide flow generation is touched only on TCP admission, never on a packet path"
)]
fn next_flow_id() -> Option<TcpFlowId> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .ok()
        .map(TcpFlowId)
}

fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) if v6.ip().to_ipv4_mapped().is_some() => {
            SocketAddr::new(v6.ip().to_canonical(), v6.port())
        }
        address => address,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FlowRoute {
    pub address: NodeTransportAddress,
    pub id: TcpFlowId,
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

#[derive(Debug)]
pub(crate) struct TcpFlow {
    pub route: FlowRoute,
    outbound: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    max_frame_bytes: usize,
}

impl TcpFlow {
    pub(crate) fn is_open(&self) -> bool {
        !self.cancel.is_cancelled() && !self.outbound.is_closed()
    }

    pub(crate) fn try_send(&self, packet: &[u8]) -> bool {
        if packet.len() > self.max_frame_bytes
            || self.cancel.is_cancelled()
            || self.outbound.try_send(packet.to_vec()).is_err()
        {
            self.cancel.cancel();
            metrics::counter!("ice_tcp_egress_close", "reason" => "full_or_closed").increment(1);
            return false;
        }
        true
    }
}

impl Drop for TcpFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
pub(crate) fn fixture(
    address: NodeTransportAddress,
    local: SocketAddr,
    remote: SocketAddr,
    capacity: usize,
) -> (TcpFlow, mpsc::Receiver<Vec<u8>>) {
    let (outbound, receiver) = mpsc::channel(capacity);
    (
        TcpFlow {
            route: FlowRoute {
                address,
                id: next_flow_id().unwrap(),
                local,
                remote,
            },
            outbound,
            cancel: CancellationToken::new(),
            max_frame_bytes: usize::from(u16::MAX),
        },
        receiver,
    )
}

pub(crate) struct TcpPacket {
    pub route: FlowRoute,
    pub packet: Vec<u8>,
    pub received_at: ReceiveTime,
    // The quota follows the allocation through inbox and deferred RTC input.
    _slot: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(test)]
impl TcpPacket {
    pub(crate) fn fixture(route: FlowRoute, packet: &[u8]) -> Self {
        let (sender, _) = inbox(1, usize::from(u16::MAX)).unwrap();
        sender.packet(route, packet, ReceiveTime::now()).unwrap()
    }
}

pub(crate) enum TcpIngress {
    Adopt {
        flow: TcpFlow,
        first_stun: TcpPacket,
        reply: oneshot::Sender<bool>,
    },
    Frame(TcpPacket),
    Closed {
        route: FlowRoute,
    },
}

#[derive(Clone)]
pub(crate) struct InboxSender {
    channel: mailbox::Sender<TcpIngress>,
    #[allow(
        clippy::disallowed_types,
        reason = "TCP-only frame quota spans the I/O executor and owning shard; UDP never touches it"
    )]
    frames: std::sync::Arc<Semaphore>,
}

impl InboxSender {
    #[allow(
        clippy::disallowed_types,
        reason = "the owned TCP-only quota permit moves with each frame until RTC consumes or drops it"
    )]
    fn packet(
        &self,
        route: FlowRoute,
        packet: &[u8],
        received_at: ReceiveTime,
    ) -> io::Result<TcpPacket> {
        let slot = self
            .frames
            .clone()
            .try_acquire_owned()
            .map_err(|_| io::Error::other("ICE-TCP ingress frame quota full"))?;
        Ok(TcpPacket {
            route,
            packet: packet.to_vec(),
            received_at,
            _slot: slot,
        })
    }
}

pub(crate) struct Inbox {
    pub receiver: mailbox::Receiver<TcpIngress>,
    pub max_frame_bytes: usize,
}

#[allow(
    clippy::disallowed_types,
    reason = "create the TCP-only frame quota once per enabled shard at startup"
)]
pub(crate) fn inbox(capacity: usize, max_frame_bytes: usize) -> io::Result<(InboxSender, Inbox)> {
    let slots = core::num::NonZeroUsize::new(capacity).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "zero ICE-TCP inbox capacity")
    })?;
    let (channel, receiver) = mailbox::bounded(slots);
    Ok((
        InboxSender {
            channel,
            frames: std::sync::Arc::new(Semaphore::new(capacity)),
        },
        Inbox {
            receiver,
            max_frame_bytes,
        },
    ))
}

#[derive(Clone, Copy)]
pub(crate) struct Routing {
    pub cluster_id: u16,
    pub node_id: u16,
    pub shard_count: usize,
}

impl Routing {
    fn hint(&self, packet: &[u8]) -> Option<NodeTransportAddress> {
        let raw = extract_stun_server_ufrag(packet)?;
        let hint = IceUfrag::decode(&raw)?;
        let address = hint.node_address();
        (hint.cluster_id == self.cluster_id
            && hint.node_id == self.node_id
            && address.shard().index() < self.shard_count)
            .then_some(address)
    }
}

pub(crate) struct Service {
    shutdown: CancellationToken,
    #[cfg(not(feature = "sim"))]
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
    #[cfg(feature = "sim")]
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Service {
    #[allow(
        clippy::too_many_arguments,
        reason = "startup transfers distinct immutable configuration and channel endpoints once"
    )]
    pub(crate) async fn start(
        bind: SocketAddr,
        advertised: Vec<SocketAddr>,
        routing: Routing,
        limits: Limits,
        inboxes: Vec<InboxSender>,
        spare_core: Option<core_affinity::CoreId>,
        shutdown: CancellationToken,
    ) -> io::Result<(Self, SocketAddr)> {
        limits.validate(routing.shard_count)?;
        #[cfg(feature = "sim")]
        {
            let _ = spare_core;
            let listener = crate::node::bind_tcp_listener(bind).await?;
            let bound = listener.local_addr()?;
            let task = tokio::spawn(accept_loop(
                listener,
                advertised,
                routing,
                limits,
                inboxes,
                shutdown.clone(),
            ));
            Ok((
                Self {
                    shutdown,
                    task: Some(task),
                },
                bound,
            ))
        }
        #[cfg(not(feature = "sim"))]
        {
            let (ready, result) = oneshot::channel();
            let stop = shutdown.clone();
            let thread = std::thread::Builder::new()
                .name("pb-ice-tcp".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async move {
                        let listener = match tune_thread(spare_core) {
                            Ok(()) => crate::node::bind_tcp_listener(bind).await,
                            Err(error) => Err(error),
                        };
                        let listener = match listener {
                            Ok(listener) => listener,
                            Err(error) => {
                                let _ = ready.send(Err(error));
                                return Ok(());
                            }
                        };
                        let bound = listener.local_addr()?;
                        if ready.send(Ok(bound)).is_ok() {
                            accept_loop(listener, advertised, routing, limits, inboxes, stop).await;
                        }
                        Ok(())
                    })
                })?;
            let service = Self {
                shutdown,
                thread: Some(thread),
            };
            let bound = result
                .await
                .map_err(|_| io::Error::other("ICE-TCP thread failed at startup"))??;
            Ok((service, bound))
        }
    }

    pub(crate) async fn finish(mut self) -> io::Result<()> {
        self.shutdown.cancel();
        #[cfg(feature = "sim")]
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)?;
        }
        #[cfg(not(feature = "sim"))]
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || {
                thread
                    .join()
                    .map_err(|_| io::Error::other("ICE-TCP thread panicked"))?
            })
            .await
            .map_err(io::Error::other)??;
        }
        Ok(())
    }
}

#[cfg(not(feature = "sim"))]
fn tune_thread(spare_core: Option<core_affinity::CoreId>) -> io::Result<()> {
    use thread_priority::{
        NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy, thread_native_id,
    };
    thread_priority::set_thread_priority_and_policy(
        thread_native_id(),
        ThreadPriority::Min,
        ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Idle),
    )
    .map_err(io::Error::other)?;
    if let Some(core) = spare_core
        && !core_affinity::set_for_current(core)
    {
        tracing::warn!(core = core.id, "ICE-TCP spare-core affinity unavailable");
    }
    tracing::info!(?spare_core, "ICE-TCP thread uses SCHED_IDLE");
    Ok(())
}

fn thread_cpu_nanos() -> u64 {
    #[cfg(not(feature = "sim"))]
    {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        u64::try_from(time.tv_sec)
            .unwrap_or_default()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_nsec).unwrap_or_default())
    }
    #[cfg(feature = "sim")]
    {
        0
    }
}

fn observe_cpu(previous: &mut u64) {
    let next = thread_cpu_nanos();
    metrics::counter!("ice_tcp_thread_cpu_ns").increment(next.saturating_sub(*previous));
    *previous = next;
}

#[allow(
    clippy::disallowed_types,
    reason = "semaphores are confined to the low-priority TCP executor and cloned at admission, not on UDP/shard packet paths"
)]
async fn accept_loop(
    listener: TcpListener,
    advertised: Vec<SocketAddr>,
    routing: Routing,
    limits: Limits,
    inboxes: Vec<InboxSender>,
    shutdown: CancellationToken,
) {
    use std::sync::Arc;
    let task_capacity = limits.max_connections.saturating_add(limits.max_pending);
    let tasks = Arc::new(Semaphore::new(task_capacity));
    let pending = Arc::new(Semaphore::new(limits.max_pending));
    let active = Arc::new(Semaphore::new(limits.max_connections));
    let (done_tx, mut done_rx) = mpsc::channel::<IpAddr>(task_capacity);
    let mut ips = HashMap::<IpAddr, usize>::new();
    let mut connections = JoinSet::new();
    let mut cpu_due = tokio::time::interval(Duration::from_secs(1));
    cpu_due.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cpu_sample = thread_cpu_nanos();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = cpu_due.tick() => observe_cpu(&mut cpu_sample),
            Some(ip) = done_rx.recv() => {
                if let Some(count) = ips.get_mut(&ip) {
                    *count = count.saturating_sub(1);
                    if *count == 0 { ips.remove(&ip); }
                }
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result { tracing::error!(?error, "ICE-TCP connection task failed"); }
            }
            accepted = listener.accept() => {
                let Ok((stream, remote)) = accepted else {
                    metrics::counter!("ice_tcp_rejected", "reason" => "accept_error").increment(1);
                    tokio::task::yield_now().await;
                    continue;
                };
                let remote = canonical(remote);
                let ip = remote.ip();
                let permits = tasks.clone().try_acquire_owned().ok()
                    .zip(pending.clone().try_acquire_owned().ok());
                let Some((task_permit, pending_permit)) = permits.filter(|_| ips.get(&ip).copied().unwrap_or(0) < limits.max_per_ip) else {
                    metrics::counter!("ice_tcp_rejected", "reason" => "admission").increment(1);
                    continue;
                };
                let Some(id) = next_flow_id() else { continue; };
                let pending_permit = AdmissionPermit::new(pending_permit, "ice_tcp_pending");
                let count = ips.entry(ip).or_default();
                *count = count.saturating_add(1);
                let active = active.clone();
                let done = done_tx.clone();
                let limits = limits.clone();
                let inboxes = inboxes.clone();
                let advertised = advertised.clone();
                let stop = shutdown.clone();
                connections.spawn(async move {
                    let mut terminal = None;
                    // This scope releases all I/O and socket admission before waiting for the terminal inbox slot.
                    {
                        let _pending = pending_permit;
                        if let Err(error) = flow(stream, remote, id, &advertised, routing, &limits, &inboxes, active, _pending, &stop, &mut terminal).await {
                            metrics::counter!("ice_tcp_flow_close", "reason" => "io_or_policy").increment(1);
                            tracing::debug!(?error, %remote, "ICE-TCP flow closed");
                        }
                    }
                    let _ = done.try_send(ip);
                    if let Some((route, sender)) = terminal {
                        tokio::select! {
                            biased;
                            _ = stop.cancelled() => {},
                            _ = sender.channel.send(TcpIngress::Closed { route }) => {},
                        }
                    }
                    // Keep the task slot while Closed waits, bounding terminal notices even when sockets release admission.
                    drop(task_permit);
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    observe_cpu(&mut cpu_sample);
}

#[allow(
    clippy::too_many_arguments,
    clippy::disallowed_types,
    reason = "one connection task owns its configuration, I/O and admission; the active semaphore stays on the TCP executor"
)]
async fn flow(
    stream: TcpStream,
    remote: SocketAddr,
    id: TcpFlowId,
    advertised: &[SocketAddr],
    routing: Routing,
    limits: &Limits,
    inboxes: &[InboxSender],
    active: std::sync::Arc<Semaphore>,
    pending: AdmissionPermit,
    shutdown: &CancellationToken,
    terminal: &mut Option<(FlowRoute, InboxSender)>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let socket_local = canonical(stream.local_addr()?);
    let local = advertised
        .iter()
        .copied()
        .find(|addr| addr.is_ipv4() == remote.ip().to_canonical().is_ipv4())
        .unwrap_or(socket_local);
    let (mut reader, mut writer) = pulsebeam_tcp::split(stream, limits.max_frame_bytes)?;
    let cancel = shutdown.child_token();
    let handshake = async {
        let mut nulls = 0usize;
        let first = loop {
            let packet = reader.recv().await?.ok_or(io::ErrorKind::UnexpectedEof)?;
            if !packet.is_empty() {
                break packet;
            }
            nulls = nulls.saturating_add(1);
            if nulls > 8 {
                return Err(io::Error::other("too many pre-auth null frames"));
            }
        };
        let received_at = ReceiveTime::now();
        let address = routing
            .hint(&first)
            .ok_or_else(|| io::Error::other("invalid first STUN route hint"))?;
        let active = AdmissionPermit::new(
            active
                .try_acquire_owned()
                .map_err(|_| io::Error::other("ICE-TCP active limit"))?,
            "ice_tcp_active",
        );
        drop(pending);
        let sender = inboxes
            .get(address.shard().index())
            .ok_or(io::ErrorKind::InvalidInput)?
            .clone();
        let route = FlowRoute {
            address,
            id,
            local,
            remote,
        };
        let (outbound, outgoing) = mpsc::channel(limits.outbound_capacity);
        let (reply, adopted) = oneshot::channel();
        let handle = TcpFlow {
            route,
            outbound,
            cancel: cancel.clone(),
            max_frame_bytes: limits.max_frame_bytes,
        };
        let first_stun = sender.packet(route, &first, received_at)?;
        sender
            .channel
            .try_send(TcpIngress::Adopt {
                flow: handle,
                first_stun,
                reply,
            })
            .map_err(|_| io::Error::other("ICE-TCP adoption inbox full or closed"))?;
        *terminal = Some((route, sender.clone()));
        if !adopted.await.unwrap_or(false) {
            return Err(io::Error::other("ICE-TCP adoption rejected"));
        }
        Ok((route, sender, outgoing, active))
    };
    let adopted = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(limits.first_stun_timeout, handshake) => result??,
    };
    let (route, sender, mut outgoing, _active) = adopted;
    let read = async {
        let mut nulls = 0usize;
        loop {
            let packet = match tokio::time::timeout(limits.stalled_io_timeout, reader.recv()).await
            {
                Ok(result) => result?,
                Err(_) if !reader.is_partial() => continue,
                Err(error) => return Err(io::Error::other(error)),
            };
            let Some(packet) = packet else {
                return Ok::<(), io::Error>(());
            };
            if packet.is_empty() {
                nulls = nulls.saturating_add(1);
                if nulls > 8 {
                    return Err(io::Error::other("too many null frames"));
                }
                continue;
            }
            nulls = 0;
            let packet = sender.packet(route, &packet, ReceiveTime::now());
            if packet
                .and_then(|packet| {
                    sender
                        .channel
                        .try_send(TcpIngress::Frame(packet))
                        .map_err(|_| io::Error::other("ICE-TCP ingress full or closed"))
                })
                .is_err()
            {
                metrics::counter!("ice_tcp_ingress_close", "reason" => "full_or_closed")
                    .increment(1);
                return Err(io::Error::other("ICE-TCP ingress full or closed"));
            }
        }
    };
    let write = async {
        while let Some(packet) = outgoing.recv().await {
            match tokio::time::timeout(limits.stalled_io_timeout, writer.send(&packet)).await {
                Ok(result) => result?,
                Err(error) => {
                    metrics::counter!("ice_tcp_stalled_write_close").increment(1);
                    return Err(io::Error::other(error));
                }
            }
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(()),
        result = read => result,
        result = write => result,
    }
}
