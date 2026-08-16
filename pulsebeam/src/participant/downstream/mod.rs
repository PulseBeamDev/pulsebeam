mod audio;
mod video;

use std::time::Duration;

use crate::entity::AudioOrigin;
use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::id::AudioSelectorSlotId;
use crate::log::LogCtx;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::START_BANDWIDTH;
use crate::participant::downstream::video::VideoAllocator;
use crate::participant::event::ParticipantSink;
use crate::rtp::RtpPacket;
use crate::track::{StreamWriter, Track, TrackLayer};
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

const HOLD_RISE: Duration = Duration::from_millis(150);
const HOLD_FALL: Duration = Duration::from_millis(800);
const CLIMB_RISE: Duration = Duration::from_millis(2_000);

#[derive(Debug)]
struct BweBand {
    hold_bps: f64,
    climb_bps: f64,
    target_bps: f64,
    last_update: Option<Instant>,
}

impl BweBand {
    fn new(initial: Bitrate) -> Self {
        Self {
            hold_bps: initial.as_f64(),
            climb_bps: initial.as_f64(),
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
            self.hold_bps = target_bps;
            self.climb_bps = target_bps;
            return;
        };

        debug_assert!(now >= last_update);
        let elapsed = now.saturating_duration_since(last_update);
        self.last_update = Some(now);

        let hold_tau = if target_bps >= self.hold_bps {
            HOLD_RISE
        } else {
            HOLD_FALL
        };
        let hold_alpha = (-elapsed.as_secs_f64() / hold_tau.as_secs_f64()).exp();
        self.hold_bps = target_bps + (self.hold_bps - target_bps) * hold_alpha;

        self.climb_bps = if target_bps <= self.climb_bps {
            target_bps
        } else {
            let climb_alpha = (-elapsed.as_secs_f64() / CLIMB_RISE.as_secs_f64()).exp();
            target_bps + (self.climb_bps - target_bps) * climb_alpha
        };

        debug_assert!(self.hold_bps.is_finite());
        debug_assert!(self.climb_bps.is_finite());
        debug_assert!(self.climb_bps >= 0.0);
        debug_assert!(self.climb_bps <= self.hold_bps + 1.0);
    }

    fn hold(&self) -> Bitrate {
        Bitrate::from(crate::bitrate::saturating_bps(self.hold_bps))
    }

    fn climb(&self) -> Bitrate {
        Bitrate::from(crate::bitrate::saturating_bps(self.climb_bps))
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

    available_bandwidth: BweBand,
    last_desired: Bitrate,

    playout_delay: Option<(MediaTime, MediaTime)>,
    playout_delay_pending: bool,
    playout_delay_confirm: Option<PlayoutDelayConfirm>,
}

impl DownstreamAllocator {
    pub(crate) fn new(ctx: LogCtx, manual_sub: bool) -> Self {
        Self {
            video: VideoAllocator::new(ctx, manual_sub),
            audio: AudioAllocator::new(ctx),
            dirty_allocation: false,

            available_bandwidth: BweBand::new(START_BANDWIDTH),
            last_desired: video::START_BANDWIDTH,
            playout_delay: None,
            playout_delay_pending: false,
            playout_delay_confirm: None,
        }
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
        // Audio too, and not folded into `removed`: that flag drives the *video* allocator's
        // rebalance. A speaker leaving still has to stop being announced, or the room keeps a
        // tile for somebody who is not in it.
        let audio_removed = self.audio.remove_track(track_id);
        removed || audio_removed
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
        let (desired, assignments_changed, _) = self.video.update_allocations(
            self.available_bandwidth.hold(),
            self.available_bandwidth.climb(),
        );
        if self.last_desired != desired {
            bwe.set_desired_bitrate(desired);
            self.last_desired = desired;
        }
        assignments_changed
    }

