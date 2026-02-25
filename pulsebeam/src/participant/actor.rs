use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use crate::gateway::GatewayWorkerHandle;
use crate::participant::batcher::Batcher;
use crate::participant::core::{CoreEvent, ParticipantCore, SLOW_POLL_INTERVAL};
use crate::{entity, gateway, room, track};
use futures_util::FutureExt;
use pulsebeam_runtime::actor;
use pulsebeam_runtime::actor::ActorKind;
use pulsebeam_runtime::mailbox::TrySendError;
use pulsebeam_runtime::net::UnifiedSocketWriter;
use pulsebeam_runtime::prelude::*;
use str0m::{Rtc, RtcError, error::SdpError};
use tokio::time::Instant;
use tokio_metrics::TaskMonitor;

pub use crate::participant::core::TrackMapping;

const MIN_QUANTA: Duration = Duration::from_millis(1);
/// Maximum number of items drained synchronously per select! arm before yielding
/// back to the Tokio scheduler. Caps task hold-time under burst load and prevents
/// one participant from starving others on the same worker thread.
///
/// Budget rationale: each forwarded packet costs ~3µs (write_rtp + to_vec alloc +
/// poll_rtc transmit output). At 8 packets + one poll() call (~15µs overhead) the
/// worst-case synchronous slice is ~39µs, keeping P99 task poll time under 50µs.
const MAX_DRAIN_PER_WAKE: usize = 8;

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
}

/// Control messages for the participant actor.
///
/// Track distribution is now handled via a `watch::Receiver` injected at
/// construction time — no per-event fan-out messages are needed.
/// This enum is intentionally empty; reserved for future out-of-band
/// control messages without requiring a protocol change.
#[derive(Debug)]
pub enum ParticipantControlMessage {}

pub struct ParticipantMessageSet;

impl actor::MessageSet for ParticipantMessageSet {
    type Msg = ParticipantControlMessage;
    type Meta = entity::ParticipantId;
    type ObservableState = ();
}

// Fields are ordered hot→cold so that the Tokio scheduler's LIFO-slot behaviour
// (task re-queued on the same worker thread) keeps the most-frequently accessed
// words in L1/L2.  Each hot field is touched on every select! arm iteration;
// cold fields are touched at most twice per participant lifetime.
pub struct ParticipantActor {
    // ── HOT (every select! iteration) ───────────────────────────────────────
    // Boxed to keep ParticipantCore off the async state machine stack. The
    // actor::run() future stores `a: ParticipantActor` inline while also holding
    // `a.run()` as __awaitee, so every unboxed field adds directly to the task's
    // memory footprint.  The box pointer (8 B) fits in the first cache line;
    // ParticipantCore itself is #[repr(align(64))] so its hot fields start on a
    // fresh cache line.
    core: Box<ParticipantCore>,
    /// UDP egress socket — flushed after every downstream drain arm.
    udp_egress: UnifiedSocketWriter,
    /// TCP egress socket — flushed after every downstream drain arm.
    tcp_egress: UnifiedSocketWriter,
    // ── WARM (polled every iteration but rarely ready) ────────────────────
    /// Watch receiver for the room-wide track map.  Polled in every select! via
    /// `tracks_rx.changed()` but fires at most N times per room lifetime.
    tracks_rx: room::TrackWatchReceiver,
    // ── COOL (~2× per participant lifetime) ─────────────────────────────────
    /// Last-seen track snapshot; used to diff additions vs. removals.
    prev_tracks: Arc<room::TrackMap>,
    /// Room mailbox — only written to when SpawnTrack events are ready (~2×).
    room_handle: room::RoomHandle,
    // ── COLD (connect / disconnect only) ────────────────────────────────────
    gateway: GatewayWorkerHandle,
}

impl actor::Actor<ParticipantMessageSet> for ParticipantActor {
    fn monitor() -> Arc<TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> ActorKind {
        "participant"
    }

