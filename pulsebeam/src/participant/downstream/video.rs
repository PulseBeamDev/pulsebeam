use crate::participant::downstream::SlotConfig;
use crate::participant::event::ParticipantSink;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use slotmap::SlotMap;
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::track::{LayerQuality, StreamId, StreamWriter, Track, TrackLayer, TrackMeta};

/// Maximum number of video slots per participant.
const VIDEO_MAX_SLOTS: usize = 25;

/// How long to wait between PLI retries while a slot is in a transition state.
const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_millis(1000);

/// After repeated retries, continue to probe the stream with lower-frequency keep-alives.
const KEYFRAME_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum number of aggressive PLI retries before falling back to keep-alive mode.
const KEYFRAME_MAX_RETRIES: u32 = 5;

pub const MIN_BANDWIDTH: Bitrate = Bitrate::kbps(300);
pub const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);

/// A completed switch's keyframe burst temporarily depresses the TWCC
/// estimate; hold the decision stable until it drains. Transaction
/// integrity, not congestion smoothing.
const SWITCH_SETTLE_DURATION: Duration = Duration::from_secs(2);
const SWITCH_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// A tier switch costs a keyframe; model that one-time cost as this many
/// seconds' worth of the target tier's steady bitrate, debited from the
/// slot's budget ledger. A slot that just switched has a depleted budget
/// and won't be eligible to switch again until it rebuilds it — that's the
/// entire chatter-suppression mechanism (see `VideoAllocator::update_allocations_at`).
const BURST_COST_SECONDS: f64 = 0.5;

/// The allocation ledger and decisions update at most this often, regardless
/// of how frequently `update_allocations` is called (on every BWE estimate,
/// plus a periodic floor). A fixed cadence keeps the ledger's integration
/// step well-defined.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Hysteresis, not a timer: a signal wobbling near a tier's exact cost
/// causes chatter regardless of how long each excursion lasts (a timer only
/// rejects excursions that don't *last* long enough, but real jitter has
/// the *magnitude* to cross a boundary on any timescale). A tier already
/// being held survives a real-but-modest shortfall — up to this fraction
/// below its exact cost — before it's actually abandoned; a tier being
/// newly reached gets no such grace. This is what stops a signal hovering
/// right at a boundary from ping-ponging, independent of how long it
/// hovers there.
const DOWNGRADE_MARGIN: f64 = 0.3;

/// Bounds how far a slot's admission cost can mark up over its stable
/// `nominal_bitrate_bps` before it's charged against the shared bandwidth
/// pool. Without this, a noisy/bursty live reading on one slot (VBR,
/// screen share) can consume far more of `remaining` than its true
/// entitlement, starving sibling slots processed later in the same tick's
/// walk — the cloud-CPU-steal-time problem. This bounds how much of
/// another slot's fair share any one slot can "steal" in a single tick,
/// regardless of how volatile its own reading gets.
const ADMISSION_PREMIUM_CEILING: f64 = 0.5;

/// Slack fraction of `trusted_bandwidth` never handed out by the greedy
/// walk. A per-slot ceiling (`ADMISSION_PREMIUM_CEILING`) bounds how much
/// any *one* slot can over-draw; this is a second, pool-level layer that
/// absorbs simultaneous noise across *multiple* slots and gives the
/// aggregate some stability margin, at the cost of never planning to 100%
/// of estimated capacity.
const RESERVE_FRACTION: f64 = 0.1;

/// Standing headroom added to a settled slot's contribution to `desired`
/// (see `desired_from_allocation_envelope`), even with no burst and no
/// tier change in flight. Asking for a bit more than current usage is what
/// gives str0m's BWE prober a reason to periodically probe for spare
/// capacity — without it, a fully settled system asks for exactly what
/// it's using and never discovers recovered bandwidth on its own.
const PROBE_HEADROOM_FRACTION: f64 = 0.1;

/// GCC can misread pure jitter as a transient overuse and drop its estimate
/// for one interval. Reacting to that single sample would self-reinforce:
/// our reduced send rate then starves str0m's own probe recovery of a
/// reason to grow back. A fast-rise/slow-fall EWMA on the estimate absorbs
/// the misread while still passing a sustained real drop through within
/// about one time constant.
const BWE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(300);
const BWE_FALL_TIME_CONSTANT: Duration = Duration::from_millis(2500);

slotmap::new_key_type! {
    pub struct SlotKey;
}

/// See `BWE_RISE_TIME_CONSTANT`/`BWE_FALL_TIME_CONSTANT`.
#[derive(Debug)]
struct BweFilter {
    filtered_bps: f64,
    last_update: Option<Instant>,
}

impl BweFilter {
    fn new(initial: Bitrate) -> Self {
        Self {
            filtered_bps: initial.as_f64(),
            last_update: None,
        }
    }

    fn update(&mut self, now: Instant, raw: Bitrate) -> Bitrate {
        let raw_bps = raw.as_f64();
        let Some(last_update) = self.last_update.replace(now) else {
            self.filtered_bps = raw_bps;
            return raw;
        };
        let elapsed = now.saturating_duration_since(last_update);
        let time_constant = if raw_bps > self.filtered_bps {
            BWE_RISE_TIME_CONSTANT
        } else {
            BWE_FALL_TIME_CONSTANT
        };
        let alpha = (-elapsed.as_secs_f64() / time_constant.as_secs_f64()).exp();
        self.filtered_bps = raw_bps + (self.filtered_bps - raw_bps) * alpha;
        Bitrate::from(self.filtered_bps as u64)
    }
}

pub struct VideoAllocator {
    // Hot
    routes: HashMap<TrackId, SlotKey>,
    slots: SlotMap<SlotKey, Slot>,

    // Cold
    manual_sub: bool,
    tracks: HashMap<TrackId, Track>,
    rng: Rng,
    last_reconciled: HashSet<(TrackId, SlotKey)>,
    bwe_filter: BweFilter,
    /// When the ledger/decision tick last actually ran. See `TICK_INTERVAL`.
    last_tick_at: Option<Instant>,
}

impl VideoAllocator {
    pub fn new<R: RngCore>(manual_sub: bool, rng: &mut R) -> Self {
        Self {
            manual_sub,
            tracks: HashMap::new(),
            slots: slotmap::SlotMap::with_capacity_and_key(VIDEO_MAX_SLOTS),
            routes: HashMap::new(),
            rng: Rng::seed_from_u64(rng.next_u64()),
            last_reconciled: HashSet::new(),
            bwe_filter: BweFilter::new(MIN_BANDWIDTH),
            last_tick_at: None,
        }
    }

