use crate::bitrate::{BitrateController, BitrateControllerConfig};
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::rtp::cache::StreamCache;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use slotmap::{SecondaryMap, SlotMap};
use std::cmp::Ordering;
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::log::{LogCtx, plog_debug, plog_error, plog_info, plog_trace, plog_warn};
use crate::track::{LayerQuality, StreamId, StreamWriter, Track, TrackLayer, TrackMeta};

/// Maximum number of video slots per participant.
const VIDEO_MAX_SLOTS: usize = 25;

/// How long to wait between PLI retries while a slot is in a transition state.
const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_millis(1000);

/// After repeated retries, continue to probe the stream with lower-frequency keep-alives.
const KEYFRAME_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum number of aggressive PLI retries before falling back to keep-alive mode.
const KEYFRAME_MAX_RETRIES: u32 = 5;

/// How long a ready switch waits for the active layer to finish the frame it is
/// midway through.
///
/// A frame's packets arrive back-to-back, so this only ever costs a millisecond
/// or two in the normal case; it is short because a packet that has not turned
/// up by now is late enough that waiting is worse than switching. Whatever the
/// switch leaves behind can still be filled afterwards by the drain tail.
const FRAME_ALIGN_GRACE: Duration = Duration::from_millis(15);

pub const MIN_BANDWIDTH: Bitrate = Bitrate::kbps(300);
pub const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);
pub const INITIAL_BANDWIDTH: Bitrate = Bitrate::mbps(2);

slotmap::new_key_type! {
    pub struct SlotKey;
}

pub struct VideoAllocator {
    // Hot
    routes: HashMap<TrackId, SlotKey>,
    slots: SlotMap<SlotKey, Slot>,

    // Cold
    ctx: LogCtx,
    manual_sub: bool,
    tracks: HashMap<TrackId, Track>,
    rng: Rng,
    last_reconciled: HashSet<(TrackId, SlotKey)>,
    desired_ctrl: BitrateController,
}

impl VideoAllocator {
    pub(crate) fn new<R: RngCore>(ctx: LogCtx, manual_sub: bool, rng: &mut R) -> Self {
        let desired_ctrl = BitrateControllerConfig {
            min_bitrate: MIN_BANDWIDTH,
            max_bitrate: MAX_BANDWIDTH,
            default_bitrate: INITIAL_BANDWIDTH,
            ..Default::default()
        }
        .build();
        Self {
            ctx,
            manual_sub,
            tracks: HashMap::new(),
            slots: slotmap::SlotMap::with_capacity_and_key(VIDEO_MAX_SLOTS),
            routes: HashMap::new(),
            rng: Rng::seed_from_u64(rng.next_u64()),
            last_reconciled: HashSet::new(),
            desired_ctrl,
        }
    }

