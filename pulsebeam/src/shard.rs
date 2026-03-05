//! Co-located participant shard.
//!
//! A [`ShardLoop`] owns a bounded set of participants and drives them all in a
//! *single* tokio task.  Within a shard the entire media forwarding path is
//! synchronous: no async channels, no lock contention, no waker registrations
//! on the hot path.
//!
//! # Parallelism
//!
//! Rooms with few participants (≤ [`MAX_PARTICIPANTS_PER_SHARD`]) run as a
//! single shard — zero intra-room synchronization.  Larger rooms spawn
//! additional shards; cross-shard forwarding uses one `Arc`-clone + channel
//! send per shard, so synchronization scales with shard count rather than
//! participant count.
//!
//! # Media path
//!
//! ```text
//! GatewayWorker → mpsc::try_recv → ParticipantCore::handle_udp_packet_batch
//!                                       ↓
//!                               str0m Event::RtpPacket
//!                                       ↓
//!                         UpstreamAllocator → spmc ring (same task)
//!                                       ↓
//!                      subscriber ParticipantCore::try_drain_egress
//!                         (spmc::try_recv — no waker, no lock wait)
//!                                       ↓
//!                               handle_forward_rtp → batcher → socket
//! ```

use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::Instant;

use crate::{
    entity::{ConnectionId, ParticipantId, TrackId},
    gateway::{self, GatewayWorkerHandle},
    participant::{
        ParticipantActor, ParticipantControlMessage,
        core::CoreEvent,
    },
    track::TrackReceiver,
};

/// Maximum participants co-located in a single shard.  When a room reaches
/// this threshold the room coordinator spawns a new shard.
pub const MAX_PARTICIPANTS_PER_SHARD: usize = 20;

const MIN_QUANTA: Duration = Duration::from_millis(1);

// ─── Control messages ────────────────────────────────────────────────────────

/// Messages sent **to** a shard from the room coordinator.
pub enum ShardControlMessage {
    /// Place a new participant into this shard and register it with the gateway.
    AddParticipant {
        actor: ParticipantActor,
        connection_id: ConnectionId,
        /// Snapshot of all currently-published tracks the new participant
        /// should subscribe to (sent by the room immediately after add).
        current_tracks: HashMap<TrackId, TrackReceiver>,
    },
    /// Evict a participant (e.g. reconnect or forceful removal).
    RemoveParticipant {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    },
    /// A new track became available — route to all participant cores in this shard.
    TracksPublished(Arc<HashMap<TrackId, TrackReceiver>>),
    /// A track was removed — route to all participant cores in this shard.
    TracksUnpublished(Arc<HashMap<TrackId, TrackReceiver>>),
    /// Terminate the shard loop.
    Shutdown,
}

/// Messages sent **from** a shard **to** the room coordinator.
pub enum ShardToRoomMessage {
    /// A participant published a new upstream track.
    TrackPublished {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        rx: TrackReceiver,
    },
    /// A participant's RTC engine disconnected and has been removed.
    ParticipantExited {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    },
}

// ─── Handle ──────────────────────────────────────────────────────────────────

/// Cheap clone handle used by the room coordinator to control a shard.
#[derive(Clone)]
pub struct ShardHandle {
    pub tx: tokio_mpsc::Sender<ShardControlMessage>,
    /// Live participant count; updated by the shard task.
    pub count: Arc<std::sync::atomic::AtomicUsize>,
}

impl ShardHandle {
    pub fn participant_count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn send(&self, msg: ShardControlMessage) {
        let _ = self.tx.send(msg).await;
    }
}

// ─── Internal entry ──────────────────────────────────────────────────────────

struct ParticipantEntry {
    participant_id: ParticipantId,
    connection_id: ConnectionId,
    core: crate::participant::core::ParticipantCore,
    ufrag: String,
    gateway_tx: pulsebeam_runtime::sync::mpsc::Sender<pulsebeam_runtime::net::RecvPacketBatch>,
    gateway_rx: pulsebeam_runtime::sync::mpsc::Receiver<pulsebeam_runtime::net::RecvPacketBatch>,
    udp_egress: pulsebeam_runtime::net::UnifiedSocketWriter,
    tcp_egress: pulsebeam_runtime::net::UnifiedSocketWriter,
    /// Deadline returned by the last `core.poll()`.  `None` = rtc disconnected.
    deadline: Option<Instant>,
    /// Scratch buffer for `try_drain_egress`; reused across iterations.
    egress_scratch: Vec<(str0m::media::Mid, crate::rtp::RtpPacket)>,
}

