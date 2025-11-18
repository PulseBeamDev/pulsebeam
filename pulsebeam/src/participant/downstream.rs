use crate::participant::bitrate::BitrateController;
use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use futures::stream::{SelectAll, Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::{Context, Poll};
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use str0m::rtp::RtpHeader;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackReceiver};

pub struct DownstreamAllocator {
    streams: SelectAll<SlotStream>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<Slot>,
    video_slots: Vec<Slot>,
    available_bandwidth: BitrateController,
    ticks: u32,
}

impl DownstreamAllocator {
    pub fn new() -> Self {
        Self {
            streams: SelectAll::new(),
            tracks: HashMap::new(),
            audio_slots: Vec::new(),
            video_slots: Vec::new(),

            available_bandwidth: BitrateController::default(),
            ticks: 0,
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
                audio_level: AudioLevel(-127),
                assigned_mid: None,
            },
        );
        self.rebalance_allocations();
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if self.tracks.remove(track_id).is_some() {
            self.rebalance_allocations();
        }
    }

    pub fn add_slot(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Audio => {
                self.audio_slots.push(SlotState {
                    mid,
                    assigned_track: None,
                    priority: 0,
                    switcher: Switcher::new(rtp::AUDIO_FREQUENCY),
                });
            }
            MediaKind::Video => {
                self.video_slots.push(SlotState {
                    mid,
                    assigned_track: None,
                    priority: 720,
                    switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
                });
            }
        }

        self.streams.push(Slot::new(mid, kind));
        self.rebalance_allocations();
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

            for slot in &self.video_slots {
                let Some(track_id) = &slot.assigned_track else {
                    continue;
                };

                let Some(state) = self.tracks.get_mut(track_id) else {
                    continue;
                };

                let track = &state.track;
                let mut config = state.current();

                // If paused, initialize to lowest quality
                if config.paused {
                    config.target_rid = track.lowest_quality().rid;
                }

                let Some(current_receiver) = track.by_rid(&config.target_rid) else {
                    continue;
                };

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
                    && (config.paused || desired.rid != current_receiver.rid)
                {
                    upgraded = true;
                    total_allocated += desired.state.bitrate_bps();
                    config.target_rid = desired.rid;
                    config.paused = false;
                    state.update(|c| *c = config);
                } else {
                    // Can't afford upgrade or change - stay at current
                    total_allocated += current_receiver.state.bitrate_bps();
                }
            }
        }

        let total_allocated = Bitrate::from(total_allocated);
        let total_desired = Bitrate::from(total_desired);

        if self.ticks % 30 == 0 {
            tracing::debug!(
                available = %self.available_bandwidth.current(),
                budget = %Bitrate::from(budget),
                allocated = %total_allocated,
                desired = %total_desired,
                "allocation summary"
            );
        }
        self.ticks += 1;

        (total_allocated, total_desired)
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) {
        let Some(slot) = self.video_slots.iter().find(|e| e.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        let Some(track) = &slot.assigned_track else {
            tracing::warn!(?req, "no assigned track, ignore keyframe request");
            return;
        };
        let Some(state) = self.tracks.get(track) else {
            tracing::warn!(?req, "no track state found, ignore keyframe request");
            return;
        };
        state.request_keyframe(req.kind);
    }

    pub fn handle_rtp(&mut self, track_id: &TrackId, hdr: &RtpHeader) -> Option<Mid> {
        let assigned_mid = self.tracks.get(track_id).and_then(|s| s.assigned_mid);

        let mut needs_rebalance = false;
        if let Some(track_state) = self.tracks.get_mut(track_id) {
            if track_state.track.meta.kind == MediaKind::Audio {
                let new_level = hdr.ext_vals.audio_level.unwrap_or(-127);
                if track_state.audio_level.0 != new_level as i32 {
                    track_state.audio_level = AudioLevel(new_level as i32);
                    needs_rebalance = true;
                }
            }
        }

        if needs_rebalance {
            self.rebalance_allocations();
        }

        assigned_mid
    }

    fn rebalance_allocations(&mut self) {
        self.rebalance_audio();
        self.rebalance_video();
    }

    fn rebalance_audio(&mut self) {
        let mut audio_tracks: Vec<_> = self
            .tracks
            .iter()
            .filter(|(_, state)| state.track.meta.kind == MediaKind::Audio)
            .map(|(id, state)| (id.clone(), state.audio_level))
            .collect();
        audio_tracks.sort_unstable_by_key(|k| k.1);

        let active_speakers: HashMap<_, _> = audio_tracks
            .into_iter()
            .take(self.audio_slots.len())
            .map(|(id, _)| (id, ()))
            .collect();

        for slot in &mut self.audio_slots {
            if let Some(track_id) = slot.assigned_track.as_ref() {
                if !active_speakers.contains_key(track_id) {
                    Self::perform_unassignment(&mut self.tracks, slot);
                }
            }
        }

        let mut speakers_to_assign: Vec<_> = active_speakers
            .keys()
            .filter(|id| {
                self.tracks
                    .get(*id)
                    .map_or(false, |s| s.assigned_mid.is_none())
            })
            .cloned()
            .collect();

        for slot in &mut self.audio_slots {
            if slot.assigned_track.is_none() {
                if let Some(track_id) = speakers_to_assign.pop() {
                    Self::perform_assignment(&mut self.tracks, slot, &track_id);
                }
            }
        }
    }

    fn rebalance_video(&mut self) {
        let mut unassigned_tracks: Vec<_> = self
            .tracks
            .iter()
            .filter(|(_, state)| {
                state.track.meta.kind == MediaKind::Video && state.assigned_mid.is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();

        for slot in &mut self.video_slots {
            if let Some(track_id) = &slot.assigned_track {
                if !self.tracks.contains_key(track_id) {
                    Self::perform_unassignment(&mut self.tracks, slot);
                    slot.assigned_track = None;
                }
            } else {
                if let Some(track_id) = unassigned_tracks.pop() {
                    Self::perform_assignment(&mut self.tracks, slot, &track_id);
                    slot.assigned_track = Some(track_id);
                }
            }
        }
    }

    fn perform_assignment(
        tracks: &mut HashMap<Arc<TrackId>, TrackState>,
        slot: &mut SlotState,
        track_id: &Arc<TrackId>,
    ) {
        if let Some(state) = tracks.get_mut(track_id) {
            slot.assigned_track = Some(track_id.clone());
            state.assigned_mid = Some(slot.mid);
            state.update(|c| {
                c.paused = false;
            });
            tracing::info!(
                %track_id,
                mid = %slot.mid,
                kind = ?state.track.meta.kind,
                "Assigned track to slot"
            );
        }
    }

    fn perform_unassignment(tracks: &mut HashMap<Arc<TrackId>, TrackState>, slot: &mut SlotState) {
        if let Some(track_id) = slot.assigned_track.take() {
            if let Some(state) = tracks.get_mut(&track_id) {
                state.assigned_mid = None;
                state.update(|c| {
                    c.paused = true;
                });
                tracing::info!(
                    %track_id,
                    mid = %slot.mid,
                    kind = ?state.track.meta.kind,
                    "Unassigned track from slot"
                );
            }
        }
    }

    fn find_default_rid(simulcast: &[SimulcastReceiver]) -> Option<Rid> {
        simulcast
            .iter()
            .find(|s| s.rid.is_none() || s.rid.unwrap().starts_with('q'))
            .and_then(|s| s.rid)
    }
}

impl Stream for DownstreamAllocator {
    type Item = SlotStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.streams.poll_next_unpin(cx)
    }
}

/// Used for comparing and sorting active speakers.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AudioLevel(i32);

/// Tracks the allocation and control state for one published track.
#[derive(Debug)]
struct TrackState {
    track: TrackReceiver,
    control_tx: watch::Sender<StreamConfig>,
    audio_level: AudioLevel,
    assigned_mid: Option<Mid>,
}

impl TrackState {
    fn update<F>(&self, update_fn: F)
    where
        F: FnOnce(&mut StreamConfig),
    {
        self.control_tx.send_if_modified(|config| {
            let old = *config;
            update_fn(config);

            old != *config
        });
    }

    fn current(&self) -> StreamConfig {
        *self.control_tx.borrow()
    }

    fn request_keyframe(&self, kind: KeyframeRequestKind) {
        let config = self.current();
        if config.paused {
            return;
        }

        if let Some(receiver) = self
            .track
            .simulcast
            .iter()
            .find(|r| r.rid == config.target_rid)
        {
            receiver.request_keyframe(kind);
        }
    }
}

pub struct Slot {
    mid: Mid,
    kind: MediaKind,

    priority: u32,
    paused: bool,
    active: Option<SimulcastReceiver>,
    stage: Option<SimulcastReceiver>,

    switcher: Switcher,
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
            paused: true,
            active: None,
            stage: None,
            switcher: Switcher::new(clock_rate),
        }
    }
}

