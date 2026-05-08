use std::fmt::{Debug, Display};
use std::time::Duration;
use str0m::rtp::Ssrc;

use str0m::media::{KeyframeRequestKind, MediaKind, Mid, Pt, Rid, SimulcastLayer};
use tokio::time::Instant;

use crate::entity::ParticipantId;
use crate::entity::TrackId;
use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};
use str0m::rtp::rtcp::SenderInfo;

pub type StreamId = (TrackId, Option<Rid>);

/// Leading-edge debounce interval for keyframe requests forwarded upstream.
pub const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);
pub const MAX_SIMULCAST_LAYERS: usize = 3;

#[derive(Debug, Clone)]
pub struct GlobalKeyframeRequest {
    pub shard_id: usize,
    pub origin: ParticipantId,
    pub stream_id: StreamId,
    pub kind: KeyframeRequestKind,
}

pub struct StreamWriter<'a>(pub &'a mut str0m::Rtc);

impl<'a> StreamWriter<'a> {
    pub fn write_video_owned(&mut self, pkt: RtpPacket, ssrc: &Ssrc, pt: Pt) {
        let mut api = self.0.direct_api();
        let Some(stream) = api.stream_tx(ssrc) else {
            tracing::warn!(
                target: crate::log::TARGET_VIDEO,
                "no stream_tx found for {}", ssrc);
            return;
        };
        tracing::trace!(
            target: crate::log::TARGET_VIDEO,
            %ssrc, %pt, seq = %pkt.seq_no, len = pkt.payload.len(), marker = pkt.marker, "Writing RTP packet");
        let res = stream.write_rtp(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.marker,
            pkt.ext_vals.clone(),
            true,
            pkt.payload,
        );
        if let Err(err) = res {
            tracing::warn!(
                target: crate::log::TARGET_VIDEO,
                %ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }

    pub fn write_audio_owned(&mut self, pkt: RtpPacket, ssrc: &Ssrc, pt: Pt) {
        let mut api = self.0.direct_api();
        let Some(stream) = api.stream_tx(ssrc) else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                "no stream_tx found for {}", ssrc);
            return;
        };
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            %ssrc, %pt, seq = %pkt.seq_no, len = pkt.payload.len(), marker = pkt.marker, "Writing RTP packet");
        let res = stream.write_rtp(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.marker,
            pkt.ext_vals.clone(),
            false, // audio is not nackable
            pkt.payload,
        );
        if let Err(err) = res {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                %ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LayerQuality {
    Low = 1,
    Medium = 2,
    High = 3,
}

impl std::fmt::Debug for LayerQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        };
        f.write_str(txt)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct TrackMeta {
    /// The shard ID that hosts this track's publisher.
    pub shard_id: usize,
    pub id: crate::entity::TrackId,
    pub origin: crate::entity::ParticipantId,
    pub kind: MediaKind,
}

#[derive(Debug)]
pub struct UpstreamTrackLayer {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub quality: LayerQuality,
    pub monitor: StreamMonitor,
    synchronizer: Synchronizer,
}

impl PartialEq for UpstreamTrackLayer {
    fn eq(&self, other: &Self) -> bool {
        self.mid == other.mid && self.rid == other.rid
    }
}

impl Eq for UpstreamTrackLayer {}

impl UpstreamTrackLayer {
    pub fn poll_stats(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.monitor.poll(now, is_any_sibling_active);
    }

    pub fn process(&mut self, pkt: &mut RtpPacket, sr: Option<SenderInfo>) -> bool {
        self.synchronizer.process(pkt, sr);
        self.monitor
            .process_packet(pkt, pkt.payload.len() + pkt.header_len);
        // audio will only be filtered at the centralized audio_selector
        true
    }
}

pub struct UpstreamTrack {
    pub meta: TrackMeta,
    pub layers: Vec<UpstreamTrackLayer>,
}

impl PartialEq for UpstreamTrack {
    fn eq(&self, other: &Self) -> bool {
        self.meta == other.meta && self.layers == other.layers
    }
}

impl Eq for UpstreamTrack {}

impl UpstreamTrack {
    pub fn process(
        &mut self,
        rid: Option<&Rid>,
        packet: &mut RtpPacket,
        sr: Option<SenderInfo>,
    ) -> bool {
        let sender = self
            .layers
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always be available");
        sender.process(packet, sr)
    }

    pub fn by_rid_mut(&mut self, rid: &Option<Rid>) -> Option<&mut UpstreamTrackLayer> {
        self.layers.iter_mut().find(|s| s.rid == *rid)
    }

