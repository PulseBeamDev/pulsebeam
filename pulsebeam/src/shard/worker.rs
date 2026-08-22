//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, cloned at
//! startup and never again. Carries the sanctioned exception in
//! `shard::metrics`; nothing else here may share.

use std::{collections::VecDeque, pin::Pin};
#[allow(
    clippy::disallowed_types,
    reason = "Arc<ShardMetrics>, one per shard, see module note"
)]
use std::{marker::PhantomData, rc::Rc, sync::Arc, time::Duration};

use crate::clock::WallAnchor;
use crate::route::Envelope;

use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
};
use str0m::media::KeyframeRequestKind;
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

pub(crate) const SHARD_VIEW_CAPACITY: usize = 1024;

pub(crate) const SHARD_COMMAND_BUDGET: usize = 64;
pub(crate) const SHARD_FRAME_BUDGET: usize = 256;
pub(crate) const SHARD_VIEW_OP_BUDGET: usize = 256;
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
/// Topology and lifecycle changes are published through `ShardView`.
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

/// What a subscriber sends back to a publisher.
///
/// Kept compact because it is going on a wire: the reverse route already
/// identifies the stream, so a body names only what the destination cannot
/// derive. Encodings are named by their index in the track's declared order —
/// both ends have that from the control plane — so a rid never travels.
///
/// ```text
/// Envelope(16) | tag(1) | body
///   Keyframe  layer(1) kind(1)        -> 19 bytes
///   Nack      layer(1) pid(2) blp(2)  -> 22 bytes
///   DataAck   len(2) payload(len)     -> 19 + len
/// ```
///
/// The header is the same 16 bytes every other payload family uses. A shorter
/// reverse-only header would save eight bytes per request and cost a second
/// route offset on the wire, which is the one thing the steering program
/// cannot afford to have two of.
#[derive(Debug, Clone)]
pub(crate) enum Reverse {
    /// Ask for a keyframe on one encoding.
    Keyframe {
        layer: u8,
        kind: KeyframeRequestKind,
    },
    /// RTP loss report in the RTCP generic-NACK shape: `pid` is the first lost
    /// sequence number and `blp` a bitmask of the 16 that follow. Not raised
    /// yet — this is the slot it goes in.
    #[allow(dead_code)]
    Nack { layer: u8, pid: u16, blp: u16 },
    /// The application's own reliability protocol for a reliable data topic:
    /// the subscriber telling the publisher what it is missing. Opaque here —
    /// the SFU relays it without interpreting it, which is what keeps
    /// end-to-end reliability an endpoint concern rather than a hop guarantee.
    DataAck(Vec<u8>),
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
    /// Anything travelling back toward a publisher, addressed by the reverse
    /// route its shard opened. One variant for all of it because they share a
    /// contract: every one is an idempotent request the sender repeats if it
    /// still needs it, so losing one costs a round trip and nothing else.
    Reverse { env: Envelope, body: Reverse },
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
        key: crate::shard::participants::ParticipantKey,
    },
    TrackSubscribed {
        subscriber: ParticipantId,
        subscriber_key: crate::shard::participants::ParticipantKey,
        slot: crate::keys::DownstreamSlotKey,
        track: crate::track::TrackMeta,
    },
    TrackUnsubscribed {
        subscriber: ParticipantId,
        slot: crate::keys::DownstreamSlotKey,
        track: crate::track::TrackMeta,
    },
    TrackPublished {
        track: Box<Track>,
    },
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
    },
    DataTopicPublished {
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        publisher_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
    },
    DataTopicUnpublished {
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        publisher_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
    },
    DataTopicSubscribed {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
        channel: str0m::channel::ChannelId,
    },
    DataTopicUnsubscribed {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
    },
    ReliableDataTopicPublished {
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        publisher_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
    },
    ReliableDataTopicUnpublished {
        room_id: crate::entity::RoomId,
        publisher: ParticipantId,
        publisher_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
    },
    ReliableDataTopicSubscribed {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
        channel: str0m::channel::ChannelId,
    },
    ReliableDataTopicUnsubscribed {
        room_id: crate::entity::RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
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
    /// This shard's own `metrics` recorder. `Rc` because it never leaves the
    /// core; the handles it hands out are the only things that must be `Sync`,
    /// and only because the `metrics` crate's signatures say so.
    recorder: Rc<ShardRecorder>,
    stats_tx: Option<mailbox::Sender<Box<ShardStatsReport>>>,
    stats_due: Instant,
    last_busy: Duration,

    // Mark !Send
    _marker: PhantomData<*mut ()>,
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
        view_rx: mailbox::Receiver<Box<crate::view::ControlBatch>>,
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
            view_rx,
        );
        let router = ChannelTransport {
            shard_id,
            frame_txs,
        };

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
            recorder: {
                let recorder = Rc::new(ShardRecorder::new());
                metrics::with_local_recorder(&*recorder, describe_shard_metrics);
                recorder
            },
            stats_tx,
            stats_due: Instant::now()
                .checked_add(STATS_REPORT_INTERVAL)
                .unwrap_or_else(Instant::now),
            last_busy: Duration::ZERO,
            _marker: PhantomData,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = %self.router.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        let sleep = tokio::time::sleep(tokio::time::Duration::MAX);
        tokio::pin!(sleep);

        let mut loop_start = Instant::now();
        loop {
            self.wait_for_inputs(sleep.as_mut()).await?;

            let busy_start = Instant::now();
            self.metrics
                .record_idle(busy_start.saturating_duration_since(loop_start));

            // Every `metrics::*` call reached from here resolves against this
            // shard's own recorder rather than the process-global one, so an
            // increment touches memory no other core does. Installed per tick
            // rather than per thread because under `SharedRuntime` every shard
            // of a node shares one thread, and attribution must come from the
            // installed recorder rather than from thread identity.
            let recorder = Rc::clone(&self.recorder);
            let previous_busy = self.last_busy;
            metrics::with_local_recorder(&*recorder, || {
                self.observe_health(previous_busy);
                self.tick(busy_start);
                self.flush_shard_events()
            })?;

            // TODO: record forwarding latency
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
        metrics::gauge!("participants_live").set(self.core.participant_count() as f64);
        let (busy_us, idle_us) = self.metrics.read_raw();
        metrics::counter!("busy_us").absolute(busy_us);
        metrics::counter!("idle_us").absolute(idle_us);
        metrics::histogram!("tick_us")
            .record(u64::try_from(previous_busy.as_micros()).unwrap_or(u64::MAX) as f64);
        if previous_busy >= LONG_TICK {
            metrics::counter!("shard_long_tick").increment(1);
        }
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
            Some(_) = self.core.view_readable() => {}
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
        if self.core.apply_view_deltas(SHARD_VIEW_OP_BUDGET) >= SHARD_VIEW_OP_BUDGET {
            self.tick_budget_hit("view");
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
mod reverse_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    /// The reverse lane is going on a wire, so the documented layout is the
    /// contract: a route already names the stream, so a body may only carry
    /// what the destination cannot derive. This pins the sizes the doc comment
    /// on [`Reverse`] promises, so a field added without thinking shows up here
    /// rather than in a datagram.
    #[test]
    fn reverse_bodies_stay_compact() {
        const HEADER: usize = crate::route::ENVELOPE_LEN + 1; // envelope + tag

        fn wire_len(body: &Reverse) -> usize {
            HEADER
                + match body {
                    Reverse::Keyframe { .. } => 2,              // layer, kind
                    Reverse::Nack { .. } => 5,                  // layer, pid, blp
                    Reverse::DataAck(bytes) => 2 + bytes.len(), // len prefix
                }
        }

        assert_eq!(
            wire_len(&Reverse::Keyframe {
                layer: 0,
                kind: KeyframeRequestKind::Pli,
            }),
            19,
            "a keyframe request must fit the documented 19 bytes"
        );
        assert_eq!(
            wire_len(&Reverse::Nack {
                layer: 0,
                pid: 1,
                blp: 0,
            }),
            22,
            "a NACK must fit the documented 22 bytes"
        );
        assert_eq!(wire_len(&Reverse::DataAck(vec![0u8; 8])), 27);
    }

    #[test]
    fn media_payload_stays_compact() {
        const _: () = assert!(
            std::mem::size_of::<MediaPayload>() <= std::mem::size_of::<RoutedTrackPacket>() + 16
        );
        let payload = MediaPayload {
            key: crate::keys::TrackKey::default(),
            packet: crate::participant::TrackPacket::Data(b"payload".to_vec()),
        };
        let crate::participant::TrackPacket::Data(bytes) = payload.packet else {
            unreachable!();
        };
        assert_eq!(bytes, b"payload");
    }

    /// An encoding is named by index, never by rid: the index is derivable from
    /// the track descriptor both ends already hold, and a rid is a variable
    /// length string that has no business on a per-request lane.
    #[test]
    fn an_encoding_is_named_by_index_not_by_rid() {
        let body = Reverse::Keyframe {
            layer: 2,
            kind: KeyframeRequestKind::Pli,
        };
        assert_eq!(
            std::mem::size_of_val(&match body {
                Reverse::Keyframe { layer, .. } => layer,
                _ => unreachable!(),
            }),
            1,
            "an encoding selector must be one byte"
        );
    }
}

#[cfg(test)]
mod architecture_tests {
    use super::*;

    fn event_variant(event: &ShardEvent) -> u8 {
        match event {
            ShardEvent::TransportAuthenticated { .. } => 13,
            ShardEvent::ParticipantClosed { .. } => 0,
            ShardEvent::TrackSubscribed { .. } => 1,
            ShardEvent::TrackUnsubscribed { .. } => 2,
            ShardEvent::TrackPublished { .. } => 3,
            ShardEvent::TrackUnpublished { .. } => 4,
            ShardEvent::DataTopicPublished { .. } => 5,
            ShardEvent::DataTopicUnpublished { .. } => 6,
            ShardEvent::DataTopicSubscribed { .. } => 7,
            ShardEvent::DataTopicUnsubscribed { .. } => 8,
            ShardEvent::ReliableDataTopicPublished { .. } => 9,
            ShardEvent::ReliableDataTopicUnpublished { .. } => 10,
            ShardEvent::ReliableDataTopicSubscribed { .. } => 11,
            ShardEvent::ReliableDataTopicUnsubscribed { .. } => 12,
        }
    }

    #[test]
    fn event_surface_has_an_exhaustive_guard() {
        let event = ShardEvent::ParticipantClosed {
            participant: ParticipantId::from_bytes([0; 16]),
            key: crate::shard::participants::ParticipantKey::default(),
        };
        assert_eq!(event_variant(&event), 0);
    }
}
