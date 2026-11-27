//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, cloned at
//! startup and never again. Carries the sanctioned exception in
//! `shard::metrics`; nothing else here may share.

use pulsebeam_runtime::rt::{ManagedIo, Service};
use std::{
    collections::VecDeque,
    future::Future,
    task::{Context, Poll},
};

#[allow(
    clippy::disallowed_types,
    reason = "Arc<ShardMetrics>, one per shard, see module note"
)]
use std::{sync::Arc, time::Duration};

use crate::clock::WallAnchor;

use pulsebeam_queue as mailbox;
use pulsebeam_runtime::net::{self, RecvPacketBatch};
use pulsebeam_runtime::time::Instant;

use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::ConnectionConfig,
    shard::metrics::ShardMetrics,
    shard::recorder::{ShardRecorder, ShardStatsReport},
    track::Track,
};

use super::core::{IngressTransport, ShardCore};

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
    crate::control::native::handoff::describe_metrics();
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
        "fabric_rtp_branch_admissions",
        "Fabric RTP branches committed by their first successful bounded RTC enqueue"
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
    MaterializeQuic {
        association: std::num::NonZeroU64,
        connection: crate::entity::ConnectionId,
        participant: crate::entity::ParticipantId,
        transport: crate::route::NodeTransportAddress,
        handoff: Box<crate::control::native::handoff::EndpointHandoff>,
        grant: super::participants::QuicGrant,
        scope: crate::entity::ConnectionScope,
        ack: pulsebeam_runtime::sync::oneshot::Sender<bool>,
    },
    MaterializeParticipant {
        transport: crate::route::NodeTransportAddress,
        config: Box<ConnectionConfig>,
        ack: pulsebeam_runtime::sync::oneshot::Sender<bool>,
    },
    AuthenticateTransport {
        source: std::net::SocketAddr,
        address: crate::route::NodeTransportAddress,
    },
    NativeReady {
        connection_id: crate::entity::ConnectionId,
    },
    PublicationCutoffResult {
        connection_id: crate::entity::ConnectionId,
        reference: u64,
        success: bool,
    },
    AuthorizationResult {
        connection_id: crate::entity::ConnectionId,
        revision: u64,
        expiry: Option<i64>,
    },
}

