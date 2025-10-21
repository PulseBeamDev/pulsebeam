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

        // Find the SimulcastReceiver for the currently active RID.
        if let Some(receiver) = self
            .track
            .simulcast
            .iter()
            .find(|r| r.rid == config.target_rid)
        {
            // Call the keyframe request method on the receiver itself,
            // which will signal the upstream track.
            receiver.request_keyframe();
        }
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
    last_switch_at: Option<Instant>,
    last_assigned_bitrate: Option<f64>,
}

/// The main downstream allocator that manages multiple tracks and media slots.
/// It consumes RTP packets from publishers and redistributes them to subscribers
/// based on available bandwidth and per-subscriber constraints.
pub struct DownstreamAllocator {
    streams: SelectAll<TrackStream>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    audio_slots: Vec<MidSlot>,
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
                    last_switch_at: None,
                    last_assigned_bitrate: None,
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
            desired_bitrate += bitrate;
        }

        desired_bitrate
    }

    pub fn handle_bwe(&mut self, bwe: BweKind) -> Option<u64> {
        let BweKind::Twcc(available_bandwidth) = bwe else {
            return None;
        };
        let new_bwe = available_bandwidth.as_f64();

        // 1. Smooth the bandwidth estimate to absorb temporary fluctuations.
        // This makes the system resilient to both noisy estimates and periods with no feedback.
        let smoothed_bwe = match self.smoothed_bwe_bps {
            Some(prev) => prev * 0.85 + new_bwe * 0.15,
            None => new_bwe,
        };
        self.smoothed_bwe_bps = Some(smoothed_bwe);

        // 2. Allocate based on a conservative portion of the stable, smoothed estimate.
        let budget = smoothed_bwe * 0.90;
        let mut total_allocated_bitrate = 0.0;
        let now = Instant::now();

        const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(8);
        const UPGRADE_HEADROOM: f64 = 1.25; // Require 25% headroom to upgrade.

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

            // Find the highest-quality layer that fits within the remaining budget.
            let best_fitting_layer = state
                .track
                .simulcast
                .iter()
                .find(|l| (l.bitrate.load(Ordering::Relaxed) as f64) <= remaining_budget);

            if state.current().paused {
                // Try to resume if a suitable layer is found.
                if let Some(layer) = best_fitting_layer {
                    new_paused = false;
                    new_rid = layer.rid;
                    new_bitrate = layer.bitrate.load(Ordering::Relaxed) as f64;
                }
            } else {
                // Stream is active. Check if it's over budget.
                if prev_bitrate > remaining_budget {
                    // Must downgrade or pause to what fits.
                    if let Some(layer) = best_fitting_layer {
                        new_rid = layer.rid;
                        new_bitrate = layer.bitrate.load(Ordering::Relaxed) as f64;
                    } else {
                        new_paused = true;
                        new_bitrate = 0.0;
                    }
                } else if can_switch {
                    // Consider an upgrade if there's enough headroom.
                    let upgrade_budget = remaining_budget / UPGRADE_HEADROOM;
                    if let Some(layer) = state.track.simulcast.iter().find(|l| {
                        let bitrate = l.bitrate.load(Ordering::Relaxed) as f64;
                        bitrate > prev_bitrate && bitrate <= upgrade_budget
                    }) {
                        new_rid = layer.rid;
                        new_bitrate = layer.bitrate.load(Ordering::Relaxed) as f64;
                    }
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
        tracing::debug!(
            "BWE: utilization={:.2}%, available={:.0}, smoothed={:.0}, allocated={:.0}",
            utilization * 100.0,
            new_bwe,
            smoothed_bwe,
            total_allocated_bitrate,
        );

        Some(total_allocated_bitrate as u64)
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
        // bwe handling will play the streams
        if let Some(state) = tracks.get_mut(track_id) {
            slot.assigned_track = Some(track_id.clone());
            state.assigned_mid = Some(slot.mid);
            state.update(|c| {
                let changed = c.paused != false;
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

    fn perform_unassignment(tracks: &mut HashMap<Arc<TrackId>, TrackState>, slot: &mut MidSlot) {
        if let Some(track_id) = slot.assigned_track.take() {
            if let Some(state) = tracks.get_mut(&track_id) {
                state.assigned_mid = None;
                state.update(|c| {
                    let changed = c.paused != true;
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
        mut track: TrackReceiver,
        mut rx: watch::Receiver<StreamConfig>,
    ) -> TrackStream {
        async_stream::stream! {
            let meta = track.meta.clone();
            let find_index_for_rid = |rid: Option<Rid>, simulcast: &[SimulcastReceiver]| -> Option<usize> {
                simulcast.iter().position(|s| s.rid == rid)
            };

            let mut local_generation = u64::MAX;
            let mut active_index: Option<usize> = None;
            // let mut waiting_keyframe = true;

            loop {
                // Get the latest configuration.
                let config = *rx.borrow();

                // Check if the configuration has changed since the last time we checked.
                if local_generation != config.generation {
                    local_generation = config.generation;
                    tracing::debug!(?track.meta.id, "Track state applied: {config:?}");

                    if config.paused {
                        active_index = None;
                    } else if let Some(new_index) = find_index_for_rid(config.target_rid, &track.simulcast) {
                        // The RID is valid, so we have a new active layer.
                        active_index = Some(new_index);
                        // A layer switch requires a new keyframe.
                        let receiver = &mut track.simulcast[new_index];
                        receiver.channel.reset();
                        receiver.request_keyframe();
                        // waiting_keyframe = true;
                    } else {
                        // The RID in the config is invalid, so we must pause until we get a valid one.
                        active_index = None;
                        tracing::warn!(?track.meta.id, ?config.target_rid, "Invalid target_rid, pausing stream");
                    }
                }
                
                // If we are paused or have an invalid RID, we have no active layer.
                // In this state, our only job is to wait for the next configuration change.
                if active_index.is_none() {
                    if rx.changed().await.is_err() {
                        break; // Channel closed, end the stream.
                    }
                    continue; // Loop again to process the new config.
                }

                // If we are here, we have an active layer to stream from.
                let receiver = &mut track.simulcast[active_index.unwrap()];

                // We must simultaneously listen for new packets and for the next config change.
                tokio::select! {
                    biased;

                    // Branch 1: The configuration changed again.
                    res = rx.changed() => {
                        if res.is_err() { break; }
                        // Do nothing here. The loop will restart and the logic at the top
                        // will handle applying the new configuration.
                    },

                    // Branch 2: A new RTP packet arrived on the active layer.
                    res = receiver.channel.recv() => {
                        match res {
                            Ok(pkt) => {
                                // if waiting_keyframe && !is_keyframe(&pkt.value) {
                                //     continue;
                                // }
                                // waiting_keyframe = false;
                                yield (meta.clone(), pkt)
                            },
                            Err(spmc::RecvError::Lagged(count)) => {
                                tracing::warn!(track_id = %meta.id, rid = ?receiver.rid, count, "Receiver lagged, requesting keyframe for recovery");
                                receiver.request_keyframe();
                            },
                            Err(spmc::RecvError::Closed) => {
                                tracing::warn!(track_id = %meta.id, rid = ?receiver.rid, "Stopping stream; channel closed");
                                break;
                            },
                        }
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

/// Heuristically detect whether this RTP packet appears to start a full intra (keyframe).
/// 
/// Notes:
/// - Stateless: only inspects this RTP packet.
/// - For H.264, detects IDR slices or the start fragment (FU-A with NAL type 5).
/// - For VP8/VP9, checks the "keyframe" and "start of frame" bits.
/// - Out-of-order packets are fine: detection is opportunistic, not sequential.
/// 
/// Reference specs:
/// - H.264: RFC 6184 §5.2
/// - VP8:  RFC 7741 §4.2
/// - VP9:  W3C WebRTC VP9 RTP Payload Format
pub fn is_keyframe(rtp: &RtpPacket) -> bool {
    let payload = &rtp.payload;
    if payload.is_empty() {
        return false;
    }

    // You might have a payload type map if you know what codec is in use.
    // But most WebRTC or RTP demuxers heuristically detect codec by payload content.
    if looks_like_h264(payload) {
        return is_h264_keyframe_start(payload);
    }

    if looks_like_vp8(payload) {
        return is_vp8_keyframe_start(payload);
    }

    if looks_like_vp9(payload) {
        return is_vp9_keyframe_start(payload);
    }

    false
}

/// Roughly guess H.264 by looking for NALU start patterns.
fn looks_like_h264(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let nal_type = payload[0] & 0x1F;
    (1..=23).contains(&nal_type) || nal_type == 28
}

/// Detect if this H.264 RTP packet starts a keyframe (IDR).
fn is_h264_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let nal_type = payload[0] & 0x1F;

    match nal_type {
        5 => true, // IDR slice (standalone)
        28 => {
            // Fragmentation Unit (FU-A)
            if payload.len() < 2 {
                return false;
            }
            let fu_header = payload[1];
            let start_bit = fu_header & 0x80 != 0;
            let frag_nal_type = fu_header & 0x1F;
            start_bit && frag_nal_type == 5
        }
        _ => false,
    }
}

/// Rough VP8 detection — first byte pattern with 0x10 (start of partition).
fn looks_like_vp8(payload: &[u8]) -> bool {
    let b0 = payload[0];
    // For VP8, bits 0..2 are usually 0b000 or 0b001 for normal streams.
    b0 & 0b1110_0000 == 0b0000_0000 || b0 & 0b1110_0000 == 0b0010_0000
}

/// Detect if this VP8 RTP packet appears to start a keyframe.
///
/// VP8 RTP Payload (RFC 7741):
/// - Bit 0 (P): 0 = keyframe, 1 = interframe.
/// - Bit 4..7 (PartID): 0 means start of a new frame.
/// - Optional X field may follow.
fn is_vp8_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let b0 = payload[0];
    let x_bit = (b0 & 0x80) != 0;
    let p_bit = (b0 & 0x01) != 0; // 0 = keyframe
    let part_id = (b0 >> 4) & 0x0F;
    let start_of_frame = part_id == 0;

    if !start_of_frame || p_bit {
        return false;
    }

    // Optionally skip extended control fields (rare but valid)
    let mut idx = 1;
    if x_bit && payload.len() > 1 {
        let x_field = payload[idx];
        idx += 1;
        if x_field & 0x80 != 0 {
            // PictureID
            if idx >= payload.len() {
                return false;
            }
            let picid = payload[idx];
            idx += 1;
            if picid & 0x80 != 0 {
                idx += 1; // long PictureID
            }
        }
        if x_field & 0x40 != 0 {
            idx += 1; // TL0PICIDX
        }
        if x_field & 0x20 != 0 {
            idx += 1; // TID/Y/KEYIDX
        }
    }

    // If we reached here, it's the first packet of a keyframe
    true
}

/// Rough VP9 detection — first byte often starts with I|P|L|F|B|E|V|Z bits.
fn looks_like_vp9(payload: &[u8]) -> bool {
    let b0 = payload[0];
    // I bit is MSB; P bit next. B bit marks beginning of frame.
    (b0 & 0xE0) == 0x80 || (b0 & 0xC0) == 0x00
}

/// Detect if this VP9 RTP packet appears to start a keyframe.
///
/// VP9 RTP Payload:
/// - I bit (MSB): 1 if PictureID present
/// - P bit: 0 = keyframe, 1 = interframe
/// - B bit: 1 = beginning of frame
fn is_vp9_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let b0 = payload[0];
    let b_bit = (b0 & 0x08) != 0; // beginning of frame
    let p_bit = (b0 & 0x40) != 0; // inter-frame indicator (1 = inter, 0 = keyframe)

    b_bit && !p_bit
}