    pub fn add_track(&mut self, track: Track) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }
        plog_info!(self.ctx, track = %track.meta.id, "video track added");
        self.tracks.insert(track.meta.id, track);
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) -> bool {
        if self.tracks.remove(track_id).is_none() {
            return false;
        }
        plog_info!(self.ctx, track = %track_id, "video track removed");
        // Stop any slot currently targeting the removed track so reconcile_routes
        // fires StreamUnsubscribed and cleans up the routing table.
        for slot in self.slots.values_mut() {
            if slot
                .staging
                .as_ref()
                .is_some_and(|l| l.meta.id == *track_id)
                || slot.active.as_ref().is_some_and(|l| l.meta.id == *track_id)
            {
                slot.stop();
            }
        }
        self.rebalance();
        true
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        for (_key, slot) in self.slots.iter_mut() {
            let tracks = &mut self.tracks;
            if let Some(intent) = intents.get(&slot.mid) {
                Self::configure_slot(tracks, slot, Some(intent));
            } else {
                Self::configure_slot(tracks, slot, None);
            }
        }
    }

    /// Routes this slot to the given track at the specified QoS, or stops
    /// routing if `track_id` is `None` or `intent.max_height` is 0.
    fn configure_slot(
        tracks: &mut HashMap<TrackId, Track>,
        slot: &mut Slot,
        intent: Option<&Intent>,
    ) -> Option<()> {
        if let Some(intent) = intent
            && intent.max_height > 0
        {
            let track_id = &intent.track_id;
            let Some(track_state) = tracks.get_mut(track_id) else {
                plog_warn!(slot.ctx, track_id=%track_id, mid=%slot.mid, "configure_slot: requested track missing");
                slot.max_height = 0;
                slot.stop();
                return None;
            };

            // Keep current layer if slot already targets this track to avoid
            // unnecessary PLI requests; otherwise start at lowest quality.
            let layer = if let Some(target) = slot.target()
                && target.meta.id == track_state.meta.id
            {
                target
            } else {
                track_state.lowest_healthy_quality()
            };

            let layer = layer.clone();
            slot.max_height = intent.max_height;
            slot.min_height = intent.min_height;
            slot.priority = intent.priority;
            slot.switch_to(&layer, false);
        } else {
            slot.max_height = 0;
            slot.min_height = 0;
            slot.priority = 0;
            slot.stop();
        }

        Some(())
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackMeta> {
        self.tracks.values().map(|s| &s.meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> + '_ {
        self.slots.values().filter_map(|s| {
            Some(SlotAssignment {
                mid: s.mid,
                paused: s.paused || matches!(s.state(), SlotState::Idle | SlotState::Starting),
                track: {
                    let layer = s.target()?;
                    self.tracks.get(&layer.meta.id)?.meta.clone()
                },
            })
        })
    }

    pub fn has_slot(&self, mid: Mid) -> bool {
        self.slots.values().any(|s| s.mid == mid)
    }

    pub fn add_slot(&mut self, config: SlotConfig) {
        if self.has_slot(config.mid) {
            plog_debug!(self.ctx, mid = %config.mid, "video slot already provisioned; skipping duplicate");
            return;
        }
        let slot = Slot::new(self.ctx, config, &mut self.rng);
        self.slots.insert(slot);
        self.rebalance();
    }

    fn rebalance(&mut self) {
        if self.manual_sub {
            return;
        }

        let already_assigned: IndexSet<TrackId> = self
            .slots
            .values()
            .flat_map(|s| {
                s.staging
                    .as_ref()
                    .map(|t| t.meta.id)
                    .into_iter()
                    .chain(s.active.as_ref().map(|t| t.meta.id))
            })
            .collect();

        let mut pending_tracks = self
            .tracks
            .iter()
            .filter(|(id, _)| !already_assigned.contains(*id))
            .map(|(_, s)| s);

        let idle_slot_count = self
            .slots
            .values()
            .filter(|s| s.state() == SlotState::Idle)
            .count();
        let pending_count = self.tracks.len().saturating_sub(already_assigned.len());
        if pending_count > 0 && idle_slot_count == 0 {
            plog_debug!(
                self.ctx,
                pending_tracks = pending_count,
                total_slots = self.slots.len(),
                "rebalance: pending tracks but no idle slots, tracks will wait"
            );
        }

        let mut staged = 0usize;
        for slot in self
            .slots
            .values_mut()
            .filter(|s| s.state() == SlotState::Idle)
        {
            if let Some(track_state) = pending_tracks.next() {
                let layer = track_state.lowest_healthy_quality();
                slot.switch_to(layer, true);
                staged += 1;
            } else {
                break;
            }
        }
        if staged > 0 {
            plog_debug!(
                self.ctx,
                staged,
                "rebalance: staged tracks into idle slots, awaiting BWE to activate"
            );
        }

        debug_assert!(
            self.no_duplicate_slot_assignments(),
            "rebalance produced duplicate track assignments: each track must map to at most one slot"
        );
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> (Bitrate, bool) {
        let available_bandwidth = available_bandwidth.max(MIN_BANDWIDTH).min(MAX_BANDWIDTH);
        // 1. Prepare the input views
        let mut views: Vec<SlotView> = self
            .slots
            .iter()
            .filter_map(|(key, s)| {
                let current = s.target()?;
                let track = self.tracks.get(&current.meta.id)?;
                let current_quality = current.quality;
                Some(SlotView {
                    key,
                    mid: s.mid,
                    max_height: s.max_height,
                    min_height: s.min_height,
                    priority: s.priority,
                    track,
                    current_quality,
                })
            })
            .collect();

        views.sort_by(AllocationEngine::priority_order);

        let desired_raw = AllocationEngine::desired_bitrate(&views);
        let decisions = AllocationEngine::compute(available_bandwidth, &views);
        let desired = self.desired_ctrl.update(desired_raw);

        let mut changed = false;
        let _keyframe_requests: Vec<KeyframeRequest> = Vec::new();
        for (key, decision) in &decisions {
            let Some(slot) = self.slots.get_mut(key) else {
                plog_warn!(self.ctx, "no slot found from decision");
                continue;
            };

            match decision {
                AllocationDecision::Forward(layer, _) => {
                    changed |= slot.switch_to(layer, false);
                }
                AllocationDecision::Pause(layer, _) => {
                    changed |= slot.pause_at(layer);
                    let _stream_id = layer.stream_id();
                }
            }
        }

        if changed {
            log_allocation(self.ctx, available_bandwidth, desired, &decisions, &views);
        }

        (desired, changed)
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) -> Option<&TrackLayer> {
        let slot = self
            .slots
            .values()
            .find(|s| s.mid == req.mid && s.rid == req.rid)?;

        slot.target()
    }

    #[inline]
    pub fn on_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: &RtpPacket,
        cache: Option<&StreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        let Some(&slot_key) = self.routes.get(&stream_id.0) else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(slot_key) else {
            plog_warn!(self.ctx, "no slot found for {:?}", stream_id);
            return false;
        };
        slot.on_rtp(stream_id, pkt, cache, writer)
    }

    pub fn poll_slow(
        &mut self,
        now: Instant,
        _bandwidth: Bitrate,
        events: &mut impl ParticipantSink,
    ) {
        self.reconcile_routes(events);
        self.retry_keyframe_requests(now, events);
    }

    fn retry_keyframe_requests(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        for (_, slot) in self.slots.iter_mut() {
            slot.pli_retry(now, events);
        }
    }

    pub fn reconcile_routes(&mut self, events: &mut impl ParticipantSink) {
        let mut current = HashSet::new();
        for (slot_key, slot) in &self.slots {
            if let Some(staging) = slot.staging.as_ref() {
                current.insert((staging.meta.id, slot_key));
            }

            if let Some(active) = slot.active.as_ref() {
                current.insert((active.meta.id, slot_key));
            }
        }

        let to_remove_streams = self.last_reconciled.difference(&current);
        let to_add_streams = current.difference(&self.last_reconciled);

        for (track_id, _slot_key) in to_remove_streams {
            self.routes.remove(track_id);
            if let Some(track) = self.tracks.get(track_id) {
                events.unsubscribe(track.meta.clone());
            }
        }

        for (track_id, slot_key) in to_add_streams {
            self.routes.insert(*track_id, *slot_key);
            if let Some(track) = self.tracks.get(track_id) {
                events.subscribe(track.meta.clone());
            }
        }

        self.last_reconciled = current;

        debug_assert!(
            self.routes_consistent(),
            "route table inconsistent after reconcile_routes"
        );
    }

    fn routes_consistent(&self) -> bool {
        self.routes.iter().all(|(sid, slot_key)| {
            self.slots
                .get(*slot_key)
                .is_some_and(|slot| slot.matches_track_id(sid))
        })
    }

    /// Returns `true` if every track ID appears in at most one slot's
    /// active or staging layer.  A track must never be assigned to two
    /// slots simultaneously, because that would cause duplicate stream
    /// forwarding and corrupt the routing table.
    fn no_duplicate_slot_assignments(&self) -> bool {
        let mut seen: HashMap<TrackId, SlotKey> = HashMap::new();
        for (slot_key, slot) in self.slots.iter() {
            for layer in slot
                .staging
                .as_ref()
                .into_iter()
                .chain(slot.active.as_ref())
            {
                if let Some(existing_slot) = seen.get(&layer.meta.id) {
                    if existing_slot != &slot_key {
                        plog_error!(
                            self.ctx,
                            track = %layer.meta.id,
                            first_slot = ?existing_slot,
                            second_slot = ?slot_key,
                            "duplicate track assigned to multiple slots"
                        );
                        return false;
                    }
                } else {
                    seen.insert(layer.meta.id, slot_key);
                }
            }
        }
        true
    }
}

#[derive(PartialEq)]
enum SlotState {
    Idle,
    Starting,
    Stable,
    Switching,
}

struct Slot {
    ctx: LogCtx,
    ssrc: Ssrc,
    pt: Pt,

    active: Option<TrackLayer>,
    staging: Option<TrackLayer>,
    /// The layer this slot has just switched away from. Its packets are still
    /// arriving and some complete frames the subscriber already has part of.
    draining: Option<TrackLayer>,

    switcher: Switcher,

    mid: Mid,
    rid: Option<Rid>,
    max_height: u32,
    min_height: u32,
    priority: u32,
    paused: bool,

    /// Number of PLI retries sent for the current staging layer.
    staging_keyframe_retries: u32,
    /// When the last PLI retry was sent for the current staging layer.
    staging_keyframe_last_at: Option<Instant>,
    /// Current retry interval for PLI probes while waiting for the staging keyframe.
    staging_keyframe_interval: Duration,
    /// When the staged layer first became switchable but the active layer was
    /// still midway through a frame.
    switch_blocked_since: Option<Instant>,
}

impl Slot {
    fn new<R: RngCore>(ctx: LogCtx, cfg: SlotConfig, rng: &mut R) -> Self {
        Self {
            ctx,
            mid: cfg.mid,
            rid: cfg.rid,
            ssrc: cfg.ssrc,
            pt: cfg.pt,

            active: None,
            staging: None,
            draining: None,

            switcher: Switcher::new(rtp::VIDEO_FREQUENCY, rng),
            // With no signaling, we assume users are viewing with 720p playback
            max_height: 720,
            min_height: 0,
            priority: 0,
            paused: true,

            staging_keyframe_retries: 0,
            staging_keyframe_last_at: None,
            staging_keyframe_interval: KEYFRAME_RETRY_INTERVAL,
            switch_blocked_since: None,
        }
    }

    fn target(&self) -> Option<&TrackLayer> {
        self.staging.as_ref().or(self.active.as_ref())
    }

    fn state(&self) -> SlotState {
        match (self.active.is_some(), self.staging.is_some()) {
            (false, false) => SlotState::Idle,
            (false, true) => SlotState::Starting,
            (true, false) => SlotState::Stable,
            (true, true) => SlotState::Switching,
        }
    }

    fn pli_reset(&mut self) {
        self.staging_keyframe_retries = 0;
        self.staging_keyframe_last_at = None;
        self.staging_keyframe_interval = KEYFRAME_RETRY_INTERVAL;
        self.switch_blocked_since = None;
    }

    /// Whether the switch may land now, or should wait for the active layer to
    /// finish the frame it is partway through.
    fn may_switch_now(&mut self, now: Instant) -> bool {
        if self.switcher.at_clean_frame_boundary() {
            self.switch_blocked_since = None;
            return true;
        }
        let blocked_since = *self.switch_blocked_since.get_or_insert(now);
        now.saturating_duration_since(blocked_since) >= FRAME_ALIGN_GRACE
    }

    fn pli_retry(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        if self.paused {
            return;
        }
        if !matches!(self.state(), SlotState::Starting | SlotState::Switching) {
            return;
        }

        let Some(staging) = self.staging.as_ref() else {
            return;
        };
        let last_at = self.staging_keyframe_last_at;
        let retries = self.staging_keyframe_retries;

        let should_request =
            last_at.is_none_or(|last| now.duration_since(last) >= self.staging_keyframe_interval);
        if !should_request {
            return;
        }

        let keepalive_mode = retries >= KEYFRAME_MAX_RETRIES;
        let reached_keepalive = !keepalive_mode && retries + 1 == KEYFRAME_MAX_RETRIES;
        if !keepalive_mode {
            self.staging_keyframe_retries += 1;
        }
        self.staging_keyframe_last_at = Some(now);

        if reached_keepalive {
            self.staging_keyframe_interval = KEYFRAME_KEEPALIVE_INTERVAL;
            plog_debug!(
                self.ctx,
                mid = %self.mid,
                retries = KEYFRAME_MAX_RETRIES,
                interval = ?self.staging_keyframe_interval,
                "slot transition still waiting for any packets on the staged stream; using low-frequency keep-alive PLIs"
            );
        }

        events.request_keyframe(staging);
    }

    fn switch_to(&mut self, new_layer: &TrackLayer, force: bool) -> bool {
        let mut changed = false;
        let old_target = self.target().map(|l| l.stream_id());
        let is_track_change = self
            .target()
            .map(|l| l.meta.id)
            .is_none_or(|id| id != new_layer.meta.id);

        if force && is_track_change {
            changed |= self.active.take().is_some();
            self.switcher.clear();
        }

        if self.active.as_ref() == Some(new_layer) {
            if self.staging.is_some() {
                self.staging = None;
                self.switcher.clear_staging();
                self.pli_reset();
                changed = true;
                plog_debug!(self.ctx, mid=%self.mid, old_target=?old_target, new_target=?new_layer.stream_id(), "slot canceled in-flight transition and preserved active layer");
            }
        } else if self.target().as_ref() != Some(&new_layer) {
            if self.draining.as_ref() == Some(new_layer) {
                self.draining = None;
            }
            self.staging = Some(new_layer.clone());
            // Reset the switcher staging buffer so stale seq-no state from a
            // previous stream doesn't mix with the new stream's packets.
            self.switcher.clear_staging();
            // Reset retry state so the new staging layer gets fresh PLI attempts.
            self.pli_reset();
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, old_target=?old_target, new_target=?new_layer.stream_id(), "slot staging new layer");
        }

        if self.paused {
            self.paused = false;
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, new_target=?new_layer.stream_id(), "slot resumed from paused state");
        }

        changed
    }

    fn stop(&mut self) {
        plog_debug!(self.ctx, mid=%self.mid, "slot stopped");
        self.active = None;
        self.staging = None;
        self.draining = None;
        self.pli_reset();
    }

    fn pause_at(&mut self, layer: &TrackLayer) -> bool {
        let mut changed = false;

        // If we weren't active, but now we are explicitly None,
        // we check if it was already None to avoid redundant dirtying.
        if self.active.is_some() {
            self.active = None;
            changed = true;
        }

        if self.staging.as_ref() != Some(layer) {
            self.staging = Some(layer.clone());
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, target=?layer.stream_id(), "slot pause_at set staging target");
        }

        if !self.paused {
            self.paused = true;
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, target=?layer.stream_id(), "slot paused");
        }

        changed
    }

    fn on_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: &RtpPacket,
        cache: Option<&StreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        if self.paused {
            plog_trace!(self.ctx, mid=%self.mid, stream_id=?stream_id, "slot paused, dropping incoming packet");
            return false;
        }
        if self.active.as_ref().is_some_and(|a| a.is(stream_id)) {
            self.switcher.push(pkt.clone());
            while let Some(out) = self.switcher.pop() {
                writer.write_video_owned(out, self.mid, self.rid, self.pt);
            }
            return false;
        }
        if self.draining.as_ref().is_some_and(|d| d.is(stream_id)) {
            if let Some(out) = self.switcher.drain_tail(pkt, pkt.arrival_ts) {
                writer.write_video_owned(out, self.mid, self.rid, self.pt);
            }
            if !self.switcher.has_tail() {
                self.draining = None;
            }
            return false;
        }
        if self.staging.as_ref().is_some_and(|s| s.is(stream_id))
            && !self.switcher.is_switching()
            && let Some(pkts) = cache.and_then(|c| c.replay())
        {
            if !self.may_switch_now(pkt.arrival_ts) {
                return false;
            }
            self.switch_blocked_since = None;
            self.switcher.stage_direct(pkts, pkt.arrival_ts);
            while let Some(out) = self.switcher.pop() {
                writer.write_video_owned(out, self.mid, self.rid, self.pt);
            }
            if self.should_promote_staging() {
                self.draining = self.active.take();
                self.active = self.staging.take();
                return true;
            }
        }
        false
    }

    fn should_promote_staging(&self) -> bool {
        self.switcher.ready_to_switch() && self.staging.is_some()
    }

    fn matches_track_id(&self, track_id: &TrackId) -> bool {
        self.active
            .as_ref()
            .map(|l| l.meta.id == *track_id)
            .unwrap_or(false)
            || self
                .staging
                .as_ref()
                .map(|l| l.meta.id == *track_id)
                .unwrap_or(false)
    }
}

