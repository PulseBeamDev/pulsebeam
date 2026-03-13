use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::slot_group::SlotGroup;
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::fmt::Display;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{Frequency, KeyframeRequest, Mid};
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastQuality, SimulcastReceiver, TrackMeta, TrackReceiver};

/// Per-slot construction parameters.  Pass one of these to [`VideoAllocator::add_slot`].
#[derive(Clone, Debug)]
pub struct SlotConfig {
    /// RTP clock rate forwarded to the [`Switcher`].
    pub clock_rate: Frequency,
}

impl Default for SlotConfig {
    fn default() -> Self {
        Self {
            clock_rate: rtp::VIDEO_FREQUENCY,
        }
    }
}

/// Maximum number of video slots per participant.  Bounded at 25 (well within
/// the 64-slot limit of [`SlotGroup`]).
const VIDEO_MAX_SLOTS: usize = 25;

pub struct VideoAllocator {
    manual_sub: bool,
    tracks: HashMap<TrackId, TrackState>,
    /// Inline, contiguous storage of all slot drivers.  The hot path polls this
    /// directly via `Stream::poll_next`; the cold path reaches individual drivers
    /// through `get_mut` / `iter_mut` — no extra indirection.
    slots: SlotGroup<SlotDriver>,
    /// Reverse map from `Mid` to slot index for O(1) cold-path lookup.
    mid_to_idx: HashMap<Mid, usize>,
    ticks: u32,
}

