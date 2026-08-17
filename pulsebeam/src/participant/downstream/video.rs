use crate::bitrate::{BitrateController, BitrateControllerConfig};
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::frame_selector::DecodeTargetSelection;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;
use slotmap::SlotMap;
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::entity::{ParticipantId, TrackId};
use crate::keys::DownstreamSlotKey;
use crate::log::{LogCtx, plog_debug, plog_error, plog_info, plog_trace, plog_warn};
use crate::rtp::monitor::StreamStats;
use crate::shard::router::TrackKey;
use crate::track::{LayerQuality, StreamId, StreamWriter, Track, TrackLayer, TrackMeta};

/// Video slots preallocated per participant.
///
/// A starting capacity, not a bound. Nothing here may assume a participant has
/// few slots: the negotiated limit is expected to rise, so anything that walks
/// slots has to stay cheap as it does.
const VIDEO_MAX_SLOTS: usize = 25;

/// How long to wait before the *first* PLI retry while a slot is transitioning.
///
/// A slot's opening request is routinely lost: it is made the moment the slot
/// is staged, which is often before the reverse route carrying it upstream
/// exists. Waiting a full second to find that out is a second of black screen
/// on every subscribe, and it is the largest single term in time-to-first-frame.
///
/// A faster retry cannot flood the publisher — [`KEYFRAME_DEBOUNCE`] is a
/// leading-edge 500ms on the way upstream, so the extra attempts are absorbed
/// there rather than turning into keyframes.
///
/// [`KEYFRAME_DEBOUNCE`]: crate::track::KEYFRAME_DEBOUNCE
const KEYFRAME_FIRST_RETRY: Duration = Duration::from_millis(250);

/// The interval those retries back off to, and hold at until keep-alive.
const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_millis(1000);

/// After repeated retries, continue to probe the stream with lower-frequency keep-alives.
const KEYFRAME_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum number of aggressive PLI retries before falling back to keep-alive mode.
const KEYFRAME_MAX_RETRIES: u32 = 5;

/// Match str0m's congestion-controller floor without adding a second SFU floor.
pub const MIN_ESTIMATE: Bitrate = Bitrate::kbps(40);

/// Where the estimate starts before any feedback has arrived.
///
/// libwebrtc's default start bitrate. Optimistic enough to reach a usable layer
/// quickly, low enough that a constrained first link is not immediately
/// overdriven.
pub const START_BANDWIDTH: Bitrate = Bitrate::kbps(300);

pub const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);

/// What the SFU announces to str0m as the starting estimate.
///
/// libwebrtc's `kDefaultStartBitrateBps`. A start bitrate is a prior, not a
/// measurement, and the allocator spends against it before a single packet has
/// been acknowledged — so it has to be a number a link is not embarrassed by.
///
/// This was 2 Mbps, which is what you need when probing cannot ramp you.
/// Probing can: `set_desired_bitrate` drives probe clusters, and the pacer
/// sends each recommendation as one burst, fills it with padding large enough
/// to measure against jitter, and paces at the committed rate rather than the
/// estimate. From 300 kbps that reaches 80% of a 3 Mbps link in 3.3s and 90% in
/// 3.5s, first frame at 361ms.
///
/// The 2 Mbps was leftover compensation, and it was expensive. On a fixed
/// 400 kbps cellular link the allocator funded ~1.3 Mbps against it: the queue
/// reached 65ms, the link dropped 37 packets and the viewer froze for 5.07s —
/// 51% of the run — before the estimate walked down to 345 kbps. Six BWE plans
/// failed on it at seeds 16, 17 and 19.
pub const INITIAL_BANDWIDTH: Bitrate = Bitrate::kbps(300);

pub struct VideoAllocator {
    routes: Vec<(TrackId, DownstreamSlotKey)>,
    slots: SlotMap<DownstreamSlotKey, Slot>,

    // Cold
    ctx: LogCtx,
    manual_sub: bool,
    tracks: Vec<Track>,
    layer_states: LayerStates,
    last_reconciled: HashSet<(TrackId, DownstreamSlotKey)>,
    desired_ctrl: BitrateController,
    current_allocation: Bitrate,
    allocation: Allocation,
}