type SlotStreamItem = (Mid, RtpPacket);
type SlotStream = Pin<Box<dyn Stream<Item = SlotStreamItem> + Send>>;
impl Stream for Slot {
    type Item = SlotStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.paused {
                self.switcher.drain();
                return Poll::Pending;
            }

            if let Some(pkt) = self.switcher.pop() {
                return Poll::Ready(Some((self.mid, pkt)));
            }

            if let Some(stream) = self.stage.as_mut() {
                match stream.channel.poll_recv(cx) {
                    Poll::Ready(Ok(pkt)) => {
                        self.switcher.stage(pkt.value.clone());
                        if self.switcher.is_ready() {
                            self.active = self.stage.take();
                        }
                        continue;
                    }
                    Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                        tracing::warn!("lagged by {} packets, pausing the stream", n);
                        self.stage = None;
                    }
                    Poll::Ready(Err(spmc::RecvError::Closed)) => {
                        self.stage = None;
                    }
                    Poll::Pending => {
                        // explicit pass through, let active stream to continue
                    }
                }
            }

            let stream = self.active.as_mut().unwrap();
            match ready!(stream.channel.poll_recv(cx)) {
                Ok(pkt) => {
                    self.switcher.push(pkt.value.clone());
                    continue;
                }
                Err(spmc::RecvError::Lagged(n)) => {
                    tracing::warn!("lagged by {} packets, pausing the stream", n);
                    self.paused = true;
                    continue;
                }
                Err(spmc::RecvError::Closed) => {
                    self.paused = true;
                    continue;
                }
            }
        }
    }
}
