use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::collections::SlotGroup;
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::fmt::Display;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{Frequency, KeyframeRequest, Mid};
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackMeta, TrackReceiver};

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
            if let Some(&idx) = self.mid_to_idx.get(&assigned_mid) {
                if let Some(mut driver) = self.slots.get_mut(idx) {
                    driver.stop();
                }
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

    fn update_desired_bitrate(&self) -> Bitrate {
        let mut desired = 0f64;
        for (i, _) in self.slots.iter().enumerate() {
            let Some(driver) = self.slots.get(i) else {
                continue;
            };
            let Some(target) = driver.slot.target() else {
                continue;
            };

            let Some(meta) = self.tracks.get(&target.meta.id) else {
                continue;
            };

            let bitrate = meta.track.highest_quality().state.bitrate_bps();
            desired += bitrate;
        }

        Bitrate::from(desired)
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> Bitrate {
        const DOWNGRADE_TOLERANCE: f64 = 0.25;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.15;
        const MIN_BANDWIDTH: f64 = 30_000.0;

        struct SlotPlan<'a> {
            driver_idx: usize,
            current_receiver: &'a SimulcastReceiver,
            target_receiver: &'a SimulcastReceiver,
            current_bitrate: f64,
            desired_bitrate: f64,
            paused: bool,
            committed: bool,
        }

        struct CommittedSlotPlan {
            driver_idx: usize,
            receiver: SimulcastReceiver,
            paused: bool,
        }

        // Sort slot indices by max_height descending (greedy highest-priority first).
        let mut sorted_indices: Vec<usize> = self.slots.iter().map(|(i, _)| i).collect();
        sorted_indices.sort_by_key(|&i| std::cmp::Reverse(self.slots.get(i).unwrap().max_height));

        let mut budget = available_bandwidth.as_f64().max(MIN_BANDWIDTH);
        let mut slot_plans: Vec<SlotPlan> = Vec::with_capacity(sorted_indices.len());

        // Step 1: Optimistic Initialization
        for &idx in &sorted_indices {
            let driver = self.slots.get(idx).unwrap();
            let Some(current_receiver) = driver.slot.current() else {
                continue;
            };
            let Some(state) = self.tracks.get(&current_receiver.meta.id) else {
                continue;
            };

            let (target_receiver, cost) =
                if driver.slot.state.is_paused() || !current_receiver.state.is_healthy() {
                    let lowest = state.track.lowest_quality();
                    if lowest.state.is_inactive() {
                        slot_plans.push(SlotPlan {
                            driver_idx: idx,
                            current_receiver,
                            target_receiver: current_receiver,
                            current_bitrate: 0.0,
                            desired_bitrate: 0.0,
                            paused: true,
                            committed: true,
                        });
                        continue;
                    }
                    (lowest, lowest.state.bitrate_bps())
                } else {
                    let cost = current_receiver.state.bitrate_bps();
                    (current_receiver, cost)
                };

            budget -= cost;
            slot_plans.push(SlotPlan {
                driver_idx: idx,
                current_receiver,
                target_receiver,
                current_bitrate: cost,
                desired_bitrate: cost,
                paused: false,
                committed: false,
            });
        }

        // Step 2: Resolve Congestion (Downgrade Phase)
        let tolerance = available_bandwidth.as_f64() * DOWNGRADE_TOLERANCE;
        while budget < -tolerance {
            let mut resolved = false;
            for plan in slot_plans.iter_mut().rev() {
                if plan.paused || plan.committed {
                    continue;
                }
                let Some(state) = self.tracks.get(&plan.current_receiver.meta.id) else {
                    continue;
                };
                if let Some(lower) = state.track.lower_quality(plan.target_receiver.quality) {
                    let savings =
                        plan.target_receiver.state.bitrate_bps() - lower.state.bitrate_bps();
                    if savings > 0.0 {
                        plan.target_receiver = lower;
                        plan.current_bitrate = lower.state.bitrate_bps();
                        plan.desired_bitrate = lower.state.bitrate_bps();
                        budget += savings;
                        resolved = true;
                        if budget >= -tolerance {
                            break;
                        }
                    }
                } else {
                    let cost = plan.target_receiver.state.bitrate_bps();
                    plan.paused = true;
                    plan.committed = true;
                    plan.current_bitrate = 0.0;
                    budget += cost;
                    resolved = true;
                    if budget >= -tolerance {
                        break;
                    }
                }
            }
            if !resolved {
                break;
            }
        }

        // Step 3: Upgrade Phase
        let mut made_progress = true;
        while made_progress {
            made_progress = false;
            for plan in &mut slot_plans {
                if plan.committed {
                    continue;
                }
                plan.committed = true;
                let Some(state) = self.tracks.get(&plan.current_receiver.meta.id) else {
                    continue;
                };
                let Some(desired_receiver) = state
                    .track
                    .higher_quality(plan.target_receiver.quality)
                    .filter(|next| {
                        !next.state.is_inactive()
                            && next.state.quality() == StreamQuality::Excellent
                    })
                else {
                    continue;
                };

                let desired_bitrate = desired_receiver.state.bitrate_bps();
                let upgrade_cost = desired_bitrate - plan.current_bitrate;
                let hysteresis_overhead =
                    if desired_receiver.quality > plan.current_receiver.quality {
                        desired_bitrate * (UPGRADE_HYSTERESIS_FACTOR - 1.0)
                    } else {
                        0.0
                    };

                if (upgrade_cost + hysteresis_overhead) > budget {
                    plan.desired_bitrate = desired_bitrate * UPGRADE_HYSTERESIS_FACTOR;
                    continue;
                }

                budget -= upgrade_cost;
                plan.committed = false;
                plan.target_receiver = desired_receiver;
                plan.current_bitrate = desired_bitrate;
                plan.desired_bitrate = desired_bitrate;
                made_progress = true;
            }
        }

        // Step 4: Apply allocations
        let mut committed = Vec::with_capacity(slot_plans.len());
        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;

        for plan in slot_plans.drain(..) {
            total_allocated += plan.current_bitrate;
            total_desired += plan.desired_bitrate;
            committed.push(CommittedSlotPlan {
                driver_idx: plan.driver_idx,
                receiver: plan.target_receiver.clone(),
                paused: plan.paused,
            });
        }

        for plan in committed {
            let idx = plan.driver_idx;
            let mut driver = self.slots.get_mut(idx).unwrap();
            if plan.paused {
                driver.assign_to(plan.receiver);
            } else {
                driver.switch_to(plan.receiver, false);
            }
        }

        let total_allocated = Bitrate::from(total_allocated);
        let total_desired = Bitrate::from(total_desired);

        if self.ticks >= 30 {
            tracing::debug!(
                available = %available_bandwidth,
                allocated = %total_allocated,
                budget = %Bitrate::from(budget),
                desired = %total_desired,
                "allocation summary"
            );
            self.ticks = 0;
        }
        self.ticks += 1;

        total_desired
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

    /// Inline hot path — called directly from `DownstreamAllocator::poll_next`
    /// to avoid constructing a `tokio::select!` future per packet.
    #[inline]
    pub(super) fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> Poll<(Mid, RtpPacket)> {
        use futures_lite::stream::Stream as _;
        match Pin::new(&mut self.slots).poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(item),
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