    pub fn poll_stats(&mut self, now: Instant) {
        let total_active_streams = self
            .layers
            .iter()
            .filter(|s| !s.monitor.shared_state().is_inactive())
            .count();

        for layer in self.layers.iter_mut() {
            let is_current_layer_active = !layer.monitor.shared_state().is_inactive();
            let is_any_sibling_active = if is_current_layer_active {
                total_active_streams > 1
            } else {
                total_active_streams > 0
            };

            layer.poll_stats(now, is_any_sibling_active);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub meta: TrackMeta,
    pub layers: Vec<TrackLayer>,
}

impl Track {
    pub fn lowest_quality(&self) -> &TrackLayer {
        self.layers
            .iter()
            .min_by_key(|l| l.quality)
            .expect("at least one layer")
    }

    pub fn by_quality(&self, quality: LayerQuality) -> Option<&TrackLayer> {
        self.layers.iter().find(|l| l.quality == quality)
    }

    pub fn higher_quality(&self, current: LayerQuality) -> Option<&TrackLayer> {
        self.layers
            .iter()
            .filter(|l| l.quality > current)
            .min_by_key(|l| l.quality)
    }

    pub fn lower_quality(&self, current: LayerQuality) -> Option<&TrackLayer> {
        self.layers
            .iter()
            .filter(|l| l.quality < current)
            .max_by_key(|l| l.quality)
    }
}

#[derive(Clone, Debug)]
pub struct TrackLayer {
    pub meta: TrackMeta,
    pub rid: Option<Rid>,
    pub quality: LayerQuality,
    // pub keyframe_requester: KeyframeRequester,
    pub state: StreamState,
}

impl Eq for TrackLayer {}

impl PartialEq for TrackLayer {
    fn eq(&self, other: &Self) -> bool {
        other.meta == self.meta && other.rid == self.rid && other.quality == self.quality
    }
}

impl TrackLayer {
    pub fn stream_id(&self) -> StreamId {
        (self.meta.id, self.rid)
    }

    pub fn is(&self, stream_id: &StreamId) -> bool {
        self.meta.id == stream_id.0 && self.rid == stream_id.1
    }

    pub fn request_keyframe(&self) {
        // self.keyframe_requester.request();
    }
}

impl Display for TrackLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.meta.id, self.rid.as_deref().unwrap_or("_"))
    }
}

/// Construct a new audio track sender and its corresponding layer descriptor.
pub fn new_audio(mid: Mid, meta: TrackMeta) -> (UpstreamTrack, Track) {
    debug_assert_eq!(meta.kind, MediaKind::Audio);
    let bitrate = 64_000;
    let stream_state = StreamState::new(true, bitrate);
    let stream_id = format!("{}:_", meta.id);
    let monitor = StreamMonitor::new(meta.kind, stream_id, stream_state.clone());

    let sender = UpstreamTrack {
        meta: meta.clone(),
        layers: vec![UpstreamTrackLayer {
            mid,
            rid: None,
            quality: LayerQuality::Low,
            synchronizer: Synchronizer::new(rtp::AUDIO_FREQUENCY),
            monitor,
        }],
    };
    (
        sender,
        Track {
            meta,
            layers: Vec::with_capacity(MAX_SIMULCAST_LAYERS),
        },
    )
}

/// Construct a new video track sender and its per-layer descriptors.
pub fn new_video(mid: Mid, meta: TrackMeta, layers: Vec<SimulcastLayer>) -> (UpstreamTrack, Track) {
    debug_assert_eq!(meta.kind, MediaKind::Video);
    let simulcast_rids: Vec<Option<Rid>> = if layers.is_empty() {
        vec![None]
    } else {
        layers.iter().map(|l| Some(l.rid)).collect()
    };

    let mut senders = Vec::new();
    let mut layers = Vec::with_capacity(simulcast_rids.len());

    for (index, &rid) in simulcast_rids.iter().enumerate() {
        let (bitrate, quality) = match (rid, index) {
            (None, _) => (500_000, LayerQuality::Low),
            (Some(_), 0) => (2_500_000, LayerQuality::High),
            (Some(_), 1) => (750_000, LayerQuality::Medium),
            (Some(_), _) => (150_000, LayerQuality::Low),
        };

        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.kind, stream_id, stream_state.clone());

        senders.push(UpstreamTrackLayer {
            mid,
            rid,
            quality,
            synchronizer: Synchronizer::new(rtp::VIDEO_FREQUENCY),
            monitor,
        });
        layers.push(TrackLayer {
            meta: meta.clone(),
            rid,
            quality,
            state: stream_state,
        });
    }
    senders.sort_by_key(|e| std::cmp::Reverse(e.quality));
    layers.sort_by_key(|e| std::cmp::Reverse(e.quality));

    tracing::info!(track_id = ?meta.id, layers = ?layers.len(), "discovered video layers mapping");
    let track = Track {
        meta: meta.clone(),
        layers,
    };

    (
        UpstreamTrack {
            meta,
            layers: senders,
        },
        track,
    )
}

#[cfg(test)]
pub mod test_utils {
    use super::*;

    pub fn make_video_track(
        participant_id: ParticipantId,
        mid: Mid,
        layers: Vec<SimulcastLayer>,
    ) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(MediaKind::Video, &mid);
        let meta = TrackMeta {
            shard_id: 0,
            id: track_id,
            origin: participant_id,
            kind: MediaKind::Video,
        };
        crate::track::new_video(mid, meta, layers)
    }

    pub fn make_audio_track(participant_id: ParticipantId, mid: Mid) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(MediaKind::Audio, &mid);
        let meta = TrackMeta {
            shard_id: 0,
            id: track_id,
            origin: participant_id,
            kind: MediaKind::Audio,
        };
        crate::track::new_audio(mid, meta)
    }
}