impl VideoAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            manual_sub,
            tracks: HashMap::new(),
            slots: SlotGroup::with_capacity(VIDEO_MAX_SLOTS),
            mid_to_idx: HashMap::new(),
            ticks: 0,
        }
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        let mids: Vec<Mid> = self.mid_to_idx.keys().copied().collect();
        for mid in mids {
            let idx = self.mid_to_idx[&mid];
            let mut driver = self.slots.get_mut(idx).unwrap();
            let tracks = &mut self.tracks;
            if let Some(intent) = intents.get(&mid) {
                Self::configure_slot(
                    tracks,
                    &mut driver,
                    intent.max_height,
                    Some(&intent.track_id),
                )
            } else {
                Self::configure_slot(tracks, &mut driver, 0, None)
            };
        }
    }

    fn configure_slot(
        tracks: &mut HashMap<TrackId, TrackState>,
        driver: &mut SlotDriver,
        max_height: u32,
        track_id: Option<&TrackId>,
    ) -> bool {
        // Clear any previous assignment on the target track.
        if let Some(target) = driver.slot.target()
            && let Some(state) = tracks.get_mut(&target.meta.id)
        {
            state.assigned_mid = None;
        }

        let switched = if let Some(track_id) = track_id
            && max_height > 0
        {
            let Some(track_state) = tracks.get_mut(track_id) else {
                return false;
            };

            let layer = if let Some(target) = driver.slot.target()
                && target.meta.id == track_state.track.meta.id
            {
                target.clone()
            } else {
                track_state.track.lowest_quality().clone()
            };

            driver.switch_to(layer, false);
            track_state.assigned_mid = Some(driver.mid);
            driver.slot.state.is_playing()
        } else {
            driver.stop();
            false
        };

        driver.max_height = max_height;
        switched
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackMeta> {
        self.tracks.values().map(|s| &*s.track.meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> + '_ {
        self.slots.iter().filter_map(|(_, d)| {
            Some(SlotAssignment {
                mid: d.mid,
                track: d.slot.current()?.meta.clone(),
            })
        })
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }
        tracing::info!(track = %track.meta.id, "video track added");
        self.tracks.insert(
            track.meta.id,
            TrackState {
                track,
                assigned_mid: None,
            },
        );
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) {
        if let Some(track) = self.tracks.remove(track_id) {
            tracing::info!(track = %track_id, "video track removed");
            let Some(assigned_mid) = track.assigned_mid else {
                return;
            };
            if let Some(&idx) = self.mid_to_idx.get(&assigned_mid)
                && let Some(mut driver) = self.slots.get_mut(idx)
            {
                driver.stop();
            }
            self.rebalance();
        }
    }

    pub fn add_slot(&mut self, mid: Mid, config: SlotConfig) {
        let idx = self.slots.insert(SlotDriver::new(mid, config));
        self.mid_to_idx.insert(mid, idx);
        self.rebalance();
    }

    fn rebalance(&mut self) {
        if self.manual_sub {
            return;
        }

        let unassigned: Vec<TrackId> = self
            .tracks
            .iter()
            .filter(|(_, s)| {
                assert!(s.track.meta.kind.is_video());
                s.assigned_mid.is_none()
            })
            .map(|(id, _)| *id)
            .collect();

        // Two-phase: collect idle indices first (immutable), then assign (mutable).
        let idle_indices: Vec<usize> = self
            .slots
            .iter()
            .filter(|(_, d)| d.slot.state.is_idle())
            .map(|(i, _)| i)
            .collect();

        for (ui, idx) in idle_indices.into_iter().enumerate() {
            let Some(track_id) = unassigned.get(ui).copied() else {
                break;
            };
            let driver_mid = self.slots.get(idx).unwrap().mid;
            let state = self.tracks.get_mut(&track_id).unwrap();
            state.assigned_mid = Some(driver_mid);
            let receiver = state.track.lowest_quality().clone();
            self.slots.get_mut(idx).unwrap().assign_to(receiver);
        }
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> Bitrate {
        let total = self.tracks.len();
        let assigned = self
            .tracks
            .values()
            .filter(|t| t.assigned_mid.is_some())
            .count();
        let playing = self
            .slots
            .iter()
            .filter(|(_, d)| d.slot.state.is_playing())
            .count();
        tracing::debug!(
            "Allocator State: {} tracks total, {} assigned to slots, {} playing",
            total,
            assigned,
            playing
        );

        // 1. Prepare the input views.
        //    `current_quality` lets the engine apply hysteresis: it only
        //    upgrades when there is genuine headroom *above* what the driver is
        //    already running, preventing the upgrade→overrun→downgrade cycle.
        let views = self
            .slots
            .iter()
            .filter_map(|(_, d)| {
                let current = d.slot.current()?;
                let track = self.tracks.get(&current.meta.id).map(|s| &s.track)?;
                let current_quality = current.quality;

                Some(SlotView {
                    mid: d.mid,
                    priority: d.max_height,
                    track,
                    current_quality,
                })
            })
            .collect();

        // 2. Run the pure allocation logic.
        let (decisions, desired) = AllocationEngine::compute(available_bandwidth, views);

        // 3. Apply the results to the drivers.
        for (mid, decision) in decisions {
            let idx = self.mid_to_idx[&mid];
            let mut driver = self.slots.get_mut(idx).unwrap();

            match decision {
                AllocationDecision::Forward(receiver) => {
                    // Only trigger a switch if the quality actually changed.
                    if driver.slot.current().map(|c| c.quality) != Some(receiver.quality) {
                        driver.switch_to(receiver.clone(), false);
                    }
                }
                AllocationDecision::Pause(receiver) => {
                    // The engine has already chosen the resume-target layer.
                    // Only call pause_at when the driver is not already paused
                    // to avoid redundant state transitions on every tick.
                    if !driver.slot.state.is_paused() {
                        driver.pause_at(receiver.clone());
                    }
                }
                AllocationDecision::Idle => {
                    driver.stop();
                }
            }
        }

        desired
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) {
        let Some(&idx) = self.mid_to_idx.get(&req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        if let Some(driver) = self.slots.get(idx) {
            driver.request_keyframe();
        }
    }

    pub fn poll_slow(&mut self, now: Instant) {
        for (_, mut driver) in self.slots.iter_mut() {
            driver.poll_slow(now);
        }
    }

    #[inline]
    pub(super) fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<(Mid, RtpPacket)>> {
        use futures_lite::stream::Stream as _;
        match Pin::new(&mut self.slots).poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
struct TrackState {
    track: TrackReceiver,
    assigned_mid: Option<Mid>,
}

pub struct SlotAssignment {
    pub mid: Mid,
    pub track: pulsebeam_runtime::sync::Arc<TrackMeta>,
}

pub struct Intent {
    pub track_id: TrackId,
    pub max_height: u32,
}

#[derive(Copy, Clone, Debug)]
enum SlotState {
    Idle,
    Paused,
    Resuming,
    Streaming,
    Switching,
}

impl SlotState {
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. })
    }

    fn is_paused_or_idle(&self) -> bool {
        matches!(self, Self::Idle | Self::Paused { .. })
    }

    fn is_switching(&self) -> bool {
        matches!(self, Self::Resuming { .. } | Self::Switching { .. })
    }

    fn is_playing(&self) -> bool {
        matches!(
            self,
            Self::Streaming { .. } | Self::Switching { .. } | Self::Resuming { .. }
        )
    }
}

impl Display for SlotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Paused { .. } => "paused",
            Self::Resuming { .. } => "resuming",
            Self::Streaming { .. } => "streaming",
            Self::Switching { .. } => "switching",
        })
    }
}

struct Slot {
    state: SlotState,
    active: Option<SimulcastReceiver>,
    staging: Option<SimulcastReceiver>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            state: SlotState::Idle,
            active: None,
            staging: None,
        }
    }
}

impl Slot {
    fn current(&self) -> Option<&SimulcastReceiver> {
        match self.state {
            SlotState::Streaming | SlotState::Switching => self.active.as_ref(),
            SlotState::Resuming | SlotState::Paused => self.staging.as_ref(),
            _ => None,
        }
    }

