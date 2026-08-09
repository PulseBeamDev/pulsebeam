mod audio;
mod video;

use std::time::Duration;

use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::id::AudioSelectorSlotId;
use crate::log::LogCtx;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::MIN_BANDWIDTH;
use crate::participant::downstream::video::VideoAllocator;
use crate::participant::event::ParticipantSink;
use crate::rtp::RtpPacket;
use crate::track::{StreamWriter, Track, TrackLayer};
use pulsebeam_runtime::rand::RngCore;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, MediaTime, Mid, Pt, Rid};
use str0m::rtp::{SeqNo, Ssrc};
use tokio::time::Instant;
pub use video::{INITIAL_BANDWIDTH, Intent};

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
            pt: 100u8.into(),
            kind: MediaKind::Video,
        }
    }
}

/// How long everything may stay paused, while demand exists, before the estimate is reset.
/// Long enough to sit out subscription-change transients, short enough that a viewer does not
/// watch a frozen stream for long.
const STARVATION_TIMEOUT: Duration = Duration::from_secs(3);

const BWE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(150);
const BWE_FALL_TIME_CONSTANT: Duration = Duration::from_millis(800);

slotmap::new_key_type! {
    pub struct SlotKey;
}

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
        Bitrate::from(self.filtered_bps as u64)
    }
}

struct PlayoutDelayConfirm {
    mid: Mid,
    rid: Option<Rid>,
    seq: SeqNo,
}

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    pub video: VideoAllocator,
    audio: AudioAllocator,

    available_bandwidth: BweFilter,
    last_desired: Bitrate,
    /// When forwarding is below the minimum useful feedback rate while demand exists.
    starved_since: Option<Instant>,

    playout_delay: Option<(MediaTime, MediaTime)>,
    playout_delay_pending: bool,
    playout_delay_confirm: Option<PlayoutDelayConfirm>,
}

impl DownstreamAllocator {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool, rng: &mut impl RngCore) -> Self {
        Self {
            video: VideoAllocator::new(ctx, manual_sub, rng),
            audio: AudioAllocator::new(ctx),
            dirty_allocation: false,

            available_bandwidth: BweFilter::new(MIN_BANDWIDTH),
            last_desired: video::MIN_BANDWIDTH,
            starved_since: None,
            playout_delay: None,
            playout_delay_pending: false,
            playout_delay_confirm: None,
        }
    }

    pub fn set_playout_delay(&mut self, bounds: Option<(u32, u32)>) {
        const MAX_HUNDREDTHS: u64 = 0xfff;
        let to_hundredths = |ms: u32| ((ms as u64 + 5) / 10).min(MAX_HUNDREDTHS);
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

    pub fn add_track(&mut self, track: Track) {
        if track.meta.id.kind() == TrackKind::Video {
            self.video.add_track(track);
            self.dirty_allocation = true;
        }
        // Audio tracks need no static registration; slots are claimed dynamically.
    }

    pub(super) fn remove_track(&mut self, track_id: &TrackId) -> bool {
        let removed = self.video.remove_track(track_id);
        if removed {
            self.dirty_allocation = true;
        }
        removed
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

    pub fn update_bitrate(&mut self, now: Instant, available_bandwidth: Bitrate) {
        self.available_bandwidth.update(now, available_bandwidth);
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, now: Instant, bwe: &mut Bwe) -> bool {
        self.available_bandwidth.tick(now);
        self.dirty_allocation = false;
        let (desired, assignments_changed) = self
            .video
            .update_allocations(self.available_bandwidth.current());
        let allocated = self.video.current_allocation();
        bwe.set_current_bitrate(allocated);
        if self.last_desired != desired {
            bwe.set_desired_bitrate(desired);
            self.last_desired = desired;
        }
        self.break_starvation_deadlock(now, desired, allocated, bwe);
        assignments_changed
    }

    /// Escape the state where too little is forwarded to sustain useful feedback.
    ///
    /// Congestion control is a feedback loop with one open-loop failure: if every slot is paused
    /// there is no media, so no transport feedback, so the estimate cannot move off the value
    /// that caused the pause - which is by definition too low to un-pause anything. Resetting it
    /// to what the application wants restores the loop; the ordinary machinery then re-measures
    /// the real link within a second or two, and backs off again if it genuinely cannot carry it.
    ///
    fn break_starvation_deadlock(
        &mut self,
        now: Instant,
        desired: Bitrate,
        allocated: Bitrate,
        bwe: &mut Bwe,
    ) {
        debug_assert!(allocated <= desired || desired == Bitrate::ZERO);
        if desired == Bitrate::ZERO || allocated > Bitrate::ZERO {
            self.starved_since = None;
            return;
        }
        let since = *self.starved_since.get_or_insert(now);
        if now.saturating_duration_since(since) < STARVATION_TIMEOUT {
            return;
        }
        self.starved_since = Some(now);
        bwe.reset(desired);
        self.available_bandwidth = BweFilter::new(desired);
    }

    pub fn reconcile_routes(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
    }

    pub fn poll_slow(
        &mut self,
        now: Instant,
        bwe: &mut Bwe,
        events: &mut impl ParticipantSink,
    ) -> bool {
        let assignments_changed = self.update_allocations(now, bwe);
        self.video
            .poll_slow(now, self.available_bandwidth.current(), events);
        assignments_changed
    }

    #[inline]
    pub fn update_layer_states(&mut self, track_id: TrackId, states: &crate::track::TrackStates) {
        self.video.update_layer_states(track_id, states);
    }

    pub fn on_forward_rtp(
        &mut self,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp(track_id, pkt, cache, writer)
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.audio.on_rtp(slot_idx, pkt, writer);
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) -> Option<&TrackLayer> {
        self.video.handle_keyframe_request(req)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core and a fixture may read the host clock.
    // See docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
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

    #[test]
    fn bwe_filter_rise_uses_each_update_interval_once() {
        let start = Instant::now();
        let initial = 300_000.0;
        let target = 1_300_000.0;
        let mut filter = BweFilter::new(Bitrate::from(initial as u64));

        filter.update(start, Bitrate::from(initial as u64));
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(target as u64),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(target as u64),
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
        let mut filter = BweFilter::new(Bitrate::from(initial as u64));

        filter.update(start, Bitrate::from(initial as u64));
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(target as u64),
        );
        filter.update(
            start + Duration::from_millis(200),
            Bitrate::from(target as u64),
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
        let mut filter = BweFilter::new(Bitrate::from(initial as u64));

        filter.update(start, Bitrate::from(initial as u64));
        filter.update(
            start + Duration::from_millis(100),
            Bitrate::from(target as u64),
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