pub fn log_allocation(
    ctx: LogCtx,
    bwe: Bitrate,
    desired: Bitrate,
    decisions: &SecondaryMap<SlotKey, AllocationDecision>,
    slots: &[SlotView],
) {
    let mut reports = Vec::with_capacity(slots.len());
    let mut total_used_bps = 0.0;

    for slot in slots {
        let entry = match decisions.get(slot.key) {
            Some(AllocationDecision::Forward(l, bw)) => {
                total_used_bps += bw.as_f64();
                let q = match l.quality {
                    LayerQuality::High => "H",
                    LayerQuality::Medium => "M",
                    LayerQuality::Low => "L",
                };
                format!("{}:{}({})", slot.mid, q, bw)
            }
            Some(AllocationDecision::Pause(_, needed)) => format!("{}:PAUSE({})", slot.mid, needed),
            _ => format!("{}:IDLE", slot.mid),
        };
        reports.push(entry);
    }

    plog_info!(
        ctx,
        %bwe,
        used = %Bitrate::from(total_used_bps as u64),
        want = %desired,
        streams = %reports.join(" "),
        "downstream"
    );
}

pub struct SlotAssignment {
    pub mid: Mid,
    pub paused: bool,
    pub track: TrackMeta,
}

pub struct Intent {
    pub track_id: TrackId,
    /// Target render height (px); `0` hides the stream.
    pub max_height: u32,
    /// Floor render height (px) to keep under contention; `0` = droppable.
    pub min_height: u32,
    /// Contention order; higher wins bandwidth first.
    pub priority: u32,
}

pub struct AllocationEngine;

/// Client-declared QoS for one subscribed stream. See `VideoRequest` in the
/// signaling proto for the authoritative semantics.
#[derive(Clone, Debug)]
pub struct SlotView<'a> {
    pub key: SlotKey,
    pub mid: Mid,
    /// Target render height (px); layers taller than this are ineligible.
    pub max_height: u32,
    /// Floor render height (px) to keep under contention; `0` = droppable.
    pub min_height: u32,
    /// Contention order; higher wins bandwidth first.
    pub priority: u32,
    pub track: &'a Track,
    pub current_quality: LayerQuality,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AllocationDecision<'a> {
    Forward(&'a TrackLayer, Bitrate),
    Pause(&'a TrackLayer, Bitrate),
}

impl<'a> std::fmt::Display for AllocationDecision<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationDecision::Forward(layer, bitrate) => {
                write!(f, "Forward({} @ {})", layer, bitrate)
            }
            AllocationDecision::Pause(layer, needed) => {
                write!(f, "Pause({} needs {})", layer, needed)
            }
        }
    }
}

impl AllocationEngine {
    const RESERVE_FRACTION: f64 = 0.10;
    // A slot keeps its layer until its cost exceeds ~1.33x the budget (1/0.75),
    // giving recoveries a hysteresis dead-band against churn.
    const DOWNGRADE_FACTOR: f64 = 0.75;

    /// Frame height (px) used for spatial gating: the sender-declared height
    /// from Video Layers Allocation when available, otherwise a per-quality
    /// fallback for senders that don't advertise resolution.
    fn height(layer: &TrackLayer) -> u32 {
        let declared = layer.state.declared_height();
        if declared > 0 {
            return declared;
        }
        match layer.quality {
            LayerQuality::Low => 180,
            LayerQuality::Medium => 360,
            LayerQuality::High => 720,
        }
    }

    /// Whether a layer is permitted by the client's spatial request.
    fn spatially_allowed(slot: &SlotView<'_>, layer: &TrackLayer) -> bool {
        Self::height(layer) <= slot.max_height
    }

    /// Whether a layer may currently be forwarded or switched into.
    fn eligible(slot: &SlotView<'_>, layer: &TrackLayer) -> bool {
        Self::spatially_allowed(slot, layer) && layer.state.is_healthy()
    }

    fn cost(layer: &TrackLayer) -> f64 {
        layer.state.bitrate_bps()
    }

    /// Lowest healthy layer ignoring the spatial constraint. Used as a
    /// last-resort fallback when all spatially-allowed layers are inactive
    /// (e.g. "f"/"h"/"q" negotiated but only "f" and "h" are active and the
    /// client requests a height that only "q" would satisfy).
    fn closest_healthy<'a>(slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|l| l.state.is_healthy())
            .min_by_key(|l| l.quality)
    }

    /// A legal layer to retain as the pause target even when no layer is
    /// currently healthy enough to forward. Falls back to the lowest healthy
    /// layer (closest rank) when no spatially-allowed layer exists.
    fn pause_target<'a>(slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| Self::spatially_allowed(slot, layer))
            .min_by_key(|layer| layer.quality)
            .or_else(|| Self::closest_healthy(slot))
    }

    /// The lowest eligible layer strictly above the selected layer.
    /// When starting from nothing (`current = None`) and no spatially-allowed
    /// healthy layer exists, falls back to the closest-rank healthy layer so
    /// the slot always gets an initial allocation rather than being silently
    /// dropped.
    fn next_layer<'a>(
        slot: &'a SlotView<'a>,
        current: Option<&'a TrackLayer>,
    ) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| Self::eligible(slot, layer))
            .filter(|layer| current.is_none_or(|current| layer.quality > current.quality))
            .min_by_key(|layer| layer.quality)
            .or_else(|| {
                if current.is_none() {
                    Self::closest_healthy(slot)
                } else {
                    None
                }
            })
    }

    /// Higher priority first, with MID as a deterministic tie-breaker.
    pub fn priority_order(a: &SlotView<'_>, b: &SlotView<'_>) -> Ordering {
        b.priority.cmp(&a.priority).then_with(|| a.mid.cmp(&b.mid))
    }

    /// Best layer worth wanting for this slot, irrespective of what the
    /// budget can currently afford. Feeds the BWE-facing desired bitrate.
    fn best_healthy<'a>(slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| Self::eligible(slot, layer))
            .max_by_key(|layer| layer.quality)
    }

    /// Aggregate bitrate the SFU would like BWE to grant next: simply the sum
    /// of every slot's highest eligible layer cost, independent of what is
    /// currently affordable. Kept separate from `compute` so the demand signal
    /// doesn't inherit the allocator's reserve/hysteresis math.
    pub fn desired_bitrate(slots: &[SlotView<'_>]) -> Bitrate {
        let total: f64 = slots
            .iter()
            .filter_map(Self::best_healthy)
            .map(Self::cost)
            .sum();
        Bitrate::from(total as u64)
    }

    /// The layer that satisfies a slot's `min_height` floor: the lowest eligible
    /// layer at least `min_height` tall, or the tallest eligible layer if none
    /// reaches it. `None` when the slot is droppable (`min_height == 0`) or has
    /// no eligible layer.
    fn floor_layer<'a>(slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        if slot.min_height == 0 {
            return None;
        }
        let eligible = || slot.track.layers.iter().filter(|l| Self::eligible(slot, l));
        eligible()
            .filter(|l| Self::height(l) >= slot.min_height)
            .min_by_key(|l| l.quality)
            .or_else(|| eligible().max_by_key(|l| l.quality))
            .or_else(|| Self::closest_healthy(slot))
    }

    /// Strict-priority allocation. `slots` must be pre-sorted by `priority_order`
    /// (highest priority first). Each stream keeps its guaranteed `min_height`
    /// floor first; leftover budget then raises streams toward their target in
    /// the same priority order, one genuine upgrade per call so send-rate rises
    /// gradually enough for BWE to track it.
    pub fn compute<'a>(
        bwe: Bitrate,
        slots: &'a [SlotView<'a>],
    ) -> SecondaryMap<SlotKey, AllocationDecision<'a>> {
        debug_assert!(
            slots.is_sorted_by(|a, b| Self::priority_order(a, b).is_le()),
            "compute expects slots sorted by priority_order",
        );

        let mut budget = bwe.as_f64();
        let reserve = budget * Self::RESERVE_FRACTION;
        let mut allocs: Vec<Option<&TrackLayer>> = vec![None; slots.len()];

        // Pass 1: guarantee each stream's floor, in priority order. A stream we
        // can't afford the floor for is left paused; droppable streams (no floor)
        // wait for Pass 2.
        for (i, slot) in slots.iter().enumerate() {
            if let Some(floor) = Self::floor_layer(slot) {
                let cost = Self::cost(floor);
                if cost <= budget {
                    budget -= cost;
                    allocs[i] = Some(floor);
                }
            }
        }

        // Pass 2: raise toward target in priority order. Retention/recovery up to
        // the current layer is free (sticky dead-band); climbing above it is a
        // genuine upgrade, and only one is granted per call.
        let mut upgrade_used = false;
        for (i, slot) in slots.iter().enumerate() {
            let mut cur = allocs[i];
            while let Some(next) = Self::next_layer(slot, cur) {
                let step = Self::cost(next) - cur.map_or(0.0, Self::cost);
                let admitted = if next.quality > slot.current_quality {
                    !upgrade_used && step + reserve <= budget
                } else {
                    step * Self::DOWNGRADE_FACTOR <= budget
                };
                if !admitted {
                    break;
                }
                if next.quality > slot.current_quality {
                    upgrade_used = true;
                }
                budget -= step;
                cur = Some(next);
            }
            allocs[i] = cur;
        }

        // Finalize.
        let mut decisions = SecondaryMap::new();
        for (i, slot) in slots.iter().enumerate() {
            if let Some(layer) = allocs[i] {
                decisions.insert(
                    slot.key,
                    AllocationDecision::Forward(layer, Bitrate::from(Self::cost(layer) as u64)),
                );
            } else if let Some(target) = Self::pause_target(slot) {
                decisions.insert(
                    slot.key,
                    AllocationDecision::Pause(target, Bitrate::from(Self::cost(target) as u64)),
                );
            }
        }

        decisions
    }
}