    fn target(&self) -> Option<&SimulcastReceiver> {
        match self.state {
            SlotState::Resuming | SlotState::Switching | SlotState::Paused => self.staging.as_ref(),
            SlotState::Streaming => self.active.as_ref(),
            _ => None,
        }
    }
}

impl Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.state.fmt(f)
    }
}

struct SlotDriver {
    mid: Mid,
    pub max_height: u32,
    slot: Slot,
    switcher: Switcher,
    switching_started_at: Option<Instant>,
    keyframe_retries: usize,
}

impl futures_lite::stream::Stream for SlotDriver {
    type Item = (Mid, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SlotDriver is Unpin (all fields are Unpin), so get_mut is always safe.
        self.get_mut().poll_packet(cx).map(Some)
    }
}

impl SlotDriver {
    fn new(mid: Mid, config: SlotConfig) -> Self {
        Self {
            mid,
            max_height: 0,
            slot: Slot::default(),
            switcher: Switcher::new(config.clock_rate),
            switching_started_at: None,
            keyframe_retries: 0,
        }
    }

    pub fn request_keyframe(&self) {
        if let Some(active) = &self.slot.active {
            active.request_keyframe();
        }
    }

    pub fn is_switchable(&mut self, receiver: &SimulcastReceiver) -> bool {
        if let Some(target) = self.slot.target() {
            if target.rid == receiver.rid && target.meta.id == receiver.meta.id {
                if !self.slot.state.is_paused_or_idle() {
                    return false;
                }
            } else if target.meta.id == receiver.meta.id
                && receiver.quality >= target.quality
                && self.slot.state.is_switching()
            {
                return false;
            }
        }

        self.switcher.ready_to_switch()
    }

    pub fn switch_to(&mut self, receiver: SimulcastReceiver, force: bool) {
        if !force && !self.is_switchable(&receiver) {
            return;
        }
        self.keyframe_retries = 0;
        self.do_switch_to(receiver, false);
    }

    pub fn assign_to(&mut self, receiver: SimulcastReceiver) {
        self.keyframe_retries = 0;
        self.transition_to(SlotState::Paused, None, Some(receiver));
    }

    pub fn stop(&mut self) {
        self.keyframe_retries = 0;
        self.transition_to(SlotState::Idle, None, None);
    }

    /// Pause at a specific receiver layer chosen by the allocation engine.
    ///
    /// Unlike the internal `pause()` which preserves whatever was current,
    /// `pause_at` accepts the engine's preferred resume target explicitly.
    /// This means when bandwidth recovers the driver jumps directly back to
    /// the right layer without re-running layer discovery.
    pub fn pause_at(&mut self, receiver: SimulcastReceiver) {
        self.keyframe_retries = 0;
        self.transition_to(SlotState::Paused, None, Some(receiver));
    }

    /// Pause the slot, preserving the current receiver as the resume target.
    /// Used internally when the driver itself detects a need to pause (e.g.
    /// stream closed) rather than when the allocator makes the decision.
    pub fn pause(&mut self) {
        self.keyframe_retries = 0;
        if let Some(receiver) = self.slot.current().cloned() {
            self.transition_to(SlotState::Paused, None, Some(receiver));
        } else {
            self.transition_to(SlotState::Idle, None, None);
        }
    }

    pub fn poll_slow(&mut self, now: Instant) {
        const KEYFRAME_RETRY_DELAYS_MS: [u64; 4] = [500, 1000, 2000, 4000];

        let Some(started_at) = self.switching_started_at else {
            self.keyframe_retries = 0;
            return;
        };

        if self.keyframe_retries >= KEYFRAME_RETRY_DELAYS_MS.len() {
            return;
        }

        let cumulative: u64 = KEYFRAME_RETRY_DELAYS_MS
            .iter()
            .take(self.keyframe_retries + 1)
            .sum();
        if started_at + Duration::from_millis(cumulative) > now {
            return;
        }

        self.keyframe_retries += 1;
        if let Some(target) = self.slot.target() {
            tracing::trace!(
                mid = %self.mid,
                rid = ?target.rid,
                "Switch slow. Retrying keyframe request (attempt {}/{}). Elapsed: {:?}",
                self.keyframe_retries,
                KEYFRAME_RETRY_DELAYS_MS.len(),
                now.duration_since(started_at)
            );
            target.request_keyframe();
        }
    }

