//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, cloned at
//! startup and never again. Carries the sanctioned exception in
//! `shard::metrics`; nothing else here may share.

use std::{collections::VecDeque, net::SocketAddr, pin::Pin};

use ahash::{HashMap, HashMapExt};
#[allow(
    clippy::disallowed_types,
    reason = "Arc<ShardMetrics>, one per shard, see module note"
)]
use std::{sync::Arc, time::Duration};

use crate::clock::WallAnchor;
use crate::route::Envelope;

use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
};
use tokio::time::{Instant, Sleep};

use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::{ParticipantConfig, RoutedTrackPacket},
    shard::metrics::ShardMetrics,
    shard::recorder::{ShardRecorder, ShardStatsReport},
    track::Track,
};

use super::core::{ShardCore, ShardTransport};

/// Depth of the shard -> controller topology queue.
///
/// Deliberately large and preallocated. Every entry is one topology change
/// (publish, subscribe, teardown, participant lifecycle) — control-rate events,
/// not per-packet — so a healthy node never comes close.
pub(crate) const SHARD_EVENT_CAPACITY: usize = 65_536;

/// Depth of the controller -> shard command queue.
///
/// Both directions are non-blocking; the controller requeues a full command
/// mailbox and retries on a later tick.
///
/// Both directions read these capacities from here so the queue bounds do not
/// drift apart at their call sites.
pub(crate) const SHARD_COMMAND_CAPACITY: usize = 1024;

/// Depth of the shard -> shard media queue, one per receiving shard.
///
/// This lane is lossy on purpose: it carries forwarded media, and a receiver
/// that has fallen this far behind is better off dropping than accumulating
/// latency it can never pay back. So the depth is a jitter buffer, not a
/// backlog — deep enough to absorb one scheduling hiccup on the receiving
/// core, shallow enough that a stalled shard sheds load promptly instead of
/// holding a second of stale video.
pub(crate) const SHARD_FRAME_CAPACITY: usize = 1024;

pub(crate) const SHARD_UPDATE_CAPACITY: usize = 1024;

pub(crate) const SHARD_COMMAND_BUDGET: usize = 64;
pub(crate) const SHARD_FRAME_BUDGET: usize = 256;
pub(crate) const SHARD_UPDATE_OP_BUDGET: usize = 256;
pub(crate) const SHARD_PLAN_OPERATION_BUDGET: usize = 256;
pub(crate) const SHARD_PIPELINE_BUDGET: usize = 512;
pub(crate) const SHARD_EVENT_BUDGET: usize = 1024;
pub(crate) const SHARD_UDP_BATCH_BUDGET: usize = 256;
pub(crate) const SHARD_PARTICIPANT_BUDGET: usize = 64;

/// How often a shard hands its metrics to the control plane.
///
/// The values are cumulative and absolute, so this is a staleness bound and
/// nothing more — a lost report costs a stale scrape, never a lost count.
pub(crate) const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Depth of the shard -> aggregator metrics queue.
///
/// Shallow on purpose. At one report per second per shard a backlog means the
/// aggregator is wedged, and the newest report supersedes every older one, so
/// queueing them would only export staler numbers more slowly.
pub(crate) const STATS_CAPACITY: usize = 4;

/// A tick busier than this defers its metrics report to the next one.
///
/// The snapshot is sub-microsecond, but there is no reason to spend it on top
/// of a tick that is already long when the next tick will almost certainly be
/// cheaper. [`STATS_DEADLINE_SLACK`] bounds how long that can go on.
const STATS_BUSY_TICK: Duration = Duration::from_micros(200);
const LONG_TICK: Duration = Duration::from_millis(10);

/// How far past its due time a report may be deferred waiting for a cheap
/// tick. A shard saturated for this long reports anyway: stale metrics from a
/// struggling shard are exactly the ones worth having.
const STATS_DEADLINE_SLACK: Duration = STATS_REPORT_INTERVAL;

