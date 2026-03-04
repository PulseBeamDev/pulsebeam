//! Room-based proactor shard.
//!
//! One `RoomShard` task replaces N individual `ParticipantActor` tasks.  It
//! co-operatively drives up to 64 participant state machines in three phases
//! per wakeup:
//!
//!   1. **Ingress drain** – pull all pending `ShardMessage`s from the channel
//!      with `try_recv` (zero waker cost per packet).
//!   2. **Downstream drain** – read the ready bits from the `BitSignal` and
//!      call `core.downstream.try_next()` for every set bit until the ring is
//!      empty.
//!   3. **Timer phase** – advance every participant to its next deadline.
//!   4. **Sleep** – park on the first of: new message, signal, or timer.
//!
//! Notifications from SPMC rings use `BitSignal`: when a publisher writes a
//! packet the O(1) atomic OR sets the subscriber's slot bit and wakes the
//! shard waker exactly once regardless of how many packets are buffered.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use pulsebeam_runtime::net;
use pulsebeam_runtime::net::UnifiedSocketWriter;
use pulsebeam_runtime::sync::bit_signal::BitSignal;
use pulsebeam_runtime::sync::mpsc;
use tokio::time::Instant;

use crate::entity::{ConnectionId, ParticipantId, TrackId};
use crate::gateway;
use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::room;
use crate::track::TrackReceiver;

const MIN_QUANTA: Duration = Duration::from_millis(1);

// ---------------------------------------------------------------------------
// Public message type
// ---------------------------------------------------------------------------

/// Messages handled by the `RoomShard` task.
pub enum ShardMessage {
    /// A UDP/TCP packet batch arriving from the gateway for slot `slot_idx`.
    IngressBatch(u8, net::RecvPacketBatch),
    /// A new track was published by someone in the room.
    TracksPublished(Arc<HashMap<TrackId, TrackReceiver>>),
    /// Tracks were removed.
    TracksUnpublished(Arc<HashMap<TrackId, TrackReceiver>>),
    /// Add a new participant to the shard.
    AddParticipant(ParticipantSlot),
    /// Remove a participant identified by (ParticipantId, ConnectionId).
    RemoveParticipant(ParticipantId, ConnectionId),
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// Owned by the room actor and controller. Used to inject messages into the
/// shard and to share the `BitSignal` with SPMC rings.
#[derive(Clone)]
pub struct ShardHandle {
    pub tx: mpsc::Sender<ShardMessage>,
    pub signal: Arc<BitSignal>,
}

/// Stored in the demuxer per-ufrag. Wraps the shard's control channel and the
/// slot index so the gateway can deliver ingress batches without looking up
/// routing tables beyond the ufrag map.
#[derive(Clone)]
pub struct ShardRouteHandle {
    pub tx: mpsc::Sender<ShardMessage>,
    pub slot_idx: u8,
}

impl ShardRouteHandle {
    /// Attempt to push a packet batch without blocking.
    pub fn try_send(&self, batch: net::RecvPacketBatch) -> Result<(), net::RecvPacketBatch> {
        self.tx
            .try_send(ShardMessage::IngressBatch(self.slot_idx, batch))
            .map_err(|e| match e {
                mpsc::TrySendError::Closed(ShardMessage::IngressBatch(_, b)) => b,
                _ => unreachable!(),
            })
    }