    /// Try to produce the next outbound packet without suspending.
    ///
    /// Hot path: calls `spmc::Receiver::try_recv` first — a bare ring read with
    /// zero coop/listener/mutex overhead.  `poll_recv` (which registers an
    /// event-listener) is called only once when the ring is genuinely empty, so
    /// the waker cost is paid once per "dry spell", not once per packet.
    pub fn poll_packet(&mut self, cx: &mut Context<'_>) -> Poll<(Mid, RtpPacket)> {
        loop {
            // Drain the switcher output first — always cheap.
            if let Some(pkt) = self.switcher.pop() {
                return Poll::Ready((self.mid, pkt));
            }

            match (
                self.slot.state,
                self.slot.active.as_mut(),
                self.slot.staging.as_mut(),
            ) {
                (SlotState::Idle, _, _) => {
                    return Poll::Pending;
                }

                (SlotState::Paused, _, _) => {
                    return Poll::Pending;
                }

                (SlotState::Resuming, _, Some(staging)) => {
                    match std::task::ready!(staging.channel.poll_recv(cx)) {
                        Ok(pkt) => {
                            self.switcher.stage(pkt);
                            if self.switcher.ready_to_stream() {
                                tracing::info!(mid = %self.mid, rid = ?staging.rid, "Resuming complete");
                                let staging = staging.clone();
                                self.transition_to(SlotState::Streaming, Some(staging), None);
                            }
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, requesting keyframe");
                            staging.request_keyframe();
                            // Loop continues naturally, SPMC already auto-recovered to head.
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %self.mid, "Resuming closed");
                            self.transition_to(SlotState::Idle, None, None);
                            return Poll::Pending;
                        }
                    }
                }

                (SlotState::Streaming, Some(active), _) => {
                    match std::task::ready!(active.channel.poll_recv(cx)) {
                        Ok(pkt) => {
                            self.switcher.push(pkt);
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Streaming lagged, requesting keyframe");
                            // Do NOT pause. Request a keyframe to fix the decoder and keep streaming.
                            active.request_keyframe();
                            continue;
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::info!(mid = %self.mid, "Streaming closed");
                            self.transition_to(SlotState::Idle, None, None);
                            return Poll::Pending;
                        }
                    }
                }

                (SlotState::Switching, Some(active), Some(staging)) => {
                    // Try staging first (looking for keyframe to complete switch).
                    let res = staging.channel.poll_recv(cx);
                    match res {
                        Poll::Ready(ready) => match ready {
                            Ok(pkt) => {
                                self.switcher.stage(pkt);
                                if self.switcher.ready_to_stream() {
                                    tracing::info!(
                                        mid = %self.mid,
                                        from = ?active.rid,
                                        to = ?staging.rid,
                                        "Switch complete"
                                    );
                                    let staging = staging.clone();
                                    self.transition_to(SlotState::Streaming, Some(staging), None);
                                }
                                continue;
                            }
                            Err(spmc::RecvError::Lagged(n)) => {
                                tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged, requesting keyframe");
                                staging.request_keyframe();
                                // Do NOT pause. Just wait for the new keyframe to arrive.
                                continue;
                            }
                            Err(spmc::RecvError::Closed) => {
                                tracing::warn!(mid = %self.mid, "Staging closed during switch");
                                let active = active.clone();
                                self.transition_to(SlotState::Streaming, Some(active), None);
                                continue;
                            }
                        },
                        Poll::Pending => {}
                    }

                    // Staging empty — also drain active to prevent ring overflow.
                    match std::task::ready!(active.channel.poll_recv(cx)) {
                        Ok(pkt) => {
                            self.switcher.push(pkt);
                            continue;
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                mid = %self.mid, skipped = n,
                                "Active lagged during switch, requesting keyframe"
                            );
                            // Do NOT overwrite the switch intent by pausing. Keep active stream alive.
                            active.request_keyframe();
                            continue;
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %self.mid, "Active closed, forcing resume");
                            let staging = staging.clone();
                            self.transition_to(SlotState::Resuming, None, Some(staging));
                            // Evaluate the new Resuming state immediately to prevent artificial latency
                            continue;
                        }
                    }
                }
                state => unreachable!("unexpected state transition: {:?}", state),
            }
        }
    }

    fn do_switch_to(&mut self, mut receiver: SimulcastReceiver, force: bool) {
        if !force && !self.is_switchable(&receiver) {
            return;
        }

        let old_state = self.slot.state;
        receiver.channel.rewind();
        receiver.request_keyframe();
        self.switching_started_at = Some(Instant::now());

        match old_state {
            SlotState::Idle | SlotState::Paused => {
                self.transition_to(SlotState::Resuming, None, Some(receiver))
            }
            SlotState::Streaming => {
                let active = self.slot.active.take();
                self.transition_to(SlotState::Switching, active, Some(receiver))
            }
            SlotState::Resuming => self.transition_to(SlotState::Resuming, None, Some(receiver)),
            SlotState::Switching => {
                let active = self
                    .slot
                    .active
                    .take()
                    .expect("active to be defined on switching");
                if active.rid == receiver.rid && active.meta.id == receiver.meta.id {
                    tracing::info!("Cancelling switch, reverting to active stream");
                    self.transition_to(SlotState::Streaming, Some(active), None);
                } else {
                    self.transition_to(SlotState::Switching, Some(active), Some(receiver));
                }
            }
        };
        self.switcher.clear();
    }

    fn transition_to(
        &mut self,
        new_state: SlotState,
        active: Option<SimulcastReceiver>,
        staging: Option<SimulcastReceiver>,
    ) {
        if let SlotState::Streaming = &new_state {
            self.switching_started_at = None;
        }

        let was_playing = self.slot.state.is_playing();
        let is_playing = new_state.is_playing();

        if was_playing && !is_playing {
            let current = self.slot.current();
            if let Some(receiver) = current {
                match &new_state {
                    SlotState::Idle => tracing::info!(stream_id = %receiver, "stream stopped"),
                    SlotState::Paused => {
                        tracing::info!(stream_id = %receiver, "stream paused (congestion or lag)");
                    }
                    _ => {}
                }
            }
        }

        // Validate state transition is legal.
        debug_assert!(
            matches!(
                (&self.slot.state, &new_state),
                // Any state may stop or be paused.
                (_, SlotState::Idle)
                    | (_, SlotState::Paused)
                    // Resuming: entered from idle, paused, or replaced while already
                    // resuming/switching (active closed).
                    | (
                        SlotState::Idle
                            | SlotState::Paused
                            | SlotState::Resuming
                            | SlotState::Switching,
                        SlotState::Resuming,
                    )
                    // Streaming: switch or resume completed.
                    | (SlotState::Resuming | SlotState::Switching, SlotState::Streaming)
                    // Switching: new target while streaming or updating target while
                    // already switching.
                    | (SlotState::Streaming | SlotState::Switching, SlotState::Switching)
            ),
            "invalid state transition: {} → {}",
            self.slot.state,
            new_state
        );

        // Validate data invariants for the incoming state.
        match &new_state {
            SlotState::Idle => {
                debug_assert!(
                    active.is_none(),
                    "Idle state must have no active receiver, got Some"
                );
                debug_assert!(
                    staging.is_none(),
                    "Idle state must have no staging receiver, got Some"
                );
            }
            SlotState::Paused => {
                debug_assert!(
                    active.is_none(),
                    "Paused state must have no active receiver, got Some"
                );
                debug_assert!(
                    staging.is_some(),
                    "Paused state must have a staging receiver, got None"
                );
            }
            SlotState::Resuming => {
                debug_assert!(
                    active.is_none(),
                    "Resuming state must have no active receiver, got Some"
                );
                debug_assert!(
                    staging.is_some(),
                    "Resuming state must have a staging receiver, got None"
                );
            }
            SlotState::Streaming => {
                debug_assert!(
                    active.is_some(),
                    "Streaming state must have an active receiver, got None"
                );
                debug_assert!(
                    staging.is_none(),
                    "Streaming state must have no staging receiver, got Some"
                );
            }
            SlotState::Switching => {
                debug_assert!(
                    active.is_some(),
                    "Switching state must have an active receiver, got None"
                );
                debug_assert!(
                    staging.is_some(),
                    "Switching state must have a staging receiver, got None"
                );
            }
        }

        self.slot.state = new_state;
        self.slot.active = active;
        self.slot.staging = staging;
    }
}

