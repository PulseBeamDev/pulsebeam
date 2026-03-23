use crate::entity::{ParticipantId, TrackId};
use crate::participant::ParticipantCore;
use crate::rtp::{
    self,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};
use ahash::HashMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

pub const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum SimulcastQuality {
    Low = 1,
    Medium = 2,
    High = 3,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: TrackId,
    pub origin_participant: ParticipantId,
    pub kind: MediaKind,
    pub mid: Mid,
    pub simulcast_rids: Option<Vec<Rid>>,
}

pub struct SimulcastLayer {
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub monitor: StreamMonitor,
    pub synchronizer: Synchronizer,
    pub filter: PacketFilter,
    pub keyframe_request_pending: bool,
    pub last_keyframe_sent: Option<Instant>,
}

pub struct Track {
    pub meta: Arc<TrackMeta>,
    pub layers: Vec<SimulcastLayer>,
    pub subscribers: BTreeSet<ParticipantId>,
}

impl Track {
    pub fn forward(
        &mut self,
        rid: Option<&Rid>,
        mut packet: rtp::RtpPacket,
        sr: Option<SenderInfo>,
        participants: &mut HashMap<ParticipantId, ParticipantCore>,
    ) {
        // 1. Find the layer (RID matching)
        let Some(layer) = self.layers.iter_mut().find(|l| l.rid.as_ref() == rid) else {
            return;
        };

        // 2. Transform (In-place sync and metrics)
        layer.synchronizer.process(&mut packet, sr);
        layer
            .monitor
            .process_packet(&packet, packet.payload.len() + packet.header_len);

        // 3. Filter (e.g., Audio VAD)
        if !(layer.filter)(&packet) {
            return;
        }

        // 4. Fanout
        for sub_key in &self.subscribers {
            if let Some(participant) = participants.get_mut(sub_key) {
                participant.on_forward_rtp(&self.meta.id, &rid, &packet);
            }
        }
    }

    /// Called by a subscriber when they detect packet loss or just joined.
    pub fn request_keyframe(&mut self, quality: SimulcastQuality) {
        if let Some(layer) = self.layers.iter_mut().find(|l| l.quality == quality) {
            layer.keyframe_request_pending = true;
        }
    }

    /// Drained by the Shard loop to send RTCP feedback upstream to the publisher.
    pub fn collect_keyframe_requests(&mut self, now: Instant) -> Vec<KeyframeRequest> {
        let mut reqs = Vec::new();
        for layer in &mut self.layers {
            if layer.keyframe_request_pending {
                // Debounce check
                if let Some(last) = layer.last_keyframe_sent {
                    if now.duration_since(last) < KEYFRAME_DEBOUNCE {
                        layer.keyframe_request_pending = false;
                        continue;
                    }
                }

                layer.last_keyframe_sent = Some(now);
                layer.keyframe_request_pending = false;
                reqs.push(KeyframeRequest {
                    kind: KeyframeRequestKind::Pli,
                    rid: layer.rid,
                    mid: self.meta.mid,
                });
            }
        }
        reqs
    }
}

/// Factory function to build a new Track with its layers.
pub fn new_track(meta: Arc<TrackMeta>) -> Track {
    let mut layers = Vec::new();

    let rids: Vec<_> = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    for (index, rid) in rids.into_iter().enumerate() {
        let (clock_rate, filter) = match meta.kind {
            MediaKind::Audio => (rtp::AUDIO_FREQUENCY, should_forward_audio as PacketFilter),
            MediaKind::Video => (rtp::VIDEO_FREQUENCY, should_forward_noop as PacketFilter),
        };

        let quality = match (meta.kind, index) {
            (MediaKind::Audio, _) => SimulcastQuality::Low,
            (MediaKind::Video, 0) => SimulcastQuality::High,
            (MediaKind::Video, 1) => SimulcastQuality::Medium,
            _ => SimulcastQuality::Low,
        };

        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let state = StreamState::new(true, 1_000_000); // Default bitrate

        layers.push(SimulcastLayer {
            rid,
            quality,
            monitor: StreamMonitor::new(meta.kind, stream_id, state),
            synchronizer: Synchronizer::new(clock_rate),
            filter,
            keyframe_request_pending: false,
            last_keyframe_sent: None,
        });
    }

    Track {
        meta,
        layers,
        subscribers: BTreeSet::new(),
    }
}

pub type PacketFilter = fn(packet: &rtp::RtpPacket) -> bool;

#[inline]
pub fn should_forward_audio(packet: &rtp::RtpPacket) -> bool {
    if packet.payload.len() < 12 {
        return true;
    }
    let ext = &packet.ext_vals;
    if let Some(vad) = ext.voice_activity {
        return vad;
    }
    if let Some(level) = ext.audio_level {
        return level > -50;
    }
    true
}

#[inline]
fn should_forward_noop(_: &rtp::RtpPacket) -> bool {
    true
}
