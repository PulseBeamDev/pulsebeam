use crate::participant::bitrate::BitrateController;
use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use futures::stream::{SelectAll, Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use str0m::rtp::RtpHeader;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackReceiver};

/// The event produced by the DownstreamAllocator stream.
///
/// It includes the packet and a boolean marker to indicate if this is the first
/// packet after a simulcast layer switch.
type TrackStreamItem = (Arc<TrackId>, RtpPacket);
type TrackStream = Pin<Box<dyn Stream<Item = TrackStreamItem> + Send>>;

/// Configuration that each downstream receiver listens to.
/// Updated by the allocator to select which simulcast layer is active.
#[derive(Debug, Copy, Clone, PartialEq)]
struct StreamConfig {
    paused: bool,
    target_rid: Option<Rid>,
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

/// Per-subscriber video slot state.
#[derive(Debug)]
struct SlotState {
    mid: Mid,
    assigned_track: Option<Arc<TrackId>>,
    // video=max_height
    priority: u32,
    switcher: Switcher,
}

pub struct DownstreamAllocator {
    streams: SelectAll<TrackStream>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<SlotState>,
    video_slots: Vec<SlotState>,
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

        let initial_config = StreamConfig {
            paused: true,
            target_rid: Self::find_default_rid(&track.simulcast),
        };

        let (control_tx, control_rx) = watch::channel(initial_config);
        self.streams
            .push(Self::create_track_stream(track.clone(), control_rx));

        self.tracks.insert(
            track.meta.id.clone(),
            TrackState {
                track,
                control_tx,
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

    fn create_track_stream(
        track: TrackReceiver,
        control_rx: watch::Receiver<StreamConfig>,
    ) -> TrackStream {
        async_stream::stream! {
            let mut reader = TrackReader::new(track, control_rx);
            while let Some(item) = reader.next_packet().await {
                yield item;
            }
        }
        .boxed()
    }
}

impl Stream for DownstreamAllocator {
    type Item = TrackStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.streams.poll_next_unpin(cx)
    }
}

/// The internal state of the TrackReader state machine.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TrackReaderState {
    /// The stream is paused and waiting for a new configuration.
    Paused,
    /// Actively streaming from a single, stable layer.
    Streaming { active_index: usize },
    /// Attempting to switch to a new layer (`active_index`) while forwarding packets
    /// from the old layer (`fallback_index`) until the new one becomes active.
    Transitioning {
        active_index: usize,
        fallback_index: usize,
    },
}

/// Manages reading RTP packets from a TrackReceiver with dynamic layer switching.
pub struct TrackReader {
    id: Arc<TrackId>,
    track: TrackReceiver,
    control_rx: watch::Receiver<StreamConfig>,

    state: TrackReaderState,
    config: StreamConfig,
    switcher: Switcher,
}

impl TrackReader {
    fn new(track: TrackReceiver, mut control_rx: watch::Receiver<StreamConfig>) -> Self {
        let config = *control_rx.borrow_and_update();
        let clock_rate = match track.meta.kind {
            MediaKind::Audio => rtp::AUDIO_FREQUENCY,
            MediaKind::Video => rtp::VIDEO_FREQUENCY,
        };
        let switcher = Switcher::new(clock_rate);
        let mut this = Self {
            id: track.meta.id.clone(),
            track,
            control_rx,
            state: TrackReaderState::Paused,
            config,
            switcher,
        };
        // seed initial state
        this.update_state(config);
        this
    }

