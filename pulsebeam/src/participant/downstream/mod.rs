mod audio;
mod data;
mod video;

use std::time::Duration;

use crate::entity::AudioOrigin;
use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::keys::TrackKey;
use crate::log::LogCtx;
use crate::participant::allocation::{AllocationInput, AllocationOutput, Bitrate};
use crate::participant::downstream::video::START_BANDWIDTH;
use crate::participant::event::ParticipantSink;
pub use crate::participant::intent::AudioIntent;
use crate::rtp::{
    EncodingId as Rid, KeyframeRequest, MediaKind, MediaSectionId as Mid, MediaTime,
    PayloadType as Pt, RtpPacket, SequenceNumber as SeqNo, Ssrc,
};
use crate::track::{StreamWriter, Track, TrackLayer, TrackMeta};
use ahash::HashSetExt;
pub use audio::DownstreamAudio;
pub(crate) use data::DownstreamData;
use indexmap::IndexMap;
use slotmap::SecondaryMap;
use tokio::time::Instant;
pub use video::{DownstreamVideo, INITIAL_BANDWIDTH};

#[derive(Clone)]
pub struct SlotConfig {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub ssrc: Ssrc,
    pub pt: Pt,
    pub kind: MediaKind,
}

impl Default for SlotConfig {
    fn default() -> Self {
        Self {
            mid: Mid::from("0"),
            rid: None,
            ssrc: 0u32.into(),
            pt: Pt::DEFAULT,
            kind: MediaKind::Video,
        }
    }
}

/// How long everything may stay paused, while demand exists, before the estimate is reset.
/// Long enough to sit out subscription-change transients, short enough that a viewer does not
/// watch a frozen stream for long.
const STARVATION_TIMEOUT: Duration = Duration::from_secs(3);

/// How much the estimate must move for feedback to count as flowing.
///
/// Small, because the question is whether the estimate is responding at all, not whether it is
/// responding well. The filter drifts slightly on its own between updates, so exact equality
/// would call a genuinely frozen estimate "moving".
const ESTIMATE_MOVED_SHARE: f64 = 0.02;

/// The share of the estimate that must be in use for the link to count as measured.
///
/// Paired with "something could not be placed" — see [`starvation_reset_target`]. Deliberately far
/// from 1.0: an allocator may spend well under its estimate for good reasons, and that must not
/// read as starvation on its own.
const MEASURED_SHARE_OF_ESTIMATE: f64 = 0.25;

const ALLOCATION_RESERVE_FRACTION: f64 = 0.10;

/// A starvation episode being timed, and the estimate it started from.
#[derive(Clone, Copy, Debug)]
struct StarvationWatch {
    since: Instant,
    estimate: Bitrate,
}

