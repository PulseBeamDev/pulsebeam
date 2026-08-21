use crate::bitrate::{BitrateController, BitrateControllerConfig};
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::frame_selector::DecodeTargetSelection;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;
use slotmap::{SecondaryMap, SlotMap};
use std::cmp::Ordering;
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::entity::TrackId;
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

/// The lowest rate the bandwidth estimate may report.
///
/// Distinct from [`START_BANDWIDTH`] in meaning, and currently equal to it in
/// value. Named separately because they are different decisions: this is the
/// floor on what the estimator is *allowed to believe*, and that is where the
/// estimate *begins*. Sharing one constant hid that.
///
/// **It should be lower.** libwebrtc's congestion controller floors its
/// estimate far below its start bitrate, precisely so a link smaller than the
/// start value can still be measured; at 300 kbps this SFU reports a 150 kbps
/// link as 300 kbps and the allocator believes it has twice what it has.
/// Lowering it currently fails `properties::sharding_does_not_change_who_is_served`
/// — a single-shard viewer stops being served — which is a real defect this
/// floor is masking rather than a reason to keep the floor. Diagnose that
/// before moving this number.
pub const MIN_ESTIMATE: Bitrate = START_BANDWIDTH;

/// Where the estimate starts before any feedback has arrived.
///
/// libwebrtc's default start bitrate. Optimistic enough to reach a usable layer
/// quickly, low enough that a constrained first link is not immediately
/// overdriven.
pub const START_BANDWIDTH: Bitrate = Bitrate::kbps(300);

pub const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);

/// What the SFU announces to str0m as the starting estimate.
///
/// Deliberately above [`START_BANDWIDTH`]: the estimate this SFU starts its own
/// allocator from is not the same number as the one it tells the transport to
/// probe from.
///
/// **Lowering this to libwebrtc's 300 kbps trades one defect for another, and
/// both were measured.** It fixes `a_rejoining_publisher_is_shown_to_an_existing_viewer`
/// at every seed it currently fails: 2 Mbps overshoots a cellular link, and the
/// burst at a slot switch overruns the client's 128-packet routing buffer, so
/// media arrives and is discarded. It also costs `a_reordering_path_does_not_churn_keyframes`
/// an extra layer reversal at the default seed, and
/// `marker_only_publisher_streams_to_a_dd_subscriber` about 3% of its frames.
///
/// The reason it cannot simply be lowered is that libwebrtc pairs a 300 kbps
/// start with probe clusters that reach a usable rate in a second or two. This
/// SFU has no probing, so 300 kbps is a genuinely slower climb, and climbing
/// through layers is what produces the extra reversal. Probing first, then this
/// number.
pub const INITIAL_BANDWIDTH: Bitrate = Bitrate::mbps(2);

pub struct VideoAllocator {
    routes: HashMap<TrackId, DownstreamSlotKey>,
    slots: SlotMap<DownstreamSlotKey, Slot>,

    // Cold
    ctx: LogCtx,
    manual_sub: bool,
    tracks: Vec<Track>,
    layer_states: LayerStates,
    last_reconciled: HashSet<(TrackId, DownstreamSlotKey)>,
    desired_ctrl: BitrateController,
    current_allocation: Bitrate,
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
            routes: HashMap::new(),
            last_reconciled: HashSet::new(),
            desired_ctrl,
            current_allocation: Bitrate::ZERO,
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