impl VideoAllocator {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool) -> Self {
        let desired_ctrl = BitrateControllerConfig {
            min_bitrate: START_BANDWIDTH,
            max_bitrate: MAX_BANDWIDTH,
            default_bitrate: INITIAL_BANDWIDTH,
            ..Default::default()
        }
        .build();
        Self {
            layer_states: LayerStates::new(),
            ctx,
            manual_sub,
            tracks: Vec::new(),
            slots: slotmap::SlotMap::with_capacity_and_key(VIDEO_MAX_SLOTS),
            routes: Vec::new(),
            last_reconciled: HashSet::new(),
            desired_ctrl,
            current_allocation: Bitrate::ZERO,
            allocation: Allocation::new(),
        }
    }

    pub fn add_track(&mut self, track: Track) {
        for existing in &self.tracks {
            if existing.meta.id == track.meta.id {
                return;
            }
        }
        plog_info!(self.ctx, track = %track.meta.id, "video track added");
        self.tracks.push(track);
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let old_len = self.tracks.len();
        self.tracks.retain(|track| track.meta.id != *track_id);
        if old_len == self.tracks.len() {
            return false;
        }
        plog_info!(self.ctx, track = %track_id, "video track removed");
        // Stop any slot currently targeting the removed track so reconcile_routes
        // fires StreamUnsubscribed and cleans up the routing table.
        for slot in self.slots.values_mut() {
            if slot.matches_track_id(track_id) {
                slot.stop();
            }
        }
        self.rebalance();
        true
    }

    /// Replace a track's measurements with the shard's latest snapshot.
    ///
    /// Pushed by the shard when the publisher's numbers move, not carried on
    /// packets: an allocation pass landing between a new snapshot and the next
    /// arriving packet would otherwise decide against the previous one.
    pub fn update_layer_states(&mut self, track_id: TrackId, states: &crate::track::TrackStates) {
        for (rid, stats) in states {
            self.layer_states.insert((track_id, *rid), *stats);
        }
    }

    pub fn update_layer_states_slot(
        &mut self,
        slot_key: DownstreamSlotKey,
        states: &crate::track::TrackStates,
    ) {
        let Some(slot) = self.slots.get(slot_key) else {
            debug_assert!(false, "compiled downstream slot must resolve");
            return;
        };
        let Some(track_id) = slot.target().map(|layer| layer.meta.id) else {
            return;
        };
        self.update_layer_states(track_id, states);
    }

    /// Seed measurements directly, standing in for media already flowing.
    #[cfg(test)]
    pub(crate) fn seed_layer_states(&mut self, states: &LayerStates) {
        self.layer_states
            .extend(states.iter().map(|(k, v)| (*k, *v)));
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        let layer_states = &self.layer_states;
        for (_key, slot) in &mut self.slots {
            let tracks = &mut self.tracks;
            if let Some(intent) = intents.get(&slot.mid) {
                Self::configure_slot(tracks, layer_states, slot, Some(intent));
            } else {
                Self::configure_slot(tracks, layer_states, slot, None);
            }
        }
    }

    /// Routes this slot to the given track at the specified QoS, or stops
    /// routing if `track_id` is `None` or `intent.max_height` is 0.
    fn configure_slot(
        tracks: &mut [Track],
        layer_states: &LayerStates,
        slot: &mut Slot,
        intent: Option<&Intent>,
    ) -> Option<()> {
        if let Some(intent) = intent
            && intent.target_height > 0
        {
            let track_id = &intent.track_id;
            let Some(track_state) = Self::track_mut_in(tracks, track_id) else {
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
                let Some(layer) = track_state.lowest_healthy_quality(|l| {
                    layer_states
                        .get(&l.stream_id())
                        .is_some_and(crate::rtp::monitor::StreamStats::is_healthy)
                }) else {
                    slot.stop();
                    return None;
                };
                layer
            };

            let layer = layer.clone();
            slot.max_height = intent.target_height;
            slot.min_height = intent.min_height;
            slot.min_fps = intent.min_fps;
            slot.priority = intent.priority;
            slot.switch_to(&layer, false);
        } else {
            slot.max_height = 0;
            slot.min_height = 0;
            slot.min_fps = 0;
            slot.priority = 0;
            slot.stop();
        }

        Some(())
    }

    pub fn tracks(&self) -> impl Iterator<Item = &TrackMeta> {
        self.tracks.iter().map(|track| &track.meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> + '_ {
        self.slots.values().filter_map(|s| {
            Some(SlotAssignment {
                mid: s.mid,
                paused: s.paused || matches!(s.state(), SlotState::Idle | SlotState::Starting),
                track: {
                    let layer = s.target()?;
                    self.track(&layer.meta.id)?.meta.clone()
                },
            })
        })
    }

    pub fn has_slot(&self, mid: Mid) -> bool {
        self.slots.values().any(|s| s.mid == mid)
    }

    pub fn refresh_ssrc(&mut self, mid: Mid, rid: Option<Rid>, ssrc: Ssrc) -> bool {
        for slot in self.slots.values_mut() {
            if slot.mid == mid && slot.rid == rid {
                slot.ssrc = ssrc;
                return true;
            }
        }
        false
    }

    pub fn add_slot(&mut self, config: SlotConfig) {
        if self.has_slot(config.mid) {
            plog_debug!(self.ctx, mid = %config.mid, "video slot already provisioned; skipping duplicate");
            return;
        }
        let slot = Slot::new(self.ctx, config);
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
            .filter_map(|s| s.desired.as_ref().map(|t| t.meta.id))
            .collect();

        let mut pending_tracks = self
            .tracks
            .iter()
            .filter(|track| !already_assigned.contains(&track.meta.id));

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
                let states = &self.layer_states;
                let Some(layer) = track_state.lowest_healthy_quality(|l| {
                    states
                        .get(&l.stream_id())
                        .is_some_and(crate::rtp::monitor::StreamStats::is_healthy)
                }) else {
                    continue;
                };
                slot.switch_to(layer, true);
                staged = staged.saturating_add(1);
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

    pub fn update_allocations(
        &mut self,
        hold: Bitrate,
        climb: Bitrate,
    ) -> (Bitrate, bool, Option<Bitrate>) {
        let hold = hold.max(MIN_ESTIMATE).min(MAX_BANDWIDTH);
        let climb = climb.max(MIN_ESTIMATE).min(hold);
        self.allocation
            .rebuild(&self.slots, &self.tracks, &self.layer_states);
        self.current_allocation = self.allocation.allocated();
        self.allocation.run(hold, climb);
        let desired_raw = self.allocation.desired();
        let desired = self
            .desired_ctrl
            .update(desired_raw)
            .max(self.current_allocation);
        debug_assert!(self.current_allocation <= desired);

        #[cfg(feature = "sim")]
        if !self.allocation.plans.is_empty() {
            crate::sim_metrics::record_downstream_bwe_for(
                self.ctx.participant_id,
                crate::bitrate::saturating_bps(hold.as_f64()),
                crate::bitrate::saturating_bps(desired.as_f64()),
            );
            for plan in &self.allocation.plans {
                let quality = plan
                    .chosen
                    .and_then(|chosen| self.allocation.rung(plan, chosen))
                    .map(|rung| rung.quality as u8);
                crate::sim_metrics::record_forwarded_quality_for(plan.origin, quality);
            }
        }

        let unfunded = self.allocation.unfunded();
        let changed = self.allocation.apply(&mut self.slots, &self.tracks);

        if changed {
            log_allocation(self.ctx, hold, desired, &self.allocation);
        }

        (desired, changed, unfunded)
    }

    pub fn current_allocation(&self) -> Bitrate {
        self.current_allocation
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) -> Option<&TrackLayer> {
        for slot in self.slots.values() {
            if slot.mid == req.mid && slot.rid == req.rid {
                return slot.target();
            }
        }
        None
    }

    #[inline]
    pub fn on_rtp(
        &mut self,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        // Keep the latest snapshot for this encoding; the allocator reads these
        // instead of a field on TrackLayer.
        //
        // Overwritten, not inserted once. These were handles into a live
        // monitor, so caching the first one was enough — the values behind it
        // kept moving. They are values now, so a first-write-wins cache would
        // freeze the allocator on whatever it happened to see first.
        let mut slot_key = None;
        for (route_track, route_slot) in &self.routes {
            if *route_track == track_id {
                slot_key = Some(*route_slot);
                break;
            }
        }
        let Some(slot_key) = slot_key else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(slot_key) else {
            plog_warn!(self.ctx, "no slot found for track {:?}", track_id);
            return false;
        };
        slot.on_rtp(track_id, pkt, cache, writer)
    }

    #[inline]
    pub fn on_rtp_slot(
        &mut self,
        slot_key: DownstreamSlotKey,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(slot_key) else {
            debug_assert!(false, "compiled downstream slot must resolve");
            return false;
        };
        let Some(track_id) = slot.target().map(|layer| layer.meta.id) else {
            return false;
        };
        slot.on_rtp(track_id, pkt, cache, writer)
    }

    pub(crate) fn poll_slow(
        &mut self,
        now: Instant,
        _bandwidth: Bitrate,
        events: &mut impl ParticipantSink,
        fanouts: &HashMap<TrackId, TrackKey>,
    ) {
        self.reconcile_routes(events);
        self.retry_keyframe_requests(now, events, fanouts);
    }

    fn retry_keyframe_requests(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
        fanouts: &HashMap<TrackId, TrackKey>,
    ) {
        for (_, slot) in &mut self.slots {
            slot.pli_retry(now, events, fanouts);
        }
    }

    pub(crate) fn reconcile_routes(&mut self, events: &mut impl ParticipantSink) {
        let mut current = HashSet::new();
        for (slot_key, slot) in &self.slots {
            // Subscribe to every stream the switcher needs packets for — active,
            // staging (awaiting its keyframe), and draining (completing its tail)
            // — plus the assigned target so a just-assigned or paused-but-staged
            // slot stays subscribed. All resolve to a track id via StreamId.0.
            for stream in [
                slot.switcher.active_stream(),
                slot.switcher.staging_stream(),
                slot.switcher.draining_stream(),
            ]
            .into_iter()
            .flatten()
            {
                current.insert((stream.0, slot_key));
            }
            if let Some(desired) = slot.desired.as_ref() {
                current.insert((desired.meta.id, slot_key));
            }
        }

        let mut removed = Vec::new();
        self.routes.retain(|route| {
            if current.contains(route) {
                true
            } else {
                removed.push(*route);
                false
            }
        });

        for (track_id, slot_key) in removed {
            if self.last_reconciled.contains(&(track_id, slot_key))
                && let Some(track) = self.track(&track_id)
            {
                events.unsubscribe(track.meta.clone(), slot_key);
            }
        }

        for (track_id, slot_key) in &current {
            if self
                .routes
                .iter()
                .all(|route| route != &(*track_id, *slot_key))
            {
                self.routes.push((*track_id, *slot_key));
                if let Some(track) = self.track(track_id) {
                    events.subscribe(track.meta.clone(), *slot_key);
                }
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

    #[cfg(test)]
    fn has_route(&self, track_id: &TrackId) -> bool {
        self.routes
            .iter()
            .any(|(route_track, _)| route_track == track_id)
    }

    #[cfg(test)]
    fn route_slot(&self, track_id: &TrackId) -> Option<DownstreamSlotKey> {
        for (route_track, slot_key) in &self.routes {
            if route_track == track_id {
                return Some(*slot_key);
            }
        }
        None
    }

    #[cfg(test)]
    fn set_route(&mut self, track_id: TrackId, slot_key: DownstreamSlotKey) {
        self.routes
            .retain(|(route_track, _)| *route_track != track_id);
        self.routes.push((track_id, slot_key));
    }

    /// Returns `true` if every track ID appears in at most one slot's
    /// assigned target.  A track must never be assigned to two slots
    /// simultaneously, because that would cause duplicate stream forwarding
    /// and corrupt the routing table.
    fn no_duplicate_slot_assignments(&self) -> bool {
        let mut seen = HashSet::new();
        for (slot_key, slot) in &self.slots {
            if let Some(layer) = slot.desired.as_ref()
                && !seen.insert(layer.meta.id)
            {
                let mut first_slot = None;
                for (candidate_key, candidate_slot) in &self.slots {
                    if candidate_key != slot_key
                        && candidate_slot
                            .desired
                            .as_ref()
                            .is_some_and(|target| target.meta.id == layer.meta.id)
                    {
                        first_slot = Some(candidate_key);
                        break;
                    }
                }
                if first_slot.is_some() {
                    plog_error!(
                        self.ctx,
                        track = %layer.meta.id,
                        first_slot = ?first_slot,
                        second_slot = ?slot_key,
                        "duplicate track assigned to multiple slots"
                    );
                    return false;
                }
            }
        }
        true
    }

    fn track(&self, track_id: &TrackId) -> Option<&Track> {
        Self::track_in(&self.tracks, track_id)
    }

    #[allow(
        clippy::manual_find,
        reason = "the routing guard keeps stable-id discovery out of the forwarding path"
    )]
    fn track_in<'a>(tracks: &'a [Track], track_id: &TrackId) -> Option<&'a Track> {
        for track in tracks {
            if track.meta.id == *track_id {
                return Some(track);
            }
        }
        None
    }

    #[allow(
        clippy::manual_find,
        reason = "the routing guard keeps stable-id discovery out of the forwarding path"
    )]
    fn track_mut_in<'a>(tracks: &'a mut [Track], track_id: &TrackId) -> Option<&'a mut Track> {
        for track in tracks {
            if track.meta.id == *track_id {
                return Some(track);
            }
        }
        None
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

    /// The layer this slot is assigned to serve — the most recent BWE choice.
    /// This is policy: it carries the quality/height the allocator reasons about.
    /// What is *actually* being forwarded (active / staging / draining streams)
    /// and every step of moving between them is owned by `switcher`.
    desired: Option<TrackLayer>,

    switcher: Switcher,

    mid: Mid,
    rid: Option<Rid>,
    max_height: u32,
    min_height: u32,
    min_fps: u32,
    priority: u32,
    paused: bool,

    /// Number of PLI retries sent for the current staging layer.
    staging_keyframe_retries: u32,
    /// When the last PLI retry was sent for the current staging layer.
    staging_keyframe_last_at: Option<Instant>,
    /// Current retry interval for PLI probes while waiting for the staging keyframe.
    staging_keyframe_interval: Duration,
}

