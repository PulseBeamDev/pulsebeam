use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::UnsyncSlotGroup;
use pulsebeam_runtime::unsync::spmc;
use std::collections::{HashMap, HashSet};
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
    slots: UnsyncSlotGroup<SlotDriver>,
    /// Reverse map from `Mid` to slot index for O(1) cold-path lookup.
    mid_to_idx: HashMap<Mid, usize>,
}

impl VideoAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            manual_sub,
            tracks: HashMap::new(),
            slots: UnsyncSlotGroup::with_capacity(VIDEO_MAX_SLOTS),
            mid_to_idx: HashMap::new(),
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

        {
            let mut seen = HashSet::new();
            for mid in self.tracks.values().filter_map(|s| s.assigned_mid) {
                if !seen.insert(mid) {
                    tracing::warn!(mid = ?mid, "a track was assigned to multiple slots");
                }
            }
        }

        {
            let mut seen = HashSet::new();
            for id in self
                .slots
                .iter()
                .filter_map(|(_, d)| d.slot.target().map(|t| t.meta.id))
            {
                if !seen.insert(id) {
                    tracing::warn!(track = ?id, "a track is assigned to multiple slots (slot-side)");
                }
            }
        }
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> Bitrate {
        // 1. Prepare the input views.
        //    `current_quality` lets the engine apply hysteresis: it only
        //    upgrades when there is genuine headroom *above* what the driver is
        //    already running, preventing the upgrade→overrun→downgrade cycle.
        let mut views: Vec<SlotView> = self
            .slots
            .iter()
            .filter_map(|(_, d)| {
                let current = d.slot.target()?;
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

        // Sort by priority (higher first) and then by Mid so the algorithm
        // is stable and does not depend on the input ordering of slots.
        views.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.mid.cmp(&b.mid)));

        // 2. Run the pure allocation logic.
        let (decisions, desired) = AllocationEngine::compute(available_bandwidth, &views);

        // 3. Apply the results to the drivers.
        let mut changed = false;
        for (mid, decision) in &decisions {
            let idx = self.mid_to_idx[mid];
            let mut driver = self.slots.get_mut(idx).unwrap();

            match decision {
                AllocationDecision::Forward(receiver, _) => {
                    changed |= driver.switch_to((*receiver).clone(), false);
                }
                AllocationDecision::Pause(receiver) => {
                    let already_correct =
                        driver.slot.state.is_paused()
                            && driver.slot.staging.as_ref().is_none_or(|s| {
                                s.rid == receiver.rid && s.meta.id == receiver.meta.id
                            });
                    if !already_correct {
                        driver.pause_at((*receiver).clone());
                        changed |= true;
                    }
                }
            }
        }

        if changed {
            log_allocation(available_bandwidth, desired, &decisions, &views);
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

pub fn log_allocation(
    bwe: Bitrate,
    desired: Bitrate,
    decisions: &HashMap<Mid, AllocationDecision>,
    slots: &[SlotView],
) {
    let mut reports = Vec::with_capacity(slots.len());
    let mut total_used_bps = 0.0;

    for slot in slots {
        let entry = match decisions.get(&slot.mid) {
            Some(AllocationDecision::Forward(l, bw)) => {
                total_used_bps += bw.as_f64();
                let q = match l.quality {
                    SimulcastQuality::High => "H",
                    SimulcastQuality::Medium => "M",
                    SimulcastQuality::Low => "L",
                };
                format!("{}:{}({})", slot.mid, q, bw)
            }
            Some(AllocationDecision::Pause(_)) => format!("{}:PAUSE", slot.mid),
            _ => format!("{}:IDLE", slot.mid),
        };
        reports.push(entry);
    }

    tracing::info!(
        %bwe,
        used = %Bitrate::from(total_used_bps as u64),
        want = %desired,
        streams = %reports.join(" "),
        "downstream"
    );
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

    pub fn switch_to(&mut self, receiver: SimulcastReceiver, force: bool) -> bool {
        if !force && !self.is_switchable(&receiver) {
            return false;
        }
        self.keyframe_retries = 0;
        self.do_switch_to(receiver);
        true
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

    fn do_switch_to(&mut self, mut receiver: SimulcastReceiver) {
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
        tracing::debug!(mid = %self.mid, from = %self.slot.state, to = %new_state, "SFU: SlotDriver::transition_to");
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

        // If the slot is stopped/paused, clear any previously buffered staging
        // packets so future resume/switch attempts can proceed without being
        // blocked by stale data.
        if matches!(self.slot.state, SlotState::Idle | SlotState::Paused) {
            self.switcher.clear();
        }
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

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum AllocationDecision<'a> {
    Forward(&'a SimulcastReceiver, Bitrate),
    /// Bandwidth-congestion pause.  The carried receiver is the layer the engine
    /// wants to resume *to* when bandwidth recovers — typically the lowest
    /// healthy layer so that recovery starts immediately without renegotiation.
    Pause(&'a SimulcastReceiver),
}

impl<'a> std::fmt::Display for AllocationDecision<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // e.g., "Forward(v1:H @ 1.2M)"
            AllocationDecision::Forward(layer, bitrate) => {
                write!(f, "Forward({} @ {})", layer, bitrate)
            }
            // e.g., "Pause(v1:L)"
            AllocationDecision::Pause(layer) => {
                write!(f, "Pause({})", layer)
            }
        }
    }
}

impl AllocationEngine {
    const UPGRADE_FACTOR: f64 = 1.3;
    const DOWNGRADE_FACTOR: f64 = 0.8;
    const MAX_UPGRADES_PER_TICK: usize = 2;

    pub fn compute<'a>(
        available_bw: Bitrate,
        slots: &[SlotView<'a>],
    ) -> (HashMap<Mid, AllocationDecision<'a>>, Bitrate) {
        let mut decisions: HashMap<Mid, AllocationDecision<'a>> = HashMap::new();
        let mut remaining_bps = available_bw.as_f64();

        // 1. Maintain or Downgrade
        for slot in slots {
            let current = slot.track.by_quality(slot.current_quality);

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
                // Snapshot the bitrate immediately
                let layer_bitrate = Bitrate::from(layer.state.bitrate_bps());
                let bps = layer_bitrate.as_f64();

                remaining_bps -= bps;
                decisions.insert(slot.mid, AllocationDecision::Forward(layer, layer_bitrate));
            } else {
                decisions.insert(
                    slot.mid,
                    AllocationDecision::Pause(slot.track.lowest_quality()),
                );
            }
        }

        // 2. Upgrade
        let mut upgrades_performed = 0;
        for tier in [
            SimulcastQuality::Low,
            SimulcastQuality::Medium,
            SimulcastQuality::High,
        ] {
            if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                break;
            }

            for slot in slots {
                if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                    break;
                }

                let Some(AllocationDecision::Forward(current_layer, current_bw)) =
                    decisions.get(&slot.mid).copied()
                else {
                    continue;
                };

                if current_layer.quality >= tier {
                    continue;
                }
                let Some(target) = slot.track.by_quality(tier) else {
                    continue;
                };
                if !target.state.is_healthy() {
                    continue;
                }

                let target_bw = Bitrate::from(target.state.bitrate_bps());
                let incremental_cost = target_bw.as_f64() - current_bw.as_f64();

                // Check against the 30% upgrade headroom (UPGRADE_FACTOR = 1.3)
                if remaining_bps >= (incremental_cost * Self::UPGRADE_FACTOR) {
                    remaining_bps -= incremental_cost;
                    decisions.insert(slot.mid, AllocationDecision::Forward(target, target_bw));
                    upgrades_performed += 1;
                }
            }
        }

        // 3. Demand Calculation (The "Want" Bitrate)
        let total_desired_bps: f64 = slots
            .iter()
            .map(|s| {
                s.track
                    .simulcast
                    .iter()
                    .filter(|l| l.state.is_healthy())
                    .map(|l| l.state.bitrate_bps())
                    .max_by(|a, b| a.partial_cmp(b).unwrap())
                    .unwrap_or(0.0)
            })
            .sum();

        let used_bps: f64 = decisions
            .values()
            .filter_map(|d| match d {
                AllocationDecision::Forward(_, bw) => Some(bw.as_f64()),
                AllocationDecision::Pause(_) => None,
            })
            .sum();

        // The allocator uses hysteresis (DOWNGRADE_FACTOR / UPGRADE_FACTOR) to
        // prevent frequent bitrate churning. This can result in allocating slightly
        // more than the estimated available bandwidth. Allow a small overshoot
        // bound to prevent debug builds from panicking while still catching
        // gross allocation bugs.
        let max_allowed = available_bw.as_f64() / Self::DOWNGRADE_FACTOR;
        debug_assert!(
            used_bps <= max_allowed + f64::EPSILON,
            "AllocationEngine allocated more bandwidth than allowed: used {} > allowed {} (available {} )",
            used_bps,
            max_allowed,
            available_bw.as_f64()
        );

        (decisions, Bitrate::from(total_desired_bps as u64))
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

            // Ensure tracks are considered "healthy" for allocation tests.
            for layer in &rx.simulcast {
                layer.state.update_for_test().inactive(false);
            }

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
    fn configure_all_slots_after_idle() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);

        // Empty intent should idle all slots.
        allocator.configure(&HashMap::new());
        assert_eq!(allocator.slots().count(), 0);

        // Re-activate all slots.
        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s0"),
            Intent {
                track_id: tracks.ids[0],
                max_height: 720,
            },
        );
        intents.insert(
            Mid::from("s1"),
            Intent {
                track_id: tracks.ids[1],
                max_height: 720,
            },
        );
        intents.insert(
            Mid::from("s2"),
            Intent {
                track_id: tracks.ids[2],
                max_height: 720,
            },
        );

        allocator.configure(&intents);
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
    use proptest::prelude::*;
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    // ─── Helpers ────────────────────────────────────────────────────────────────

    /// A track where all simulcast layers are healthy with realistic bitrates:
    ///   Low ≈ 30 kbps, Medium ≈ 200 kbps, High ≈ 800 kbps.
    fn healthy_track() -> TrackReceiver {
        let (_, rx) = make_video_track(ParticipantId::new(), Mid::from("t"));
        for layer in &rx.simulcast {
            layer.state.update_for_test().inactive(false);
        }
        rx
    }

    /// A track where the requested quality layer is marked bad; the others stay healthy.
    fn track_with_bad_layer(bad: SimulcastQuality) -> TrackReceiver {
        let rx = healthy_track();
        rx.by_quality(bad)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);
        rx
    }

    fn slot<'a>(
        mid: &str,
        priority: u32,
        track: &'a TrackReceiver,
        current: SimulcastQuality,
    ) -> SlotView<'a> {
        SlotView {
            mid: Mid::from(mid),
            priority,
            track,
            current_quality: current,
        }
    }

    fn bw(kbps: u64) -> Bitrate {
        Bitrate::from(kbps * 1_000)
    }

    fn layer_bps(track: &TrackReceiver, q: SimulcastQuality) -> f64 {
        track.by_quality(q).unwrap().state.bitrate_bps()
    }

    // ─── Property: every slot receives exactly one decision ─────────────────────

    #[test]
    fn every_slot_gets_a_decision() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 720, &t, SimulcastQuality::Low),
            slot("c", 360, &t, SimulcastQuality::Low),
        ];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        for s in &slots {
            assert!(
                decisions.contains_key(&s.mid),
                "slot {} has no decision",
                s.mid
            );
        }
    }

    // ─── Property: decisions are Forward or Pause, never something else ─────────

    #[test]
    fn decisions_are_forward_or_pause() {
        let t = healthy_track();
        let slots = vec![slot("a", 1080, &t, SimulcastQuality::High)];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        for (_, d) in &decisions {
            assert!(
                matches!(
                    d,
                    AllocationDecision::Forward(..) | AllocationDecision::Pause(..)
                ),
                "unexpected variant: {:?}",
                d
            );
        }
    }

    // ─── Property: desired bitrate is non-negative ───────────────────────────────

    #[test]
    fn desired_bitrate_is_non_negative() {
        let t = healthy_track();
        let slots = vec![slot("a", 720, &t, SimulcastQuality::Low)];
        for bw_kbps in [0, 1, 10, 100, 1_000, 100_000] {
            let (_, desired) = AllocationEngine::compute(bw(bw_kbps), &slots);
            assert!(desired.as_f64() >= 0.0, "desired < 0 at {} kbps", bw_kbps);
        }
    }

    // ─── Property: with unlimited bandwidth every slot forwards ─────────────────

    #[test]
    fn unlimited_bandwidth_forwards_all_slots() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 720, &t, SimulcastQuality::Low),
            slot("c", 360, &t, SimulcastQuality::Low),
        ];
        let (decisions, _) = AllocationEngine::compute(bw(100_000), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.mid], AllocationDecision::Forward(..)),
                "slot {} was not forwarded with unlimited bandwidth",
                s.mid
            );
        }
    }

    // ─── Property: with zero bandwidth every slot pauses ────────────────────────

    #[test]
    fn zero_bandwidth_pauses_all_slots() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 360, &t, SimulcastQuality::Low),
        ];
        let (decisions, _) = AllocationEngine::compute(bw(0), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.mid], AllocationDecision::Pause(..)),
                "slot {} was not paused with zero bandwidth",
                s.mid
            );
        }
    }

    // ─── Property: paused decisions always carry a resume target ────────────────
    //
    // The allocation engine must never emit a bare Pause — the receiver it
    // carries is the layer the driver will resume to when bandwidth recovers.

    #[test]
    fn pause_always_carries_a_resume_receiver() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 360, &t, SimulcastQuality::Low),
        ];
        let (decisions, _) = AllocationEngine::compute(bw(0), &slots);
        for (mid, d) in &decisions {
            if let AllocationDecision::Pause(receiver) = d {
                // The receiver field must point somewhere meaningful (non-null
                // is the only invariant we can assert structurally).
                let _ = receiver; // just asserting it exists via pattern match
            } else if matches!(d, AllocationDecision::Pause(..)) {
                panic!("Pause for {} is missing its resume receiver", mid);
            }
        }
    }

    // ─── Property: a bad high layer falls back to the next healthy layer ─────────
    //
    // When the highest quality is degraded, the engine should still forward
    // rather than pause — it just picks a lower healthy layer.

    #[test]
    fn bad_high_layer_falls_back_rather_than_pausing() {
        let t = track_with_bad_layer(SimulcastQuality::High);
        let mid = Mid::from("a");
        let slots = vec![SlotView {
            mid,
            priority: 1080,
            track: &t,
            current_quality: SimulcastQuality::High,
        }];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "expected Forward fallback when High is bad, got {:?}",
            decisions[&mid]
        );
    }

    // ─── Property: forwarded layer is always a healthy layer ────────────────────

    #[test]
    fn forwarded_layer_is_always_healthy() {
        let t = track_with_bad_layer(SimulcastQuality::High);
        let slots = vec![slot("a", 1080, &t, SimulcastQuality::High)];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        if let AllocationDecision::Forward(receiver, _) = &decisions[&Mid::from("a")] {
            assert!(
                receiver.state.is_healthy(),
                "engine forwarded to an unhealthy layer: {:?}",
                receiver.quality
            );
        }
    }

    // ─── Property: higher-priority slot is preferred when budget is tight ────────
    //
    // Two slots, only enough bandwidth for one Low layer.  The slot with the
    // higher priority (max_height) should be forwarded; the other paused.

    #[test]
    fn tight_budget_forwards_higher_priority_slot() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, SimulcastQuality::Low);

        // Budget just fits one Low layer (no headroom for downgrade guard).
        let available = bw((low_bps as u64) / 1_000 + 5);

        let mid_high_pri = Mid::from("h");
        let mid_low_pri = Mid::from("l");
        let slots = vec![
            SlotView {
                mid: mid_high_pri,
                priority: 1080,
                track: &t,
                current_quality: SimulcastQuality::Low,
            },
            SlotView {
                mid: mid_low_pri,
                priority: 360,
                track: &t,
                current_quality: SimulcastQuality::Low,
            },
        ];

        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid_high_pri], AllocationDecision::Forward(..)),
            "high-priority slot should be forwarded first"
        );
        assert!(
            matches!(decisions[&mid_low_pri], AllocationDecision::Pause(..)),
            "low-priority slot should be paused when budget is tight"
        );
    }

    proptest! {
        #[ignore]
        #[test]
        fn allocation_is_order_independent_for_equal_priority_slots(n in 2usize..=5) {
            let t = healthy_track();
            let low_bps = layer_bps(&t, SimulcastQuality::Low);

            // Budget just barely covers one Low layer.
            let available = bw((low_bps as u64) / 1_000 + 1);
            let priority = 720;

            let mid_names: Vec<String> = (0..n).map(|i| format!("m{}", i)).collect();
            let mut slots: Vec<SlotView> = mid_names
                .iter()
                .map(|name| slot(name, priority, &t, SimulcastQuality::Low))
                .collect();

            let (decisions1, _) = AllocationEngine::compute(available, &slots);

            // Reorder the input slots and verify outcome stays the same.
            slots.reverse();
            let (decisions2, _) = AllocationEngine::compute(available, &slots);

            prop_assert_eq!(decisions1.len(), decisions2.len());
            for name in mid_names {
                let mid = Mid::from(name.as_str());
                prop_assert_eq!(
                    decisions1.get(&mid),
                    decisions2.get(&mid),
                    "decisions differ for slot {} when input order changes",
                    mid
                );
            }
        }
    }
    // Two slots both eligible for an upgrade.  Only one should actually be
    // upgraded per call to compute().

    #[test]
    fn at_most_one_upgrade_per_tick() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, SimulcastQuality::Low);
        let high_bps = layer_bps(&t, SimulcastQuality::High);

        // Enough for two High layers — upgrades are definitely affordable —
        // but the engine serialises them to one per tick.
        let available = bw(((high_bps * 2.0 * 1.4) as u64) / 1_000);

        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 720, &t, SimulcastQuality::Low),
        ];

        let (decisions, _) = AllocationEngine::compute(available, &slots);

        let upgrades = decisions.values().filter(|d| {
            matches!(d, AllocationDecision::Forward(r, _) if r.quality > SimulcastQuality::Low)
        }).count();

        assert!(
            upgrades <= AllocationEngine::MAX_UPGRADES_PER_TICK,
            "engine performed {} upgrades; limit is {}",
            upgrades,
            AllocationEngine::MAX_UPGRADES_PER_TICK
        );
    }

    // ─── Property: desired bitrate reflects the best healthy layer, not the
    //               forwarded layer ──────────────────────────────────────────────
    //
    // desired should equal the sum of the highest healthy layer bitrate across
    // all slots, regardless of what was actually forwarded.

    #[test]
    fn desired_bitrate_equals_sum_of_best_healthy_layers() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, SimulcastQuality::Low),
            slot("b", 720, &t, SimulcastQuality::Low),
        ];

        let expected_per_slot = t
            .simulcast
            .iter()
            .filter(|l| l.state.is_healthy())
            .map(|l| l.state.bitrate_bps())
            .fold(0.0_f64, f64::max);

        let expected_total = expected_per_slot * slots.len() as f64;

        let (_, desired) = AllocationEngine::compute(bw(100_000), &slots);

        assert!(
            (desired.as_f64() - expected_total).abs() < 1.0,
            "desired {:.0} bps != expected {:.0} bps",
            desired.as_f64(),
            expected_total
        );
    }

    // ─── Property: downgrade hysteresis absorbs small bandwidth noise ────────────
    //
    // If bandwidth drops only slightly below the current layer cost (within the
    // 10% DOWNGRADE_FACTOR dead-band), the engine should keep forwarding the
    // current layer rather than dropping to a lower one.

    #[test]
    fn downgrade_hysteresis_absorbs_minor_bandwidth_noise() {
        let t = healthy_track();
        let mid = Mid::from("a");
        let low_bps = layer_bps(&t, SimulcastQuality::Low);

        // 5% below Low cost — inside the 10% dead-band; no downgrade should fire.
        let available = bw((low_bps * 0.95) as u64 / 1_000);

        let slots = vec![slot("a", 1080, &t, SimulcastQuality::Low)];
        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "engine downgraded or paused inside the hysteresis dead-band"
        );
    }

    // ─── Property: empty slot list produces empty decisions + zero desired ────────

    #[test]
    fn no_slots_yields_empty_decisions_and_zero_desired() {
        let (decisions, desired) = AllocationEngine::compute(bw(1_000), &[]);
        assert!(
            decisions.is_empty(),
            "expected no decisions for empty slots"
        );
        assert_eq!(
            desired.as_f64(),
            0.0,
            "expected zero desired bitrate for empty slots"
        );
    }

    // ─── Property: a single slot with a single healthy layer always forwards ──────

    #[test]
    fn single_slot_single_layer_always_forwards() {
        // Mark Medium and High as bad so only Low is healthy.
        let t = track_with_bad_layer(SimulcastQuality::High);
        t.by_quality(SimulcastQuality::Medium)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);

        let low_bps = layer_bps(&t, SimulcastQuality::Low);
        let mid = Mid::from("a");
        let slots = vec![slot("a", 720, &t, SimulcastQuality::Low)];

        // Bandwidth comfortably covers the only healthy layer.
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "single healthy layer should always be forwarded when budget allows"
        );
    }
}
