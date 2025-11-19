use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::ready;
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
            self.rebalance();
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.push(Slot::new(mid));
        self.rebalance();
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

    // Update allocations based on the following events:
    //  1. Available bandwidth
    //  2. Video slots
    //  3. update_allocations get polled every 500ms
    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> (Bitrate, Bitrate) {
        const DOWNGRADE_HYSTERESIS_FACTOR: f64 = 0.85;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.25;

        if self.slots.is_empty() {
            return (Bitrate::from(0), Bitrate::from(0));
        }

        let budget = available_bandwidth.as_f64().max(300_000.0);
        self.slots.sort_by_key(|s| s.max_height);

        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;
        let mut upgraded = true;

        while upgraded {
            upgraded = false;
            total_allocated = 0.0;
            total_desired = 0.0;

            for slot in &mut self.slots {
                let Some(current_receiver) = slot.target_receiver() else {
                    continue;
                };

                let Some(state) = self.tracks.get_mut(&current_receiver.meta.id) else {
                    continue;
                };

                let mut target_receiver = current_receiver;
                let track = &state.track;
                let paused = slot.is_paused();

                // If paused, initialize to lowest quality
                if paused {
                    target_receiver = track.lowest_quality();
                }

                let desired = if current_receiver.state.is_inactive() {
                    // very likely the sender can't keep up with sending higher resolution.
                    track.lowest_quality()
                } else {
                    // Conservative upgrade logic: only upgrade if higher layer is excellent
                    match current_receiver.state.quality() {
                        StreamQuality::Bad => track.lowest_quality(),
                        StreamQuality::Good => current_receiver, // Stay at current when good
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

                // Apply hysteresis
                let desired_bitrate = if is_upgrade {
                    desired.state.bitrate_bps() * UPGRADE_HYSTERESIS_FACTOR
                } else if is_downgrade {
                    desired.state.bitrate_bps() * DOWNGRADE_HYSTERESIS_FACTOR
                } else {
                    desired.state.bitrate_bps()
                };

                total_desired += desired_bitrate;

                if total_allocated + desired_bitrate <= budget
                    && (paused || desired.rid != current_receiver.rid)
                {
                    upgraded = true;
                    total_allocated += desired.state.bitrate_bps();
                    slot.switch_to(target_receiver.clone());
                } else {
                    // Can't afford upgrade or change - stay at current
                    total_allocated += current_receiver.state.bitrate_bps();
                }
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
    state: SlotState,
}

impl Slot {
    fn new(mid: Mid) -> Self {
        Self {
            mid,
            max_height: 0,
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            state: SlotState::Idle,
        }
    }

    /// Helper to get the "Forward Looking" receiver.
    /// If we are switching or resuming, this returns the staging receiver.
    /// If we are streaming or paused, it returns the active one.
    fn target_receiver(&self) -> Option<&SimulcastReceiver> {
        match &self.state {
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
        // Check if we are already playing (or trying to play) this exact stream
        if let Some(current) = self.target_receiver() {
            if current.rid == receiver.rid && current.meta.id == receiver.meta.id {
                // If we are Paused, we must wake up (continue to transition logic).
                // If we are already Streaming or Switching to this, do nothing.
                if !matches!(self.state, SlotState::Paused { .. }) {
                    return;
                }
            }
        }

        let old_state = std::mem::replace(&mut self.state, SlotState::Idle);

        receiver.channel.reset();
        receiver.request_keyframe(KeyframeRequestKind::Fir);
        self.state = match old_state {
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
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state, SlotState::Idle | SlotState::Paused { .. })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, SlotState::Idle)
    }

    pub fn stop(&mut self) {
        self.state = SlotState::Idle;
    }

    fn request_keyframe(&self, kind: KeyframeRequestKind) {
        // Priority: Request from the new stream we are waiting for.
        match &self.state {
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

            match &mut self.state {
                SlotState::Idle => return Poll::Pending,

                SlotState::Paused { .. } => {
                    self.switcher.drain();
                    return Poll::Pending;
                }

                SlotState::Resuming { staging } => match staging.channel.poll_recv(cx) {
                    Poll::Ready(Ok(pkt)) => {
                        self.switcher.stage(pkt.value.clone());
                        if self.switcher.is_ready() {
                            tracing::info!(mid = %self.mid, rid = ?staging.rid, "Resuming complete");
                            self.state = SlotState::Streaming {
                                active: staging.clone(),
                            };
                        }
                    }
                    Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                        tracing::warn!(mid = %self.mid, skipped = n, "Resuming lagged, pausing");
                        self.state = SlotState::Paused {
                            active: staging.clone(),
                        };
                    }
                    Poll::Ready(Err(spmc::RecvError::Closed)) => {
                        tracing::warn!(mid = %self.mid, "Resuming closed");
                        self.state = SlotState::Idle;
                    }
                    Poll::Pending => return Poll::Pending,
                },

                SlotState::Streaming { active } => match ready!(active.channel.poll_recv(cx)) {
                    Ok(pkt) => self.switcher.push(pkt.value.clone()),
                    Err(spmc::RecvError::Lagged(n)) => {
                        tracing::warn!(mid = %self.mid, skipped = n, "Streaming lagged, pausing");
                        self.state = SlotState::Paused {
                            active: active.clone(),
                        };
                    }
                    Err(spmc::RecvError::Closed) => {
                        tracing::info!(mid = %self.mid, "Streaming closed");
                        self.state = SlotState::Idle;
                    }
                },

                SlotState::Switching { active, staging } => {
                    match staging.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            self.switcher.stage(pkt.value.clone());
                            if self.switcher.is_ready() {
                                tracing::info!(
                                    mid = %self.mid,
                                    from = ?active.rid,
                                    to = ?staging.rid,
                                    "Switch complete"
                                );
                                self.state = SlotState::Streaming {
                                    active: staging.clone(),
                                };
                            }
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged, pausing");
                            self.state = SlotState::Paused {
                                active: staging.clone(),
                            };
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %self.mid, "Staging closed during switch");
                            self.state = SlotState::Streaming {
                                active: active.clone(),
                            };
                            continue;
                        }
                        Poll::Pending => {}
                    }

                    match ready!(active.channel.poll_recv(cx)) {
                        Ok(pkt) => self.switcher.push(pkt.value.clone()),
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %self.mid, skipped = n, "Active lagged during switch, pausing");
                            self.state = SlotState::Paused {
                                active: active.clone(),
                            };
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %self.mid, "Active closed, forcing resume");
                            self.state = SlotState::Resuming {
                                staging: staging.clone(),
                            };
                        }
                    }
                }
            }
        }
    }
}