impl Slot {
    fn new(ctx: LogCtx, cfg: SlotConfig) -> Self {
        Self {
            ctx,
            mid: cfg.mid,
            rid: cfg.rid,
            ssrc: cfg.ssrc,
            pt: cfg.pt,

            desired: None,

            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            // With no signaling, we assume users are viewing with 720p playback
            max_height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
            paused: true,

            staging_keyframe_retries: 0,
            staging_keyframe_last_at: None,
            staging_keyframe_interval: KEYFRAME_FIRST_RETRY,
        }
    }

    fn target(&self) -> Option<&TrackLayer> {
        self.desired.as_ref()
    }

    fn active_stream(&self) -> Option<StreamId> {
        self.switcher.active_stream()
    }

    fn decode_target(&self) -> DecodeTargetSelection {
        self.switcher.decode_target()
    }

    fn awaiting_switch(&self) -> bool {
        self.switcher.awaiting_switch()
    }

    fn state(&self) -> SlotState {
        match (
            self.switcher.active_stream().is_some(),
            self.switcher.staging_stream().is_some(),
        ) {
            (false, false) => SlotState::Idle,
            (false, true) => SlotState::Starting,
            (true, false) => SlotState::Stable,
            (true, true) => SlotState::Switching,
        }
    }

    fn pli_reset(&mut self) {
        self.staging_keyframe_retries = 0;
        self.staging_keyframe_last_at = None;
        self.staging_keyframe_interval = KEYFRAME_FIRST_RETRY;
    }

    fn pli_retry(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
        fanouts: &HashMap<TrackId, TrackKey>,
    ) {
        if self.paused {
            return;
        }
        // The switcher is the authority on whether a switch is still pending; the
        // layer it is waiting on is the one this slot is assigned to (`desired`).
        if !self.switcher.awaiting_switch() {
            return;
        }
        let Some(staging) = self.desired.as_ref() else {
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
        let reached_keepalive =
            !keepalive_mode && retries.saturating_add(1) == KEYFRAME_MAX_RETRIES;
        if !keepalive_mode {
            self.staging_keyframe_retries = self.staging_keyframe_retries.saturating_add(1);
        }
        self.staging_keyframe_last_at = Some(now);

        if !keepalive_mode && !reached_keepalive {
            self.staging_keyframe_interval = self
                .staging_keyframe_interval
                .saturating_mul(2)
                .min(KEYFRAME_RETRY_INTERVAL);
        }

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

        events.request_keyframe(staging, fanouts.get(&staging.stream_id().0).copied());
    }

    fn switch_to(&mut self, new_layer: &TrackLayer, force: bool) -> bool {
        let mut changed = false;
        let is_track_change = self
            .desired
            .as_ref()
            .map(|l| l.meta.id)
            .is_none_or(|id| id != new_layer.meta.id);

        // A forced track change hard-resets the switcher so no stale stream state
        // from the previous track leaks across.
        if force && is_track_change && self.switcher.active_stream().is_some() {
            self.switcher.stop();
            changed = true;
        }

        if self.desired.as_ref() != Some(new_layer) {
            self.desired = Some(new_layer.clone());
            changed = true;
        }

        // Drive the switcher toward the new target. It is the authority on the
        // active/staging/draining streams; observe whether that changed.
        let before = (
            self.switcher.active_stream(),
            self.switcher.staging_stream(),
        );
        self.switcher.switch_to(new_layer.stream_id());
        let after = (
            self.switcher.active_stream(),
            self.switcher.staging_stream(),
        );
        if before != after {
            self.pli_reset();
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, new_target=?new_layer.stream_id(), "slot switching target");
        }

        if self.paused {
            self.paused = false;
            changed = true;
            plog_debug!(self.ctx, mid=%self.mid, new_target=?new_layer.stream_id(), "slot resumed from paused state");
        }

        changed
    }

    /// Set the decode target the switcher forwards the active encoding at — `Full`
    /// for every frame, or a lowered target that sheds temporal/spatial layers.
    /// Returns whether it changed.
    fn set_decode_target(&mut self, target: DecodeTargetSelection) -> bool {
        if self.switcher.decode_target() == target {
            return false;
        }
        self.switcher.set_decode_target(target);
        true
    }

    fn stop(&mut self) {
        plog_debug!(self.ctx, mid=%self.mid, "slot stopped");
        self.desired = None;
        self.switcher.stop();
        self.pli_reset();
    }

    fn pause_at(&mut self, layer: &TrackLayer) -> bool {
        let mut changed = false;

        if self.desired.as_ref() != Some(layer) {
            self.desired = Some(layer.clone());
            changed = true;
        }

        // Stop forwarding but stay staged on the layer, so the route stays
        // subscribed for a quick resume. `paused` gates `feed`, and `pli_retry`
        // already skips paused slots, so no keyframes are requested meanwhile.
        if self.switcher.active_stream().is_some()
            || self.switcher.staging_stream() != Some(layer.stream_id())
        {
            self.switcher.stop();
            self.switcher.switch_to(layer.stream_id());
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
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        if self.paused {
            plog_trace!(self.ctx, mid=%self.mid, track=?track_id, "slot paused, dropping incoming packet");
            return false;
        }
        let Some(cache) = cache else {
            return false;
        };

        // The switcher owns the entire switching state machine; hand it the
        // whole track cache and let it emit whatever the subscriber should see. A
        // change in the active stream means a switch was promoted this tick.
        let (mid, rid, ssrc, pt) = (self.mid, self.rid, self.ssrc, self.pt);
        let before = self.switcher.active_stream();
        self.switcher
            .feed(track_id, cache, pkt.arrival_ts, &mut |out| {
                writer.write_video_owned(out, mid, rid, ssrc, pt);
            });
        self.switcher.active_stream() != before
    }

    fn matches_track_id(&self, track_id: &TrackId) -> bool {
        self.switcher
            .active_stream()
            .is_some_and(|s| s.0 == *track_id)
            || self
                .switcher
                .staging_stream()
                .is_some_and(|s| s.0 == *track_id)
            || self
                .switcher
                .draining_stream()
                .is_some_and(|s| s.0 == *track_id)
            || self
                .desired
                .as_ref()
                .is_some_and(|l| l.meta.id == *track_id)
    }
}

#[cfg(test)]
impl Slot {
    /// The stream the switcher is actively forwarding.
    fn test_active(&self) -> Option<crate::track::StreamId> {
        self.switcher.active_stream()
    }

    /// The stream the switcher is awaiting a keyframe on before switching.
    fn test_staging(&self) -> Option<crate::track::StreamId> {
        self.switcher.staging_stream()
    }

    /// Force the slot's assigned layer and the switcher's stream roles, for
    /// tests that need to construct a state without feeding real packets.
    fn set_roles_for_test(&mut self, active: Option<&TrackLayer>, staging: Option<&TrackLayer>) {
        self.desired = staging.or(active).cloned();
        self.switcher.test_set_roles(
            active.map(crate::track::TrackLayer::stream_id),
            staging.map(crate::track::TrackLayer::stream_id),
        );
    }

