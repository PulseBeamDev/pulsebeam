use futures::stream::{SelectAll, Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use str0m::bwe::BweKind;
use str0m::media::{KeyframeRequest, MediaKind, Mid, Rid};
use str0m::rtp::RtpPacket;
use tokio::sync::watch;

use crate::entity::TrackId;
use crate::track::{SimulcastReceiver, TrackMeta, TrackReceiver};

type TrackStream = Pin<Box<dyn Stream<Item = (Arc<TrackMeta>, Arc<spmc::Slot<RtpPacket>>)> + Send>>;

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

            if old != *config {
                config.generation = config.generation.wrapping_add(1);
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
        self.control_tx.send_if_modified(|config| {
            config.generation = config.generation.wrapping_add(1);
            true
        });
    }
}

/// Represents an active media slot in an SDP session.
/// Audio slots are simple; video slots have adaptive state.
struct MidSlot {
    mid: Mid,
    assigned_track: Option<Arc<TrackId>>,
}

/// Per-subscriber video slot state.
/// Keeps track of target quality, allocation history, and hysteresis timers.
#[derive(Debug, Clone)]
struct VideoSlotState {
    mid: Mid,
    assigned_track: Option<Arc<TrackId>>,
    max_height: u32,
    last_assigned_bitrate: Option<f64>,
    last_rid_switch_at: Option<Instant>,
}

/// The main downstream allocator that manages multiple tracks and media slots.
/// It consumes RTP packets from publishers and redistributes them to subscribers
/// based on available bandwidth and per-subscriber constraints.
pub struct DownstreamAllocator {
    streams: SelectAll<TrackStream>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<MidSlot>,
    video_slots: Vec<VideoSlotState>,
}