    fn meta(&self) -> entity::ParticipantId {
        self.core.participant_id
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ParticipantMessageSet>,
    ) -> Result<(), actor::ActorError> {
        let ufrag = self.core.rtc.direct_api().local_ice_credentials().ufrag;
        // 64 slots × ~128 B/slot × 300 participants = 2.4 MB — fits in L3.
        // 30 fps video with a 300 ms jitter buffer reaches at most ~54 in-flight
        // packets, so 64 slots is sufficient headroom.
        let (gateway_tx, mut gateway_rx) = pulsebeam_runtime::sync::mpsc::channel(64);

        let _ = self
            .gateway
            .send(gateway::GatewayControlMessage::AddParticipant(
                ufrag.clone(),
                gateway_tx,
            ))
            .await;

        // Sync the initial track snapshot before entering the loop so that any
        // tracks published before this participant joined are immediately visible.
        {
            let initial = self.tracks_rx.borrow_and_update();
            self.core.handle_available_tracks(&**initial);
            self.prev_tracks = Arc::clone(&*initial);
        }

        let mut maybe_deadline = self.core.poll();
        // Box Sleep futures so their ~152-byte state lives on the heap rather than
        // inline in the select! state machine (which Tokio measures for the stack
        // warning).  Both are declared here so they share the same outer scope and
        // are not re-allocated on each loop iteration.
        let mut sleep = Box::pin(tokio::time::sleep(MIN_QUANTA));
        // Dedicated slow-poll timer.  Isolating slow-poll into its own select! arm
        // means it gets its OWN Tokio iteration: no packet-processing arm ever has
        // slow-poll cost added to its poll time.  This turns the P99 poll spike
        // from "slow_poll_cost + packet_cost" into just "slow_poll_cost" on a
        // dedicated iteration, and leaves all hot-path iterations unaffected.
        //
        // THUNDERING-HERD MITIGATION: seed the first firing with a random jitter
        // in [0, SLOW_POLL_INTERVAL) derived from the participant UUID.  Without
        // this, all N participants initialized in the same burst share the same
        // tick phase and fire simultaneously every 200 ms, saturating the run
        // queue and spiking scheduling P99.  After the first firing the timer
        // resets to Instant::now() + SLOW_POLL_INTERVAL and the phases stay
        // permanently spread.
        let initial_slow_poll_jitter = {
            use pulsebeam_runtime::rand::RngCore;
            let raw = pulsebeam_runtime::rand::rng().next_u64();
            let jitter_ms = raw % SLOW_POLL_INTERVAL.as_millis() as u64;
            Duration::from_millis(jitter_ms.max(1))
        };
        let mut slow_poll_sleep =
            Box::pin(tokio::time::sleep(initial_slow_poll_jitter));

        'actor: while let Some(deadline) = maybe_deadline {
            let now = Instant::now();

            // If the deadline is 'now' or in the past, we must not busy-wait.
            // We enforce a minimum 1ms "quanta" to prevent CPU starvation.
            let adjusted_deadline = if deadline <= now {
                now + MIN_QUANTA
            } else {
                deadline
            };

            if sleep.deadline() != adjusted_deadline {
                sleep.as_mut().reset(adjusted_deadline);
            }

            tokio::select! {
                biased;

                // --- Priority 0: System (terminate / get-state) ---
                res = ctx.sys_rx.recv() => {
                    match res {
                        Some(msg) => match msg {
                            actor::SystemMsg::GetState(responder) => {
                                let _: () = self.get_observable_state();
                                responder.send(());
                            }
                            actor::SystemMsg::Terminate => break 'actor,
                        },
                        None => break 'actor,
                    }
                }

                // --- Priority 1: Track map changes (cold path, ~2× per peer per room lifetime) ---
                //
                // The room actor writes the complete track snapshot once per track event.
                // We diff against our last known snapshot to determine additions and
                // removals without touching the downstream allocator's internals.
                Ok(()) = self.tracks_rx.changed() => {
                    // Acquire new snapshot; drop the borrow guard before calling &mut self methods.
                    let new_tracks: Arc<room::TrackMap> = {
                        let borrow = self.tracks_rx.borrow_and_update();
                        Arc::clone(&*borrow)
                    };

                    // Detect removals by diffing against the previous snapshot.
                    let removed: HashMap<entity::TrackId, track::TrackReceiver> = self
                        .prev_tracks
                        .iter()
                        .filter(|(id, _)| !new_tracks.contains_key(id))
                        .map(|(id, rx)| (*id, rx.clone()))
                        .collect();

                    if !removed.is_empty() {
                        self.core.remove_available_tracks(&removed);
                    }
                    // handle_available_tracks is idempotent for already-known tracks.
                    self.core.handle_available_tracks(&new_tracks);
                    self.prev_tracks = new_tracks;
                    maybe_deadline = self.core.poll();
                }

                // --- Priority 2: Keyframe requests (upstream → downstream, ~2 Hz) ---
                _ = self.core.upstream.notified() => {
                    let now = Instant::now();
                    // Collect first to release the borrow on upstream before handle_keyframe_request
                    // borrows all of core.  Allocations here are rare (~2 Hz) and negligible.
                    let reqs: Vec<_> = self.core.upstream.drain_keyframe_requests(now).collect();
                    for req in reqs {
                        self.core.handle_keyframe_request(req);
                    }
                }

                // --- Priority 3: Slow poll (BWE + allocation recalc, 5 Hz) ---
                //
                // Runs in its own dedicated select! arm so that its cost (~30-80µs)
                // is isolated to a single Tokio iteration.  Before this change,
                // slow poll fired at the top of the outer loop and its cost was
                // SUMMED with the arm that happened to fire in the same iteration,
                // pushing P99 poll time to ~229µs.  Now slow-poll iterations show
                // elevated poll time but packet-processing iterations do not.
                _ = &mut slow_poll_sleep => {
                    slow_poll_sleep
                        .as_mut()
                        .reset(Instant::now() + SLOW_POLL_INTERVAL);
                    maybe_deadline = self.core.run_slow_poll();
                }

                // --- Priority 4: Downstream RTP forwarding (hot path) ---
                Some((meta, pkt)) = self.core.downstream.next() => {
                    self.core.handle_forward_rtp(meta, pkt);

                    for _ in 0..MAX_DRAIN_PER_WAKE {
                        match self.core.downstream.next().now_or_never() {
                            Some(Some((meta, pkt))) => self.core.handle_forward_rtp(meta, pkt),
                            _ => break,
                        }
                    }

                    // poll_fast: never runs poll_slow, bounded and predictable.
                    maybe_deadline = self.core.poll_fast();
                    // Do NOT yield_now() here: at 30fps all 300 tasks would yield
                    // simultaneously, flooding the run queue (yield storm) and
                    // driving scheduling P99 to 400µs+. The drain cap of
                    // MAX_DRAIN_PER_WAKE already bounds the per-poll slice; the
                    // natural select! suspend when the channel empties is the
                    // correct yield point.
                }

                // --- Priority 5: UDP ingress (hot path) ---
                Ok(batch) = gateway_rx.recv() => {
                    maybe_deadline = self.core.handle_udp_packet_batch(batch, now);

                    for _ in 0..MAX_DRAIN_PER_WAKE {
                        match gateway_rx.recv().now_or_never() {
                            Some(Ok(batch)) => {
                                maybe_deadline = self.core.handle_udp_packet_batch(batch, now);
                            }
                            _ => break,
                        }
                    }
                    // Same reasoning as downstream: no yield_now() to avoid yield storm.
                }

                // --- Priority 6: RTC timer tick ---
                _ = &mut sleep => {
                    maybe_deadline = self.core.handle_tick();
                }
            }

            // Non-blocking egress flush: send any data that accumulated in the
            // batchers this iteration.  `flush()` calls `try_send_batch` which
            // returns immediately on WouldBlock, so this is never a yield point.
            // Previously, two dedicated `writable()` select! arms handled this;
            // removing them eliminates two epoll waker registrations per iteration
            // (~2 of the ~13 waker clone/drop ops seen at P99 in tokio-console).
            if !self.core.udp_batcher.is_empty() {
                self.core.udp_batcher.flush(&self.udp_egress);
            }
            if !self.core.tcp_batcher.is_empty() {
                self.core.tcp_batcher.flush(&self.tcp_egress);
            }

            // Drain SpawnTrack events via non-blocking try_send. This fires at most
            // twice per participant lifetime (one audio track, one video track) so
            // the clone + loop overhead is negligible. Using try_send instead of
            // reserve_many eliminates all waker registrations on the room channel,
            // which was the primary source of the 1.1M waker clone/drop events seen
            // in the tokio-console trace.
            let mut i = 0;
            while i < self.core.events.len() {
                let track_rx = match &self.core.events[i] {
                    CoreEvent::SpawnTrack(rx) => rx.clone(),
                };
                match self
                    .room_handle
                    .tx
                    .try_send(room::RoomMessage::PublishTrack(track_rx))
                {
                    Ok(()) => {
                        // Sent successfully; remove and keep index pointing at next element.
                        self.core.events.swap_remove(i);
                    }
                    Err(TrySendError::Full(_)) => {
                        // Room mailbox temporarily full (backpressure). Leave the event
                        // in place and retry on the next iteration. The mailbox is
                        // 1024-deep so this resolves within microseconds under any
                        // realistic load.
                        i += 1;
                    }
                    Err(TrySendError::Closed(_)) => {
                        tracing::error!("Room handle closed, shutting down participant actor");
                        break 'actor;
                    }
                }
            }
        }

