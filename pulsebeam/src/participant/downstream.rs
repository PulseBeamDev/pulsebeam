use crate::participant::bitrate::BitrateController;
use crate::rtp::monitor::{StreamQuality, StreamState};
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use futures::stream::{Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::{Context, Poll};
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastQuality, SimulcastReceiver, TrackReceiver};

pub struct DownstreamAllocator {
    audio_tracks: HashMap<Arc<TrackId>, TrackState>,
    video_tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<Slot>,
    video_slots: Vec<Slot>,
    available_bandwidth: BitrateController,
    ticks: u32,
    rr_cursor: usize,
}

impl DownstreamAllocator {
    pub fn new() -> Self {
        Self {
            audio_tracks: HashMap::new(),
            video_tracks: HashMap::new(),
            audio_slots: Vec::new(),
            video_slots: Vec::new(),

            available_bandwidth: BitrateController::default(),
            ticks: 0,
            rr_cursor: 0,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        match track.meta.kind {
            MediaKind::Audio => {
                if self.audio_tracks.contains_key(&track.meta.id) {
                    return;
                }

                self.audio_tracks.insert(
                    track.meta.id.clone(),
                    TrackState {
                        track,
                        assigned_mid: None,
                    },
                );
                self.rebalance_audio();
            }
            MediaKind::Video => {
                if self.video_tracks.contains_key(&track.meta.id) {
                    return;
                }

                self.video_tracks.insert(
                    track.meta.id.clone(),
                    TrackState {
                        track,
                        assigned_mid: None,
                    },
                );
                self.rebalance_video();
            }
        }
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if self.audio_tracks.remove(track_id).is_some() {
            self.rebalance_audio();
        } else if self.video_tracks.remove(track_id).is_some() {
            self.rebalance_video();
        }
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => {
                self.audio_slots.push(Slot::new(mid, kind));
            }
            MediaKind::Video => {
                self.video_slots.push(Slot::new(mid, kind));
            }
        }

        self.rebalance_assignments();
    }

    pub fn set_slot_max_height(&mut self, mid: Mid, max_height: u32) {
        if let Some(slot) = self.video_slots.iter_mut().find(|s| s.mid == mid) {
            slot.priority = max_height;
        }
    }

