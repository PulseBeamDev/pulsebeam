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

use crate::entity::TrackId;
use crate::track::{SimulcastQuality, SimulcastReceiver, TrackReceiver};

#[derive(Default)]
pub struct VideoAllocator {
    tracks: HashMap<Arc<TrackId>, TrackState>,
    slots: Vec<Slot>,

    ticks: u32,
    rr_cursor: usize,
    waker: Option<Waker>,
}

impl VideoAllocator {
    pub fn set_slot_max_height(&mut self, mid: Mid, max_height: u32) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.mid == mid) {
            slot.max_height = max_height;
        }
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

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
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
            slot.switch_to(next_track.1.track.lowest_quality().clone());
        }
    }

    // based on Greedy Knapsack
    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> (Bitrate, Bitrate) {
        const DOWNGRADE_HYSTERESIS_FACTOR: f64 = 0.85;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.25;
        const MIN_BANDWIDTH: f64 = 300_000.0;

        struct SlotPlan<'a> {
            current_receiver: &'a SimulcastReceiver,
            target_receiver: &'a SimulcastReceiver,
            current_bitrate: f64,
            desired_bitrate: f64,
            paused: bool,
            commited: bool,
        }

        if self.slots.is_empty() {
            return (Bitrate::from(0), Bitrate::from(0));
        }

        // Sort by priority (max_height) - higher priority slots first
        self.slots.sort_by_key(|s| std::cmp::Reverse(s.max_height));
        let mut budget = available_bandwidth.as_f64().max(MIN_BANDWIDTH);

        // Step 1: Start everyone at lowest quality
        let mut slot_plans: Vec<SlotPlan> = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let Some(current_receiver) = slot.target_receiver() else {
                continue;
            };

            let Some(state) = self.tracks.get(&current_receiver.meta.id) else {
                continue;
            };

            let lowest = state.track.lowest_quality();
            let current_bitrate = current_receiver.state.bitrate_bps();
            let paused = current_bitrate > budget;

            slot_plans.push(SlotPlan {
                current_receiver,
                target_receiver: lowest,
                current_bitrate,
                desired_bitrate: current_bitrate,
                paused,
                commited: paused, // if we're pausing, we're commited
            });
        }

        // Step 2: Upgrade slots one step at a time in priority order
        // Keep looping until no more upgrades can be made
        let mut made_progress = true;
        while made_progress {
            made_progress = false;

            for plan in &mut slot_plans {
                if plan.commited {
                    continue;
                }

                // if any executes continue, we assume that the plan is commited
                plan.commited = true;

                if plan.current_receiver.state.is_inactive() {
                    continue;
                }

                let Some(state) = self.tracks.get(&plan.current_receiver.meta.id) else {
                    continue;
                };

                let desired_receiver = match plan.target_receiver.state.quality() {
                    StreamQuality::Bad => continue,
                    StreamQuality::Good => continue,
                    StreamQuality::Excellent => {
                        let Some(desired) = state
                            .track
                            .higher_quality(plan.target_receiver.quality)
                            .filter(|next| {
                                next.state.quality() == StreamQuality::Excellent
                                    && !next.state.is_inactive()
                            })
                        else {
                            continue;
                        };
                        desired
                    }
                };

                let desired_bitrate =
                    desired_receiver.state.bitrate_bps() * UPGRADE_HYSTERESIS_FACTOR;

                let upgrade_cost = if desired_bitrate > plan.current_bitrate {
                    desired_bitrate - plan.current_bitrate
                } else {
                    0.0
                };

                if upgrade_cost > budget {
                    continue;
                }

                // we might upgrade again
                plan.commited = false;
                plan.current_bitrate += upgrade_cost;
                budget -= upgrade_cost;
            }
        }

        // Step 3: Apply the allocations
        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;
        for (idx, plan) in slot_plans.drain(..).enumerate() {
            let slot = &mut self.slots[idx];
            slot.switch_to(plan.target_receiver.clone());
            total_allocated += plan.current_bitrate;
            total_desired += plan.desired_bitrate;
        }

        let total_allocated = Bitrate::from(total_allocated);
        let total_desired = Bitrate::from(total_desired);

        if self.ticks >= 30 {
            tracing::debug!(
                available = %available_bandwidth,
                budget = %Bitrate::from(budget),
                allocated = %total_allocated,
                desired = %total_desired,
                "allocation summary"
            );
            self.ticks = 0;
        }
        self.ticks += 1;

        (total_allocated, total_desired)
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
    keyframe_req_count: u32,
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
            keyframe_req_count: 0,
            waker: None,
        }
    }

    fn state(&self) -> &SlotState {
        self.state.as_ref().expect("State invalid outside poll")
    }

    fn target_receiver(&self) -> Option<&SimulcastReceiver> {
        match self.state() {
            SlotState::Resuming { staging } | SlotState::Switching { staging, .. } => Some(staging),
            SlotState::Streaming { active } | SlotState::Paused { active } => Some(active),
            SlotState::Idle => None,
        }
    }

    pub fn switch_to(&mut self, mut receiver: SimulcastReceiver) {
        if let Some(current) = self.target_receiver()
            && current.rid == receiver.rid
            && current.meta.id == receiver.meta.id
            && !matches!(self.state(), SlotState::Paused { .. })
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
        self.keyframe_req_count = 1;

        tracing::info!(
            mid = %self.mid,
            to_rid = ?receiver.rid,
            track = %receiver.meta.id,
            "switch_to: initiating switch"
        );
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
        let Some(started_at) = self.switching_started_at else {
            return;
        };

        if self.keyframe_req_count >= 5 {
            return;
        }

        let deadline = started_at + Duration::from_secs(1) * self.keyframe_req_count;
        if deadline > now {
            return;
        }

        self.keyframe_req_count += 1;
        let Some(receiver) = self.target_receiver() else {
            return;
        };
        receiver.request_keyframe(KeyframeRequestKind::Fir);
    }

    #[inline]
    fn transition_to(&mut self, new_state: SlotState) {
        match &new_state {
            SlotState::Streaming { .. } => {
                self.switching_started_at = None;
                self.keyframe_req_count = 0;
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
                            tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, pausing");
                            self.transition_to(SlotState::Paused { active: staging });
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
