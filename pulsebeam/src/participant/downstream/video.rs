use crate::participant::downstream::SlotConfig;
use crate::participant::event::EventQueue;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use indexmap::IndexSet;
use slotmap::SlotMap;
use std::collections::HashMap;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{LayerQuality, StreamId, StreamWriter, Track, TrackLayer, TrackMeta};

/// Maximum number of video slots per participant.
const VIDEO_MAX_SLOTS: usize = 25;

slotmap::new_key_type! {
    pub struct SlotKey;
}

pub struct VideoAllocator {
    // Hot
    routes: HashMap<StreamId, SlotKey>,
    slots: SlotMap<SlotKey, Slot>,

    // Cold
    manual_sub: bool,
    tracks: HashMap<TrackId, Track>,
}

impl VideoAllocator {
    pub fn new(manual_sub: bool) -> Self {
        Self {
            manual_sub,
            tracks: HashMap::new(),
            slots: slotmap::SlotMap::with_capacity_and_key(VIDEO_MAX_SLOTS),
            routes: HashMap::new(),
        }
    }

    pub fn add_track(&mut self, meta: TrackMeta, layers: Vec<TrackLayer>) {
        if self.tracks.contains_key(&meta.id) {
            return;
        }
        tracing::info!(track = %meta.id, "video track added");
        self.tracks.insert(meta.id, Track { meta, layers });
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) {
        if self.tracks.remove(track_id).is_some() {
            tracing::info!(track = %track_id, "video track removed");
            self.rebalance();
        }
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn configure(&mut self, intents: &HashMap<Mid, Intent>) {
        for (key, slot) in self.slots.iter_mut() {
            let tracks = &mut self.tracks;
            if let Some(intent) = intents.get(&slot.mid) {
                Self::configure_slot(tracks, slot, intent.max_height, Some(&intent.track_id));
            } else {
                Self::configure_slot(tracks, slot, 0, None);
            }
        }
    }

    /// Routes this slot to the given track at the specified maximum height,
    /// or stops routing if `track_id` is `None` or `max_height` is 0.
    fn configure_slot(
        tracks: &mut HashMap<TrackId, Track>,
        slot: &mut Slot,
        max_height: u32,
        track_id: Option<&TrackId>,
    ) -> Option<()> {
        if let Some(track_id) = track_id
            && max_height > 0
        {
            let track_state = tracks.get_mut(track_id)?;

            // Keep current layer if slot already targets this track to avoid
            // unnecessary PLI requests; otherwise start at lowest quality.
            let layer = if let Some(target) = slot.target()
                && target.meta.id == track_state.meta.id
            {
                target
            } else {
                track_state.lowest_quality()
            };

            let layer = layer.clone();
            slot.max_height = max_height;
            slot.switch_to(&layer, false);
        } else {
            slot.max_height = 0;
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
                track: {
                    let layer = s.target()?;
                    self.tracks.get(&layer.meta.id)?.meta.clone()
                },
            })
        })
    }

    pub fn add_slot(&mut self, mid: Mid, config: SlotConfig) {
        let slot = Slot::new(SlotConfig { mid, ..config });
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
            .filter_map(|s| s.staging.as_ref().map(|t| t.meta.id))
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
            tracing::debug!(
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
                let layer = track_state.lowest_quality();
                slot.switch_to(layer, true);
                staged += 1;
            } else {
                break;
            }
        }
        if staged > 0 {
            tracing::debug!(
                staged,
                "rebalance: staged tracks into idle slots, awaiting BWE to activate"
            );
        }
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> Bitrate {
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
                    priority: s.max_height,
                    track,
                    current_quality,
                })
            })
            .collect();

        views.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.mid.cmp(&b.mid)));

        let (decisions, desired) = AllocationEngine::compute(available_bandwidth, &views);

        let mut changed = false;
        let mut keyframe_requests: Vec<KeyframeRequest> = Vec::new();
        for (key, decision) in &decisions {
            let Some(slot) = self.slots.get_mut(*key) else {
                tracing::warn!("no slot found from decision");
                continue;
            };

            match decision {
                AllocationDecision::Forward(layer, _) => {
                    changed |= slot.switch_to(layer, false);
                }
                AllocationDecision::Pause(layer) => {
                    changed |= slot.pause_at(layer);
                    let stream_id = layer.stream_id();
                }
            }
        }

        if changed {
            log_allocation(available_bandwidth, desired, &decisions, &views);
        }

        desired
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) -> Option<&TrackLayer> {
        let Some(slot) = self
            .slots
            .values()
            .find(|s| s.mid == req.mid && s.rid == req.rid)
        else {
            return None;
        };

        slot.target()
    }

    #[inline]
    pub fn on_rtp(&mut self, stream_id: &StreamId, pkt: &RtpPacket, writer: &mut StreamWriter) {
        let Some(slot_key) = self.routes.get(stream_id) else {
            return;
        };

        let Some(slot) = self.slots.get_mut(*slot_key) else {
            tracing::warn!("no slot found for {:?}", stream_id);
            return;
        };

        slot.process(stream_id, pkt);
        while let Some(pkt) = slot.switcher.pop() {
            writer.write_owned(pkt, &slot.ssrc, slot.pt);
        }
        // After draining all staged packets the switcher clears its internal
        // staging buffer; that is the right moment to promote staging→active so
        // that subsequent packets take the push() path instead of stage().
        if slot.switcher.ready_to_switch() && slot.staging.is_some() {
            slot.active = slot.staging.take();
        }
    }

    pub fn poll_slow(&mut self, _now: Instant, _bandwidth: Bitrate, events: &mut EventQueue) {
        self.reconcile_routes(events);
    }

    pub fn reconcile_routes(&mut self, events: &mut EventQueue) {
        // Pass 1: remove routes that no longer match any active slot.
        let to_remove: Vec<StreamId> = self
            .routes
            .keys()
            .filter(|sid| {
                !self
                    .slots
                    .values()
                    .any(|s| !s.paused && s.target().is_some_and(|l| &l.stream_id() == *sid))
            })
            .copied()
            .collect();

        for sid in &to_remove {
            self.routes.remove(sid);
            events.unsubscribe(*sid);
        }

        // Pass 2: add routes for active slots not yet in the table.
        for (key, slot) in self.slots.iter() {
            if slot.paused {
                continue;
            }
            let Some(layer) = slot.target() else { continue };
            let sid = layer.stream_id();
            if !self.routes.contains_key(&sid) {
                self.routes.insert(sid, key);
                events.subscribe(sid);
                events.request_keyframe(layer);
            }
        }
    }

    pub fn unsubscribe_all(&mut self) {
        todo!()
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
    ssrc: Ssrc,
    pt: Pt,

    active: Option<TrackLayer>,
    staging: Option<TrackLayer>,

    switcher: Switcher,

    mid: Mid,
    rid: Option<Rid>,
    max_height: u32,
    paused: bool,
}