/// Registered once per shard so `/metrics` carries `# HELP` for what every
/// shard always reports. Repeating these per tick would be harmless but
/// pointless; the recorder ignores a description it already has.
fn describe_shard_metrics() {
    metrics::describe_gauge!(
        "participants_live",
        "participants this shard currently owns"
    );
    metrics::describe_counter!(
        "busy_us",
        metrics::Unit::Microseconds,
        "cumulative time this shard spent processing"
    );
    metrics::describe_counter!(
        "idle_us",
        metrics::Unit::Microseconds,
        "cumulative time this shard spent parked"
    );
    metrics::describe_histogram!(
        "tick_us",
        metrics::Unit::Microseconds,
        "how long one shard loop iteration took"
    );
    metrics::describe_counter!(
        "routing_drop",
        "recoverable packet or topology drops by lane, stage, and origin"
    );
    metrics::describe_counter!(
        "shard_wrong_owner_drop",
        "packets received by a shard that does not own their transport route"
    );
    metrics::describe_counter!(
        "shard_tick_budget_hit",
        "ticks that exhausted a bounded work phase before its queue drained"
    );
    metrics::describe_counter!(
        "shard_long_tick",
        "ticks longer than the realtime latency alarm threshold"
    );
    metrics::describe_counter!(
        "upstream_route_miss",
        "incoming media with no matching upstream SSRC route"
    );
    metrics::describe_counter!(
        "upstream_route_table_full",
        "incoming media whose upstream SSRC route table could not accept a new entry"
    );
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Manager hung up")]
    ManagerDisconnected,
}

/// The only imperative operations the controller may request from a shard.
/// Topology and lifecycle changes are published through `ShardUpdate`.
#[derive(Debug)]
pub(crate) enum ShardCommand {
    #[cfg(feature = "sim")]
    Observe(tokio::sync::oneshot::Sender<crate::sim_observation::ShardObservation>),
    MaterializeParticipant {
        transport: crate::route::NodeTransportAddress,
        config: Box<ParticipantConfig>,
        ack: tokio::sync::oneshot::Sender<bool>,
    },
    AdoptTcpConnection {
        stream: pulsebeam_runtime::net::tcp::BufferedTcpStream,
        peer_addr: std::net::SocketAddr,
        address: crate::route::NodeTransportAddress,
    },
    AuthenticateTransport {
        source: std::net::SocketAddr,
        address: crate::route::NodeTransportAddress,
    },
    AuthorizationResult {
        participant: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        expiry: Option<i64>,
    },
}

pub(crate) type MediaPayload = RoutedTrackPacket;

/// Everything one shard sends another: the data plane.
///
/// Best-effort by construction. Cross-node this becomes a UDP datagram, so
/// nothing that must not be dropped is representable here — topology travels
/// the control plane ([`ShardCommand`] / [`ShardEvent`]) and never this way.
#[allow(
    clippy::large_enum_variant,
    reason = "the payload enum carries materialized TrackPacket values and owned SCTP bytes so cross-shard payloads remain core-local"
)]
pub(crate) enum ShardFrame {
    Ingress {
        batch: RecvPacketBatch,
        address: crate::route::NodeTransportAddress,
        source_shard: ShardId,
    },
    /// Forward payload, addressed by the destination's own route. `TrackId`
    /// validates that the route still names the intended stable entity; the
    /// destination resolves its own local handle from the route entry.
    Media {
        env: Envelope,
        payload: MediaPayload,
    },
    Reverse {
        env: Envelope,
        packet: crate::participant::reverse::ReversePacket,
    },
}

pub(crate) struct RenewalToken(pub(crate) String);

impl std::fmt::Debug for RenewalToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

pub(crate) type ShardEventMessage = (ShardId, ShardEvent);