    pub(crate) fn reconcile_routes(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
        fanouts: &ahash::HashMap<TrackId, crate::shard::router::TrackKey>,
    ) {
        self.video
            .poll_slow(now, self.available_bandwidth.hold(), events, fanouts);
    }

    pub(crate) fn poll_slow(
        &mut self,
        now: Instant,
        bwe: &mut Bwe,
        events: &mut impl ParticipantSink,
        fanouts: &ahash::HashMap<TrackId, crate::shard::router::TrackKey>,
    ) -> bool {
        let assignments_changed = self.update_allocations(now, bwe);
        self.video
            .poll_slow(now, self.available_bandwidth.hold(), events, fanouts);
        assignments_changed
    }

    #[inline]
    pub fn update_layer_states(&mut self, track_id: TrackId, states: &crate::track::TrackStates) {
        self.video.update_layer_states(track_id, states);
    }

    pub fn update_layer_states_slot(
        &mut self,
        slot: crate::keys::DownstreamSlotKey,
        states: &crate::track::TrackStates,
    ) {
        self.video.update_layer_states_slot(slot, states);
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

    pub fn on_forward_rtp_slot(
        &mut self,
        slot: crate::keys::DownstreamSlotKey,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp_slot(slot, pkt, cache, writer)
    }

    /// Forward an audio packet through the per-subscriber slot gate.
    #[inline]
    pub fn on_forward_audio_rtp(
        &mut self,
        slot_idx: AudioSelectorSlotId,
        origin: AudioOrigin,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        self.audio.on_rtp(slot_idx, origin, pkt, writer);
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
    use super::*;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 2.0,
            "actual={actual}, expected={expected}"
        );
    }

    fn expected(initial: f64, target: f64, elapsed: Duration, tau: Duration) -> f64 {
        let alpha = (-elapsed.as_secs_f64() / tau.as_secs_f64()).exp();
        target + (initial - target) * alpha
    }

    #[test]
    fn hold_and_climb_advance_from_the_same_sample() {
        let start = Instant::now();
        let mut band = BweBand::new(Bitrate::kbps(300));
        band.update(start, Bitrate::kbps(300));
        band.update(start + Duration::from_secs(1), Bitrate::kbps(1_300));

        assert_close(
            band.hold().as_f64(),
            expected(300_000.0, 1_300_000.0, Duration::from_secs(1), HOLD_RISE),
        );
        assert_close(
            band.climb().as_f64(),
            expected(300_000.0, 1_300_000.0, Duration::from_secs(1), CLIMB_RISE),
        );
        assert!(band.climb() <= band.hold());
    }

    #[test]
    fn climb_falls_immediately_while_hold_decays() {
        let start = Instant::now();
        let mut band = BweBand::new(Bitrate::kbps(2_000));
        band.update(start, Bitrate::kbps(2_000));
        band.update(start + Duration::from_millis(100), Bitrate::kbps(300));

        assert_eq!(band.climb(), Bitrate::kbps(300));
        assert!(band.hold() > band.climb());
    }

    #[test]
    fn ticks_finish_a_climb_without_new_samples() {
        let start = Instant::now();
        let mut band = BweBand::new(Bitrate::kbps(300));
        band.update(start, Bitrate::kbps(300));
        band.update(start + Duration::from_millis(100), Bitrate::kbps(1_300));
        band.tick(start + Duration::from_secs(4));

        assert!(band.climb().as_f64() > 1_100_000.0);
        assert!(band.climb() <= band.hold());
    }

    #[test]
    fn band_order_survives_alternating_samples() {
        let start = Instant::now();
        let mut band = BweBand::new(Bitrate::kbps(300));
        band.update(start, Bitrate::kbps(300));

        for step in 1..100 {
            let sample = if step % 2 == 0 {
                Bitrate::kbps(4_000)
            } else {
                Bitrate::kbps(80)
            };
            band.update(start + Duration::from_millis(step * 37), sample);
            assert!(band.climb() <= band.hold());
        }
    }
}