pub struct AllocationEngine;

#[derive(Clone, Debug)]
pub struct SlotView<'a> {
    pub mid: Mid,
    pub priority: u32, // maps to max_height
    pub track: &'a TrackReceiver,
    pub current_quality: SimulcastQuality,
}

impl<'a> std::fmt::Display for SlotView<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let layers = self
            .track
            .simulcast
            .iter()
            .map(|l| {
                let status = if l.state.is_inactive() {
                    "inv"
                } else if !l.state.is_healthy() {
                    "unh"
                } else {
                    "ok"
                };
                format!(
                    "{}:{}",
                    l.rid.map(|r| r.to_string()).unwrap_or_else(|| "?".into()),
                    status
                )
            })
            .collect::<Vec<_>>()
            .join("|");
        write!(f, "{}(P{}|{})", self.mid, self.priority, layers)
    }
}
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum AllocationDecision<'a> {
    Forward(&'a SimulcastReceiver),
    /// Bandwidth-congestion pause.  The carried receiver is the layer the engine
    /// wants to resume *to* when bandwidth recovers — typically the lowest
    /// healthy layer so that recovery starts immediately without renegotiation.
    Pause(&'a SimulcastReceiver),
    /// No track is associated with this slot; stop the driver entirely.
    Idle,
}

impl<'a> std::fmt::Display for AllocationDecision<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationDecision::Forward(l) => write!(f, "Forward({})", l),
            AllocationDecision::Pause(l) => write!(f, "Pause({})", l),
            AllocationDecision::Idle => write!(f, "Idle"),
        }
    }
}