// ─── Loop ────────────────────────────────────────────────────────────────────

struct ShardLoop {
    shard_id: usize,
    participants: Vec<ParticipantEntry>,
    ctrl_rx: tokio_mpsc::Receiver<ShardControlMessage>,
    room_tx: tokio_mpsc::Sender<ShardToRoomMessage>,
    gateway: GatewayWorkerHandle,
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl ShardLoop {
    async fn run(mut self) {
        let sleep = tokio::time::sleep(MIN_QUANTA);
        tokio::pin!(sleep);

        tracing::debug!(shard_id = self.shard_id, "Shard loop started");

        'outer: loop {
            // ── 1. Drain all pending control messages (non-blocking) ──────────
            loop {
                match self.ctrl_rx.try_recv() {
                    Ok(msg) => {
                        if matches!(msg, ShardControlMessage::Shutdown) {
                            tracing::info!(shard_id = self.shard_id, "Shard received Shutdown");
                            break 'outer;
                        }
                        self.handle_control(msg).await;
                    }
                    Err(_) => break,
                }
            }

            // If no participants yet, block cheaply until one arrives.
            if self.participants.is_empty() {
                tokio::select! {
                    biased;
                    msg = self.ctrl_rx.recv() => match msg {
                        Some(ShardControlMessage::Shutdown) | None => break 'outer,
                        Some(msg) => self.handle_control(msg).await,
                    },
                }
                continue;
            }

            let now = Instant::now();

            // ── 2. Drain all inbound UDP for every participant (try_recv) ─────
            for entry in &mut self.participants {
                while let Some(batch) = entry.gateway_rx.try_recv() {
                    entry.core.handle_udp_packet_batch(batch, now);
                }
            }

            // ── 3. Drive str0m poll + flush transmit batchers ─────────────────
            for entry in &mut self.participants {
                entry.deadline = entry.core.poll();
                entry.core.udp_batcher.flush(&entry.udp_egress);
                entry.core.tcp_batcher.flush(&entry.tcp_egress);
            }

            // ── 4. Drain downstream egress synchronously ──────────────────────
            //
            // Because the upstream spmc::Sender was written during step 2/3
            // (same task), the ring already contains fresh packets.
            // try_recv reads them without registering any waker.
            for entry in &mut self.participants {
                entry.core.try_drain_egress(&mut entry.egress_scratch);
                entry.core.udp_batcher.flush(&entry.udp_egress);
                entry.core.tcp_batcher.flush(&entry.tcp_egress);
            }

            // ── 5. Handle keyframe requests ───────────────────────────────────
            for entry in &mut self.participants {
                let now = Instant::now();
                let reqs: Vec<_> = entry.core.upstream.drain_keyframe_requests(now).collect();
                for req in reqs {
                    entry.core.handle_keyframe_request(req);
                }
            }

            // ── 6. Collect and dispatch core events ───────────────────────────
            let mut tracks_to_publish: Vec<(ParticipantId, ConnectionId, TrackReceiver)> =
                Vec::new();
            let mut exited: Vec<(ParticipantId, ConnectionId, String)> = Vec::new();

            for entry in &mut self.participants {
                for event in entry.core.events.drain(..) {
                    match event {
                        CoreEvent::SpawnTrack(rx) => {
                            tracks_to_publish.push((
                                entry.participant_id,
                                entry.connection_id,
                                rx,
                            ));
                        }
                    }
                }
                if entry.core.disconnect_reason().is_some() {
                    exited.push((
                        entry.participant_id,
                        entry.connection_id,
                        entry.ufrag.clone(),
                    ));
                }
            }

            for (participant_id, connection_id, rx) in tracks_to_publish {
                let _ = self
                    .room_tx
                    .send(ShardToRoomMessage::TrackPublished {
                        participant_id,
                        connection_id,
                        rx,
                    })
                    .await;
            }

            for (participant_id, connection_id, ufrag) in exited {
                self.evict_by_ufrag(&ufrag).await;
                tracing::info!(
                    shard_id = self.shard_id,
                    ?participant_id,
                    ?connection_id,
                    "Participant exited"
                );
                let _ = self
                    .room_tx
                    .send(ShardToRoomMessage::ParticipantExited {
                        participant_id,
                        connection_id,
                    })
                    .await;
            }

            self.count
                .store(self.participants.len(), std::sync::atomic::Ordering::Relaxed);

            // ── 7. Sleep until the nearest deadline or control arrives ─────────
            let min_deadline = self
                .participants
                .iter()
                .filter_map(|e| e.deadline)
                .min();

            match min_deadline {
                // All participants disconnected and cleaning in progress — yield once.
                None => tokio::task::yield_now().await,
                Some(deadline) => {
                    let adjusted = deadline.max(now + MIN_QUANTA);
                    sleep.as_mut().reset(adjusted.into());
                    tokio::select! {
                        biased;
                        msg = self.ctrl_rx.recv() => match msg {
                            Some(ShardControlMessage::Shutdown) | None => break 'outer,
                            Some(msg) => self.handle_control(msg).await,
                        },
                        _ = &mut sleep => {}
                    }
                }
            }
        }

        // Clean up: deregister all remaining participants from the gateway.
        let ufrags: Vec<String> = self.participants.iter().map(|e| e.ufrag.clone()).collect();
        for ufrag in ufrags {
            let _ = self
                .gateway
                .send(gateway::GatewayControlMessage::RemoveParticipant(ufrag))
                .await;
        }

        tracing::debug!(shard_id = self.shard_id, "Shard loop exited");
    }