        if let Some(reason) = self.core.disconnect_reason() {
            tracing::info!(participant_id = %self.meta(), %reason, "Shutting down actor due to disconnect.");
        } else {
            tracing::info!(participant_id = %self.meta(), "Shutting down actor.");
        }
        let _ = self
            .gateway
            .send(gateway::GatewayControlMessage::RemoveParticipant(ufrag))
            .await;
        Ok(())
    }
}

impl ParticipantActor {
    pub fn new(
        gateway_handle: GatewayWorkerHandle,
        room_handle: room::RoomHandle,
        udp_egress: UnifiedSocketWriter,
        tcp_egress: UnifiedSocketWriter,
        participant_id: entity::ParticipantId,
        rtc: Rtc,
        manual_sub: bool,
        tracks_rx: room::TrackWatchReceiver,
    ) -> Self {
        let udp_batcher = Batcher::with_capacity(udp_egress.max_gso_segments());
        let tcp_batcher = Batcher::with_capacity(tcp_egress.max_gso_segments());
        let core = Box::new(ParticipantCore::new(
            manual_sub,
            participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
        ));
        // Snapshot the initial track map so the diff in the first `changed()` event
        // starts from a known baseline rather than an empty map.
        let prev_tracks = Arc::clone(&*tracks_rx.borrow());
        Self {
            core,
            udp_egress,
            tcp_egress,
            tracks_rx,
            prev_tracks,
            room_handle,
            gateway: gateway_handle,
        }
    }
}

pub type ParticipantHandle = actor::ActorHandle<ParticipantMessageSet>;