impl AllocationEngine {
    /// Upgrade hysteresis: Require 30% surplus to absorb the Keyframe/PLI burst.
    const UPGRADE_FACTOR: f64 = 1.3;
    /// Downgrade hysteresis: Ignore 10% BWE noise; only drop if truly over budget.
    const DOWNGRADE_FACTOR: f64 = 0.9;
    /// Serializes upgrades to prevent simultaneous Keyframe Storms.
    const MAX_UPGRADES_PER_TICK: usize = 1;

    pub fn compute<'a>(
        available_bw: Bitrate,
        slots: Vec<SlotView<'a>>,
    ) -> (HashMap<Mid, AllocationDecision<'a>>, Bitrate) {
        // Assert pre-sorted by priority descending (highest first)
        debug_assert!(
            slots.windows(2).all(|w| w[0].priority >= w[1].priority),
            "Slots must be pre-sorted by priority descending"
        );

        let mut decisions: HashMap<Mid, AllocationDecision<'a>> = HashMap::new();
        let mut remaining_bps = available_bw.as_f64();

        // 1. Maintain or Downgrade
        for slot in &slots {
            let current = slot.track.by_quality(slot.current_quality);

            // Can we afford to keep doing what we're doing?
            let stay_layer = current.filter(|l| {
                l.state.is_healthy()
                    && (l.state.bitrate_bps() * Self::DOWNGRADE_FACTOR) <= remaining_bps
            });

            let final_layer = stay_layer.or_else(|| {
                slot.track
                    .lower_quality(slot.current_quality)
                    .filter(|l| l.state.is_healthy() && l.state.bitrate_bps() <= remaining_bps)
            });

            if let Some(layer) = final_layer {
                remaining_bps -= layer.state.bitrate_bps();
                decisions.insert(slot.mid, AllocationDecision::Forward(layer));
            } else {
                // Paused: We carry the lowest quality as the "Target" for the driver to watch.
                decisions.insert(
                    slot.mid,
                    AllocationDecision::Pause(slot.track.lowest_quality()),
                );
            }
        }

        // 2. Upgrade
        let mut upgrades_performed = 0;

        // Try to upgrade levels (Low->Med, Med->High)
        for tier in [
            SimulcastQuality::Low,
            SimulcastQuality::Medium,
            SimulcastQuality::High,
        ] {
            if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                break;
            }

            for slot in &slots {
                if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                    break;
                }

                let decision = decisions.get(&slot.mid).copied();
                let Some(AllocationDecision::Forward(current)) = decision else {
                    continue;
                };

                if current.quality >= tier {
                    continue;
                }

                let Some(target) = slot.track.by_quality(tier) else {
                    continue;
                };

                if !target.state.is_healthy() {
                    continue;
                }

                let incremental_cost = target.state.bitrate_bps() - current.state.bitrate_bps();

                // Must have the upgrade headroom of the incremental jump
                if remaining_bps >= (incremental_cost * Self::UPGRADE_FACTOR) {
                    remaining_bps -= incremental_cost;
                    decisions.insert(slot.mid, AllocationDecision::Forward(target));
                    upgrades_performed += 1;
                }
            }
        }

        let total_desired_bps: f64 = slots
            .iter()
            .map(|slot| {
                // Find the highest layer that is currently marked healthy.
                // If none are healthy, we count it as 0.
                slot.track
                    .simulcast
                    .iter()
                    .filter(|l| l.state.is_healthy())
                    .map(|l| l.state.bitrate_bps())
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0)
            })
            .sum();

        (decisions, Bitrate::from(total_desired_bps))
    }
}

#[cfg(test)]
mod assignment_tests {
    use super::*;
    use crate::entity::{ParticipantId, TrackId};
    use crate::track::{TrackSender, test_utils::make_video_track};
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    struct TestTracks {
        pub senders: Vec<TrackSender>,
        pub ids: Vec<TrackId>,
    }

    fn setup_allocator() -> VideoAllocator {
        VideoAllocator::new(false)
    }

    fn add_tracks(allocator: &mut VideoAllocator, count: usize) -> TestTracks {
        let pid = ParticipantId::new();

        let mut senders = Vec::new();
        let mut ids = Vec::new();

        for i in 0..count {
            let mid = Mid::from(&format!("v{i}")[..]);
            let (tx, rx) = make_video_track(pid, mid);
            ids.push(rx.meta.id);
            allocator.add_track(rx);
            senders.push(tx);
        }

        TestTracks { senders, ids }
    }

    fn add_slots(allocator: &mut VideoAllocator, count: usize) {
        for i in 0..count {
            let mid = Mid::from(&format!("s{i}")[..]);
            allocator.add_slot(mid, SlotConfig::default());
        }
    }