/// Whether the feedback loop has fallen open, and what to restart it at.
///
/// Congestion control is a feedback loop with one open-loop failure: transport feedback only
/// describes traffic that was actually sent, so when far too little is being forwarded the
/// estimate cannot move off the value that caused the pause - which is by definition too low to
/// un-pause anything. Resetting it to what the application wants restores the loop; the ordinary
/// machinery re-measures the real link within a second or two.
///
/// Two conditions have to hold together, and getting either one alone wrong is expensive:
///
/// - **Something could not be placed, *and* the link is idle relative to the estimate.** Both
///   halves are needed and neither works alone. "Something could not be placed" on its own fires
///   whenever an allocator backgrounds a stream on purpose. A rate threshold on its own cannot
///   separate a real deadlock at 150 kbps of a 997 kbps estimate (15 %) from a healthy allocator
///   underfilling at 473 kbps of 2.74 Mbps (17 %) — and an absolute floor is worse still, since a
///   single minimum-rate layer sits exactly on it. Together they say the thing that matters: the
///   allocator is holding headroom it cannot spend, because the cheapest rung of what it wants
///   costs more than what it is told it has.
/// - **The estimate is not moving.** A moving estimate means feedback is arriving and the loop is
///   closed, whatever the allocator is doing. Without this, ramp-up and priority reconfiguration
///   both look like starvation - and resetting there pins the estimate above what the link can
///   carry, so the allocator overshoots and the stream reverses a layer coming back down.
///
/// A converged controller fails the first test, because it runs *at* its estimate. A
/// reconfiguring one fails the second.
fn starvation_reset_target(
    watch: &mut Option<StarvationWatch>,
    now: Instant,
    desired: Bitrate,
    unfunded: Option<Bitrate>,
    allocated: Bitrate,
    estimate: Bitrate,
) -> Option<Bitrate> {
    // Headroom the allocator believes it has and is not using.
    let idle = allocated.as_f64() < estimate.as_f64() * MEASURED_SHARE_OF_ESTIMATE;
    if desired == Bitrate::ZERO || desired <= estimate || unfunded.is_none() || !idle {
        *watch = None;
        return None;
    }

    let started = *watch.get_or_insert(StarvationWatch {
        since: now,
        estimate,
    });

    let moved = estimate.as_f64() > started.estimate.as_f64() * (1.0 + ESTIMATE_MOVED_SHARE);
    if moved {
        *watch = Some(StarvationWatch {
            since: now,
            estimate,
        });
        return None;
    }
    if now.saturating_duration_since(started.since) < STARVATION_TIMEOUT {
        return None;
    }
    // Probe at what would actually unblock, not at what the application wants.
    // Resetting to full demand pins the estimate above what the link can carry,
    // so the allocator overshoots and the stream reverses a layer coming back
    // down — on a genuinely small link that is a permanent oscillation rather
    // than a transient. The cheapest unfunded layer is the smallest step that
    // can restart feedback: if it fits, the estimator takes over from there; if
    // it does not, the link truly cannot carry it and pausing was right.
    // Never below the current estimate. This exists to escape an estimate that
    // is stuck too low; resetting downward would deepen exactly the condition
    // it is meant to break. `unfunded` is a layer's cost, and another slot may
    // already have spent most of the budget, so it is not bounded below by the
    // estimate on its own.
    let probe = unfunded.unwrap_or(desired).as_f64() / (1.0 - ALLOCATION_RESERVE_FRACTION);
    let target = Bitrate::from(crate::bitrate::saturating_bps(probe))
        .min(desired)
        .max(estimate);
    *watch = Some(StarvationWatch {
        since: now,
        estimate: target,
    });
    Some(target)
}

const BWE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(150);
const BWE_FALL_TIME_CONSTANT: Duration = Duration::from_millis(800);

#[derive(Debug)]
struct BweFilter {
    filtered_bps: f64,
    target_bps: f64,
    last_update: Option<Instant>,
}

impl BweFilter {
    fn new(initial: Bitrate) -> Self {
        Self {
            filtered_bps: initial.as_f64(),
            target_bps: initial.as_f64(),
            last_update: None,
        }
    }

    fn tick(&mut self, now: Instant) {
        self.advance(now, self.target_bps);
    }

    fn update(&mut self, now: Instant, raw: Bitrate) {
        self.target_bps = raw.as_f64();
        self.advance(now, self.target_bps);
    }

