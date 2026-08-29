use crate::bitrate::{BitrateController, BitrateControllerConfig};
use crate::participant::allocation::Bitrate;
use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::rtp;
#[cfg(test)]
use crate::rtp::PayloadType as Pt;
#[cfg(test)]
use crate::rtp::RtpPacket;
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::frame_selector::DecodeTargetSelection;
use crate::rtp::switcher::{LayerStates, Switcher};
use crate::rtp::{
    CodecPayloadTypes, EncodingId as Rid, KeyframeRequest, KeyframeRequestKind,
    MediaSectionId as Mid, Ssrc,
};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;
use slotmap::{SecondaryMap, SlotMap};
use std::cmp::Ordering;
use std::ops::{Deref, DerefMut};
use std::time::Duration;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::keys::{DownstreamSlotKey, TrackKey};
use crate::log::{LogCtx, plog_debug, plog_error, plog_info, plog_trace, plog_warn};
use crate::participant::intent::VideoIntent as Intent;
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

/// The starting estimate applied by the SFU allocator.
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
    routes: SecondaryMap<TrackKey, DownstreamSlotKey>,
    slots: SlotMap<DownstreamSlotKey, Slot>,

    // Cold
    ctx: LogCtx,
    manual_sub: bool,
    tracks: SecondaryMap<TrackKey, Track>,
    track_keys: HashMap<TrackId, TrackKey>,
    active_track_keys: HashMap<TrackId, TrackKey>,
    last_reconciled: HashSet<(TrackKey, DownstreamSlotKey)>,
    desired_ctrl: BitrateController,
    current_allocation: Bitrate,
    #[cfg(test)]
    test_keys: SlotMap<TrackKey, ()>,
    #[cfg(test)]
    test_layer_states: LayerStates,
}

pub struct DownstreamVideo {
    allocator: VideoAllocator,
}

impl DownstreamVideo {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool) -> Self {
        Self {
            allocator: VideoAllocator::new(ctx, manual_sub),
        }
    }
}

impl Deref for DownstreamVideo {
    type Target = VideoAllocator;

    fn deref(&self) -> &Self::Target {
        &self.allocator
    }
}

