use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::task::Waker;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, Mid};
use tokio::time::Instant;

use crate::entity::{EntityId, TrackId};
use crate::track::{SimulcastReceiver, TrackMeta, TrackReceiver};

pub struct SlotAssignment {
    pub mid: Mid,
    pub track: Arc<TrackMeta>,
}

#[derive(Default)]
pub struct VideoAllocator {
    tracks: HashMap<TrackId, TrackState>,
    slots: Vec<Slot>,

    ticks: u32,
    rr_cursor: usize,
    waker: Option<Waker>,
}

impl VideoAllocator {
    pub fn configure_slot(&mut self, mid: Mid, track_id: EntityId, max_height: u32) {
        if !self.tracks.contains_key(&track_id) {
            tracing::warn!(%track_id, "ignoring slot configuration, track doesn't exist");
            return;
        }

        let Some(slot) = self.slots.iter_mut().find(|s| s.mid == mid) else {
            tracing::warn!("ignoring slot configuration, mid={} doesn't exist", mid);
            return;
        };

        if let Some(current) = slot.current_receiver()
            && let Some(current_state) = self.tracks.get_mut(&current.meta.id)
        {
            current_state.assigned_mid = None;
        }

        let Some(track_state) = self.tracks.get_mut(&track_id) else {
            // This shouldn't happen
            return;
        };
        slot.max_height = max_height;
        let layer = track_state.track.lowest_quality().clone();
        if slot.max_height == 0 {
            slot.assign_to(layer);
        } else {
            slot.switch_to(layer);
        }
        track_state.assigned_mid = Some(mid);
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackMeta> {
        self.tracks.values().map(|s| &*s.track.meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> {
        self.slots.iter().filter_map(|s| {
            Some(SlotAssignment {
                mid: s.mid,
                track: s.current_receiver()?.meta.clone(),
            })
        })
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }

        tracing::info!(
            track = %track.meta.id,
            "video track added"
        );
        self.tracks.insert(
            track.meta.id.clone(),
            TrackState {
                track,
                assigned_mid: None,
            },
        );
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) {
        if let Some(track) = self.tracks.remove(track_id) {
            tracing::info!(
                track = %track_id,
                "video track removed"
            );

            let Some(assigned_mid) = track.assigned_mid else {
                return;
            };

            let Some(slot) = self.slots.iter_mut().find(|s| s.mid == assigned_mid) else {
                return;
            };

            slot.stop();
            self.rebalance();
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        tracing::info!(%mid, "video slot added");
        self.slots.push(Slot::new(mid));
        self.rebalance();

        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn rebalance(&mut self) {
        let mut unassigned_tracks = self.tracks.iter_mut().filter(|(_, state)| {
            assert!(state.track.meta.kind.is_video());
            state.assigned_mid.is_none()
        });

        for slot in &mut self.slots {
            if !slot.is_idle() {
                continue;
            }

            let Some(next_track) = unassigned_tracks.next() else {
                break;
            };

            next_track.1.assigned_mid.replace(slot.mid);
            slot.assign_to(next_track.1.track.lowest_quality().clone());
        }
    }

    // based on Greedy Knapsack
    pub fn update_allocations(
        &mut self,
        available_bandwidth: Bitrate,
    ) -> Option<(Bitrate, Bitrate)> {
        const DOWNGRADE_TOLERANCE: f64 = 0.25;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.3;
        const MIN_BANDWIDTH: f64 = 300_000.0;

        struct SlotPlan<'a> {
            current_receiver: &'a SimulcastReceiver,
            target_receiver: &'a SimulcastReceiver,
            current_bitrate: f64,
            desired_bitrate: f64,
            paused: bool,
            committed: bool,
        }

        struct CommittedSlotPlan {
            receiver: SimulcastReceiver,
            paused: bool,
        }

        if self.slots.is_empty() {
            return None;
        }

        self.slots.sort_by_key(|s| std::cmp::Reverse(s.max_height));
        let mut budget = available_bandwidth.as_f64().max(MIN_BANDWIDTH);

        // Step 1: Optimistic Initialization
        let mut slot_plans: Vec<SlotPlan> = Vec::with_capacity(self.slots.len());

        for slot in &self.slots {
            let Some(current_receiver) = slot.current_receiver() else {
                continue;
            };

            let Some(state) = self.tracks.get(&current_receiver.meta.id) else {
                continue;
            };

            // If the slot is paused OR the layer we are currently watching has died (is_inactive),
            // we do NOT give up. Instead, we reset our target to the "Lowest" layer (`q`).
            // This forces the allocator to try and maintain at least the base layer.
            let (target_receiver, cost) =
                if slot.is_paused() || !current_receiver.state.is_healthy() {
                    let lowest = state.track.lowest_quality();

                    // If even the lowest layer is dead, THEN we truly pause.
                    // It's okay if the quality is not great. At least, we're trying to stream.
                    if lowest.state.is_inactive() {
                        slot_plans.push(SlotPlan {
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
                    // Current layer is healthy, try to keep it.
                    (current_receiver, current_receiver.state.bitrate_bps())
                };

            budget -= cost;

            slot_plans.push(SlotPlan {
                current_receiver,
                target_receiver,
                current_bitrate: cost,
                desired_bitrate: cost,
                paused: false, // Optimistically active
                committed: false,
            });
        }

        // Step 2: Resolve Congestion (Downgrade Phase)
        let tolerance = available_bandwidth.as_f64() * DOWNGRADE_TOLERANCE;

        while budget < -tolerance {
            let mut resolved_some_debt = false;

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
                        resolved_some_debt = true;

                        if budget >= -tolerance {
                            break;
                        }
                    }
                } else {
                    // Must pause
                    let cost = plan.target_receiver.state.bitrate_bps();
                    plan.paused = true;
                    plan.committed = true;
                    plan.current_bitrate = 0.0;
                    budget += cost;
                    resolved_some_debt = true;

                    if budget >= -tolerance {
                        break;
                    }
                }
            }

            if !resolved_some_debt {
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

                // Step 1 has created a baseline for healthy layers.
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

                let hysteresis = if desired_receiver.quality > plan.current_receiver.quality {
                    UPGRADE_HYSTERESIS_FACTOR
                } else {
                    1.0
                };

                let effective_cost = upgrade_cost * hysteresis;

                if effective_cost > budget {
                    plan.desired_bitrate = desired_bitrate * UPGRADE_HYSTERESIS_FACTOR;
                    continue;
                }

                budget -= upgrade_cost;
                plan.committed = false;
                plan.target_receiver = desired_receiver;
                plan.current_bitrate = desired_bitrate;
                plan.desired_bitrate = desired_bitrate * UPGRADE_HYSTERESIS_FACTOR;
                made_progress = true;
            }
        }

        // Step 4: Apply allocations
        let mut committed = Vec::with_capacity(self.slots.len());
        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;

        for plan in slot_plans.drain(..) {
            total_allocated += plan.current_bitrate;
            total_desired += plan.desired_bitrate;
            committed.push(CommittedSlotPlan {
                receiver: plan.target_receiver.clone(),
                paused: plan.paused,
            });
        }

        for (idx, plan) in committed.drain(..).enumerate() {
            let slot = &mut self.slots[idx];
            if plan.paused {
                slot.assign_to(plan.receiver);
            } else {
                slot.switch_to(plan.receiver);
            }
        }

        let total_allocated = Bitrate::from(total_allocated.max(MIN_BANDWIDTH));
        let total_desired = Bitrate::from(total_desired.max(MIN_BANDWIDTH));

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
        let Some(slot) = self.slots.iter().find(|e| e.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        slot.request_keyframe(req.kind);
    }

    pub fn poll_slow(&mut self, now: Instant) {
        for slot in &mut self.slots {
            slot.poll_slow(now);
        }
    }

    pub fn poll_fast(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
        if self.slots.is_empty() {
            self.waker.replace(cx.waker().clone());
            return Poll::Pending;
        }

        let mut checked_count = 0;
        while checked_count < self.slots.len() {
            if self.rr_cursor >= self.slots.len() {
                self.rr_cursor = 0;
            }

            let slot = &mut self.slots[self.rr_cursor];
            match slot.poll_fast(cx) {
                Poll::Ready(Some(pkt)) => {
                    // ensure each frame to be flushed before moving to another stream,
                    // allows related contexts to stay in L1/L2 caches longer.
                    return Poll::Ready(Some((slot.mid, pkt)));
                }
                Poll::Ready(None) => {
                    self.rr_cursor += 1;
                    checked_count += 1;
                }
                Poll::Pending => {
                    self.rr_cursor += 1;
                    checked_count += 1;
                }
            }
        }
        Poll::Pending
    }
}

#[derive(Debug)]
struct TrackState {
    track: TrackReceiver,
    assigned_mid: Option<Mid>,
}

enum SlotState {
    Idle,
    Paused {
        active: SimulcastReceiver,
    },
    Resuming {
        staging: SimulcastReceiver,
    },
    Streaming {
        active: SimulcastReceiver,
    },
    Switching {
        active: SimulcastReceiver,
        staging: SimulcastReceiver,
    },
}

impl Display for SlotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            Self::Idle => "idle",
            Self::Paused { .. } => "paused",
            Self::Resuming { .. } => "resuming",
            Self::Streaming { .. } => "streaming",
            Self::Switching { .. } => "switching",
        };

        f.write_str(state)
    }
}

struct Slot {
    mid: Mid,
    max_height: u32,
    switcher: Switcher,
    state: Option<SlotState>,
    switching_started_at: Option<Instant>,
    keyframe_retries: usize,
    waker: Option<Waker>,
}

impl Slot {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            max_height: 0,
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            state: Some(SlotState::Idle),
            switching_started_at: None,
            keyframe_retries: 0,
            waker: None,
        }
    }

    fn state(&self) -> &SlotState {
        self.state.as_ref().expect("State invalid outside poll")
    }

    fn current_receiver(&self) -> Option<&SimulcastReceiver> {
        match self.state() {
            SlotState::Resuming { staging } => Some(staging),
            SlotState::Switching { active, .. } => Some(active),
            SlotState::Streaming { active } | SlotState::Paused { active } => Some(active),
            SlotState::Idle => None,
        }
    }

    fn target_receiver(&self) -> Option<&SimulcastReceiver> {
        match self.state() {
            SlotState::Resuming { staging } | SlotState::Switching { staging, .. } => Some(staging),
            SlotState::Streaming { active } | SlotState::Paused { active } => Some(active),
            SlotState::Idle => None,
        }
    }

    // similar to switch_to but it will start as paused
    pub fn assign_to(&mut self, receiver: SimulcastReceiver) {
        self.transition_to(SlotState::Paused { active: receiver });
    }

    pub fn switch_to(&mut self, mut receiver: SimulcastReceiver) {
        if let Some(current) = self.target_receiver()
            && current.rid == receiver.rid
            && current.meta.id == receiver.meta.id
            && !self.is_paused()
        {
            return;
        }

        // there are buffered packets, we must flush them first before dropping packets
        if !self.switcher.ready_to_switch() {
            return;
        }

        // Take ownership of state to move internals
        let old_state = self.state.take().unwrap_or(SlotState::Idle);

        receiver.channel.reset();
        receiver.request_keyframe(KeyframeRequestKind::Fir);
        self.switching_started_at = Some(Instant::now());
        self.keyframe_retries = 0;

        let old_state_str = old_state.to_string();
        let track_id = receiver.meta.id.to_string();
        let rid = receiver.rid;

        let new_state = match old_state {
            SlotState::Idle | SlotState::Paused { .. } => SlotState::Resuming { staging: receiver },

            SlotState::Streaming { active } => SlotState::Switching {
                active,
                staging: receiver,
            },

            SlotState::Resuming { .. } => SlotState::Resuming { staging: receiver },

            SlotState::Switching {
                active,
                staging: _old_staging,
            } => {
                if active.rid == receiver.rid && active.meta.id == receiver.meta.id {
                    tracing::info!("Cancelling switch, reverting to active stream");
                    SlotState::Streaming { active }
                } else {
                    SlotState::Switching {
                        active,
                        staging: receiver,
                    }
                }
            }
        };
        tracing::info!(
            mid = %self.mid,
            to_rid = ?rid,
            track = track_id,
            old_state = old_state_str,
            new_state = %new_state,
            "switch_to: initiating switch"
        );

        self.transition_to(new_state);
        self.switcher.clear();

        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state(), SlotState::Idle | SlotState::Paused { .. })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state(), SlotState::Idle)
    }

    pub fn stop(&mut self) {
        self.transition_to(SlotState::Idle);
    }

    fn request_keyframe(&self, kind: KeyframeRequestKind) {
        match self.state() {
            SlotState::Resuming { staging } => {
                staging.request_keyframe(kind);
            }
            // We're not requesting staging stream since we're currently streaming active.
            SlotState::Switching { active, .. } => {
                active.request_keyframe(kind);
            }
            SlotState::Streaming { active } => {
                active.request_keyframe(kind);
            }
            SlotState::Paused { .. } | SlotState::Idle => {}
        }
    }

    fn poll_slow(&mut self, now: Instant) {
        const KEYFRAME_RETRY_DELAYS_MS: [u64; 4] = [500, 1000, 2000, 4000];

        // Chrome shares keyframe request throttling for all simulcast streams &
        // Our keyframe request might get lost in the middle. Retry some to at least
        // reduce the probability to miss the keyframe.
        let Some(started_at) = self.switching_started_at else {
            return;
        };

        if self.keyframe_retries >= KEYFRAME_RETRY_DELAYS_MS.len() {
            return;
        }

        let current_cumulative_delay: u64 = KEYFRAME_RETRY_DELAYS_MS
            .iter()
            .take(self.keyframe_retries + 1)
            .sum();
        let deadline = started_at + Duration::from_millis(current_cumulative_delay);
        if deadline > now {
            return;
        }

        self.keyframe_retries += 1;
        if let Some(receiver) = self.target_receiver() {
            tracing::warn!(
                receiver = %receiver,
                "Switch slow. Retrying keyframe request (attempt {}/{}). Elapsed: {:?}",
                self.keyframe_retries,
                KEYFRAME_RETRY_DELAYS_MS.len(),
                now.duration_since(started_at)
            );

            receiver.request_keyframe(KeyframeRequestKind::Fir);
        }
    }

    #[inline]
    fn transition_to(&mut self, new_state: SlotState) {
        match &new_state {
            SlotState::Streaming { .. } => {
                self.switching_started_at = None;
                self.keyframe_retries = 0;
            }
            _ => {}
        }
        self.state = Some(new_state);
    }

    fn poll_fast(&mut self, cx: &mut Context<'_>) -> Poll<Option<RtpPacket>> {
        loop {
            if let Some(pkt) = self.switcher.pop() {
                return Poll::Ready(Some(pkt));
            }

            // Take the state to perform transitions by move
            let state = self.state.take().unwrap();

            match state {
                SlotState::Idle => {
                    self.waker.replace(cx.waker().clone());
                    self.switcher.clear();
                    self.transition_to(SlotState::Idle);
                    return Poll::Pending;
                }

                SlotState::Paused { active } => {
                    self.waker.replace(cx.waker().clone());
                    self.switcher.clear();
                    self.transition_to(SlotState::Paused { active });
                    return Poll::Pending;
                }

                SlotState::Resuming { mut staging } => {
                    match staging.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.stage(pkt);
                            if self.switcher.ready_to_stream() {
                                tracing::info!(mid = %self.mid, rid = ?staging.rid, "Resuming complete");
                                // Transition: Move staging to active
                                self.transition_to(SlotState::Streaming { active: staging });
                            } else {
                                // Stay
                                self.transition_to(SlotState::Resuming { staging });
                            }
                            // Continue loop to drain switcher or process more
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            // We're still resuming, it is okay to continue since we haven't sent
                            // any packet yet to downstream
                            tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, keep going");
                            self.transition_to(SlotState::Resuming { staging });
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Resuming closed");
                            self.transition_to(SlotState::Idle);
                        }
                        Poll::Pending => {
                            self.transition_to(SlotState::Resuming { staging });
                            return Poll::Pending;
                        }
                    }
                }

                SlotState::Streaming { mut active } => match active.channel.poll_recv(cx) {
                    Poll::Ready(Ok(pkt)) => {
                        self.switcher.push(pkt);
                        self.transition_to(SlotState::Streaming { active });
                    }
                    Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                        tracing::warn!(mid = %self.mid, skipped = n, "Streaming lagged, pausing");
                        self.transition_to(SlotState::Paused { active });
                    }
                    Poll::Ready(Err(spmc::RecvError::Closed)) => {
                        tracing::info!(mid = %self.mid, "Streaming closed");
                        self.transition_to(SlotState::Idle);
                    }
                    Poll::Pending => {
                        self.transition_to(SlotState::Streaming { active });
                        return Poll::Pending;
                    }
                },

                SlotState::Switching {
                    mut active,
                    mut staging,
                } => {
                    // 1. Poll Staging (Priority)
                    let staging_poll = staging.channel.poll_recv(cx);
                    match staging_poll {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.stage(pkt);
                            if self.switcher.ready_to_stream() {
                                tracing::info!(mid = %self.mid, from = ?active.rid, to = ?staging.rid, "Switch complete");
                                // Transition: Move staging to active, Drop old active
                                self.transition_to(SlotState::Streaming { active: staging });
                            } else {
                                self.transition_to(SlotState::Switching { active, staging });
                            }
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged, pausing");
                            self.transition_to(SlotState::Paused { active: staging });
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Staging closed during switch");
                            // Revert to streaming active
                            self.transition_to(SlotState::Streaming { active });
                            continue;
                        }
                        Poll::Pending => { /* Check active next */ }
                    }

                    // 2. Poll Active
                    match active.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.push(pkt);
                            self.transition_to(SlotState::Switching { active, staging });
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Active lagged during switch, pausing");
                            self.transition_to(SlotState::Paused { active });
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Active closed, forcing resume");
                            self.transition_to(SlotState::Resuming { staging });
                        }
                        Poll::Pending => {
                            // Both are pending, restore state and return
                            self.transition_to(SlotState::Switching { active, staging });
                            return Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}
