use crate::rtp::rtp_rewriter::RtpRewriter;
use crate::rtp::{RtpPacket, TimingHeader};
use futures::stream::{SelectAll, Stream, StreamExt};
use pulsebeam_runtime::sync::spmc::{self, Slot};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use str0m::bwe::{Bitrate, BweKind};
use str0m::media::{KeyframeRequest, MediaKind, Mid, Rid};
use str0m::rtp::RtpHeader;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackMeta, TrackReceiver};

/// The event produced by the DownstreamAllocator stream.
///
/// It includes the packet and a boolean marker to indicate if this is the first
/// packet after a simulcast layer switch.
type TrackStreamItem = (Arc<TrackMeta>, TimingHeader, Arc<Slot<RtpPacket>>);
type TrackStream = Pin<Box<dyn Stream<Item = TrackStreamItem> + Send>>;

/// Configuration that each downstream receiver listens to.
/// Updated by the allocator to select which simulcast layer is active.
#[derive(Debug, Copy, Clone, PartialEq)]
struct StreamConfig {
    paused: bool,
    target_rid: Option<Rid>,
    generation: u64,
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
        F: FnOnce(&mut StreamConfig) -> bool,
    {
        self.control_tx.send_if_modified(|config| {
            let changed = update_fn(config);
            if changed {
                config.generation = config.generation.wrapping_add(1);
                tracing::debug!(?self.track.meta.id, "track state updated: {config:?}");
                true
            } else {
                false
            }
        });
    }

    fn current(&self) -> StreamConfig {
        *self.control_tx.borrow()
    }

    fn request_keyframe(&self) {
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
            receiver.request_keyframe();
        }
    }
}

/// Represents an active media slot in an SDP session.
struct AudioSlotState {
    mid: Mid,
    assigned_track: Option<Arc<TrackId>>,
}

/// Per-subscriber video slot state.
#[derive(Debug)]
struct VideoSlotState {
    mid: Mid,
    assigned_track: Option<Arc<TrackId>>,
    max_height: u32,
    last_switch_at: Option<Instant>,
    last_assigned_bitrate: Option<f64>,
}

pub struct DownstreamAllocator {
    streams: SelectAll<TrackStream>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<AudioSlotState>,
    video_slots: Vec<VideoSlotState>,
    smoothed_bwe_bps: Option<f64>,
}