    #[test]
    fn rebalance_assigns_tracks_to_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn more_tracks_than_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 5);
        add_slots(&mut allocator, 2);
        assert_eq!(allocator.slots().count(), 2);
    }

    #[test]
    fn tracks_before_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 2);
        add_slots(&mut allocator, 2);
        assert_eq!(allocator.slots().count(), 2);
    }

    #[test]
    fn removing_track_releases_slot() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);
        assert_eq!(allocator.slots().count(), 1);
        allocator.remove_track(&tracks.ids[0]);
        assert_eq!(allocator.slots().count(), 0);
    }

    #[test]
    fn multiple_slot_candidates_exist() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn allocator_returns_positive_desired_bitrate() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);
        let desired = allocator.update_allocations(Bitrate::from(5_000_000));
        assert!(desired.as_f64() > 0.0);
    }

    #[test]
    fn allocator_handles_track_churn() {
        let mut allocator = setup_allocator();
        let mut tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        allocator.remove_track(&tracks.ids[1]);
        let pid = ParticipantId::new();
        let (tx, rx) = make_video_track(pid, Mid::from("new_track"));
        tracks.senders.push(tx);
        allocator.add_track(rx);
        assert_eq!(allocator.slots().count(), 3);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    use crate::entity::ParticipantId;
    use crate::rtp::monitor::StreamQuality;
    use crate::track::{SimulcastQuality, TrackReceiver, test_utils::make_video_track};
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    fn setup_test_context() -> (ParticipantId, TrackReceiver) {
        let p_id = ParticipantId::new();
        let (_, rx) = make_video_track(p_id, Mid::from("100"));
        for layer in &rx.simulcast {
            layer.state.update_for_test().inactive(false);
        }
        (p_id, rx)
    }

    fn make_slot<'a>(
        mid: Mid,
        priority: u32,
        track: &'a TrackReceiver,
        current_quality: Option<SimulcastQuality>,
    ) -> SlotView<'a> {
        SlotView {
            mid,
            priority,
            track: Some(track),
            current_quality,
        }
    }

    #[test]
    fn test_fairness_over_greedy_priority() {
        let (_, track) = setup_test_context();
        let mid_a = Mid::from("1");
        let mid_b = Mid::from("2");

        let available = Bitrate::from(80_000);
        let slots = vec![
            make_slot(mid_a, 1080, &track, None),
            make_slot(mid_b, 360, &track, None),
        ];

        let (decisions, _) = AllocationEngine::compute(available, slots);

        assert_eq!(
            decisions.get(&mid_a).unwrap(),
            &AllocationDecision::Forward(track.by_quality(SimulcastQuality::Low).unwrap())
        );
        assert_eq!(
            decisions.get(&mid_b).unwrap(),
            &AllocationDecision::Forward(track.by_quality(SimulcastQuality::Low).unwrap())
        );
    }

    #[test]
    fn test_health_check_degradation() {
        let (_, track) = setup_test_context();
        let mid = Mid::from("1");

        track
            .by_quality(SimulcastQuality::High)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);

        let available = Bitrate::from(1_000_000);
        let slots = vec![make_slot(mid, 1080, &track, None)];

        let (decisions, _) = AllocationEngine::compute(available, slots);

        assert_eq!(
            decisions.get(&mid).unwrap(),
            &AllocationDecision::Forward(track.by_quality(SimulcastQuality::Medium).unwrap())
        );
    }

    #[test]
    fn test_starvation_sacrifice_order() {
        let (_, track) = setup_test_context();
        let mid_high = Mid::from("high");
        let mid_low = Mid::from("low");

        // Only enough for one Low-layer stream (~30 kbps) plus hysteresis margin.
        let available = Bitrate::from(40_000);
        let slots = vec![
            make_slot(mid_high, 1080, &track, None),
            make_slot(mid_low, 360, &track, None),
        ];

        let (decisions, _) = AllocationEngine::compute(available, slots);

        assert!(matches!(
            decisions.get(&mid_high),
            Some(AllocationDecision::Forward(_))
        ));
        // Pause must carry a resume-target receiver, not be a unit variant.
        assert!(matches!(
            decisions.get(&mid_low),
            Some(AllocationDecision::Pause(_))
        ));
    }

    #[test]
    fn test_idle_decision_for_empty_slot() {
        let mid = Mid::from("empty");
        let slots = vec![SlotView {
            mid,
            priority: 720,
            track: None,
            current_quality: None,
        }];

        let (decisions, _) = AllocationEngine::compute(Bitrate::from(1_000_000), slots);

        assert_eq!(decisions.get(&mid).unwrap(), &AllocationDecision::Idle);
    }

    #[test]
    fn test_pause_carries_resume_receiver() {
        let (_, track) = setup_test_context();
        let mid_high = Mid::from("h");
        let mid_low = Mid::from("l");

        let available = Bitrate::from(40_000);
        let slots = vec![
            make_slot(mid_high, 1080, &track, None),
            make_slot(mid_low, 360, &track, None),
        ];

        let (decisions, _) = AllocationEngine::compute(available, slots);

        match decisions.get(&mid_low).unwrap() {
            AllocationDecision::Pause(receiver) => {
                assert_eq!(receiver.quality, SimulcastQuality::Low);
            }
            other => panic!("expected Pause(_), got {:?}", other),
        }
    }

    #[test]
    fn test_no_oscillation_at_boundary() {
        // Place available bandwidth just 10 % above the Low layer cost.
        // The UPGRADE_HEADROOM guard (15 %) means no upgrade should fire,
        // and the SACRIFICE_HYSTERESIS (50 kbps) means no sacrifice should
        // fire either.  The decision must stay stable across many passes.
        let (_, track) = setup_test_context();
        let mid = Mid::from("m");

        let low_bps = track
            .by_quality(SimulcastQuality::Low)
            .unwrap()
            .state
            .bitrate_bps();
        // 10 % headroom — below the 15 % UPGRADE_HEADROOM guard band.
        let available = Bitrate::from((low_bps * 1.10) as u64);

        let slots = vec![make_slot(mid, 1080, &track, Some(SimulcastQuality::Low))];

        for _ in 0..20 {
            let (decisions, _) = AllocationEngine::compute(available, slots.clone());
            assert_eq!(
                decisions.get(&mid).unwrap(),
                &AllocationDecision::Forward(track.by_quality(SimulcastQuality::Low).unwrap()),
                "oscillation detected: decision changed away from Low"
            );
        }
    }

    #[test]
    fn test_update_allocations_returns_padded_bitrate() {
        let mut allocator = VideoAllocator::new(false);
        let (_, track) = setup_test_context();

        let mid = Mid::from("s1");
        allocator.add_slot(mid, SlotConfig::default());
        allocator.add_track(track.clone());

        let desired = allocator.update_allocations(Bitrate::from(1_000_000));

        // Desired = highest healthy layer of the one active slot.
        let expected = track
            .simulcast
            .iter()
            .filter(|l| l.state.is_healthy())
            .map(|l| l.state.bitrate_bps())
            .fold(0.0_f64, f64::max);
        assert_eq!(desired.as_f64(), expected);
    }

    #[test]
    fn test_switching_does_not_occur_if_unnecessary() {
        let mut allocator = VideoAllocator::new(false);
        let (_, track) = setup_test_context();
        let mid = Mid::from("s1");

        allocator.add_slot(mid, SlotConfig::default());
        allocator.add_track(track);

        allocator.update_allocations(Bitrate::from(1_000_000));

        let idx = allocator.mid_to_idx[&mid];
        assert!(allocator.slots.get(idx).unwrap().slot.state.is_playing());
    }

    #[test]
    fn test_pause_preserves_assignment_and_resumes() {
        let mut allocator = VideoAllocator::new(false);

        let (_, track1) = setup_test_context();
        let (_, track2) = setup_test_context();
        let (_, track3) = setup_test_context();

        allocator.add_slot(Mid::from("s1"), SlotConfig::default());
        allocator.add_slot(Mid::from("s2"), SlotConfig::default());
        allocator.add_slot(Mid::from("s3"), SlotConfig::default());

        allocator.add_track(track1.clone());
        allocator.add_track(track2.clone());
        allocator.add_track(track3.clone());

        allocator.update_allocations(Bitrate::from(5_000_000));
        assert_eq!(allocator.slots().count(), 3);
        assert_eq!(
            allocator
                .slots
                .iter()
                .filter(|(_, d)| d.slot.state.is_playing())
                .count(),
            3
        );

        // Starve the link — all slots should pause, not idle.
        allocator.update_allocations(Bitrate::from(0));

        assert_eq!(allocator.slots().count(), 3);
        for state in allocator.tracks.values() {
            assert!(state.assigned_mid.is_some(), "track lost assignment");
        }
        assert_eq!(
            allocator
                .slots
                .iter()
                .filter(|(_, d)| d.slot.state.is_playing())
                .count(),
            0
        );

        // Restoring bandwidth should resume all three streams.
        allocator.update_allocations(Bitrate::from(5_000_000));
        assert_eq!(
            allocator
                .slots
                .iter()
                .filter(|(_, d)| d.slot.state.is_playing())
                .count(),
            3
        );
    }

    #[test]
    fn test_pause_on_empty_slot_clears_assignment() {
        let mut allocator = VideoAllocator::new(false);
        let (_, track) = setup_test_context();

        allocator.add_slot(Mid::from("s1"), SlotConfig::default());
        allocator.add_track(track.clone());

        allocator.update_allocations(Bitrate::from(1_000_000));
        let assigned_mid = allocator.tracks.values().next().unwrap().assigned_mid;
        assert!(assigned_mid.is_some());

        let mid = assigned_mid.unwrap();
        let idx = allocator.mid_to_idx[&mid];
        allocator.slots.get_mut(idx).unwrap().stop();
        assert!(allocator.slots.get(idx).unwrap().slot.state.is_idle());
    }
}