/// Runtime facts observed by a shard. The controller converts these facts into
/// canonical state and publishes the resulting execution image.
#[derive(Debug)]
pub(crate) enum ShardEvent {
    TransportAuthenticated {
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: ShardId,
        address: crate::route::NodeTransportAddress,
        shard: ShardId,
    },
    ParticipantClosed {
        participant: ParticipantId,
        connection_id: crate::entity::ConnectionId,
    },
    RenewAuthorization {
        participant: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        token: RenewalToken,
    },
    TrackSubscribed {
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        track: crate::track::TrackMeta,
    },
    TrackUnsubscribed {
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        track: crate::track::TrackMeta,
    },
    TrackSubscriptionAdded {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        selector: crate::track::TrackSelector,
        selection: crate::track::SelectionPolicy,
    },
    TrackSubscriptionRemoved {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        selector: crate::track::TrackSelector,
    },
    TrackPublished(Track),
    TrackUnpublished(crate::track::TrackMeta),
}

#[derive(Clone)]
pub(crate) struct ShardContext {
    pub(crate) command_tx: mailbox::Sender<ShardCommand>,
    #[allow(
        clippy::disallowed_types,
        reason = "Arc<ShardMetrics>, one per shard, see module note"
    )]
    pub(crate) metrics: Arc<ShardMetrics>,
}

/// Carries the best-effort lane over in-process channels. Cross-node this
/// becomes a UDP datagram of `[envelope || payload]`; it is the only
/// implementation until then.
struct ChannelTransport {
    shard_id: ShardId,
    frame_txs: Vec<mailbox::Sender<ShardFrame>>,
}

impl ChannelTransport {
    fn enqueue(&self, dst: ShardId, ev: ShardFrame) -> bool {
        let Some(tx) = self.frame_txs.get(dst.index()) else {
            debug_assert!(
                false,
                "shard {} is not in this node's frame table",
                dst.index()
            );
            return false;
        };
        tx.try_send(ev).is_ok()
    }
}

impl ShardTransport for ChannelTransport {
    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload) {
        // Dropping under backpressure is the media contract: this lane is
        // lossy by design, and `link_seq` makes the loss visible downstream.
        let _ = self.enqueue(dst, ShardFrame::Media { env, payload });
    }

    fn send_frame(&self, dst: ShardId, frame: ShardFrame) {
        // Same best-effort contract as media, but these are rare enough that a
        // drop is worth seeing: it means the queue is saturated by media.
        if !self.enqueue(dst, frame) {
            tracing::debug!(
                from = %self.shard_id,
                %dst,
                "dropped a cross-shard frame; the queue is full"
            );
        }
    }
}

struct TcpFlowRoutes {
    flows: HashMap<net::tcp::TcpFlowId, (SocketAddr, crate::route::NodeTransportAddress)>,
    transports: HashMap<crate::route::NodeTransportAddress, Vec<(SocketAddr, net::tcp::TcpFlowId)>>,
}

impl TcpFlowRoutes {
    fn new() -> Self {
        Self {
            flows: HashMap::new(),
            transports: HashMap::new(),
        }
    }

    fn insert(
        &mut self,
        flow: net::tcp::TcpFlowId,
        peer: SocketAddr,
        address: crate::route::NodeTransportAddress,
    ) {
        let previous = self.flows.insert(flow, (peer, address));
        debug_assert!(previous.is_none(), "TCP flow identities must not be reused");
        self.transports
            .entry(address)
            .or_default()
            .push((peer, flow));
    }

    fn route(&self, batch: &RecvPacketBatch) -> Option<crate::route::NodeTransportAddress> {
        if batch.transport != net::Transport::Tcp {
            return None;
        }
        let (peer, address) = self.flows.get(&batch.tcp_flow?)?;
        (*peer == batch.src).then_some(*address)
    }

    fn retire(
        &mut self,
        flow: net::tcp::TcpFlowId,
    ) -> Option<(SocketAddr, crate::route::NodeTransportAddress)> {
        let (peer, address) = self.flows.remove(&flow)?;
        if let Some(flows) = self.transports.get_mut(&address) {
            flows.retain(|(_, installed)| *installed != flow);
            if flows.is_empty() {
                self.transports.remove(&address);
            }
        }
        Some((peer, address))
    }