    /// Poll for the next RTP packet, handling fallback and layer switching.
    pub async fn next_packet(&mut self) -> Option<TrackStreamItem> {
        loop {
            self.maybe_update_state();
            if let Some(pkt) = self.switcher.pop() {
                return Some((self.id.clone(), pkt));
            }

            match self.state {
                TrackReaderState::Paused => {
                    // While paused, the only thing we can do is wait for the configuration to change.
                    if self.control_rx.changed().await.is_err() {
                        return None;
                    }
                }
                TrackReaderState::Streaming { active_index } => {
                    let receiver = &mut self.track.simulcast[active_index];
                    tokio::select! {
                        biased;

                        res = self.control_rx.changed() => {
                            if res.is_err() { return None; }
                            continue; // Re-evaluate state on next loop iteration
                        },

                        res = receiver.channel.recv() => {
                            match res {
                                Ok(pkt) => {
                                    self.switcher.push(pkt.value.clone());
                                },
                                Err(spmc::RecvError::Lagged(n)) => {
                                    tracing::warn!(track_id = %self.id, "Receiver lagged {n}, requesting keyframe");
                                    receiver.request_keyframe(str0m::media::KeyframeRequestKind::Pli);
                                }
                                Err(spmc::RecvError::Closed) => {
                                    tracing::warn!(track_id = %self.id, "Channel closed, ending stream");
                                    return None;
                                }
                            }
                        }
                    }
                }
                TrackReaderState::Transitioning {
                    active_index,
                    fallback_index,
                } => {
                    let (active_receiver, fallback_receiver) = Self::split_receivers(
                        &mut self.track.simulcast,
                        active_index,
                        fallback_index,
                    );

                    tokio::select! {
                        biased;

                        res = self.control_rx.changed() => {
                            if res.is_err() { return None; }
                            continue; // Re-evaluate state
                        },

                        // The first packet from the new active layer completes the switch.
                        res = active_receiver.channel.recv() => {
                            match res {
                                Ok(pkt) => {
                                    self.switcher.push(pkt.value.clone());
                                }
                                Err(spmc::RecvError::Lagged(n)) => {
                                    tracing::warn!(track_id = %self.id, "New active stream lagged {n}, completing switch anyway");
                                    active_receiver.request_keyframe(KeyframeRequestKind::Pli);
                                    self.state = TrackReaderState::Streaming { active_index }; // Commit to the switch
                                }
                                Err(spmc::RecvError::Closed) => {
                                    tracing::warn!(track_id = %self.id, "New active stream closed, reverting to fallback");
                                    self.state = TrackReaderState::Streaming { active_index: fallback_index };
                                }
                            }
                        },

                        // Continue forwarding the old stream until the new one is ready.
                        res = fallback_receiver.channel.recv() => {
                            match res {
                                Ok(pkt) => {
                                    tracing::trace!(track_id = %self.id, "Forwarding fallback packet during transition");
                                    self.switcher.push(pkt.value.clone());
                                }
                                Err(spmc::RecvError::Lagged(n)) => {
                                    tracing::warn!(track_id = %self.id, "Fallback stream lagged by {n} packets during transition. Committing to active stream.");
                                    self.state = TrackReaderState::Streaming { active_index };
                                }
                                Err(spmc::RecvError::Closed) => {
                                    tracing::debug!(track_id = %self.id, "Fallback stream closed during transition. Committing to active stream.");
                                    self.state = TrackReaderState::Streaming { active_index };
                                }
                            }
                        }
                    }

                    if self.switcher.is_stable() {
                        self.state = TrackReaderState::Streaming { active_index };
                    }
                }
            }
        }
    }

    /// Checks for a new stream configuration and updates the state machine accordingly.
    fn maybe_update_state(&mut self) {
        let current = *self.control_rx.borrow_and_update();
        if current == self.config {
            return;
        }

        self.update_state(current);
    }

    fn update_state(&mut self, current: StreamConfig) {
        self.config = current;
        tracing::debug!(track_id = %self.id, "Applying new stream config: {current:?}");

        let new_active_index = if current.paused {
            None
        } else {
            self.track
                .simulcast
                .iter()
                .position(|s| s.rid == current.target_rid)
        };

        // Determine the next state based on the current state and the new config.
        let next_state = match self.state {
            TrackReaderState::Paused => new_active_index
                .map(|idx| TrackReaderState::Streaming { active_index: idx })
                .unwrap_or(TrackReaderState::Paused),
            TrackReaderState::Streaming { active_index } => {
                match new_active_index {
                    Some(new_idx) if new_idx != active_index => TrackReaderState::Transitioning {
                        active_index: new_idx,
                        fallback_index: active_index,
                    },
                    Some(_) => self.state, // No change
                    None => TrackReaderState::Paused,
                }
            }
            TrackReaderState::Transitioning {
                active_index,
                fallback_index,
            } => match new_active_index {
                Some(new_idx) if new_idx != active_index && new_idx != fallback_index => {
                    TrackReaderState::Transitioning {
                        active_index: new_idx,
                        fallback_index, // Keep the original fallback
                    }
                }
                Some(_) => self.state, // No change
                None => TrackReaderState::Paused,
            },
        };

        if self.state != next_state {
            tracing::debug!(track_id = %self.id, "TrackReader state changed from {:?} to {:?}", self.state, next_state);

            match (self.state, next_state) {
                (
                    _,
                    TrackReaderState::Transitioning {
                        active_index,
                        fallback_index,
                    },
                ) => {
                    let fallback_receiver_rid = { self.track.simulcast[fallback_index].rid };
                    let active_receiver = &mut self.track.simulcast[active_index];
                    active_receiver.channel.reset();
                    active_receiver.request_keyframe(KeyframeRequestKind::Fir);
                    tracing::info!(track_id = %self.id,
                        "switch simulcast layer: {:?} -> {:?}",
                        fallback_receiver_rid, active_receiver.rid
                    );
                }
                (TrackReaderState::Paused, TrackReaderState::Streaming { active_index }) => {
                    let active_receiver = &mut self.track.simulcast[active_index];
                    active_receiver.channel.reset();
                    active_receiver.request_keyframe(KeyframeRequestKind::Fir);
                    tracing::info!(track_id = %self.id,
                        "resuming simulcast layer: {:?}",
                        active_receiver.rid
                    );
                }
                _ => {}
            }
            self.state = next_state;
        }
    }

    /// Safely gets mutable references to two different elements in a slice.
    fn split_receivers(
        simulcast: &mut [SimulcastReceiver],
        a: usize,
        b: usize,
    ) -> (&mut SimulcastReceiver, &mut SimulcastReceiver) {
        assert!(a != b);
        if a < b {
            let (left, right) = simulcast.split_at_mut(b);
            (&mut left[a], &mut right[0])
        } else {
            let (left, right) = simulcast.split_at_mut(a);
            (&mut right[0], &mut left[b])
        }
    }
}