    async fn handle_control(&mut self, msg: ShardControlMessage) {
        match msg {
            ShardControlMessage::AddParticipant {
                actor,
                connection_id,
                current_tracks,
            } => {
                // Register with the gateway demuxer.
                let _ = self
                    .gateway
                    .send(gateway::GatewayControlMessage::AddParticipant(
                        actor.ufrag.clone(),
                        actor.gateway_tx.clone(),
                    ))
                    .await;

                let mut core = actor.core;

                // Feed the current track snapshot so the participant can
                // immediately subscribe to already-published streams.
                core.handle_available_tracks(&current_tracks);

                tracing::info!(
                    shard_id = self.shard_id,
                    participant_id = ?core.participant_id,
                    ?connection_id,
                    "Participant added to shard"
                );

                self.participants.push(ParticipantEntry {
                    participant_id: core.participant_id,
                    connection_id,
                    ufrag: actor.ufrag,
                    gateway_tx: actor.gateway_tx,
                    gateway_rx: actor.gateway_rx,
                    udp_egress: actor.udp_egress,
                    tcp_egress: actor.tcp_egress,
                    core,
                    deadline: None,
                    egress_scratch: Vec::new(),
                });

                self.count
                    .store(self.participants.len(), std::sync::atomic::Ordering::Relaxed);
            }

            ShardControlMessage::RemoveParticipant {
                participant_id,
                connection_id,
            } => {
                if let Some(idx) = self.participants.iter().position(|e| {
                    e.participant_id == participant_id && e.connection_id == connection_id
                }) {
                    let entry = self.participants.swap_remove(idx);
                    let _ = self
                        .gateway
                        .send(gateway::GatewayControlMessage::RemoveParticipant(
                            entry.ufrag,
                        ))
                        .await;
                    self.count
                        .store(self.participants.len(), std::sync::atomic::Ordering::Relaxed);
                }
            }

            ShardControlMessage::TracksPublished(tracks) => {
                for entry in &mut self.participants {
                    entry.core.handle_available_tracks(&tracks);
                }
            }

            ShardControlMessage::TracksUnpublished(tracks) => {
                for entry in &mut self.participants {
                    entry.core.remove_available_tracks(&tracks);
                }
            }

            ShardControlMessage::Shutdown => unreachable!("handled in outer loop"),
        }
    }

    /// Remove a participant identified by ufrag (used on disconnect).
    async fn evict_by_ufrag(&mut self, ufrag: &str) {
        if let Some(idx) = self.participants.iter().position(|e| e.ufrag == ufrag) {
            let entry = self.participants.swap_remove(idx);
            let _ = self
                .gateway
                .send(gateway::GatewayControlMessage::RemoveParticipant(
                    entry.ufrag,
                ))
                .await;
        }
    }
}

// ─── Public constructor ───────────────────────────────────────────────────────

/// Spawn a new shard task and return its handle.
///
/// `room_tx` is used by the shard to report track publications and participant
/// exits back to the room coordinator.
pub fn spawn(
    shard_id: usize,
    gateway: GatewayWorkerHandle,
    room_tx: tokio_mpsc::Sender<ShardToRoomMessage>,
) -> ShardHandle {
    let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel(64);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let shard = ShardLoop {
        shard_id,
        participants: Vec::new(),
        ctrl_rx,
        room_tx,
        gateway,
        count: count.clone(),
    };

    tokio::spawn(shard.run());

    ShardHandle {
        tx: ctrl_tx,
        count,
    }
}