    fn advance(&mut self, now: Instant, target_bps: f64) {
        debug_assert!(target_bps.is_finite());
        debug_assert!(target_bps >= 0.0);
        let Some(last_update) = self.last_update else {
            self.last_update = Some(now);
            self.filtered_bps = target_bps;
            return;
        };

        debug_assert!(now >= last_update);
        let elapsed = now.saturating_duration_since(last_update);
        self.last_update = Some(now);
        if target_bps >= self.filtered_bps {
            let alpha = (-elapsed.as_secs_f64() / BWE_RISE_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = target_bps + (self.filtered_bps - target_bps) * alpha;
        } else {
            let alpha = (-elapsed.as_secs_f64() / BWE_FALL_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = target_bps + (self.filtered_bps - target_bps) * alpha;
        }
        debug_assert!(self.filtered_bps.is_finite());
        debug_assert!(self.filtered_bps >= 0.0);
    }

    fn current(&self) -> Bitrate {
        Bitrate::from(crate::bitrate::saturating_bps(self.filtered_bps))
    }
}

struct PlayoutDelayConfirm {
    mid: Mid,
    rid: Option<Rid>,
    seq: SeqNo,
}

pub struct Downstream {
    pub dirty_allocation: bool,
    pub video: DownstreamVideo,
    pub(crate) audio: DownstreamAudio,
    pub(crate) data: DownstreamData,
    catalog: SecondaryMap<TrackKey, TrackCatalogEntry>,
    /// Audio publications in the room, whether or not anyone is hearing them.
    ///
    /// The allocator claims slots dynamically and needs no registration to do
    /// it, but the roster does: a client cannot pin a speaker it was never told
    /// exists, and until someone is loud enough to be forwarded there is
    /// otherwise nothing that names them.
    audio_tracks: IndexMap<TrackId, TrackMeta>,

    available_bandwidth: BweFilter,
    application_limited: bool,
    last_desired: Bitrate,
    /// When forwarding fell below the rate needed to keep measuring the link, while demand
    /// exists, and the estimate it was at then.
    starved_since: Option<StarvationWatch>,

    playout_delay: Option<(MediaTime, MediaTime)>,
    playout_delay_pending: bool,
    playout_delay_confirm: Option<PlayoutDelayConfirm>,
}

#[derive(Clone, Copy)]
pub(crate) struct TrackCatalogEntry {
    pub(crate) participant_id: ParticipantId,
    pub(crate) track_id: TrackId,
}

pub type DownstreamAllocator = Downstream;

impl Downstream {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool) -> Self {
        Self {
            video: DownstreamVideo::new(ctx, manual_sub),
            audio: DownstreamAudio::new(ctx, manual_sub),
            data: DownstreamData::new(),
            catalog: SecondaryMap::new(),
            audio_tracks: IndexMap::new(),
            dirty_allocation: false,

            available_bandwidth: BweFilter::new(START_BANDWIDTH),
            application_limited: false,
            last_desired: video::START_BANDWIDTH,
            starved_since: None,
            playout_delay: None,
            playout_delay_pending: false,
            playout_delay_confirm: None,
        }
    }

    pub(crate) fn add_track_candidate(
        &mut self,
        key: TrackKey,
        track: &Track,
        channels: &[(pulsebeam_rtc::ChannelId, crate::track::DataTopicChannel)],
    ) -> bool {
        let entry = TrackCatalogEntry {
            participant_id: track.meta().origin,
            track_id: track.id(),
        };
        if let Some(previous) = self.catalog.get(key) {
            debug_assert_eq!(previous.track_id, entry.track_id);
            debug_assert_eq!(previous.participant_id, entry.participant_id);
            return false;
        }
        let previous = self.catalog.insert(key, entry);
        debug_assert!(previous.is_none(), "a TrackKey must be installed once");
        self.data.add_candidate(key, track, channels);
        true
    }

    pub(crate) fn remove_track_candidate(&mut self, key: TrackKey) -> Option<TrackCatalogEntry> {
        self.data.remove_candidate(key);
        self.catalog.remove(key)
    }

    pub(crate) fn track_candidate(&self, key: TrackKey) -> Option<TrackCatalogEntry> {
        self.catalog.get(key).copied()
    }

    pub fn set_playout_delay(&mut self, bounds: Option<(u32, u32)>) {
        const MAX_HUNDREDTHS: u64 = 0xfff;
        let to_hundredths = |ms: u32| ((ms as u64).saturating_add(5) / 10).min(MAX_HUNDREDTHS);
        let Some(bounds) = bounds else {
            return;
        };
        let max = to_hundredths(bounds.1);
        let min = to_hundredths(bounds.0).min(max);
        let delay = (
            MediaTime::from_hundredths(min),
            MediaTime::from_hundredths(max),
        );
        if self.playout_delay == Some(delay) {
            return;
        }
        self.playout_delay = Some(delay);
        self.playout_delay_pending = true;
        self.playout_delay_confirm = None;
    }

    /// Returns the playout delay to stamp if the receiver has not yet confirmed
    /// receipt. Returns `None` once confirmed — extension is sticky so no need
    /// to keep sending unchanged values.
    #[inline]
    pub fn playout_delay_to_stamp(&self) -> Option<(MediaTime, MediaTime)> {
        if self.playout_delay_pending {
            self.playout_delay
        } else {
            None
        }
    }

    /// Record that a packet with the current playout delay values was stamped.
    /// Tracks the first such packet per change for RTCP confirmation.
    pub fn record_playout_delay_stamp(&mut self, mid: Mid, rid: Option<Rid>, seq: SeqNo) {
        if self.playout_delay_confirm.is_none() {
            self.playout_delay_confirm = Some(PlayoutDelayConfirm { mid, rid, seq });
        }
    }

    /// Called when RTCP receiver report stats arrive for a stream. Clears the
    /// pending flag once the remote has acknowledged receipt past our tracked seq.
    pub fn handle_egress_stats(&mut self, mid: Mid, rid: Option<Rid>, remote_max_seq: SeqNo) {
        let Some(confirm) = &self.playout_delay_confirm else {
            return;
        };
        if confirm.mid == mid && confirm.rid == rid && remote_max_seq >= confirm.seq {
            self.playout_delay_pending = false;
            self.playout_delay_confirm = None;
        }
    }

    pub(crate) fn install_track(&mut self, key: TrackKey, track: Track) {
        if track.kind() == TrackKind::Video {
            self.video.install_track(key, track);
            self.dirty_allocation = true;
            return;
        }
        // The allocator claims audio slots dynamically, so this registration is
        // purely so the roster can name the track before anyone hears it.
        let id = track.id();
        self.audio_tracks.insert(id, track.meta().clone());
    }

    pub(crate) fn activate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.activate_track_binding(key, track_id);
        }
    }

    pub(crate) fn deactivate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        if track_id.kind() == TrackKind::Video {
            self.video.deactivate_track_binding(key, track_id);
        }
    }

    /// Apply the client's audio selection policy.
    pub fn set_audio_intent(&mut self, intent: AudioIntent) {
        self.audio.set_intent(intent);
    }

    pub(crate) fn apply_signaling_intents(
        &mut self,
        intents: crate::participant::signaling::SignalingIntents,
    ) {
        if let Some(video) = intents.video {
            self.video.configure(&video);
        }
        if let Some(audio) = intents.audio {
            self.set_audio_intent(audio);
        }
        if intents.playout_delay.is_some() {
            self.set_playout_delay(intents.playout_delay);
        }
        self.dirty_allocation = true;
    }

    pub(crate) fn signaling_snapshot(&self) -> crate::participant::signaling::SignalingSnapshot {
        use crate::participant::signaling::{
            SignalingAudioBinding, SignalingSnapshot, SignalingVideoBinding,
        };
        SignalingSnapshot {
            publications: self
                .video
                .tracks()
                .chain(self.audio_tracks.values())
                .cloned()
                .collect(),
            participants: ahash::HashSet::new(),
            video: self
                .video
                .slots()
                .map(|slot| SignalingVideoBinding {
                    mid: slot.mid.to_string(),
                    track_id: slot.track.id.as_str(),
                    paused: slot.paused,
                })
                .collect(),
            audio: self
                .audio_assignments()
                .iter()
                .map(|heard| SignalingAudioBinding {
                    mid: heard.mid.to_string(),
                    track_id: heard.origin.track.as_str(),
                    level_dbov: i32::from(heard.level_dbov),
                })
                .collect(),
        }
    }

    pub fn audio_slot_count(&self) -> usize {
        self.audio.slot_count()
    }

    pub(super) fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let removed = self.video.remove_track(track_id);
        if removed {
            self.dirty_allocation = true;
        }
        // Audio too, and not folded into `removed`: that flag drives the *video* allocator's
        // rebalance. A speaker leaving still has to stop being announced, or the room keeps a
        // tile for somebody who is not in it.
        let audio_removed = self.audio.remove_track(track_id);
        let announced = self.audio_tracks.shift_remove(track_id).is_some();
        removed || audio_removed || announced
    }

    pub fn add_slot(&mut self, slot: SlotConfig) {
        match slot.kind {
            MediaKind::Video => {
                self.video.add_slot(slot);
            }
            MediaKind::Audio => {
                self.audio.add_slot(slot);
            }
        }
        self.dirty_allocation = true;
    }

    pub fn has_slot(&self, kind: MediaKind, mid: Mid) -> bool {
        match kind {
            MediaKind::Video => self.video.has_slot(mid),
            MediaKind::Audio => self.audio.has_slot(mid),
        }
    }

    pub fn refresh_ssrc(
        &mut self,
        kind: MediaKind,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
    ) -> bool {
        match kind {
            MediaKind::Video => self.video.refresh_ssrc(mid, rid, ssrc),
            MediaKind::Audio => {
                debug_assert!(rid.is_none());
                self.audio.refresh_ssrc(mid, ssrc)
            }
        }
    }

    pub fn update_allocation_input(&mut self, now: Instant, input: AllocationInput) {
        self.available_bandwidth.update(now, input.estimate);
        self.application_limited = input.application_limited;
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, now: Instant) -> (bool, AllocationOutput) {
        self.available_bandwidth.tick(now);
        self.dirty_allocation = false;
        let (desired, assignments_changed, unfunded) = self
            .video
            .update_allocations(self.available_bandwidth.current());
        let allocated = self.video.current_allocation();
        let recovery = self.break_starvation_deadlock(now, desired, unfunded, allocated);
        self.last_desired = desired;
        (
            assignments_changed,
            AllocationOutput {
                desired,
                allocated,
                recovery,
                probe: self.application_limited.then_some(recovery).flatten(),
            },
        )
    }

    /// Escape the state where too little is forwarded to sustain useful feedback.
    ///
    /// Applies whatever [`starvation_reset_target`] decides; the decision itself is separated out
    /// so it can be tested against the shapes that distinguish a deadlock from a controller
    /// merely working.
    fn break_starvation_deadlock(
        &mut self,
        now: Instant,
        desired: Bitrate,
        unfunded: Option<Bitrate>,
        allocated: Bitrate,
    ) -> Option<Bitrate> {
        let estimate = self.available_bandwidth.current();
        let target = starvation_reset_target(
            &mut self.starved_since,
            now,
            desired,
            unfunded,
            allocated,
            estimate,
        )?;
        self.available_bandwidth = BweFilter::new(target);
        Some(target)
    }

    pub(crate) fn reconcile_routes(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
    }

    pub(crate) fn poll_slow(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> (bool, AllocationOutput) {
        let (assignments_changed, output) = self.update_allocations(now);
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
        (assignments_changed, output)
    }

    pub fn on_forward_rtp(
        &mut self,
        track_key: TrackKey,
        arrival_ts: Instant,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp(track_key, arrival_ts, cache, writer)
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        origin: AudioOrigin,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.audio.on_rtp(origin, pkt, writer);
    }

    /// Whether someone new took over an audio slot since this was last asked.
    pub fn take_audio_speakers_changed(&mut self) -> bool {
        self.audio.take_speakers_changed()
    }

    /// Who this subscriber is currently hearing, loudest first.
    pub fn audio_assignments(&self) -> Vec<crate::participant::downstream::audio::Heard> {
        self.audio.assignments()
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) -> Option<&TrackLayer> {
        self.video.handle_keyframe_request(req)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn expected(initial: f64, target: f64, elapsed: Duration, time_constant: Duration) -> f64 {
        let alpha = (-elapsed.as_secs_f64() / time_constant.as_secs_f64()).exp();
        target + (initial - target) * alpha
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 2.0,
            "actual={actual}, expected={expected}"
        );
    }

    /// A stream too big to divide leaves the link idle, and the estimate cannot recover on its own.
    ///
    /// The shape that motivated this: a 2.5 Mbps single-layer screen share does not fit a 997 kbps
    /// estimate and has no lower rung, so it is dropped whole, leaving a 150 kbps camera trickling
    /// on a 3 Mbps link. Feedback describes the traffic that was sent, so the estimate stays where
    /// it is - measured at 997190 bps for 900 consecutive samples - and the allocator can never
    /// afford to un-pause the thing that would prove the link is fine.
    #[test]
    fn an_idle_link_with_a_frozen_estimate_is_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let (desired, estimate) = (bps(3_000_000), bps(997_190));

        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now,
                desired,
                Some(desired),
                bps(150_000),
                estimate
            ),
            None,
            "the deadlock must be held for {STARVATION_TIMEOUT:?} before it counts"
        );
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                Some(desired),
                bps(150_000),
                estimate
            ),
            Some(desired),
            "an idle link whose estimate has not moved has no way back without a reset"
        );
    }

    /// Everything paused is the extreme of the same condition, and must still be caught.
    #[test]
    fn a_fully_paused_allocation_is_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let (desired, estimate) = (bps(3_000_000), bps(300_000));
        let _ = starvation_reset_target(
            &mut watch,
            now,
            desired,
            Some(desired),
            Bitrate::ZERO,
            estimate,
        );
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                Some(desired),
                Bitrate::ZERO,
                estimate
            ),
            Some(desired)
        );
    }

    /// A moving estimate means feedback is arriving, so nothing is stuck.
    ///
    /// This is what the first attempt at the predicate got wrong. Running below the estimate is
    /// ordinary during ramp-up and during a priority reconfiguration; resetting there pins the
    /// estimate above what the link can carry, the allocator overshoots, and the stream reverses a
    /// layer on the way back down - turning a healthy transient into the oscillation that
    /// `priority_reconfiguration_quality_churn_test` forbids.
    #[test]
    fn an_estimate_that_is_still_moving_is_not_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let desired = bps(4_600_000);

        let _ = starvation_reset_target(
            &mut watch,
            now,
            desired,
            Some(desired),
            bps(100_000),
            bps(1_000_000),
        );
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                Some(desired),
                bps(100_000),
                bps(1_400_000)
            ),
            None,
            "the estimate climbed 40%, so feedback is flowing and the loop is closed"
        );
    }

    /// An allocator that is forwarding *something* is not deadlocked, however
    /// little that is.
    ///
    /// This is the case no bitrate threshold can express. By rate it is
    /// indistinguishable from the real deadlock above — 473 kbps of a 2.74 Mbps
    /// estimate is 17 %, the deadlock is 150 kbps of 997 kbps, 15 % — and an
    /// absolute floor fares no better, since one minimum-rate layer sits
    /// exactly on it. The allocator afforded a layer, so it is spending by
    /// choice and the loop is closed.
    #[test]
    fn an_underfilled_live_allocation_is_not_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let desired = bps(4_600_000);
        let estimate = bps(2_741_782);

        let _ = starvation_reset_target(&mut watch, now, desired, None, bps(2_900_000), estimate);
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                None,
                bps(2_900_000),
                estimate
            ),
            None,
            "an allocation that fit is not a deadlock, whatever share of the estimate it uses"
        );
    }

    /// A controller using the bandwidth it believes in is behaving correctly, however static the
    /// estimate looks. Convergence is the goal, not a symptom.
    #[test]
    fn a_converged_controller_is_not_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let (desired, estimate) = (bps(4_000_000), bps(3_000_000));

        let _ = starvation_reset_target(&mut watch, now, desired, None, bps(2_900_000), estimate);
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                None,
                bps(2_900_000),
                estimate
            ),
            None,
            "the link is in use at its estimate; there is nothing to escape"
        );
    }

    /// An allocator spending well under its estimate on purpose is not deadlocked.
    ///
    /// `priority_reconfiguration_quality_churn_test` backgrounds a screen share, so the viewer
    /// runs at roughly 44% of its estimate with plenty of feedback flowing. Treating that as
    /// starvation resets the estimate above the link's real capacity, and the overshoot costs the
    /// backgrounded stream a layer reversal on the way back down - the exact churn that plan
    /// forbids.
    #[test]
    fn an_allocator_underspending_on_purpose_is_not_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let (desired, estimate) = (bps(4_200_000), bps(2_838_598));

        let _ = starvation_reset_target(&mut watch, now, desired, None, bps(2_900_000), estimate);
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                None,
                bps(2_900_000),
                estimate
            ),
            None,
            "an allocation that fit is a link in use, not one that has gone quiet"
        );
    }

    /// The reset raises a stuck estimate; it must never lower one.
    ///
    /// `unfunded` is one layer's cost, not a floor — another slot may already
    /// have spent most of the budget, so the cheapest thing that did not fit
    /// can be cheaper than the estimate itself. Resetting to it would deepen
    /// the stall this exists to break.
    #[tokio::test(start_paused = true)]
    async fn a_reset_never_lowers_the_estimate() {
        let mut watch = None;
        let now = Instant::now();
        let desired = bps(4_000_000);
        let estimate = bps(1_000_000);
        let cheap = bps(120_000);

        let _ =
            starvation_reset_target(&mut watch, now, desired, Some(cheap), bps(10_000), estimate);
        let target = starvation_reset_target(
            &mut watch,
            now + STARVATION_TIMEOUT,
            desired,
            Some(cheap),
            bps(10_000),
            estimate,
        );
        assert_eq!(
            target,
            Some(estimate),
            "a probe cheaper than the estimate must not drag the estimate down to it"
        );
    }

    /// Demand within the estimate is not starvation whatever the allocator is doing with it.
    #[test]
    fn demand_the_estimate_already_covers_is_not_a_deadlock() {
        let mut watch = None;
        let now = Instant::now();
        let (desired, estimate) = (bps(800_000), bps(3_000_000));
        let _ = starvation_reset_target(
            &mut watch,
            now,
            desired,
            Some(desired),
            bps(150_000),
            estimate,
        );
        assert_eq!(
            starvation_reset_target(
                &mut watch,
                now + STARVATION_TIMEOUT,
                desired,
                Some(desired),
                bps(150_000),
                estimate
            ),
            None
        );
    }

    fn bps(v: u64) -> Bitrate {
        Bitrate::from(v)
    }

    #[test]
    fn bwe_filter_rise_uses_each_update_interval_once() {
        let start = Instant::now();
        let initial = 300_000.0;
        let target = 1_300_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(200),
                BWE_RISE_TIME_CONSTANT,
            ),
        );
    }

    #[test]
    fn bwe_filter_fall_uses_each_update_interval_once() {
        let start = Instant::now();
        let initial = 2_000_000.0;
        let target = 300_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(200),
                BWE_FALL_TIME_CONSTANT,
            ),
        );
    }

    #[test]
    fn bwe_filter_ticks_converge_to_the_latest_sample() {
        let start = Instant::now();
        let initial = 300_000.0;
        let target = 4_200_000.0;
        let mut filter = BweFilter::new(Bitrate::from(crate::bitrate::saturating_bps(initial)));

        filter.update(
            start,
            Bitrate::from(crate::bitrate::saturating_bps(initial)),
        );
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(crate::bitrate::saturating_bps(target)),
        );
        for elapsed_ms in (200..=1_000).step_by(100) {
            filter.tick(start + Duration::from_millis(elapsed_ms));
        }

        assert_close(
            filter.current().as_f64(),
            expected(
                initial,
                target,
                Duration::from_millis(1_000),
                BWE_RISE_TIME_CONSTANT,
            ),
        );
    }
}