    fn close_transport(
        &mut self,
        address: crate::route::NodeTransportAddress,
        socket: &mut net::tcp::TcpTransport,
    ) {
        if let Some(flows) = self.transports.remove(&address) {
            for (peer, flow) in flows {
                self.flows.remove(&flow);
                socket.close_flow(&peer, flow);
            }
        }
    }
}

pub(crate) struct ShardWorker {
    core: ShardCore,
    recv_batch: Vec<RecvPacketBatch>,
    tcp_recv_batch: Vec<RecvPacketBatch>,
    tcp_closed_batch: Vec<(SocketAddr, net::tcp::TcpFlowId)>,
    tcp_routes: TcpFlowRoutes,
    udp_socket: UnifiedSocket,
    tcp_socket: net::tcp::TcpTransport,
    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEventMessage>,
    shard_event_backlog: VecDeque<ShardEvent>,
    frame_rx: mailbox::Receiver<ShardFrame>,
    frame_batch: Vec<ShardFrame>,
    router: ChannelTransport,
    #[allow(
        clippy::disallowed_types,
        reason = "Arc<ShardMetrics>, one per shard, see module note"
    )]
    metrics: Arc<ShardMetrics>,
    health_metrics: ShardHealthMetrics,
    #[allow(
        clippy::disallowed_types,
        reason = "Arc<ShardRecorder>, only cloned at init"
    )]
    recorder: Arc<ShardRecorder>,
    stats_tx: Option<mailbox::Sender<Box<ShardStatsReport>>>,
    stats_due: Instant,
    last_busy: Duration,
}

struct ShardHealthMetrics {
    participants_live: metrics::Gauge,
    busy_us: metrics::Counter,
    idle_us: metrics::Counter,
    tick_us: metrics::Histogram,
    shard_long_tick: metrics::Counter,
}

impl ShardHealthMetrics {
    fn register(recorder: &ShardRecorder) -> Self {
        metrics::with_local_recorder(recorder, || Self {
            participants_live: metrics::gauge!("participants_live"),
            busy_us: metrics::counter!("busy_us"),
            idle_us: metrics::counter!("idle_us"),
            tick_us: metrics::histogram!("tick_us"),
            shard_long_tick: metrics::counter!("shard_long_tick"),
        })
    }

    fn observe(&self, participants: usize, busy_us: u64, idle_us: u64, previous_busy: Duration) {
        self.participants_live.set(participants as f64);
        self.busy_us.absolute(busy_us);
        self.idle_us.absolute(idle_us);
        self.tick_us
            .record(u64::try_from(previous_busy.as_micros()).unwrap_or(u64::MAX) as f64);
        if previous_busy >= LONG_TICK {
            self.shard_long_tick.increment(1);
        }
    }
}

