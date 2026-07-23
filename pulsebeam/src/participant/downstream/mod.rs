mod audio;
mod video;

use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::entity::TrackKind;
use crate::id::AudioSelectorSlotId;
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::VideoAllocator;
use crate::participant::event::ParticipantSink;
use crate::rtp::RtpPacket;
use crate::track::{StreamId, StreamWriter, Track, TrackLayer};
use pulsebeam_runtime::rand::RngCore;
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaKind, Mid, Pt, Rid};
use str0m::rtp::Ssrc;
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

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    pub video: VideoAllocator,
    audio: AudioAllocator,

    available_bandwidth: Bitrate,
    last_desired: Bitrate,
}

/// `set_desired_bitrate` wakes str0m's probe controller. Encoder-rate samples
/// naturally drift a little, but they are not a new application demand. Only
/// communicate a material policy change to BWE.
fn desired_bitrate_changed(previous: Bitrate, next: Bitrate) -> bool {
    if previous.is_zero() || next.is_zero() {
        return previous != next;
    }

    (next.as_f64() - previous.as_f64()).abs() / previous.as_f64() >= 0.10
}

impl DownstreamAllocator {
    pub fn new(_participant_id: ParticipantId, manual_sub: bool, rng: &mut impl RngCore) -> Self {
        Self {
            video: VideoAllocator::new(manual_sub, rng),
            audio: AudioAllocator::new(),
            dirty_allocation: false,

            available_bandwidth: video::MIN_BANDWIDTH,
            last_desired: video::MIN_BANDWIDTH,
        }
    }

    pub fn add_track(&mut self, track: Track, now: Instant) {
        if track.meta.id.kind() == TrackKind::Video {
            self.video.add_track(track, now);
            self.dirty_allocation = true;
        }
        // Audio tracks need no static registration; slots are claimed dynamically.
    }

    pub(super) fn remove_track(&mut self, track_id: &TrackId, now: Instant) -> bool {
        let removed = self.video.remove_track(track_id, now);
        if removed {
            self.dirty_allocation = true;
        }
        removed
    }

    pub fn add_slot(&mut self, slot: SlotConfig, now: Instant) {
        match slot.kind {
            MediaKind::Video => {
                self.video.add_slot(slot, now);
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

    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) {
        self.available_bandwidth = available_bandwidth;
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, bwe: &mut Bwe, now: Instant) -> bool {
        self.dirty_allocation = false;
        let (desired, assignments_changed) = self
            .video
            .update_allocations_at(self.available_bandwidth, now);
        if desired_bitrate_changed(self.last_desired, desired) {
            tracing::info!(
                desired = %desired,
                previous = %self.last_desired,
                "set BWE application demand"
            );
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
        let assignments_changed = self.update_allocations(bwe, now);
        self.video.poll_slow(now, self.available_bandwidth, events);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_bitrate_changes_only_for_material_demand_updates() {
        assert!(!desired_bitrate_changed(
            Bitrate::kbps(1250),
            Bitrate::kbps(1320)
        ));
        assert!(desired_bitrate_changed(
            Bitrate::kbps(1250),
            Bitrate::kbps(1400)
        ));
        assert!(desired_bitrate_changed(Bitrate::kbps(1250), Bitrate::ZERO));
    }
}
