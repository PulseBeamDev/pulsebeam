//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, cloned at
//! startup and never again. Carries the sanctioned exception in
//! `shard::metrics`; nothing else here may share.

#[allow(
    clippy::disallowed_types,
    reason = "Arc<ShardMetrics>, one per shard, see module note"
)]
use std::{marker::PhantomData, pin::Pin, rc::Rc, sync::Arc, time::Duration};

use crate::clock::WallAnchor;
use crate::route::Envelope;

use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
    rand::Rng,
};
use str0m::media::KeyframeRequestKind;
use tokio::time::{Instant, Sleep};

use crate::{
    entity::{ParticipantId, TrackId},
    id::ShardId,
    participant::ParticipantConfig,
    rtp::RtpPacket,
    shard::metrics::ShardMetrics,
    shard::recorder::{ShardRecorder, ShardStatsReport},
    track::Track,
};

use super::core::{ShardCore, ShardTransport};

/// Depth of the shard -> controller topology queue.
///
/// Deliberately large and preallocated. Every entry is one topology change
/// (publish, subscribe, teardown, participant lifecycle) — control-rate events,
/// not per-packet — so a healthy node never comes close. It is sized this way
/// so that filling it is unambiguous evidence of a stalled controller rather
/// than a burst, which is what lets [`ShardWorker::flush_shard_events`] treat
/// it as fatal instead of blocking.
pub(crate) const SHARD_EVENT_CAPACITY: usize = 65_536;

/// Depth of the controller -> shard command queue.
///
/// The shard *can* block on this one, so it does not need the headroom its
/// reverse does; sizing them alike would make an ordinary burst look like the
/// stalled controller [`SHARD_EVENT_CAPACITY`]'s overflow exists to diagnose.
///
/// Both directions must read this from here. Restating the depth at the call
/// site would let the two drift apart, and the ratio below would then be
/// asserted about a number nothing uses.
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

/// How far past its due time a report may be deferred waiting for a cheap
/// tick. A shard saturated for this long reports anyway: stale metrics from a
/// struggling shard are exactly the ones worth having.
const STATS_DEADLINE_SLACK: Duration = STATS_REPORT_INTERVAL;

const _: () = assert!(
    SHARD_EVENT_CAPACITY >= SHARD_COMMAND_CAPACITY * 16,
    "the queue a shard cannot block on must have far more headroom than the one it can"
);

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
        config: Box<ParticipantConfig>,
    },
    AdoptTcpConnection {
        stream: pulsebeam_runtime::net::tcp::BufferedTcpStream,
        peer_addr: std::net::SocketAddr,
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

/// Payload carried under an [`Envelope`]. Still typed this pass; byte
/// serialization arrives with the UDP transport.
pub(crate) enum MediaPayload {
    Video(RtpPacket),
    Audio(RtpPacket),
    /// SCTP bytes for a client data channel. Which lane the client asked for is
    /// in the destination's route entry, not here — the destination already
    /// knows it, and it describes the client's channel, not this hop.
    Data(Vec<u8>),
}

/// Everything one shard sends another: the data plane.
///
/// Best-effort by construction. Cross-node this becomes a UDP datagram, so
/// nothing that must not be dropped is representable here — topology travels
/// the control plane ([`ShardCommand`] / [`ShardEvent`]) and never this way.
#[allow(
    clippy::large_enum_variant,
    reason = "boxing MediaPayload would put a heap allocation on every forwarded media frame; the mesh moves these by value precisely to avoid that, and the ratio only became visible once the client-packet variant was deleted"
)]
pub(crate) enum ShardFrame {
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
    /// Forward telemetry: what a publisher's encodings currently measure,
    /// addressed by the destination's own route.
    ///
    /// A value, not a handle. Measurements used to be an `Arc` of atomics the
    /// subscriber's shard read directly, which shared a refcount across cores
    /// and — worse — never gave a coherent view, since eight independent atomic
    /// reads can straddle a writer. One message is one consistent snapshot, and
    /// it is the only shape that works when the destination is another node.
    ///
    /// Latest-wins: losing one costs a slightly stale allocation and nothing
    /// else, so it belongs on the best-effort lane.
    Telemetry {
        env: Envelope,
        stats: crate::track::TrackStates,
    },
}

pub(crate) type ShardEventMessage = (ShardId, ShardEvent);