/// Everything one shard sends another: the data plane.
///
/// Best-effort by construction. Cross-node this becomes a UDP datagram, so
/// nothing that must not be dropped is representable here — topology travels
/// the control plane ([`ShardCommand`] / [`ShardEvent`]) and never this way.
#[allow(
    clippy::large_enum_variant,
    reason = "typed shard work retains owned payloads for core-local processing"
)]
#[derive(Clone, Debug)]
pub(crate) enum ShardFrame {
    Ingress {
        batch: RecvPacketBatch,
        address: crate::route::NodeTransportAddress,
        source_shard: ShardId,
    },
    QuicIngress {
        shard: ShardId,
        datagram: NativeDatagram,
    },
    Fabric(crate::route::fabric::ShardOffer),
    FabricDataExpectation(crate::route::fabric::ShardDataExpectation),
    FabricDataReceipt(crate::route::fabric::ShardDataReceipt),
    FabricDataRepair(crate::route::fabric::ShardDataRepair),
    FabricDataRepairImage(crate::route::fabric::ShardDataRepairImage),
    FabricRefresh(crate::route::fabric::ShardBinding),
    FabricWanImage {
        ingress: pulsebeam_link::FabricIngress,
        image: crate::route::fabric::NetworkPacket,
    },
    Reverse {
        address: crate::route::NodeRouteAddress,
        packet: crate::participant::reverse::ReversePacket,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct NativeDatagram {
    source: std::net::SocketAddr,
    destination: std::net::SocketAddr,
    received_at: net::ReceiveTime,
    ecn: Option<pulsebeam_udp::Ecn>,
    payload: Box<[u8]>,
}

impl NativeDatagram {
    fn new(item: pulsebeam_udp::ReceivedDatagram<'_>) -> Self {
        Self {
            source: item.source,
            destination: item.destination,
            received_at: item.received_at,
            ecn: item.ecn,
            payload: item.payload.into(),
        }
    }

    fn as_received(&self) -> pulsebeam_udp::ReceivedDatagram<'_> {
        pulsebeam_udp::ReceivedDatagram {
            source: self.source,
            destination: self.destination,
            received_at: self.received_at,
            ecn: self.ecn,
            payload: &self.payload,
        }
    }
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
    LocalFabricRtpSourceReady {
        meta: crate::track::TrackMeta,
        encoding: crate::route::fabric::RtpSourceEncoding,
        incarnation: std::num::NonZeroU64,
        emission: crate::route::fabric::local::Emission,
    },
    ShardDataReceiverReady(crate::route::fabric::ShardDataReady),
    PublisherDataExpectation(crate::route::fabric::PublisherExpectation),
    NetworkDataRepair(Box<crate::route::fabric::network_repair::InterestEvent>),
    NetworkMediaRepair(Box<crate::route::fabric::network_repair::MediaInterest>),
    NetworkMediaRefresh(Box<crate::route::fabric::network_repair::MediaInterest>),
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
    OwnedSubscriptionSet {
        connection_id: crate::entity::ConnectionId,
        request: crate::control::topology::OwnedSubscription,
    },
    OwnedSubscriptionRemoved {
        source_room: crate::entity::ProjectRoomKey,
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        reference: u64,
    },
    #[cfg(test)]
    TrackSubscriptionAdded {
        room_id: crate::entity::ProjectRoomKey,
        subscriber: ParticipantId,
        connection_id: crate::entity::ConnectionId,
        selector: crate::track::TrackSelector,
        selection: crate::track::SelectionPolicy,
    },
    TrackPublished(Track),
    TrackUnpublished(crate::track::TrackMeta),
    OwnedTrackUnpublished {
        meta: crate::track::TrackMeta,
        reference: u64,
    },
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

struct LinkTransport {
    shard_id: ShardId,
    frame_txs: Vec<pulsebeam_link::LinkTx<ShardFrame>>,
    clock_origin: Instant,
}

impl LinkTransport {
    fn enqueue(&self, dst: ShardId, ev: ShardFrame) -> bool {
        let Some(tx) = self.frame_txs.get(dst.index()) else {
            debug_assert!(
                false,
                "shard {} is not in this node's frame table",
                dst.index()
            );
            return false;
        };
        send_link_frame(tx, self.clock_origin, ev)
    }
}

pub(crate) fn send_link_frame(
    tx: &pulsebeam_link::LinkTx<ShardFrame>,
    clock_origin: Instant,
    ev: ShardFrame,
) -> bool {
    let (monotonic, wall) = match &ev {
        ShardFrame::Ingress { batch, .. } => {
            (batch.received_at.monotonic, Some(batch.received_at.wall))
        }
        ShardFrame::QuicIngress { shard, datagram } => {
            if tx.local_delivery_tag() != Some(shard_tag(*shard)) {
                return false;
            }
            (
                datagram.received_at.monotonic,
                Some(datagram.received_at.wall),
            )
        }
        ShardFrame::Fabric(offer) => (offer.arrival, None),
        ShardFrame::FabricDataExpectation(notice) => {
            if tx.local_delivery_tag()
                != Some(crate::link::delivery_tag(notice.binding.destination))
            {
                return false;
            }
            (Instant::now(), None)
        }
        ShardFrame::FabricDataRepair(repair) => {
            if tx.local_delivery_tag() != Some(shard_tag(repair.binding.origin)) {
                return false;
            }
            (Instant::now(), None)
        }
        ShardFrame::FabricDataRepairImage(image) => {
            if tx.local_delivery_tag()
                != Some(crate::link::delivery_tag(image.repair.binding.destination))
            {
                return false;
            }
            (Instant::now(), None)
        }
        ShardFrame::FabricDataReceipt(receipt) => {
            if tx.local_delivery_tag() != Some(shard_tag(receipt.binding.origin)) {
                return false;
            }
            (Instant::now(), None)
        }
        ShardFrame::FabricWanImage { .. } => return false,
        ShardFrame::Reverse { .. } | ShardFrame::FabricRefresh(_) => (Instant::now(), None),
    };
    let received_at = pulsebeam_link::ReceiveTime {
        monotonic: pulsebeam_link::LinkTime::from_nanos(
            u64::try_from(monotonic.saturating_duration_since(clock_origin).as_nanos())
                .unwrap_or(u64::MAX),
        ),
        wall,
    };
    tx.try_send_received(
        ev,
        received_at,
        pulsebeam_link::LinkTime::from_nanos(u64::MAX),
    )
    .is_ok()
}

pub(super) fn bind_received_frame(
    shard: ShardId,
    received: &mut pulsebeam_link::Received<ShardFrame>,
) -> bool {
    let tagged_route =
        || crate::link::delivery_address(received.tag).filter(|address| address.shard() == shard);
    match &mut received.packet {
        ShardFrame::Fabric(offer) => tagged_route() == Some(offer.binding.destination),
        ShardFrame::FabricDataExpectation(notice) => {
            tagged_route() == Some(notice.binding.destination)
        }
        ShardFrame::FabricDataRepair(repair) => {
            repair.binding.origin == shard
                && repair.binding.destination.shard() != shard
                && received.tag == shard_tag(shard)
        }
        ShardFrame::FabricDataRepairImage(image) => {
            tagged_route() == Some(image.repair.binding.destination)
        }
        ShardFrame::FabricDataReceipt(receipt) => {
            receipt.binding.origin == shard
                && receipt.binding.destination.shard() != shard
                && received.tag == shard_tag(shard)
        }
        ShardFrame::FabricRefresh(binding) => {
            binding.origin == shard
                && binding.destination.shard() != shard
                && received.tag == shard_tag(shard)
        }
        ShardFrame::FabricWanImage { .. } => false,
        ShardFrame::Reverse { address, .. } => {
            let Some(destination) = tagged_route() else {
                return false;
            };
            *address = destination;
            true
        }
        ShardFrame::Ingress { .. } => true,
        ShardFrame::QuicIngress { shard: target, .. } => {
            *target == shard && received.tag == shard_tag(shard)
        }
    }
}

fn shard_tag(shard: ShardId) -> pulsebeam_link::DeliveryTag {
    pulsebeam_link::DeliveryTag::from_raw(u64::try_from(shard.index()).unwrap_or(u64::MAX))
}

impl IngressTransport for LinkTransport {
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

pub(crate) struct ShardWorker {
    core: ShardCore,
    quic: Option<super::quic::QuicShard>,
    recv_batch: Vec<RecvPacketBatch>,
    tcp_inbox: Option<crate::ice_tcp::Inbox>,
    loop_start: Instant,
    command_rx: mailbox::Receiver<ShardCommand>,
    clock_rx: mailbox::Receiver<crate::clock::reference::Snapshot>,
    event_tx: mailbox::Sender<ShardEventMessage>,
    shard_event_backlog: VecDeque<ShardEvent>,
    frame_rx: pulsebeam_link::LinkRx<ShardFrame>,
    frame_batch: Vec<pulsebeam_link::Received<ShardFrame>>,
    router: LinkTransport,
    #[allow(
        clippy::disallowed_types,
        reason = "Arc<ShardMetrics>, one per shard, see module note"
    )]
    metrics: Arc<ShardMetrics>,
    health_metrics: ShardHealthMetrics,
    #[allow(
        clippy::disallowed_types,
        reason = "shard-owned recorder; temporary clones never escape a poll"
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

impl Service for ShardWorker {
    fn poll_service(
        &mut self,
        io: &mut ManagedIo,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let ready = {
            let wait = self.wait_for_inputs(io);
            pulsebeam_runtime::pin!(wait);
            wait.as_mut().poll(cx)
        };
        match ready {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
            Poll::Ready(Ok(())) => {}
        }
        let at = pulsebeam_runtime::net::ReceiveTime::now();
        let busy_start = at.monotonic;
        self.metrics
            .record_idle(busy_start.saturating_duration_since(self.loop_start));
        let previous_busy = self.last_busy;
        let recorder = self.recorder.clone();
        let result = metrics::with_local_recorder(&*recorder, || {
            self.observe_health(previous_busy);
            self.tick(at, io.socket())?;
            if let Some(quic) = &mut self.quic {
                quic.poll(
                    self.router.shard_id,
                    io.socket(),
                    Instant::now(),
                    &mut self.shard_event_backlog,
                    &mut self.core,
                )?;
            }
            self.flush_shard_events()
        });
        if let Err(error) = result {
            return Poll::Ready(Err(std::io::Error::other(error)));
        }
        let end = Instant::now();
        self.loop_start = end;
        self.last_busy = end.saturating_duration_since(busy_start);
        self.metrics.record_busy(self.last_busy);
        self.report_stats(end, self.last_busy);
        // End every bounded tick at an explicit poll boundary, even under permanent readiness.
        io.yield_after_tick(cx);
        Poll::Pending
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
        max_gso_segments: usize,
        tcp_inbox: Option<crate::ice_tcp::Inbox>,
        command_rx: mailbox::Receiver<ShardCommand>,
        update_rx: mailbox::Receiver<Box<crate::shard_update::ShardUpdate>>,
        event_tx: mailbox::Sender<ShardEventMessage>,
        frame_rx: pulsebeam_link::LinkRx<ShardFrame>,
        frame_txs: Vec<pulsebeam_link::LinkTx<ShardFrame>>,
        clock_origin: Instant,
        metrics: Arc<ShardMetrics>,
        stats_tx: Option<mailbox::Sender<Box<ShardStatsReport>>>,
        wall: WallAnchor,
        global_clock: crate::clock::reference::Snapshot,
        clock_rx: mailbox::Receiver<crate::clock::reference::Snapshot>,
    ) -> Self {
        let shard_count = frame_txs.len();
        let mut core = ShardCore::new(shard_id, max_gso_segments, shard_count, wall, update_rx);
        core.set_global_clock(global_clock);
        let router = LinkTransport {
            shard_id,
            frame_txs,
            clock_origin,
        };
        let recorder = ShardRecorder::shared();
        metrics::with_local_recorder(&*recorder, describe_shard_metrics);
        let health_metrics = ShardHealthMetrics::register(&recorder);

        Self {
            core,
            quic: None,
            recv_batch: Vec::with_capacity(net::BATCH_SIZE),
            tcp_inbox,
            loop_start: Instant::now(),
            command_rx,
            clock_rx,
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

    pub(crate) fn enable_quic(
        &mut self,
        ingress: pulsebeam_agent_quic::transport::ingress::Ingress,
        controller: mailbox::Sender<crate::control::controller::ControllerCommand>,
    ) {
        self.quic = Some(super::quic::QuicShard::new(ingress, controller));
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

    async fn wait_for_inputs(&mut self, io: &mut ManagedIo) -> Result<(), ShardError> {
        if !self.shard_event_backlog.is_empty() {
            return Ok(());
        }
        let mut deadline = self.next_wait_deadline();
        let quic_enabled = self.quic.is_some();
        let wait_for_write = self.quic.as_ref().is_some_and(|quic| {
            deadline = deadline.into_iter().chain(quic.deadline()).min();
            quic.wants_write()
        });

        // Block until at least one source is ready.
        pulsebeam_runtime::select! {
            biased;
            true = self.core.update_readable() => {}
            true = self.clock_rx.readable() => {}
            () = async {
                if let Some(quic) = &mut self.quic { quic.ready().await; }
            }, if quic_enabled => {}
            ready = io.udp_ready(deadline, wait_for_write) => { ready?; }
            true = self.frame_rx.readable() => {}
            true = async {
                match self.tcp_inbox.as_mut() {
                    Some(inbox) => inbox.receiver.readable().await,
                    None => false,
                }
            }, if self.tcp_inbox.is_some() => {}
            true = self.command_rx.readable() => {}
            else => return Err(ShardError::ManagerDisconnected),
        }

        Ok(())
    }

    fn tick(
        &mut self,
        at: pulsebeam_runtime::net::ReceiveTime,
        udp_socket: &mut pulsebeam_udp::UdpSocket,
    ) -> std::io::Result<()> {
        let now = self.core.observe_execution_time(at).monotonic;
        let local = udp_socket.local_addr();
        if let Ok(snapshot) = self.clock_rx.try_recv() {
            self.core.set_global_clock(snapshot);
        }
        // phase 1: input
        let mut commands: usize = 0;
        for _ in 0..SHARD_COMMAND_BUDGET {
            let Ok(cmd) = self.command_rx.try_recv() else {
                break;
            };
            commands = commands.saturating_add(1);
            match cmd {
                ShardCommand::MaterializeQuic {
                    association,
                    connection,
                    participant,
                    transport,
                    handoff,
                    grant,
                    scope,
                    ack,
                } => {
                    let installed = super::quic::Installed::new(
                        connection,
                        participant,
                        transport,
                        scope,
                        handoff,
                    );
                    let accepted = self.quic.as_mut().is_some_and(|quic| {
                        if !self
                            .core
                            .register_quic_owner(connection, transport, association, grant)
                        {
                            return false;
                        }
                        if quic.install(association, installed) {
                            true
                        } else {
                            self.core
                                .retire_quic_owner(connection, transport, association);
                            false
                        }
                    });
                    let _ = ack.send(accepted);
                }
                ShardCommand::PublicationCutoffResult {
                    connection_id,
                    reference,
                    success,
                } if self.quic.as_mut().is_some_and(|quic| {
                    quic.publication_cutoff_result(connection_id, reference, success)
                }) => {}
                ShardCommand::AuthorizationResult {
                    connection_id,
                    revision,
                    expiry,
                } if self.quic.as_mut().is_some_and(|quic| {
                    quic.authorization_result(connection_id, revision, expiry)
                }) =>
                {
                    self.core.update_quic_grant(connection_id, revision, expiry);
                }
                ShardCommand::NativeReady { connection_id }
                    if self
                        .quic
                        .as_mut()
                        .is_some_and(|quic| quic.native_ready(connection_id)) => {}
                cmd => {
                    let _ = self.core.on_command(cmd, &self.router);
                }
            }
        }
        if commands == SHARD_COMMAND_BUDGET {
            self.tick_budget_hit("commands");
        }
        let quic = &mut self.quic;
        if self.core.apply_updates_with_retirement(
            SHARD_UPDATE_OP_BUDGET,
            |connection, transport| {
                if let Some(quic) = quic {
                    quic.retire(connection, transport, now);
                }
            },
        ) >= SHARD_UPDATE_OP_BUDGET
        {
            self.tick_budget_hit("shard_update");
        }
        self.frame_batch.clear();
        let frames = self
            .frame_rx
            .try_recv_batch(&mut self.frame_batch, SHARD_FRAME_BUDGET);
        for mut received in self.frame_batch.drain(..) {
            let mut deliver_fabric = |ingress, image| {
                if let Some(address) = crate::link::delivery_address(received.tag)
                    .filter(|address| address.shard() == self.router.shard_id)
                    && let Some(arrival) = self.router.clock_origin.checked_add(
                        Duration::from_nanos(received.received_at.monotonic.as_nanos()),
                    )
                {
                    self.core
                        .on_network_fabric(address, ingress, now, arrival, image);
                }
            };
            match received.packet {
                ShardFrame::FabricWanImage { ingress, image } => {
                    deliver_fabric(ingress, image);
                    continue;
                }
                packet => received.packet = packet,
            }
            if !bind_received_frame(self.router.shard_id, &mut received) {
                continue;
            }
            match received.packet {
                ShardFrame::QuicIngress { datagram, .. } => {
                    if let Some(quic) = &mut self.quic {
                        #[cfg(feature = "sim")]
                        crate::sim_metrics::record_routing_counter("quic_wrong_ingress_received");
                        quic.receive(local, datagram.as_received(), now, &mut self.core)?;
                    }
                }
                frame => self
                    .core
                    .on_shard_frames(std::iter::once(frame), now, &self.router),
            }
        }
        if frames == SHARD_FRAME_BUDGET {
            self.tick_budget_hit("frames");
        }
        if self.recv_batch.is_empty() {
            let mode = if udp_socket.capabilities().max_batch_messages > 1 {
                net::UdpMode::Batch
            } else {
                net::UdpMode::Scalar
            };
            if let Ok(batch) = udp_socket.try_recv_batch() {
                use super::demux::{UdpDestination, classify_udp};
                let quic_shards = if self.quic.is_some() {
                    self.router.frame_txs.len()
                } else {
                    0
                };
                let mut native_budget = SHARD_UDP_BATCH_BUDGET;
                for datagram in batch.iter() {
                    match classify_udp(datagram.payload, quic_shards) {
                        Some(UdpDestination::Rtc) => {
                            append_owned_udp(&mut self.recv_batch, mode, datagram);
                        }
                        Some(UdpDestination::Quic(shard)) if native_budget != 0 => {
                            native_budget = native_budget.saturating_sub(1);
                            if shard != self.router.shard_id {
                                self.router.send_frame(
                                    shard,
                                    ShardFrame::QuicIngress {
                                        shard,
                                        datagram: NativeDatagram::new(datagram),
                                    },
                                );
                            } else if let Some(quic) = &mut self.quic {
                                quic.receive(local, datagram, now, &mut self.core)?;
                            }
                        }
                        _ => {}
                    }
                }
                if native_budget == 0 {
                    self.tick_budget_hit("quic_udp");
                }
            }
        }
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

        if self
            .core
            .poll_and_flush_dirty(udp_socket, SHARD_PARTICIPANT_BUDGET, &self.router)
            == SHARD_PARTICIPANT_BUDGET
        {
            self.tick_budget_hit("participants");
        }
        if self
            .core
            .flush_stream_buffers(Instant::now(), SHARD_PIPELINE_BUDGET)
            == SHARD_PIPELINE_BUDGET
        {
            self.tick_budget_hit("pipeline");
        }
        if self
            .core
            .poll_and_flush_dirty(udp_socket, SHARD_PARTICIPANT_BUDGET, &self.router)
            == SHARD_PARTICIPANT_BUDGET
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

        self.core.drain_pending_close().for_each(drop);

        // TCP is deliberately last. Charge every item at its configured maximum
        // so even hostile full-size frames fit the fixed 64 KiB per-tick budget.
        if let Some(inbox) = &mut self.tcp_inbox {
            let budget = usize::from(u16::MAX)
                .checked_div(inbox.max_frame_bytes)
                .unwrap_or_default()
                .min(8);
            for _ in 0..budget {
                let Ok(event) = inbox.receiver.try_recv() else {
                    break;
                };
                self.core.on_tcp_ingress(event);
            }
        }
        Ok(())
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

fn append_owned_udp(
    out: &mut Vec<RecvPacketBatch>,
    mode: net::UdpMode,
    datagram: pulsebeam_udp::ReceivedDatagram<'_>,
) {
    let len = datagram.payload.len();
    if mode == net::UdpMode::Batch
        && len > 0
        && let Some(previous) = out.last_mut()
        && previous.src == datagram.source
        && previous.dst == datagram.destination
        && previous.transport == net::Transport::Udp(mode)
        && previous.received_at == datagram.received_at
        && previous.offset == 0
        && previous.stride == len
        && previous.len.div_ceil(previous.stride) < net::UDP_MAX_GSO_SEGMENTS
        && previous.len.saturating_add(len) <= net::MAX_UDP_GSO_PAYLOAD_SIZE
    {
        previous.buf.extend_from_slice(datagram.payload);
        previous.len = previous.len.saturating_add(len);
        return;
    }
    out.push(RecvPacketBatch {
        src: datagram.source,
        dst: datagram.destination,
        buf: datagram.payload.to_vec(),
        stride: len.max(1),
        len,
        transport: net::Transport::Udp(mode),
        received_at: datagram.received_at,
        offset: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::recorder::MetricSeries;

    #[test]
    fn owned_udp_handoff_preserves_bounded_batches_and_arrival_identity() {
        let mut out = Vec::new();
        let payload = [7; 800];
        let datagram = pulsebeam_udp::ReceivedDatagram {
            source: "127.0.0.1:1000".parse().unwrap(),
            destination: "127.0.0.1:2000".parse().unwrap(),
            payload: &payload,
            received_at: net::ReceiveTime::now(),
            ecn: None,
        };
        for _ in 0..=net::UDP_MAX_GSO_SEGMENTS {
            append_owned_udp(&mut out, net::UdpMode::Batch, datagram);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len, payload.len() * net::UDP_MAX_GSO_SEGMENTS);
        assert_eq!(out[1].len, payload.len());
        assert_eq!(out[0].received_at, datagram.received_at);
        for _ in 0..net::UDP_MAX_GSO_SEGMENTS {
            assert_eq!(out[0].next_packet().unwrap(), payload);
        }
        assert!(out[0].next_packet().is_none());
        let later = pulsebeam_udp::ReceivedDatagram {
            received_at: net::ReceiveTime {
                monotonic: datagram.received_at.monotonic + std::time::Duration::from_nanos(1),
                ..datagram.received_at
            },
            ..datagram
        };
        append_owned_udp(&mut out, net::UdpMode::Batch, later);
        assert_eq!(out.len(), 3);
        append_owned_udp(
            &mut out,
            net::UdpMode::Batch,
            pulsebeam_udp::ReceivedDatagram {
                destination: "127.0.0.1:3000".parse().unwrap(),
                ..later
            },
        );
        assert_eq!(out.len(), 4);
        append_owned_udp(&mut out, net::UdpMode::Scalar, later);
        append_owned_udp(&mut out, net::UdpMode::Scalar, later);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn shared_udp_routes_same_peer_by_header_and_bounds_wrong_shard_handoff() {
        use super::super::demux::{UdpDestination, classify_udp};
        use pulsebeam_agent_quic::transport::routing::{ROUTED_CID_BYTES, mint};
        use pulsebeam_link::{Destination, LinkControl, LinkTime};

        let ingress = ShardId::new(0);
        let owner = ShardId::new(1);
        let now = Instant::now();
        let mut links = LinkControl::<ShardFrame>::new(|| LinkTime::from_nanos(0)).unwrap();
        let mut receivers = Vec::new();
        let mut frame_txs = Vec::new();
        for shard in [ingress, owner] {
            let (port, rx) = links.create_port().unwrap();
            receivers.push(rx);
            let (_, tx) = links
                .bind_send(Destination::Local {
                    port,
                    tag: shard_tag(shard),
                })
                .unwrap();
            frame_txs.push(tx);
        }
        let router = LinkTransport {
            shard_id: ingress,
            frame_txs,
            clock_origin: now,
        };

        let mut rtp = [0; 12];
        *rtp.first_mut().unwrap() = 0x80;
        let peer = pulsebeam_udp::ReceivedDatagram {
            source: "192.0.2.10:1000".parse().unwrap(),
            destination: "192.0.2.20:2000".parse().unwrap(),
            payload: &rtp,
            received_at: net::ReceiveTime::now(),
            ecn: Some(pulsebeam_udp::Ecn::Ect1),
        };
        assert_eq!(classify_udp(peer.payload, 2), Some(UdpDestination::Rtc));
        for short in [
            &[][..],
            &[0][..],
            &[22][..],
            &[0x80][..],
            &[0x40][..],
            &[0xc0][..],
        ] {
            assert_eq!(classify_udp(short, 2), None);
        }
        let mut short_header = Vec::with_capacity(1 + ROUTED_CID_BYTES + 20);
        short_header.push(0x40);
        short_header.extend(mint(1));
        short_header.extend([0; 20]);
        let rebound = pulsebeam_udp::ReceivedDatagram {
            source: "192.0.2.10:1001".parse().unwrap(),
            payload: &short_header,
            ..peer
        };
        for item in [
            pulsebeam_udp::ReceivedDatagram {
                payload: &short_header,
                ..peer
            },
            rebound,
        ] {
            assert_eq!(
                classify_udp(item.payload, 2),
                Some(UdpDestination::Quic(owner))
            );
            assert_eq!(classify_udp(item.payload, 1), None);
        }
        let frame = ShardFrame::QuicIngress {
            shard: owner,
            datagram: NativeDatagram::new(rebound),
        };
        assert!(!router.enqueue(ingress, frame.clone()));
        assert!(router.enqueue(owner, frame.clone()));
        assert!((0..2048).any(|_| !router.enqueue(owner, frame.clone())));

        let mut received = Vec::new();
        assert_eq!(
            receivers
                .last_mut()
                .unwrap()
                .try_recv_batch(&mut received, 1),
            1
        );
        let mut received = received.pop().unwrap();
        assert!(!bind_received_frame(ingress, &mut received));
        assert!(bind_received_frame(owner, &mut received));
        let ShardFrame::QuicIngress { datagram, .. } = received.packet else {
            panic!("expected native ingress")
        };
        let forwarded = datagram.as_received();
        assert_eq!(forwarded.payload, rebound.payload);
        assert_eq!(forwarded.source, rebound.source);
        assert_eq!(forwarded.destination, rebound.destination);
        assert_eq!(forwarded.ecn, rebound.ecn);
        assert_eq!(forwarded.received_at, rebound.received_at);
    }

    fn find(keys: &[MetricSeries], name: &str) -> usize {
        for (idx, key) in keys.iter().enumerate() {
            if key.name == name {
                return idx;
            }
        }
        panic!("missing metric {name}");
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