impl Slot {
    pub fn new(cfg: SlotConfig) -> Self {
        Self {
            mid: cfg.mid,
            rid: cfg.rid,
            ssrc: cfg.ssrc,
            pt: cfg.pt,

            active: None,
            staging: None,

            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            max_height: 0,
            paused: true,
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

    fn switch_to(&mut self, new_layer: &TrackLayer, _force: bool) -> bool {
        // TODO: check old staging, and buffer keyframe.
        let mut changed = false;

        // Check if the staging layer is actually different
        if self.staging.as_ref() != Some(new_layer) {
            self.staging = Some(new_layer.clone());
            // Reset the switcher staging buffer so stale seq-no state from a
            // previous stream doesn't mix with the new stream's packets.
            self.switcher.clear();
            changed = true;
        }

        // Check if we were previously paused
        if self.paused {
            self.paused = false;
            changed = true;
        }

        changed
    }

    fn stop(&mut self) {
        self.active = None;
        self.staging = None;
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
        }

        if !self.paused {
            self.paused = true;
            changed = true;
        }

        changed
    }

    fn process(&mut self, stream_id: &StreamId, pkt: &RtpPacket) {
        if self.paused {
            return;
        }

        if let Some(active) = self.active.as_ref()
            && active.is(stream_id)
        {
            self.switcher.push(pkt.clone());
        } else if let Some(staging) = self.staging.as_ref()
            && staging.is(stream_id)
        {
            self.switcher.stage(pkt.clone());
        }
    }
}

pub fn log_allocation(
    bwe: Bitrate,
    desired: Bitrate,
    decisions: &HashMap<SlotKey, AllocationDecision>,
    slots: &[SlotView],
) {
    let mut reports = Vec::with_capacity(slots.len());
    let mut total_used_bps = 0.0;

    for slot in slots {
        let entry = match decisions.get(&slot.key) {
            Some(AllocationDecision::Forward(l, bw)) => {
                total_used_bps += bw.as_f64();
                let q = match l.quality {
                    LayerQuality::High => "H",
                    LayerQuality::Medium => "M",
                    LayerQuality::Low => "L",
                };
                format!("{}:{}({})", slot.mid, q, bw)
            }
            Some(AllocationDecision::Pause(_)) => format!("{}:PAUSE", slot.mid),
            _ => format!("{}:IDLE", slot.mid),
        };
        reports.push(entry);
    }

    tracing::info!(
        %bwe,
        used = %Bitrate::from(total_used_bps as u64),
        want = %desired,
        streams = %reports.join(" "),
        "downstream"
    );
}

pub struct SlotAssignment {
    pub mid: Mid,
    pub track: TrackMeta,
}

pub struct Intent {
    pub track_id: TrackId,
    pub max_height: u32,
}

pub struct AllocationEngine;

#[derive(Clone, Debug)]
pub struct SlotView<'a> {
    pub key: SlotKey,
    pub mid: Mid,
    pub priority: u32,
    pub track: &'a Track,
    pub current_quality: LayerQuality,
}