/// Runtime facts observed by a shard. The controller converts these facts into
/// canonical state and publishes the resulting execution image.
#[derive(Debug)]
pub(crate) enum ShardEvent {
    TrackObserved {
        track: Box<Track>,
        states: crate::track::TrackStates,
    },
    TrackClosed {
        origin: ParticipantId,
        track_id: TrackId,
    },
    ParticipantClosed {
        participant: ParticipantId,
    },
    DataChannelObserved {
        intent: crate::shard::events::ClientIntent,
    },
    SubscriptionIntent {
        intent: crate::shard::events::ClientIntent,
    },
    TrackStatsObserved {
        track_id: TrackId,
        states: crate::track::TrackStates,
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
    frame_rx: mailbox::Receiver<ShardFrame>,
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
        view_rx: mailbox::Receiver<Box<crate::view::ShardViewDelta>>,
        event_tx: mailbox::Sender<ShardEventMessage>,
        frame_rx: mailbox::Receiver<ShardFrame>,
        frame_txs: Vec<mailbox::Sender<ShardFrame>>,
        metrics: Arc<ShardMetrics>,
        stats_tx: Option<mailbox::Sender<Box<ShardStatsReport>>>,
        rng: Rng,
        wall: WallAnchor,
    ) -> Self {
        let core = ShardCore::new(shard_id, udp_socket.max_gso_segments(), rng, wall, view_rx);
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
            frame_rx,
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

    async fn wait_for_inputs(&mut self, mut sleep: Pin<&mut Sleep>) -> Result<(), ShardError> {
        // A quiet shard must still wake to report, or its metrics would freeze
        // at whatever it last had traffic for.
        let stats_deadline = self.stats_tx.as_ref().map(|_| self.stats_due);
        let deadline = match (self.core.next_timer_deadline(), stats_deadline) {
            (Some(timer), Some(stats)) => Some(timer.min(stats)),
            (timer, stats) => timer.or(stats),
        };
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
        self.core.apply_view_deltas();
        // phase 1: input
        while let Ok(cmd) = self.command_rx.try_recv() {
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
        while let Ok(ev) = self.frame_rx.try_recv() {
            self.core.on_shard_frame(ev, now, &self.router);
        }
        self.core.fire_timers(now);

        let _ = self.udp_socket.try_recv_batch(&mut self.recv_batch);
        let _ = self.tcp_socket.try_recv_batch(&mut self.recv_batch);
        for batch in self.recv_batch.drain(..) {
            self.core.on_udp_batch(batch);
        }

        self.core
            .poll_and_flush_dirty(now, &mut self.udp_socket, &mut self.tcp_socket);
        self.core.flush_stream_buffers(&self.router);
        self.core
            .poll_and_flush_dirty(now, &mut self.udp_socket, &mut self.tcp_socket);
        self.core.flush_participant_events(&self.router);

        self.core
            .flush_close_peers(&mut self.udp_socket, &mut self.tcp_socket);
    }

    /// Hand this tick's topology events to the controller.
    ///
    /// **Never await here.** The controller awaits when it sends a shard a
    /// command, so a shard that awaits sending an event closes a cycle: with
    /// both channels full, the controller blocks on a shard that is blocked on
    /// the controller and neither ever drains. The shard side is the one that
    /// must not block, because a shard that never blocks is what makes the
    /// controller's await safe — its commands are always drained.
    ///
    /// So this is `try_send`, and a full queue is fatal rather than handled:
    /// [`SHARD_EVENT_CAPACITY`] is sized far above any rate real topology churn
    /// can produce, so reaching it means the controller has stopped consuming
    /// and the cluster's view of the topology is already wrong. Continuing from
    /// there would silently drop subscriptions and teardowns.
    fn flush_shard_events(&mut self) -> Result<(), ShardError> {
        while let Some(event) = self.core.pop_shard_event() {
            match self.event_tx.try_send((self.router.shard_id, event)) {
                Ok(()) => {}
                Err(mailbox::TrySendError::Closed(_)) => {
                    tracing::warn!("shard event channel is closed, exiting");
                    return Err(ShardError::ManagerDisconnected);
                }
                Err(mailbox::TrySendError::Full(_ev)) => {
                    metrics::counter!("shard_event_shed").increment(1);
                    tracing::error!(
                        shard = %self.router.shard_id,
                        "shard event queue is full; shedding a recoverable control event"
                    );
                }
            }
        }

        Ok(())
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
