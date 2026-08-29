//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, cloned at
//! startup and never again. Carries the sanctioned exception in
//! `shard::metrics`; nothing else here may share.

use std::{collections::VecDeque, pin::Pin};
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
    entity::{ParticipantId, TrackId},
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
    metrics::describe_histogram!(
        "forwarding_service_us",
        metrics::Unit::Microseconds,
        "ingress to pacer admission for forwarded media"
    );
    metrics::describe_histogram!(
        "forwarding_pacing_us",
        metrics::Unit::Microseconds,
        "required pacing between admission and earliest eligible departure"
    );
    metrics::describe_histogram!(
        "forwarding_egress_lateness_us",
        metrics::Unit::Microseconds,
        "eligible departure to authoritative socket departure for forwarded media"
    );
    metrics::describe_histogram!(
        "forwarding_total_us",
        metrics::Unit::Microseconds,
        "ingress to authoritative socket departure for forwarded media"
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
    MaterializeParticipant {
        key: crate::shard::participants::ParticipantKey,
        transport: crate::route::TransportHandle,
        config: Box<ParticipantConfig>,
        ack: tokio::sync::oneshot::Sender<bool>,
    },
    AdoptTcpConnection {
        stream: pulsebeam_runtime::net::tcp::BufferedTcpStream,
        peer_addr: std::net::SocketAddr,
    },
    AuthenticateTransport {
        source: std::net::SocketAddr,
        handle: crate::route::TransportHandle,
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
        handle: crate::route::TransportHandle,
        source_shard: ShardId,
    },
    /// Forward payload, addressed by the destination's own route. Carries no
    /// semantic ids: everything needed to deliver it lives in the destination's
    /// compiled route entry.
    Media {
        env: Envelope,
        payload: MediaPayload,
    },
    Reverse {
        env: Envelope,
        packet: crate::participant::reverse::ReversePacket,
    },
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
        handle: crate::route::TransportHandle,
        shard: ShardId,
    },
    ParticipantClosed {
        participant: ParticipantId,
    },
    TrackSubscribed {
        subscriber: ParticipantId,
        track: crate::track::TrackMeta,
    },
    TrackUnsubscribed {
        subscriber: ParticipantId,
        track: crate::track::TrackMeta,
    },
    TrackSubscriptionAdded {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        selector: crate::track::TrackSelector,
        selection: crate::track::SelectionPolicy,
    },
    TrackSubscriptionRemoved {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        selector: crate::track::TrackSelector,
    },
    TrackPublished {
        track: Track,
    },
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
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

pub(crate) struct ShardWorker {
    core: ShardCore,
    recv_batch: Vec<RecvPacketBatch>,
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

            let busy_start = Instant::now();
            self.metrics
                .record_idle(busy_start.saturating_duration_since(loop_start));

            let previous_busy = self.last_busy;
            metrics::with_local_recorder(recorder, || {
                self.observe_health(previous_busy);
                self.tick(busy_start);
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

    fn tick(&mut self, now: Instant) {
        // phase 1: input
        let mut commands: usize = 0;
        for _ in 0..SHARD_COMMAND_BUDGET {
            let Ok(cmd) = self.command_rx.try_recv() else {
                break;
            };
            commands = commands.saturating_add(1);
            match cmd {
                ShardCommand::AdoptTcpConnection { stream, peer_addr } => {
                    if let Err(err) = self.tcp_socket.add_connection(stream, peer_addr) {
                        tracing::warn!(%peer_addr, error = ?err, "Failed to add new TCP connection to shard");
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
        self.core.fire_timers(now);

        let _ = self.udp_socket.try_recv_batch(&mut self.recv_batch);
        let _ = self.tcp_socket.try_recv_batch(&mut self.recv_batch);
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

        if self.core.poll_and_flush_dirty(
            now,
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
            now,
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

        self.core
            .flush_close_peers(&mut self.udp_socket, &mut self.tcp_socket);
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
    use crate::shard::recorder::MetricKey;

    fn find(keys: &[MetricKey], name: &str) -> usize {
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