impl DownstreamAllocator {
    pub fn new() -> Self {
        Self {
            streams: SelectAll::new(),
            tracks: HashMap::new(),
            audio_slots: Vec::new(),
            video_slots: Vec::new(),
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
                self.audio_slots.push(MidSlot {
                    mid,
                    assigned_track: None,
                });
            }
            MediaKind::Video => {
                self.video_slots.push(VideoSlotState {
                    mid,
                    assigned_track: None,
                    max_height: 720,
                    last_assigned_bitrate: None,
                    last_rid_switch_at: None,
                });
            }
        }
        self.rebalance_allocations();
    }

    /// Allows dynamic client-side adaptation (e.g. when resizing a video element).
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

            // simulcast is already sorted based on highest quality first
            let Some(layer) = state.track.simulcast.first() else {
                continue;
            };

            let bitrate = layer.bitrate.load(Ordering::Relaxed);
            desired_bitrate = bitrate;
        }

        desired_bitrate
    }

    /// Applies TWCC feedback to perform bandwidth allocation across all active video slots.
    /// This implements a greedy bin-packing strategy:
    /// - Slots are sorted by priority (max_height)
    /// - Each slot selects the highest simulcast layer that fits in the remaining bandwidth
    /// - Hysteresis is used to avoid rapid switching
    pub fn handle_bwe(&mut self, bwe: BweKind) -> Option<u64> {
        let BweKind::Twcc(available_bandwidth) = bwe else {
            return None; // only respond to TWCC-based feedback
        };
        let available_bandwidth = available_bandwidth.as_f64();

        tracing::debug!("current downstream bitrate available: {available_bandwidth}");

        // Sort slots by descending priority (higher max_height first)
        self.video_slots
            .sort_by(|a, b| b.max_height.cmp(&a.max_height));

        let mut remaining_bw = available_bandwidth;
        let now = Instant::now();

        for slot in &mut self.video_slots {
            let Some(track_id) = &slot.assigned_track else {
                continue;
            };
            let Some(state) = self.tracks.get(track_id) else {
                continue;
            };

            // Pick the highest bitrate layer that fits in the remaining bandwidth
            let mut chosen = None;
            // "f" -> "h" -> "q"
            for layer in &state.track.simulcast {
                let bitrate = layer.bitrate.load(Ordering::Relaxed) as f64;
                if bitrate <= remaining_bw {
                    chosen = Some((layer.rid, bitrate));
                    break;
                }
            }

            // If nothing fits, pause the track
            let Some((target_rid, bitrate)) = chosen else {
                state.update(|c| c.paused = true);
                slot.last_assigned_bitrate = None;
                continue;
            };

            // Apply amplitude + temporal hysteresis
            let too_soon = slot
                .last_rid_switch_at
                .map(|t| now.duration_since(t) < Duration::from_secs(2))
                .unwrap_or(false);

            let big_jump = match slot.last_assigned_bitrate {
                Some(prev) if bitrate > prev => bitrate > prev * 120.0 / 100.0, // increase >20%
                Some(prev) if bitrate < prev => bitrate < prev * 80.0 / 100.0,  // decrease >20%
                _ => true,
            };

            if !too_soon && big_jump {
                state.update(|c| {
                    c.paused = false;
                    c.target_rid = target_rid;
                });

                slot.last_assigned_bitrate = Some(bitrate);
                slot.last_rid_switch_at = Some(now);
                remaining_bw -= bitrate;

                tracing::info!(
                    mid = %slot.mid,
                    bitrate,
                    rid = ?target_rid,
                    remaining_bw,
                    "updated video allocation"
                );
            } else {
                // keep existing allocation under hysteresis window
                if let Some(prev) = slot.last_assigned_bitrate {
                    remaining_bw -= prev;
                }
                tracing::debug!(
                    mid = %slot.mid,
                    rid = ?state.current().target_rid,
                    "kept current layer (hysteresis active)"
                );
            }
        }

        let total_bitrate = available_bandwidth - remaining_bw;
        tracing::debug!("remaining bandwidth allocation: {remaining_bw}");
        Some(total_bitrate as u64)
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) {
        let Some(slot) = self.video_slots.iter().find(|e| e.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found to handle keyframe request");
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

    pub fn handle_rtp(&mut self, meta: &Arc<TrackMeta>, rtp: &RtpPacket) -> Option<Mid> {
        let assigned_mid = self.tracks.get(&meta.id).and_then(|s| s.assigned_mid);

        let mut needs_rebalance = false;
        if let Some(track_state) = self.tracks.get_mut(&meta.id) {
            if meta.kind == MediaKind::Audio {
                let new_level = rtp.header.ext_vals.audio_level.unwrap_or(-127);
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
                    let mut tmp = MidSlot {
                        mid: slot.mid,
                        assigned_track: slot.assigned_track.clone(),
                    };
                    Self::perform_unassignment(&mut self.tracks, &mut tmp);
                    slot.assigned_track = None;
                }
            }
            if slot.assigned_track.is_none() {
                if let Some(track_id) = unassigned_tracks.pop() {
                    let mut tmp = MidSlot {
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
        slot: &mut MidSlot,
        track_id: &Arc<TrackId>,
    ) {
        if let Some(state) = tracks.get_mut(track_id) {
            slot.assigned_track = Some(track_id.clone());
            state.assigned_mid = Some(slot.mid);
            state.update(|c| c.paused = false);
            tracing::info!(
                %track_id,
                mid = %slot.mid,
                kind = ?state.track.meta.kind,
                "Assigned track to slot"
            );
        }
    }

    fn perform_unassignment(tracks: &mut HashMap<Arc<TrackId>, TrackState>, slot: &mut MidSlot) {
        if let Some(track_id) = slot.assigned_track.take() {
            if let Some(state) = tracks.get_mut(&track_id) {
                state.assigned_mid = None;
                state.update(|c| c.paused = true);
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
            .find(|s| s.rid.is_none() || s.rid.unwrap().starts_with('f'))
            .and_then(|s| s.rid)
    }

    fn create_track_stream(
        mut track: TrackReceiver,
        mut rx: watch::Receiver<StreamConfig>,
    ) -> TrackStream {
        async_stream::stream! {
            let meta = track.meta.clone();
            let find_index_for_rid = |rid: Option<Rid>, simulcast: &[SimulcastReceiver]| -> Option<usize> {
                simulcast.iter().position(|s| s.rid == rid)
            };

            let Some(mut active_index) = find_index_for_rid(rx.borrow().target_rid, &track.simulcast) else {
                return;
            };

            let mut local_generation = 0;
            rx.borrow_and_update();

            loop {
                if rx.borrow().paused {
                    if rx.changed().await.is_err() { break; }
                } else {
                    let receiver = &mut track.simulcast[active_index];
                    tokio::select! {
                        biased;
                        res = rx.changed() => if res.is_err() { break; },
                        res = receiver.channel.recv() => match res {
                            Ok(pkt) => yield (meta.clone(), pkt),
                            Err(spmc::RecvError::Lagged(count)) => {
                                tracing::warn!(track_id = %meta.id, rid = ?receiver.rid, count, "Dropping packets due to receiver lag");
                                receiver.request_keyframe();
                            },
                            Err(spmc::RecvError::Closed) => {
                                tracing::warn!(track_id = %meta.id, rid = ?receiver.rid, "Stopping stream; channel closed");
                                break;
                            },
                        },
                    }
                }
                let config = rx.borrow_and_update();
                if local_generation != config.generation {
                    local_generation = config.generation;
                    if let Some(new_index) = find_index_for_rid(config.target_rid, &track.simulcast) {
                        active_index = new_index;
                    }
                    if !config.paused {
                        let receiver = &mut track.simulcast[active_index];
                        receiver.channel.reset();
                        receiver.request_keyframe();
                    }
                }
            }
        }
        .boxed()
    }
}

impl Stream for DownstreamAllocator {
    type Item = (Arc<TrackMeta>, Arc<spmc::Slot<RtpPacket>>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.streams.poll_next_unpin(cx)
    }
}