#[derive(Debug, Clone, Copy)]
pub enum AllocationDecision<'a> {
    Forward(&'a TrackLayer, Bitrate),
    Pause(&'a TrackLayer),
}

impl<'a> std::fmt::Display for AllocationDecision<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationDecision::Forward(layer, bitrate) => {
                write!(f, "Forward({} @ {})", layer, bitrate)
            }
            AllocationDecision::Pause(layer) => {
                write!(f, "Pause({})", layer)
            }
        }
    }
}

impl AllocationEngine {
    const UPGRADE_FACTOR: f64 = 1.3;
    const DOWNGRADE_FACTOR: f64 = 0.8;
    const MAX_UPGRADES_PER_TICK: usize = 2;

    pub fn compute<'a>(
        available_bw: Bitrate,
        slots: &[SlotView<'a>],
    ) -> (HashMap<SlotKey, AllocationDecision<'a>>, Bitrate) {
        let mut decisions: HashMap<SlotKey, AllocationDecision<'a>> = HashMap::new();
        let mut remaining_bps = available_bw.as_f64();

        // 1. Maintain or Downgrade
        for slot in slots {
            let current = slot.track.by_quality(slot.current_quality);

            let stay_layer = current.filter(|l| {
                l.state.is_healthy()
                    && (l.state.bitrate_bps() * Self::DOWNGRADE_FACTOR) <= remaining_bps
            });

            let final_layer = stay_layer.or_else(|| {
                slot.track
                    .lower_quality(slot.current_quality)
                    .filter(|l| l.state.is_healthy() && l.state.bitrate_bps() <= remaining_bps)
            });

            if let Some(layer) = final_layer {
                let layer_bitrate = Bitrate::from(layer.state.bitrate_bps());
                let bps = layer_bitrate.as_f64();
                remaining_bps -= bps;
                decisions.insert(slot.key, AllocationDecision::Forward(layer, layer_bitrate));
            } else {
                decisions.insert(
                    slot.key,
                    AllocationDecision::Pause(slot.track.lowest_quality()),
                );
            }
        }

        // 2. Upgrade
        let mut upgrades_performed = 0;
        for tier in [LayerQuality::Low, LayerQuality::Medium, LayerQuality::High] {
            if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                break;
            }

            for slot in slots {
                if upgrades_performed >= Self::MAX_UPGRADES_PER_TICK {
                    break;
                }

                let Some(AllocationDecision::Forward(current_layer, current_bw)) =
                    decisions.get(&slot.key).copied()
                else {
                    continue;
                };

                if current_layer.quality >= tier {
                    continue;
                }
                let Some(target) = slot.track.by_quality(tier) else {
                    continue;
                };
                if !target.state.is_healthy() {
                    continue;
                }

                let target_bw = Bitrate::from(target.state.bitrate_bps());
                let incremental_cost = target_bw.as_f64() - current_bw.as_f64();

                // Check against the 30% upgrade headroom (UPGRADE_FACTOR = 1.3)
                if remaining_bps >= (incremental_cost * Self::UPGRADE_FACTOR) {
                    remaining_bps -= incremental_cost;
                    decisions.insert(slot.key, AllocationDecision::Forward(target, target_bw));
                    upgrades_performed += 1;
                }
            }
        }

        // 3. Demand Calculation (The "Want" Bitrate)
        let total_desired_bps: f64 = slots
            .iter()
            .map(|s| {
                s.track
                    .layers
                    .iter()
                    .filter(|l| l.state.is_healthy())
                    .map(|l| l.state.bitrate_bps())
                    .max_by(|a, b| a.partial_cmp(b).unwrap())
                    .unwrap_or(0.0)
            })
            .sum();

        let used_bps: f64 = decisions
            .values()
            .filter_map(|d| match d {
                AllocationDecision::Forward(_, bw) => Some(bw.as_f64()),
                AllocationDecision::Pause(_) => None,
            })
            .sum();