#[cfg(test)]
mod assignment_tests {
    use super::*;
    use crate::entity::{ParticipantId, TrackId, TrackKind};
    use crate::participant::event::test_utils::MockParticipantSink;
    use crate::rtp::RtpPacket;
    use crate::track::{LayerQuality, UpstreamTrack, test_utils::make_video_track};
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};
    use str0m::bwe::Bitrate;
    use str0m::media::{Mid, SimulcastLayer};

    fn test_rng() -> impl RngCore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    #[derive(Default)]
    struct FakeRouter {
        subscribed: std::collections::HashSet<StreamId>,
    }

    struct TestTracks {
        pub senders: Vec<UpstreamTrack>,
        pub ids: Vec<TrackId>,
    }

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(&mut test_rng()),
        }
    }

    fn setup_allocator() -> VideoAllocator {
        VideoAllocator::new(test_ctx(), false, &mut test_rng())
    }

    fn add_tracks(allocator: &mut VideoAllocator, count: usize) -> TestTracks {
        let pid = ParticipantId::new(&mut test_rng());

        let mut senders = Vec::new();
        let mut ids = Vec::new();

        for i in 0..count {
            let mid = Mid::from(&format!("v{i}")[..]);
            let (tx, track) = make_video_track(pid, mid, vec![]);
            let meta = tx.meta.clone();

            // Ensure tracks are considered "healthy" for allocation tests.
            for layer in &track.layers {
                layer.state.update_for_test().inactive(false);
            }

            ids.push(meta.id);
            allocator.add_track(Track {
                meta,
                layers: track.layers,
            });
            senders.push(tx);
        }

        TestTracks { senders, ids }
    }

    fn add_slots(allocator: &mut VideoAllocator, count: usize) {
        for i in 0..count {
            allocator.add_slot(SlotConfig {
                mid: Mid::from(&format!("s{i}")[..]),
                ..SlotConfig::default()
            });
        }
    }

    #[test]
    fn rebalance_assigns_tracks_to_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn configure_all_slots_after_idle() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);

        // Empty intent should idle all slots.
        allocator.configure(&HashMap::new());
        assert_eq!(allocator.slots().count(), 0);

        // Re-activate all slots.
        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s0"),
            Intent {
                track_id: tracks.ids[0],
                max_height: 720,
                min_height: 0,
                priority: 0,
            },
        );
        intents.insert(
            Mid::from("s1"),
            Intent {
                track_id: tracks.ids[1],
                max_height: 720,
                min_height: 0,
                priority: 0,
            },
        );
        intents.insert(
            Mid::from("s2"),
            Intent {
                track_id: tracks.ids[2],
                max_height: 720,
                min_height: 0,
                priority: 0,
            },
        );

        allocator.configure(&intents);
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn configure_missing_requested_track_stops_slot() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        allocator.rebalance();

        let missing_track_id = ParticipantId::new(&mut test_rng())
            .derive_track_id(TrackKind::Video, &Mid::from("missing"));
        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s0"),
            Intent {
                track_id: missing_track_id,
                max_height: 720,
                min_height: 0,
                priority: 0,
            },
        );

        allocator.configure(&intents);
        assert!(
            allocator
                .slots
                .values()
                .all(|s| matches!(s.state(), SlotState::Idle))
        );
    }

    #[test]
    fn more_tracks_than_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 5);
        add_slots(&mut allocator, 2);
        assert_eq!(allocator.slots().count(), 2);
    }

    #[test]
    fn tracks_before_slots() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 2);
        add_slots(&mut allocator, 2);
        assert_eq!(allocator.slots().count(), 2);
    }

    #[test]
    fn route_subscription_initializes_keyframe_retry_state() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let track = allocator.tracks.get(&tracks.ids[0]).unwrap();
        let low = track.lowest_quality().clone();
        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = None;
        slot.staging = Some(low);
        slot.paused = false;

        let now = Instant::now();
        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);
        assert_eq!(
            queue.request_keyframe_calls.len(),
            0,
            "reconcile_routes no longer emits an immediate keyframe request"
        );

        let mut queue = MockParticipantSink::new();
        allocator.retry_keyframe_requests(now, &mut queue);
        assert_eq!(
            queue.request_keyframe_calls.len(),
            1,
            "retry_keyframe_requests should not send an immediate duplicate PLI after reconcile_routes"
        );
    }

    #[test]
    fn staging_preserves_old_route_until_switch_complete() {
        let pid = ParticipantId::new(&mut test_rng());
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let track_layers = vec![
            SimulcastLayer::new("h"),
            SimulcastLayer::new("m"),
            SimulcastLayer::new("l"),
        ];
        let (tx, track) = make_video_track(pid, mid, track_layers);
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let track_id = tx.meta.id;
        allocator.add_track(Track {
            meta: tx.meta.clone(),
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let track = allocator.tracks.get(&track_id).unwrap();
        let low = track.lowest_quality().clone();
        let high = track.by_quality(LayerQuality::High).unwrap().clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(low.clone());
        slot.staging = Some(high.clone());
        slot.paused = false;

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert!(allocator.routes.contains_key(&low.meta.id));
        assert!(allocator.routes.contains_key(&high.meta.id));
        assert_eq!(
            queue.subscribe_calls.len(),
            1,
            "routes are tracked per track, so staging and active layers share one subscription"
        );
        assert_eq!(
            queue.unsubscribe_calls.len(),
            0,
            "routes are tracked per track, so staging and active layers share one subscription"
        );
        assert_eq!(
            queue.request_keyframe_calls.len(),
            0,
            "reconcile_routes does not request keyframes directly"
        );
    }

    #[test]
    fn route_removed_only_when_slot_has_no_active_or_staging_target() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let track = allocator.tracks.get(&tracks.ids[0]).unwrap();
        let old_stream_id = track.lowest_quality().stream_id();
        let slot_key = allocator.slots.keys().next().unwrap();
        allocator.routes.insert(old_stream_id.0, slot_key);
        allocator
            .last_reconciled
            .insert((old_stream_id.0, slot_key));

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = None;
        slot.staging = None;
        slot.paused = false;

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert!(allocator.routes.is_empty());
        assert_eq!(queue.unsubscribe_calls.len(), 1);
    }

    #[test]
    fn reconcile_routes_corrects_invalid_route_slot_mapping() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 2);

        let track = allocator.tracks.get(&tracks.ids[0]).unwrap();
        let low = track.lowest_quality().clone();
        let slot_keys: Vec<_> = allocator.slots.keys().collect();
        let correct_slot_key = slot_keys[0];
        let stale_slot_key = slot_keys[1];

        let slot = allocator.slots.get_mut(correct_slot_key).unwrap();
        slot.active = Some(low.clone());
        slot.paused = false;

        allocator.routes.insert(low.meta.id, stale_slot_key);

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert_eq!(allocator.routes.get(&low.meta.id), Some(&correct_slot_key));
        assert_eq!(queue.subscribe_calls.len(), 1);
    }

    #[test]
    fn does_not_promote_staging_before_staging_packets() {
        let pid = ParticipantId::new(&mut test_rng());
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (tx, track) = make_video_track(
            pid,
            mid,
            vec![SimulcastLayer::new("h"), SimulcastLayer::new("m")],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        allocator.add_track(Track {
            meta: tx.meta.clone(),
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let track = allocator.tracks.get(&tx.meta.id).unwrap();
        let high = track.by_quality(LayerQuality::High).unwrap().clone();
        let medium = track.by_quality(LayerQuality::Medium).unwrap().clone();

        let slot_key = allocator.slots.keys().next().unwrap().clone();
        let slot = allocator.slots.get_mut(slot_key).unwrap();
        slot.active = Some(high.clone());
        slot.staging = Some(medium.clone());
        slot.paused = false;

        let mut pkt = RtpPacket::default();
        pkt.seq_no = 1.into();

        let mut writer = crate::track::StreamWriter::new();
        slot.on_rtp(&high.stream_id(), &pkt, None, &mut writer);

        assert!(
            !slot.should_promote_staging(),
            "staging should not promote before any staging packets are seen"
        );
        assert_eq!(slot.active.as_ref().unwrap().stream_id(), high.stream_id());
        assert_eq!(
            slot.staging.as_ref().unwrap().stream_id(),
            medium.stream_id()
        );
    }

    #[test]
    fn removing_track_releases_slot() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);
        assert_eq!(allocator.slots().count(), 1);
        allocator.remove_track(&tracks.ids[0]);
        assert_eq!(allocator.slots().count(), 0);
    }

    #[test]
    fn multiple_slot_candidates_exist() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn allocator_returns_positive_desired_bitrate() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let (desired, _) = allocator.update_allocations(Bitrate::from(5_000_000));
        assert!(desired.as_f64() > 0.0);
    }

    #[test]
    fn switch_to_same_active_layer_is_idempotent() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let track_id = tracks.ids[0];
        let layer = allocator
            .tracks
            .get(&track_id)
            .unwrap()
            .lowest_quality()
            .clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(layer.clone());
        slot.staging = None;
        slot.paused = false;

        assert!(
            !slot.switch_to(&layer, false),
            "re-applying the same active layer should not mark a change"
        );
    }

    #[test]
    fn switch_to_accepts_ongoing_upgrade() {
        let pid = ParticipantId::new(&mut test_rng());
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track) = make_video_track(
            pid,
            mid,
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            layer.state.update_for_test().inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let staging = track.by_quality(LayerQuality::Medium).unwrap().clone();
        let new_stage = track.by_quality(LayerQuality::High).unwrap().clone();

        slot.active = None;
        slot.staging = Some(staging);
        slot.paused = false;

        assert!(slot.switch_to(&new_stage, false));
        assert_eq!(
            slot.staging.as_ref().unwrap().stream_id(),
            new_stage.stream_id()
        );
    }

    #[test]
    fn switch_to_cancels_transition_when_target_reverts_to_active() {
        let pid = ParticipantId::new(&mut test_rng());
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track) = make_video_track(
            pid,
            mid,
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            layer.state.update_for_test().inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let active = track.by_quality(LayerQuality::Low).unwrap().clone();
        let staging = track.by_quality(LayerQuality::High).unwrap().clone();

        slot.active = Some(active.clone());
        slot.staging = Some(staging);
        slot.paused = false;

        assert!(slot.switch_to(&active, false));
        assert!(slot.staging.is_none());
        assert_eq!(
            slot.active.as_ref().unwrap().stream_id(),
            active.stream_id()
        );
    }

    #[test]
    fn switch_to_allows_downgrade_during_transition() {
        let pid = ParticipantId::new(&mut test_rng());
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track) = make_video_track(
            pid,
            mid,
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            layer.state.update_for_test().inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let staging = track.by_quality(LayerQuality::High).unwrap().clone();
        let new_stage = track.by_quality(LayerQuality::Low).unwrap().clone();

        slot.active = None;
        slot.staging = Some(staging.clone());
        slot.paused = false;

        assert!(slot.switch_to(&new_stage, false));
        assert_eq!(
            slot.staging.as_ref().unwrap().stream_id(),
            new_stage.stream_id()
        );
    }

    #[test]
    fn force_switch_to_different_track_clears_active_immediately() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 2);
        add_slots(&mut allocator, 1);

        let t0 = allocator.tracks.get(&tracks.ids[0]).unwrap();
        let t1 = allocator.tracks.get(&tracks.ids[1]).unwrap();
        let active = t0.lowest_quality().clone();
        let new_target = t1.lowest_quality().clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(active);
        slot.staging = None;
        slot.paused = false;

        assert!(slot.switch_to(&new_target, true));
        assert!(
            slot.active.is_none(),
            "force switch must clear active stream"
        );
        assert_eq!(
            slot.staging.as_ref().unwrap().meta.id,
            new_target.meta.id,
            "new track must become staging target"
        );
    }

    /// Regression test for the bug where `rebalance` only checked `staging`
    /// when building `already_assigned`, so tracks that had been promoted from
    /// staging → active (Stable state, staging=None) were treated as
    /// unassigned and re-allocated to idle slots on the next `rebalance` call.
    #[test]
    fn no_double_assignment_after_staging_promoted_to_active() {
        let mut allocator = setup_allocator();
        // 4 senders publishing into a 5-person room → 4 tracks, 7 recv slots.
        let tracks = add_tracks(&mut allocator, 4);
        add_slots(&mut allocator, 7);

        // Manually promote every staged slot to Stable (simulate the normal
        // on_rtp path that sets active = staging.take()).
        for slot in allocator.slots.values_mut() {
            if let Some(layer) = slot.staging.take() {
                slot.active = Some(layer);
            }
        }

        // Adding a new slot triggers rebalance().  Before the fix this would
        // double-assign the active tracks into the newly-idle slots.
        allocator.add_slot(SlotConfig {
            mid: Mid::from("extra"),
            ..SlotConfig::default()
        });

        // Every track must appear in at most one slot.
        assert!(
            allocator.no_duplicate_slot_assignments(),
            "rebalance double-assigned at least one track after staging was promoted to active"
        );

        // None of the original 4 tracks should have been re-staged in a second slot.
        let assignment_count = |id: &TrackId| {
            allocator
                .slots
                .values()
                .filter(|s| {
                    s.staging.as_ref().map_or(false, |l| l.meta.id == *id)
                        || s.active.as_ref().map_or(false, |l| l.meta.id == *id)
                })
                .count()
        };
        for id in &tracks.ids {
            assert_eq!(
                assignment_count(id),
                1,
                "track {:?} was assigned to more than one slot",
                id
            );
        }
    }

    /// Variant: adding a *new track* after existing tracks have been promoted
    /// to active must assign the new track to a fresh idle slot without
    /// disturbing the already-active assignments.
    #[test]
    fn no_double_assignment_when_new_track_added_after_stabilisation() {
        let mut allocator = setup_allocator();
        let existing = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 7);

        // Promote all staged slots to Stable.
        for slot in allocator.slots.values_mut() {
            if let Some(layer) = slot.staging.take() {
                slot.active = Some(layer);
            }
        }

        // Trigger rebalance with a new incoming track.
        let pid = ParticipantId::new(&mut test_rng());
        let (tx, track) = make_video_track(pid, Mid::from("late"), vec![]);
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        allocator.add_track(Track {
            meta: tx.meta.clone(),
            layers: track.layers,
        });

        assert!(
            allocator.no_duplicate_slot_assignments(),
            "rebalance double-assigned a track when a new track arrived post-stabilisation"
        );

        // Existing tracks must still each be in exactly one slot.
        for id in &existing.ids {
            let count = allocator
                .slots
                .values()
                .filter(|s| {
                    s.staging.as_ref().map_or(false, |l| l.meta.id == *id)
                        || s.active.as_ref().map_or(false, |l| l.meta.id == *id)
                })
                .count();
            assert_eq!(count, 1, "existing track {:?} was double-assigned", id);
        }
    }

    #[test]
    fn allocator_handles_track_churn() {
        let mut allocator = setup_allocator();
        let mut tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        allocator.remove_track(&tracks.ids[1]);
        let pid = ParticipantId::new(&mut test_rng());
        let (tx, track) = make_video_track(pid, Mid::from("new_track"), vec![]);
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let meta = tx.meta.clone();
        tracks.senders.push(tx);
        allocator.add_track(Track {
            meta,
            layers: track.layers,
        });
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn same_slot_switching_same_track_is_not_duplicate_assignment() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (tx, track) = make_video_track(
            pid,
            Mid::from("t"),
            vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
        );

        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }

        allocator.add_track(Track {
            meta: tx.meta.clone(),
            layers: track.layers.clone(),
        });
        add_slots(&mut allocator, 1);

        allocator.rebalance();
        let upgraded_layer = track
            .by_quality(LayerQuality::Medium)
            .expect("track should have an upgrade layer")
            .clone();

        {
            let slot = allocator.slots.values_mut().next().unwrap();
            slot.active = slot.staging.take();
            slot.paused = false;
            // Force a quality transition for the same track, leaving active + staging in one slot.
            slot.switch_to(&upgraded_layer, true);
            assert!(slot.active.is_some());
            assert!(slot.staging.is_some());
            assert_eq!(
                slot.active.as_ref().unwrap().meta.id,
                slot.staging.as_ref().unwrap().meta.id
            );
        }

        assert!(allocator.no_duplicate_slot_assignments());
        assert_eq!(allocator.slots.len(), 1);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    use crate::entity::ParticipantId;
    use crate::rtp::monitor::StreamQuality;
    use crate::track::{LayerQuality, test_utils::make_video_track};
    use proptest::prelude::*;
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    fn test_rng() -> impl RngCore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn next_slot_key() -> SlotKey {
        use std::cell::RefCell;
        thread_local! {
            static KEY_SM: RefCell<SlotMap<SlotKey, ()>> = RefCell::new(SlotMap::with_key());
        }
        KEY_SM.with(|sm| sm.borrow_mut().insert(()))
    }

    fn healthy_track() -> Track {
        use str0m::media::SimulcastLayer;
        let (tx, track) = make_video_track(
            ParticipantId::new(&mut test_rng()),
            Mid::from("t"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        Track {
            meta: tx.meta,
            layers: track.layers,
        }
    }

    fn track_with_bad_layer(bad: LayerQuality) -> Track {
        let vt = healthy_track();
        vt.by_quality(bad)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);
        vt
    }

    fn slot<'a>(
        mid: &str,
        max_height: u32,
        track: &'a Track,
        current: LayerQuality,
    ) -> SlotView<'a> {
        SlotView {
            key: next_slot_key(),
            mid: Mid::from(mid),
            max_height,
            min_height: 0,
            track,
            priority: 0,
            current_quality: current,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn qos_slot<'a>(
        mid: &str,
        max_height: u32,
        min_height: u32,
        priority: u32,
        track: &'a Track,
        current: LayerQuality,
    ) -> SlotView<'a> {
        SlotView {
            key: next_slot_key(),
            mid: Mid::from(mid),
            max_height,
            min_height,
            track,
            priority,
            current_quality: current,
        }
    }

    /// `compute` requires slots pre-sorted by priority; helper for tests.
    fn sorted(mut slots: Vec<SlotView<'_>>) -> Vec<SlotView<'_>> {
        slots.sort_by(AllocationEngine::priority_order);
        slots
    }

    fn forwarded_quality(
        decisions: &SecondaryMap<SlotKey, AllocationDecision<'_>>,
        key: SlotKey,
    ) -> Option<LayerQuality> {
        match decisions.get(key) {
            Some(AllocationDecision::Forward(l, _)) => Some(l.quality),
            _ => None,
        }
    }

    #[test]
    fn at_most_one_genuine_upgrade_per_call() {
        let t = healthy_track();
        let high = layer_bps(&t, LayerQuality::High);
        // Ample budget — several upgrades are affordable, but only one is granted.
        let available = bw((high * 4.0) as u64 / 1_000);
        let slots = sorted(vec![
            qos_slot("a", 1080, 0, 10, &t, LayerQuality::Low),
            qos_slot("b", 1080, 0, 5, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots);
        let upgrades = slots
            .iter()
            .filter(|s| forwarded_quality(&decisions, s.key).is_some_and(|q| q > s.current_quality))
            .count();
        assert!(
            upgrades <= 1,
            "granted {upgrades} genuine upgrades in one call"
        );
    }

    #[test]
    fn higher_priority_slot_served_first_under_contention() {
        let t = healthy_track();
        let low = layer_bps(&t, LayerQuality::Low);
        // Budget fits only one Low layer.
        let available = bw((low as u64) / 1_000 + 5);
        let slots = sorted(vec![
            qos_slot("hi", 1080, 0, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, 0, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots);
        let hi = slots.iter().find(|s| s.priority == 100).unwrap();
        let lo = slots.iter().find(|s| s.priority == 0).unwrap();
        assert!(
            forwarded_quality(&decisions, hi.key).is_some(),
            "high-priority paused"
        );
        assert!(
            forwarded_quality(&decisions, lo.key).is_none(),
            "low-priority not starved"
        );
    }

    #[test]
    fn pinned_stream_does_not_drop_when_another_joins() {
        let t = healthy_track();
        let high_h = AllocationEngine::height(t.by_quality(LayerQuality::High).unwrap());
        let high_bps = layer_bps(&t, LayerQuality::High);
        // Enough for the pinned High plus a little — but not for a second High.
        let available = bw((high_bps * 1.3) as u64 / 1_000);

        // Pinned: min_height == its High layer's height (floor == target), already
        // forwarding High. A lower-priority background stream joins.
        let slots = sorted(vec![
            qos_slot("pin", 1080, high_h, 100, &t, LayerQuality::High),
            qos_slot("bg", 1080, 0, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots);
        let pin = slots.iter().find(|s| s.mid == Mid::from("pin")).unwrap();
        assert_eq!(
            forwarded_quality(&decisions, pin.key),
            Some(LayerQuality::High),
            "pinned stream degraded when a background stream joined"
        );
    }

    #[test]
    fn droppable_stream_pauses_before_a_floored_one() {
        let t = healthy_track();
        let low = layer_bps(&t, LayerQuality::Low);
        let low_h = AllocationEngine::height(t.by_quality(LayerQuality::Low).unwrap());
        // Budget fits exactly one Low floor.
        let available = bw((low as u64) / 1_000 + 5);
        let slots = sorted(vec![
            // Droppable (min_height 0), same priority as the floored one.
            qos_slot("drop", 1080, 0, 0, &t, LayerQuality::Low),
            // Floored: must stay visible.
            qos_slot("keep", 1080, low_h, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots);
        let drop = slots.iter().find(|s| s.mid == Mid::from("drop")).unwrap();
        let keep = slots.iter().find(|s| s.mid == Mid::from("keep")).unwrap();
        assert!(
            forwarded_quality(&decisions, keep.key).is_some(),
            "floored stream should keep its guaranteed layer"
        );
        assert!(
            forwarded_quality(&decisions, drop.key).is_none(),
            "droppable stream should yield first"
        );
    }

    fn bw(kbps: u64) -> Bitrate {
        Bitrate::from(kbps * 1_000)
    }

    fn layer_bps(track: &Track, q: LayerQuality) -> f64 {
        track.by_quality(q).unwrap().state.bitrate_bps()
    }

    /// Allocation cost comes from bitrate_bps, which is set by the upstream
    /// monitor's RateFilter (smoothing VLA-declared targets). When bitrate_bps
    /// is stable (as it is after the monitor's fast-rise/slow-fall filter
    /// converges), the chosen layer is stable regardless of VBR content bursts.
    #[test]
    fn stable_bitrate_bps_makes_allocation_stable() {
        let t = healthy_track();

        // Decide the forwarded layer with given bitrate_bps values (the
        // smoothed cost signal written by StreamMonitor::poll).
        let decide = |high_bps: u64, med_bps: u64| -> Option<LayerQuality> {
            t.by_quality(LayerQuality::High)
                .unwrap()
                .state
                .update_for_test()
                .bitrate(high_bps);
            t.by_quality(LayerQuality::Medium)
                .unwrap()
                .state
                .update_for_test()
                .bitrate(med_bps);
            let slots = vec![slot("a", 1080, &t, LayerQuality::Medium)];
            let decisions = AllocationEngine::compute(bw(886), &slots);
            match decisions[slots[0].key] {
                AllocationDecision::Forward(l, _) => Some(l.quality),
                AllocationDecision::Pause(..) => None,
            }
        };

        // When bitrate_bps reflects VLA-declared stable targets (as set by the
        // upstream monitor's RateFilter), allocation is stable regardless of VBR.
        let stable_high = 2_600_000u64;
        let stable_med = 800_000u64;
        assert_eq!(
            decide(stable_high, stable_med),
            decide(stable_high, stable_med),
            "stable bitrate_bps must produce stable allocation"
        );

        // When bitrate_bps fluctuates (as it does without the RateFilter),
        // allocation flaps — confirming that stability comes from the filter.
        assert_ne!(
            decide(100_000, 100_000),
            decide(3_000_000, 1_500_000),
            "unstable bitrate_bps should flap (expected behavior without filter)"
        );
    }

    /// Sender-declared resolution (Video Layers Allocation) replaces the
    /// hard-coded per-quality height guess for spatial gating.
    #[test]
    fn declared_height_overrides_quality_fallback_for_spatial_gate() {
        let t = healthy_track();
        // The High layer is actually only 180p; the sender declares it.
        t.by_quality(LayerQuality::High)
            .unwrap()
            .state
            .update_for_test()
            .declared_height(180);

        // Client caps at 200p. The hard-coded fallback rates High at 720p and
        // would forbid it, but the declared 180p must be allowed.
        let slot = slot("a", 200, &t, LayerQuality::High);
        let high = t.by_quality(LayerQuality::High).unwrap();
        assert!(AllocationEngine::spatially_allowed(&slot, high));

        // The Medium layer declared nothing → keeps its 360p fallback → forbidden.
        let medium = t.by_quality(LayerQuality::Medium).unwrap();
        assert!(!AllocationEngine::spatially_allowed(&slot, medium));
    }

    // ─── Property: every slot receives exactly one decision ─────────────────────

    #[test]
    fn every_slot_gets_a_decision() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
            slot("c", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(10_000), &slots);
        for s in &slots {
            assert!(
                decisions.contains_key(s.key),
                "slot {} has no decision",
                s.mid
            );
        }
    }

    // ─── Property: decisions are Forward or Pause, never something else ─────────

    #[test]
    fn decisions_are_forward_or_pause() {
        let t = healthy_track();
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots);
        for (_, d) in &decisions {
            assert!(
                matches!(
                    d,
                    AllocationDecision::Forward(..) | AllocationDecision::Pause(..)
                ),
                "unexpected variant: {:?}",
                d
            );
        }
    }

    // ─── Property: desired bitrate is non-negative ───────────────────────────────

    #[test]
    fn desired_bitrate_is_non_negative() {
        let t = healthy_track();
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        let desired = AllocationEngine::desired_bitrate(&slots);
        assert!(desired.as_f64() >= 0.0, "desired must be non-negative");
    }

    // ─── Property: with unlimited bandwidth every slot forwards ─────────────────

    #[test]
    fn unlimited_bandwidth_forwards_all_slots() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
            slot("c", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(100_000), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[s.key], AllocationDecision::Forward(..)),
                "slot {} was not forwarded with unlimited bandwidth",
                s.mid
            );
        }
    }

    // ─── Property: with zero bandwidth every slot pauses ────────────────────────

    #[test]
    fn zero_bandwidth_pauses_all_slots() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(0), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[s.key], AllocationDecision::Pause(..)),
                "slot {} was not paused with zero bandwidth",
                s.mid
            );
        }
    }

    // ─── Property: paused decisions always carry a resume target ────────────────
    //
    // The allocation engine must never emit a bare Pause — the receiver it
    // carries is the layer the driver will resume to when bandwidth recovers.

    #[test]
    fn pause_always_carries_a_resume_receiver() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(0), &slots);
        for (key, d) in &decisions {
            if let AllocationDecision::Pause(receiver, needed) = d {
                // The receiver field must point somewhere meaningful (non-null
                // is the only invariant we can assert structurally).
                let _ = receiver; // just asserting it exists via pattern match
                assert!(needed.as_f64() > 0.0, "Pause bitrate must be positive");
            } else if matches!(d, AllocationDecision::Pause(..)) {
                panic!("Pause for {:?} is missing its resume receiver", key);
            }
        }
    }

    // ─── Property: a bad high layer falls back to the next healthy layer ─────────
    //
    // When the highest quality is degraded, the engine should still forward
    // rather than pause — it just picks a lower healthy layer.

    #[test]
    fn bad_high_layer_falls_back_rather_than_pausing() {
        let t = track_with_bad_layer(LayerQuality::High);
        let slots = vec![SlotView {
            key: next_slot_key(),
            mid: Mid::from("a"),
            max_height: 1080,
            min_height: 0,
            track: &t,
            priority: 0,
            current_quality: LayerQuality::High,
        }];
        let decisions = AllocationEngine::compute(bw(10_000), &slots);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "expected Forward fallback when High is bad, got {:?}",
            decisions[slots[0].key]
        );
    }

    // ─── Property: forwarded layer is always a healthy layer ────────────────────

    #[test]
    fn forwarded_layer_is_always_healthy() {
        let t = track_with_bad_layer(LayerQuality::High);
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots);
        if let AllocationDecision::Forward(receiver, _) = &decisions[slots[0].key] {
            assert!(
                receiver.state.is_healthy(),
                "engine forwarded to an unhealthy layer: {:?}",
                receiver.quality
            );
        }
    }

    // ─── Property: higher-priority slot is preferred when budget is tight ────────
    //
    // Two slots, only enough bandwidth for one Low layer.  The slot with the
    // higher priority (max_height) should be forwarded; the other paused.

    #[test]
    fn tight_budget_forwards_higher_priority_slot() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);

        // Budget just fits one Low layer (no headroom for downgrade guard).
        let available = bw((low_bps as u64) / 1_000 + 5);

        let slots = vec![
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("h"),
                max_height: 1080,
                min_height: 0,
                track: &t,
                priority: 200,
                current_quality: LayerQuality::Low,
            },
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("l"),
                max_height: 360,
                min_height: 0,
                track: &t,
                priority: 0,
                current_quality: LayerQuality::Low,
            },
        ];

        let decisions = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "high-priority slot should be forwarded first"
        );
        assert!(
            matches!(decisions[slots[1].key], AllocationDecision::Pause(..)),
            "low-priority slot should be paused when budget is tight"
        );
    }

    proptest! {
        #[ignore]
        #[test]
        fn allocation_is_order_independent_for_equal_priority_slots(n in 2usize..=5) {
            let t = healthy_track();
            let low_bps = layer_bps(&t, LayerQuality::Low);

            // Budget just barely covers one Low layer.
            let available = bw((low_bps as u64) / 1_000 + 1);
            let priority = 720;

            let mid_names: Vec<String> = (0..n).map(|i| format!("m{}", i)).collect();
            let slots: Vec<SlotView> = mid_names
                .iter()
                .map(|name| slot(name, priority, &t, LayerQuality::Low))
                .collect();

            let decisions1 = AllocationEngine::compute(available, &slots);

            // Reorder the input slots and verify outcome stays the same.
            let mut reversed = slots.clone();
            reversed.reverse();
            let decisions2 = AllocationEngine::compute(available, &reversed);

            prop_assert_eq!(decisions1.len(), decisions2.len());
            for s in &slots {
                prop_assert_eq!(
                    decisions1.get(s.key),
                    decisions2.get(s.key),
                    "decisions differ for slot {} when input order changes",
                    s.mid
                );
            }
        }
    }
    // ─── Property: desired bitrate reflects the best healthy layer, not the
    //               forwarded layer ──────────────────────────────────────────────
    //
    // desired should equal the sum of the highest healthy layer bitrate across
    // all slots, regardless of what was actually forwarded.

    #[test]
    fn desired_bitrate_equals_sum_of_best_healthy_layers() {
        let t = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
        ];

        let expected_per_slot = t
            .layers
            .iter()
            .filter(|l| l.state.is_healthy())
            .map(|l| l.state.bitrate_bps())
            .fold(0.0_f64, f64::max);

        let expected_total = expected_per_slot * slots.len() as f64;

        let desired = AllocationEngine::desired_bitrate(&slots);

        assert!(
            (desired.as_f64() - expected_total).abs() < 1.0,
            "desired {:.0} bps != expected {:.0} bps",
            desired.as_f64(),
            expected_total
        );
    }

    // ─── Property: downgrade hysteresis absorbs small bandwidth noise ────────────
    //
    // If bandwidth drops only slightly below the current layer cost (within the
    // 10% DOWNGRADE_FACTOR dead-band), the engine should keep forwarding the
    // current layer rather than dropping to a lower one.

    #[test]
    fn downgrade_hysteresis_absorbs_minor_bandwidth_noise() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);

        // 5% below Low cost — inside the downgrade dead-band; no downgrade should fire.
        let available = bw((low_bps * 0.95) as u64 / 1_000);

        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "engine downgraded or paused inside the hysteresis dead-band"
        );
    }

    // ─── Property: empty slot list produces empty decisions + zero desired ────────

    #[test]
    fn no_slots_yields_empty_decisions_and_zero_desired() {
        let decisions = AllocationEngine::compute(bw(1_000), &[]);
        assert!(
            decisions.is_empty(),
            "expected no decisions for empty slots"
        );
        assert_eq!(
            AllocationEngine::desired_bitrate(&[]).as_f64(),
            0.0,
            "expected zero desired bitrate for empty slots"
        );
    }

    // ─── Budget-floor invariants ────────────────────────────────────────────────

    #[test]
    fn tight_budget_pauses_floored_slot() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let low_h = AllocationEngine::height(t.by_quality(LayerQuality::Low).unwrap());

        let tight = bw((low_bps * 0.5) as u64 / 1_000);
        let slots = vec![qos_slot("a", 1080, low_h, 0, &t, LayerQuality::Low)];

        let decisions = AllocationEngine::compute(tight, &slots);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Pause(..)),
            "budget below floor must pause; got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn tight_budget_pauses_lower_priority_slot() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);

        let available = bw((low_bps as u64) / 1_000 + 5);
        let slots = sorted(vec![
            qos_slot("hi", 1080, 0, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, 0, 0, &t, LayerQuality::Low),
        ]);

        let decisions = AllocationEngine::compute(available, &slots);
        let hi = slots.iter().find(|s| s.priority == 100).unwrap();
        let lo = slots.iter().find(|s| s.priority == 0).unwrap();

        assert!(
            matches!(decisions[hi.key], AllocationDecision::Forward(..)),
            "high-priority slot must forward"
        );
        assert!(
            matches!(decisions[lo.key], AllocationDecision::Pause(..)),
            "lower-priority slot must pause when budget is exhausted"
        );
    }

    #[test]
    fn tight_budget_pauses_all_slots() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let low_h = AllocationEngine::height(t.by_quality(LayerQuality::Low).unwrap());

        let tight = bw((low_bps * 0.3) as u64 / 1_000);
        let slots = sorted(vec![
            qos_slot("hi", 1080, low_h, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, low_h, 0, &t, LayerQuality::Low),
        ]);

        let decisions = AllocationEngine::compute(tight, &slots);
        let hi = slots.iter().find(|s| s.priority == 100).unwrap();
        let lo = slots.iter().find(|s| s.priority == 0).unwrap();

        assert!(
            matches!(decisions[hi.key], AllocationDecision::Pause(..)),
            "high-priority slot must pause when budget is insufficient"
        );
        assert!(
            matches!(decisions[lo.key], AllocationDecision::Pause(..)),
            "low-priority slot must pause when budget is insufficient"
        );
    }

    // ─── Property: a single slot with a single healthy layer always forwards ──────

    #[test]
    fn single_slot_single_layer_always_forwards() {
        // Mark Medium and High as bad so only Low is healthy.
        let t = track_with_bad_layer(LayerQuality::High);
        t.by_quality(LayerQuality::Medium)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);

        let low_bps = layer_bps(&t, LayerQuality::Low);
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];

        // Bandwidth comfortably covers the only healthy layer.
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let decisions = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "single healthy layer should always be forwarded when budget allows"
        );
    }

    #[test]
    fn always_forward_lowest_layer() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        // Budget covers the lowest layer but not the next one up.
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let decisions = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "lowest layer must be forwarded when the budget affords it"
        );
    }

    /// When "f"/"h"/"q" are negotiated but only "f" (High) and "h" (Medium)
    /// are active, a client requesting max_height=180 (which only the inactive
    /// "q"/Low would satisfy) must still receive a Forward decision rather than
    /// being silently dropped. The engine should pick the lowest active layer
    /// (Medium/"h") as the closest-rank fallback.
    #[test]
    fn closest_rank_fallback_when_low_layer_inactive() {
        use crate::rtp::monitor::StreamQuality;

        let t = healthy_track();
        // Mark Low/"q" inactive — only High and Medium are publishing.
        t.by_quality(LayerQuality::Low)
            .unwrap()
            .state
            .update_for_test()
            .quality(StreamQuality::Bad);

        let med_bps = layer_bps(&t, LayerQuality::Medium);

        // Client requests max_height=180 (only the inactive Low would normally fit).
        let slots = sorted(vec![qos_slot("a", 180, 0, 0, &t, LayerQuality::Low)]);
        let available = bw((med_bps * 2.0) as u64 / 1_000);
        let decisions = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "expected Forward (closest-rank fallback) when the spatially-preferred layer is inactive"
        );

        if let AllocationDecision::Forward(layer, _) = decisions[slots[0].key] {
            assert!(
                layer.state.is_healthy(),
                "forwarded layer must be healthy; got {:?}",
                layer.quality
            );
        }
    }
}