    /// Handle BWE and compute both current and desired bitrate in one pass.
    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) -> (Bitrate, Bitrate) {
        self.available_bandwidth
            .update(available_bandwidth, Instant::now());
        self.update_allocations()
    }

    // Update allocations based on the following events:
    //  1. Available bandwidth
    //  2. Video slots
    //  3. update_allocations get polled every 500ms
    pub fn update_allocations(&mut self) -> (Bitrate, Bitrate) {
        const DOWNGRADE_HYSTERESIS_FACTOR: f64 = 0.85;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.25;

        if self.video_slots.is_empty() {
            return (Bitrate::from(0), Bitrate::from(0));
        }

        let budget = self.available_bandwidth.current().as_f64().max(300_000.0);
        self.video_slots.sort_by_key(|s| s.priority);

        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;
        let mut upgraded = true;

        while upgraded {
            upgraded = false;
            total_allocated = 0.0;
            total_desired = 0.0;

            for slot in &mut self.video_slots {
                let Some(current_receiver) = slot.target_receiver() else {
                    continue;
                };

                let Some(state) = self.video_tracks.get_mut(&current_receiver.meta.id) else {
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
                available = %self.available_bandwidth.current(),
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
        let Some(slot) = self.video_slots.iter().find(|e| e.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        slot.request_keyframe(req.kind);
    }

    fn rebalance_assignments(&mut self) {
        self.rebalance_audio();
        self.rebalance_video();
    }

    fn rebalance_audio(&mut self) {}

    fn rebalance_video(&mut self) {
        let mut unassigned_tracks = self.video_tracks.iter_mut().filter(|(_, state)| {
            assert!(state.track.meta.kind.is_video());
            state.assigned_mid.is_none()
        });

        for slot in &mut self.video_slots {
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
}

fn compare_audio(a: &StreamState, b: &StreamState) -> Ordering {
    // Thresholds for stability
    const SPEECH_THRESHOLD: f32 = 0.01; // Minimum envelope to count as "talking"
    const ENVELOPE_EPSILON: f32 = 0.05; // 5% diff required to swap active speakers
    const SILENCE_EPSILON_MS: u128 = 500; // 500ms diff required to swap silent speakers

    let env_a = a.audio_envelope();
    let env_b = b.audio_envelope();
    let silence_a = a.silence_duration();
    let silence_b = b.silence_duration();

    let a_is_talking = env_a > SPEECH_THRESHOLD;
    let b_is_talking = env_b > SPEECH_THRESHOLD;

    // 1. BINARY PRIORITY: Active Speaker > Silent User
    if a_is_talking != b_is_talking {
        return if a_is_talking {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }

    // 2. MOMENTUM: Compare Intensity (if both active)
    if a_is_talking && b_is_talking {
        let diff = (env_a - env_b).abs();
        if diff > ENVELOPE_EPSILON {
            // Significant difference: Louder wins
            return env_b.partial_cmp(&env_a).unwrap();
        }
        // If difference is small (< 5%), treat as tied and fall through to Recency
    }

    // 3. RECENCY: Compare Silence Duration (Who spoke last?)
    let ms_a = silence_a.as_millis();
    let ms_b = silence_b.as_millis();
    let diff_ms = ms_a.abs_diff(ms_b);

    if diff_ms > SILENCE_EPSILON_MS {
        // Significant difference: More recent (lower duration) wins
        return ms_a.cmp(&ms_b);
    }

    // 4. TIE: Effectively identical audio state
    Ordering::Equal
}

impl Stream for DownstreamAllocator {
    type Item = (Mid, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        for slot in &mut this.audio_slots {
            if let Poll::Ready(Some(pkt)) = slot.poll_next_unpin(cx) {
                return Poll::Ready(Some((slot.mid, pkt)));
            }
        }

        let mut checked_count = 0;
        while checked_count < this.video_slots.len() {
            if this.rr_cursor >= this.video_slots.len() {
                this.rr_cursor = 0;
            }

            let slot = &mut this.video_slots[this.rr_cursor];

            match slot.poll_next_unpin(cx) {
                Poll::Ready(Some(pkt)) => {
                    // ensure each frame to be flushed before moving to another stream,
                    // this allows related contexts to stay in L1/L2 caches longer.
                    return Poll::Ready(Some((slot.mid, pkt)));
                }
                Poll::Ready(None) => {
                    this.rr_cursor += 1;
                    checked_count += 1;
                }
                Poll::Pending => {
                    this.rr_cursor += 1;
                    checked_count += 1;
                }
            }
        }

        // If we checked everyone and found nothing.
        Poll::Pending
    }
}

#[derive(Debug)]
struct TrackState {
    track: TrackReceiver,
    assigned_mid: Option<Mid>,
}

pub enum SlotState {
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

pub struct Slot {
    mid: Mid,
    kind: MediaKind,

    priority: u32,
    switcher: Switcher,
    state: SlotState,
}

impl Slot {
    fn new(mid: Mid, kind: MediaKind) -> Self {
        let clock_rate = match kind {
            MediaKind::Audio => rtp::AUDIO_FREQUENCY,
            MediaKind::Video => rtp::VIDEO_FREQUENCY,
        };
        Self {
            mid,
            kind,
            priority: 0,
            switcher: Switcher::new(clock_rate),
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
}

impl Stream for Slot {
    type Item = RtpPacket;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(pkt) = this.switcher.pop() {
                return Poll::Ready(Some(pkt));
            }

            match &mut this.state {
                SlotState::Idle => return Poll::Pending,

                SlotState::Paused { .. } => {
                    this.switcher.drain();
                    return Poll::Pending;
                }

                SlotState::Resuming { staging } => match staging.channel.poll_recv(cx) {
                    Poll::Ready(Ok(pkt)) => {
                        this.switcher.stage(pkt.value.clone());
                        if this.switcher.is_ready() {
                            tracing::info!(mid = %this.mid, rid = ?staging.rid, "Resuming complete");
                            this.state = SlotState::Streaming {
                                active: staging.clone(),
                            };
                        }
                    }
                    Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                        tracing::warn!(mid = %this.mid, skipped = n, "Resuming lagged, pausing");
                        this.state = SlotState::Paused {
                            active: staging.clone(),
                        };
                    }
                    Poll::Ready(Err(spmc::RecvError::Closed)) => {
                        tracing::warn!(mid = %this.mid, "Resuming closed");
                        this.state = SlotState::Idle;
                    }
                    Poll::Pending => return Poll::Pending,
                },

                SlotState::Streaming { active } => match ready!(active.channel.poll_recv(cx)) {
                    Ok(pkt) => this.switcher.push(pkt.value.clone()),
                    Err(spmc::RecvError::Lagged(n)) => {
                        tracing::warn!(mid = %this.mid, skipped = n, "Streaming lagged, pausing");
                        this.state = SlotState::Paused {
                            active: active.clone(),
                        };
                    }
                    Err(spmc::RecvError::Closed) => {
                        tracing::info!(mid = %this.mid, "Streaming closed");
                        this.state = SlotState::Idle;
                    }
                },

                SlotState::Switching { active, staging } => {
                    match staging.channel.poll_recv(cx) {
                        Poll::Ready(Ok(pkt)) => {
                            this.switcher.stage(pkt.value.clone());
                            if this.switcher.is_ready() {
                                tracing::info!(
                                    mid = %this.mid,
                                    from = ?active.rid,
                                    to = ?staging.rid,
                                    "Switch complete"
                                );
                                this.state = SlotState::Streaming {
                                    active: staging.clone(),
                                };
                            }
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                            tracing::warn!(mid = %this.mid, skipped = n, "Staging lagged, pausing");
                            this.state = SlotState::Paused {
                                active: staging.clone(),
                            };
                            continue;
                        }
                        Poll::Ready(Err(spmc::RecvError::Closed)) => {
                            tracing::warn!(mid = %this.mid, "Staging closed during switch");
                            this.state = SlotState::Streaming {
                                active: active.clone(),
                            };
                            continue;
                        }
                        Poll::Pending => {}
                    }

                    match ready!(active.channel.poll_recv(cx)) {
                        Ok(pkt) => this.switcher.push(pkt.value.clone()),
                        Err(spmc::RecvError::Lagged(n)) => {
                            tracing::warn!(mid = %this.mid, skipped = n, "Active lagged during switch, pausing");
                            this.state = SlotState::Paused {
                                active: active.clone(),
                            };
                        }
                        Err(spmc::RecvError::Closed) => {
                            tracing::warn!(mid = %this.mid, "Active closed, forcing resume");
                            this.state = SlotState::Resuming {
                                staging: staging.clone(),
                            };
                        }
                    }
                }
            }
        }
    }
}