    /// Current fill ratio of the underlying MPSC channel (0.0-1.0).
    pub fn fill_ratio(&self) -> f64 {
        self.tx.fill_ratio()
    }
}

// ---------------------------------------------------------------------------
// Participant slot (before insertion into shard)
// ---------------------------------------------------------------------------

/// One participant's state, created by the controller and sent to the shard.
pub struct ParticipantSlot {
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    /// ICE ufrag -- needed to register/deregister with the gateway worker.
    pub ufrag: String,
    pub core: ParticipantCore,
    pub udp_egress: UnifiedSocketWriter,
    pub tcp_egress: UnifiedSocketWriter,
    /// Current room tracks delivered on join.
    pub initial_tracks: HashMap<TrackId, TrackReceiver>,
    pub gateway: gateway::GatewayWorkerHandle,
}

impl ParticipantSlot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        ufrag: String,
        core: ParticipantCore,
        udp_egress: UnifiedSocketWriter,
        tcp_egress: UnifiedSocketWriter,
        initial_tracks: HashMap<TrackId, TrackReceiver>,
        gateway: gateway::GatewayWorkerHandle,
    ) -> Self {
        Self {
            participant_id,
            connection_id,
            ufrag,
            core,
            udp_egress,
            tcp_egress,
            initial_tracks,
            gateway,
        }
    }

    /// Convenience constructor that builds `Batcher` + `ParticipantCore` from raw ingredients.
    /// Extracts `ufrag` from `rtc` before consuming it into the core.
    #[allow(clippy::too_many_arguments)]
    pub fn from_rtc(
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        manual_sub: bool,
        mut rtc: str0m::Rtc,
        udp_egress: UnifiedSocketWriter,
        tcp_egress: UnifiedSocketWriter,
        initial_tracks: HashMap<TrackId, TrackReceiver>,
        gateway: gateway::GatewayWorkerHandle,
    ) -> Self {
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag.clone();
        let udp_batcher = super::batcher::Batcher::with_capacity(udp_egress.max_gso_segments());
        let tcp_batcher = super::batcher::Batcher::with_capacity(tcp_egress.max_gso_segments());
        let core = ParticipantCore::new(
            manual_sub,
            participant_id,
            rtc,
            udp_batcher,
            tcp_batcher,
        );
        Self {
            participant_id,
            connection_id,
            ufrag,
            core,
            udp_egress,
            tcp_egress,
            initial_tracks,
            gateway,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal live slot (after insertion into shard array)
// ---------------------------------------------------------------------------

struct LiveSlot {
    participant_id: ParticipantId,
    connection_id: ConnectionId,
    ufrag: String,
    core: ParticipantCore,
    udp_egress: UnifiedSocketWriter,
    tcp_egress: UnifiedSocketWriter,
    deadline: Option<Instant>,
    gateway: gateway::GatewayWorkerHandle,
}

impl LiveSlot {
    fn flush_egress(&mut self) {
        self.core.udp_batcher.flush(&self.udp_egress);
        self.core.tcp_batcher.flush(&self.tcp_egress);
    }
}

// ---------------------------------------------------------------------------
// RoomShard
// ---------------------------------------------------------------------------

/// A single cooperative task driving all participants in one room.
pub struct RoomShard {
    signal: Arc<BitSignal>,
    /// Clone of the sender used when constructing `ShardRouteHandle`s.
    tx: mpsc::Sender<ShardMessage>,
    rx: mpsc::Receiver<ShardMessage>,
    slots: Box<[Option<LiveSlot>; 64]>,
    /// Bitmask of occupied slots (bit i <=> slots[i].is_some()).
    occupied: u64,
    /// Min-heap of participant deadlines encoded as Reverse(max-heap tuple).
    deadline_heap: BinaryHeap<Reverse<(Instant, u8, u64)>>,
    /// Per-slot generation used to invalidate stale heap entries.
    deadline_generation: [u64; 64],
    room_handle: room::RoomHandle,
}

impl RoomShard {
    pub const MAX_PARTICIPANTS: usize = 64;

    pub fn new(room_handle: room::RoomHandle) -> (Self, ShardHandle) {
        let signal = BitSignal::new();
        let (tx, rx) = mpsc::channel(512);
        let handle = ShardHandle {
            tx: tx.clone(),
            signal: signal.clone(),
        };
        let shard = Self {
            signal,
            tx,
            rx,
            slots: Box::new(std::array::from_fn(|_| None)),
            occupied: 0,
            deadline_heap: BinaryHeap::new(),
            deadline_generation: [0; 64],
            room_handle,
        };
        (shard, handle)
    }

    fn schedule_deadline(&mut self, idx: u8, deadline: Instant) {
        let i = idx as usize;
        self.deadline_generation[i] = self.deadline_generation[i].wrapping_add(1);
        let generation = self.deadline_generation[i];
        self.deadline_heap
            .push(Reverse((deadline, idx, generation)));
    }

    fn schedule_soon(&mut self, idx: u8) {
        self.schedule_deadline(idx, Instant::now() + MIN_QUANTA);
    }

    fn next_valid_deadline(&mut self) -> Option<Instant> {
        loop {
            let Reverse((deadline, idx, generation)) = *self.deadline_heap.peek()?;
            let i = idx as usize;
            let occupied = (self.occupied & (1u64 << i)) != 0;
            if !occupied || self.deadline_generation[i] != generation {
                self.deadline_heap.pop();
                continue;
            }
            return Some(deadline);
        }
    }

    fn collect_due_slots(&mut self, now: Instant, out: &mut Vec<u8>) {
        while let Some(Reverse((deadline, idx, generation))) = self.deadline_heap.peek().copied() {
            if deadline > now {
                break;
            }
            self.deadline_heap.pop();
            let i = idx as usize;
            let occupied = (self.occupied & (1u64 << i)) != 0;
            if !occupied || self.deadline_generation[i] != generation {
                continue;
            }
            out.push(idx);
        }
    }

    // ------------------------------------------------------------------
    // Slot management
    // ------------------------------------------------------------------

    fn alloc_slot(&mut self, mut slot: ParticipantSlot) -> Option<u8> {
        let idx = (!self.occupied).trailing_zeros() as usize;
        if idx >= Self::MAX_PARTICIPANTS {
            return None;
        }
        let bit = 1u64 << idx;
        self.occupied |= bit;

        // Deliver initial tracks before attaching the signal so downstream
        // receivers exist before any ring wakeups can arrive.
        let tracks = std::mem::take(&mut slot.initial_tracks);
        slot.core.handle_available_tracks(&tracks);

        // Attach the shard signal so SPMC rings notify this shard.
        slot.core.downstream.attach_shard_signal(self.signal.clone(), bit);

        self.slots[idx] = Some(LiveSlot {
            participant_id: slot.participant_id,
            connection_id: slot.connection_id,
            ufrag: slot.ufrag,
            core: slot.core,
            udp_egress: slot.udp_egress,
            tcp_egress: slot.tcp_egress,
            deadline: Some(Instant::now() + MIN_QUANTA),
            gateway: slot.gateway,
        });
        self.schedule_soon(idx as u8);
        Some(idx as u8)
    }

    fn free_slot(&mut self, idx: u8) {
        let i = idx as usize;
        if let Some(mut slot) = self.slots[i].take() {
            self.occupied &= !(1u64 << i);
            self.deadline_generation[i] = self.deadline_generation[i].wrapping_add(1);
            let ufrag = std::mem::take(&mut slot.ufrag);
            let mut gateway = slot.gateway.clone();
            let mut room = self.room_handle.clone();
            let (pid, cid) = (slot.participant_id, slot.connection_id);
            tokio::spawn(async move {
                let _ = gateway
                    .send(gateway::GatewayControlMessage::RemoveParticipant(ufrag))
                    .await;
                let _ = room
                    .send(room::RoomMessage::ParticipantExited(pid, cid))
                    .await;
            });
        }
    }

    fn find_slot(&self, pid: ParticipantId, cid: ConnectionId) -> Option<u8> {
        for i in 0..Self::MAX_PARTICIPANTS {
            if self.occupied & (1u64 << i) == 0 {
                continue;
            }
            if let Some(s) = &self.slots[i] {
                if s.participant_id == pid && s.connection_id == cid {
                    return Some(i as u8);
                }
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Proactor loop
    // ------------------------------------------------------------------

    pub async fn run(mut self) {
        loop {
            // ---- Phase 1+2: drain messages and forward packets ----
            // Loop until neither the channel nor any downstream ring has work.
            let mut did_work = true;
            while did_work {
                did_work = false;

                // Try-recv from the control channel (noop waker = no listener alloc).
                // cx must be dropped before any `.await`, so we extract the result first.
                loop {
                    let poll_result = {
                        let noop = std::task::Waker::noop();
                        let mut cx = Context::from_waker(noop);
                        self.rx.poll_recv(&mut cx)
                    };
                    match poll_result {
                        Poll::Ready(Ok(msg)) => {
                            self.handle_message(msg).await;
                            did_work = true;
                        }
                        Poll::Ready(Err(_)) => {
                            if self.occupied == 0 {
                                return;
                            }
                            break;
                        }
                        Poll::Pending => break,
                    }
                }

                // Drain downstream rings flagged by the BitSignal.
                let mut ready_bits = self.signal.take();
                while ready_bits != 0 {
                    let idx = ready_bits.trailing_zeros() as usize;
                    ready_bits &= ready_bits - 1;
                    let mut reschedule_soon = false;
                    if let Some(slot) = &mut self.slots[idx] {
                        let mut produced = false;
                        while let Some((mid, pkt)) = slot.core.downstream.try_next() {
                            slot.core.handle_forward_rtp(mid, pkt);
                            did_work = true;
                            produced = true;
                        }
                        if !produced {
                            // Signaled but no packet produced.  This happens when a Paused
                            // slot's upstream ring gets its first packet (stream became
                            // active).  Force an immediate allocation re-check so the slot
                            // transitions Paused → Resuming without waiting for poll_slow.
                            slot.core.downstream.dirty_allocation = true;
                            reschedule_soon = true;
                        }
                        slot.flush_egress();
                    }
                    if reschedule_soon {
                        self.schedule_soon(idx as u8);
                    }
                }
            }

            // ---- Phase 3: Timer phase ----
            let now = Instant::now();
            let mut due_slots = Vec::new();
            self.collect_due_slots(now, &mut due_slots);
            let mut to_free: Vec<u8> = Vec::new();

            for idx in due_slots {
                let i = idx as usize;
                let Some(slot) = &mut self.slots[i] else {
                    continue;
                };
                let mut next_deadline_for_slot: Option<Instant> = None;
                let mut should_free = false;

                // Drain keyframe requests. `take_pending` is O(1) (atomic load)
                // when nothing is pending so this is always cheap.
                let reqs: Vec<_> =
                    slot.core.upstream.drain_keyframe_requests(now).collect();
                for req in reqs {
                    slot.core.handle_keyframe_request(req);
                }

                // Drive the RTC engine.
                let deadline = slot.core.poll();
                slot.flush_egress();

                // Propagate CoreEvents to the room.
                // Use try_send (non-blocking) so the shard never yields here.
                // The room actor has a generous mailbox; if it is momentarily
                // full we simply discard the publish – this is a transient
                // failure that manifests as the track never appearing in the
                // room, which is already handled by reconnection flows.
                for event in slot.core.events.drain(..) {
                    match event {
                        CoreEvent::SpawnTrack(rx) => {
                            let _ = self
                                .room_handle
                                .try_send(room::RoomMessage::PublishTrack(rx));
                        }
                    }
                }

                match deadline {
                    None => {
                        should_free = true;
                    }
                    Some(d) => {
                        let adjusted = d.max(now + MIN_QUANTA);
                        slot.deadline = Some(adjusted);
                        next_deadline_for_slot = Some(adjusted);
                    }
                }

                if should_free {
                    to_free.push(idx);
                } else if let Some(adjusted) = next_deadline_for_slot {
                    self.schedule_deadline(idx, adjusted);
                }
            }

            for idx in to_free {
                self.free_slot(idx);
            }

            // ---- Phase 4: Sleep until next event ----
            let sleep_until = self
                .next_valid_deadline()
                .unwrap_or(now + Duration::from_secs(30));

            tokio::select! {
                biased;
                res = self.rx.recv() => {
                    match res {
                        Ok(msg) => self.handle_message(msg).await,
                        Err(_) if self.occupied == 0 => return,
                        Err(_) => {}
                    }
                }
                _ = std::future::poll_fn(|cx: &mut Context<'_>| {
                    self.signal.register(cx);
                    if self.signal.has_pending() { Poll::Ready(()) } else { Poll::Pending }
                }) => {}
                _ = tokio::time::sleep_until(sleep_until) => {}
            }
        }
    }

    // ------------------------------------------------------------------
    // Message dispatch
    // ------------------------------------------------------------------

    async fn handle_message(&mut self, msg: ShardMessage) {
        match msg {
            ShardMessage::IngressBatch(slot_idx, batch) => {
                let i = slot_idx as usize;
                let mut new_deadline = None;
                if let Some(slot) = &mut self.slots[i] {
                    let now = Instant::now();
                    if let Some(d) = slot.core.handle_udp_packet_batch(batch, now) {
                        let adjusted = d.max(now + MIN_QUANTA);
                        slot.deadline = Some(adjusted);
                        new_deadline = Some(adjusted);
                    }
                    slot.flush_egress();
                }
                if let Some(deadline) = new_deadline {
                    self.schedule_deadline(slot_idx, deadline);
                }
            }
            ShardMessage::AddParticipant(slot) => {
                let ufrag = slot.ufrag.clone();
                let mut gateway = slot.gateway.clone();
                if let Some(idx) = self.alloc_slot(slot) {
                    let route = ShardRouteHandle {
                        tx: self.tx.clone(),
                        slot_idx: idx,
                    };
                    tokio::spawn(async move {
                        let _ = gateway
                            .send(gateway::GatewayControlMessage::AddParticipant(
                                ufrag, route,
                            ))
                            .await;
                    });
                } else {
                    tracing::error!("RoomShard: all 64 slots occupied, dropping participant");
                    tokio::spawn(async move {
                        let _ = gateway
                            .send(gateway::GatewayControlMessage::RemoveParticipant(ufrag))
                            .await;
                    });
                }
            }
            ShardMessage::RemoveParticipant(pid, cid) => {
                if let Some(idx) = self.find_slot(pid, cid) {
                    if let Some(slot) = &mut self.slots[idx as usize] {
                        slot.core.disconnect(
                            crate::participant::core::DisconnectReason::SystemTerminated,
                        );
                    }
                    self.free_slot(idx);
                }
            }
            ShardMessage::TracksPublished(tracks) => {
                self.broadcast_tracks(|core| core.handle_available_tracks(&tracks));
            }
            ShardMessage::TracksUnpublished(tracks) => {
                self.broadcast_tracks(|core| core.remove_available_tracks(&tracks));
            }
        }
    }

    fn broadcast_tracks<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut ParticipantCore),
    {
        let mut touched = Vec::new();
        for i in 0..Self::MAX_PARTICIPANTS {
            if self.occupied & (1u64 << i) != 0 {
                if let Some(slot) = &mut self.slots[i] {
                    f(&mut slot.core);
                    touched.push(i as u8);
                }
            }
        }
        for idx in touched {
            self.schedule_soon(idx);
        }
    }
}
