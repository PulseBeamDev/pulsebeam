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
use crate::track::{StreamId, StreamWriter, Track, TrackLayer};
use pulsebeam_runtime::rand::RngCore;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, MediaTime, Mid, Pt, Rid};
use str0m::rtp::{SeqNo, Ssrc};
use tokio::time::Instant;
pub use video::Intent;

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

const BWE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(150);
const BWE_FALL_TIME_CONSTANT: Duration = Duration::from_millis(800);

slotmap::new_key_type! {
    pub struct SlotKey;
}

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

    fn tick(&mut self, now: Instant, overusing: bool) {
        self.update(now, self.current(), overusing);
    }

    fn update(&mut self, now: Instant, raw: Bitrate, overusing: bool) {
        let raw_bps = raw.as_f64();
        let Some(last_update) = self.last_update.replace(now) else {
            self.filtered_bps = raw_bps;
            return;
        };
        let elapsed = now.saturating_duration_since(last_update);
        if raw_bps >= self.filtered_bps {
            let alpha = (-elapsed.as_secs_f64() / BWE_RISE_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = raw_bps + (self.filtered_bps - raw_bps) * alpha;
        } else if overusing {
            // Genuine congestion (delay detector overusing): follow the estimate down.
            let alpha = (-elapsed.as_secs_f64() / BWE_FALL_TIME_CONSTANT.as_secs_f64()).exp();
            self.filtered_bps = raw_bps + (self.filtered_bps - raw_bps) * alpha;
        }
        // Otherwise str0m is lowering the estimate without congestion (it keeps
        // shrinking bwe when we're application-limited) — hold, don't let that
        // drag the allocator down.
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
    /// Whether str0m's delay detector currently signals congestion. Refreshed
    /// each allocation tick and consulted when the BWE estimate falls.
    overusing: bool,

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
            overusing: false,
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

    pub fn update_bitrate(&mut self, now: Instant, available_bandwidth: Bitrate) {
        self.available_bandwidth
            .update(now, available_bandwidth, self.overusing);
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, now: Instant, bwe: &mut Bwe) -> bool {
        self.overusing = bwe.is_overusing();
        // update rate per time
        self.available_bandwidth.tick(now, self.overusing);
        self.dirty_allocation = false;
        let (desired, assignments_changed) = self
            .video
            .update_allocations(self.available_bandwidth.current(), self.overusing);
        if self.last_desired != desired {
            bwe.set_desired_bitrate(desired);
            self.last_desired = desired;
        }
        assignments_changed
    }

    pub fn reconcile_routes(&mut self, _now: Instant, events: &mut impl ParticipantSink) {
        self.video.reconcile_routes(events);
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
    pub fn on_forward_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: &RtpPacket,
        writer: &mut StreamWriter,
    ) -> bool {
        self.video.on_rtp(stream_id, pkt, writer)
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