impl DownstreamAllocator {
    pub fn new() -> Self {
        Self {
            streams: SelectAll::new(),
            tracks: HashMap::new(),
            audio_slots: Vec::new(),
            video_slots: Vec::new(),
            smoothed_bwe_bps: None,
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }

        let initial_config = StreamConfig {
            paused: true,
            target_rid: Self::find_default_rid(&track.simulcast),
            generation: 0,
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
                self.audio_slots.push(AudioSlotState {
                    mid,
                    assigned_track: None,
                });
            }
            MediaKind::Video => {
                self.video_slots.push(VideoSlotState {
                    mid,
                    assigned_track: None,
                    max_height: 720,
                    last_switch_at: None,
                    last_assigned_bitrate: None,
                });
            }
        }
        self.rebalance_allocations();
    }

    pub fn set_slot_max_height(&mut self, mid: Mid, max_height: u32) {
        if let Some(slot) = self.video_slots.iter_mut().find(|s| s.mid == mid) {
            slot.max_height = max_height;
        }
    }

    pub fn desired_bitrate(&self) -> u64 {
        let mut desired_bitrate = 0;
        for slot in &self.video_slots {
            let Some(track_id) = &slot.assigned_track else {
                continue;
            };
            let Some(state) = self.tracks.get(track_id) else {
                continue;
            };

            let Some(layer) = state.track.simulcast.first() else {
                continue;
            };

            desired_bitrate += layer.estimated_bitrate();
        }
        desired_bitrate
    }

    pub fn handle_bwe(&mut self, bwe: BweKind) -> Option<Bitrate> {
        let BweKind::Twcc(available_bandwidth) = bwe else {
            return None;
        };

        let now = Instant::now();
        let new_bwe = available_bandwidth.as_f64();

        let smoothed_bwe = match self.smoothed_bwe_bps {
            Some(prev) => prev * 0.85 + new_bwe * 0.15,
            None => new_bwe,
        };
        self.smoothed_bwe_bps = Some(smoothed_bwe);

        let budget = smoothed_bwe * 0.90;
        let mut total_allocated_bitrate = 0.0;

        const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(8);
        const UPGRADE_HEADROOM: f64 = 1.25;

        self.video_slots
            .sort_by(|a, b| b.max_height.cmp(&a.max_height));

        for slot in &mut self.video_slots {
            let Some(track_id) = &slot.assigned_track else {
                continue;
            };
            let Some(state) = self.tracks.get(track_id) else {
                continue;
            };

            let remaining_budget = budget - total_allocated_bitrate;
            let prev_bitrate = slot.last_assigned_bitrate.unwrap_or(0.0);
            let can_switch = slot
                .last_switch_at
                .is_none_or(|t| now.duration_since(t) > MIN_SWITCH_INTERVAL);

            let mut new_rid = state.current().target_rid;
            let mut new_bitrate = prev_bitrate;
            let mut new_paused = state.current().paused;

            let best_fitting_layer = state
                .track
                .simulcast
                .iter()
                .find(|l| !l.is_paused() && (l.estimated_bitrate() as f64) <= remaining_budget);

            if state.current().paused {
                if let Some(layer) = best_fitting_layer {
                    new_paused = false;
                    new_rid = layer.rid;
                    new_bitrate = layer.estimated_bitrate() as f64;
                }
            } else if prev_bitrate > remaining_budget {
                if let Some(layer) = best_fitting_layer {
                    new_rid = layer.rid;
                    new_bitrate = layer.estimated_bitrate() as f64;
                } else {
                    new_paused = true;
                    new_bitrate = 0.0;
                }
            } else if can_switch {
                let upgrade_budget = remaining_budget / UPGRADE_HEADROOM;
                if let Some(layer) = state.track.simulcast.iter().find(|l| {
                    let bitrate = l.estimated_bitrate() as f64;
                    !l.is_paused() && bitrate > prev_bitrate && bitrate <= upgrade_budget
                }) {
                    new_rid = layer.rid;
                    new_bitrate = layer.estimated_bitrate() as f64;
                }
            }

            if new_paused != state.current().paused || new_rid != state.current().target_rid {
                state.update(|c| {
                    let changed = c.paused != new_paused || c.target_rid != new_rid;
                    c.paused = new_paused;
                    c.target_rid = new_rid;
                    changed
                });
                slot.last_switch_at = Some(now);
            }

            slot.last_assigned_bitrate = Some(new_bitrate);
            total_allocated_bitrate += new_bitrate;
        }

        let utilization = total_allocated_bitrate / smoothed_bwe;
        let total_allocated_bitrate = Bitrate::from(total_allocated_bitrate);
        tracing::debug!(
            "BWE: utilization={:.2}%, available={}, smoothed={}, allocated={}",
            utilization * 100.0,
            Bitrate::from(new_bwe),
            Bitrate::from(smoothed_bwe),
            total_allocated_bitrate,
        );

        Some(total_allocated_bitrate)
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
        state.request_keyframe();
    }

    pub fn handle_rtp(&mut self, meta: &Arc<TrackMeta>, hdr: &RtpHeader) -> Option<Mid> {
        let assigned_mid = self.tracks.get(&meta.id).and_then(|s| s.assigned_mid);

        let mut needs_rebalance = false;
        if let Some(track_state) = self.tracks.get_mut(&meta.id) {
            if meta.kind == MediaKind::Audio {
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
                    let mut tmp = AudioSlotState {
                        mid: slot.mid,
                        assigned_track: slot.assigned_track.clone(),
                    };
                    Self::perform_unassignment(&mut self.tracks, &mut tmp);
                    slot.assigned_track = None;
                }
            }
            if slot.assigned_track.is_none() {
                if let Some(track_id) = unassigned_tracks.pop() {
                    let mut tmp = AudioSlotState {
                        mid: slot.mid,
                        assigned_track: None,
                    };
                    Self::perform_assignment(&mut self.tracks, &mut tmp, &track_id);
                    slot.assigned_track = Some(track_id);
                }
            }
        }
    }

    fn perform_assignment(
        tracks: &mut HashMap<Arc<TrackId>, TrackState>,
        slot: &mut AudioSlotState,
        track_id: &Arc<TrackId>,
    ) {
        if let Some(state) = tracks.get_mut(track_id) {
            slot.assigned_track = Some(track_id.clone());
            state.assigned_mid = Some(slot.mid);
            state.update(|c| {
                let changed = c.paused;
                c.paused = false;
                changed
            });
            tracing::info!(
                %track_id,
                mid = %slot.mid,
                kind = ?state.track.meta.kind,
                "Assigned track to slot"
            );
        }
    }

    fn perform_unassignment(
        tracks: &mut HashMap<Arc<TrackId>, TrackState>,
        slot: &mut AudioSlotState,
    ) {
        if let Some(track_id) = slot.assigned_track.take() {
            if let Some(state) = tracks.get_mut(&track_id) {
                state.assigned_mid = None;
                state.update(|c| {
                    let changed = !c.paused;
                    c.paused = true;
                    changed
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
    meta: Arc<TrackMeta>,
    track: TrackReceiver,
    control_rx: watch::Receiver<StreamConfig>,

    state: TrackReaderState,
    local_generation: u64,
    rewriter: RtpRewriter,
}

impl TrackReader {
    fn new(track: TrackReceiver, control_rx: watch::Receiver<StreamConfig>) -> Self {
        Self {
            meta: track.meta.clone(),
            track,
            control_rx,
            state: TrackReaderState::Paused,
            local_generation: u64::MAX,
            rewriter: RtpRewriter::new(),
        }
    }

    /// Poll for the next RTP packet, handling fallback and layer switching.
    pub async fn next_packet(&mut self) -> Option<TrackStreamItem> {
        loop {
            self.maybe_update_state();

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
                                    if let Some(pkt) = self.rewrite(pkt, false) {
                                        return Some(pkt);
                                    }
                                },
                                Err(spmc::RecvError::Lagged(n)) => {
                                    tracing::warn!(track_id = %self.meta.id, "Receiver lagged {n}, requesting keyframe");
                                    receiver.request_keyframe();
                                }
                                Err(spmc::RecvError::Closed) => {
                                    tracing::warn!(track_id = %self.meta.id, "Channel closed, ending stream");
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
                                    tracing::debug!(track_id = %self.meta.id, "First packet from new layer received, switch complete");
                                    self.state = TrackReaderState::Streaming { active_index };
                                    if let Some(pkt) = self.rewrite(pkt, true) {
                                        return Some(pkt);
                                    }
                                }
                                Err(spmc::RecvError::Lagged(n)) => {
                                    tracing::warn!(track_id = %self.meta.id, "New active stream lagged {n}, completing switch anyway");
                                    active_receiver.request_keyframe();
                                    self.state = TrackReaderState::Streaming { active_index }; // Commit to the switch
                                }
                                Err(spmc::RecvError::Closed) => {
                                    tracing::warn!(track_id = %self.meta.id, "New active stream closed, reverting to fallback");
                                    self.state = TrackReaderState::Streaming { active_index: fallback_index };
                                }
                            }
                        },

                        // Continue forwarding the old stream until the new one is ready.
                        res = fallback_receiver.channel.recv() => {
                            match res {
                                Ok(pkt) => {
                                    tracing::trace!(track_id = %self.meta.id, "Forwarding fallback packet during transition");
                                    if let Some(pkt) = self.rewrite(pkt, false) {
                                        return Some(pkt);
                                    }
                                }
                                Err(_) => {
                                    // Fallback stream ended. We must commit to the new active stream.
                                    self.state = TrackReaderState::Streaming { active_index };
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Checks for a new stream configuration and updates the state machine accordingly.
    fn maybe_update_state(&mut self) {
        if self.control_rx.borrow().generation == self.local_generation {
            return;
        }

        let config = *self.control_rx.borrow();
        self.local_generation = config.generation;
        tracing::debug!(track_id = %self.meta.id, "Applying new stream config: {config:?}");

        let new_active_index = if config.paused {
            None
        } else {
            self.track
                .simulcast
                .iter()
                .position(|s| s.rid == config.target_rid)
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
                Some(new_idx) if new_idx != active_index => TrackReaderState::Transitioning {
                    active_index: new_idx,
                    fallback_index, // Keep the original fallback
                },
                Some(_) => self.state, // No change
                None => TrackReaderState::Paused,
            },
        };

        if self.state != next_state {
            tracing::debug!(track_id = %self.meta.id, "TrackReader state changed from {:?} to {:?}", self.state, next_state);
            self.state = next_state;

            // If we are starting a transition, request a keyframe on the new target layer.
            if let TrackReaderState::Transitioning { active_index, .. } = self.state {
                let receiver = &mut self.track.simulcast[active_index];
                receiver.channel.reset();
                receiver.request_keyframe();
                tracing::debug!(track_id = %self.meta.id, rid = ?receiver.rid, "Requested keyframe for new active layer");
            }
        }
    }

    /// Rewrites the packet and wraps it for forwarding.
    fn rewrite(&mut self, pkt: Arc<Slot<RtpPacket>>, is_switch: bool) -> Option<TrackStreamItem> {
        let (seq_no, ts) = self.rewriter.rewrite(&pkt.value, is_switch)?;
        let mut hdr: TimingHeader = (&pkt.value).into();
        hdr.rtp_ts = ts;
        hdr.seq_no = seq_no;
        Some((self.meta.clone(), hdr, pkt))
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