    /// Simulate the burst landing: the staged stream becomes active.
    fn test_promote(&mut self) {
        self.switcher.test_promote();
    }
}

fn log_allocation(ctx: LogCtx, hold: Bitrate, desired: Bitrate, allocation: &Allocation) {
    let mut reports = Vec::with_capacity(allocation.plans.len());
    let mut total_used_bps = 0u64;

    for plan in &allocation.plans {
        let entry = if let Some(rung) = plan.chosen.and_then(|chosen| allocation.rung(plan, chosen))
        {
            total_used_bps = total_used_bps.saturating_add(u64::from(rung.send_bps));
            let quality = match rung.quality {
                LayerQuality::High => "H",
                LayerQuality::Medium => "M",
                LayerQuality::Low => "L",
            };
            let suffix = match rung.target {
                DecodeTargetSelection::Full => "",
                DecodeTargetSelection::Target(_) => "b",
            };
            format!(
                "{}:{}{}({}/{}bps)",
                plan.mid, quality, suffix, rung.send_bps, rung.price_bps
            )
        } else {
            format!("{}:PAUSE", plan.mid)
        };
        reports.push(entry);
    }

    plog_info!(
        ctx,
        %hold,
        used = %Bitrate::bps(total_used_bps),
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
    pub target_height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}

pub type LayerStates = HashMap<StreamId, StreamStats>;

const MAX_ENCODINGS_PER_SLOT: usize = 3;
const MAX_RUNGS_PER_SLOT: usize = MAX_ENCODINGS_PER_SLOT * crate::rtp::monitor::MAX_LADDER_TARGETS;
const RESERVE_FRACTION: f64 = 0.10;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rung {
    layer: u8,
    target: DecodeTargetSelection,
    price_bps: u32,
    send_bps: u32,
    height: u32,
    fps: u32,
    quality: LayerQuality,
}

#[derive(Clone, Copy)]
struct SlotPlan {
    key: DownstreamSlotKey,
    mid: Mid,
    origin: ParticipantId,
    track: u16,
    priority: u32,
    floored: bool,
    first: u32,
    len: u8,
    pause_layer: u8,
    held: Option<u8>,
    active_layer: Option<u8>,
    switch_pending: bool,
    chosen: Option<u8>,
}

struct Allocation {
    rungs: Vec<Rung>,
    plans: Vec<SlotPlan>,
}

impl Allocation {
    fn new() -> Self {
        Self {
            rungs: Vec::with_capacity(VIDEO_MAX_SLOTS * MAX_RUNGS_PER_SLOT),
            plans: Vec::with_capacity(VIDEO_MAX_SLOTS),
        }
    }

    fn rebuild(
        &mut self,
        slots: &SlotMap<DownstreamSlotKey, Slot>,
        tracks: &[Track],
        states: &LayerStates,
    ) {
        self.rungs.clear();
        self.plans.clear();

        for (key, slot) in slots {
            let Some(target) = slot.target() else {
                continue;
            };
            let Some((track_index, track)) = tracks
                .iter()
                .enumerate()
                .find(|(_, track)| track.meta.id == target.meta.id)
            else {
                debug_assert!(false, "assigned slot track must exist");
                continue;
            };
            debug_assert!(!track.layers.is_empty());
            debug_assert!(track.layers.len() <= MAX_ENCODINGS_PER_SLOT);
            let first = self.rungs.len();
            self.build_rungs(slot, track, states);
            let len = self.rungs.len().saturating_sub(first);
            debug_assert!(len <= MAX_RUNGS_PER_SLOT);
            let pause_layer = track
                .layers
                .iter()
                .position(|layer| layer == target)
                .and_then(|index| u8::try_from(index).ok())
                .unwrap_or(0);

            let active_layer = (!slot.paused)
                .then(|| slot.active_stream())
                .flatten()
                .and_then(|stream| {
                    track
                        .layers
                        .iter()
                        .position(|layer| layer.is(&stream))
                        .and_then(|index| u8::try_from(index).ok())
                });
            let held = active_layer.and_then(|layer| {
                let target = slot.decode_target();
                self.rungs
                    .get(first..)
                    .unwrap_or_default()
                    .iter()
                    .position(|rung| rung.layer == layer && rung.target == target)
                    .and_then(|index| u8::try_from(index).ok())
            });

            self.plans.push(SlotPlan {
                key,
                mid: slot.mid,
                origin: track.meta.origin,
                track: u16::try_from(track_index).unwrap_or(u16::MAX),
                priority: slot.priority,
                floored: slot.min_height > 0,
                first: u32::try_from(first).unwrap_or(u32::MAX),
                len: u8::try_from(len).unwrap_or(u8::MAX),
                pause_layer,
                held,
                active_layer,
                switch_pending: active_layer.is_some() && slot.awaiting_switch(),
                chosen: None,
            });
        }

        self.plans.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| b.floored.cmp(&a.floored))
                .then_with(|| a.mid.cmp(&b.mid))
        });
        debug_assert!(self.plans.iter().all(|plan| {
            usize::try_from(plan.first)
                .ok()
                .and_then(|first| first.checked_add(usize::from(plan.len)))
                .is_some_and(|end| end <= self.rungs.len())
        }));
    }

    fn build_rungs(&mut self, slot: &Slot, track: &Track, states: &LayerStates) {
        let mut snapshots = [StreamStats::default(); MAX_ENCODINGS_PER_SLOT];
        let mut heights = [0u32; MAX_ENCODINGS_PER_SLOT];
        let layer_count = track.layers.len().min(MAX_ENCODINGS_PER_SLOT);
        debug_assert!(!track.layers.is_empty());
        debug_assert_eq!(layer_count, track.layers.len());
        let layers = track.layers.get(..layer_count).unwrap_or_default();

        for ((layer, snapshot_slot), height_slot) in layers
            .iter()
            .zip(snapshots.iter_mut())
            .zip(heights.iter_mut())
        {
            let snapshot = states.get(&layer.stream_id()).copied().unwrap_or_else(|| {
                StreamStats::new(
                    true,
                    layer.quality.seed_bitrate_bps(),
                    layer.quality.fallback_height(),
                )
            });
            *snapshot_slot = snapshot;
            *height_slot = if snapshot.height == 0 {
                layer.quality.fallback_height()
            } else {
                snapshot.height
            };
            debug_assert_ne!(*height_slot, 0);
            debug_assert!(snapshot.stable_bitrate_bps >= snapshot.bitrate_bps);
        }

        let snapshots = snapshots.get(..layer_count).unwrap_or_default();
        let heights = heights.get(..layer_count).unwrap_or_default();
        let min_track_height = heights.iter().copied().min().unwrap_or_default();
        let request = slot.max_height.max(min_track_height);
        let ceiling = heights
            .iter()
            .copied()
            .filter(|height| *height >= request)
            .min()
            .unwrap_or(request);
        let tallest_allowed = heights
            .iter()
            .copied()
            .filter(|height| *height <= ceiling)
            .max()
            .unwrap_or(ceiling);
        let effective_min = slot.min_height.min(tallest_allowed);

        let mut selected = [false; MAX_ENCODINGS_PER_SLOT];
        let selected = selected.get_mut(..layer_count).unwrap_or_default();
        for ((selected, snapshot), height) in selected
            .iter_mut()
            .zip(snapshots.iter())
            .zip(heights.iter())
        {
            *selected = *height <= ceiling
                && *height >= effective_min
                && snapshot.healthy
                && snapshot.bitrate_bps > 0;
        }

        if !selected.iter().any(|selected| *selected) {
            let cheapest_healthy = snapshots
                .iter()
                .enumerate()
                .filter(|(_, snapshot)| snapshot.healthy && snapshot.bitrate_bps > 0)
                .min_by_key(|(_, snapshot)| snapshot.stable_bitrate_bps)
                .map(|(index, _)| index);
            if let Some(index) = cheapest_healthy {
                let Some(selected) = selected.get_mut(index) else {
                    debug_assert!(false, "healthy layer index must resolve");
                    return;
                };
                *selected = true;
            } else {
                for (selected, height) in selected.iter_mut().zip(heights.iter()) {
                    *selected = *height <= ceiling;
                }
            }
        }

        let first = self.rungs.len();
        for (layer_index, (((layer, selected), snapshot), height)) in layers
            .iter()
            .zip(selected.iter())
            .zip(snapshots.iter())
            .zip(heights.iter())
            .enumerate()
        {
            if !*selected {
                continue;
            }
            let count = usize::from(snapshot.decode_targets.max(1))
                .min(crate::rtp::monitor::MAX_LADDER_TARGETS);
            let full_send = snapshot.bitrate_bps;
            let full_price = snapshot.stable_bitrate_bps.max(full_send);
            let envelope = f64::from(full_price) / f64::from(full_send.max(1));

            for decode_target in 0..count.saturating_sub(1) {
                let fps = decode_target_fps(snapshot.full_fps, count, decode_target);
                if fps < slot.min_fps {
                    continue;
                }
                let declared = snapshot
                    .decode_target_kbps
                    .get(decode_target)
                    .copied()
                    .unwrap_or_default()
                    .saturating_mul(1_000);
                let send_bps = if declared > 0 {
                    declared
                } else {
                    let fraction =
                        0.5 + 0.5 * decode_target as f64 / count.saturating_sub(1) as f64;
                    saturating_u32(f64::from(full_send) * fraction)
                };
                let price_bps = saturating_u32(f64::from(send_bps) * envelope);
                self.rungs.push(Rung {
                    layer: u8::try_from(layer_index).unwrap_or(u8::MAX),
                    target: DecodeTargetSelection::Target(decode_target),
                    price_bps,
                    send_bps,
                    height: *height,
                    fps,
                    quality: layer.quality,
                });
            }

            if snapshot.full_fps == 0 || snapshot.full_fps >= slot.min_fps {
                self.rungs.push(Rung {
                    layer: u8::try_from(layer_index).unwrap_or(u8::MAX),
                    target: DecodeTargetSelection::Full,
                    price_bps: full_price,
                    send_bps: full_send,
                    height: *height,
                    fps: snapshot.full_fps,
                    quality: layer.quality,
                });
            }
        }

        let Some(new_rungs) = self.rungs.get_mut(first..) else {
            debug_assert!(false, "new rung span must resolve");
            return;
        };
        new_rungs.sort_by(|a, b| {
            a.price_bps
                .cmp(&b.price_bps)
                .then_with(|| b.height.cmp(&a.height))
        });

        let mut write = first;
        let end = self.rungs.len();
        for read in first..end {
            let Some(candidate) = self.rungs.get(read).copied() else {
                debug_assert!(false, "candidate rung index must resolve");
                break;
            };
            let dominated = self
                .rungs
                .get(first..write)
                .unwrap_or_default()
                .iter()
                .any(|kept| {
                    kept.price_bps <= candidate.price_bps
                        && kept.height >= candidate.height
                        && kept.fps >= candidate.fps
                        && (kept.price_bps < candidate.price_bps
                            || kept.height > candidate.height
                            || kept.fps > candidate.fps)
                });
            if !dominated {
                let Some(destination) = self.rungs.get_mut(write) else {
                    debug_assert!(false, "destination rung index must resolve");
                    break;
                };
                *destination = candidate;
                write = write.saturating_add(1);
            }
        }
        self.rungs.truncate(write);
        debug_assert!(
            self.rungs
                .get(first..)
                .unwrap_or_default()
                .windows(2)
                .all(|pair| pair
                    .first()
                    .zip(pair.get(1))
                    .is_some_and(|(a, b)| { a.price_bps <= b.price_bps }))
        );
        debug_assert!(self.rungs.len().saturating_sub(first) <= MAX_RUNGS_PER_SLOT);
    }

    fn run(&mut self, hold: Bitrate, climb: Bitrate) {
        debug_assert!((0.0..1.0).contains(&RESERVE_FRACTION));
        let ceiling_hold = crate::bitrate::saturating_bps(hold.as_f64());
        let ceiling_climb =
            crate::bitrate::saturating_bps(climb.as_f64() * (1.0 - RESERVE_FRACTION));
        debug_assert!(ceiling_climb <= ceiling_hold);

        let rungs = &self.rungs;
        let mut spent = 0u64;
        let mut force_minimum = true;
        for plan in &mut self.plans {
            plan.chosen = None;
            if plan.len == 0 {
                continue;
            }
            let first = usize::try_from(plan.first).unwrap_or(usize::MAX);
            debug_assert!(first.saturating_add(usize::from(plan.len)) <= rungs.len());
            for index in 0..usize::from(plan.len) {
                let offset = first.saturating_add(index);
                let Some(rung) = rungs.get(offset).copied() else {
                    debug_assert!(false, "allocation rung index must resolve");
                    break;
                };
                let retaining = plan.held.is_some_and(|held| index <= usize::from(held));
                if !retaining && plan.switch_pending && plan.active_layer != Some(rung.layer) {
                    break;
                }
                let ceiling = if retaining {
                    ceiling_hold
                } else {
                    ceiling_climb
                };
                let forced_minimum = force_minimum && index == 0;
                if !forced_minimum && spent.saturating_add(u64::from(rung.price_bps)) > ceiling {
                    break;
                }
                plan.chosen = Some(u8::try_from(index).unwrap_or(u8::MAX));
            }
            if let Some(chosen) = plan.chosen {
                let offset = first.saturating_add(usize::from(chosen));
                let Some(rung) = rungs.get(offset) else {
                    debug_assert!(false, "chosen rung index must resolve");
                    continue;
                };
                spent = spent.saturating_add(u64::from(rung.price_bps));
            }
            force_minimum = false;
        }
    }

    fn desired(&self) -> Bitrate {
        debug_assert!((0.0..1.0).contains(&RESERVE_FRACTION));
        let total = self
            .plans
            .iter()
            .filter(|plan| plan.len > 0)
            .filter_map(|plan| {
                let last = plan.len.saturating_sub(1);
                self.rung(plan, last).map(|rung| u64::from(rung.price_bps))
            })
            .fold(0u64, u64::saturating_add);
        Bitrate::from(crate::bitrate::saturating_bps(
            total as f64 / (1.0 - RESERVE_FRACTION),
        ))
    }

    fn allocated(&self) -> Bitrate {
        let total = self
            .plans
            .iter()
            .filter_map(|plan| plan.held.and_then(|held| self.rung(plan, held)))
            .map(|rung| rung.send_bps)
            .map(u64::from)
            .fold(0u64, u64::saturating_add);
        Bitrate::bps(total)
    }

    fn unfunded(&self) -> Option<Bitrate> {
        self.plans
            .iter()
            .filter_map(|plan| {
                if plan.len == 0 {
                    return None;
                }
                let next = plan.chosen.map_or(0, |chosen| chosen.saturating_add(1));
                (next < plan.len)
                    .then(|| self.rung(plan, next))
                    .flatten()
                    .map(|rung| Bitrate::bps(u64::from(rung.price_bps)))
            })
            .min_by(|a, b| a.as_f64().total_cmp(&b.as_f64()))
    }

    fn apply(&self, slots: &mut SlotMap<DownstreamSlotKey, Slot>, tracks: &[Track]) -> bool {
        let mut changed = false;
        for plan in &self.plans {
            let Some(slot) = slots.get_mut(plan.key) else {
                debug_assert!(false, "allocation slot key must resolve");
                continue;
            };
            let Some(track) = tracks.get(usize::from(plan.track)) else {
                debug_assert!(false, "allocation track index must resolve");
                continue;
            };
            debug_assert_eq!(track.meta.origin, plan.origin);
            let rung = plan.chosen.and_then(|chosen| self.rung(plan, chosen));
            let layer_index = rung.map_or(plan.pause_layer, |rung| rung.layer);
            let Some(layer) = track.layers.get(usize::from(layer_index)) else {
                debug_assert!(false, "allocation layer index must resolve");
                continue;
            };

            if let Some(rung) = rung {
                changed |= slot.switch_to(layer, false);
                changed |= slot.set_decode_target(rung.target);
            } else {
                changed |= slot.pause_at(layer);
            }
        }
        changed
    }

    fn rung(&self, plan: &SlotPlan, index: u8) -> Option<Rung> {
        debug_assert!(index < plan.len);
        let first = usize::try_from(plan.first).unwrap_or(usize::MAX);
        let offset = first.saturating_add(usize::from(index));
        debug_assert!(offset < self.rungs.len());
        self.rungs.get(offset).copied()
    }
}