impl DerefMut for DownstreamVideo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.allocator
    }
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
            ctx,
            manual_sub,
            tracks: SecondaryMap::new(),
            track_keys: HashMap::new(),
            active_track_keys: HashMap::new(),
            slots: slotmap::SlotMap::with_capacity_and_key(VIDEO_MAX_SLOTS),
            routes: SecondaryMap::new(),
            last_reconciled: HashSet::new(),
            desired_ctrl,
            current_allocation: Bitrate::ZERO,
            #[cfg(test)]
            test_keys: SlotMap::with_key(),
            #[cfg(test)]
            test_layer_states: LayerStates::new(),
        }
    }

    pub(crate) fn install_track(&mut self, key: TrackKey, track: Track) {
        if self.track_keys.insert(track.id(), key).is_some() {
            debug_assert!(false, "a TrackId must have one installed TrackKey");
            return;
        }
        plog_info!(self.ctx, track = %track.meta().id, "video track added");
        let previous = self.tracks.insert(key, track);
        debug_assert!(previous.is_none(), "a TrackKey must be installed once");
        self.rebalance();
    }

    pub(crate) fn activate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        debug_assert_eq!(self.track_keys.get(&track_id), Some(&key));
        let previous = self.active_track_keys.insert(track_id, key);
        debug_assert!(previous.is_none() || previous == Some(key));
    }

    pub(crate) fn deactivate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        debug_assert_eq!(self.active_track_keys.get(&track_id), Some(&key));
        if self.active_track_keys.get(&track_id) == Some(&key) {
            self.active_track_keys.remove(&track_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn add_track(&mut self, track: Track) {
        let key = self.test_keys.insert(());
        let track_id = track.id();
        self.install_track(key, track);
        self.activate_track_binding(key, track_id);
    }

    pub fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let Some(key) = self.track_keys.remove(track_id) else {
            return false;
        };
        debug_assert!(!self.active_track_keys.contains_key(track_id));
        self.active_track_keys.remove(track_id);
        let removed = self.tracks.remove(key);
        debug_assert!(removed.is_some(), "track index must resolve to catalog");
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

    /// Seed measurements directly, standing in for media already flowing.
    #[cfg(test)]
    pub(crate) fn seed_layer_states(&mut self, states: &LayerStates) {
        self.test_layer_states = states.clone();
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        let slots = &mut self.slots;
        let tracks = &self.tracks;
        let track_keys = &self.track_keys;
        for (_key, slot) in slots {
            if let Some(intent) = intents.get(&slot.mid) {
                Self::configure_slot(tracks, track_keys, slot, Some(intent));
            } else {
                Self::configure_slot(tracks, track_keys, slot, None);
            }
        }
    }

    /// Routes this slot to the given track at the specified QoS, or stops
    /// routing if `track_id` is `None` or `intent.max_height` is 0.
    fn configure_slot(
        tracks: &SecondaryMap<TrackKey, Track>,
        track_keys: &HashMap<TrackId, TrackKey>,
        slot: &mut Slot,
        intent: Option<&Intent>,
    ) -> Option<()> {
        if let Some(intent) = intent
            && intent.target_height > 0
        {
            let track_id = &intent.track_id;
            let Some(&track_key) = track_keys.get(track_id) else {
                plog_warn!(slot.ctx, track_id=%track_id, mid=%slot.mid, "configure_slot: requested track missing");
                slot.max_height = 0;
                slot.stop();
                return None;
            };
            let Some(track_state) = tracks.get(track_key) else {
                debug_assert!(false, "track index must resolve to catalog");
                slot.stop();
                return None;
            };

            // Keep current layer if slot already targets this track to avoid
            // unnecessary PLI requests; otherwise start at lowest quality.
            let layer = if let Some(target) = slot.target()
                && target.meta.id == track_state.id()
            {
                target
            } else {
                let states = track_states(track_state);
                let ceiling = track_state
                    .layers()
                    .iter()
                    .map(|layer| layer.quality.fallback_height())
                    .filter(|&height| height >= intent.target_height)
                    .min()
                    .unwrap_or(intent.target_height);
                let layer = track_state
                    .layers()
                    .iter()
                    .filter(|layer| layer.quality.fallback_height() <= ceiling)
                    .find(|layer| {
                        states
                            .get(&layer.stream_id())
                            .is_some_and(crate::rtp::monitor::StreamStats::is_healthy)
                    })
                    .or_else(|| {
                        track_state
                            .layers()
                            .iter()
                            .filter(|layer| layer.quality.fallback_height() <= ceiling)
                            .min_by_key(|layer| layer.quality)
                    });
                let Some(layer) = layer else {
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
        self.tracks.values().map(Track::meta)
    }

    pub fn slots(&self) -> impl Iterator<Item = SlotAssignment> + '_ {
        self.slots.values().filter_map(|s| {
            Some(SlotAssignment {
                mid: s.mid,
                paused: s.paused || matches!(s.state(), SlotState::Idle | SlotState::Starting),
                track: {
                    let layer = s.target()?;
                    self.track(&layer.meta.id)?.meta().clone()
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
            .values()
            .filter(|track| !already_assigned.contains(&track.meta().id));

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
                let states = track_states(track_state);
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
                let track_key = *self.track_keys.get(&current.meta.id)?;
                let track = self.tracks.get(track_key)?;
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
        let states = self.snapshot_states();
        let engine = AllocationEngine::new(&views, &states);
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
                crate::sim_metrics::record_forwarded_quality_for(view.track.meta().origin, quality);
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
        track_key: TrackKey,
        arrival_ts: Instant,
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
        let Some(&slot_key) = self.routes.get(track_key) else {
            return false;
        };
        let Some(track) = self.tracks.get(track_key) else {
            debug_assert!(false, "route key must resolve to an installed track");
            return false;
        };
        let track_id = track.id();
        let Some(slot) = self.slots.get_mut(slot_key) else {
            plog_warn!(self.ctx, "no slot found for track {:?}", track_key);
            return false;
        };
        slot.on_rtp(track_id, arrival_ts, cache, writer)
    }

    pub(crate) fn poll_slow(
        &mut self,
        now: Instant,
        _bandwidth: Bitrate,
        events: &mut impl ParticipantSink,
    ) {
        self.reconcile_routes(events);
        self.retry_keyframe_requests(now, events);
    }

    fn retry_keyframe_requests(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        let track_keys = &self.active_track_keys;
        for (_, slot) in &mut self.slots {
            slot.pli_retry(now, events, |track_id| track_keys.get(&track_id).copied());
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
                let Some(&track_key) = self.track_keys.get(&stream.0) else {
                    continue;
                };
                current.insert((track_key, slot_key));
            }
            if let Some(desired) = slot.desired.as_ref()
                && let Some(&track_key) = self.track_keys.get(&desired.meta.id)
            {
                current.insert((track_key, slot_key));
            }
        }

        let previous_tracks: HashSet<_> = self
            .last_reconciled
            .iter()
            .map(|(track_key, _)| *track_key)
            .collect();
        let current_tracks: HashSet<_> = current.iter().map(|(track_key, _)| *track_key).collect();
        let old_routes = std::mem::take(&mut self.routes);
        for (track_id, slot_key) in old_routes {
            if current.contains(&(track_id, slot_key)) {
                self.routes.insert(track_id, slot_key);
            }
        }

        for (track_key, slot_key) in &current {
            if self.routes.get(*track_key) != Some(slot_key) {
                self.routes.insert(*track_key, *slot_key);
            }
        }

        for track_key in previous_tracks.difference(&current_tracks) {
            if let Some(track) = self.tracks.get(*track_key) {
                events.deactivate_track(track.meta().clone());
            }
        }
        for track_key in current_tracks.difference(&previous_tracks) {
            if let Some(track) = self.tracks.get(*track_key) {
                events.activate_track(track.meta().clone());
            }
        }

        self.last_reconciled = current;

        debug_assert!(
            self.routes_consistent(),
            "route table inconsistent after reconcile_routes"
        );
    }

    fn routes_consistent(&self) -> bool {
        self.routes.iter().all(|(track_key, slot_key)| {
            self.slots.get(*slot_key).is_some_and(|slot| {
                self.tracks
                    .get(track_key)
                    .is_some_and(|track| slot.matches_track_id(&track.id()))
            })
        })
    }

    #[cfg(test)]
    fn has_route(&self, track_id: &TrackId) -> bool {
        self.track_keys
            .get(track_id)
            .is_some_and(|key| self.routes.contains_key(*key))
    }

    #[cfg(test)]
    fn set_route(&mut self, track_id: TrackId, slot_key: DownstreamSlotKey) {
        let key = *self.track_keys.get(&track_id).unwrap();
        self.routes.insert(key, slot_key);
    }

    #[cfg(test)]
    fn route_slot(&self, track_id: &TrackId) -> Option<DownstreamSlotKey> {
        self.track_keys
            .get(track_id)
            .and_then(|key| self.routes.get(*key).copied())
    }

    #[cfg(test)]
    fn on_rtp_slot(
        &mut self,
        _slot: DownstreamSlotKey,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        let Some(&key) = self.track_keys.get(&track_id) else {
            return false;
        };
        self.on_rtp(key, pkt.arrival_ts, cache, writer)
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
        self.track_keys
            .get(track_id)
            .and_then(|key| self.tracks.get(*key))
    }

    fn snapshot_states(&self) -> LayerStates {
        let mut states = LayerStates::new();
        for track in self.tracks.values() {
            states.extend(track_states(track));
        }
        #[cfg(test)]
        states.extend(
            self.test_layer_states
                .iter()
                .map(|(key, value)| (*key, *value)),
        );
        states
    }
}

fn track_states(track: &Track) -> LayerStates {
    let Some(stats) = track.stats() else {
        return LayerStates::new();
    };
    let states = stats.layer_states();
    debug_assert_eq!(
        states.len(),
        track.layers().len(),
        "a video track snapshot must cover every layer"
    );
    states
        .iter()
        .zip(track.layers())
        .map(|((rid, stats), layer)| {
            debug_assert_eq!(
                *rid, layer.rid,
                "a video layer snapshot must retain its RID"
            );
            (layer.stream_id(), *stats)
        })
        .collect()
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
    payload_types: CodecPayloadTypes,

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
            payload_types: cfg.payload_types,

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
        fanout_for: impl Fn(TrackId) -> Option<TrackKey>,
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
        let Some(fanout) = fanout_for(staging.stream_id().0) else {
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

        events.request_reverse(
            fanout,
            crate::participant::reverse::ReversePacket::keyframe(
                staging.stream_id().1,
                KeyframeRequestKind::Pli,
            ),
        );
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
        arrival_ts: Instant,
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
        let (mid, rid, ssrc, payload_types) = (self.mid, self.rid, self.ssrc, self.payload_types);
        let before = self.switcher.active_stream();
        self.switcher.feed(track_id, cache, arrival_ts, &mut |out| {
            let Some(pt) = payload_types.get(out.codec) else {
                debug_assert!(
                    false,
                    "forwarded video codec must be negotiated by the egress slot"
                );
                return;
            };
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

    fn test_payload_type(&self, codec: crate::rtp::Codec) -> Option<Pt> {
        self.payload_types.get(codec)
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
        let layer_count = slots.iter().map(|s| s.track.layers().len()).sum();
        let mut snaps = HashMap::with_capacity_and_hasher(layer_count, Default::default());
        snaps.extend(slots.iter().flat_map(|s| s.track.layers().iter()).map(|l| {
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
    // A slot keeps its layer until its cost exceeds ~1.54x the budget (1/0.65),
    // giving recoveries a hysteresis dead-band against churn.
    const DOWNGRADE_FACTOR: f64 = 0.65;

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
            .layers()
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
            .layers()
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

    fn closest_healthy<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers()
            .iter()
            .filter(|layer| {
                self.spatially_allowed(slot, layer)
                    && self.snap(layer).healthy
                    && self.cost(layer) > 0.0
            })
            .min_by_key(|l| l.quality)
    }

    fn nothing_spatially_healthy(&self, slot: &SlotView<'_>) -> bool {
        !slot
            .track
            .layers()
            .iter()
            .any(|layer| self.spatially_allowed(slot, layer) && self.snap(layer).healthy)
    }

    /// The bottom rung of the ladder the client's spatial request allows,
    /// ignoring health and bitrate. Health measures the *publisher's* uplink, so
    /// it is never a reason to refuse a slot every candidate — it only ranks
    /// them.
    fn lowest_ladder<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        slot.track
            .layers()
            .iter()
            .filter(|layer| self.spatially_allowed(slot, layer))
            .min_by_key(|layer| layer.quality)
            .or_else(|| slot.track.layers().iter().min_by_key(|layer| layer.quality))
    }

    /// A legal layer to retain as the pause target even when no layer is
    /// currently healthy enough to forward. Falls back to the lowest healthy
    /// layer (closest rank) when no spatially-allowed layer exists.
    fn pause_target<'a>(&self, slot: &'a SlotView<'a>) -> Option<&'a TrackLayer> {
        let target = slot
            .track
            .layers()
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
            .layers()
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
            .layers()
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
            .layers()
            .iter()
            .filter(|layer| self.eligible(slot, layer))
            .max_by(|a, b| self.cost(a).total_cmp(&self.cost(b)))
            .or_else(|| self.closest_healthy(slot))
            .or_else(|| {
                self.nothing_spatially_healthy(slot)
                    .then(|| self.lowest_ladder(slot))?
            })
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
        let eligible = || {
            slot.track
                .layers()
                .iter()
                .filter(|l| self.eligible(slot, l))
        };
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

            if cur.is_none()
                && self.nothing_spatially_healthy(slot)
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
    use crate::rtp::SimulcastEncoding as SimulcastLayer;
    use crate::rtp::monitor::StreamStats;
    use crate::track::UpstreamTrack;
    use crate::track::test_utils::make_video_track;

    /// Measurement handles standing in for what the publisher's shard would
    /// have supplied. Seeded active, which is what the old `inactive(false)`
    /// loops did.
    pub(super) fn states_for(track: &Track) -> LayerStates {
        track
            .layers()
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

    use crate::participant::allocation::Bitrate;
    use crate::rtp::{MediaSectionId as Mid, SimulcastEncoding as SimulcastLayer};

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
            allocator.add_track(Track::video(meta, track.layers().to_vec(), None));
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
        let track = Track::video(tx.meta, built.layers().to_vec(), None);

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
    fn retained_layer_uses_the_wider_downgrade_dead_band() {
        let pid = ParticipantId::new();
        let (tx, built, mut states) = video_track_with_states(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let track = Track::video(tx.meta, built.layers().to_vec(), None);
        let high = track.by_quality(LayerQuality::High).unwrap();
        state_of_mut(&mut states, high)
            .update_for_test()
            .bitrate(2_000_000);

        let mut keys: SlotMap<DownstreamSlotKey, ()> = SlotMap::with_key();
        let key = keys.insert(());
        let view = SlotView {
            key,
            mid: Mid::from("s0"),
            max_height: 720,
            min_height: 720,
            min_fps: 0,
            priority: 0,
            track: &track,
            current_quality: LayerQuality::High,
            forwarding: true,
        };
        let engine = AllocationEngine::new(std::slice::from_ref(&view), &states);

        let retained = engine.run_compute(Bitrate::from(1_310_000), std::slice::from_ref(&view));
        assert!(matches!(
            retained.get(key),
            Some(AllocationDecision::Forward(layer, _)) if layer.quality == LayerQuality::High
        ));

        let downgraded = engine.run_compute(Bitrate::from(1_290_000), std::slice::from_ref(&view));
        assert!(!matches!(
            downgraded.get(key),
            Some(AllocationDecision::Forward(layer, _)) if layer.quality == LayerQuality::High
        ));
    }

    #[test]
    fn unmeasured_requested_layer_starts_forwarding_for_keyframe_recovery() {
        let pid = ParticipantId::new();
        let (tx, built, mut states) = video_track_with_states(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        let track = Track::video(tx.meta, built.layers().to_vec(), None);
        let low = track.by_quality(LayerQuality::Low).unwrap();
        state_of_mut(&mut states, low).set_inactive(true);

        let mut keys: SlotMap<DownstreamSlotKey, ()> = SlotMap::with_key();
        let key = keys.insert(());
        let view = SlotView {
            key,
            mid: Mid::from("s0"),
            max_height: low.quality.fallback_height(),
            min_height: 0,
            min_fps: 0,
            priority: 0,
            track: &track,
            current_quality: LayerQuality::Low,
            forwarding: false,
        };
        let engine = AllocationEngine::new(std::slice::from_ref(&view), &states);
        let decisions = engine.run_compute(Bitrate::mbps(2), std::slice::from_ref(&view));

        assert!(matches!(
            decisions.get(key),
            Some(AllocationDecision::Forward(layer, _)) if layer.quality == LayerQuality::Low
        ));
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
            queue.reverse_requests.len(),
            0,
            "reconcile_routes no longer emits an immediate keyframe request"
        );

        let mut queue = MockParticipantSink::new();
        allocator.retry_keyframe_requests(now, &mut queue);
        assert_eq!(
            queue.reverse_requests.len(),
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
        fresh.active_track_keys.clear();
        fresh.retry_keyframe_requests(now, &mut unbound);
        assert_eq!(
            unbound.reverse_requests.len(),
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
        allocator.add_track(Track::video(tx.meta, track.layers().to_vec(), None));
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
            queue.activate_track_calls.len(),
            1,
            "routes are tracked per track, so staging and active layers share one subscription"
        );
        assert_eq!(
            queue.deactivate_track_calls.len(),
            0,
            "routes are tracked per track, so staging and active layers share one subscription"
        );
        assert_eq!(
            queue.reverse_requests.len(),
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
        let old_key = *allocator.track_keys.get(&old_stream_id.0).unwrap();
        allocator.last_reconciled.insert((old_key, slot_key));

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.set_roles_for_test(None, None);
        slot.paused = false;

        let mut queue = MockParticipantSink::new();
        allocator.reconcile_routes(&mut queue);

        assert!(allocator.routes.is_empty());
        assert_eq!(queue.deactivate_track_calls.len(), 1);
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
        assert_eq!(queue.activate_track_calls.len(), 1);
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
        allocator.add_track(Track::video(tx.meta.clone(), track.layers().to_vec(), None));
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
        slot.on_rtp(high.meta.id, pkt.arrival_ts, None, &mut writer);

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
        let key = allocator.track_keys[&tracks.ids[0]];
        allocator.deactivate_track_binding(key, tracks.ids[0]);
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
        for layer in track.layers() {
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
        for layer in track.layers() {
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
        for layer in track.layers() {
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
        allocator.add_track(Track::video(tx.meta, track.layers().to_vec(), None));

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
        let key = allocator.track_keys[&tracks.ids[1]];
        allocator.deactivate_track_binding(key, tracks.ids[1]);
        allocator.remove_track(&tracks.ids[1]);
        let pid = ParticipantId::new();
        let (tx, track, states) = video_track_with_states(pid, Mid::from("new_track"), vec![]);
        let meta = tx.meta.clone();
        tracks.senders.push(tx);
        allocator.seed_layer_states(&states);
        allocator.add_track(Track::video(meta, track.layers().to_vec(), None));
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

        allocator.add_track(Track::video(tx.meta, track.layers().to_vec(), None));
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

    use crate::rtp::SimulcastEncoding as SimulcastLayer;

    fn test_ctx() -> LogCtx {
        use crate::entity::{ExternalRoomId, RoomId};
        LogCtx {
            room_id: RoomId::from_external(&ExternalRoomId::new("test").unwrap()),
            participant_id: ParticipantId::new(),
        }
    }

    #[test]
    fn egress_slot_uses_the_payload_type_for_the_forwarded_codec() {
        let mut payload_types = CodecPayloadTypes::default();
        payload_types.insert(
            crate::rtp::Codec::H264,
            Pt::new(102).expect("test payload type is valid"),
        );
        let slot = Slot::new(
            test_ctx(),
            SlotConfig {
                payload_types,
                ..SlotConfig::default()
            },
        );

        assert_eq!(
            slot.test_payload_type(crate::rtp::Codec::H264),
            Pt::new(102)
        );
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
            pkt.extensions.rid = layer.rid.map(|rid| crate::rtp::EncodingId::from(&*rid));
            self.cache.push(pkt.clone());
            let promoted = self.slot.on_rtp(
                track_id,
                pkt.arrival_ts,
                Some(&self.cache),
                &mut self.writer,
            );
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
        fx.slot.pli_retry(Instant::now(), &mut sink, |track_id| {
            fanouts.get(&track_id).copied()
        });
        assert_eq!(
            sink.reverse_requests.first().copied(),
            fanouts.get(&low.stream_id().0).copied(),
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