impl ShardWorker {
    // Every argument is a distinct resource the worker takes ownership of —
    // two sockets, three channel ends, metrics, RNG, clock. Grouping them into
    // a parameter struct would move the same list one level out and add a type
    // whose only purpose is to be destructured here.
    #[allow(
        clippy::too_many_arguments,
        clippy::disallowed_types,
        reason = "Arc<ShardMetrics>, one per shard, see module note"
    )]
    pub(crate) fn new(
        shard_id: ShardId,
        udp_socket: UnifiedSocket,
        tcp_socket: net::tcp::TcpTransport,
        command_rx: mailbox::Receiver<ShardCommand>,
        update_rx: mailbox::Receiver<Box<crate::shard_update::ShardUpdate>>,
        event_tx: mailbox::Sender<ShardEventMessage>,
        frame_rx: mailbox::Receiver<ShardFrame>,
        frame_txs: Vec<mailbox::Sender<ShardFrame>>,
        metrics: Arc<ShardMetrics>,
        stats_tx: Option<mailbox::Sender<Box<ShardStatsReport>>>,
        wall: WallAnchor,
    ) -> Self {
        let shard_count = frame_txs.len();
        let core = ShardCore::new(
            shard_id,
            udp_socket.max_gso_segments(),
            shard_count,
            wall,
            update_rx,
        );
        let router = ChannelTransport {
            shard_id,
            frame_txs,
        };
        let recorder = ShardRecorder::shared();
        metrics::with_local_recorder(&*recorder, describe_shard_metrics);
        let health_metrics = ShardHealthMetrics::register(&recorder);

        Self {
            core,
            recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            tcp_recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            tcp_closed_batch: Vec::with_capacity(net::BATCH_SIZE),
            tcp_routes: TcpFlowRoutes::new(),
            udp_socket,
            tcp_socket,
            command_rx,
            event_tx,
            shard_event_backlog: VecDeque::new(),
            frame_rx,
            frame_batch: Vec::with_capacity(SHARD_FRAME_BUDGET),
            router,
            metrics,
            health_metrics,
            recorder,
            stats_tx,
            stats_due: Instant::now()
                .checked_add(STATS_REPORT_INTERVAL)
                .unwrap_or_else(Instant::now),
            last_busy: Duration::ZERO,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = %self.router.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        let recorder = &self.recorder.clone();
        let sleep = tokio::time::sleep(tokio::time::Duration::MAX);
        tokio::pin!(sleep);

        let mut loop_start = Instant::now();
        loop {
            self.wait_for_inputs(sleep.as_mut()).await?;

            let at = pulsebeam_runtime::net::ReceiveTime::now();
            let busy_start = at.monotonic;
            self.metrics
                .record_idle(busy_start.saturating_duration_since(loop_start));

            let previous_busy = self.last_busy;
            metrics::with_local_recorder(recorder, || {
                self.observe_health(previous_busy);
                self.tick(at);
                self.flush_shard_events()
            })?;

            let busy_end = Instant::now();
            loop_start = busy_end;
            let busy_duration = busy_end.duration_since(busy_start);
            self.metrics.record_busy(busy_duration);
            self.last_busy = busy_duration;

            self.report_stats(busy_end, busy_duration);
        }
    }

    /// What every shard reports whether or not it is carrying traffic.
    ///
    /// Without these a quiet shard registers no metrics at all, and `/metrics`
    /// cannot distinguish a healthy idle shard from one that died — which is
    /// the first question worth asking of a thread-per-core node.
    ///
    /// Recorded for the previous tick so it costs one recorder scope per loop
    /// rather than two.
    fn observe_health(&self, previous_busy: Duration) {
        let (busy_us, idle_us) = self.metrics.read_raw();
        self.health_metrics.observe(
            self.core.participant_count(),
            busy_us,
            idle_us,
            previous_busy,
        );
    }

    /// Hand this shard's cumulative metrics to the control plane.
    ///
    /// Runs after the tick's work is done and the shard is on its way back to
    /// the park, never between packets. It waits for a cheap tick when it can,
    /// so the snapshot lands in slack rather than on top of a long tick, and
    /// gives up waiting after [`STATS_DEADLINE_SLACK`] so a saturated shard —
    /// the one whose numbers matter most — still reports.
    fn report_stats(&mut self, now: Instant, busy: Duration) {
        let Some(tx) = &self.stats_tx else {
            return;
        };
        if now < self.stats_due {
            return;
        }
        let hard_deadline = self.stats_due.checked_add(STATS_DEADLINE_SLACK);
        if busy > STATS_BUSY_TICK && hard_deadline.is_some_and(|hard| now < hard) {
            return;
        }

        // Dropping is free: the values are cumulative and absolute, so the
        // next report carries everything this one would have.
        let _ = tx.try_send(Box::new(self.recorder.snapshot(self.router.shard_id)));
        self.stats_due = now.checked_add(STATS_REPORT_INTERVAL).unwrap_or(now);
    }

    fn next_wait_deadline(&mut self) -> Option<Instant> {
        // A quiet shard must still wake to report, or its metrics would freeze
        // at whatever it last had traffic for.
        let stats_deadline = self.stats_tx.as_ref().map(|_| self.stats_due);
        match (self.core.next_timer_deadline(), stats_deadline) {
            (Some(timer), Some(stats)) => Some(timer.min(stats)),
            (timer, stats) => timer.or(stats),
        }
    }

    async fn wait_for_inputs(&mut self, mut sleep: Pin<&mut Sleep>) -> Result<(), ShardError> {
        if !self.shard_event_backlog.is_empty() {
            return Ok(());
        }
        let deadline = self.next_wait_deadline();
        let has_timer = if let Some(d) = deadline {
            sleep.as_mut().reset(d);
            true
        } else {
            false
        };

        // Block until at least one source is ready.
        tokio::select! {
            biased;
            Some(_) = self.core.update_readable() => {}
            Ok(_) = self.udp_socket.readable() => {}
            Some(_) = self.frame_rx.readable() => {}
            Ok(_) = self.tcp_socket.readable() => {}
            Some(_) = self.command_rx.readable() => {}
            _ = sleep.as_mut(), if has_timer => {}
            else => return Err(ShardError::ManagerDisconnected),
        }

        Ok(())
    }

    fn tick(&mut self, at: pulsebeam_runtime::net::ReceiveTime) {
        let now = self.core.observe_execution_time(at).monotonic;
        // phase 1: input
        let mut commands: usize = 0;
        for _ in 0..SHARD_COMMAND_BUDGET {
            let Ok(cmd) = self.command_rx.try_recv() else {
                break;
            };
            commands = commands.saturating_add(1);
            match cmd {
                ShardCommand::AdoptTcpConnection {
                    stream,
                    peer_addr,
                    address,
                } => {
                    if !self.core.has_transport(address) {
                        continue;
                    }
                    match self.tcp_socket.add_connection(stream, peer_addr) {
                        Ok(flow) => {
                            self.core.bind_tcp_flow(address, peer_addr, flow);
                            self.tcp_routes.insert(flow, peer_addr, address);
                        }
                        Err(err) => {
                            tracing::warn!(%peer_addr, error = ?err, "Failed to add new TCP connection to shard");
                        }
                    }
                }
                cmd => {
                    let _ = self.core.on_command(cmd, &self.router);
                }
            }
        }
        if commands == SHARD_COMMAND_BUDGET {
            self.tick_budget_hit("commands");
        }
        if self.core.apply_updates(SHARD_UPDATE_OP_BUDGET) >= SHARD_UPDATE_OP_BUDGET {
            self.tick_budget_hit("shard_update");
        }
        let mut frames: usize = 0;
        self.frame_batch.clear();
        for _ in 0..SHARD_FRAME_BUDGET {
            let Ok(ev) = self.frame_rx.try_recv() else {
                break;
            };
            frames = frames.saturating_add(1);
            self.frame_batch.push(ev);
        }
        self.core
            .on_shard_frames(self.frame_batch.drain(..), now, &self.router);
        if frames == SHARD_FRAME_BUDGET {
            self.tick_budget_hit("frames");
        }
        let _ = self.udp_socket.try_recv_batch(&mut self.recv_batch);
        let received = self.recv_batch.len();
        for batch in self
            .recv_batch
            .drain(..SHARD_UDP_BATCH_BUDGET.min(received))
        {
            self.core.on_udp_batch_routed(batch, &self.router);
        }
        if received >= SHARD_UDP_BATCH_BUDGET {
            self.tick_budget_hit("udp");
        }

        self.tcp_recv_batch.clear();
        self.tcp_closed_batch.clear();
        let tcp_events = self
            .tcp_socket
            .try_recv_events(&mut self.tcp_recv_batch, &mut self.tcp_closed_batch)
            .unwrap_or(0);
        for batch in self.tcp_recv_batch.drain(..) {
            if let Some(address) = self.tcp_routes.route(&batch) {
                self.core.on_tcp_batch(batch, address);
            } else {
                metrics::counter!("shard_tcp_retired_input_drop").increment(1);
            }
        }
        // Complete frames precede their EOF notice and retain their original transport address.
        for (_, flow) in self.tcp_closed_batch.drain(..) {
            if let Some((peer, address)) = self.tcp_routes.retire(flow) {
                self.core.retire_tcp_flow(address, peer, flow);
            }
        }
        if tcp_events == net::BATCH_SIZE {
            self.tick_budget_hit("tcp");
        }

        if self.core.poll_and_flush_dirty(
            &mut self.udp_socket,
            &mut self.tcp_socket,
            SHARD_PARTICIPANT_BUDGET,
        ) == SHARD_PARTICIPANT_BUDGET
        {
            self.tick_budget_hit("participants");
        }
        if self
            .core
            .flush_stream_buffers(&self.router, SHARD_PIPELINE_BUDGET)
            == SHARD_PIPELINE_BUDGET
        {
            self.tick_budget_hit("pipeline");
        }
        if self.core.poll_and_flush_dirty(
            &mut self.udp_socket,
            &mut self.tcp_socket,
            SHARD_PARTICIPANT_BUDGET,
        ) == SHARD_PARTICIPANT_BUDGET
        {
            self.tick_budget_hit("participants");
        }
        if self
            .core
            .flush_participant_events(&self.router, SHARD_PIPELINE_BUDGET)
            == SHARD_PIPELINE_BUDGET
        {
            self.tick_budget_hit("participant_events");
        }

        for (address, peers) in self.core.drain_pending_close() {
            for peer in peers {
                self.udp_socket.close_peer(&peer);
            }
            self.tcp_routes
                .close_transport(address, &mut self.tcp_socket);
        }
    }

    /// Hand this tick's topology events to the controller without dropping
    /// lifecycle state when the controller mailbox is temporarily full.
    fn flush_shard_events(&mut self) -> Result<(), ShardError> {
        for _ in 0..SHARD_EVENT_BUDGET {
            let event = self
                .shard_event_backlog
                .pop_front()
                .or_else(|| self.core.pop_shard_event());
            let Some(event) = event else {
                break;
            };
            match self.event_tx.try_send((self.router.shard_id, event)) {
                Ok(()) => {}
                Err(mailbox::TrySendError::Closed(_)) => {
                    tracing::warn!("shard event channel is closed, exiting");
                    return Err(ShardError::ManagerDisconnected);
                }
                Err(mailbox::TrySendError::Full(ev)) => {
                    self.shard_event_backlog.push_front(ev.1);
                    break;
                }
            }
        }

        if !self.shard_event_backlog.is_empty() || self.core.has_pending_events() {
            self.tick_budget_hit("shard_events");
        }

        Ok(())
    }

    fn tick_budget_hit(&self, phase: &'static str) {
        metrics::counter!("shard_tick_budget_hit", "phase" => phase).increment(1);
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_counter("shard_tick_budget_hit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::recorder::MetricSeries;

    fn find(keys: &[MetricSeries], name: &str) -> usize {
        for (idx, key) in keys.iter().enumerate() {
            if key.name == name {
                return idx;
            }
        }
        panic!("missing metric {name}");
    }

    #[test]
    fn tcp_eof_preserves_decoded_frames_without_rebinding_retired_input_or_closes() {
        use crate::route::{NodeTransportAddress, TransportRoute};
        use pulsebeam_core::net::{TcpListener, TcpStream};
        use pulsebeam_runtime::net::tcp::{BufferedTcpStream, TcpTransport};
        use std::net::Ipv4Addr;

        let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.15");
        pulsebeam_runtime::testing::run_local(host, async move {
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .unwrap();
            let local = listener.local_addr().unwrap();
            let (client, accepted) = tokio::join!(TcpStream::connect(local), listener.accept());
            let client = client.unwrap();
            let (stream, peer) = accepted.unwrap();
            let mut socket = TcpTransport::new(local);
            let at = net::ReceiveTime::now();
            let original = socket
                .add_connection(
                    BufferedTcpStream::with_buffered(stream, b"\0\x03old".to_vec(), at),
                    peer,
                )
                .unwrap();
            let route = TransportRoute::new(ShardId::new(0), 7);
            let original_address = NodeTransportAddress::new(route, 1);
            let replacement_address = NodeTransportAddress::new(route, 2);
            let mut routes = TcpFlowRoutes::new();
            routes.insert(original, peer, original_address);
            drop(client);

            let mut frames = Vec::new();
            let mut closed = Vec::new();
            let mut delivered = Vec::new();
            let mut retired_input = None;
            while closed.is_empty() {
                socket.readable().await.unwrap();
                let _ = socket.try_recv_events(&mut frames, &mut closed);
                assert!(frames.len().saturating_add(closed.len()) <= net::BATCH_SIZE);
                for frame in frames.drain(..) {
                    assert_eq!(routes.route(&frame), Some(original_address));
                    assert_eq!(frame.received_at, at);
                    delivered.extend_from_slice(frame.data());
                    retired_input = Some(frame);
                }
            }
            assert_eq!(delivered, b"old");
            assert_eq!(closed, vec![(peer, original)]);
            for (_, flow) in closed.drain(..) {
                assert_eq!(routes.retire(flow), Some((peer, original_address)));
            }
            assert_eq!(routes.route(retired_input.as_ref().unwrap()), None);

            let (client, accepted) = tokio::join!(TcpStream::connect(local), listener.accept());
            let _client = client.unwrap();
            let (stream, _) = accepted.unwrap();
            let replacement = socket
                .add_connection(
                    BufferedTcpStream::with_buffered(stream, b"\0\x03new".to_vec(), at),
                    peer,
                )
                .unwrap();
            routes.insert(replacement, peer, replacement_address);
            assert!(routes.retire(original).is_none());
            routes.close_transport(original_address, &mut socket);
            assert_eq!(socket.flow_for_peer(&peer), Some(replacement));
            assert_eq!(routes.route(retired_input.as_ref().unwrap()), None);
            socket.try_recv_events(&mut frames, &mut closed).unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(routes.route(&frames[0]), Some(replacement_address));
            assert_eq!(frames[0].data(), b"new");
            assert!(closed.is_empty());
            routes.close_transport(replacement_address, &mut socket);
            assert_eq!(socket.flow_for_peer(&peer), None);
            assert!(routes.flows.is_empty());
            assert!(routes.transports.is_empty());
        });
    }

    #[test]
    fn cached_health_handles_preserve_observations() {
        let recorder = ShardRecorder::new();
        let health = ShardHealthMetrics::register(&recorder);

        health.observe(4, 100, 200, Duration::from_micros(50));
        health.observe(7, 300, 400, LONG_TICK);

        let report = recorder.snapshot(ShardId::new(0));
        let schema = report.schema.expect("first report carries schema");
        let participants = find(&schema.gauges, "participants_live");
        let busy = find(&schema.counters, "busy_us");
        let idle = find(&schema.counters, "idle_us");
        let long_tick = find(&schema.counters, "shard_long_tick");
        let tick = find(&schema.histograms, "tick_us");

        assert_eq!(report.gauges[participants], 7.0);
        assert_eq!(report.counters[busy], 300);
        assert_eq!(report.counters[idle], 400);
        assert_eq!(report.counters[long_tick], 1);
        assert_eq!(report.histograms[tick].count, 2);
        assert_eq!(
            report.histograms[tick].sum,
            50 + u64::try_from(LONG_TICK.as_micros()).expect("long tick fits microseconds")
        );
    }
}