fn decode_target_fps(full_fps: u32, count: usize, target: usize) -> u32 {
    debug_assert!(count > 0);
    debug_assert!(target < count);
    if full_fps == 0 || target.saturating_add(1) >= count {
        full_fps
    } else {
        full_fps >> count.saturating_sub(1).saturating_sub(target)
    }
}

fn saturating_u32(value: f64) -> u32 {
    debug_assert!(value.is_finite());
    debug_assert!(value >= 0.0);
    let rounded = crate::bitrate::saturating_bps(value.ceil()).min(u64::from(u32::MAX));
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod alloc_test_support {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ParticipantId;
    use crate::track::UpstreamTrack;
    use crate::track::test_utils::make_video_track;
    use str0m::media::SimulcastLayer;

    /// Measurement handles standing in for what the publisher's shard would
    /// have supplied. Seeded active, which is what the old `inactive(false)`
    /// loops did.
    pub(super) fn states_for(track: &Track) -> LayerStates {
        track
            .layers
            .iter()
            .map(|l| {
                (
                    l.stream_id(),
                    StreamStats::new(
                        false,
                        l.quality.seed_bitrate_bps(),
                        l.quality.fallback_height(),
                    ),
                )
            })
            .collect()
    }

    pub(super) fn video_track_with_states(
        pid: ParticipantId,
        mid: Mid,
        layers: Vec<SimulcastLayer>,
    ) -> (UpstreamTrack, Track, LayerStates) {
        let (tx, track) = make_video_track(pid, mid, layers);
        let states = states_for(&track);
        (tx, track, states)
    }

    /// Measurements are values now, so a test that adjusts one mutates it in
    /// place rather than reaching through a shared handle.
    pub(super) fn state_of_mut<'a>(
        states: &'a mut LayerStates,
        layer: &TrackLayer,
    ) -> &'a mut StreamStats {
        states
            .get_mut(&layer.stream_id())
            .expect("layer must have seeded state")
    }
}