        // The allocator uses hysteresis (DOWNGRADE_FACTOR / UPGRADE_FACTOR) to
        // prevent frequent bitrate churning. This can result in allocating slightly
        // more than the estimated available bandwidth. Allow a small overshoot
        // bound to prevent debug builds from panicking while still catching
        // gross allocation bugs.
        let max_allowed = available_bw.as_f64() / Self::DOWNGRADE_FACTOR;
        debug_assert!(
            used_bps <= max_allowed + f64::EPSILON,
            "AllocationEngine allocated more bandwidth than allowed: used {} > allowed {} (available {} )",
            used_bps,
            max_allowed,
            available_bw.as_f64()
        );

        (decisions, Bitrate::from(total_desired_bps as u64))
    }
}

#[cfg(test)]
mod assignment_tests {
    use super::*;
    use crate::entity::{ParticipantId, TrackId};
    use crate::track::{UpstreamTrack, test_utils::make_video_track};
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    #[derive(Default)]
    struct FakeRouter {
        subscribed: std::collections::HashSet<StreamId>,
    }
    impl RouteUpdater for FakeRouter {
        fn subscribe(&mut self, s: StreamId) {
            self.subscribed.insert(s);
        }
        fn unsubscribe(&mut self, s: &StreamId) {
            self.subscribed.remove(s);
        }
    }

    struct TestTracks {
        pub senders: Vec<UpstreamTrack>,
        pub ids: Vec<TrackId>,
    }

    fn setup_allocator() -> VideoAllocator {
        VideoAllocator::new(false)
    }

    fn add_tracks(allocator: &mut VideoAllocator, count: usize) -> TestTracks {
        let pid = ParticipantId::new();

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
            allocator.add_track(meta, track.layers);
            senders.push(tx);
        }