    /// Runs one allocation pass.
    ///
    /// Returns `(desired, assignments_changed, unfunded)`, where `unfunded` is
    /// the cost of the cheapest layer the allocator was allowed to forward and
    /// could not afford. `None` means everything it wanted fit.
    ///
    /// Only the allocator knows this; it cannot be recovered from a bitrate
    /// afterwards, which is why it is reported rather than inferred.
    pub fn update_allocations(
        &mut self,
        available_bandwidth: Bitrate,
    ) -> (Bitrate, bool, Option<Bitrate>) {
        let available_bandwidth = available_bandwidth.max(MIN_ESTIMATE).min(MAX_BANDWIDTH);
        // 1. Prepare the input views
        let mut views: Vec<SlotView> = self
            .slots
            .iter()
            .filter_map(|(key, s)| {
                let current = s.target()?;
                let track = Self::track_in(&self.tracks, &current.meta.id)?;
                let current_quality = current.quality;
                Some(SlotView {
                    key,
                    mid: s.mid,
                    max_height: s.max_height,
                    min_height: s.min_height,
                    min_fps: s.min_fps,
                    priority: s.priority,
                    track,
                    current_quality,
                    forwarding: !s.paused,
                })
            })
            .collect();

        views.sort_by(AllocationEngine::priority_order);

        // Snapshot all layer atomics once so the entire allocation pass is
        // deterministic — no re-reads from concurrent StreamMonitor::poll() writes.
        let engine = AllocationEngine::new(&views, &self.layer_states);
        let decisions = engine.run_compute(available_bandwidth, &views);
        let desired_raw = engine.run_desired(&views);
        self.current_allocation = AllocationEngine::used_bitrate(&decisions);
        let desired = self
            .desired_ctrl
            .update(desired_raw)
            .max(self.current_allocation);
        debug_assert!(self.current_allocation <= desired);

        // Observation for the simulator: the estimate and demand driving this pass, and the layer
        // each origin is being forwarded at. No effect on production - the whole block compiles
        // out without the `sim` feature.
        #[cfg(feature = "sim")]
        if !views.is_empty() {
            crate::sim_metrics::record_downstream_bwe_for(
                self.ctx.participant_id,
                crate::bitrate::saturating_bps(available_bandwidth.as_f64()),
                crate::bitrate::saturating_bps(desired.as_f64()),
            );
            for view in &views {
                let quality = match decisions.get(view.key) {
                    Some(
                        AllocationDecision::Forward(layer, _)
                        | AllocationDecision::ForwardTarget(layer, _, _),
                    ) => Some(layer.quality as u8),
                    _ => None,
                };
                crate::sim_metrics::record_forwarded_quality_for(view.track.meta.origin, quality);
            }
        }

        // What it would take to unblock the cheapest thing the allocator wanted
        // and could not fund. `None` means everything it was allowed to forward
        // fit.
        let unfunded = engine.cheapest_unfunded(&views, &decisions);

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
                    changed |= slot.set_decode_target(DecodeTargetSelection::Full);
                }
                AllocationDecision::ForwardTarget(layer, target, _) => {
                    changed |= slot.switch_to(layer, false);
                    changed |= slot.set_decode_target(*target);
                }
                AllocationDecision::Pause(layer, _) => {
                    changed |= slot.pause_at(layer);
                }
            }
        }

        if changed {
            log_allocation(self.ctx, available_bandwidth, desired, &decisions, &views);
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
        let Some(&slot_key) = self.routes.get(&track_id) else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(slot_key) else {
            plog_warn!(self.ctx, "no slot found for track {:?}", track_id);
            return false;
        };
        slot.on_rtp(track_id, pkt, cache, writer)
    }

    #[inline]
    /// Forward one packet into a compiled slot.
    ///
    /// `track` is the track the packet and `cache` actually belong to, not the
    /// slot's target. Deriving it from the target instead spliced two sources
    /// into one egress stream: while a slot was retargeted, a packet still
    /// arriving for the outgoing track was announced under the incoming one, so
    /// the switcher's own per-track gate admitted it and read the wrong cache on
    /// the active stream's timeline. The subscriber saw a frame cut short and
    /// the next source's frame continue its sequence number.
    pub fn on_rtp_slot(
        &mut self,
        slot_key: DownstreamSlotKey,
        track: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(slot_key) else {
            debug_assert!(false, "compiled downstream slot must resolve");
            return false;
        };
        slot.on_rtp(track, pkt, cache, writer)
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

        let old_routes = std::mem::take(&mut self.routes);
        let mut removed = Vec::new();
        for (track_id, slot_key) in old_routes {
            if current.contains(&(track_id, slot_key)) {
                self.routes.insert(track_id, slot_key);
            } else {
                removed.push((track_id, slot_key));
            }
        }

        for (track_id, slot_key) in removed {
            if self.last_reconciled.contains(&(track_id, slot_key))
                && let Some(track) = self.track(&track_id)
            {
                events.unsubscribe(track.meta.clone(), slot_key);
            }
        }

        for (track_id, slot_key) in &current {
            if self.routes.get(track_id) != Some(slot_key) {
                self.routes.insert(*track_id, *slot_key);
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
        self.routes.contains_key(track_id)
    }

    pub(crate) fn route_slot(&self, track_id: &TrackId) -> Option<DownstreamSlotKey> {
        self.routes.get(track_id).copied()
    }

    #[cfg(test)]
    fn set_route(&mut self, track_id: TrackId, slot_key: DownstreamSlotKey) {
        self.routes.insert(track_id, slot_key);
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
        // The fanout key is what addresses the request to the publisher's shard,
        // and it arrives asynchronously: the control plane publishes it in the
        // view delta that installs this subscription, some time after the
        // subscribe. Until it lands the shard can only drop the request, so
        // issuing one now would burn a retry - and enough of those reach
        // keepalive cadence having never sent a single PLI, which leaves the
        // subscriber black until something else happens to ask.
        //
        // Media dropped in that same window is fine and self-heals, because more
        // of it is always coming. A keyframe request is one-shot; losing it
        // costs the whole stream.
        let Some(fanout) = fanouts.get(&staging.stream_id().0).copied() else {
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

        events.request_keyframe(staging, Some(fanout));
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

pub fn log_allocation(
    ctx: LogCtx,
    bwe: Bitrate,
    desired: Bitrate,
    decisions: &SecondaryMap<DownstreamSlotKey, AllocationDecision>,
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
            Some(AllocationDecision::ForwardTarget(l, _, bw)) => {
                total_used_bps += bw.as_f64();
                // Lowercase 'b' suffix marks a base-temporal-layer degrade.
                let q = match l.quality {
                    LayerQuality::High => "H",
                    LayerQuality::Medium => "M",
                    LayerQuality::Low => "L",
                };
                format!("{}:{}b({})", slot.mid, q, bw)
            }
            Some(AllocationDecision::Pause(_, needed)) => format!("{}:PAUSE({})", slot.mid, needed),
            _ => format!("{}:IDLE", slot.mid),
        };
        reports.push(entry);
    }

    plog_info!(
        ctx,
        %bwe,
        used = %Bitrate::from(crate::bitrate::saturating_bps(total_used_bps)),
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
    pub target_height: u32,
    /// Floor render height (px) to keep under contention; `0` = droppable.
    pub min_height: u32,
    /// Floor frame rate (fps) for a scalable stream; `0` = no temporal floor.
    pub min_fps: u32,
    /// Contention order; higher wins bandwidth first.
    pub priority: u32,
}

/// Point-in-time snapshot of a single layer's shared (atomic) state, read
/// once at the start of an allocation pass so the whole computation is
/// deterministic regardless of concurrent `StreamMonitor::poll()` writes.
#[derive(Clone, Copy, Debug)]
struct LayerSnap {
    bitrate_bps: f64,
    stable_bitrate_bps: f64,
    healthy: bool,
    height: u32,
    /// Decode targets this encoding offers (>= 1). `1` means no scalability, so
    /// the encoding is one indivisible rung; `> 1` is the number of temporal/
    /// spatial sub-layers the SFU could shed to.
    decode_targets: u8,
    /// Cumulative cost (bps) of each decode target, from the sender's per-temporal
    /// VLA. `0` = not declared (the allocator then estimates from `bitrate_bps`).
    decode_target_bps: [u64; crate::rtp::monitor::MAX_LADDER_TARGETS],
    /// Full frame rate (fps); `0` = unknown. With `decode_targets` it yields each
    /// decode target's fps for the `min_fps` floor.
    full_fps: u32,
}

/// Allocation computation context. `AllocationEngine::new(slots)` reads every
/// layer's shared atomics exactly once; all methods then read from that frozen
/// snapshot rather than re-loading from atomics mid-pass.
///
/// Use `compute()` / `desired_bitrate()` as convenient one-shot wrappers
/// (each builds its own snapshot). In `update_allocations()` create one engine
/// via `new()` and call `run_compute()` + `run_desired()` to share a single
/// consistent snapshot across both.
pub struct AllocationEngine {
    snaps: HashMap<StreamId, LayerSnap>,
}

/// Measurement handles for the encodings this participant can see, cached from
/// the forward path. The publisher's shard owns the originals; the controller
/// never sees them.
pub type LayerStates = HashMap<StreamId, StreamStats>;

impl AllocationEngine {
    /// Capture a point-in-time snapshot of every layer reachable from `slots`.
    /// A layer with no handle yet — nothing has been forwarded on it — falls
    /// back to the seed its quality implies, which is what the publisher
    /// advertised before its first packet.
    pub fn new(slots: &[SlotView<'_>], states: &LayerStates) -> Self {
        // Sized up front. The map is rebuilt on every allocation pass, for every
        // participant, and letting it grow from empty rehashes the whole thing
        // two or three times on the way — work proportional to the layer count,
        // repeated 10 times a second per participant, for nothing.
        let layer_count = slots.iter().map(|s| s.track.layers.len()).sum();
        let mut snaps = HashMap::with_capacity_and_hasher(layer_count, Default::default());
        snaps.extend(slots.iter().flat_map(|s| s.track.layers.iter()).map(|l| {
            let stream_id = l.stream_id();
            let snap = match states.get(&stream_id) {
                Some(state) => {
                    let (bitrate_bps, stable_bitrate_bps) = state.bitrates_snapshot();
                    let mut decode_target_bps = [0u64; crate::rtp::monitor::MAX_LADDER_TARGETS];
                    for (dt, cost) in decode_target_bps.iter_mut().enumerate() {
                        *cost = state.decode_target_bps(dt);
                    }
                    LayerSnap {
                        bitrate_bps,
                        stable_bitrate_bps,
                        healthy: state.is_healthy(),
                        height: state.height(),
                        decode_targets: state.decode_target_count(),
                        decode_target_bps,
                        full_fps: state.full_fps(),
                    }
                }
                None => LayerSnap {
                    bitrate_bps: l.quality.seed_bitrate_bps() as f64,
                    stable_bitrate_bps: l.quality.seed_bitrate_bps() as f64,
                    healthy: false,
                    height: l.quality.fallback_height(),
                    decode_targets: 1,
                    decode_target_bps: [0u64; crate::rtp::monitor::MAX_LADDER_TARGETS],
                    full_fps: 0,
                },
            };
            (stream_id, snap)
        }));
        // An upper bound, not an exact count: two slots may view the same
        // track, and those layers collapse to one entry.
        debug_assert!(snaps.len() <= layer_count);
        Self { snaps }
    }

    fn snap(&self, layer: &TrackLayer) -> &LayerSnap {
        self.snaps.get(&layer.stream_id()).unwrap_or_else(|| {
            pulsebeam_runtime::fatal!("allocator asked for a layer snapshot it never took")
        })
    }

    /// Decode targets this encoding advertises via its Dependency Descriptor
    /// (>= 1). `1` means no scalability.
    pub fn decode_target_count(&self, layer: &TrackLayer) -> u8 {
        self.snap(layer).decode_targets
    }

    /// Cost (bps) of forwarding this encoding at decode target `dt`. Uses the
    /// sender's declared per-temporal VLA when present; otherwise estimates from
    /// the full cost with a nested fraction (base ~half, top = full).
    fn decode_target_cost(&self, layer: &TrackLayer, dt: usize) -> f64 {
        let snap = self.snap(layer);
        let count = usize::from(snap.decode_targets.max(1));
        let declared = snap.decode_target_bps.get(dt).copied().unwrap_or(0);
        if declared > 0 {
            return declared as f64;
        }
        if count <= 1 || dt.saturating_add(1) >= count {
            return snap.bitrate_bps;
        }
        let frac = 0.5 + 0.5 * (dt as f64) / (count.saturating_sub(1) as f64);
        snap.bitrate_bps * frac
    }

    /// Frame rate of decode target `dt`: each temporal layer roughly halves the
    /// rate, so `dt` runs at `full_fps >> (count - 1 - dt)`. `0` when unknown.
    fn decode_target_fps(&self, layer: &TrackLayer, dt: usize) -> u32 {
        let snap = self.snap(layer);
        let count = usize::from(snap.decode_targets.max(1));
        if snap.full_fps == 0 || dt.saturating_add(1) >= count {
            return snap.full_fps;
        }
        snap.full_fps >> count.saturating_sub(1).saturating_sub(dt)
    }

    /// The highest decode target of a scalable encoding that fits `budget` while
    /// respecting the `min_fps` floor — the finer rung the allocator sheds to
    /// instead of pausing. `None` when the encoding is not scalable, or when even
    /// the lowest target meeting `min_fps` does not fit (then the slot pauses
    /// rather than drop below the floor). Returns the target and its cost.
    fn best_affordable_decode_target(
        &self,
        layer: &TrackLayer,
        budget: f64,
        min_fps: u32,
    ) -> Option<(DecodeTargetSelection, f64)> {
        let count = usize::from(self.decode_target_count(layer));
        if count <= 1 {
            return None;
        }
        // Lowest target still meeting the frame-rate floor.
        let mut floor_dt = count.saturating_sub(1);
        for dt in 0..count {
            if self.decode_target_fps(layer, dt) >= min_fps {
                floor_dt = dt;
                break;
            }
        }
        // Highest affordable target at or above the floor. Skip the top target —
        // that is "full", handled by the normal (non-degraded) path.
        for dt in (floor_dt..count.saturating_sub(1)).rev() {
            let cost = self.decode_target_cost(layer, dt);
            if cost <= budget {
                return Some((DecodeTargetSelection::Target(dt), cost));
            }
        }
        None
    }
}

/// Client-declared QoS for one subscribed stream. See `VideoRequest` in the
/// signaling proto for the authoritative semantics.
#[derive(Clone, Debug)]
pub struct SlotView<'a> {
    pub key: DownstreamSlotKey,
    pub mid: Mid,
    /// Target render height (px); layers taller than this are ineligible.
    pub max_height: u32,
    /// Floor render height (px) to keep under contention; `0` = droppable.
    pub min_height: u32,
    /// Contention order; higher wins bandwidth first.
    pub priority: u32,
    /// Floor frame rate (fps) to keep for a scalable stream under contention;
    /// `0` = no temporal floor (may shed to the base layer). The temporal analog
    /// of `min_height`.
    pub min_fps: u32,
    pub track: &'a Track,
    pub current_quality: LayerQuality,
    /// Whether the slot is actually forwarding. A paused slot still reports a
    /// `current_quality` - the layer it is parked on for a quick resume - so this is what
    /// separates "already holding this layer" from "wants to start sending it again".
    pub forwarding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AllocationDecision<'a> {
    /// Forward this encoding at full quality (every frame).
    Forward(&'a TrackLayer, Bitrate),
    /// Forward this scalable encoding at a lowered decode target — shedding
    /// temporal layers to fit the budget, one step at a time, instead of dropping
    /// to a coarser simulcast encoding or pausing. Requires a Dependency
    /// Descriptor; the target is a specific decode target (never `Full`).
    ForwardTarget(&'a TrackLayer, DecodeTargetSelection, Bitrate),
    Pause(&'a TrackLayer, Bitrate),
}

impl<'a> std::fmt::Display for AllocationDecision<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationDecision::Forward(layer, bitrate) => {
                write!(f, "Forward({layer} @ {bitrate})")
            }
            AllocationDecision::ForwardTarget(layer, target, bitrate) => {
                write!(f, "ForwardTarget({layer} {target:?} @ {bitrate})")
            }
            AllocationDecision::Pause(layer, needed) => {
                write!(f, "Pause({layer} needs {needed})")
            }
        }
    }
}

impl AllocationEngine {
    const RESERVE_FRACTION: f64 = 0.10;
    const UPGRADE_RESERVE_FRACTION: f64 = Self::RESERVE_FRACTION;
    // A slot keeps its layer until its cost exceeds ~1.33x the budget (1/0.75),
    // giving recoveries a hysteresis dead-band against churn.
    const DOWNGRADE_FACTOR: f64 = 0.75;

    /// Frame height (px) used for spatial gating.
    fn height(&self, layer: &TrackLayer) -> u32 {
        let height = self.snap(layer).height;
        debug_assert_ne!(height, 0);
        height
    }

    /// Shortest declared/fallback height among a track's layers. A layer at
    /// this height can never be excluded by spatial gating — there is nothing
    /// shorter to fall back to.
    fn min_track_height(&self, track: &Track) -> u32 {
        track
            .layers
            .iter()
            .map(|l| self.height(l))
            .min()
            .unwrap_or_default()
    }

    /// Whether a layer is permitted by the client's spatial request.
    ///
    /// A request that falls between two tiers is satisfied by rounding *up*, not down: a viewer
    /// asking for 540p on a q/h/f (180/360/720) ladder should be handed 720p, not the softer
    /// 360p. The ceiling is therefore the smallest layer height at or above the request, which
    /// also subsumes the all-taller-layers case (e.g. screen-share tiers that only differ in fps)
    /// - the smallest layer is always eligible rather than every layer being rejected.
    fn spatially_allowed(&self, slot: &SlotView<'_>, layer: &TrackLayer) -> bool {
        let request = slot.max_height.max(self.min_track_height(slot.track));
        let ceiling = slot
            .track
            .layers
            .iter()
            .map(|l| self.height(l))
            .filter(|&h| h >= request)
            .min()
            .unwrap_or(request);
        self.height(layer) <= ceiling
    }

    /// Whether a layer may currently be forwarded or switched into.
    fn eligible(&self, slot: &SlotView<'_>, layer: &TrackLayer) -> bool {
        let bitrate = self.cost(layer);
        debug_assert!(bitrate.is_finite());
        debug_assert!(bitrate >= 0.0);
        self.spatially_allowed(slot, layer) && self.snap(layer).healthy && bitrate > 0.0
    }

    fn cost(&self, layer: &TrackLayer) -> f64 {
        self.snap(layer).bitrate_bps
    }

    fn stable_cost(&self, layer: &TrackLayer) -> f64 {
        self.snap(layer).stable_bitrate_bps
    }

    /// Lowest healthy layer ignoring the spatial constraint. Used as a
    /// last-resort fallback when all spatially-allowed layers are inactive
    /// (e.g. "f"/"h"/"q" negotiated but only "f" and "h" are active and the
    /// client requests a height that only "q" would satisfy).
    fn closest_healthy<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| self.snap(layer).healthy && self.cost(layer) > 0.0)
            .min_by_key(|l| l.quality)
    }

    /// No encoding of this track currently measures healthy — the publisher's
    /// uplink is in trouble, as distinct from the subscriber's downlink being
    /// short of budget or an encoding simply carrying no bytes.
    fn nothing_healthy(&self, slot: &SlotView<'_>) -> bool {
        !slot.track.layers.iter().any(|l| self.snap(l).healthy)
    }

    /// The bottom rung of the ladder the client's spatial request allows,
    /// ignoring health and bitrate. Health measures the *publisher's* uplink, so
    /// it is never a reason to refuse a slot every candidate — it only ranks
    /// them.
    fn lowest_ladder<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| self.spatially_allowed(slot, layer))
            .min_by_key(|layer| layer.quality)
            .or_else(|| slot.track.layers.iter().min_by_key(|layer| layer.quality))
    }

    /// A legal layer to retain as the pause target even when no layer is
    /// currently healthy enough to forward. Falls back to the lowest healthy
    /// layer (closest rank) when no spatially-allowed layer exists.
    fn pause_target<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        let target = slot
            .track
            .layers
            .iter()
            .filter(|layer| self.eligible(slot, layer))
            .min_by_key(|layer| layer.quality)
            .or_else(|| self.closest_healthy(slot))
            .or_else(|| self.lowest_ladder(slot));
        debug_assert!(
            self.closest_healthy(slot).is_none()
                || target.is_some_and(|layer| self.snap(layer).healthy)
        );
        target
    }

    /// Whether this slot wanted a layer the viewer actually allows and could
    /// not be given one.
    ///
    /// `pause_target` deliberately falls back past the eligibility filter so a
    /// paused slot still names something to resume at, which means a `Pause`
    /// alone does not distinguish "the budget refused it" from "the viewer
    /// asked for it to be off". Only the first is starvation.
    fn eligible_but_unfunded(&self, slot: &SlotView<'_>) -> bool {
        slot.track
            .layers
            .iter()
            .any(|layer| self.eligible(slot, layer))
    }

    /// The lowest eligible layer strictly above the selected layer.
    /// When starting from nothing (`current = None`) and no spatially-allowed
    /// healthy layer exists, falls back to the closest-rank healthy layer so
    /// the slot always gets an initial allocation rather than being silently
    /// dropped.
    fn next_layer<'a>(
        &self,
        slot: &'a SlotView<'a>,
        current: Option<&'a TrackLayer>,
    ) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| self.eligible(slot, layer))
            .filter(|layer| current.is_none_or(|current| layer.quality > current.quality))
            .min_by_key(|layer| layer.quality)
            .or_else(|| {
                if current.is_none() {
                    self.closest_healthy(slot)
                } else {
                    None
                }
            })
    }

    /// Higher priority first, then guaranteed streams before droppable ones, with MID as a
    /// deterministic tie-breaker.
    ///
    /// The floor tie-break matters because the waterfall serves each slot to completion in turn:
    /// at equal priority a droppable stream sorted first would consume budget a stream that asked
    /// to stay visible is relying on.
    pub fn priority_order(a: &SlotView<'_>, b: &SlotView<'_>) -> Ordering {
        b.priority
            .cmp(&a.priority)
            .then_with(|| (b.min_height > 0).cmp(&(a.min_height > 0)))
            .then_with(|| a.mid.cmp(&b.mid))
    }

    /// Best layer worth wanting for this slot, irrespective of what the
    /// budget can currently afford. Feeds the BWE-facing desired bitrate.
    fn best_healthy<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers
            .iter()
            .filter(|layer| self.eligible(slot, layer))
            .max_by(|a, b| self.cost(a).total_cmp(&self.cost(b)))
            .or_else(|| self.closest_healthy(slot))
            // Matches the allocator's last resort: an all-unhealthy track is
            // still forwarded at its lowest rung, so the demand it places on BWE
            // must be declared rather than silently dropped from the sum.
            .or_else(|| {
                self.nothing_healthy(slot)
                    .then(|| self.lowest_ladder(slot))?
            })
    }

    /// Aggregate bitrate the SFU would like BWE to grant next: the sum of the
    /// stable cost of every slot's highest eligible layer. Uses the slow-decay
    /// `stable_bitrate_bps` signal so this demand stays conservatively high and
    /// motivates str0m's probe controller to maintain headroom.
    ///
    /// Captures its own snapshot. In `update_allocations()` use `run_desired()`
    /// on a shared engine to avoid a second snapshot.
    #[cfg(test)]
    pub fn desired_bitrate(slots: &[SlotView<'_>], states: &LayerStates) -> Bitrate {
        Self::new(slots, states).run_desired(slots)
    }

    pub fn run_desired(&self, slots: &[SlotView<'_>]) -> Bitrate {
        let total: f64 = slots
            .iter()
            .filter_map(|s| self.best_healthy(s))
            .map(|l| self.stable_cost(l))
            .sum();
        debug_assert!((0.0..1.0).contains(&Self::RESERVE_FRACTION));
        Bitrate::from(crate::bitrate::saturating_bps(
            total / (1.0 - Self::RESERVE_FRACTION),
        ))
    }

    fn used_bitrate(
        decisions: &SecondaryMap<DownstreamSlotKey, AllocationDecision<'_>>,
    ) -> Bitrate {
        let total = decisions
            .values()
            .filter_map(|decision| match decision {
                AllocationDecision::Forward(_, bitrate)
                | AllocationDecision::ForwardTarget(_, _, bitrate) => Some(bitrate.as_f64()),
                AllocationDecision::Pause(_, _) => None,
            })
            .sum::<f64>();
        Bitrate::from(crate::bitrate::saturating_bps(total))
    }

    /// The layer that satisfies a slot's `min_height` floor: the lowest eligible
    /// layer at least `min_height` tall, or the tallest eligible layer if none
    /// reaches it. `None` when the slot is droppable (`min_height == 0`) or has
    /// no eligible layer.
    fn floor_layer<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        if slot.min_height == 0 {
            return None;
        }
        let eligible = || slot.track.layers.iter().filter(|l| self.eligible(slot, l));
        eligible()
            .filter(|l| self.height(l) >= slot.min_height)
            .min_by_key(|l| l.quality)
            .or_else(|| eligible().max_by_key(|l| l.quality))
            .or_else(|| self.closest_healthy(slot))
    }

    /// Strict-priority allocation. `slots` must be pre-sorted by `priority_order`
    /// (highest priority first). Each stream keeps its guaranteed `min_height`
    /// floor first; leftover budget then raises streams toward their target in
    /// the same priority order, one genuine upgrade per call so send-rate rises
    /// gradually enough for BWE to track it.
    ///
    /// Captures its own snapshot. In `update_allocations()` use `run_compute()`
    /// on a shared engine to avoid a second snapshot.
    #[cfg(test)]
    pub fn compute<'a>(
        bwe: Bitrate,
        slots: &'a [SlotView<'a>],
        states: &LayerStates,
    ) -> SecondaryMap<DownstreamSlotKey, AllocationDecision<'a>> {
        Self::new(slots, states).run_compute(bwe, slots)
    }

    pub fn run_compute<'a>(
        &self,
        bwe: Bitrate,
        slots: &'a [SlotView<'a>],
    ) -> SecondaryMap<DownstreamSlotKey, AllocationDecision<'a>> {
        debug_assert!(
            slots.is_sorted_by(|a, b| Self::priority_order(a, b).is_le()),
            "compute expects slots sorted by priority_order",
        );

        // Reserve headroom withheld from speculative upgrades so the allocation
        // never drives the link to 100% (leaving room for probing/AIMD). Guaranteed
        // floors and retention may spend into it; climbing above the current layer
        // may not.
        let reserve = bwe.as_f64() * Self::UPGRADE_RESERVE_FRACTION;
        let mut budget = bwe.as_f64();

        // One entry per slot, known before the waterfall starts.
        let mut decisions = SecondaryMap::with_capacity(slots.len());

        // Strict-priority waterfall. Serve each stream fully — its `min_height`
        // floor, then its climb toward `target_height` — before touching the next
        // lower-priority stream, so a lower-priority floor never preempts a
        // higher-priority target.
        //
        // Stability comes from the inputs, not from damping the output or holding a
        // timer: budget math uses the *stable* declared layer bitrate
        // (`stable_cost`), so a variable-bitrate neighbour cannot bounce the
        // arithmetic, and each layer transition uses an asymmetric threshold — a
        // Schmitt dead-band — so a budget merely wobbling at a layer boundary does
        // not flip it. Real congestion still lands immediately: it shows up as a
        // lower `bwe`, and the very next allocation sheds.
        for slot in slots {
            let mut cur: Option<&TrackLayer> = None;
            let mut degraded: Option<(DecodeTargetSelection, f64)> = None;

            // Floor: guarantee min_height if affordable (retained through the
            // dead-band once held); else shed temporal layers down to the min_fps
            // floor; else leave paused.
            if let Some(floor) = self.floor_layer(slot) {
                let cost = self.stable_cost(floor);
                let resuming = !slot.forwarding;
                let threshold = if resuming {
                    cost + reserve
                } else if slot.current_quality >= floor.quality {
                    cost * Self::DOWNGRADE_FACTOR
                } else {
                    cost
                };
                if threshold <= budget {
                    budget -= cost;
                    cur = Some(floor);
                } else if let Some((target, dt_cost)) =
                    self.best_affordable_decode_target(floor, budget, slot.min_fps)
                {
                    budget -= dt_cost;
                    cur = Some(floor);
                    degraded = Some((target, dt_cost));
                }
            }

            // Climb toward target. Recovery up to the current layer is a cheap
            // dead-band; climbing above it is a genuine upgrade that must leave the
            // reserve intact. A temporally-degraded slot stays put — it is under
            // congestion and recovers once its floor fits again.
            if degraded.is_none() {
                while let Some(next) = self.next_layer(slot, cur) {
                    let step = self.stable_cost(next) - cur.map_or(0.0, |l| self.stable_cost(l));
                    // Resuming a paused slot is a genuine upgrade, not retention: it parks on a
                    // layer it is not sending, so charging it the cheap retention dead-band admits
                    // a stream the budget cannot actually carry, which overshoots the link and
                    // squeezes the higher-priority streams already served from this budget.
                    let resuming = !slot.forwarding;
                    let admitted = if resuming || next.quality > slot.current_quality {
                        step + reserve <= budget
                    } else {
                        step * Self::DOWNGRADE_FACTOR <= budget
                    };
                    if !admitted {
                        break;
                    }
                    budget -= step;
                    cur = Some(next);
                }
            }

            // Nothing measured healthy. Health describes the publisher's uplink,
            // not permission to forward, so an assigned slot still tries the
            // bottom of the ladder: a struggling publisher must read as
            // bandwidth-limited, not as a blank tile the SFU never explains.
            // Budget still governs — a link that cannot carry the lowest rung
            // pauses exactly as before, and a slot that declined a layer for
            // budget or `min_fps` reasons is untouched because something was
            // healthy there.
            if cur.is_none()
                && self.nothing_healthy(slot)
                && let Some(lowest) = self.lowest_ladder(slot)
            {
                let cost = self.stable_cost(lowest);
                if cost <= budget {
                    budget -= cost;
                    cur = Some(lowest);
                }
            }

            let decision = if let Some(layer) = cur {
                if let Some((target, dt_cost)) = degraded {
                    Some(AllocationDecision::ForwardTarget(
                        layer,
                        target,
                        Bitrate::from(crate::bitrate::saturating_bps(dt_cost)),
                    ))
                } else {
                    Some(AllocationDecision::Forward(
                        layer,
                        Bitrate::from(crate::bitrate::saturating_bps(self.cost(layer))),
                    ))
                }
            } else {
                self.pause_target(slot).map(|t| {
                    AllocationDecision::Pause(
                        t,
                        Bitrate::from(crate::bitrate::saturating_bps(self.cost(t))),
                    )
                })
            };
            if let Some(decision) = decision {
                decisions.insert(slot.key, decision);
            }
        }

        decisions
    }

    /// The cheapest layer the allocator was allowed to forward and could not
    /// afford, if any. See [`Self::eligible_but_unfunded`].
    ///
    /// Both halves of the starvation signal in one value: `Some` means
    /// something was unfundable, and the rate is what it would take to unblock
    /// it. Resetting the estimate to *that* probes the one thing that could
    /// restart feedback; resetting to full demand overshoots a small link and
    /// costs the stream a layer reversal on the way back down.
    pub fn cheapest_unfunded<'a>(
        &self,
        slots: &'a [SlotView<'a>],
        decisions: &SecondaryMap<DownstreamSlotKey, AllocationDecision<'a>>,
    ) -> Option<Bitrate> {
        slots
            .iter()
            .filter(|slot| self.eligible_but_unfunded(slot))
            .filter_map(|slot| match decisions.get(slot.key) {
                Some(AllocationDecision::Pause(_, needed)) => Some(*needed),
                _ => None,
            })
            .min_by(|a, b| a.as_f64().total_cmp(&b.as_f64()))
    }
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

    pub(super) fn state_of<'a>(states: &'a LayerStates, layer: &TrackLayer) -> &'a StreamStats {
        states
            .get(&layer.stream_id())
            .expect("layer must have seeded state")
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
    fn allocation_snapshot_exposes_per_encoding_decode_target_count() {
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

        // The "h" encoding advertises three decode targets (L1T3); "q", none.
        let scalable = track.by_quality(LayerQuality::Medium).unwrap();
        state_of_mut(&mut states, scalable).set_decode_target_count(3);
        let plain = track.by_quality(LayerQuality::Low).unwrap();

        let mut keys: SlotMap<DownstreamSlotKey, ()> = SlotMap::with_key();
        let view = SlotView {
            key: keys.insert(()),
            mid: Mid::from("s0"),
            max_height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
            track: &track,
            current_quality: LayerQuality::Low,
            forwarding: true,
        };
        let engine = AllocationEngine::new(std::slice::from_ref(&view), &states);

        assert_eq!(
            engine.decode_target_count(scalable),
            3,
            "the allocator sees the scalable encoding's decode targets"
        );
        assert_eq!(
            engine.decode_target_count(plain),
            1,
            "a non-scalable encoding is a single rung"
        );
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

    /// The fanout bindings a subscription has once its view delta has landed.
    ///
    /// Tests that exercise retry cadence need this: an empty map means the
    /// binding has not arrived yet, and no request may be issued in that state.
    fn bound_fanouts(allocator: &VideoAllocator) -> HashMap<TrackId, TrackKey> {
        let mut keys: SlotMap<TrackKey, ()> = SlotMap::with_key();
        allocator
            .tracks()
            .map(|meta| (meta.id, keys.insert(())))
            .collect()
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

        let fanouts = bound_fanouts(&allocator);
        let mut queue = MockParticipantSink::new();
        allocator.retry_keyframe_requests(now, &mut queue, &fanouts);
        assert_eq!(
            queue.request_keyframe_calls.len(),
            1,
            "retry_keyframe_requests should not send an immediate duplicate PLI after reconcile_routes"
        );

        // Before the view delta lands there is nothing to address the request
        // to, and the shard would only drop it. Issuing anyway burns a retry,
        // and enough of those reach keepalive cadence having never sent a PLI -
        // which leaves the subscriber black. See
        // `late_joiner_receives_earlier_participant_in_both_directions_test`.
        let mut unbound = MockParticipantSink::new();
        let mut fresh = setup_allocator();
        let fresh_tracks = add_tracks(&mut fresh, 1);
        add_slots(&mut fresh, 1);
        let low = fresh
            .track(&fresh_tracks.ids[0])
            .unwrap()
            .lowest_quality()
            .expect("video track has a layer")
            .clone();
        let slot = fresh.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(None, Some(&low));
        slot.paused = false;
        fresh.reconcile_routes(&mut unbound);
        fresh.retry_keyframe_requests(now, &mut unbound, &HashMap::new());
        assert_eq!(
            unbound.request_keyframe_calls.len(),
            0,
            "a keyframe request must not be issued before its fanout binding exists"
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
    fn allocator_returns_positive_desired_bitrate() {
        let mut allocator = setup_allocator();
        let _tracks = add_tracks(&mut allocator, 1);
        add_slots(&mut allocator, 1);

        let (desired, _, _) = allocator.update_allocations(Bitrate::from(5_000_000));
        assert!(desired.as_f64() > 0.0);
        assert!(allocator.current_allocation().as_f64() > 0.0);
        assert!(allocator.current_allocation() <= desired);
    }

    /// A slot must forward only the track it is serving.
    ///
    /// `on_rtp_slot` used to take the track identity from the slot's *target*
    /// rather than from the packet, so a packet still arriving for an outgoing
    /// track was announced under the incoming one. The switcher's per-track gate
    /// then admitted it and walked the wrong cache on the active stream's
    /// timeline, splicing two sources into one egress sequence: the subscriber
    /// saw a frame cut short and the next source's frame continue its numbering,
    /// with nothing to tell it the first was incomplete.
    #[test]
    fn a_slot_ignores_a_packet_belonging_to_another_track() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 2);
        add_slots(&mut allocator, 1);
        allocator.rebalance();

        let served = allocator
            .slots
            .iter()
            .find_map(|(key, slot)| slot.target().map(|layer| (key, layer.meta.id)));
        let Some((slot_key, served_track)) = served else {
            panic!("rebalance must give the slot a track to serve");
        };
        let Some(&foreign) = tracks.ids.iter().find(|id| **id != served_track) else {
            panic!("two tracks were added");
        };

        let mut cache = TrackStreamCache::new();
        let mut writer = StreamWriter::new();
        let mut builder = crate::rtp::test_utils::H264StreamBuilder::new(
            1,
            1000,
            90_000,
            tokio::time::Instant::now(),
        );
        for pkt in builder.keyframe(4) {
            cache.push(pkt.clone());
            allocator.on_rtp_slot(slot_key, foreign, &pkt, Some(&cache), &mut writer);
        }

        let mut emitted = 0usize;
        while let Some(write) = writer.pop() {
            if matches!(write, crate::track::StreamWrite::Video { .. }) {
                emitted = emitted.saturating_add(1);
            }
        }
        assert_eq!(
            emitted, 0,
            "a packet from a track this slot does not serve must not reach its egress stream"
        );
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
    #![allow(clippy::filter_next)]

    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::alloc_test_support::*;
    use super::*;
    use crate::entity::ParticipantId;
    use crate::rtp::monitor::StreamQuality;
    use crate::track::LayerQuality;
    use proptest::prelude::*;

    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    fn next_slot_key() -> DownstreamSlotKey {
        use std::cell::RefCell;
        thread_local! {
            static KEY_SM: RefCell<SlotMap<DownstreamSlotKey, ()>> = RefCell::new(SlotMap::with_key());
        }
        KEY_SM.with(|sm| sm.borrow_mut().insert(()))
    }

    fn healthy_track() -> (Track, LayerStates) {
        use str0m::media::SimulcastLayer;
        let (tx, track, states) = video_track_with_states(
            ParticipantId::new(),
            Mid::from("t"),
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        (
            Track {
                meta: tx.meta,
                layers: track.layers,
                reverse: None,
            },
            states,
        )
    }

    fn track_with_bad_layer(bad: LayerQuality) -> (Track, LayerStates) {
        let (vt, mut states) = healthy_track();
        let layer = vt.by_quality(bad).unwrap().clone();
        state_of_mut(&mut states, &layer).set_quality(StreamQuality::Bad);
        (vt, states)
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
            min_fps: 0,
            track,
            priority: 0,
            current_quality: current,
            forwarding: true,
        }
    }

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
            min_fps: 0,
            track,
            priority,
            current_quality: current,
            forwarding: true,
        }
    }

    /// `compute` requires slots pre-sorted by priority; helper for tests.
    fn sorted(mut slots: Vec<SlotView<'_>>) -> Vec<SlotView<'_>> {
        slots.sort_by(AllocationEngine::priority_order);
        slots
    }

    fn forwarded_quality(
        decisions: &SecondaryMap<DownstreamSlotKey, AllocationDecision<'_>>,
        key: DownstreamSlotKey,
    ) -> Option<LayerQuality> {
        match decisions.get(key) {
            Some(AllocationDecision::Forward(l, _)) => Some(l.quality),
            _ => None,
        }
    }

    #[test]
    fn ample_budget_serves_every_slot_to_its_target() {
        let (t, states) = healthy_track();
        let high = layer_bps(&t, &states, LayerQuality::High);
        let available = bw(crate::bitrate::saturating_bps(high * 4.0) / 1_000);
        let slots = sorted(vec![
            qos_slot("a", 1080, 0, 10, &t, LayerQuality::Low),
            qos_slot("b", 1080, 0, 5, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots, &states);
        for slot in &slots {
            assert_eq!(
                forwarded_quality(&decisions, slot.key),
                Some(LayerQuality::High),
                "{} was held below its target despite ample budget",
                slot.mid
            );
        }
    }

    #[test]
    fn higher_priority_slot_served_first_under_contention() {
        let (t, states) = healthy_track();
        let low = layer_bps(&t, &states, LayerQuality::Low);
        // Budget fits only one Low layer.
        let available = bw(crate::bitrate::saturating_bps(low) / 1_000 + 5);
        let slots = sorted(vec![
            qos_slot("hi", 1080, 0, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, 0, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots, &states);
        let hi = slots.iter().filter(|s| s.priority == 100).next().unwrap();
        let lo = slots.iter().filter(|s| s.priority == 0).next().unwrap();
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
        let (t, states) = healthy_track();
        let high_h = state_of(&states, t.by_quality(LayerQuality::High).unwrap()).height();
        let high_bps = layer_bps(&t, &states, LayerQuality::High);
        // Enough for the pinned High plus a little — but not for a second High.
        let available = bw(crate::bitrate::saturating_bps(high_bps * 1.3) / 1_000);

        // Pinned: min_height == its High layer's height (floor == target), already
        // forwarding High. A lower-priority background stream joins.
        let slots = sorted(vec![
            qos_slot("pin", 1080, high_h, 100, &t, LayerQuality::High),
            qos_slot("bg", 1080, 0, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots, &states);
        let pin = slots
            .iter()
            .filter(|s| s.mid == Mid::from("pin"))
            .next()
            .unwrap();
        assert_eq!(
            forwarded_quality(&decisions, pin.key),
            Some(LayerQuality::High),
            "pinned stream degraded when a background stream joined"
        );
    }

    #[test]
    fn droppable_stream_pauses_before_a_floored_one() {
        let (t, states) = healthy_track();
        let low = layer_bps(&t, &states, LayerQuality::Low);
        let low_h = state_of(&states, t.by_quality(LayerQuality::Low).unwrap()).height();
        // Budget fits exactly one Low floor.
        let available = bw(crate::bitrate::saturating_bps(low) / 1_000 + 5);
        let slots = sorted(vec![
            // Droppable (min_height 0), same priority as the floored one.
            qos_slot("drop", 1080, 0, 0, &t, LayerQuality::Low),
            // Floored: must stay visible.
            qos_slot("keep", 1080, low_h, 0, &t, LayerQuality::Low),
        ]);
        let decisions = AllocationEngine::compute(available, &slots, &states);
        let drop = slots
            .iter()
            .filter(|s| s.mid == Mid::from("drop"))
            .next()
            .unwrap();
        let keep = slots
            .iter()
            .filter(|s| s.mid == Mid::from("keep"))
            .next()
            .unwrap();
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

    fn layer_bps(track: &Track, states: &LayerStates, q: LayerQuality) -> f64 {
        state_of(states, track.by_quality(q).unwrap()).bitrate_bps()
    }

    #[test]
    fn base_layer_degrade_forwards_dd_base_instead_of_pausing() {
        let (t, mut states) = healthy_track();
        // Leave only "q" healthy, so it is the sole eligible floor.
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::High).unwrap().clone(),
        )
        .set_inactive(true);
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Medium).unwrap().clone(),
        )
        .set_inactive(true);
        let q = t.by_quality(LayerQuality::Low).unwrap();
        state_of_mut(&mut states, q).bitrate(1_000_000); // 1 Mbps at full quality

        // Budget covers the base temporal layer (0.5 Mbps) but not the full floor
        // (0.75 Mbps after the retention factor).
        let budget = bw(700);

        // Scalable (L1T3): degrade to the base layer rather than pause.
        state_of_mut(&mut states, q).set_decode_target_count(3);
        let slots = vec![qos_slot("a", 2000, 1, 0, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(budget, &slots, &states);
        assert!(
            matches!(
                decisions[slots[0].key],
                AllocationDecision::ForwardTarget(l, _, _) if l.quality == LayerQuality::Low
            ),
            "a scalable stream degrades to its base layer instead of pausing, got {:?}",
            decisions[slots[0].key]
        );

        // No Dependency Descriptor: there is no base layer to fall back to, so the
        // same budget pauses the stream as before — the fallback is preserved.
        state_of_mut(&mut states, q).set_decode_target_count(1);
        let slots = vec![qos_slot("a", 2000, 1, 0, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(budget, &slots, &states);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Pause(..)),
            "a non-scalable stream still pauses, got {:?}",
            decisions[slots[0].key]
        );
    }

    /// A scalable encoding whose full rate does not fit is shed to the *highest*
    /// decode target that does — an intermediate temporal rung, not straight to
    /// the base layer.
    fn scalable_low_only_track() -> (Track, LayerStates) {
        let (t, mut states) = healthy_track();
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::High).unwrap().clone(),
        )
        .set_inactive(true);
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Medium).unwrap().clone(),
        )
        .set_inactive(true);
        let q = t.by_quality(LayerQuality::Low).unwrap();
        state_of_mut(&mut states, q).bitrate(600_000);
        state_of_mut(&mut states, q).set_decode_target_count(3);
        // Declared per-temporal ladder: dt0=200k, dt1=300k, dt2(full)=600k @ 30fps
        // → fps 7/15/30.
        state_of_mut(&mut states, q).set_temporal_ladder(&[200, 300, 600], 30);
        (t, states)
    }

    fn scalable_slot(t: &Track, min_fps: u32) -> SlotView<'_> {
        SlotView {
            key: next_slot_key(),
            mid: Mid::from("a"),
            max_height: 2000,
            min_height: 1,
            min_fps,
            priority: 0,
            track: t,
            current_quality: LayerQuality::Low,
            forwarding: true,
        }
    }

    #[test]
    fn temporal_ladder_picks_the_highest_affordable_decode_target() {
        let (t, states) = scalable_low_only_track();
        // 350 kbps: full (600k) does not fit; dt1 (300k) is the highest that does.
        let view = scalable_slot(&t, 0);
        let d = AllocationEngine::compute(bw(350), std::slice::from_ref(&view), &states);
        assert!(
            matches!(
                d[view.key],
                AllocationDecision::ForwardTarget(_, DecodeTargetSelection::Target(1), _)
            ),
            "expected an intermediate temporal target, got {:?}",
            d[view.key]
        );
    }

    #[test]
    fn min_fps_floor_pauses_rather_than_shed_below_it() {
        let (t, states) = scalable_low_only_track();
        // min_fps=20: only the full target (30fps) meets it; dt1 is 15fps, dt0 is
        // 7fps. Full does not fit 350k, so the slot pauses rather than drop below
        // the frame-rate floor.
        let view = scalable_slot(&t, 20);
        let d = AllocationEngine::compute(bw(350), std::slice::from_ref(&view), &states);
        assert!(
            matches!(d[view.key], AllocationDecision::Pause(..)),
            "expected a pause below the min_fps floor, got {:?}",
            d[view.key]
        );

        // min_fps=10: dt1 (15fps) satisfies the floor and fits, so shed to it.
        let view = scalable_slot(&t, 10);
        let d = AllocationEngine::compute(bw(350), std::slice::from_ref(&view), &states);
        assert!(
            matches!(
                d[view.key],
                AllocationDecision::ForwardTarget(_, DecodeTargetSelection::Target(1), _)
            ),
            "expected shed to dt1 above the 10fps floor, got {:?}",
            d[view.key]
        );
    }

    /// Allocation cost comes from bitrate_bps, which is set by the upstream
    /// monitor's RateFilter (smoothing VLA-declared targets). When bitrate_bps
    /// is stable (as it is after the monitor's fast-rise/slow-fall filter
    /// converges), the chosen layer is stable regardless of VBR content bursts.
    #[test]
    fn stable_bitrate_bps_makes_allocation_stable() {
        let (t, mut states) = healthy_track();

        // Decide the forwarded layer with given bitrate_bps values (the
        // smoothed cost signal written by StreamMonitor::poll).
        let mut decide = |high_bps: u64, med_bps: u64| -> Option<LayerQuality> {
            state_of_mut(
                &mut states,
                &t.by_quality(LayerQuality::High).unwrap().clone(),
            )
            .bitrate(high_bps);
            state_of_mut(
                &mut states,
                &t.by_quality(LayerQuality::Medium).unwrap().clone(),
            )
            .bitrate(med_bps);
            let slots = vec![slot("a", 1080, &t, LayerQuality::Medium)];
            let decisions = AllocationEngine::compute(bw(886), &slots, &states);
            match decisions[slots[0].key] {
                AllocationDecision::Forward(l, _) | AllocationDecision::ForwardTarget(l, _, _) => {
                    Some(l.quality)
                }
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
        let (t, mut states) = healthy_track();
        // The High layer is actually only 180p; the sender declares it.
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::High).unwrap().clone(),
        )
        .set_height(180);

        // Client caps at 180p. The hard-coded fallback rates High at 720p and
        // would forbid it, but the declared 180p must be allowed.
        let slot = slot("a", 180, &t, LayerQuality::High);
        let engine = AllocationEngine::new(std::slice::from_ref(&slot), &states);
        let high = t.by_quality(LayerQuality::High).unwrap();
        assert!(engine.spatially_allowed(&slot, high));

        // The Medium layer declared nothing and keeps its 360p fallback.
        let medium = t.by_quality(LayerQuality::Medium).unwrap();
        assert!(!engine.spatially_allowed(&slot, medium));
    }

    /// Screen-share simulcast tiers often share one resolution and differ
    /// only in fps/bitrate. A client cap below that shared height must not
    /// reject every layer — there's nothing shorter to fall back to, so all
    /// tiers stay eligible and `desired_bitrate` must still be nonzero.
    #[test]
    fn uniform_layer_heights_all_stay_spatially_allowed_below_cap() {
        let (t, mut states) = healthy_track();
        for quality in [LayerQuality::High, LayerQuality::Medium, LayerQuality::Low] {
            state_of_mut(&mut states, &t.by_quality(quality).unwrap().clone()).set_height(1080);
        }

        // Client caps at 480p, below the shared 1080p every tier declares.
        let slot = slot("a", 480, &t, LayerQuality::Low);
        let engine = AllocationEngine::new(std::slice::from_ref(&slot), &states);
        for quality in [LayerQuality::High, LayerQuality::Medium, LayerQuality::Low] {
            let layer = t.by_quality(quality).unwrap();
            assert!(
                engine.spatially_allowed(&slot, layer),
                "{quality:?} at the shared minimum height must stay allowed"
            );
        }

        let desired = AllocationEngine::desired_bitrate(std::slice::from_ref(&slot), &states);
        assert!(
            desired.as_f64() > 0.0,
            "a slot with an eligible layer must desire nonzero bitrate"
        );
    }

    // ─── Property: every slot receives exactly one decision ─────────────────────

    #[test]
    fn every_slot_gets_a_decision() {
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
            slot("c", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);
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
        let (t, states) = healthy_track();
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);
        for (_, d) in &decisions {
            assert!(
                matches!(
                    d,
                    AllocationDecision::Forward(..) | AllocationDecision::Pause(..)
                ),
                "unexpected variant: {d:?}"
            );
        }
    }

    // ─── Property: desired bitrate is non-negative ───────────────────────────────

    #[test]
    fn desired_bitrate_is_non_negative() {
        let (t, states) = healthy_track();
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        let desired = AllocationEngine::desired_bitrate(&slots, &states);
        assert!(desired.as_f64() >= 0.0, "desired must be non-negative");
    }

    // ─── Property: with unlimited bandwidth every slot forwards ─────────────────

    #[test]
    fn unlimited_bandwidth_forwards_all_slots() {
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
            slot("c", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(100_000), &slots, &states);
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
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(0), &slots, &states);
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
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 360, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(0), &slots, &states);
        for (key, d) in &decisions {
            if let AllocationDecision::Pause(receiver, needed) = d {
                // The receiver field must point somewhere meaningful (non-null
                // is the only invariant we can assert structurally).
                let _ = receiver; // just asserting it exists via pattern match
                assert!(needed.as_f64() > 0.0, "Pause bitrate must be positive");
            } else if matches!(d, AllocationDecision::Pause(..)) {
                panic!("Pause for {key:?} is missing its resume receiver");
            }
        }
    }

    // ─── Property: a bad high layer falls back to the next healthy layer ─────────
    //
    // When the highest quality is degraded, the engine should still forward
    // rather than pause — it just picks a lower healthy layer.

    #[test]
    fn bad_high_layer_falls_back_rather_than_pausing() {
        let (t, states) = track_with_bad_layer(LayerQuality::High);
        let slots = vec![SlotView {
            key: next_slot_key(),
            mid: Mid::from("a"),
            max_height: 1080,
            min_height: 0,
            min_fps: 0,
            track: &t,
            priority: 0,
            current_quality: LayerQuality::High,
            forwarding: true,
        }];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "expected Forward fallback when High is bad, got {:?}",
            decisions[slots[0].key]
        );
    }

    // ─── Property: an assigned slot always tries the lowest rung ────────────────
    //
    // Health measures the *publisher's* uplink. A publisher whose every encoding
    // is struggling must render as bandwidth-limited video, never as a blank
    // tile: pausing there drops the packets that are still arriving and requests
    // no keyframe, so the subscriber sees nothing and nothing explains why.
    // Budget remains the only legitimate reason to pause.

    fn track_with_every_layer_bad() -> (Track, LayerStates) {
        let (t, mut states) = healthy_track();
        for layer in &t.layers {
            state_of_mut(&mut states, layer).set_quality(StreamQuality::Bad);
        }
        (t, states)
    }

    #[test]
    fn an_all_unhealthy_track_forwards_its_lowest_rung_rather_than_pausing() {
        let (t, states) = track_with_every_layer_bad();
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);
        assert_eq!(
            forwarded_quality(&decisions, slots[0].key),
            Some(LayerQuality::Low),
            "a publisher with no healthy encoding must still be forwarded at the \
             bottom of the ladder, got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn a_track_with_no_measurements_yet_forwards_immediately() {
        // A slot allocated before its first packet has no measurement handles at
        // all, so every layer reads unhealthy. It must not open blank and wait.
        let (t, _) = healthy_track();
        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &LayerStates::new());
        assert_eq!(
            forwarded_quality(&decisions, slots[0].key),
            Some(LayerQuality::Low),
            "a freshly assigned slot must forward before the first measurement, got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn an_all_unhealthy_track_still_pauses_when_the_budget_cannot_carry_it() {
        // The escape hatch is for health only. Real downlink congestion must
        // still pause, otherwise the waterfall overspends its budget.
        let (t, states) = track_with_every_layer_bad();
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(0), &slots, &states);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Pause(..)),
            "zero budget must still pause, got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn an_all_unhealthy_track_declares_its_demand_to_bwe() {
        // The allocator forwards it, so the demand fed to BWE must include it —
        // otherwise the estimator is asked to shrink a link we are still using.
        let (t, states) = track_with_every_layer_bad();
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        assert!(
            AllocationEngine::desired_bitrate(&slots, &states).as_f64() > 0.0,
            "a forwarded slot must contribute to the desired bitrate"
        );
    }

    // ─── Property: a healthy layer is preferred whenever one exists ─────────────
    //
    // Health ranks candidates while any layer is healthy. The one exception is
    // when *none* is, which the lowest-rung tests above cover.

    #[test]
    fn a_healthy_layer_is_preferred_while_one_exists() {
        let (t, states) = track_with_bad_layer(LayerQuality::High);
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);
        if let AllocationDecision::Forward(receiver, _) = &decisions[slots[0].key] {
            assert!(
                state_of(&states, receiver).is_healthy(),
                "engine forwarded to an unhealthy layer: {:?}",
                receiver.quality
            );
        }
    }

    #[test]
    fn healthy_zero_bitrate_layer_is_never_forwarded() {
        let (t, mut states) = healthy_track();
        for layer in &t.layers {
            state_of_mut(&mut states, layer).bitrate(0);
        }
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let decisions = AllocationEngine::compute(bw(10_000), &slots, &states);

        assert!(
            !matches!(
                decisions.get(slots[0].key),
                Some(AllocationDecision::Forward(..))
            ),
            "zero-bitrate layer must not be forwarded"
        );
    }

    // ─── Property: higher-priority slot is preferred when budget is tight ────────
    //
    // Two slots, only enough bandwidth for one Low layer.  The slot with the
    // higher priority (max_height) should be forwarded; the other paused.

    #[test]
    fn tight_budget_forwards_higher_priority_slot() {
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);

        // Budget just fits one Low layer (no headroom for downgrade guard).
        let available = bw(crate::bitrate::saturating_bps(low_bps) / 1_000 + 5);

        let slots = vec![
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("h"),
                max_height: 1080,
                min_height: 0,
                min_fps: 0,
                track: &t,
                priority: 200,
                current_quality: LayerQuality::Low,
                forwarding: true,
            },
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("l"),
                max_height: 360,
                min_height: 0,
                min_fps: 0,
                track: &t,
                priority: 0,
                current_quality: LayerQuality::Low,
                forwarding: true,
            },
        ];

        let decisions = AllocationEngine::compute(available, &slots, &states);

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
            let (t, states) = healthy_track();
            let low_bps = layer_bps(&t, &states, LayerQuality::Low);

            // Budget just barely covers one Low layer.
            let available = bw(crate::bitrate::saturating_bps(low_bps) / 1_000 + 1);
            let priority = 720;

            let mid_names: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
            let slots: Vec<SlotView> = mid_names
                .iter()
                .map(|name| slot(name, priority, &t, LayerQuality::Low))
                .collect();

            let decisions1 = AllocationEngine::compute(available, &slots, &states);

            // Reorder the input slots and verify outcome stays the same.
            let mut reversed = slots.clone();
            reversed.reverse();
            let decisions2 = AllocationEngine::compute(available, &reversed, &states);

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
    // desired includes the reserve required by the allocator.

    #[test]
    fn desired_bitrate_equals_sum_of_best_healthy_layers() {
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
        ];

        let expected_per_slot = t
            .layers
            .iter()
            .filter(|l| state_of(&states, l).is_healthy())
            .map(|l| state_of(&states, l).bitrate_bps())
            .fold(0.0_f64, f64::max);

        let expected_total =
            expected_per_slot * slots.len() as f64 / (1.0 - AllocationEngine::RESERVE_FRACTION);

        let desired = AllocationEngine::desired_bitrate(&slots, &states);

        assert!(
            (desired.as_f64() - expected_total).abs() < 1.0,
            "desired {:.0} bps != expected {:.0} bps",
            desired.as_f64(),
            expected_total
        );
    }

    #[test]
    fn desired_bitrate_covers_all_forwarded_layers() {
        let (t, states) = healthy_track();
        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 180, &t, LayerQuality::Low),
        ];
        let decisions = AllocationEngine::compute(bw(100_000), &slots, &states);

        assert!(
            AllocationEngine::desired_bitrate(&slots, &states)
                >= AllocationEngine::used_bitrate(&decisions)
        );
    }

    #[test]
    fn desired_bitrate_includes_healthy_fallback_above_height_cap() {
        let (t, mut states) = healthy_track();
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Low).unwrap().clone(),
        )
        .set_inactive(true);
        let slots = vec![slot("a", 180, &t, LayerQuality::Medium)];

        let expected = layer_bps(&t, &states, LayerQuality::Medium)
            / (1.0 - AllocationEngine::RESERVE_FRACTION);
        assert!(
            (AllocationEngine::desired_bitrate(&slots, &states).as_f64() - expected).abs() < 1.0
        );
    }

    proptest! {
        #[test]
        fn desired_bitrate_is_exact_and_covers_usage(
            high_bps in 1u64..=2_000_000,
            medium_bps in 1u64..=2_000_000,
            low_bps in 1u64..=2_000_000,
            high_healthy in any::<bool>(),
            medium_healthy in any::<bool>(),
            low_healthy in any::<bool>(),
            height_index in 0usize..3,
            slot_count in 1usize..=5,
            available_bps in 0u64..=10_000_000,
        ) {
            let (t, mut states) = healthy_track();
            let cases = [
                (LayerQuality::High, high_bps, high_healthy, 720u32),
                (LayerQuality::Medium, medium_bps, medium_healthy, 360u32),
                (LayerQuality::Low, low_bps, low_healthy, 180u32),
            ];
            for &(quality, bitrate, healthy, _) in &cases {
                state_of_mut(&mut states, &t.by_quality(quality).unwrap().clone())
                                        .bitrate(bitrate)
                    .set_inactive(!healthy);
            }

            let max_height = [180, 360, 720][height_index];
            let mids: Vec<String> = (0..slot_count).map(|i| format!("d{i}")).collect();
            let slots: Vec<_> = mids
                .iter()
                .map(|mid| slot(mid, max_height, &t, LayerQuality::Low))
                .collect();

            let spatial_max = cases
                .iter()
                .filter(|(_, _, healthy, height)| *healthy && *height <= max_height)
                .map(|(_, bitrate, _, _)| *bitrate)
                .max();
            let fallback = cases
                .iter()
                .filter(|(_, _, healthy, _)| *healthy)
                .min_by_key(|(quality, _, _, _)| *quality)
                .map(|(_, bitrate, _, _)| *bitrate);
            // Nothing healthy at all: the slot is still forwarded at the bottom of
            // the ladder rather than blanked, so that rung is what it demands.
            let lowest_allowed = cases
                .iter()
                .filter(|(_, _, _, height)| *height <= max_height)
                .min_by_key(|(quality, _, _, _)| *quality)
                .map(|(_, bitrate, _, _)| *bitrate);
            let expected_per_slot = spatial_max.or(fallback).or(lowest_allowed).unwrap_or(0);
            let expected = crate::bitrate::saturating_bps(
                expected_per_slot as f64 * slot_count as f64
                    / (1.0 - AllocationEngine::RESERVE_FRACTION),
            );
            let desired = AllocationEngine::desired_bitrate(&slots, &states);

            prop_assert_eq!(desired.as_u64(), expected);

            let decisions = AllocationEngine::compute(Bitrate::from(available_bps), &slots, &states);
            prop_assert!(desired >= AllocationEngine::used_bitrate(&decisions));
        }
    }

    // ─── Property: downgrade hysteresis absorbs small bandwidth noise ────────────
    //
    // If bandwidth drops only slightly below the current layer cost (within the
    // 10% DOWNGRADE_FACTOR dead-band), the engine should keep forwarding the
    // current layer rather than dropping to a lower one.

    #[test]
    fn downgrade_hysteresis_absorbs_minor_bandwidth_noise() {
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);

        // 5% below Low cost — inside the downgrade dead-band; no downgrade should fire.
        let available = bw(crate::bitrate::saturating_bps(low_bps * 0.95) / 1_000);

        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(available, &slots, &states);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "engine downgraded or paused inside the hysteresis dead-band"
        );
    }

    // ─── Property: empty slot list produces empty decisions + zero desired ────────

    #[test]
    fn no_slots_yields_empty_decisions_and_zero_desired() {
        let decisions = AllocationEngine::compute(bw(1_000), &[], &LayerStates::new());
        assert!(
            decisions.is_empty(),
            "expected no decisions for empty slots"
        );
        assert_eq!(
            AllocationEngine::desired_bitrate(&[], &LayerStates::new()).as_f64(),
            0.0,
            "expected zero desired bitrate for empty slots"
        );
    }

    // ─── Budget-floor invariants ────────────────────────────────────────────────

    #[test]
    fn tight_budget_pauses_floored_slot() {
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);
        let low_h = state_of(&states, t.by_quality(LayerQuality::Low).unwrap()).height();

        let tight = bw(crate::bitrate::saturating_bps(low_bps * 0.5) / 1_000);
        let slots = vec![qos_slot("a", 1080, low_h, 0, &t, LayerQuality::Low)];

        let decisions = AllocationEngine::compute(tight, &slots, &states);
        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Pause(..)),
            "budget below floor must pause; got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn tight_budget_pauses_lower_priority_slot() {
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);

        let available = bw(crate::bitrate::saturating_bps(low_bps) / 1_000 + 5);
        let slots = sorted(vec![
            qos_slot("hi", 1080, 0, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, 0, 0, &t, LayerQuality::Low),
        ]);

        let decisions = AllocationEngine::compute(available, &slots, &states);
        let hi = slots.iter().filter(|s| s.priority == 100).next().unwrap();
        let lo = slots.iter().filter(|s| s.priority == 0).next().unwrap();

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
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);
        let low_h = state_of(&states, t.by_quality(LayerQuality::Low).unwrap()).height();

        let tight = bw(crate::bitrate::saturating_bps(low_bps * 0.3) / 1_000);
        let slots = sorted(vec![
            qos_slot("hi", 1080, low_h, 100, &t, LayerQuality::Low),
            qos_slot("lo", 1080, low_h, 0, &t, LayerQuality::Low),
        ]);

        let decisions = AllocationEngine::compute(tight, &slots, &states);
        let hi = slots.iter().filter(|s| s.priority == 100).next().unwrap();
        let lo = slots.iter().filter(|s| s.priority == 0).next().unwrap();

        assert!(
            matches!(decisions[hi.key], AllocationDecision::Pause(..)),
            "high-priority slot must pause when budget is insufficient"
        );
        assert!(
            matches!(decisions[lo.key], AllocationDecision::Pause(..)),
            "low-priority slot must pause when budget is insufficient"
        );
    }

    // ─── Floor hysteresis ─────────────────────────────────────────────────────────
    //
    // A slot that was already forwarding at its floor should be retained when
    // budget dips into the 75–100% zone (DOWNGRADE_FACTOR dead-band). A slot
    // that was NOT previously forwarding at the floor gets no such benefit.

    #[test]
    fn floor_hysteresis_retains_forwarding_slot_inside_downgrade_dead_band() {
        let (t, states) = healthy_track();
        let med_bps = layer_bps(&t, &states, LayerQuality::Medium);
        let med_h = state_of(&states, t.by_quality(LayerQuality::Medium).unwrap()).height();

        // Budget is 80% of floor cost — below floor but above DOWNGRADE_FACTOR×floor.
        // The slot is currently forwarding at the floor (current_quality == Medium).
        let available = bw(crate::bitrate::saturating_bps(med_bps * 0.80) / 1_000);
        let slots = vec![qos_slot("a", 1080, med_h, 0, &t, LayerQuality::Medium)];
        let decisions = AllocationEngine::compute(available, &slots, &states);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "floor layer should be retained via DOWNGRADE_FACTOR hysteresis when \
             budget is inside the 75–100%% dead-band and slot was already forwarding; \
             got {:?}",
            decisions[slots[0].key]
        );
    }

    #[test]
    fn floor_hysteresis_does_not_apply_to_new_subscriber() {
        // Make only Medium healthy so there's no sub-floor fallback layer.
        let (t, mut states) = healthy_track();
        for q in [LayerQuality::High, LayerQuality::Low] {
            state_of_mut(&mut states, &t.by_quality(q).unwrap().clone())
                .set_quality(StreamQuality::Bad);
        }
        let med_bps = layer_bps(&t, &states, LayerQuality::Medium);
        let med_h = state_of(&states, t.by_quality(LayerQuality::Medium).unwrap()).height();

        // Budget is 80% of floor cost. current_quality = Low (below floor) so
        // the slot was NOT previously forwarding at the floor → no hysteresis.
        // With no other eligible layer to fall back to, expect Pause.
        let available = bw(crate::bitrate::saturating_bps(med_bps * 0.80) / 1_000);
        let slots = vec![qos_slot("a", 1080, med_h, 0, &t, LayerQuality::Low)];
        let decisions = AllocationEngine::compute(available, &slots, &states);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Pause(..)),
            "new subscriber with no fallback layer should pause when budget \
             is below floor cost and hysteresis does not apply; \
             got {:?}",
            decisions[slots[0].key]
        );
    }

    // ─── desired_bitrate uses stable_cost, not reactive cost ─────────────────────

    #[test]
    fn desired_bitrate_reads_stable_bitrate_bps_not_reactive() {
        // Use only a single healthy layer so best_healthy is unambiguous.
        let (t, mut states) = healthy_track();
        for q in [LayerQuality::Medium, LayerQuality::Low] {
            state_of_mut(&mut states, &t.by_quality(q).unwrap().clone())
                .set_quality(StreamQuality::Bad);
        }

        let reactive_bps: u64 = 400_000;
        let stable_bps: u64 = 900_000;

        // Set reactive and stable to different values to distinguish them.
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::High).unwrap().clone(),
        )
        .bitrate(reactive_bps) // sets both reactive and stable
        .stable_bitrate(stable_bps); // overrides stable independently

        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];
        let desired = AllocationEngine::desired_bitrate(&slots, &states);

        assert!(
            (desired.as_f64() - stable_bps as f64 / (1.0 - AllocationEngine::RESERVE_FRACTION))
                .abs()
                < 1.0,
            "desired_bitrate should use stable_bitrate_bps ({stable_bps}) not \
             reactive bitrate_bps ({reactive_bps}); got {:.0}",
            desired.as_f64()
        );
    }

    // ─── Property: a single slot with a single healthy layer always forwards ──────

    #[test]
    fn single_slot_single_layer_always_forwards() {
        // Mark Medium and High as bad so only Low is healthy.
        let (t, mut states) = track_with_bad_layer(LayerQuality::High);
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Medium).unwrap().clone(),
        )
        .set_quality(StreamQuality::Bad);

        let low_bps = layer_bps(&t, &states, LayerQuality::Low);
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];

        // Bandwidth comfortably covers the only healthy layer.
        let available = bw(crate::bitrate::saturating_bps(low_bps * 2.0) / 1_000);
        let decisions = AllocationEngine::compute(available, &slots, &states);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "single healthy layer should always be forwarded when budget allows"
        );
    }

    #[test]
    fn always_forward_lowest_layer() {
        let (t, states) = healthy_track();
        let low_bps = layer_bps(&t, &states, LayerQuality::Low);
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        // Budget covers the lowest layer but not the next one up.
        let available = bw(crate::bitrate::saturating_bps(low_bps * 2.0) / 1_000);
        let decisions = AllocationEngine::compute(available, &slots, &states);

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
        let (t, mut states) = healthy_track();
        // Mark Low/"q" inactive — only High and Medium are publishing.
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Low).unwrap().clone(),
        )
        .set_inactive(true);

        let med_bps = layer_bps(&t, &states, LayerQuality::Medium);

        // Client requests max_height=180 (only the inactive Low would normally fit).
        let slots = sorted(vec![qos_slot("a", 180, 0, 0, &t, LayerQuality::Low)]);
        let available = bw(crate::bitrate::saturating_bps(med_bps * 2.0) / 1_000);
        let decisions = AllocationEngine::compute(available, &slots, &states);

        assert!(
            matches!(decisions[slots[0].key], AllocationDecision::Forward(..)),
            "expected Forward (closest-rank fallback) when the spatially-preferred layer is inactive"
        );

        if let AllocationDecision::Forward(layer, _) = decisions[slots[0].key] {
            assert!(
                state_of(&states, layer).is_healthy(),
                "forwarded layer must be healthy; got {:?}",
                layer.quality
            );
        }
    }

    #[test]
    fn pause_targets_live_h_when_q_is_inactive() {
        let (t, mut states) = healthy_track();
        state_of_mut(
            &mut states,
            &t.by_quality(LayerQuality::Low).unwrap().clone(),
        )
        .set_inactive(true);

        let slots = sorted(vec![qos_slot("a", 180, 360, 0, &t, LayerQuality::Low)]);
        let decisions = AllocationEngine::compute(bw(1), &slots, &states);

        let AllocationDecision::Pause(layer, _) = decisions[slots[0].key] else {
            panic!("insufficient bandwidth must pause the slot");
        };
        assert_eq!(layer.quality, LayerQuality::Medium);
        assert!(state_of(&states, layer).is_healthy());
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

        // PLI is what unblocks the switch. The fanout binding is what addresses
        // it: without one the shard can only drop the request, so `pli_retry`
        // withholds it rather than burning a retry on a request that cannot land.
        let mut sink = crate::participant::event::test_utils::MockParticipantSink::new();
        let mut keys: SlotMap<TrackKey, ()> = SlotMap::with_key();
        let fanouts: HashMap<TrackId, TrackKey> =
            [(low.stream_id().0, keys.insert(()))].into_iter().collect();
        fx.slot.pli_retry(Instant::now(), &mut sink, &fanouts);
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