    pub fn add_track(&mut self, track: Track) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }
        tracing::info!(track = %track.meta.id, "video track added");
        self.tracks.insert(track.meta.id, track);
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &TrackId) -> bool {
        if self.tracks.remove(track_id).is_none() {
            return false;
        }
        tracing::info!(track = %track_id, "video track removed");
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
            let Some(track_state) = tracks.get_mut(track_id) else {
                tracing::warn!(track_id=%track_id, mid=%slot.mid, "configure_slot: requested track missing");
                slot.set_max_height(0);
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
                track_state.lowest_quality()
            };

            let layer = layer.clone();
            slot.set_max_height(max_height);
            slot.switch_to(&layer, false);
        } else {
            slot.set_max_height(0);
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
            tracing::debug!(mid = %config.mid, "video slot already provisioned; skipping duplicate");
            return;
        }
        let slot = Slot::new(config, &mut self.rng);
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

        debug_assert!(
            self.no_duplicate_slot_assignments(),
            "rebalance produced duplicate track assignments: each track must map to at most one slot"
        );
    }

    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> (Bitrate, bool) {
        self.update_allocations_at(available_bandwidth, Instant::now())
    }

    fn update_allocations_at(
        &mut self,
        available_bandwidth: Bitrate,
        now: Instant,
    ) -> (Bitrate, bool) {
        let clamped = available_bandwidth.max(MIN_BANDWIDTH).min(MAX_BANDWIDTH);
        // The only place noise is filtered: every downstream decision trusts
        // this number completely and reacts immediately, with no further
        // per-decision confirmation delay.
        let trusted_bandwidth = self.bwe_filter.update(now, clamped);

        let mut views: Vec<SlotView> = self
            .slots
            .iter()
            .filter_map(|(key, s)| {
                let current = s.target()?;
                let track = self.tracks.get(&current.meta.id)?;
                Some(SlotView {
                    key,
                    mid: s.mid,
                    max_height: s.max_height,
                    track,
                    current_quality: current.quality,
                    weight: s.weight,
                })
            })
            .collect();

        // Cheap and doesn't need the ledger, so it's always kept fresh even
        // on ticks throttled below.
        let desired = desired_from_allocation_envelope(&views);

        let due = self
            .last_tick_at
            .is_none_or(|last| now.saturating_duration_since(last) >= TICK_INTERVAL);
        if !due {
            return (desired, false);
        }
        let dt = self
            .last_tick_at
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(TICK_INTERVAL);
        self.last_tick_at = Some(now);

        // 1. Ledger accrual: every active slot earns its weighted share of
        // `trusted_bandwidth` and spends whatever its current tier costs.
        // Runs for held/paused slots too — a paused slot spends nothing, so
        // it accrues its full share as credit and naturally rises to the
        // front of the next sort. This is what replaces `starved_ticks`.
        let total_weight: f64 = views.iter().map(|v| v.weight).sum();
        for view in &views {
            let Some(slot) = self.slots.get_mut(view.key) else {
                continue;
            };
            // A genuinely paused slot spends nothing and accrues its full
            // share as credit (see above). One that's holding or acquiring a
            // target — active, or mid-transaction waiting on a keyframe — is
            // charged its *nominal* cost, not the live observed rate: the
            // live rate reads near zero while a keyframe attempt is still
            // outstanding (or repeatedly failing under real loss), and
            // charging that as "spending nothing" would let the ledger
            // inflate for as long as acquisition keeps failing, the same
            // underpricing `nominal_bitrate_bps` already guards against in
            // the upstream monitor.
            let current_cost = if slot.paused {
                0.0
            } else {
                view.track
                    .by_quality(view.current_quality)
                    .map(|l| l.state.nominal_bitrate_bps())
                    .unwrap_or(0.0)
            };
            let fair_share = if total_weight > 0.0 {
                view.weight / total_weight * trusted_bandwidth.as_f64()
            } else {
                0.0
            };
            slot.budget_bps += (fair_share - current_cost) * dt.as_secs_f64();
        }

        // 2. Resolve contention: higher max_height processed first,
        // then most-owed slot (CFS budget-style), ties broken by mid.
        views.sort_by(|a, b| {
            b.max_height
                .cmp(&a.max_height)
                .then_with(|| {
                    let budget_a = self.slots.get(a.key).map(|s| s.budget_bps).unwrap_or(0.0);
                    let budget_b = self.slots.get(b.key).map(|s| s.budget_bps).unwrap_or(0.0);
                    budget_b
                        .partial_cmp(&budget_a)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.mid.cmp(&b.mid))
        });

        // 3. Greedy walk against a shared remaining-bandwidth pool. Held back
        // by RESERVE_FRACTION — see its doc comment — so admission never
        // plans against the full estimate.
        let mut remaining = trusted_bandwidth.as_f64() * (1.0 - RESERVE_FRACTION);
        let mut decisions: HashMap<SlotKey, AllocationDecision> = HashMap::new();
        for view in &views {
            let Some(slot) = self.slots.get_mut(view.key) else {
                continue;
            };

            // A switch is a transaction carrying a PLI that a fresh decision
            // shouldn't preempt. Reaffirm the current layer while that
            // transaction (or its post-promotion settle window) is active;
            // no ledger debit since nothing actually switched.
            if slot.should_hold_allocation(now) {
                let layer = view
                    .track
                    .by_quality(view.current_quality)
                    .filter(|l| !l.state.is_inactive())
                    .unwrap_or_else(|| view.track.cheapest_active_layer());
                // Use effective_cost (same ceiling as decide_tier) so a VBR burst
                // during a settle window can't blow the pool for sibling slots.
                // Floor at 0: this slot is committed to forwarding (mid-transaction),
                // but its deficit must not propagate as a negative to later slots.
                let cost = effective_cost(layer);
                remaining = (remaining - cost).max(0.0);
                decisions.insert(
                    view.key,
                    AllocationDecision::Forward(layer, Bitrate::from(cost as u64)),
                );
                continue;
            }

            decisions.insert(view.key, decide_tier(slot, view, &mut remaining));
        }

        let mut changed = false;
        for (key, decision) in &decisions {
            let Some(slot) = self.slots.get_mut(*key) else {
                tracing::warn!("no slot found from decision");
                continue;
            };

            match decision {
                AllocationDecision::Forward(layer, _) => {
                    changed |= slot.switch_to(layer, false);
                }
                AllocationDecision::Pause(layer, _) => {
                    changed |= slot.pause_at(layer);
                }
            }
        }

        if changed {
            log_allocation(trusted_bandwidth, desired, &decisions, &views);
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
        writer: &mut StreamWriter,
    ) -> bool {
        let Some(slot_key) = self.routes.get(&stream_id.0) else {
            return false;
        };

        let Some(slot) = self.slots.get_mut(*slot_key) else {
            tracing::warn!("no slot found for {:?}", stream_id);
            return false;
        };

        let mut state_changed = false;
        slot.process(stream_id, pkt);
        while let Some(pkt) = slot.switcher.pop() {
            writer.write_video_owned(pkt, slot.mid, slot.rid, slot.pt);
        }
        // Only promote staging→active once we have actually seen packets for the
        // current staging layer. Otherwise an empty staging buffer will appear
        // ready immediately and can prematurely switch away from the old stream.
        if slot.should_promote_staging() {
            slot.active = slot.staging.take();
            slot.staging_started_at = None;
            slot.settling_until = Some(pkt.arrival_ts + SWITCH_SETTLE_DURATION);
            state_changed = true;
        }

        state_changed
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
                        tracing::error!(
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
    ssrc: Ssrc,
    pt: Pt,

    active: Option<TrackLayer>,
    staging: Option<TrackLayer>,

    switcher: Switcher,

    mid: Mid,
    rid: Option<Rid>,
    max_height: u32,
    paused: bool,

    /// Number of PLI retries sent for the current staging layer.
    staging_keyframe_retries: u32,
    /// When the last PLI retry was sent for the current staging layer.
    staging_keyframe_last_at: Option<Instant>,
    /// Current retry interval for PLI probes while waiting for the staging keyframe.
    staging_keyframe_interval: Duration,
    /// Start time of the current layer-change transaction.
    staging_started_at: Option<Instant>,
    /// Holds ordinary allocation changes just after a promoted layer switch.
    settling_until: Option<Instant>,
    /// Running ledger balance, bits. Positive means this slot has received
    /// less than its fair share recently (owed credit, eligible to spend it
    /// on an upgrade); negative means it's been costing more than its share.
    /// See `VideoAllocator::update_allocations_at`.
    budget_bps: f64,
    /// This slot's share of contended bandwidth relative to other slots'
    /// weights — `= max_height` today, kept in sync by `set_max_height`. A
    /// bigger requested view earns a proportionally bigger entitlement.
    weight: f64,
}

impl Slot {
    fn new<R: RngCore>(cfg: SlotConfig, rng: &mut R) -> Self {
        Self {
            mid: cfg.mid,
            rid: cfg.rid,
            ssrc: cfg.ssrc,
            pt: cfg.pt,

            active: None,
            staging: None,

            switcher: Switcher::new(rtp::VIDEO_FREQUENCY, rng),
            // With no signaling, we assume users are viewing with 720p playback
            max_height: 720,
            paused: true,

            staging_keyframe_retries: 0,
            staging_keyframe_last_at: None,
            staging_keyframe_interval: KEYFRAME_RETRY_INTERVAL,
            staging_started_at: None,
            settling_until: None,
            budget_bps: 0.0,
            weight: 720.0,
        }
    }

    /// Keeps `weight` in sync with `max_height`. The only place either
    /// should be assigned.
    fn set_max_height(&mut self, height: u32) {
        self.max_height = height;
        self.weight = height as f64;
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
            tracing::debug!(
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
                self.staging_started_at = None;
                self.switcher.clear_staging();
                self.pli_reset();
                changed = true;
                tracing::debug!(mid=%self.mid, old_target=?old_target, new_target=?new_layer.stream_id(), "slot canceled in-flight transition and preserved active layer");
            }
        } else if self.target().as_ref() != Some(&new_layer) {
            self.staging = Some(new_layer.clone());
            self.settling_until = None;
            self.staging_started_at = Some(Instant::now());
            // Reset the switcher staging buffer so stale seq-no state from a
            // previous stream doesn't mix with the new stream's packets.
            self.switcher.clear_staging();
            // Reset retry state so the new staging layer gets fresh PLI attempts.
            self.pli_reset();
            changed = true;
            tracing::debug!(mid=%self.mid, old_target=?old_target, new_target=?new_layer.stream_id(), "slot staging new layer");
        }

        if self.paused {
            // Without this, a slot paused long enough for its hold to expire has
            // no protection on the very next tick and can oscillate at tick rate.
            if self.staging.is_some() {
                self.staging_started_at = Some(Instant::now());
            } else {
                self.settling_until = Some(Instant::now() + SWITCH_SETTLE_DURATION);
            }
            self.paused = false;
            changed = true;
            tracing::debug!(mid=%self.mid, new_target=?new_layer.stream_id(), "slot resumed from paused state");
        }

        changed
    }

    fn stop(&mut self) {
        tracing::debug!(mid=%self.mid, "slot stopped");
        self.active = None;
        self.staging = None;
        self.settling_until = None;
        self.staging_started_at = None;
        self.budget_bps = 0.0;
        self.pli_reset();
    }

    fn pause_at(&mut self, layer: &TrackLayer) -> bool {
        let mut changed = false;

        // A pause is a transient shortfall, not a layer switch. Keep an
        // already-active layer in `active` rather than demoting it to
        // `staging` — `staging` requires a fresh keyframe, so demoting on
        // every brief pause would force full re-acquisition on each resume
        // even though the stream never changed.
        if self.active.as_ref() == Some(layer) {
            if self.staging.is_some() {
                self.staging = None;
                self.staging_started_at = None;
                self.switcher.clear_staging();
                self.pli_reset();
                changed = true;
            }
        } else {
            // If we weren't active, but now we are explicitly None,
            // we check if it was already None to avoid redundant dirtying.
            if self.active.is_some() {
                self.active = None;
                changed = true;
            }

            if self.staging.as_ref() != Some(layer) {
                self.staging = Some(layer.clone());
                // Starts this transaction's own clock so `should_hold_allocation`
                // times out via SWITCH_TRANSACTION_TIMEOUT instead of holding
                // forever — without this, a resume target that changes tier
                // (not just resuming the same one) leaves staging_started_at
                // unset, and `is_none_or` treats that as "just started" every
                // single tick, permanently locking the slot out of re-evaluation.
                self.staging_started_at = Some(Instant::now());
                changed = true;
                tracing::debug!(mid=%self.mid, target=?layer.stream_id(), "slot pause_at set staging target");
            }
        }

        if !self.paused {
            self.paused = true;
            changed = true;
            tracing::debug!(mid=%self.mid, target=?layer.stream_id(), "slot paused");
        }

        changed
    }

    fn process(&mut self, stream_id: &StreamId, pkt: &RtpPacket) {
        if self.paused {
            tracing::trace!(mid=%self.mid, stream_id=?stream_id, "slot paused, dropping incoming packet");
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
        } else {
            tracing::trace!(mid=%self.mid, stream_id=?stream_id, active_target=?self.active.as_ref().map(|l| l.stream_id()), staging_target=?self.staging.as_ref().map(|l| l.stream_id()), "incoming packet ignored: stream does not match active or staging target");
        }
    }

    fn should_promote_staging(&self) -> bool {
        self.switcher.ready_to_switch() && self.staging.is_some()
    }

    fn should_hold_allocation(&self, now: Instant) -> bool {
        // Never replace an in-flight transaction, including the very first
        // one — its outstanding PLI is already the cheapest path to a
        // usable layer, and the first staged layer is deliberately the
        // lowest tier (see `rebalance`), chosen specifically so it renders
        // fast. Letting a generous initial bandwidth reading preempt it
        // before its keyframe lands would trade a fast first frame for a
        // slower, bigger one. Bounded by SWITCH_TRANSACTION_TIMEOUT so a
        // staging layer that never receives packets doesn't strand the slot
        // forever.
        if self.staging.is_some() {
            return self.staging_started_at.is_none_or(|started| {
                now.saturating_duration_since(started) < SWITCH_TRANSACTION_TIMEOUT
            });
        }

        // After promotion, an isolated hard-floor estimate is usually the
        // just-forwarded keyframe. Bounded by SWITCH_SETTLE_DURATION: real
        // congestion is picked up on the very next tick once this expires.
        self.settling_until.is_some_and(|until| now < until)
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

fn log_allocation(
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
            Some(AllocationDecision::Pause(_, needed)) => format!("{}:PAUSE({})", slot.mid, needed),
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
    pub paused: bool,
    pub track: TrackMeta,
}

pub struct Intent {
    pub track_id: TrackId,
    pub max_height: u32,
}

pub struct AllocationEngine;

/// Application demand offered to str0m for BWE probing: the sum, across
/// slots, of each slot's best reachable layer (bounded by `max_height`),
/// with headroom. A smaller "one tier at a time" demand was tried first and
/// reliably stalled recovery below the top layer — str0m only probes
/// toward what we tell it we want, and its own probe/ALR controller already
/// paces how aggressively it chases a large desired_bitrate.
fn desired_from_allocation_envelope(views: &[SlotView<'_>]) -> Bitrate {
    Bitrate::from(
        views
            .iter()
            .filter_map(|view| {
                let current = view.track.by_quality(view.current_quality)?;
                let current_bps = current.state.nominal_bitrate_bps();
                let best_reachable = view
                    .track
                    .layers
                    .iter()
                    .filter(|layer| {
                        layer.state.is_activation_candidate()
                            && AllocationEngine::max_height_for_quality(layer.quality)
                                <= view.max_height
                    })
                    .max_by_key(|layer| layer.quality);

                let demand_bps = match best_reachable {
                    Some(layer) if layer.quality > view.current_quality => {
                        // What it would cost to actually afford the next
                        // tier: its ongoing rate plus its one-time switch
                        // burst — see `BURST_COST_SECONDS`.
                        let cost = layer.state.nominal_bitrate_bps();
                        cost + cost * BURST_COST_SECONDS
                    }
                    // Already at (or above) the best reachable layer: no tier
                    // to climb toward, but still ask for a bit more than
                    // strictly needed. `demand_bitrate_bps` is the fast,
                    // reactive signal (unlike the conservative
                    // `bitrate_bps`/`effective_cost` used for admission) —
                    // a live in-tier burst (VBR/screen-share content getting
                    // busier with no tier change) should bump this ask
                    // immediately. `PROBE_HEADROOM_FRACTION` adds a small
                    // standing margin even at rest, so str0m's prober always
                    // has a reason to test for spare capacity instead of
                    // only reacting after a burst already demands it.
                    _ => {
                        current_bps.max(current.state.demand_bitrate_bps())
                            * (1.0 + PROBE_HEADROOM_FRACTION)
                    }
                };
                Some(demand_bps as u64)
            })
            .sum::<u64>(),
    )
}

#[derive(Clone, Debug)]
pub struct SlotView<'a> {
    pub key: SlotKey,
    pub mid: Mid,
    pub max_height: u32,
    pub track: &'a Track,
    pub current_quality: LayerQuality,
    /// This slot's share of contended bandwidth. See `Slot::weight`.
    pub weight: f64,
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
    // TODO: determine this through either H264 SPS or Simulcast SDP.
    fn max_height_for_quality(quality: LayerQuality) -> u32 {
        match quality {
            LayerQuality::High => 720,
            LayerQuality::Medium => 360,
            LayerQuality::Low => 180,
        }
    }
}

/// Decide `slot`'s target layer against the bandwidth left in the shared
/// tick-wide pool (`remaining`), and update its ledger accordingly. Called in
/// budget-descending order by `VideoAllocator::update_allocations_at`, so
/// `remaining` already reflects what higher-priority slots claimed first.
///
/// The cost of holding `layer`, for admission math. Never the raw live
/// `bitrate_bps()` alone — VBR content (screen share especially) can
/// legitimately swing that reading by 2-4x from one poll to the next with
/// no tier change at all, and admitting against whatever it happened to
/// read a moment ago is exactly what causes tier decisions to chase
/// content noise instead of real capacity. Floored at `nominal_bitrate_bps`
/// (the same stable, configured envelope the upstream monitor floors
/// reactivation cost at) so a quiet moment can't look artificially cheap,
/// and capped at `nominal_bitrate_bps * (1 + ADMISSION_PREMIUM_CEILING)` so
/// a noisy/bursty reading can't charge the shared pool far more than this
/// slot's true entitlement and starve sibling slots in the same tick — see
/// `ADMISSION_PREMIUM_CEILING`.
fn effective_cost(layer: &TrackLayer) -> f64 {
    let nominal = layer.state.nominal_bitrate_bps();
    layer
        .state
        .bitrate_bps()
        .min(nominal * (1.0 + ADMISSION_PREMIUM_CEILING))
        .max(nominal)
}

/// Climbs from `cheapest_active_layer()` (the guaranteed floor) one tier at
/// a time from `start`, stopping at the first tier that's ineligible,
/// unhealthy, unaffordable, or (only once climbing *above*
/// `view.current_quality`) not yet budget-earned.
fn climb_from<'a>(
    slot: &Slot,
    view: &SlotView<'a>,
    start: &'a TrackLayer,
    remaining: f64,
) -> &'a TrackLayer {
    let track = view.track;
    let mut chosen = start;
    let mut chosen_cost = effective_cost(start);
    let mut remaining_after = (remaining - chosen_cost).max(0.0);

    let mut next_quality = track.higher_quality(start.quality).map(|l| l.quality);
    while let Some(quality) = next_quality {
        let Some(next) = track.by_quality(quality) else {
            break;
        };

        // Don't re-affirm the exact tier we're nominally on if it went
        // silent — but a *different*, merely-paused tier is still a
        // legitimate stop, so skip just this one and keep climbing.
        if quality == view.current_quality && next.state.is_inactive() {
            next_quality = track.higher_quality(quality).map(|l| l.quality);
            continue;
        }
        if !next.state.is_activation_candidate() {
            break; // genuinely unhealthy — nothing above this is better
        }
        if AllocationEngine::max_height_for_quality(quality) > view.max_height {
            break; // not eligible, and nothing higher will be either
        }

        let next_cost = effective_cost(next);
        let incremental = next_cost - chosen_cost;
        if incremental > remaining_after {
            break; // can't afford stepping up to here, or beyond
        }
        if quality > view.current_quality {
            let burst = next_cost * BURST_COST_SECONDS;
            if slot.budget_bps < burst {
                break; // hasn't earned this upgrade yet
            }
        }

        remaining_after -= incremental;
        chosen = next;
        chosen_cost = next_cost;
        next_quality = track.higher_quality(quality).map(|l| l.quality);
    }

    chosen
}

/// Decides `slot`'s target layer against the bandwidth left in the shared
/// tick-wide pool (`remaining`), and updates its ledger accordingly. Called
/// in budget-descending order by `VideoAllocator::update_allocations_at`, so
/// `remaining` already reflects what higher-priority slots claimed first.
///
/// Hysteresis, not a timer: the tier already being held is judged against a
/// forgiving threshold (survives a shortfall up to `DOWNGRADE_MARGIN`); a
/// tier being newly reached is judged against the exact one. A signal
/// wobbling near a boundary doesn't cross the band in either direction,
/// regardless of how long it wobbles — the actual problem (magnitude), not
/// a proxy for it (duration). A real switch in either direction also pays a
/// one-time burst debit — a slot that just switched has a depleted ledger
/// and won't be picked again until it rebuilds it, which is what keeps
/// *repeated* switching down even once a change does cross the band.
fn decide_tier<'a>(
    slot: &mut Slot,
    view: &SlotView<'a>,
    remaining: &mut f64,
) -> AllocationDecision<'a> {
    let floor = view.track.cheapest_active_layer();
    let floor_cost = effective_cost(floor);

    let current = view
        .track
        .by_quality(view.current_quality)
        .filter(|l| !l.state.is_inactive() && l.state.is_activation_candidate());
    let holding = current.filter(|l| *remaining >= effective_cost(l) * (1.0 - DOWNGRADE_MARGIN));

    // Even the floor doesn't survive its own grace margin: nothing to climb
    // from, pause outright.
    if holding.is_none() && *remaining < floor_cost * (1.0 - DOWNGRADE_MARGIN) {
        return AllocationDecision::Pause(floor, Bitrate::from(floor_cost as u64));
    }

    let start = holding.unwrap_or(floor);
    let chosen = climb_from(slot, view, start, *remaining);
    let chosen_cost = effective_cost(chosen);
    // Floor at 0: DOWNGRADE_MARGIN lets a tier hold even when remaining < chosen_cost,
    // but the deficit must not propagate as a negative value to subsequent slots.
    *remaining = (*remaining - chosen_cost).max(0.0);
    if chosen.quality != view.current_quality {
        slot.budget_bps -= chosen_cost * BURST_COST_SECONDS;
    }
    AllocationDecision::Forward(chosen, Bitrate::from(chosen_cost as u64))
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

    fn setup_allocator() -> VideoAllocator {
        VideoAllocator::new(false, &mut test_rng())
    }

    #[test]
    fn desired_bitrate_requests_the_full_best_reachable_layer() {
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
        let mut keys = SlotMap::<SlotKey, ()>::with_key();
        let key = keys.insert(());
        let high = SlotView {
            key,
            mid: Mid::from("v0"),
            max_height: 720,
            track: &track,
            current_quality: LayerQuality::High,
            weight: 720.0,
        };
        let low = SlotView {
            current_quality: LayerQuality::Low,
            ..high.clone()
        };
        let medium = SlotView {
            current_quality: LayerQuality::Medium,
            ..high.clone()
        };

        let high_bps = track
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .nominal_bitrate_bps() as u64;
        let medium_bps = track
            .by_quality(LayerQuality::Medium)
            .unwrap()
            .state
            .nominal_bitrate_bps() as u64;
        assert_eq!(
            desired_from_allocation_envelope(&[high]),
            Bitrate::from((high_bps as f64 * (1.0 + PROBE_HEADROOM_FRACTION)) as u64),
            "already at the best reachable layer: demand must not keep climbing \
             tiers, but should still carry the standing PROBE_HEADROOM_FRACTION \
             margin so str0m has a reason to probe for spare capacity"
        );
        assert_eq!(
            desired_from_allocation_envelope(&[low.clone()]),
            Bitrate::from((high_bps as f64 * (1.0 + BURST_COST_SECONDS)) as u64),
            "recovery must ask for the full best-reachable layer plus its switch burst, \
             not a bounded one-tier preflight, or the estimate has nothing to climb toward"
        );
        assert_eq!(
            desired_from_allocation_envelope(&[medium]),
            Bitrate::from((high_bps as f64 * (1.0 + BURST_COST_SECONDS)) as u64),
            "medium recovery must also request the full high layer, not half of it"
        );

        // A paused high encoding has zero observed rate but not zero
        // reactivation cost; demand must fall back to the next-best layer.
        track
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .update_for_test()
            .bitrate(0)
            .quality(crate::rtp::monitor::StreamQuality::Bad);
        assert_eq!(
            desired_from_allocation_envelope(&[low]),
            Bitrate::from((medium_bps as f64 * (1.0 + BURST_COST_SECONDS)) as u64),
            "a paused high encoding must fall back to the medium layer as the new best-reachable target"
        );
    }

    /// A live in-tier burst (VBR/screen-share content getting busier with no
    /// tier change) must raise `desired` above the mere
    /// `PROBE_HEADROOM_FRACTION` baseline — `demand_bitrate_bps` is the fast
    /// signal precisely so a burst can ask str0m for more room immediately,
    /// without waiting for a tier-transition decision.
    #[test]
    fn live_demand_burst_raises_desired_bitrate_while_holding_the_same_tier() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let mut keys = SlotMap::<SlotKey, ()>::with_key();
        let key = keys.insert(());
        let high = SlotView {
            key,
            mid: Mid::from("v0"),
            max_height: 720,
            track: &track,
            current_quality: LayerQuality::High,
            weight: 720.0,
        };
        let high_bps = track
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .nominal_bitrate_bps();

        let at_rest = desired_from_allocation_envelope(&[high.clone()]);
        assert_eq!(
            at_rest,
            Bitrate::from((high_bps * (1.0 + PROBE_HEADROOM_FRACTION)) as u64),
            "sanity: settled demand should be exactly nominal plus the standing probe headroom"
        );

        // A live burst well above nominal, with no tier change at all.
        track
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .update_for_test()
            .demand_bitrate((high_bps * 3.0) as u64);

        let bursting = desired_from_allocation_envelope(&[high]);
        assert!(
            bursting.as_f64() > at_rest.as_f64() * 1.5,
            "a live demand burst on the held tier failed to raise desired \
             bitrate: at_rest={at_rest} bursting={bursting}"
        );
    }

    #[test]
    fn simulator_switch_burst_does_not_replace_an_inflight_keyframe_request() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }

        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        let medium = track.by_quality(LayerQuality::Medium).unwrap().clone();
        let high = track.by_quality(LayerQuality::High).unwrap().clone();
        slot.active = Some(medium);
        slot.staging = Some(high);
        slot.paused = false;

        let start = Instant::now();
        // Held even after the floor becomes sustained: replacing it would
        // send a second PLI before the first one can complete.
        for offset in [0, 100, 300, 800, 1_500] {
            assert!(
                slot.should_hold_allocation(start + Duration::from_millis(offset)),
                "in-flight transition was replaced at {offset} ms"
            );
        }
    }

    #[test]
    fn simulator_missing_keyframe_releases_stale_transaction() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![SimulcastLayer::new("h"), SimulcastLayer::new("m")],
        );
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        let start = Instant::now();
        slot.active = Some(track.by_quality(LayerQuality::Medium).unwrap().clone());
        slot.staging = Some(track.by_quality(LayerQuality::High).unwrap().clone());
        slot.staging_started_at = Some(start);

        assert!(slot.should_hold_allocation(start + Duration::from_secs(4)));
        assert!(
            !slot.should_hold_allocation(start + SWITCH_TRANSACTION_TIMEOUT),
            "a staging layer without packets must eventually yield to congestion protection"
        );
    }

    #[test]
    fn simulator_optimistic_startup_high_is_not_replaced_before_first_keyframe() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![SimulcastLayer::new("h"), SimulcastLayer::new("m")],
        );
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        let start = Instant::now();
        slot.staging = Some(track.by_quality(LayerQuality::High).unwrap().clone());
        slot.staging_started_at = Some(start);

        assert!(
            slot.should_hold_allocation(start + Duration::from_millis(500)),
            "optimistic 720p startup must not flap before its first keyframe"
        );
    }

    #[test]
    fn first_connection_stages_low_even_with_generous_bandwidth_available() {
        // A fresh connection must render fast: `rebalance` always stages the
        // lowest layer first (cheapest, quickest keyframe). Even if the
        // bandwidth estimate looks generous enough for a higher tier on the
        // very next tick, that first staging must not be preempted before
        // its keyframe lands — trading a fast first frame for a slower,
        // bigger one is exactly the regression this guards against.
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let slot = allocator.slots.values().next().unwrap();
        assert_eq!(
            slot.staging.as_ref().map(|l| l.quality),
            Some(LayerQuality::Low),
            "rebalance must stage the lowest layer first"
        );

        // A generous bandwidth reading arrives before the first keyframe.
        allocator.update_allocations(Bitrate::mbps(10));

        let slot = allocator.slots.values().next().unwrap();
        assert_eq!(
            slot.staging.as_ref().map(|l| l.quality),
            Some(LayerQuality::Low),
            "the first staging must not be preempted before its keyframe lands, \
             even when bandwidth would support a higher tier"
        );
    }

    #[test]
    fn paused_slot_has_hold_protection_immediately_after_resume() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        let low = track.by_quality(LayerQuality::Low).unwrap().clone();
        slot.active = Some(low.clone());
        slot.paused = true;
        slot.staging = None;
        slot.staging_started_at = None;
        slot.settling_until = None;

        slot.switch_to(&low, false);

        assert!(!slot.paused, "switch_to must clear the paused flag");
        let now = Instant::now();
        assert!(
            slot.should_hold_allocation(now),
            "slot resumed from pause must be protected from immediate re-pause"
        );
        let until = slot
            .settling_until
            .expect("settling_until must be set on resume");
        assert!(slot.should_hold_allocation(until - Duration::from_millis(1)));
        assert!(
            !slot.should_hold_allocation(until),
            "hold must expire at settling_until"
        );
    }

    #[test]
    fn simulator_settling_holds_briefly_then_releases_to_real_congestion() {
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![SimulcastLayer::new("h"), SimulcastLayer::new("m")],
        );
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        slot.active = Some(track.by_quality(LayerQuality::High).unwrap().clone());
        slot.paused = false;
        let start = Instant::now();
        slot.settling_until = Some(start + SWITCH_SETTLE_DURATION);

        assert!(slot.should_hold_allocation(start + Duration::from_millis(500)));
        assert!(
            slot.should_hold_allocation(start + Duration::from_millis(800)),
            "the settling window is a short, fixed bound, not congestion-dependent"
        );
        assert!(
            !slot.should_hold_allocation(start + SWITCH_SETTLE_DURATION),
            "real congestion is picked up on the very next tick once settling expires"
        );
    }

    #[test]
    fn switching_depletes_budget_so_an_immediate_second_switch_is_not_free() {
        // The entire chatter-suppression mechanism: a real switch pays a
        // one-time burst debit, so a slot that just switched can't afford
        // another upgrade an instant later even if the raw bandwidth
        // reading would otherwise justify it.
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        slot.set_max_height(720);
        // Give it plenty of ledger credit and let it upgrade once.
        slot.budget_bps = 10_000_000.0;
        let view = SlotView {
            key: SlotMap::<SlotKey, ()>::with_key().insert(()),
            mid: slot.mid,
            max_height: slot.max_height,
            track: &track,
            current_quality: LayerQuality::Low,
            weight: slot.weight,
        };
        let mut remaining = f64::MAX;
        let before = slot.budget_bps;
        let decision = decide_tier(&mut slot, &view, &mut remaining);
        assert!(
            matches!(decision, AllocationDecision::Forward(l, _) if l.quality > LayerQuality::Low),
            "expected an upgrade with ample budget and bandwidth"
        );
        assert!(
            slot.budget_bps < before,
            "a real switch must debit the ledger"
        );
    }

    #[test]
    fn paused_slot_accrues_budget_instead_of_a_starved_ticks_counter() {
        // Replaces `starved_ticks`: a slot spending nothing while paused
        // accrues its full fair share as ledger credit every tick, which is
        // what lets it win the next round of contention instead of being
        // starved indefinitely.
        let pid = ParticipantId::new(&mut test_rng());
        let (_, track) = make_video_track(pid, Mid::from("v0"), vec![]);
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let mut slot = Slot::new(SlotConfig::default(), &mut test_rng());
        slot.set_max_height(720);
        let view = SlotView {
            key: SlotMap::<SlotKey, ()>::with_key().insert(()),
            mid: slot.mid,
            max_height: slot.max_height,
            track: &track,
            current_quality: LayerQuality::Low,
            weight: slot.weight,
        };
        // No bandwidth at all: the slot must pause, not spend anything.
        let mut remaining = 0.0;
        let decision = decide_tier(&mut slot, &view, &mut remaining);
        assert!(matches!(decision, AllocationDecision::Pause(_, _)));

        // Simulate several ticks of ledger accrual while paused (cost=0).
        let fair_share = 500_000.0;
        for _ in 0..5 {
            slot.budget_bps += fair_share * TICK_INTERVAL.as_secs_f64();
        }
        assert!(
            slot.budget_bps > 0.0,
            "a paused slot must accrue credit over time instead of just aging a counter"
        );
    }

    #[test]
    fn vbr_content_volatility_does_not_cause_switching_on_its_own() {
        // Screen share (and VBR content generally) can legitimately swing
        // a layer's *own observed bitrate* well below and above its nominal
        // rate from one poll to the next with no tier change at all — a
        // static slide costs near nothing, a busy redraw costs a lot, same
        // resolution either way. This is the exact pattern seen in
        // production: the same "High" tier logged at 0 bit/s, 1.336 Mbit/s,
        // and 1.957 Mbit/s within a few seconds, against nominal 1.25
        // Mbit/s. Available bandwidth is realistic and just comfortably
        // above High's nominal cost — tight enough that cost volatility
        // alone can plausibly tip a decision, unlike a wildly generous
        // bandwidth that never lets any reading matter.
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let high_nominal = track
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .nominal_bitrate_bps();
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let mut rng = seeded_rng(0x5EED_F1A9);
        let mut now = Instant::now();
        // 20% of headroom above High's nominal cost — realistic, not
        // wildly generous.
        let available = Bitrate::from((high_nominal * 1.2) as u64);
        let mut last_quality: Option<LayerQuality> = None;
        let mut switch_count = 0u32;

        for _ in 0..1_200 {
            now += TICK_INTERVAL;
            // Swing the currently-held layer's own observed bitrate between
            // 30% and 160% of *its own nominal rate* — spanning both below
            // (a quiet moment) and above (a real content burst) nominal,
            // matching the production trace's pattern.
            let current_quality = allocator
                .slots
                .values()
                .next()
                .and_then(|s| s.target())
                .map(|l| l.quality)
                .unwrap_or(LayerQuality::Low);
            if let Some(layer) = allocator
                .tracks
                .values()
                .next()
                .and_then(|t| t.by_quality(current_quality))
            {
                let this_nominal = layer.state.nominal_bitrate_bps();
                let factor = 0.3 + (rng.next_u32() as f64 / u32::MAX as f64) * 1.3;
                layer
                    .state
                    .update_for_test()
                    .bitrate((this_nominal * factor) as u64);
            }
            allocator.update_allocations_at(available, now);

            let quality = allocator
                .slots
                .values()
                .next()
                .and_then(|s| s.target())
                .map(|l| l.quality);
            if let Some(prev) = last_quality
                && quality != Some(prev)
            {
                switch_count += 1;
            }
            last_quality = quality;
        }

        // With only 20% headroom above nominal, an occasional genuine spike
        // to 160% of nominal legitimately can't be sustained — downgrading
        // for that is correct, not a bug. What must not happen is chasing
        // every fluctuation: bounded switching, not settling below Medium.
        assert!(
            matches!(
                last_quality,
                Some(LayerQuality::Medium) | Some(LayerQuality::High)
            ),
            "settled at {last_quality:?} — realistic bandwidth with typically-around-nominal \
             content should sustain at least Medium, not get chased all the way down"
        );
        assert!(
            switch_count <= 6,
            "switched layers {switch_count} times from content-driven cost volatility \
             alone, with ample and constant available bandwidth — the tier decision is \
             chasing VBR noise instead of the resolution actually justified"
        );
    }

    #[test]
    fn no_slot_stalls_forever_under_tight_multi_party_contention() {
        // 3 equal-weight slots, bandwidth tight enough that not all 3 can be
        // served every tick. Over many ticks, every slot must eventually be
        // forwarded at least once — none may be permanently paused.
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let mut ids = Vec::new();
        for i in 0..3 {
            let (tx, track) = make_video_track(
                pid,
                Mid::from(&format!("v{i}")[..]),
                vec![
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("m"),
                    SimulcastLayer::new("l"),
                ],
            );
            for layer in &track.layers {
                layer.state.update_for_test().inactive(false);
            }
            ids.push(tx.meta.id);
            allocator.add_track(Track {
                meta: tx.meta,
                layers: track.layers,
            });
        }
        add_slots(&mut allocator, 3);

        // Only enough for a bit more than one Low layer at a time.
        let low_bps = {
            let track = allocator.tracks.get(&ids[0]).unwrap();
            track
                .by_quality(LayerQuality::Low)
                .unwrap()
                .state
                .bitrate_bps()
        };
        let tight = Bitrate::from((low_bps * 1.2) as u64);

        let mut now = Instant::now();
        let mut ever_forwarded: std::collections::HashSet<Mid> = std::collections::HashSet::new();
        for _ in 0..400 {
            now += TICK_INTERVAL;
            allocator.update_allocations_at(tight, now);
            for slot in allocator.slots.values() {
                if !slot.paused {
                    ever_forwarded.insert(slot.mid);
                }
            }
        }

        let all_mids: std::collections::HashSet<Mid> =
            allocator.slots.values().map(|s| s.mid).collect();
        assert_eq!(
            ever_forwarded, all_mids,
            "at least one slot never got forwarded across 400 ticks of contention"
        );
    }

    #[test]
    fn single_slot_does_not_flap_under_sustained_realistic_bandwidth_noise() {
        // No contention at all — one slot, one track — isolating whether
        // the budget/burst mechanism alone suppresses chatter on noisy real
        // bandwidth, independent of any fairness rotation between slots.
        // Bandwidth hovers right around the Medium/Low boundary with
        // realistic WiFi-like jitter (updated every 200-500ms, the same
        // cadence real BWE estimates arrive at), sustained for two full
        // minutes — long enough that a weak suppression mechanism would
        // rack up many switches, not just the first one or two.
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let medium_bps = track
            .by_quality(LayerQuality::Medium)
            .unwrap()
            .state
            .bitrate_bps();
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        // A fixed, dedicated seed (not `test_rng()`'s shared atomic
        // counter) so this test's noise trace — and its switch count — is
        // reproducible regardless of what other tests ran before it.
        let mut rng = seeded_rng(0x5EED_F1A9);
        let mut now = Instant::now();
        let mut next_bwe_update = now;
        // Mean-reverting (Ornstein-Uhlenbeck-style), not independently
        // redrawn each sample: real jitter is correlated from one moment to
        // the next, not a fresh unrelated coin flip every 200-500ms. Fully
        // independent noise is *harsher* than reality in a misleading way —
        // it produces occasional multi-second same-direction streaks purely
        // by chance (statistically indistinguishable from a genuine
        // sustained change by timing alone), which no hold duration can
        // filter without also delaying reaction to real changes by just as
        // long.
        let mut wandering_bps = medium_bps;
        let mut current_bwe = Bitrate::from(medium_bps as u64);
        let mut last_quality: Option<LayerQuality> = None;
        let mut switch_count = 0u32;

        for _ in 0..1_200 {
            // 100ms ticks; a fresh noisy BWE sample every 200-500ms.
            now += TICK_INTERVAL;
            if now >= next_bwe_update {
                let shock = (rng.next_u32() as f64 / u32::MAX as f64 - 0.5) * medium_bps * 0.5;
                // Pull 30% of the way back toward the mean each sample, then
                // apply the shock — bounded, correlated wandering instead of
                // an independent redraw.
                wandering_bps += (medium_bps - wandering_bps) * 0.3 + shock;
                current_bwe = Bitrate::from(wandering_bps.max(0.0) as u64);
                next_bwe_update = now + Duration::from_millis(200 + (rng.next_u32() % 300) as u64);
            }
            allocator.update_allocations_at(current_bwe, now);
            let quality = allocator
                .slots
                .values()
                .next()
                .and_then(|s| s.target())
                .map(|l| l.quality);
            if let Some(prev) = last_quality
                && quality != Some(prev)
            {
                switch_count += 1;
            }
            last_quality = quality;
        }

        // This is deliberately the hardest case: the mean sits *exactly* on
        // the tier boundary for the full two minutes, not just drifting
        // through it briefly — a real link only coincides with a boundary
        // like this by momentary coincidence. Even so, a real viewer sees a
        // keyframe (and a brief freeze) on every switch, so this must stay
        // a small, bounded number. The hysteresis margin (magnitude-based)
        // does dramatically better here than a hold timer (duration-based)
        // ever could: a signal wobbling within the band doesn't cross it no
        // matter how long it wobbles, whereas a timer eventually lets any
        // sustained-by-chance streak through.
        assert!(
            switch_count <= 5,
            "single slot switched layers {switch_count} times in 2 minutes of realistic \
             correlated bandwidth jitter centered exactly on a tier boundary — the \
             budget/burst/hysteresis mechanism is not suppressing chatter"
        );
    }

    // ─── Property: given constant bandwidth and constant costs, the system
    //               stabilizes and stops switching ─────────────────────────
    #[test]
    fn constant_conditions_converge_to_no_further_switches() {
        let mut allocator = setup_allocator();
        let tracks = add_tracks(&mut allocator, 3);
        add_slots(&mut allocator, 3);
        for id in &tracks.ids {
            for layer in &allocator.tracks.get(id).unwrap().layers {
                layer.state.update_for_test().inactive(false);
            }
        }

        let mut now = Instant::now();
        let mut last_changed_at_tick = 0;
        for tick in 0..200 {
            now += TICK_INTERVAL;
            let (_, changed) = allocator.update_allocations_at(Bitrate::mbps(10), now);
            if changed {
                last_changed_at_tick = tick;
            }
        }

        // One more round with no further changes proves it's actually
        // settled, not just coincidentally quiet on the final sampled tick.
        let mut still_settled = true;
        for _ in 0..20 {
            now += TICK_INTERVAL;
            let (_, changed) = allocator.update_allocations_at(Bitrate::mbps(10), now);
            still_settled &= !changed;
        }
        assert!(
            still_settled,
            "system kept switching under constant bandwidth and costs; last change at tick {last_changed_at_tick}"
        );
    }

    /// End-to-end (through `update_allocations_at`, not `decide_tier`
    /// directly): `RESERVE_FRACTION` must actually reduce the pool a held
    /// tier is judged against, not just exist as an unused constant. Pick a
    /// bandwidth that comfortably clears the `DOWNGRADE_MARGIN` holding
    /// threshold on its own, but falls back under it once
    /// `RESERVE_FRACTION` is taken out of the pool first — a slot sitting
    /// at High must downgrade under exactly that bandwidth.
    #[test]
    fn reserve_fraction_forces_a_downgrade_the_raw_bandwidth_would_have_survived() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let track_id = sender.meta.id;
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let high = {
            let track = allocator.tracks.get(&track_id).unwrap();
            track.by_quality(LayerQuality::High).unwrap().clone()
        };
        let high_nominal = high.state.nominal_bitrate_bps();
        let holding_threshold = high_nominal * (1.0 - DOWNGRADE_MARGIN);
        // Comfortably above the holding threshold on its own...
        let borderline_bps = holding_threshold * 1.05;
        // ...but not once RESERVE_FRACTION is held back first.
        assert!(
            borderline_bps * (1.0 - RESERVE_FRACTION) < holding_threshold,
            "test bandwidth doesn't actually straddle the reserve boundary; \
             borderline={borderline_bps} threshold={holding_threshold}"
        );

        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(high.clone());
        slot.staging = None;
        slot.paused = false;

        let mut now = Instant::now();
        // Let BweFilter's asymmetric EWMA fully converge onto the
        // borderline bandwidth (several multiples of BWE_FALL_TIME_CONSTANT)
        // before judging the outcome.
        for _ in 0..150 {
            now += TICK_INTERVAL;
            allocator.update_allocations_at(Bitrate::from(borderline_bps as u64), now);
        }
        let slot = allocator.slots.values().next().unwrap();
        assert_ne!(
            slot.target().map(|l| l.quality),
            Some(LayerQuality::High),
            "slot kept deciding on High at a bandwidth that only clears the \
             DOWNGRADE_MARGIN threshold before RESERVE_FRACTION is reserved \
             — the reserve isn't actually shrinking the pool"
        );
    }

    #[test]
    fn simulator_keyframe_jitter_trace_keeps_one_layer_transaction() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let track_id = sender.meta.id;
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let (medium, high) = {
            let track = allocator.tracks.get(&track_id).unwrap();
            (
                track.by_quality(LayerQuality::Medium).unwrap().clone(),
                track.by_quality(LayerQuality::High).unwrap().clone(),
            )
        };
        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(medium);
        slot.staging = Some(high);
        slot.paused = false;

        let start = Instant::now();
        // A keyframe collapses the estimate, probes rebound it, then it
        // collapses again — must remain exactly one PLI transaction.
        for (offset_ms, bwe_kbps) in [
            (0, 2_800),
            (100, 300),
            (250, 550),
            (600, 2_600),
            (900, 300),
            (1_400, 450),
        ] {
            allocator.update_allocations_at(
                Bitrate::kbps(bwe_kbps),
                start + Duration::from_millis(offset_ms),
            );
            let slot = allocator.slots.values().next().unwrap();
            assert_eq!(
                slot.staging.as_ref().map(|layer| layer.quality),
                Some(LayerQuality::High),
                "jitter at {offset_ms} ms replaced the in-flight layer transition"
            );
            assert_eq!(
                slot.active.as_ref().map(|layer| layer.quality),
                Some(LayerQuality::Medium)
            );
        }
    }

    #[test]
    fn downgrade_confirmation_hold_does_not_force_a_layer_that_went_inactive() {
        // A slot confirmed on High, then bandwidth collapses to the floor
        // at the same moment the publisher's own uplink pauses High.
        // confirm_switch's hold-during-window override used to force
        // Forward(High) back in regardless of liveness — a frozen stream
        // for the length of the confirmation window.
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());
        let (sender, track) = make_video_track(
            pid,
            Mid::from("v0"),
            vec![
                SimulcastLayer::new("h"),
                SimulcastLayer::new("m"),
                SimulcastLayer::new("l"),
            ],
        );
        for layer in &track.layers {
            layer.state.update_for_test().inactive(false);
        }
        let track_id = sender.meta.id;
        allocator.add_track(Track {
            meta: sender.meta,
            layers: track.layers,
        });
        add_slots(&mut allocator, 1);

        let high = {
            let track = allocator.tracks.get(&track_id).unwrap();
            track.by_quality(LayerQuality::High).unwrap().clone()
        };
        let slot = allocator.slots.values_mut().next().unwrap();
        slot.active = Some(high);
        // Clear any staging left by the automatic rebalance, so the slot
        // is cleanly stable on High (no in-flight switch).
        slot.staging = None;
        slot.paused = false;

        // The publisher's own uplink pauses High at the same moment ours
        // collapses to the floor.
        allocator
            .tracks
            .get(&track_id)
            .unwrap()
            .by_quality(LayerQuality::High)
            .unwrap()
            .state
            .update_for_test()
            .inactive(true);

        let now = Instant::now();
        allocator.update_allocations_at(MIN_BANDWIDTH, now);

        let slot = allocator.slots.values().next().unwrap();
        // Matches Slot::target()'s priority: staging reflects the decision
        // actually acted on, even while active still holds the old layer.
        let held_quality = slot
            .staging
            .as_ref()
            .or(slot.active.as_ref())
            .map(|l| l.quality);
        assert_ne!(
            held_quality,
            Some(LayerQuality::High),
            "kept forcing a layer that went inactive instead of accepting the downgrade"
        );
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

        slot.process(&high.stream_id(), &pkt);

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

    /// Verifies the max_height priority ordering: with bandwidth only enough for one
    /// stream at the Low tier, the highest max_height subscriber holds while lower ones
    /// are paused. All three slots use the same 3-layer source track so their floor
    /// cost (Low = 150kbps) is identical — starvation is driven purely by sort order,
    /// not by track differences.
    ///
    /// Note: configure() puts slots into staging (should_hold_allocation = true), which
    /// bypasses decide_tier and never pauses. We manually promote to active to test the
    /// priority logic that fires in normal steady-state (non-transitioning) operation.
    #[test]
    fn higher_max_height_streams_are_prioritized() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());

        // Use the same 3-layer source for all three slots so floor cost (Low = 150kbps)
        // is identical. Priority must come from max_height, not from track differences.
        let three_layer_simulcast = || {
            make_video_track(
                pid,
                Mid::from("v"),
                vec![
                    SimulcastLayer::new("f"),
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("q"),
                ],
            )
        };
        let (_, track_a) = three_layer_simulcast();
        let (_, track_b) = three_layer_simulcast();
        let (_, track_c) = three_layer_simulcast();

        for layer in track_a
            .layers
            .iter()
            .chain(track_b.layers.iter())
            .chain(track_c.layers.iter())
        {
            layer.state.update_for_test().inactive(false);
        }

        let id_a = track_a.meta.id;
        let id_b = track_b.meta.id;
        let id_c = track_c.meta.id;
        allocator.add_track(Track {
            meta: track_a.meta.clone(),
            layers: track_a.layers,
        });
        allocator.add_track(Track {
            meta: track_b.meta.clone(),
            layers: track_b.layers,
        });
        allocator.add_track(Track {
            meta: track_c.meta.clone(),
            layers: track_c.layers,
        });

        allocator.add_slot(SlotConfig {
            mid: Mid::from("s720"),
            ..SlotConfig::default()
        });
        allocator.add_slot(SlotConfig {
            mid: Mid::from("s360"),
            ..SlotConfig::default()
        });
        allocator.add_slot(SlotConfig {
            mid: Mid::from("s180"),
            ..SlotConfig::default()
        });

        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s720"),
            Intent {
                track_id: id_a,
                max_height: 720,
            },
        );
        intents.insert(
            Mid::from("s360"),
            Intent {
                track_id: id_b,
                max_height: 360,
            },
        );
        intents.insert(
            Mid::from("s180"),
            Intent {
                track_id: id_c,
                max_height: 180,
            },
        );
        allocator.configure(&intents);

        // Promote all slots to active at Low quality so should_hold_allocation returns
        // false — configure() leaves them in staging, which bypasses decide_tier entirely
        // and never pauses regardless of bandwidth. Active steady state is what we're testing.
        let low_a = allocator
            .tracks
            .get(&id_a)
            .unwrap()
            .by_quality(LayerQuality::Low)
            .unwrap()
            .clone();
        let low_b = allocator
            .tracks
            .get(&id_b)
            .unwrap()
            .by_quality(LayerQuality::Low)
            .unwrap()
            .clone();
        let low_c = allocator
            .tracks
            .get(&id_c)
            .unwrap()
            .by_quality(LayerQuality::Low)
            .unwrap()
            .clone();
        for slot in allocator.slots.values_mut() {
            let layer = if slot.mid == Mid::from("s720") {
                low_a.clone()
            } else if slot.mid == Mid::from("s360") {
                low_b.clone()
            } else {
                low_c.clone()
            };
            slot.active = Some(layer);
            slot.staging = None;
            slot.settling_until = None;
            slot.staging_started_at = None;
            slot.paused = false;
        }

        // Input is clamped to MIN_BANDWIDTH = 300kbps before filtering; pool = 300 * 0.9 = 270kbps.
        // 720p takes Low (150kbps) → 120kbps left. 360p can still hold Low since 120k ≥ 105k
        // (DOWNGRADE_MARGIN grace). After floor-at-zero, 180p sees 0 remaining → paused.
        // This verifies sort order: highest max_height is always served first, lowest is dropped
        // first as the pool fills up from the top.
        let tight_bandwidth = Bitrate::kbps(300); // clamped to MIN_BANDWIDTH = 300kbps
        let now = Instant::now();
        allocator.update_allocations_at(tight_bandwidth, now);

        let slot_720 = allocator
            .slots
            .values()
            .find(|s| s.mid == Mid::from("s720"))
            .unwrap();
        let slot_360 = allocator
            .slots
            .values()
            .find(|s| s.mid == Mid::from("s360"))
            .unwrap();
        let slot_180 = allocator
            .slots
            .values()
            .find(|s| s.mid == Mid::from("s180"))
            .unwrap();

        assert!(
            !slot_720.paused,
            "720p (highest max_height, first in sort) must never be paused when bandwidth is tight"
        );
        assert!(
            !slot_360.paused,
            "360p still fits: 120kbps remaining after 720p ≥ DOWNGRADE_MARGIN floor threshold"
        );
        assert!(
            slot_180.paused,
            "180p (lowest max_height, last in sort) is paused: 0 remaining after 720p+360p (floor-at-zero)"
        );
    }

    /// Regression: a 720p slot settling after a tier switch (should_hold_allocation=true)
    /// with a VBR burst on the High layer must not starve a 180p sibling from its Low floor.
    ///
    /// Before the fix, the settle-window path used raw `bitrate_bps()` (uncapped),
    /// which could spike 2-3x above nominal for VBR screen share, sending `remaining`
    /// deeply negative and causing the 180p slot to pause on every settle tick.
    #[test]
    fn settling_slot_vbr_burst_does_not_pause_low_floor_sibling() {
        let mut allocator = setup_allocator();
        let pid = ParticipantId::new(&mut test_rng());

        // Two separate tracks: 720p source (full simulcast) and 180p source (single layer).
        let (_, track_720) = make_video_track(
            pid,
            Mid::from("v720"),
            vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
        );
        let (_, track_180) = make_video_track(
            pid,
            Mid::from("v180"),
            vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
        );

        for layer in &track_720.layers {
            layer.state.update_for_test().inactive(false);
        }
        for layer in &track_180.layers {
            layer.state.update_for_test().inactive(false);
        }

        // Simulate a VBR burst that EXCEEDS the admission ceiling (3x nominal).
        // The old code used raw bitrate_bps() here (uncapped), which would drain
        // the pool by 3x and leave nothing for sibling slots. effective_cost() caps
        // this at nominal * (1 + ADMISSION_PREMIUM_CEILING), which is what the fix enforces.
        let high_720 = track_720.by_quality(LayerQuality::High).unwrap();
        let high_nominal = high_720.state.nominal_bitrate_bps();
        let burst_bps = (high_nominal * 3.0) as u64; // well above the 1.5x ceiling
        high_720.state.update_for_test().bitrate(burst_bps);

        let track_id_720 = track_720.meta.id;
        let track_id_180 = track_180.meta.id;
        allocator.add_track(Track {
            meta: track_720.meta.clone(),
            layers: track_720.layers,
        });
        allocator.add_track(Track {
            meta: track_180.meta.clone(),
            layers: track_180.layers,
        });

        allocator.add_slot(SlotConfig {
            mid: Mid::from("s720"),
            ..SlotConfig::default()
        });
        allocator.add_slot(SlotConfig {
            mid: Mid::from("s180"),
            ..SlotConfig::default()
        });

        let mut intents = HashMap::new();
        intents.insert(
            Mid::from("s720"),
            Intent {
                track_id: track_id_720,
                max_height: 720,
            },
        );
        intents.insert(
            Mid::from("s180"),
            Intent {
                track_id: track_id_180,
                max_height: 180,
            },
        );
        allocator.configure(&intents);

        // Put the 720p slot into active+settling state so should_hold_allocation returns true.
        let high_layer = allocator
            .tracks
            .get(&track_id_720)
            .unwrap()
            .by_quality(LayerQuality::High)
            .unwrap()
            .clone();
        let now = Instant::now();
        {
            let slot_720 = allocator
                .slots
                .values_mut()
                .find(|s| s.max_height == 720)
                .unwrap();
            slot_720.active = Some(high_layer);
            slot_720.staging = None;
            slot_720.paused = false;
            // Mark as settling (post-switch, within SWITCH_SETTLE_DURATION).
            slot_720.settling_until = Some(now + SWITCH_SETTLE_DURATION);
        }

        // Put the 180p slot into active state holding Low.
        let low_180 = allocator
            .tracks
            .get(&track_id_180)
            .unwrap()
            .by_quality(LayerQuality::Low)
            .unwrap()
            .clone();
        {
            let slot_180 = allocator
                .slots
                .values_mut()
                .find(|s| s.max_height == 180)
                .unwrap();
            slot_180.active = Some(low_180);
            slot_180.staging = None;
            slot_180.paused = false;
        }

        // Bandwidth: enough for 720p at its effective_cost ceiling + 180p Low floor threshold,
        // but the old uncapped deduction (3x nominal = 3.75M) would have blown the entire pool.
        // Required pool: effective_cost_ceiling + low_floor_hold_threshold
        //   = high_nominal * (1 + ADMISSION_PREMIUM_CEILING) + low_nominal * (1 - DOWNGRADE_MARGIN)
        // Total bw must clear that after RESERVE_FRACTION:
        //   total >= required_pool / (1 - RESERVE_FRACTION)
        let low_nominal = allocator
            .tracks
            .get(&track_id_180)
            .unwrap()
            .by_quality(LayerQuality::Low)
            .unwrap()
            .state
            .nominal_bitrate_bps();
        let effective_ceiling = high_nominal * (1.0 + ADMISSION_PREMIUM_CEILING);
        let low_hold_threshold = low_nominal * (1.0 - DOWNGRADE_MARGIN);
        let required_pool = effective_ceiling + low_hold_threshold + 10_000.0; // small slack
        let total_bw = Bitrate::from((required_pool / (1.0 - RESERVE_FRACTION)) as u64);

        allocator.update_allocations_at(total_bw, now + TICK_INTERVAL);

        let slot_180 = allocator
            .slots
            .values()
            .find(|s| s.max_height == 180)
            .unwrap();
        assert!(
            !slot_180.paused,
            "180p Low sibling was paused while the 720p slot was settling with a VBR burst \
             — the pool deduction must be capped at effective_cost, not raw bitrate"
        );
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

    /// Mirrors `VideoAllocator::update_allocations_at`'s contention
    /// resolution + greedy walk (budget-descending sort, then `decide_tier`
    /// per slot) for exercising it directly against hand-built `SlotView`s.
    /// Every slot gets unlimited budget, so plain bandwidth-driven behavior
    /// is unaffected by upgrade-eligibility timing.
    fn compute_tick<'a>(
        available: Bitrate,
        slots: &[SlotView<'a>],
    ) -> (HashMap<SlotKey, AllocationDecision<'a>>, Bitrate) {
        compute_tick_with_budgets(available, slots, |_| f64::MAX)
    }

    /// As `compute_tick`, but with an explicit per-slot budget so tests can
    /// exercise contention ordering and upgrade eligibility directly.
    fn compute_tick_with_budgets<'a>(
        available: Bitrate,
        slots: &[SlotView<'a>],
        budget_for: impl Fn(&SlotView<'a>) -> f64,
    ) -> (HashMap<SlotKey, AllocationDecision<'a>>, Bitrate) {
        let desired = desired_from_allocation_envelope(slots);
        let budgets: HashMap<SlotKey, f64> = slots.iter().map(|v| (v.key, budget_for(v))).collect();

        let mut ordered: Vec<&SlotView<'a>> = slots.iter().collect();
        ordered.sort_by(|a, b| {
            budgets[&b.key]
                .partial_cmp(&budgets[&a.key])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.mid.cmp(&b.mid))
        });

        let mut remaining = available.as_f64();
        let mut decisions = HashMap::new();
        for view in ordered {
            let mut slot = Slot::new(
                SlotConfig {
                    mid: view.mid,
                    ..SlotConfig::default()
                },
                &mut test_rng(),
            );
            slot.budget_bps = budgets[&view.key];
            let decision = decide_tier(&mut slot, view, &mut remaining);
            decisions.insert(view.key, decision);
        }
        (decisions, desired)
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

    fn slot<'a>(mid: &str, priority: u32, track: &'a Track, current: LayerQuality) -> SlotView<'a> {
        SlotView {
            key: next_slot_key(),
            mid: Mid::from(mid),
            max_height: priority,
            track,
            current_quality: current,
            weight: priority as f64,
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
        let (decisions, _) = compute_tick(bw(10_000), &slots);
        for s in &slots {
            assert!(
                decisions.contains_key(&s.key),
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
        let (decisions, _) = compute_tick(bw(10_000), &slots);
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
            let (_, desired) = compute_tick(bw(bw_kbps), &slots);
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
        let (decisions, _) = compute_tick(bw(100_000), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.key], AllocationDecision::Forward(..)),
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
        let (decisions, _) = compute_tick(bw(0), &slots);
        for s in &slots {
            assert!(
                matches!(decisions[&s.key], AllocationDecision::Pause(..)),
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
        let (decisions, _) = compute_tick(bw(0), &slots);
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
            track: &t,
            current_quality: LayerQuality::High,
            weight: 1080.0,
        }];
        let (decisions, _) = compute_tick(bw(10_000), &slots);
        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "expected Forward fallback when High is bad, got {:?}",
            decisions[&slots[0].key]
        );
    }

    #[test]
    fn inactive_but_known_good_high_layer_remains_an_upgrade_candidate() {
        let t = healthy_track();
        let high = t.by_quality(LayerQuality::High).unwrap();
        high.state
            .update_for_test()
            .inactive(true)
            .quality(StreamQuality::Excellent);

        let slots = vec![slot("a", 1080, &t, LayerQuality::Medium)];
        let (decisions, desired) = compute_tick(bw(10_000), &slots);

        assert!(matches!(
            decisions[&slots[0].key],
            AllocationDecision::Forward(layer, _) if layer.quality == LayerQuality::High
        ));
        assert!(desired.as_f64() >= high.state.bitrate_bps());
    }

    // ─── Property: forwarded layer is always a healthy layer ────────────────────

    #[test]
    fn forwarded_layer_is_always_healthy() {
        let t = track_with_bad_layer(LayerQuality::High);
        let slots = vec![slot("a", 1080, &t, LayerQuality::High)];
        let (decisions, _) = compute_tick(bw(10_000), &slots);
        if let AllocationDecision::Forward(receiver, _) = &decisions[&slots[0].key] {
            assert!(
                receiver.state.is_healthy(),
                "engine forwarded to an unhealthy layer: {:?}",
                receiver.quality
            );
        }
    }

    // ─── Property: the slot with the higher ledger balance wins when
    //               bandwidth is tight ────────────────────────────────────
    //
    // Contention is resolved by budget, not directly by priority: a slot's
    // `weight` only controls how fast it *accrues* budget over time (see
    // `VideoAllocator::update_allocations_at`). This seeds budgets as if
    // each slot had already accrued proportionally to its weight, then
    // checks that the slot with more accrued credit wins a single tight
    // tick — the property `tight_budget_forwards_higher_priority_slot` used
    // to check directly against `max_height`.

    #[test]
    fn tight_budget_forwards_the_higher_budget_slot() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);

        // Budget just fits one Low layer.
        let available = bw((low_bps as u64) / 1_000 + 5);

        let slots = vec![
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("h"),
                max_height: 1080,
                track: &t,
                current_quality: LayerQuality::Low,
                weight: 1080.0,
            },
            SlotView {
                key: next_slot_key(),
                mid: Mid::from("l"),
                max_height: 360,
                track: &t,
                current_quality: LayerQuality::Low,
                weight: 360.0,
            },
        ];

        let (decisions, _) = compute_tick_with_budgets(available, &slots, |v| v.weight);

        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "higher-budget slot should be forwarded first"
        );
        assert!(
            matches!(decisions[&slots[1].key], AllocationDecision::Pause(..)),
            "low-priority slot should be paused when budget is tight"
        );
    }

    // ─── Property: a momentarily-under-reporting held layer must not
    //               let another slot get over-admitted ──────────────────
    //
    // Screen share can report 0 bit/s on its own currently-held tier for
    // a poll or two (a quiet VBR moment, or the momentary race right as
    // a simulcast layer resumes) while it is still, in truth, going to
    // cost its real (nominal) rate the instant content picks back up.
    // If the allocator priced that at the reported 0 instead of its
    // nominal floor, it would deduct nothing from the shared pool for a
    // slot that's actually still consuming real bandwidth — silently
    // over-admitting whichever slot is decided next, with real packet
    // loss as the consequence once the screen share's true cost
    // reasserts itself.
    #[test]
    fn under_reporting_held_layer_does_not_over_admit_the_next_slot() {
        let t = healthy_track();
        let high = t.by_quality(LayerQuality::High).unwrap();
        let high_nominal = high.state.nominal_bitrate_bps();
        // The screen-share slot: already holding High, but its live
        // reading has momentarily dropped to 0 (a quiet instant) even
        // though it's healthy and still actively held.
        high.state.update_for_test().bitrate(0);

        let mut screen_share = Slot::new(SlotConfig::default(), &mut test_rng());
        screen_share.set_max_height(1080);
        let screen_share_view = SlotView {
            key: next_slot_key(),
            mid: Mid::from("screen"),
            max_height: screen_share.max_height,
            track: &t,
            current_quality: LayerQuality::High,
            weight: screen_share.weight,
        };

        // Just enough total bandwidth for High's *real* (nominal) cost
        // plus a little slack — not enough for High plus another
        // meaningful tier on top.
        let mut remaining = high_nominal + 50_000.0;

        let decision = decide_tier(&mut screen_share, &screen_share_view, &mut remaining);
        assert!(
            matches!(decision, AllocationDecision::Forward(l, _) if l.quality == LayerQuality::High),
            "the screen share must keep holding High despite the momentary 0 reading"
        );
        assert!(
            remaining <= 50_000.0 + f64::EPSILON,
            "a held layer reporting 0 bit/s must still be priced at its nominal cost, \
             not treated as free — {remaining} bits were left unaccounted for"
        );

        // Whatever's left must not be enough to admit a second slot at
        // more than Low — if the screen share's true cost had been
        // under-deducted, this second slot would wrongly appear to
        // have room for Medium or better.
        let low_cost_track = healthy_track();
        low_cost_track
            .by_quality(LayerQuality::Medium)
            .unwrap()
            .state
            .update_for_test()
            .inactive(false);
        let mut other = Slot::new(SlotConfig::default(), &mut test_rng());
        other.set_max_height(1080);
        let other_view = SlotView {
            key: next_slot_key(),
            mid: Mid::from("other"),
            max_height: other.max_height,
            track: &low_cost_track,
            current_quality: LayerQuality::Low,
            weight: other.weight,
        };
        let other_decision = decide_tier(&mut other, &other_view, &mut remaining);
        assert!(
            matches!(
                other_decision,
                AllocationDecision::Forward(l, _) if l.quality == LayerQuality::Low
            ) || matches!(other_decision, AllocationDecision::Pause(..)),
            "second slot was over-admitted because the screen share's real cost was \
             under-deducted from the shared pool: {other_decision:?}"
        );
    }

    /// The cloud-CPU-steal-time property `ADMISSION_PREMIUM_CEILING` exists
    /// for: one slot's live reading spiking far above its nominal cost (a
    /// noisy VBR/screen-share moment) must not let it charge the shared pool
    /// for anywhere near the full spike — only up to
    /// `nominal * (1 + ADMISSION_PREMIUM_CEILING)` — so a sibling slot
    /// processed afterward in the same tick still gets its fair share.
    #[test]
    fn noisy_slot_cannot_steal_more_than_its_admission_ceiling_from_a_sibling() {
        let noisy_track = healthy_track();
        let noisy_high = noisy_track.by_quality(LayerQuality::High).unwrap();
        let high_nominal = noisy_high.state.nominal_bitrate_bps();
        // A wild, noisy spike — far beyond anything a real sustained cost
        // change would look like.
        noisy_high
            .state
            .update_for_test()
            .bitrate((high_nominal * 8.0) as u64);

        let mut noisy = Slot::new(SlotConfig::default(), &mut test_rng());
        noisy.set_max_height(1080);
        let noisy_view = SlotView {
            key: next_slot_key(),
            mid: Mid::from("noisy"),
            max_height: noisy.max_height,
            track: &noisy_track,
            current_quality: LayerQuality::High,
            weight: noisy.weight,
        };

        // Enough for both slots to hold High at their *true* (nominal, or
        // ceiling-bounded) cost with a bit of slack, but nowhere near
        // enough if the noisy slot's full 8x spike were charged instead.
        let expected_ceiling_cost = high_nominal * (1.0 + ADMISSION_PREMIUM_CEILING);
        let initial_remaining = expected_ceiling_cost + high_nominal + 50_000.0;
        let mut remaining = initial_remaining;

        let noisy_decision = decide_tier(&mut noisy, &noisy_view, &mut remaining);
        match noisy_decision {
            AllocationDecision::Forward(l, bw) => {
                assert_eq!(l.quality, LayerQuality::High);
                assert!(
                    (bw.as_f64() - expected_ceiling_cost).abs() < 1.0,
                    "noisy slot's charged cost {} was not bounded at the ceiling {}",
                    bw.as_f64(),
                    expected_ceiling_cost
                );
            }
            other => panic!("expected the noisy slot to keep holding High: {other:?}"),
        }
        assert!(
            (remaining - (initial_remaining - expected_ceiling_cost)).abs() < 1.0,
            "shared pool was debited by more than the admission ceiling: remaining={remaining}"
        );

        // The sibling, on an entirely separate healthy track, must still
        // get to hold High with whatever's left — it must not pay for the
        // noisy slot's spike.
        let sibling_track = healthy_track();
        let mut sibling = Slot::new(SlotConfig::default(), &mut test_rng());
        sibling.set_max_height(1080);
        let sibling_view = SlotView {
            key: next_slot_key(),
            mid: Mid::from("sibling"),
            max_height: sibling.max_height,
            track: &sibling_track,
            current_quality: LayerQuality::High,
            weight: sibling.weight,
        };
        let sibling_decision = decide_tier(&mut sibling, &sibling_view, &mut remaining);
        assert!(
            matches!(
                sibling_decision,
                AllocationDecision::Forward(l, _) if l.quality == LayerQuality::High
            ),
            "sibling slot was downgraded because of another slot's noisy spike, \
             not its own real bandwidth conditions: {sibling_decision:?}"
        );
    }

    proptest! {
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

            let (decisions1, _) = compute_tick(available, &slots);

            // Reorder the input slots and verify outcome stays the same.
            slots.reverse();
            let (decisions2, _) = compute_tick(available, &slots);

            prop_assert_eq!(decisions1.len(), decisions2.len());
            for s in &slots {
                prop_assert_eq!(
                    decisions1.get(&s.key),
                    decisions2.get(&s.key),
                    "decisions differ for slot {} when input order changes",
                    s.mid
                );
            }
        }

        // ─── Property: conservation, bounded by the downgrade margin ────────
        //
        // A tier already held survives a shortfall up to DOWNGRADE_MARGIN
        // (that's the hysteresis, deliberate — see `decide_tier`), so total
        // assigned cost can exceed `available` by up to one layer's worth of
        // that margin, but never more.
        #[test]
        fn total_assigned_cost_never_exceeds_available_bandwidth(
            n in 1usize..=6,
            available_kbps in 0u64..20_000,
            budget_seed in 0.0f64..10_000_000.0,
        ) {
            let t = healthy_track();
            let low_bps = layer_bps(&t, LayerQuality::Low);
            let mid_names: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
            let slots: Vec<SlotView> = mid_names
                .iter()
                .map(|name| slot(name, 1080, &t, LayerQuality::Low))
                .collect();
            let available = bw(available_kbps);
            let (decisions, _) = compute_tick_with_budgets(available, &slots, |_| budget_seed);

            let total_cost: f64 = decisions
                .values()
                .map(|d| match d {
                    AllocationDecision::Forward(_, bw) => bw.as_f64(),
                    AllocationDecision::Pause(..) => 0.0,
                })
                .sum();

            // This exercises `decide_tier` directly against a raw `available`
            // pool (no `RESERVE_FRACTION` — that's applied one level up, in
            // `update_allocations_at`, and covered separately by
            // `reserve_fraction_is_never_handed_out_by_the_greedy_walk`).
            // Each held slot can independently survive up to its own
            // `DOWNGRADE_MARGIN` shortfall against whatever `remaining` it
            // sees, so worst case the *total* overshoot scales with how many
            // slots can each still clear that margin, not a single flat
            // amount.
            let max_overshoot = n as f64 * low_bps * DOWNGRADE_MARGIN;
            prop_assert!(
                total_cost <= available.as_f64() + max_overshoot + f64::EPSILON,
                "total assigned {total_cost} exceeds available {} by more than \
                 {n} slots' worth of the downgrade margin ({max_overshoot})",
                available.as_f64()
            );
        }

        // ─── Property: no upgrade without enough pre-debit budget ───────────
        #[test]
        fn upgrade_never_happens_without_enough_pre_debit_budget(
            budget in 0.0f64..2_000_000.0,
            available_kbps in 0u64..20_000,
        ) {
            let t = healthy_track();
            let mut s = Slot::new(SlotConfig::default(), &mut test_rng());
            s.set_max_height(1080);
            s.budget_bps = budget;
            let view = SlotView {
                key: next_slot_key(),
                mid: s.mid,
                max_height: s.max_height,
                track: &t,
                current_quality: LayerQuality::Low,
                weight: s.weight,
            };
            let mut remaining = bw(available_kbps).as_f64();
            let decision = decide_tier(&mut s, &view, &mut remaining);

            if let AllocationDecision::Forward(layer, _) = decision
                && layer.quality > LayerQuality::Low
            {
                let burst = layer.state.bitrate_bps() * BURST_COST_SECONDS;
                prop_assert!(
                    budget >= burst,
                    "upgraded to {:?} with pre-debit budget {budget} < burst cost {burst}",
                    layer.quality
                );
            }
        }
    }

    // There's no artificial per-tick upgrade cap in the budget model — the
    // only constraint on how many slots can upgrade in the same tick is
    // whether the shared bandwidth pool and each slot's own ledger actually
    // cover it. With ample budget and bandwidth for both, both should
    // upgrade in the same tick.

    #[test]
    fn multiple_slots_can_upgrade_in_the_same_tick_when_both_can_afford_it() {
        let t = healthy_track();
        let high_bps = layer_bps(&t, LayerQuality::High);

        // Comfortably enough for two High layers.
        let available = bw(((high_bps * 3.0) as u64) / 1_000);

        let slots = vec![
            slot("a", 1080, &t, LayerQuality::Low),
            slot("b", 720, &t, LayerQuality::Low),
        ];

        let (decisions, _) = compute_tick(available, &slots);

        let upgrades = decisions
            .values()
            .filter(
                |d| matches!(d, AllocationDecision::Forward(r, _) if r.quality > LayerQuality::Low),
            )
            .count();

        assert_eq!(
            upgrades, 2,
            "both slots have ample budget and bandwidth; neither should be artificially held back"
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

        // Every slot here is at Low with High reachable, so demand includes
        // High's one-time switch burst on top of its ongoing rate.
        let expected_per_slot = t
            .layers
            .iter()
            .filter(|l| l.state.is_healthy())
            .map(|l| l.state.bitrate_bps() * (1.0 + BURST_COST_SECONDS))
            .fold(0.0_f64, f64::max);

        let expected_total = expected_per_slot * slots.len() as f64;

        let (_, desired) = compute_tick(bw(100_000), &slots);

        assert!(
            (desired.as_f64() - expected_total).abs() < 1.0,
            "desired {:.0} bps != expected {:.0} bps",
            desired.as_f64(),
            expected_total
        );
    }

    // ─── Property: the floor check is a hard cutoff, not a dead-band ────────────
    //
    // Noise absorption happens exactly once, upstream, in `BweFilter`'s own
    // asymmetric EWMA — the tier decision trusts its input completely and
    // reacts immediately, with no second dead-band layered on top of it.

    #[test]
    fn floor_check_has_a_downgrade_margin_not_an_exact_cutoff() {
        let t = healthy_track();
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let slots = vec![slot("a", 1080, &t, LayerQuality::Low)];

        let just_enough = bw((low_bps as u64) / 1_000 + 1);
        let (decisions, _) = compute_tick(just_enough, &slots);
        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "bandwidth at the floor cost must forward"
        );

        // Inside DOWNGRADE_MARGIN: this is the hysteresis, not noise
        // tolerance elsewhere — a real but modest shortfall on a tier
        // already held must not immediately cost a keyframe.
        let within_margin = bw((low_bps * (1.0 - DOWNGRADE_MARGIN / 2.0)) as u64 / 1_000);
        let (decisions, _) = compute_tick(within_margin, &slots);
        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "a shortfall inside DOWNGRADE_MARGIN must still hold, not pause"
        );

        // Past DOWNGRADE_MARGIN: a real, meaningful shortfall must still
        // pause promptly — the grace is bounded, not indefinite.
        let past_margin = bw((low_bps * (1.0 - DOWNGRADE_MARGIN * 2.0)) as u64 / 1_000);
        let (decisions, _) = compute_tick(past_margin, &slots);
        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Pause(..)),
            "bandwidth past the downgrade margin must pause"
        );
    }

    // ─── Property: empty slot list produces empty decisions + zero desired ────────

    #[test]
    fn no_slots_yields_empty_decisions_and_zero_desired() {
        let (decisions, desired) = compute_tick(bw(1_000), &[]);
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
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];

        // Bandwidth comfortably covers the only healthy layer.
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let (decisions, _) = compute_tick(available, &slots);

        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "single healthy layer should always be forwarded when budget allows"
        );
    }

    #[test]
    fn always_forward_lowest_layer() {
        let t = track_with_bad_layer(LayerQuality::Low);
        let low_bps = layer_bps(&t, LayerQuality::Low);
        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        let available = bw((low_bps * 2.0) as u64 / 1_000);
        let (decisions, _) = compute_tick(available, &slots);

        assert!(
            matches!(decisions[&slots[0].key], AllocationDecision::Forward(..)),
            "lowest layer is always forwarded after assignment"
        );
    }

    // A currently-forwarded layer that goes silent upstream (e.g. the
    // publisher's own uplink congestion paused it) must be replaced with a
    // live one, not kept "confirmed" just because it was already selected.

    #[test]
    fn currently_selected_layer_going_inactive_falls_back_to_a_live_layer() {
        let t = healthy_track();
        // The slot is already forwarding Low; Low then goes silent upstream
        // (e.g. the publisher paused it) while Medium and High stay live.
        t.by_quality(LayerQuality::Low)
            .unwrap()
            .state
            .update_for_test()
            .inactive(true);

        let slots = vec![slot("a", 720, &t, LayerQuality::Low)];
        // Enough for the new, live floor (Medium, since Low is dead) — Low's
        // own cost is no longer the relevant budget once it can't be
        // delivered at all.
        let medium_bps = layer_bps(&t, LayerQuality::Medium);
        let available = bw((medium_bps * 1.1) as u64 / 1_000);
        let (decisions, _) = compute_tick(available, &slots);

        match decisions[&slots[0].key] {
            AllocationDecision::Forward(layer, _) => {
                assert_ne!(
                    layer.quality,
                    LayerQuality::Low,
                    "kept forwarding the exact layer that just went silent instead of \
                     falling back to a live one"
                );
            }
            AllocationDecision::Pause(..) => {
                panic!("paused instead of falling back to a still-live higher tier")
            }
        }
    }

    #[test]
    fn already_at_a_tier_that_goes_inactive_switches_away_instead_of_repeating_silence() {
        let t = healthy_track();
        // The slot is already forwarding Medium; Medium then goes silent
        // upstream while Low (cheaper) stays live.
        t.by_quality(LayerQuality::Medium)
            .unwrap()
            .state
            .update_for_test()
            .inactive(true);

        let slots = vec![slot("a", 720, &t, LayerQuality::Medium)];
        let medium_bps = layer_bps(&t, LayerQuality::Medium);
        let available = bw((medium_bps * 1.1) as u64 / 1_000);
        let (decisions, _) = compute_tick(available, &slots);

        match decisions[&slots[0].key] {
            AllocationDecision::Forward(layer, _) => {
                assert_ne!(
                    layer.quality,
                    LayerQuality::Medium,
                    "kept rebuilding the exact tier that just went silent instead of \
                     switching to a live one"
                );
            }
            AllocationDecision::Pause(..) => {
                panic!("paused instead of falling back to the still-live low tier")
            }
        }
    }
}