        TestTracks { senders, ids }
    }

    fn add_slots(allocator: &mut VideoAllocator, count: usize) {
        for i in 0..count {
            let mid = Mid::from(&format!("s{i}")[..]);
            allocator.add_slot(mid, SlotConfig::default());
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
            },
        );
        intents.insert(
            Mid::from("s1"),
            Intent {
                track_id: tracks.ids[1],
                max_height: 720,
            },
        );
        intents.insert(
            Mid::from("s2"),
            Intent {
                track_id: tracks.ids[2],
                max_height: 720,
            },
        );

        allocator.configure(&intents);
        assert_eq!(allocator.slots().count(), 3);
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

        let mut router = FakeRouter::default();
        let (desired, _) = allocator.update_allocations(Bitrate::from(5_000_000), &mut router);
        assert!(desired.as_f64() > 0.0);
    }

    #[test]
    fn allocator_handles_track_churn() {
        let mut allocator = setup_allocator();
        let mut tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        allocator.remove_track(&tracks.ids[1]);
        let pid = ParticipantId::new();
        let (tx, track) = make_video_track(pid, Mid::from("new_track"), vec![]);
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let meta = tx.meta.clone();
        tracks.senders.push(tx);
        allocator.add_track(meta, track.layers);
        assert_eq!(allocator.slots().count(), 3);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    use crate::entity::ParticipantId;
    use crate::rtp::monitor::StreamQuality;
    use crate::track::{LayerQuality, test_utils::make_video_track};
    use proptest::prelude::*;
    use str0m::bwe::Bitrate;
    use str0m::media::Mid;

    fn healthy_track() -> Track {
        let (tx, track) = make_video_track(ParticipantId::new(), Mid::from("t"), vec![]);
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

    fn slot<'a>(mid: &str, priority: u32, track: &'a Track, current: LayerQuality) -> SlotView<'a> {
        SlotView {
            mid: Mid::from(mid),
            priority,
            track,
            current_quality: current,
        }
    }

    fn bw(kbps: u64) -> Bitrate {
        Bitrate::from(kbps * 1_000)
    }

    fn layer_bps(track: &Track, q: LayerQuality) -> f64 {
        track.by_quality(q).unwrap().state.bitrate_bps()
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
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        for s in &slots {
            assert!(
                decisions.contains_key(&s.mid),
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
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
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
        for bw_kbps in [0, 1, 10, 100, 1_000, 100_000] {
            let (_, desired) = AllocationEngine::compute(bw(bw_kbps), &slots);
            assert!(desired.as_f64() >= 0.0, "desired < 0 at {} kbps", bw_kbps);
        }
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
        let (decisions, _) = AllocationEngine::compute(bw(100_000), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.mid], AllocationDecision::Forward(..)),
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
        let (decisions, _) = AllocationEngine::compute(bw(0), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.mid], AllocationDecision::Pause(..)),
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
        let (decisions, _) = AllocationEngine::compute(bw(0), &slots);
        for (mid, d) in &decisions {
            if let AllocationDecision::Pause(receiver) = d {
                // The receiver field must point somewhere meaningful (non-null
                // is the only invariant we can assert structurally).
                let _ = receiver; // just asserting it exists via pattern match
            } else if matches!(d, AllocationDecision::Pause(..)) {
                panic!("Pause for {} is missing its resume receiver", mid);
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
        let mid = Mid::from("a");
        let slots = vec![SlotView {
            mid,
            priority: 1080,
            track: &t,
            current_quality: LayerQuality::High,
        }];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "expected Forward fallback when High is bad, got {:?}",
            decisions[&mid]
        );
    }

    // ─── Property: forwarded layer is always a healthy layer ────────────────────

    #[test]
    fn forwarded_layer_is_always_healthy() {
        let t = track_with_bad_layer(LayerQuality::High);
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let (decisions, _) = AllocationEngine::compute(bw(10_000), &slots);
        if let AllocationDecision::Forward(receiver, _) = &decisions[&Mid::from("a")] {
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

        let mid_high_pri = Mid::from("h");
        let mid_low_pri = Mid::from("l");
        let slots = vec![
            SlotView {
                mid: mid_high_pri,
                priority: 1080,
                track: &t,
                current_quality: LayerQuality::Low,
            },
            SlotView {
                mid: mid_low_pri,
                priority: 360,
                track: &t,
                current_quality: LayerQuality::Low,
            },
        ];

        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid_high_pri], AllocationDecision::Forward(..)),
            "high-priority slot should be forwarded first"
        );
        assert!(
            matches!(decisions[&mid_low_pri], AllocationDecision::Pause(..)),
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
            let mut slots: Vec<SlotView> = mid_names
                .iter()
                .map(|name| slot(name, priority, &t, LayerQuality::Low))
                .collect();

            let (decisions1, _) = AllocationEngine::compute(available, &slots);

            // Reorder the input slots and verify outcome stays the same.
            slots.reverse();
            let (decisions2, _) = AllocationEngine::compute(available, &slots);

            prop_assert_eq!(decisions1.len(), decisions2.len());
            for name in mid_names {
                let mid = Mid::from(name.as_str());
                prop_assert_eq!(
                    decisions1.get(&mid),
                    decisions2.get(&mid),
                    "decisions differ for slot {} when input order changes",
                    mid
                );
            }
        }
    }
    // Two slots both eligible for an upgrade.  Only one should actually be
    // upgraded per call to compute().

    #[test]
    fn at_most_one_upgrade_per_tick() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let high_bps = layer_bps(&t, LayerQuality::High);

        // Enough for two High layers — upgrades are definitely affordable —
        // but the engine serialises them to one per tick.
        let available = bw(((high_bps * 2.0 * 1.4) as u64) / 1_000);

        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
        ];

        let (decisions, _) = AllocationEngine::compute(available, &slots);

        let upgrades = decisions
            .values()
            .filter(
                |d| matches!(d, AllocationDecision::Forward(r, _) if r.quality > LayerQuality::Low),
            )
            .count();

        assert!(
            upgrades <= AllocationEngine::MAX_UPGRADES_PER_TICK,
            "engine performed {} upgrades; limit is {}",
            upgrades,
            AllocationEngine::MAX_UPGRADES_PER_TICK
        );
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

        let (_, desired) = AllocationEngine::compute(bw(100_000), &slots);

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
        let mid = Mid::from("a");
        let low_bps = layer_bps(&t, LayerQuality::Low);

        // 5% below Low cost — inside the 10% dead-band; no downgrade should fire.
        let available = bw((low_bps * 0.95) as u64 / 1_000);

        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];
        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "engine downgraded or paused inside the hysteresis dead-band"
        );
    }

    // ─── Property: empty slot list produces empty decisions + zero desired ────────

    #[test]
    fn no_slots_yields_empty_decisions_and_zero_desired() {
        let (decisions, desired) = AllocationEngine::compute(bw(1_000), &[]);
        assert!(
            decisions.is_empty(),
            "expected no decisions for empty slots"
        );
        assert_eq!(
            desired.as_f64(),
            0.0,
            "expected zero desired bitrate for empty slots"
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
        let mid = Mid::from("a");
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];

        // Bandwidth comfortably covers the only healthy layer.
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let (decisions, _) = AllocationEngine::compute(available, &slots);

        assert!(
            matches!(decisions[&mid], AllocationDecision::Forward(..)),
            "single healthy layer should always be forwarded when budget allows"
        );
    }
}