#[cfg(test)]
mod assignment_tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::alloc_test_support::*;
    use super::*;
    use crate::entity::{ParticipantId, TrackId, TrackKind};
    use crate::participant::event::test_utils::MockParticipantSink;
    use crate::rtp::RtpPacket;
    use crate::track::{LayerQuality, UpstreamTrack};

    use str0m::bwe::Bitrate;
    use str0m::media::{Mid, SimulcastLayer};

    struct TestTracks {
        pub senders: Vec<UpstreamTrack>,
        pub ids: Vec<TrackId>,
    }

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(),
        }
    }

    fn setup_allocator() -> VideoAllocator {
        VideoAllocator::new(test_ctx(), false)
    }

    fn add_tracks(allocator: &mut VideoAllocator, count: usize) -> TestTracks {
        let pid = ParticipantId::new();

        let mut senders = Vec::new();
        let mut ids = Vec::new();

        for i in 0..count {
            let mid = Mid::from(&format!("v{i}")[..]);
            let (tx, track, states) = video_track_with_states(pid, mid, vec![]);
            let meta = tx.meta.clone();

            ids.push(meta.id);
            allocator.seed_layer_states(&states);
            allocator.add_track(Track {
                meta,
                layers: track.layers,
                reverse: None,
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
    fn allocation_ladder_exposes_per_encoding_decode_targets() {
        let pid = ParticipantId::new();
        let (tx, built, mut states) = video_track_with_states(
            pid,
            Mid::from("v0"),
            vec![SimulcastLayer::new("q"), SimulcastLayer::new("h")],
        );
        let track = Track {
            meta: tx.meta,
            layers: built.layers,
            reverse: None,
        };

        let scalable = track.by_quality(LayerQuality::Medium).unwrap();
        state_of_mut(&mut states, scalable).set_decode_target_count(3);
        let plain = track.by_quality(LayerQuality::Low).unwrap();

        let mut slots = SlotMap::with_key();
        let mut slot = Slot::new(test_ctx(), SlotConfig::default());
        slot.switch_to(plain, false);
        slots.insert(slot);
        let mut allocation = Allocation::new();
        allocation.rebuild(&slots, std::slice::from_ref(&track), &states);

        assert!(allocation.rungs.iter().any(|rung| {
            rung.quality == LayerQuality::Medium
                && matches!(rung.target, DecodeTargetSelection::Target(_))
        }));
        assert!(allocation.rungs.iter().all(|rung| {
            rung.quality != LayerQuality::Low || matches!(rung.target, DecodeTargetSelection::Full)
        }));
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
                target_height: 720,
                min_height: 0,
                min_fps: 0,
                priority: 0,
            },
        );
        intents.insert(
            Mid::from("s1"),
            Intent {
                track_id: tracks.ids[1],
                target_height: 720,
                min_height: 0,
                min_fps: 0,
                priority: 0,
            },
        );
        intents.insert(
            Mid::from("s2"),
            Intent {
                track_id: tracks.ids[2],
                target_height: 720,
                min_height: 0,
                min_fps: 0,
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

        let missing_track_id =
            ParticipantId::new().derive_track_id(TrackKind::Video, &Mid::from("missing"));
        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s0"),
            Intent {
                track_id: missing_track_id,
                target_height: 720,
                min_height: 0,
                min_fps: 0,
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

        let track = allocator.track(&tracks.ids[0]).unwrap();
        let low = track
            .lowest_quality()
            .expect("video track has a layer")
            .clone();
        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(None, Some(&low));
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
        allocator.retry_keyframe_requests(now, &mut queue, &HashMap::new());
        assert_eq!(
            queue.request_keyframe_calls.len(),
            1,
            "retry_keyframe_requests should not send an immediate duplicate PLI after reconcile_routes"
        );
    }

    #[test]
    fn staging_preserves_old_route_until_switch_complete() {
        let pid = ParticipantId::new();
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let track_layers = vec![
            SimulcastLayer::new("q"),
            SimulcastLayer::new("h"),
            SimulcastLayer::new("f"),
        ];
        let (tx, track, states) = video_track_with_states(pid, mid, track_layers);
        let track_id = tx.meta.id;
        allocator.seed_layer_states(&states);
        allocator.add_track(Track {
            meta: tx.meta,
            layers: track.layers,
            reverse: None,
        });
        add_slots(&mut allocator, 1);

        let track = allocator.track(&track_id).unwrap();
        let low = track
            .lowest_quality()
            .expect("video track has a layer")
            .clone();
        let high = track.by_quality(LayerQuality::High).unwrap().clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(Some(&low), Some(&high));
        slot.paused = false;

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert!(allocator.has_route(&low.meta.id));
        assert!(allocator.has_route(&high.meta.id));
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

        let track = allocator.track(&tracks.ids[0]).unwrap();
        let old_stream_id = track
            .lowest_quality()
            .expect("video track has a layer")
            .stream_id();
        let slot_key = allocator.slots.keys().next().unwrap();
        allocator.set_route(old_stream_id.0, slot_key);
        allocator
            .last_reconciled
            .insert((old_stream_id.0, slot_key));

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(None, None);
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

        let track = allocator.track(&tracks.ids[0]).unwrap();
        let low = track
            .lowest_quality()
            .expect("video track has a layer")
            .clone();
        let slot_keys: Vec<_> = allocator.slots.keys().collect();
        let correct_slot_key = slot_keys[0];
        let stale_slot_key = slot_keys[1];

        let slot = allocator.slots.get_mut(correct_slot_key).unwrap();
        slot.set_roles_for_test(Some(&low), None);
        slot.paused = false;

        allocator.set_route(low.meta.id, stale_slot_key);

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert_eq!(allocator.route_slot(&low.meta.id), Some(correct_slot_key));
        assert_eq!(queue.subscribe_calls.len(), 1);
    }

    #[test]
    fn does_not_promote_staging_before_staging_packets() {
        let pid = ParticipantId::new();
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (tx, track, states) = video_track_with_states(
            pid,
            mid,
            vec![SimulcastLayer::new("h"), SimulcastLayer::new("f")],
        );
        allocator.seed_layer_states(&states);
        allocator.add_track(Track {
            meta: tx.meta.clone(),
            layers: track.layers,
            reverse: None,
        });
        add_slots(&mut allocator, 1);

        let track = allocator.track(&tx.meta.id).unwrap();
        let high = track.by_quality(LayerQuality::High).unwrap().clone();
        let medium = track.by_quality(LayerQuality::Medium).unwrap().clone();

        let slot_key = allocator.slots.keys().next().unwrap();
        let slot = allocator.slots.get_mut(slot_key).unwrap();
        slot.set_roles_for_test(Some(&high), Some(&medium));
        slot.paused = false;

        let pkt = RtpPacket {
            seq_no: 1.into(),
            ..Default::default()
        };

        let mut writer = crate::track::StreamWriter::new();
        slot.on_rtp(high.meta.id, &pkt, None, &mut writer);

        assert_eq!(
            slot.test_active(),
            Some(high.stream_id()),
            "no keyframe on the staged layer yet, so the active layer stays"
        );
        assert_eq!(slot.test_staging(), Some(medium.stream_id()));
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
    fn staged_slot_declares_demand_before_it_is_forwarding() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let (desired, _, _) =
            allocator.update_allocations(Bitrate::from(5_000_000), Bitrate::from(5_000_000));
        assert!(desired.as_f64() > 0.0);
        assert_eq!(allocator.current_allocation(), Bitrate::ZERO);
        assert_eq!(allocator.allocation.plans[0].chosen, Some(0));
        assert!(allocator.current_allocation() <= desired);
    }

    #[test]
    fn switch_to_same_active_layer_is_idempotent() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let track_id = tracks.ids[0];
        let layer = allocator
            .track(&track_id)
            .unwrap()
            .lowest_quality()
            .expect("video track has a layer")
            .clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(Some(&layer), None);
        slot.paused = false;

        assert!(
            !slot.switch_to(&layer, false),
            "re-applying the same active layer should not mark a change"
        );
    }

    #[test]
    fn switch_to_accepts_ongoing_upgrade() {
        let pid = ParticipantId::new();
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track, mut states) = video_track_with_states(
            pid,
            mid,
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            state_of_mut(&mut states, layer).set_inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let staging = track.by_quality(LayerQuality::Medium).unwrap().clone();
        let new_stage = track.by_quality(LayerQuality::High).unwrap().clone();

        slot.set_roles_for_test(None, Some(&staging));
        slot.paused = false;

        assert!(slot.switch_to(&new_stage, false));
        assert_eq!(slot.test_staging(), Some(new_stage.stream_id()));
    }

    #[test]
    fn switch_to_cancels_transition_when_target_reverts_to_active() {
        let pid = ParticipantId::new();
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track, mut states) = video_track_with_states(
            pid,
            mid,
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            state_of_mut(&mut states, layer).set_inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let active = track.by_quality(LayerQuality::Low).unwrap().clone();
        let staging = track.by_quality(LayerQuality::High).unwrap().clone();

        slot.set_roles_for_test(Some(&active), Some(&staging));
        slot.paused = false;

        assert!(slot.switch_to(&active, false));
        assert!(slot.test_staging().is_none());
        assert_eq!(slot.test_active(), Some(active.stream_id()));
    }

    #[test]
    fn switch_to_allows_downgrade_during_transition() {
        let pid = ParticipantId::new();
        let mut allocator = setup_allocator();

        let mid = Mid::from("v0");
        let (_, track, mut states) = video_track_with_states(
            pid,
            mid,
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let mut track = track;
        for layer in &mut track.layers {
            state_of_mut(&mut states, layer).set_inactive(false);
        }

        allocator.add_slot(SlotConfig::default());
        let slot = allocator.slots.values_mut().next().unwrap();
        let staging = track.by_quality(LayerQuality::High).unwrap().clone();
        let new_stage = track.by_quality(LayerQuality::Low).unwrap().clone();

        slot.set_roles_for_test(None, Some(&staging));
        slot.paused = false;

        assert!(slot.switch_to(&new_stage, false));
        assert_eq!(slot.test_staging(), Some(new_stage.stream_id()));
    }

    #[test]
    fn force_switch_to_different_track_clears_active_immediately() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 2);
        add_slots(&mut allocator, 1);

        let t0 = allocator.track(&tracks.ids[0]).unwrap();
        let t1 = allocator.track(&tracks.ids[1]).unwrap();
        let active = t0
            .lowest_quality()
            .expect("video track has a layer")
            .clone();
        let new_target = t1
            .lowest_quality()
            .expect("video track has a layer")
            .clone();

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(Some(&active), None);
        slot.paused = false;

        assert!(slot.switch_to(&new_target, true));
        assert!(
            slot.test_active().is_none(),
            "force switch must clear active stream"
        );
        assert_eq!(
            slot.test_staging().map(|s| s.0),
            Some(new_target.meta.id),
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
        // feed path that lands the burst and makes the staged stream active).
        for slot in allocator.slots.values_mut() {
            slot.test_promote();
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
                .filter(|s| s.desired.as_ref().is_some_and(|l| l.meta.id == *id))
                .count()
        };
        for id in &tracks.ids {
            assert_eq!(
                assignment_count(id),
                1,
                "track {id:?} was assigned to more than one slot"
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
            slot.test_promote();
        }

        // Trigger rebalance with a new incoming track.
        let pid = ParticipantId::new();
        let (tx, track, states) = video_track_with_states(pid, Mid::from("late"), vec![]);
        allocator.seed_layer_states(&states);
        allocator.add_track(Track {
            meta: tx.meta,
            layers: track.layers,
            reverse: None,
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
                .filter(|s| s.desired.as_ref().is_some_and(|l| l.meta.id == *id))
                .count();
            assert_eq!(count, 1, "existing track {id:?} was double-assigned");
        }
    }

    #[test]
    fn allocator_handles_track_churn() {
        let mut allocator = setup_allocator();
        let mut tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        allocator.remove_track(&tracks.ids[1]);
        let pid = ParticipantId::new();
        let (tx, track, states) = video_track_with_states(pid, Mid::from("new_track"), vec![]);
        let meta = tx.meta.clone();
        tracks.senders.push(tx);
        allocator.seed_layer_states(&states);
        allocator.add_track(Track {
            meta,
            layers: track.layers,
            reverse: None,
        });
        assert_eq!(allocator.slots().count(), 3);
    }

    #[test]
    fn same_slot_switching_same_track_is_not_duplicate_assignment() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new();
        let (tx, track, states) = video_track_with_states(
            pid,
            Mid::from("t"),
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );

        allocator.seed_layer_states(&states);

        allocator.add_track(Track {
            meta: tx.meta,
            layers: track.layers.clone(),
            reverse: None,
        });
        add_slots(&mut allocator, 1);

        allocator.rebalance();
        let upgraded_layer = track
            .by_quality(LayerQuality::Medium)
            .expect("track should have an upgrade layer")
            .clone();

        {
            let slot = allocator.slots.values_mut().next().unwrap();
            slot.test_promote();
            slot.paused = false;
            // Force a quality transition for the same track, leaving active + staging in one slot.
            slot.switch_to(&upgraded_layer, false);
            assert!(slot.test_active().is_some());
            assert!(slot.test_staging().is_some());
            assert_eq!(
                slot.test_active().unwrap().0,
                slot.test_staging().unwrap().0
            );
        }

        assert!(allocator.no_duplicate_slot_assignments());
        assert_eq!(allocator.slots.len(), 1);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::alloc_test_support::*;
    use super::*;
    use crate::entity::ParticipantId;
    use str0m::media::{Mid, SimulcastLayer};

    fn ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("allocation").unwrap()),
            participant_id: ParticipantId::new(),
        }
    }

    fn track(mid: &str, layers: Vec<SimulcastLayer>) -> (Track, LayerStates) {
        let (tx, built, states) =
            video_track_with_states(ParticipantId::new(), Mid::from(mid), layers);
        (
            Track {
                meta: tx.meta,
                layers: built.layers,
                reverse: None,
            },
            states,
        )
    }

    fn slot(
        track: &Track,
        mid: &str,
        priority: u32,
        min_height: u32,
        active: Option<LayerQuality>,
    ) -> Slot {
        let mut slot = Slot::new(
            ctx(),
            SlotConfig {
                mid: Mid::from(mid),
                ..SlotConfig::default()
            },
        );
        slot.priority = priority;
        slot.min_height = min_height;
        let target = track.lowest_quality().expect("track has a layer");
        match active {
            Some(quality) => {
                let active = track.by_quality(quality).expect("active layer");
                slot.set_roles_for_test(Some(active), None);
                slot.paused = false;
            }
            None => {
                slot.pause_at(target);
            }
        }
        slot
    }

    fn rebuild(
        tracks: &[Track],
        states: &LayerStates,
        slots: SlotMap<DownstreamSlotKey, Slot>,
    ) -> (Allocation, SlotMap<DownstreamSlotKey, Slot>) {
        let mut allocation = Allocation::new();
        allocation.rebuild(&slots, tracks, states);
        (allocation, slots)
    }

    #[test]
    fn rungs_are_sorted_and_pareto_pruned() {
        let (track, mut states) = track(
            "v0",
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let medium = track.by_quality(LayerQuality::Medium).unwrap();
        let state = state_of_mut(&mut states, medium);
        state.decode_targets = 3;
        state.decode_target_kbps = [200, 350, 500];
        state.full_fps = 30;

        let mut slots = SlotMap::with_key();
        slots.insert(slot(&track, "s0", 0, 0, Some(LayerQuality::Low)));
        let (allocation, _) = rebuild(std::slice::from_ref(&track), &states, slots);
        let plan = allocation.plans[0];
        let span =
            &allocation.rungs[usize::try_from(plan.first).unwrap()..][..usize::from(plan.len)];

        assert!(
            span.windows(2)
                .all(|pair| pair[0].price_bps <= pair[1].price_bps)
        );
        for (index, rung) in span.iter().enumerate() {
            assert!(!span[..index].iter().any(|other| {
                other.price_bps <= rung.price_bps
                    && other.height >= rung.height
                    && other.fps >= rung.fps
                    && (other.price_bps < rung.price_bps
                        || other.height > rung.height
                        || other.fps > rung.fps)
            }));
        }
        assert!(span.iter().any(|rung| {
            rung.quality == LayerQuality::Medium
                && matches!(rung.target, DecodeTargetSelection::Target(_))
        }));
    }

    #[test]
    fn an_unreachable_height_floor_clamps_to_the_tallest_layer() {
        let (track, states) = track("v0", vec![SimulcastLayer::new("q")]);
        let mut slots = SlotMap::with_key();
        slots.insert(slot(&track, "s0", 0, 720, None));
        let (allocation, _) = rebuild(std::slice::from_ref(&track), &states, slots);

        assert_eq!(allocation.plans.len(), 1);
        assert!(!allocation.rungs.is_empty());
        assert_eq!(allocation.rungs[0].quality, LayerQuality::Low);
    }

    #[test]
    fn strict_priority_outweighs_a_lower_priority_floor() {
        let (high_track, high_states) = track("v0", vec![SimulcastLayer::new("q")]);
        let (low_track, low_states) = track("v1", vec![SimulcastLayer::new("q")]);
        let mut states = high_states;
        states.extend(low_states);
        let mut slots = SlotMap::with_key();
        let high_key = slots.insert(slot(&high_track, "z", 10, 0, None));
        let low_key = slots.insert(slot(&low_track, "a", 1, 180, None));
        let tracks = [high_track, low_track];
        let (mut allocation, _) = rebuild(&tracks, &states, slots);

        allocation.run(Bitrate::ZERO, Bitrate::ZERO);

        assert_eq!(allocation.plans[0].key, high_key);
        assert_eq!(allocation.plans[0].chosen, Some(0));
        assert_eq!(
            allocation
                .plans
                .iter()
                .find(|plan| plan.key == low_key)
                .unwrap()
                .chosen,
            None
        );
    }

    #[test]
    fn equal_priority_floored_slot_wins_the_tie() {
        let (droppable, droppable_states) = track("v0", vec![SimulcastLayer::new("q")]);
        let (floored, floored_states) = track("v1", vec![SimulcastLayer::new("q")]);
        let mut states = droppable_states;
        states.extend(floored_states);
        let mut slots = SlotMap::with_key();
        slots.insert(slot(&droppable, "a", 5, 0, None));
        let floor_key = slots.insert(slot(&floored, "z", 5, 180, None));
        let tracks = [droppable, floored];
        let (allocation, _) = rebuild(&tracks, &states, slots);

        assert_eq!(allocation.plans[0].key, floor_key);
    }

    #[test]
    fn held_rung_uses_hold_while_paused_slot_requires_climb() {
        let (first, first_states) = track("v0", vec![SimulcastLayer::new("q")]);
        let (second, second_states) = track("v1", vec![SimulcastLayer::new("q")]);
        let mut states = first_states;
        states.extend(second_states);
        let tracks = [first, second];

        let mut active_slots = SlotMap::with_key();
        active_slots.insert(slot(&tracks[0], "a", 10, 0, Some(LayerQuality::Low)));
        let active_key = active_slots.insert(slot(&tracks[1], "b", 0, 0, Some(LayerQuality::Low)));
        let (mut active, _) = rebuild(&tracks, &states, active_slots);
        let price = u64::from(active.rungs[0].price_bps);
        active.run(Bitrate::bps(price.saturating_mul(2)), Bitrate::ZERO);
        assert!(
            active
                .plans
                .iter()
                .find(|plan| plan.key == active_key)
                .unwrap()
                .chosen
                .is_some()
        );

        let mut paused_slots = SlotMap::with_key();
        paused_slots.insert(slot(&tracks[0], "a", 10, 0, Some(LayerQuality::Low)));
        let paused_key = paused_slots.insert(slot(&tracks[1], "b", 0, 0, None));
        let (mut paused, _) = rebuild(&tracks, &states, paused_slots);
        paused.run(Bitrate::bps(price.saturating_mul(2)), Bitrate::ZERO);
        assert_eq!(
            paused
                .plans
                .iter()
                .find(|plan| plan.key == paused_key)
                .unwrap()
                .chosen,
            None
        );
    }

    #[test]
    fn actual_allocation_comes_from_the_active_rung() {
        let (track, mut states) = track(
            "v0",
            vec![SimulcastLayer::new("q"), SimulcastLayer::new("h")],
        );
        let low = track.by_quality(LayerQuality::Low).unwrap();
        let medium = track.by_quality(LayerQuality::Medium).unwrap();
        state_of_mut(&mut states, low).bitrate_bps = 111_000;
        state_of_mut(&mut states, low).stable_bitrate_bps = 150_000;
        state_of_mut(&mut states, medium).bitrate_bps = 444_000;
        state_of_mut(&mut states, medium).stable_bitrate_bps = 500_000;
        let mut slots = SlotMap::with_key();
        let mut switching = slot(&track, "s0", 0, 0, Some(LayerQuality::Low));
        switching.switch_to(medium, false);
        slots.insert(switching);
        let (allocation, _) = rebuild(std::slice::from_ref(&track), &states, slots);

        assert_eq!(allocation.allocated(), Bitrate::bps(111_000));
    }

    #[test]
    fn pending_spatial_switch_cannot_start_another_climb() {
        let (track, states) = track(
            "v0",
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let low = track.by_quality(LayerQuality::Low).unwrap();
        let high = track.by_quality(LayerQuality::High).unwrap();
        let mut pending = slot(&track, "s0", 0, 0, Some(LayerQuality::Low));
        pending.set_roles_for_test(Some(low), Some(high));
        pending.paused = false;
        let mut slots = SlotMap::with_key();
        slots.insert(pending);
        let (mut allocation, _) = rebuild(std::slice::from_ref(&track), &states, slots);

        allocation.run(Bitrate::mbps(10), Bitrate::mbps(10));

        let plan = allocation.plans[0];
        let chosen = allocation
            .rung(&plan, plan.chosen.unwrap())
            .expect("chosen rung must resolve");
        assert_eq!(chosen.layer, plan.active_layer.unwrap());
    }

    #[test]
    fn desired_includes_paused_slots_and_reserve() {
        let (track, states) = track(
            "v0",
            vec![SimulcastLayer::new("q"), SimulcastLayer::new("h")],
        );
        let mut slots = SlotMap::with_key();
        slots.insert(slot(&track, "s0", 0, 0, None));
        let (allocation, _) = rebuild(std::slice::from_ref(&track), &states, slots);
        let top = allocation.rungs.last().unwrap().price_bps as f64;

        assert!((allocation.desired().as_f64() - top / (1.0 - RESERVE_FRACTION)).abs() < 2.0);
    }

    #[test]
    fn applying_zero_budget_keeps_the_top_priority_slot_visible() {
        let (track, states) = track("v0", vec![SimulcastLayer::new("q")]);
        let mut slots = SlotMap::with_key();
        let key = slots.insert(slot(&track, "s0", 0, 0, None));
        let (mut allocation, mut slots) = rebuild(std::slice::from_ref(&track), &states, slots);
        allocation.run(Bitrate::ZERO, Bitrate::ZERO);

        assert!(allocation.apply(&mut slots, std::slice::from_ref(&track)));
        assert!(!slots[key].paused);
        assert_eq!(allocation.plans[0].chosen, Some(0));
    }
}

#[cfg(test)]
mod slot_switch_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::alloc_test_support::*;
    use super::*;
    use crate::entity::ParticipantId;
    use crate::log::LogCtx;
    use crate::rtp::conformance::assert_decodable;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};

    use crate::track::{StreamWrite, StreamWriter};

    use str0m::media::SimulcastLayer;

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(),
        }
    }

    /// A slot with two simulcast layers of one track available to switch between.
    struct Fixture {
        slot: Slot,
        high: TrackLayer,
        low: TrackLayer,
        cache: TrackStreamCache,
        writer: StreamWriter,
        emitted: Vec<RtpPacket>,
    }

    impl Fixture {
        fn new() -> Self {
            let pid = ParticipantId::new();
            let (_, track, _states) = video_track_with_states(
                pid,
                Mid::from("v0"),
                vec![
                    SimulcastLayer::new("q"),
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("f"),
                ],
            );
            let high = track.by_quality(LayerQuality::High).unwrap().clone();
            let low = track.by_quality(LayerQuality::Low).unwrap().clone();

            let mut slot = Slot::new(test_ctx(), SlotConfig::default());
            slot.paused = false;

            Self {
                slot,
                high,
                low,
                cache: TrackStreamCache::new(),
                writer: StreamWriter::new(),
                emitted: Vec::new(),
            }
        }

        /// Mirrors `route_video`: stamp the encoding's rid (as ingress does),
        /// push into the track cache, then hand the packet to the slot.
        fn ingest(&mut self, layer: &TrackLayer, pkt: &RtpPacket) -> bool {
            let track_id = layer.meta.id;
            let mut pkt = pkt.clone();
            pkt.ext_vals.rid = layer.rid;
            self.cache.push(pkt.clone());
            let promoted = self
                .slot
                .on_rtp(track_id, &pkt, Some(&self.cache), &mut self.writer);
            while let Some(w) = self.writer.pop() {
                if let StreamWrite::Video { pkt, .. } = w {
                    self.emitted.push(pkt);
                }
            }
            promoted
        }

        fn ingest_all(&mut self, layer: &TrackLayer, pkts: &[RtpPacket]) -> bool {
            let mut promoted = false;
            for p in pkts {
                promoted |= self.ingest(layer, p);
            }
            promoted
        }
    }

    #[test]
    fn slot_switch_emits_a_decodable_stream_to_the_subscriber() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi = H264StreamBuilder::new(1, 300, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut lo = H264StreamBuilder::new(2, 40_000, 600_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        let kf = hi.keyframe(3);
        assert!(fx.ingest_all(&high, &kf));

        for _ in 0..10 {
            let f = hi.delta_frame(3);
            fx.ingest_all(&high, &f);
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &f);
        }

        fx.slot.switch_to(&low, false);
        let kf = lo.keyframe(2);
        assert!(
            fx.ingest_all(&low, &kf),
            "a fresh keyframe must promote the staged layer"
        );
        for _ in 0..5 {
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &f);
        }

        assert_eq!(fx.slot.test_active(), Some(low.stream_id()));
        assert_decodable(&fx.emitted, "Slot::on_rtp across a simulcast switch");
    }

    #[test]
    fn a_stale_gop_defers_the_switch_and_keeps_the_current_layer_flowing() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi = H264StreamBuilder::new(1, 300, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        // The low layer's keyframe is far in the past; its GOP is long.
        let mut lo = H264StreamBuilder::new(2, 40_000, 600_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        fx.ingest_all(&high, &hi.keyframe(3));
        let lo_kf = lo.keyframe(2);
        fx.ingest_all(&low, &lo_kf);

        // Push the low layer's segment past the replay window.
        let frames = 10;
        for _ in 0..frames {
            let f = hi.delta_frame(3);
            fx.ingest_all(&high, &f);
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &f);
        }

        let before = fx.emitted.len();
        fx.slot.switch_to(&low, false);
        for _ in 0..3 {
            let f = lo.delta_frame(2);
            fx.ingest_all(&low, &f);
        }

        assert_eq!(
            fx.slot.test_active(),
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
        fx.ingest_all(&high, &f);
        assert!(fx.emitted.len() > before);

        // PLI is what unblocks the switch.
        let mut sink = crate::participant::event::test_utils::MockParticipantSink::new();
        fx.slot
            .pli_retry(Instant::now(), &mut sink, &HashMap::new());
        assert_eq!(
            sink.request_keyframe_calls.first().map(|c| c.0),
            Some(low.stream_id()),
            "a deferred switch must keep asking the publisher for a keyframe"
        );

        // And once the publisher answers, the switch completes.
        let kf = lo.keyframe(2);
        assert!(fx.ingest_all(&low, &kf));
        assert_eq!(fx.slot.test_active(), Some(low.stream_id()));
        assert_decodable(&fx.emitted, "deferred switch after PLI");
    }

    #[test]
    fn many_slot_switches_stay_decodable_and_never_reuse_a_sequence_number() {
        let t0 = Instant::now();
        let mut fx = Fixture::new();
        let (high, low) = (fx.high.clone(), fx.low.clone());
        let mut hi = H264StreamBuilder::new(1, 65_000, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::AggregatedWithIdr);
        let mut lo = H264StreamBuilder::new(2, 65_500, 4_000_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);

        fx.slot.switch_to(&high, false);
        fx.ingest_all(&high, &hi.keyframe(3));

        for round in 0..30 {
            for _ in 0..4 {
                let f = hi.delta_frame(3);
                fx.ingest_all(&high, &f);
                let f = lo.delta_frame(2);
                fx.ingest_all(&low, &f);
            }
            let to_low = round % 2 == 0;
            let target = if to_low { low.clone() } else { high.clone() };
            fx.slot.switch_to(&target, false);
            if to_low {
                let kf = lo.keyframe(2);
                fx.ingest_all(&low, &kf);
                let f = hi.delta_frame(3);
                fx.ingest_all(&high, &f);
            } else {
                let kf = hi.keyframe(3);
                fx.ingest_all(&high, &kf);
                let f = lo.delta_frame(2);
                fx.ingest_all(&low, &f);
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
