use pulsebeam_runtime::sync::Arc;
use std::fmt::{Debug, Display};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use str0m::rtp::Ssrc;

use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Pt, Rid, SimulcastLayer};
use tokio::sync::Notify;
use tokio::time::Instant;

#[cfg(test)]
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

pub struct StreamWriter<'a>(pub &'a mut str0m::Rtc);

impl<'a> StreamWriter<'a> {
    pub fn write(&mut self, pkt: &RtpPacket, ssrc: &Ssrc, pt: Pt) {
        let mut api = self.0.direct_api();
        let Some(stream) = api.stream_tx(ssrc) else {
            tracing::warn!("no stream_tx found for {}", ssrc);
            return;
        };
        let res = stream.write_rtp(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.marker,
            pkt.ext_vals.clone(),
            true,
            pkt.payload.to_vec(),
        );
        if let Err(err) = res {
            tracing::warn!(%ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }
}

/// Sends keyframe requests to the upstream publisher.
///
/// Always requests PLI — the publisher decides the exact RTCP feedback type.
#[derive(Clone, Debug)]
pub struct KeyframeRequester {
    signal: Arc<AtomicBool>,
    notify: Arc<Notify>,
    #[cfg(test)]
    pub request_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl KeyframeRequester {
    /// Signal that a PLI keyframe is needed.
    pub fn request(&self) {
        self.signal.store(true, Ordering::Relaxed);
        self.notify.notify_one();
        #[cfg(test)]
        {
            self.request_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Polls a pending keyframe request with leading-edge debounce.
///
/// Owned by `UpstreamAllocator`; the corresponding [`KeyframeRequester`] is held
/// by each [`TrackLayer`].
#[derive(Debug)]
pub struct KeyframePoll {
    signal: Arc<AtomicBool>,
    pub mid: Mid,
    pub rid: Option<Rid>,
    debounce: Duration,
    last_sent: Option<Instant>,
}

impl KeyframePoll {
    /// Return a pending PLI [`KeyframeRequest`] if one is flagged and debounce permits.
    ///
    /// Leading-edge semantics: the first request in any window passes through
    /// immediately; subsequent requests within the same window are discarded.
    pub fn take_pending(&mut self, now: Instant) -> Option<KeyframeRequest> {
        if !self.signal.load(Ordering::Relaxed) {
            return None;
        }
        self.signal.store(false, Ordering::Relaxed);
        // Within debounce window: discard.
        if let Some(last) = self.last_sent
            && now.duration_since(last) < self.debounce
        {
            return None;
        }
        self.last_sent = Some(now);
        Some(KeyframeRequest {
            kind: KeyframeRequestKind::Pli,
            rid: self.rid,
            mid: self.mid,
        })
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
    filter: PacketFilter,
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
        (self.filter)(pkt)
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
            filter: should_forward_audio,
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

    let _keyframe_notify = Arc::new(Notify::new());
    let mut senders = Vec::new();
    let mut layers = Vec::with_capacity(simulcast_rids.len());
    let mut keyframe_polls = Vec::new();

    for (index, &rid) in simulcast_rids.iter().enumerate() {
        let (bitrate, quality) = match (rid, index) {
            (None, _) => (500_000, LayerQuality::Low),
            (Some(_), 0) => (2_500_000, LayerQuality::High),
            (Some(_), 1) => (750_000, LayerQuality::Medium),
            (Some(_), _) => (150_000, LayerQuality::Low),
        };

        let signal = Arc::new(AtomicBool::new(false));
        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.kind, stream_id, stream_state.clone());

        keyframe_polls.push(KeyframePoll {
            signal: signal.clone(),
            mid,
            rid,
            debounce: KEYFRAME_DEBOUNCE,
            last_sent: None,
        });

        senders.push(UpstreamTrackLayer {
            mid,
            rid,
            quality,
            filter: should_forward_noop,
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

type PacketFilter = fn(packet: &RtpPacket) -> bool;

/// Determines if an audio packet is essential (Speech or DTX) and should be forwarded.
#[inline]
pub fn should_forward_audio(packet: &RtpPacket) -> bool {
    const DTX_THRESHOLD: usize = 12;
    // -50dBov is a standard noise floor. Anything quieter is background hiss.
    const NOISE_FLOOR_DB: i8 = -50;

    // 1. Always relay tiny packets (Comfort Noise / DTX)
    if packet.payload.len() < DTX_THRESHOLD {
        return true;
    }

    let ext = &packet.ext_vals;

    // 2. Strict VAD Check (Priority)
    // If the V bit is present, trust it explicitly.
    if let Some(vad) = ext.voice_activity {
        return vad;
    }

    // 3. Noise Gate Fallback
    // If VAD is missing but we have levels, drop silence/background noise.
    if let Some(level) = ext.audio_level {
        return level > NOISE_FLOOR_DB;
    }

    // 4. Fallback: No metadata, assumed active.
    true
}

#[inline]
fn should_forward_noop(_: &RtpPacket) -> bool {
    true
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
            id: track_id,
            origin: participant_id,
            kind: MediaKind::Video,
        };
        crate::track::new_video(mid, meta, layers)
    }

    pub fn make_audio_track(participant_id: ParticipantId, mid: Mid) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(MediaKind::Audio, &mid);
        let meta = TrackMeta {
            id: track_id,
            origin: participant_id,
            kind: MediaKind::Audio,
        };
        crate::track::new_audio(mid, meta)
    }
}
