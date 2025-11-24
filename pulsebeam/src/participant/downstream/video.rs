use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::Waker;
use std::task::{Context, Poll};
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, Mid, Rid};

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
        if self.tracks.remove(track_id).is_some() {
            tracing::info!(
                track = %track_id,
                "video track removed"
            );
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

        if self.slots.is_empty() {
            return (Bitrate::from(0), Bitrate::from(0));
        }

        let budget = available_bandwidth.as_f64().max(300_000.0);

        // Sort by priority (max_height) so important slots get budget first
        self.slots.sort_by_key(|s| std::cmp::Reverse(s.max_height));

        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;

        for slot in &mut self.slots {
            let Some(current_receiver) = slot.target_receiver() else {
                continue;
            };

            let Some(state) = self.tracks.get_mut(&current_receiver.meta.id) else {
                continue;
            };

            let track = &state.track;
            let paused = slot.is_paused();

            let desired = if current_receiver.state.is_inactive() {
                track.lowest_quality()
            } else {
                match current_receiver.state.quality() {
                    StreamQuality::Bad => track.lowest_quality(),
                    StreamQuality::Good => current_receiver,
                    StreamQuality::Excellent => track
                        .higher_quality(current_receiver.quality)
                        .filter(|next| {
                            next.state.quality() == StreamQuality::Excellent
                                && !next.state.is_inactive()
                        })
                        .unwrap_or(current_receiver),
                }
            };

            let is_upgrade = desired.quality > current_receiver.quality;
            let is_downgrade = desired.quality < current_receiver.quality;

            let desired_bitrate = if is_upgrade {
                desired.state.bitrate_bps() * UPGRADE_HYSTERESIS_FACTOR
            } else if is_downgrade {
                desired.state.bitrate_bps() * DOWNGRADE_HYSTERESIS_FACTOR
            } else {
                desired.state.bitrate_bps()
            };

            total_desired += desired_bitrate;

            // Greedy allocation: If it fits, take it.
            // Note: Because we sorted by max_height above, high priority tracks
            // are processed first and claim the budget.
            if total_allocated + desired_bitrate <= budget
                && (paused || desired.rid != current_receiver.rid)
            {
                total_allocated += desired.state.bitrate_bps();
                slot.switch_to(desired.clone());
            } else {
                // Can't afford upgrade, or no change needed.
                // We must account for the cost of keeping the *current* stream.
                total_allocated += current_receiver.state.bitrate_bps();
            }
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

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Mid, RtpPacket)>> {
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
            match slot.poll_next(cx) {
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

struct Slot {
    mid: Mid,
    max_height: u32,
    switcher: Switcher,
    state: Option<SlotState>,
    waker: Option<Waker>,
}

impl Slot {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            max_height: 0,
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            state: Some(SlotState::Idle),
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

    pub fn track_id(&self) -> Option<&Arc<TrackId>> {
        self.target_receiver().map(|r| &r.meta.id)
    }

    pub fn rid(&self) -> Option<Rid> {
        self.target_receiver().and_then(|r| r.rid)
    }

    pub fn quality(&self) -> SimulcastQuality {
        self.target_receiver()
            .map(|r| r.quality)
            .unwrap_or(SimulcastQuality::Undefined)
    }

    pub fn switch_to(&mut self, mut receiver: SimulcastReceiver) {
        if let Some(current) = self.target_receiver()
            && current.rid == receiver.rid
            && current.meta.id == receiver.meta.id
            && !matches!(self.state(), SlotState::Paused { .. })
        {
            return;
        }

        // Take ownership of state to move internals
        let old_state = self.state.take().unwrap_or(SlotState::Idle);

        tracing::info!(
            mid = %self.mid,
            to_rid = ?receiver.rid,
            track = %receiver.meta.id,
            "switch_to: initiating switch"
        );
        receiver.channel.reset();
        receiver.request_keyframe(KeyframeRequestKind::Fir);

        let new_state = match old_state {
            SlotState::Idle | SlotState::Paused { .. } => SlotState::Resuming { staging: receiver },
            SlotState::Streaming { active } => SlotState::Switching {
                active,
                staging: receiver,
            },
            SlotState::Resuming { .. } => SlotState::Resuming { staging: receiver },
            SlotState::Switching { active, .. } => SlotState::Switching {
                active,
                staging: receiver,
            },
        };

        self.state = Some(new_state);
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
        self.state = Some(SlotState::Idle);
    }

    fn request_keyframe(&self, kind: KeyframeRequestKind) {
        match self.state() {
            SlotState::Resuming { staging } | SlotState::Switching { staging, .. } => {
                staging.request_keyframe(kind);
            }
            SlotState::Streaming { active } => {
                active.request_keyframe(kind);
            }
            SlotState::Paused { .. } | SlotState::Idle => {}
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<RtpPacket>> {
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
                    self.state = Some(SlotState::Idle);
                    return Poll::Pending;
                }

                SlotState::Paused { active } => {
                    self.waker.replace(cx.waker().clone());
                    self.switcher.clear();
                    self.state = Some(SlotState::Paused { active });
                    return Poll::Pending;
                }

                SlotState::Resuming { mut staging } => {
                    match staging.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.stage(pkt);
                            if self.switcher.is_ready() {
                                tracing::info!(mid = %self.mid, rid = ?staging.rid, "Resuming complete");
                                // Transition: Move staging to active
                                self.state = Some(SlotState::Streaming { active: staging });
                            } else {
                                // Stay
                                self.state = Some(SlotState::Resuming { staging });
                            }
                            // Continue loop to drain switcher or process more
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, pausing");
                            self.state = Some(SlotState::Paused { active: staging });
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Resuming closed");
                            self.state = Some(SlotState::Idle);
                        }
                        Poll::Pending => {
                            self.state = Some(SlotState::Resuming { staging });
                            return Poll::Pending;
                        }
                    }
                }

                SlotState::Streaming { mut active } => match active.channel.poll_recv(cx) {
                    Poll::Ready(Ok(pkt)) => {
                        self.switcher.push(pkt);
                        self.state = Some(SlotState::Streaming { active });
                    }
                    Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                        tracing::warn!(mid = %self.mid, skipped = n, "Streaming lagged, pausing");
                        self.state = Some(SlotState::Paused { active });
                    }
                    Poll::Ready(Err(spmc::RecvError::Closed)) => {
                        tracing::info!(mid = %self.mid, "Streaming closed");
                        self.state = Some(SlotState::Idle);
                    }
                    Poll::Pending => {
                        self.state = Some(SlotState::Streaming { active });
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
                            if self.switcher.is_ready() {
                                tracing::info!(mid = %self.mid, from = ?active.rid, to = ?staging.rid, "Switch complete");
                                // Transition: Move staging to active, Drop old active
                                self.state = Some(SlotState::Streaming { active: staging });
                            } else {
                                self.state = Some(SlotState::Switching { active, staging });
                            }
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged, pausing");
                            self.state = Some(SlotState::Paused { active: staging });
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Staging closed during switch");
                            // Revert to streaming active
                            self.state = Some(SlotState::Streaming { active });
                            continue;
                        }
                        Poll::Pending => { /* Check active next */ }
                    }

                    // 2. Poll Active
                    match active.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.push(pkt);
                            self.state = Some(SlotState::Switching { active, staging });
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Active lagged during switch, pausing");
                            self.state = Some(SlotState::Paused { active });
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Active closed, forcing resume");
                            self.state = Some(SlotState::Resuming { staging });
                        }
                        Poll::Pending => {
                            // Both are pending, restore state and return
                            self.state = Some(SlotState::Switching { active, staging });
                            return Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}