#[cfg(test)]
mod slot_switch_tests {
    use super::*;
    use crate::entity::ParticipantId;
    use crate::log::LogCtx;
    use crate::rtp::cache::StreamCache;
    use crate::rtp::conformance::assert_decodable;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
    use crate::track::test_utils::make_video_track;
    use crate::track::{StreamWrite, StreamWriter};
    use pulsebeam_runtime::rand::seeded_rng;
    use str0m::media::SimulcastLayer;

    fn test_rng() -> impl RngCore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(9000);
        seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(&mut test_rng()),
        }
    }

    /// A slot with two simulcast layers of one track available to switch between.
    struct Fixture {
        slot: Slot,
        high: TrackLayer,
        low: TrackLayer,
        writer: StreamWriter,
        emitted: Vec<RtpPacket>,
    }

    impl Fixture {
        fn new() -> Self {
            let pid = ParticipantId::new(&mut test_rng());
            let (_, track) = make_video_track(
                pid,
                Mid::from("v0"),
                vec![
                    SimulcastLayer::new("f"),
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("q"),
                ],
            );
            let high = track.by_quality(LayerQuality::High).unwrap().clone();
            let low = track.by_quality(LayerQuality::Low).unwrap().clone();

            let mut slot = Slot::new(test_ctx(), SlotConfig::default(), &mut test_rng());
            slot.paused = false;

            Self {
                slot,
                high,
                low,
                writer: StreamWriter::new(),
                emitted: Vec::new(),
            }
        }

        /// Mirrors `route_video`: cache first, then hand the packet to the slot.
        fn ingest(&mut self, layer: &TrackLayer, cache: &mut StreamCache, pkt: &RtpPacket) -> bool {
            cache.push(pkt);
            let stream_id = layer.stream_id();
            let promoted = self
                .slot
                .on_rtp(&stream_id, pkt, Some(cache), &mut self.writer);
            while let Some(w) = self.writer.pop() {
                if let StreamWrite::Video { pkt, .. } = w {
                    self.emitted.push(pkt);
                }
            }
            promoted
        }

        fn ingest_all(
            &mut self,
            layer: &TrackLayer,
            cache: &mut StreamCache,
            pkts: &[RtpPacket],
        ) -> bool {
            let mut promoted = false;
            for p in pkts {
                promoted |= self.ingest(layer, cache, p);
            }
            promoted
        }
    }

    #[test]
    fn slot_switch_emits_a_decodable_stream_to_the_subscriber() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi_cache = StreamCache::new();
        let mut lo_cache = StreamCache::new();
        let mut hi = H264StreamBuilder::new(1, 300, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut lo = H264StreamBuilder::new(2, 40_000, 600_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        let kf = hi.keyframe(3);
        assert!(fx.ingest_all(&high, &mut hi_cache, &kf));

        for _ in 0..10 {
            let f = hi.delta_frame(3);
            fx.ingest_all(&high, &mut hi_cache, &f);
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &mut lo_cache, &f);
        }

        fx.slot.switch_to(&low, false);
        let kf = lo.keyframe(2);
        assert!(
            fx.ingest_all(&low, &mut lo_cache, &kf),
            "a fresh keyframe must promote the staged layer"
        );
        for _ in 0..5 {
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &mut lo_cache, &f);
        }

        assert_eq!(
            fx.slot.active.as_ref().map(|l| l.stream_id()),
            Some(low.stream_id())
        );
        assert_decodable(&fx.emitted, "Slot::on_rtp across a simulcast switch");
    }

    #[test]
    fn a_stale_gop_defers_the_switch_and_keeps_the_current_layer_flowing() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi_cache = StreamCache::new();
        let mut lo_cache = StreamCache::new();
        let mut hi = H264StreamBuilder::new(1, 300, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        // The low layer's keyframe is far in the past; its GOP is long.
        let mut lo = H264StreamBuilder::new(2, 40_000, 600_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        fx.ingest_all(&high, &mut hi_cache, &hi.keyframe(3));
        let lo_kf = lo.keyframe(2);
        fx.ingest_all(&low, &mut lo_cache, &lo_kf);

        // Push the low layer's segment past the replay window.
        let frames = 10;
        for _ in 0..frames {
            let f = hi.delta_frame(3);
            fx.ingest_all(&high, &mut hi_cache, &f);
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &mut lo_cache, &f);
        }

        let before = fx.emitted.len();
        fx.slot.switch_to(&low, false);
        for _ in 0..3 {
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &mut lo_cache, &f);
        }

        assert_eq!(
            fx.slot.active.as_ref().map(|l| l.stream_id()),
            Some(high.stream_id()),
            "a stale GOP must not be burst at the subscriber"
        );
        assert_eq!(
            fx.emitted.len(),
            before,
            "the staged layer must not leak packets before it is promoted"
        );

        // The high layer keeps flowing while we wait.
        let f = hi.delta_frame(3);
        fx.ingest_all(&high, &mut hi_cache, &f);
        assert!(fx.emitted.len() > before);

        // PLI is what unblocks the switch.
        let mut sink = crate::participant::event::test_utils::MockParticipantSink::new();
        fx.slot.pli_retry(Instant::now(), &mut sink);
        assert_eq!(
            sink.request_keyframe_calls.first().map(|c| c.0),
            Some(low.stream_id()),
            "a deferred switch must keep asking the publisher for a keyframe"
        );

        // And once the publisher answers, the switch completes.
        let kf = lo.keyframe(2);
        assert!(fx.ingest_all(&low, &mut lo_cache, &kf));
        assert_eq!(
            fx.slot.active.as_ref().map(|l| l.stream_id()),
            Some(low.stream_id())
        );
        assert_decodable(&fx.emitted, "deferred switch after PLI");
    }

    #[test]
    fn many_slot_switches_stay_decodable_and_never_reuse_a_sequence_number() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi_cache = StreamCache::new();
        let mut lo_cache = StreamCache::new();
        let mut hi = H264StreamBuilder::new(1, 65_000, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::AggregatedWithIdr);
        let mut lo = H264StreamBuilder::new(2, 65_500, 4_000_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        fx.ingest_all(&high, &mut hi_cache, &hi.keyframe(3));

        for round in 0..30 {
            for _ in 0..4 {
                let f = hi.delta_frame(3);
                fx.ingest_all(&high, &mut hi_cache, &f);
                let f = lo.delta_frame(2);
                fx.ingest_all(&low, &mut lo_cache, &f);
            }
            let to_low = round % 2 == 0;
            let target = if to_low { low.clone() } else { high.clone() };
            fx.slot.switch_to(&target, false);
            if to_low {
                let kf = lo.keyframe(2);
                fx.ingest_all(&low, &mut lo_cache, &kf);
                let f = hi.delta_frame(3);
                fx.ingest_all(&high, &mut hi_cache, &f);
            } else {
                let kf = hi.keyframe(3);
                fx.ingest_all(&high, &mut hi_cache, &kf);
                let f = lo.delta_frame(2);
                fx.ingest_all(&low, &mut lo_cache, &f);
            }
        }

        assert_decodable(&fx.emitted, "30 slot switches");

        let mut seen = std::collections::HashMap::new();
        for (i, p) in fx.emitted.iter().enumerate() {
            if let Some(prev) = seen.insert(*p.seq_no, i) {
                panic!("output seq {} emitted at both {prev} and {i}", *p.seq_no);
            }
        }
    }
}
