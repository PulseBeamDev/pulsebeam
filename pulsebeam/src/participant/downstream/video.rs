use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::bit_signal::BitSignal;
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid};
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackMeta, TrackReceiver};

pub struct VideoAllocator {
    manual_sub: bool,
    tracks: HashMap<TrackId, TrackState>,
    drivers: Vec<SlotDriver>,
    ticks: u32,
    last_polled: usize,
    shard_signal: Option<(Arc<BitSignal>, u64)>,
}

impl VideoAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            manual_sub,
            tracks: HashMap::new(),
            drivers: Vec::new(),
            ticks: 0,
            last_polled: 0,
            shard_signal: None,
        }
    }

    /// Register a shard's BitSignal on every current and future slot driver so
    /// the shard task is woken only when a slot can actually produce packets.
    pub fn attach_shard_signal(&mut self, signal: Arc<BitSignal>, bits: u64) {
        for driver in &mut self.drivers {
            driver.set_shard_signal(signal.clone(), bits);
        }
        self.shard_signal = Some((signal, bits));
    }

    /// Non-blocking round-robin drain across all slot drivers.
    /// Returns the first ready packet, advancing `last_polled` for fairness.
    pub fn try_next(&mut self) -> Option<(Mid, RtpPacket)> {
        let len = self.drivers.len();
        if len == 0 {
            return None;
        }
        for i in 0..len {
            let idx = (self.last_polled + i) % len;
            if let Some(item) = self.drivers[idx].try_packet() {
                self.last_polled = (idx + 1) % len;
                return Some(item);
            }
        }
        None
    }

    pub fn slot_count(&self) -> usize {
        self.drivers.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        let tracks = &mut self.tracks;
        // Collect mids to avoid borrow issues.
        let mids: Vec<Mid> = self.drivers.iter().map(|d| d.mid).collect();
        for mid in mids {
            let driver = self.drivers.iter_mut().find(|d| d.mid == mid).unwrap();
            if let Some(intent) = intents.get(&mid) {
                Self::configure_slot(tracks, driver, intent.max_height, Some(&intent.track_id));
            } else {
                Self::configure_slot(tracks, driver, 0, None);
            }
        }
    }

    fn configure_slot(
        tracks: &mut HashMap<TrackId, TrackState>,
        driver: &mut SlotDriver,
        max_height: u32,
        track_id: Option<&TrackId>,
    ) {
        // Clear any previous assignment on the target track.
        if let Some(target) = driver.slot.target()
            && let Some(state) = tracks.get_mut(&target.meta.id)
        {
            state.assigned_mid = None;
        }

        if let Some(track_id) = track_id
            && max_height > 0
        {
            let Some(track_state) = tracks.get_mut(track_id) else {
                return;
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
        } else {
            driver.stop();
        }

        driver.max_height = max_height;
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackMeta> {
        self.tracks.values().map(|s| &*s.track.meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> {
        self.drivers.iter().filter_map(|d| {
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
            if let Some(driver) = self.drivers.iter_mut().find(|d| d.mid == assigned_mid) {
                driver.stop();
            }
            self.rebalance();
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        let mut driver = SlotDriver::new(mid);
        if let Some((signal, bits)) = &self.shard_signal {
            driver.set_shard_signal(signal.clone(), *bits);
        }
        self.drivers.push(driver);
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

        let mut ui = 0;
        for driver in &mut self.drivers {
            if !driver.slot.state.is_idle() {
                continue;
            }
            let Some(track_id) = unassigned.get(ui).copied() else {
                break;
            };
            ui += 1;
            let state = self.tracks.get_mut(&track_id).unwrap();
            state.assigned_mid = Some(driver.mid);
            driver.assign_to(state.track.lowest_quality().clone());
        }
    }

    // based on Greedy Knapsack
    pub fn update_allocations(
        &mut self,
        available_bandwidth: Bitrate,
    ) -> Option<(Bitrate, Bitrate)> {
        const DOWNGRADE_TOLERANCE: f64 = 0.25;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.15;
        const MIN_BANDWIDTH: f64 = 300_000.0;

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

        if self.drivers.is_empty() {
            return None;
        }

        self.drivers
            .sort_by_key(|d| std::cmp::Reverse(d.max_height));
        let mut budget = available_bandwidth.as_f64().max(MIN_BANDWIDTH);
        let mut slot_plans: Vec<SlotPlan> = Vec::with_capacity(self.drivers.len());

        // Step 1: Optimistic Initialization
        for (idx, driver) in self.drivers.iter().enumerate() {
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
                    // No lower quality layer exists for this stream.
                    // Pausing would require a full keyframe round-trip to recover, and
                    // single-layer streams should rely on TWCC/BWE rather than SFU-side
                    // pausing to handle transient bitrate spikes (e.g. keyframe bursts).
                    // Commit the plan without pausing and accept the temporary budget overrun.
                    plan.committed = true;
                    resolved = true;
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
            let driver = &mut self.drivers[plan.driver_idx];
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

        Some((total_allocated, total_desired))
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) {
        let Some(driver) = self.drivers.iter().find(|d| d.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        driver.request_keyframe();
    }

    pub fn poll_slow(&mut self, now: Instant) {
        for driver in &mut self.drivers {
            driver.poll_slow(now);
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
    pub track: std::sync::Arc<TrackMeta>,
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
    shard_signal: Option<(Arc<BitSignal>, u64)>,
}

impl SlotDriver {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            max_height: 0,
            slot: Slot::default(),
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            switching_started_at: None,
            keyframe_retries: 0,
            shard_signal: None,
        }
    }

    /// Attach/replace the shard signal. Detaches from any previously-attached
    /// receivers before re-attaching to the currently active ones.
    pub fn set_shard_signal(&mut self, signal: Arc<BitSignal>, bits: u64) {
        // Detach old signal from current receivers.
        if let Some((old_signal, old_bits)) = &self.shard_signal {
            if let Some(r) = &self.slot.active {
                r.channel.detach_signal(old_signal, *old_bits);
            }
            if let Some(r) = &self.slot.staging {
                r.channel.detach_signal(old_signal, *old_bits);
            }
        }
        self.shard_signal = Some((signal, bits));
        // Attach to receivers that can produce packets in the current state.
        if let Some((signal, bits)) = &self.shard_signal {
            match self.slot.state {
                SlotState::Resuming => {
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                SlotState::Streaming => {
                    if let Some(r) = &self.slot.active {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                SlotState::Switching => {
                    if let Some(r) = &self.slot.active {
                        r.channel.attach_signal(signal, *bits);
                    }
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                // Paused: attach to staging so the shard wakes as soon as the
                // upstream stream's first packet lands in the ring.  The wake is
                // "spurious" in that try_packet returns None, but it lets us
                // call update_allocations immediately instead of waiting 200 ms
                // for poll_slow to notice the stream has become active.
                SlotState::Paused => {
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                // Idle: no receiver to attach.
                SlotState::Idle => {}
            }
        }
    }

    /// Non-blocking packet poll.
    pub fn try_packet(&mut self) -> Option<(Mid, RtpPacket)> {
        match self.poll_packet() {
            Poll::Ready(item) => Some(item),
            Poll::Pending => None,
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
    pub fn poll_packet(&mut self) -> Poll<(Mid, RtpPacket)> {
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
                    match std::task::ready!(staging.channel.poll_recv()) {
                        Ok(pkt) => {
                            self.switcher.stage(pkt);
                            if self.switcher.ready_to_stream() {
                                tracing::info!(mid = %self.mid, rid = ?staging.rid, "Resuming complete");
                                let staging = staging.clone();
                                self.transition_to(SlotState::Streaming, Some(staging), None);
                            }
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, keep going");
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %self.mid, "Resuming closed");
                            self.transition_to(SlotState::Idle, None, None);
                            return Poll::Pending;
                        }
                    }
                }

                (SlotState::Streaming, Some(active), _) => {
                    match std::task::ready!(active.channel.poll_recv()) {
                        Ok(pkt) => {
                            self.switcher.push(pkt);
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Streaming lagged, pausing");
                            let active = active.clone();
                            self.transition_to(SlotState::Paused, None, Some(active));
                            return Poll::Pending;
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
                    let res = staging.channel.poll_recv();
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
                                tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged, pausing");
                                let staging = staging.clone();
                                self.transition_to(SlotState::Paused, None, Some(staging));
                                return Poll::Pending;
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
                    match std::task::ready!(active.channel.poll_recv()) {
                        Ok(pkt) => {
                            self.switcher.push(pkt);
                            continue;
                        }
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                mid = %self.mid, skipped = n,
                                "Active lagged during switch, pausing"
                            );
                            let active = active.clone();
                            self.transition_to(SlotState::Paused, None, Some(active));
                            return Poll::Pending;
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %self.mid, "Active closed, forcing resume");
                            let staging = staging.clone();
                            self.transition_to(SlotState::Resuming, None, Some(staging));
                            return Poll::Pending;
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

        // Detach signal from receivers about to be replaced.
        if let Some((signal, bits)) = &self.shard_signal {
            if let Some(r) = &self.slot.active {
                r.channel.detach_signal(signal, *bits);
            }
            if let Some(r) = &self.slot.staging {
                r.channel.detach_signal(signal, *bits);
            }
        }

        self.slot.state = new_state;
        self.slot.active = active;
        self.slot.staging = staging;

        // Attach signal to new receivers only for states that produce packets.
        if let Some((signal, bits)) = &self.shard_signal {
            match new_state {
                SlotState::Resuming => {
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                SlotState::Streaming => {
                    if let Some(r) = &self.slot.active {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                SlotState::Switching => {
                    if let Some(r) = &self.slot.active {
                        r.channel.attach_signal(signal, *bits);
                    }
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                // Paused: attach to staging so the shard wakes as soon as the
                // upstream stream's first packet lands in the ring.  The wake is
                // "spurious" in that try_packet returns None, but it lets us
                // call update_allocations immediately instead of waiting 200 ms
                // for poll_slow to notice the stream has become active.
                SlotState::Paused => {
                    if let Some(r) = &self.slot.staging {
                        r.channel.attach_signal(signal, *bits);
                    }
                }
                // Idle: no receiver to attach.
                SlotState::Idle => {}
            }
        }
    }
}
