#![allow(
    clippy::disallowed_types,
    reason = "Track distributes immutable publisher snapshots through ArcSwap"
)]

use arc_swap::ArcSwap;
use std::collections::VecDeque;
use std::fmt::{Debug, Display};
use std::sync::Arc;
use std::time::Duration;

use crate::entity::TrackId;
use crate::entity::{ParticipantId, TrackKind};
use crate::id::ShardId;
use crate::rtp::normalize::{Normalization, StreamFacts, StreamNormalizer};
use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamStats},
    sync::TrackSynchronizer,
};
pub use data_track::*;
pub use pulsebeam_core::simulcast::LayerQuality;
use str0m::media::{Mid, Pt, Rid, SimulcastLayer};
use str0m::rtp::Ssrc;
use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

pub type StreamId = (TrackId, Option<Rid>);

pub struct ProcessedRtp {
    pub first: Option<RtpPacket>,
    pub remaining: Vec<RtpPacket>,
    pub request_keyframe: bool,
    pub valid_route: bool,
}

/// Leading-edge debounce interval for keyframe requests forwarded upstream.
pub const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);
pub const MAX_SIMULCAST_LAYERS: usize = 3;

/// Deferred outbound RTP write.  Applying it to `Rtc` is deliberately the
/// participant core's responsibility so it can drain str0m between writes.
pub enum StreamWrite {
    Video {
        pkt: RtpPacket,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
        pt: Pt,
    },
    Audio {
        pkt: RtpPacket,
        mid: Mid,
        ssrc: Ssrc,
        pt: Pt,
    },
}

/// Reusable packet queue shared by all downstream allocators for one
/// participant. It never touches `Rtc`; callers only enqueue writes.
pub struct StreamWriter {
    pending: VecDeque<StreamWrite>,
}

impl Default for StreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamWriter {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(64),
        }
    }

    pub fn write_video_owned(
        &mut self,
        pkt: RtpPacket,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
        pt: Pt,
    ) {
        self.pending.push_back(StreamWrite::Video {
            pkt,
            mid,
            rid,
            ssrc,
            pt,
        });
    }

    pub fn write_audio_owned(&mut self, pkt: RtpPacket, mid: Mid, ssrc: Ssrc, pt: Pt) {
        self.pending
            .push_back(StreamWrite::Audio { pkt, mid, ssrc, pt });
    }

    pub fn pop(&mut self) -> Option<StreamWrite> {
        self.pending.pop_front()
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct TrackMeta {
    pub room_id: crate::entity::RoomId,
    /// The shard ID that hosts this track's publisher.
    pub shard_id: ShardId,
    pub id: crate::entity::TrackId,
    pub origin: crate::entity::ParticipantId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackSelector {
    pub(crate) track: Option<TrackId>,
    pub(crate) publisher: Option<ParticipantId>,
    pub(crate) kind: Option<TrackKind>,
    pub(crate) label: Option<String>,
}

impl TrackSelector {
    pub(crate) fn audio() -> Self {
        Self::kind(TrackKind::Audio)
    }

    pub(crate) fn video() -> Self {
        Self::kind(TrackKind::Video)
    }

    pub(crate) fn data_topic(publisher: Option<ParticipantId>, label: String) -> Self {
        Self {
            track: None,
            publisher,
            kind: Some(TrackKind::Data),
            label: Some(label),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn track(track: TrackId) -> Self {
        Self {
            track: Some(track),
            publisher: None,
            kind: None,
            label: None,
        }
    }

    fn kind(kind: TrackKind) -> Self {
        Self {
            track: None,
            publisher: None,
            kind: Some(kind),
            label: None,
        }
    }

    pub(crate) fn matches(&self, track: &Track) -> bool {
        self.track.is_none_or(|expected| track.id() == expected)
            && self
                .publisher
                .is_none_or(|expected| track.meta().origin == expected)
            && self.kind.is_none_or(|expected| track.kind() == expected)
            && self
                .label
                .as_deref()
                .is_none_or(|expected| track.publication_label().as_deref() == Some(expected))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionPolicy {
    All,
    Allocated,
}

/// One encoding's ingest: normalize the packet, then measure it.
///
/// The two halves are deliberately separate objects. Normalization is the
/// once-per-node work a future UDP ingress reuses verbatim; measurement is what
/// the whole node shares through `StreamStats`.
#[derive(Debug)]
pub struct UpstreamTrackLayer {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub quality: LayerQuality,
    normalizer: StreamNormalizer,
    pub monitor: StreamMonitor,
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

    fn normalize(&mut self, pkt: RtpPacket) -> Normalization {
        self.normalizer.normalize(pkt)
    }

    fn process_normalized(&mut self, pkt: &RtpPacket, facts: StreamFacts) {
        self.monitor.process_packet(pkt);
        if let Some(count) = facts.decode_targets {
            self.monitor.set_decode_target_count(count);
        }
    }

    #[cfg(test)]
    pub fn process(&mut self, pkt: &mut RtpPacket) -> bool {
        let normalization = self.normalize(std::mem::take(pkt));
        let Some((packet, facts)) = normalization.first else {
            return false;
        };
        if !normalization.remaining.is_empty() {
            return false;
        }
        self.process_normalized(&packet, facts);
        *pkt = packet;
        true
    }

    /// Apply a (track-wide) Video Layers Allocation to this layer using its
    /// learned stream index: the sender's declared target bitrate, resolution,
    /// and active/inactive state.
    fn apply_vla(&mut self, vla: &str0m::rtp::vla::VideoLayersAllocation) {
        let Some(idx) = self.normalizer.vla_index().map(usize::from) else {
            return;
        };
        let target_bps = vla_stream_target_bps(vla, idx).unwrap_or(0);
        let height = vla_stream_height_px(vla, idx).map(u32::from);
        let first_declaration = self.monitor.apply_vla(target_bps, height);

        // Record the per-decode-target cost ladder and frame rate so the allocator
        // can cost each temporal rung rather than estimating.
        let temporal = vla_stream_temporal_cumulative_kbps(vla, idx);
        let full_fps = vla_stream_framerate(vla, idx).unwrap_or(0);
        if !temporal.is_empty() {
            self.monitor.set_temporal_ladder(&temporal, full_fps);
        }
        if first_declaration {
            tracing::info!(
                mid = %self.mid,
                rid = ?self.rid,
                target_kbps = target_bps / 1000,
                height = self.monitor.stats().height(),
                "VLA: sender declared layer target bitrate; allocating on it"
            );
        }
    }
}

/// Declared target bitrate (bps) for simulcast stream `idx` in a Video Layers
/// Allocation: the highest cumulative temporal bitrate across its spatial
/// layers. `None` when the stream is absent or declared inactive.
pub(crate) fn vla_stream_target_bps(
    vla: &str0m::rtp::vla::VideoLayersAllocation,
    idx: usize,
) -> Option<u64> {
    let stream = vla.simulcast_streams.get(idx)?;
    let kbps = stream
        .spatial_layers
        .iter()
        .filter_map(|sl| sl.temporal_layers.iter().map(|t| t.cumulative_kbps).max())
        .max()?;
    Some(kbps.saturating_mul(1000))
}

/// Declared frame height (px) for simulcast stream `idx`: the tallest spatial
/// layer that carries a resolution. `None` when absent (VLA omits resolution).
pub(crate) fn vla_stream_height_px(
    vla: &str0m::rtp::vla::VideoLayersAllocation,
    idx: usize,
) -> Option<u16> {
    let stream = vla.simulcast_streams.get(idx)?;
    stream
        .spatial_layers
        .iter()
        .filter_map(|sl| sl.resolution_and_framerate.as_ref().map(|r| r.height))
        .max()
}

/// Cumulative bitrate (kbps) per decode target for simulcast stream `idx`, from
/// its first spatial layer's temporal ladder — the per-temporal costs the SFU
/// allocates each decode target against. Empty when the sender declared none.
pub(crate) fn vla_stream_temporal_cumulative_kbps(
    vla: &str0m::rtp::vla::VideoLayersAllocation,
    idx: usize,
) -> Vec<u64> {
    vla.simulcast_streams
        .get(idx)
        .and_then(|s| s.spatial_layers.first())
        .map(|sl| {
            sl.temporal_layers
                .iter()
                .map(|t| t.cumulative_kbps)
                .collect()
        })
        .unwrap_or_default()
}

/// Declared full frame rate for simulcast stream `idx`: the highest framerate any
/// of its spatial layers reports. `None` when the VLA omits resolution/framerate.
pub(crate) fn vla_stream_framerate(
    vla: &str0m::rtp::vla::VideoLayersAllocation,
    idx: usize,
) -> Option<u32> {
    let stream = vla.simulcast_streams.get(idx)?;
    stream
        .spatial_layers
        .iter()
        .filter_map(|sl| {
            sl.resolution_and_framerate
                .as_ref()
                .map(|r| u32::from(r.framerate))
        })
        .max()
}

pub enum UpstreamStats {
    Video(VideoStats),
    Audio(NullStats),
    Data(NullStats),
}

impl UpstreamStats {
    fn update(&mut self, monitor: &TrackMonitor) {
        match self {
            Self::Video(stats) => stats.update(monitor),
            Self::Audio(stats) | Self::Data(stats) => stats.update(monitor),
        }
    }
}

pub struct UpstreamTrack {
    pub meta: TrackMeta,
    /// One clock for the whole track. Every encoding is the same source over the
    /// same connection, so their `playout_time`s are reconciled onto a single
    /// timeline rather than each encoding running an independent clock.
    synchronizer: TrackSynchronizer,
    /// One monitor for the whole track. It owns every simulcast encoding's
    /// ingest state and all cross-encoding reasoning (VLA fan-out, sibling
    /// activity, aggregate demand), while each encoding keeps its own
    /// `StreamStats` so the downstream allocator still sees per-layer metadata.
    pub monitor: TrackMonitor,
    stats: UpstreamStats,
}

impl PartialEq for UpstreamTrack {
    fn eq(&self, other: &Self) -> bool {
        self.meta == other.meta && self.monitor == other.monitor
    }
}

impl Eq for UpstreamTrack {}

impl UpstreamTrack {
    pub fn process(
        &mut self,
        rid: Option<&Rid>,
        mut packet: RtpPacket,
        sr: Option<SenderInfo>,
    ) -> ProcessedRtp {
        // Stamp playout_time on the track's shared clock before the encoding's
        // monitor and the switcher downstream see it.
        self.synchronizer.process(&mut packet, sr);
        self.monitor.process(rid, packet)
    }

    pub fn by_rid_mut(&mut self, rid: &Option<Rid>) -> Option<&mut UpstreamTrackLayer> {
        self.monitor.by_rid_mut(rid)
    }

    pub fn poll_stats(&mut self, now: Instant) {
        self.monitor.poll(now);
        self.stats.update(&self.monitor);
    }
}

/// The whole-track monitor: every simulcast encoding of one upstream track,
/// mashed into a single unit. Per-encoding metrics stay separated (each
/// `UpstreamTrackLayer` owns its own `StreamStats`) so the allocator keeps its
/// fine-grained per-layer view, but the cross-encoding decisions — a VLA on one
/// encoding describing all of them, whether a layer has a live sibling, the
/// track's aggregate demand — are made here where the whole ladder is visible.
#[derive(Debug)]
pub struct TrackMonitor {
    encodings: Vec<UpstreamTrackLayer>,
}

impl PartialEq for TrackMonitor {
    fn eq(&self, other: &Self) -> bool {
        self.encodings == other.encodings
    }
}

impl Eq for TrackMonitor {}

impl TrackMonitor {
    fn new(encodings: Vec<UpstreamTrackLayer>) -> Self {
        Self { encodings }
    }

    pub fn process(&mut self, rid: Option<&Rid>, packet: RtpPacket) -> ProcessedRtp {
        let Some(index) = self.encodings.iter().position(|s| s.rid.as_ref() == rid) else {
            return ProcessedRtp {
                first: None,
                remaining: Vec::new(),
                request_keyframe: false,
                valid_route: false,
            };
        };
        let normalization = self
            .encodings
            .get_mut(index)
            .map(|encoding| encoding.normalize(packet));
        let Some(normalization) = normalization else {
            debug_assert!(false, "located encoding disappeared before normalization");
            return ProcessedRtp {
                first: None,
                remaining: Vec::new(),
                request_keyframe: false,
                valid_route: false,
            };
        };

        let first = normalization
            .first
            .map(|(packet, facts)| self.process_normalized(index, packet, facts));
        let remaining = normalization
            .remaining
            .into_iter()
            .map(|(packet, facts)| self.process_normalized(index, packet, facts))
            .collect();
        ProcessedRtp {
            first,
            remaining,
            request_keyframe: normalization.request_keyframe,
            valid_route: true,
        }
    }

    fn process_normalized(
        &mut self,
        index: usize,
        packet: RtpPacket,
        facts: StreamFacts,
    ) -> RtpPacket {
        let Some(encoding) = self.encodings.get_mut(index) else {
            debug_assert!(
                false,
                "normalization selected an encoding outside the track"
            );
            return packet;
        };
        encoding.process_normalized(&packet, facts);

        if let Some(vla) = packet
            .ext_vals
            .user_values
            .get::<str0m::rtp::vla::VideoLayersAllocation>()
            .cloned()
        {
            for encoding in &mut self.encodings {
                encoding.apply_vla(&vla);
            }
        }
        packet
    }

    pub fn by_rid_mut(&mut self, rid: &Option<Rid>) -> Option<&mut UpstreamTrackLayer> {
        self.encodings.iter_mut().find(|s| s.rid == *rid)
    }

    pub fn layer_states(&self) -> TrackStates {
        self.encodings
            .iter()
            .map(|e| (e.rid, e.monitor.stats()))
            .collect()
    }

    fn snapshot(&self) -> VideoStatsSnapshot {
        VideoStatsSnapshot {
            layers: self.layer_states(),
        }
    }

    pub fn poll(&mut self, now: Instant) {
        // Derive the sibling gate from packet arrivals, never from the encodings'
        // published `inactive` flags: those flags are what this loop writes, so
        // reading them back closes a feedback loop. It previously oscillated —
        // with every encoding silent, all of them paused on one tick, which made
        // the count zero, which read as "no sibling active" and un-paused them
        // all on the next, at the poll rate for the whole 1s..3s window between
        // the pause and dead timeouts.
        let recent = self
            .encodings
            .iter()
            .filter(|e| e.monitor.has_recent_packets(now))
            .count();
        debug_assert!(recent <= self.encodings.len());

        for encoding in &mut self.encodings {
            let self_recent = encoding.monitor.has_recent_packets(now);
            let is_any_sibling_active = recent.saturating_sub(usize::from(self_recent)) > 0;
            encoding.poll_stats(now, is_any_sibling_active);
        }
    }

    /// Aggregate slow-decay demand across every active encoding — the whole
    /// track's stable bitrate, for demand/reservation reasoning that wants the
    /// ladder as one figure rather than per layer.
    pub fn aggregate_stable_bitrate_bps(&self) -> f64 {
        self.encodings
            .iter()
            .filter(|s| !s.monitor.stats().is_inactive())
            .map(|s| s.monitor.stats().stable_bitrate_bps())
            .sum()
    }
}

#[derive(Debug, Clone)]
pub enum Track {
    Audio(AudioTrack),
    Video(VideoTrack),
    Data(DataTrack),
}

#[derive(Debug, Clone)]
pub struct AudioTrack {
    pub meta: TrackMeta,
    pub reverse: Option<crate::route::RouteHandle>,
}

#[derive(Debug, Clone)]
pub struct VideoTrack {
    pub meta: TrackMeta,
    pub layers: Vec<TrackLayer>,
    stats: VideoStats,
    pub reverse: Option<crate::route::RouteHandle>,
}

#[derive(Debug, Clone)]
pub struct DataTrack {
    pub meta: TrackMeta,
    pub topic: crate::track::Topic,
    pub lane: crate::track::DataLane,
    pub reverse: Option<crate::route::RouteHandle>,
}

#[derive(Debug, Clone, Default)]
struct VideoStatsSnapshot {
    layers: Vec<(Option<Rid>, StreamStats)>,
}

#[derive(Debug, Clone)]
pub struct VideoStats(Arc<ArcSwap<VideoStatsSnapshot>>);

impl VideoStats {
    fn new(layers: &[TrackLayer]) -> Self {
        let snapshot = VideoStatsSnapshot {
            layers: layers
                .iter()
                .map(|layer| {
                    (
                        layer.rid,
                        StreamStats::new(
                            false,
                            layer.quality.seed_bitrate_bps(),
                            layer.quality.fallback_height(),
                        ),
                    )
                })
                .collect(),
        };
        Self(Arc::new(ArcSwap::from_pointee(snapshot)))
    }

    fn update(&self, monitor: &TrackMonitor) {
        let previous = self.0.load_full();
        let mut snapshot = monitor.snapshot();
        for (rid, current) in &mut snapshot.layers {
            if !current.inactive {
                continue;
            }
            let Some((_, old)) = previous.layers.iter().find(|(old_rid, _)| old_rid == rid) else {
                continue;
            };
            current.bitrate_bps = old.bitrate_bps;
            current.stable_bitrate_bps = old.stable_bitrate_bps;
            current.decode_targets = old.decode_targets;
            current.decode_target_kbps = old.decode_target_kbps;
            current.full_fps = old.full_fps;
        }
        self.0.store(Arc::new(snapshot));
    }

    pub(crate) fn layer_states(&self) -> TrackStates {
        self.0.load().layers.clone()
    }
}

#[derive(Debug, Clone)]
pub struct NullStats;

impl NullStats {
    fn new() -> Self {
        Self {}
    }

    fn update(&self, _monitor: &TrackMonitor) {}
}

impl Track {
    pub fn audio(meta: TrackMeta, reverse: Option<crate::route::RouteHandle>) -> Self {
        Self::Audio(AudioTrack { meta, reverse })
    }

    pub fn video(
        meta: TrackMeta,
        layers: Vec<TrackLayer>,
        reverse: Option<crate::route::RouteHandle>,
    ) -> Self {
        Self::Video(VideoTrack {
            meta,
            stats: VideoStats::new(&layers),
            layers,
            reverse,
        })
    }

    pub fn data(
        meta: TrackMeta,
        topic: Topic,
        lane: DataLane,
        reverse: Option<crate::route::RouteHandle>,
    ) -> Self {
        Self::Data(DataTrack {
            meta,
            topic,
            lane,
            reverse,
        })
    }

    pub fn meta(&self) -> &TrackMeta {
        match self {
            Self::Audio(track) => &track.meta,
            Self::Video(track) => &track.meta,
            Self::Data(track) => &track.meta,
        }
    }

    pub fn meta_mut(&mut self) -> &mut TrackMeta {
        match self {
            Self::Audio(track) => &mut track.meta,
            Self::Video(track) => &mut track.meta,
            Self::Data(track) => &mut track.meta,
        }
    }

    pub fn id(&self) -> TrackId {
        self.meta().id
    }

    pub fn kind(&self) -> TrackKind {
        self.id().kind()
    }

    pub fn reverse(&self) -> Option<crate::route::RouteHandle> {
        match self {
            Self::Audio(track) => track.reverse,
            Self::Video(track) => track.reverse,
            Self::Data(track) => track.reverse,
        }
    }

    pub fn set_reverse(&mut self, reverse: Option<crate::route::RouteHandle>) {
        match self {
            Self::Audio(track) => track.reverse = reverse,
            Self::Video(track) => track.reverse = reverse,
            Self::Data(track) => track.reverse = reverse,
        }
    }

    pub fn publication_label(&self) -> Option<String> {
        match self {
            Self::Data(track) => Some(publication_label(track.lane, &track.topic)),
            Self::Audio(_) | Self::Video(_) => None,
        }
    }

    pub fn requires_reverse_route(&self) -> bool {
        matches!(
            self,
            Self::Video(_)
                | Self::Data(DataTrack {
                    lane: DataLane::Reliable,
                    ..
                })
        )
    }

    pub fn layers(&self) -> &[TrackLayer] {
        match self {
            Self::Video(track) => &track.layers,
            Self::Audio(_) | Self::Data(_) => &[],
        }
    }

    pub fn as_video(&self) -> Option<&VideoTrack> {
        match self {
            Self::Video(track) => Some(track),
            Self::Audio(_) | Self::Data(_) => None,
        }
    }

    pub fn as_video_mut(&mut self) -> Option<&mut VideoTrack> {
        match self {
            Self::Video(track) => Some(track),
            Self::Audio(_) | Self::Data(_) => None,
        }
    }

    pub(crate) fn stats(&self) -> Option<VideoStats> {
        self.as_video().map(|track| track.stats.clone())
    }

    pub fn lowest_quality(&self) -> Option<&TrackLayer> {
        self.layers().iter().min_by_key(|l| l.quality)
    }

    /// Lowest layer that is currently healthy, falling back to the absolute
    /// lowest when no layer is healthy yet. Prefer this over `lowest_quality`
    /// when staging an initial layer so the slot can actually receive a keyframe
    /// (an inactive layer never produces packets and the slot would stall).
    pub fn lowest_healthy_quality(
        &self,
        is_healthy: impl Fn(&TrackLayer) -> bool,
    ) -> Option<&TrackLayer> {
        self.layers()
            .iter()
            .filter(|l| is_healthy(l))
            .min_by_key(|l| l.quality)
            .or_else(|| self.lowest_quality())
    }

    pub fn by_quality(&self, quality: LayerQuality) -> Option<&TrackLayer> {
        self.layers().iter().find(|l| l.quality == quality)
    }

    pub fn higher_quality(&self, current: LayerQuality) -> Option<&TrackLayer> {
        self.layers()
            .iter()
            .filter(|l| l.quality > current)
            .min_by_key(|l| l.quality)
    }

    pub fn lower_quality(&self, current: LayerQuality) -> Option<&TrackLayer> {
        self.layers()
            .iter()
            .filter(|l| l.quality < current)
            .max_by_key(|l| l.quality)
    }
}

/// A track's shape as it crosses a shard or the control plane: no measurement
/// handles, so the controller never holds media-path state. Consumers get the
/// measurements separately, keyed by [`StreamId`].
#[derive(Clone, Debug)]
pub struct TrackLayer {
    pub meta: TrackMeta,
    pub rid: Option<Rid>,
    pub quality: LayerQuality,
}

/// The per-encoding measurement handles for one track.
///
/// Travels the media path only — participant to its shard, then shard to shard
/// — never through the controller.
pub type TrackStates = Vec<(Option<Rid>, StreamStats)>;

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
    debug_assert_eq!(meta.id.kind(), TrackKind::Audio);
    let bitrate = 64_000;
    let stream_state = StreamStats::new(true, bitrate, 0);
    let stream_id = format!("{}:_", meta.id);
    let monitor = StreamMonitor::new(meta.id.kind(), stream_id, stream_state);

    let sender = UpstreamTrack {
        meta: meta.clone(),
        synchronizer: TrackSynchronizer::new(rtp::AUDIO_FREQUENCY),
        monitor: TrackMonitor::new(vec![UpstreamTrackLayer {
            mid,
            rid: None,
            quality: LayerQuality::Low,
            normalizer: StreamNormalizer::new(mid, None),
            monitor,
        }]),
        stats: UpstreamStats::Audio(NullStats::new()),
    };
    (sender, Track::audio(meta, None))
}

/// Construct a new video track sender and its per-layer descriptors.
///
/// # Arguments
///
/// * `mid` - The Media Identifier associated with this video stream.
/// * `meta` - Metadata describing the track. `meta.kind` **must** be `Video`.
/// * `layers` - A vector of configurations defining the simulcast layers.
///
/// # Layer Quality Mapping
///
/// `LayerQuality` is derived from each layer's rid string
/// (`pulsebeam_core::simulcast::LayerQuality::from_rid`), never from vector position —
/// `"f"` → High, `"h"` → Medium, `"q"` → Low, anything else defaults to Low.
///
/// ### Sorting Post-Processing
/// After initialization, both the internal `UpstreamTrack` and `Track` layers are
/// **sorted in descending order** by their `LayerQuality` enum fields (`High -> Medium -> Low`),
/// regardless of the order `layers` was supplied in.
pub fn new_video(mid: Mid, meta: TrackMeta, layers: Vec<SimulcastLayer>) -> (UpstreamTrack, Track) {
    debug_assert_eq!(meta.id.kind(), TrackKind::Video);
    let simulcast_rids: Vec<Option<Rid>> = if layers.is_empty() {
        vec![None]
    } else {
        layers.iter().map(|l| Some(l.rid)).collect()
    };

    let mut senders = Vec::new();
    let mut layers = Vec::with_capacity(simulcast_rids.len());

    for &rid in &simulcast_rids {
        let quality = LayerQuality::from_rid(rid.as_deref());
        let bitrate = quality.seed_bitrate_bps();
        let fallback_height = quality.fallback_height();
        let stream_state = StreamStats::new(true, bitrate, fallback_height);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.id.kind(), stream_id, stream_state);

        senders.push(UpstreamTrackLayer {
            mid,
            rid,
            quality,
            normalizer: StreamNormalizer::new(mid, rid),
            monitor,
        });
        layers.push(TrackLayer {
            meta: meta.clone(),
            rid,
            quality,
        });
    }
    senders.sort_by_key(|e| std::cmp::Reverse(e.quality));
    layers.sort_by_key(|e| std::cmp::Reverse(e.quality));

    tracing::info!(track_id = ?meta.id, layers = ?layers.len(), "discovered video layers mapping");
    let stats = VideoStats::new(&layers);
    let track = Track::Video(VideoTrack {
        meta: meta.clone(),
        layers,
        stats: stats.clone(),
        reverse: None,
    });

    (
        UpstreamTrack {
            meta,
            synchronizer: TrackSynchronizer::new(rtp::VIDEO_FREQUENCY),
            monitor: TrackMonitor::new(senders),
            stats: UpstreamStats::Video(stats),
        },
        track,
    )
}

#[cfg(test)]
pub mod test_utils {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    pub fn make_video_track(
        participant_id: ParticipantId,
        mid: Mid,
        layers: Vec<SimulcastLayer>,
    ) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(TrackKind::Video, &mid);
        let meta = TrackMeta {
            room_id: crate::entity::RoomId::from_external(
                &crate::entity::ExternalRoomId::new("test-room").unwrap(),
            ),
            shard_id: ShardId::new(0),
            id: track_id,
            origin: participant_id,
        };
        crate::track::new_video(mid, meta, layers)
    }

    pub fn make_audio_track(participant_id: ParticipantId, mid: Mid) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(TrackKind::Audio, &mid);
        let meta = TrackMeta {
            room_id: crate::entity::RoomId::from_external(
                &crate::entity::ExternalRoomId::new("test-room").unwrap(),
            ),
            shard_id: ShardId::new(0),
            id: track_id,
            origin: participant_id,
        };
        crate::track::new_audio(mid, meta)
    }
}

mod data_track {
    use std::fmt::Display;

    use crate::entity::ParticipantId;
    use str0m::channel::{ChannelConfig, Reliability};

    const MAX_DATA_TRACK_NAMESPACE_LEN: usize = 96;

    pub const MAX_DATA_TOPIC_CHANNELS: usize = 64;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum DataTrackDirection {
        Publish,
        Subscribe,
    }

    impl Display for DataTrackDirection {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                DataTrackDirection::Publish => f.write_str("pub"),
                DataTrackDirection::Subscribe => f.write_str("sub"),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    /// A data-channel topic name.
    ///
    /// An owned `String`, deliberately. `Arc<str>` would make the two clones
    /// per cross-shard data frame refcount bumps instead of allocations, but a
    /// topic travels between shards on the control plane, so those bumps would
    /// land on a count another core holds — trading a core-local malloc for
    /// cross-core traffic, which is the wrong direction.
    ///
    /// The clones are not inherent: they exist only to build lookup keys, and a
    /// dense key in `RouteAction::Unreliable` removes them the way `TrackKey`
    /// did for video. Fix the cause, not the symptom.
    pub struct Topic(String);

    impl Topic {
        /// Production builds a `Topic` only by parsing a channel label; tests
        /// need one without going through the label grammar.
        #[cfg(test)]
        pub fn for_test(topic: &str) -> Self {
            Self(topic.to_string())
        }
    }

    impl AsRef<str> for Topic {
        #[inline]
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    impl Display for Topic {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::ops::Deref for Topic {
        type Target = str;

        #[inline]
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl std::borrow::Borrow<str> for Topic {
        #[inline]
        fn borrow(&self) -> &str {
            &self.0
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum DataLane {
        Realtime,
        Reliable,
    }

    impl DataLane {
        pub fn as_str(self) -> &'static str {
            match self {
                DataLane::Realtime => "rt",
                DataLane::Reliable => "rel",
            }
        }
    }

    /// The canonical label naming a data *publication*: a topic on a lane.
    ///
    /// Direction and scope describe a channel, not the thing published on it,
    /// so they are absent here — a publisher's channel and a subscriber's
    /// channel for the same topic name one publication between them.
    ///
    /// Injective without escaping, which is why this can be plain
    /// concatenation: `rt` and `rel` are prefix-free after `v1/`, and a topic
    /// is `[A-Za-z0-9_-]+` by the grammar above, so it can carry no separator.
    /// That is enforced where a label is parsed, not assumed here.
    pub fn publication_label(lane: DataLane, topic: &crate::track::Topic) -> String {
        format!("v1/{}/{}", lane.as_str(), topic)
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct DataTopicChannel {
        pub direction: DataTrackDirection,
        pub topic: crate::track::Topic,
        pub scope: Option<ParticipantId>,
        pub lane: DataLane,
    }

    impl Display for DataTopicChannel {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            debug_assert!(self.direction == DataTrackDirection::Subscribe || self.scope.is_none());
            write!(
                f,
                "v1/{}/{}/{}",
                self.lane.as_str(),
                self.direction,
                self.topic
            )?;
            if let Some(scope) = &self.scope {
                write!(f, "/{}", scope.as_str())?;
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum DataTrackIntent {
        InternalSignaling,
        UserTopic(DataTopicChannel),
    }

    #[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
    pub enum DataTrackIntentError {
        #[error("The input string exceeds the maximum permitted security boundary size")]
        LabelTooLong,

        #[error("Invalid or missing API version protocol prefix (expected 'v1')")]
        InvalidVersion,

        #[error("Invalid transport lane identifier (expected 'sys', 'rt', or 'rel')")]
        InvalidLane,

        #[error("Invalid routing direction parameter (expected 'pub' or 'sub')")]
        InvalidDirection,

        #[error("The target user asset label component is missing or empty")]
        MissingLabel,

        #[error(
            "Unsupported data channel configuration for label '{label}': expected unordered with MaxRetransmits(0), but got ordered={ordered}, reliability={reliability:?}"
        )]
        UnsupportedDataChannelConfig {
            label: String,
            ordered: bool,
            reliability: Reliability,
        },

        #[error(
            "The label contains illegal characters (only alphanumeric, dashes, and underscores allowed)"
        )]
        IllegalCharacters,

        #[error("Scoped subscribe requires a valid participant id, got: {0}")]
        InvalidScope(String),

        #[error("Publish channels cannot carry a publisher scope segment")]
        ScopeNotAllowedForPublish,

        #[error("Reliable subscribe channels cannot carry a publisher scope segment")]
        ScopeNotAllowedForReliableSubscribe,
    }

    impl TryFrom<&ChannelConfig> for DataTrackIntent {
        type Error = DataTrackIntentError;

        fn try_from(cfg: &ChannelConfig) -> Result<Self, Self::Error> {
            let s = &cfg.label;
            if s.len() > MAX_DATA_TRACK_NAMESPACE_LEN {
                return Err(DataTrackIntentError::LabelTooLong);
            }

            let mut parts = s.splitn(5, '/');

            if parts.next() != Some("v1") {
                return Err(DataTrackIntentError::InvalidVersion);
            }

            match parts.next() {
                Some("sys") => {
                    if parts.next() == Some("signaling") && parts.next().is_none() {
                        Ok(Self::InternalSignaling)
                    } else {
                        Err(DataTrackIntentError::InvalidDirection)
                    }
                }
                lane_str @ (Some("rt") | Some("rel")) => {
                    let lane = if lane_str == Some("rel") {
                        DataLane::Reliable
                    } else {
                        DataLane::Realtime
                    };

                    let supported_delivery_guarantee = match lane {
                        DataLane::Realtime => {
                            matches!(
                                cfg.reliability,
                                Reliability::MaxRetransmits { retransmits: 0 }
                            ) && !cfg.ordered
                        }
                        DataLane::Reliable => {
                            matches!(cfg.reliability, Reliability::Reliable) && cfg.ordered
                        }
                    };
                    if !supported_delivery_guarantee {
                        return Err(DataTrackIntentError::UnsupportedDataChannelConfig {
                            label: s.clone(),
                            ordered: cfg.ordered,
                            reliability: cfg.reliability,
                        });
                    }

                    let direction = match parts.next() {
                        Some("pub") => DataTrackDirection::Publish,
                        Some("sub") => DataTrackDirection::Subscribe,
                        _ => return Err(DataTrackIntentError::InvalidDirection),
                    };

                    let topic_slice = parts.next().ok_or(DataTrackIntentError::MissingLabel)?;
                    if topic_slice.is_empty() {
                        return Err(DataTrackIntentError::MissingLabel);
                    }

                    let is_valid = topic_slice
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');

                    if !is_valid {
                        return Err(DataTrackIntentError::IllegalCharacters);
                    }

                    let scope_slice = parts.next();
                    let scope = match (lane, direction, scope_slice) {
                        (_, DataTrackDirection::Publish, Some(_)) => {
                            return Err(DataTrackIntentError::ScopeNotAllowedForPublish);
                        }
                        (_, DataTrackDirection::Publish | DataTrackDirection::Subscribe, None) => {
                            None
                        }
                        (DataLane::Reliable, DataTrackDirection::Subscribe, Some(_)) => {
                            return Err(DataTrackIntentError::ScopeNotAllowedForReliableSubscribe);
                        }
                        (_, DataTrackDirection::Subscribe, Some(raw)) => {
                            if raw.is_empty() {
                                return Err(DataTrackIntentError::InvalidScope(raw.to_string()));
                            }
                            Some(
                                ParticipantId::try_from(raw.to_string()).map_err(|_| {
                                    DataTrackIntentError::InvalidScope(raw.to_string())
                                })?,
                            )
                        }
                    };

                    let topic = DataTopicChannel {
                        direction,
                        topic: Topic(topic_slice.to_string()),
                        scope,
                        lane,
                    };
                    Ok(Self::UserTopic(topic))
                }
                _ => Err(DataTrackIntentError::InvalidLane),
            }
        }
    }

    #[cfg(test)]
    mod test {
        // Convenience only: a test is not a shard, so nothing here is
        // cross-core, and a fixture may read the host clock. Allowed at the
        // module, never the file, so it cannot drift over production code
        // sharing it. See crates/pulsebeam/docs/thread-per-core.md.
        use std::ops::Deref;

        use super::*;
        use pulsebeam_runtime::rand::RngCore;

        fn test_rng() -> impl RngCore {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(1);
            pulsebeam_runtime::rand::seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed))
        }

        fn cfg(label: &str) -> ChannelConfig {
            ChannelConfig {
                label: label.to_string(),
                ordered: false,
                reliability: Reliability::MaxRetransmits { retransmits: 0 },
                negotiated: None,
                protocol: "".to_string(),
            }
        }

        fn rel_cfg(label: &str) -> ChannelConfig {
            ChannelConfig {
                label: label.to_string(),
                ordered: true,
                reliability: Reliability::Reliable,
                negotiated: None,
                protocol: "".to_string(),
            }
        }

        #[test]
        fn test_modern_system_routing() {
            let res = DataTrackIntent::try_from(&cfg("v1/sys/signaling")).unwrap();
            assert!(matches!(res, DataTrackIntent::InternalSignaling));
        }

        #[test]
        fn test_invalid_system_channels() {
            // Unknown system channel
            let err = DataTrackIntent::try_from(&cfg("v1/sys/metrics")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::InvalidDirection);

            // Malformed layout trailing after signaling
            let err = DataTrackIntent::try_from(&cfg("v1/sys/signaling/extra")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::InvalidDirection);
        }

        #[test]
        fn test_valid_user_topics() {
            let res = DataTrackIntent::try_from(&cfg("v1/rt/pub/game-sync")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Publish);
                assert_eq!(e.topic.deref(), "game-sync");
                assert_eq!(e.lane, DataLane::Realtime);
            } else {
                panic!("Expected UserTopic variant");
            }

            let res = DataTrackIntent::try_from(&cfg("v1/rt/sub/audio_stream_12")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Subscribe);
                assert_eq!(e.topic.deref(), "audio_stream_12");
                assert_eq!(e.scope, None);
                assert_eq!(e.lane, DataLane::Realtime);
            } else {
                panic!("Expected UserTopic variant");
            }
        }

        #[test]
        fn test_reliable_publish() {
            let res = DataTrackIntent::try_from(&rel_cfg("v1/rel/pub/chat")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Publish);
                assert_eq!(e.topic.deref(), "chat");
                assert_eq!(e.lane, DataLane::Reliable);
                assert_eq!(e.scope, None);
            } else {
                panic!("Expected UserTopic variant");
            }
        }

        #[test]
        fn test_reliable_subscribe_is_topic_wide() {
            let res = DataTrackIntent::try_from(&rel_cfg("v1/rel/sub/chat")).unwrap();
            let DataTrackIntent::UserTopic(channel) = res else {
                panic!("Expected UserTopic variant");
            };
            assert_eq!(channel.direction, DataTrackDirection::Subscribe);
            assert_eq!(channel.topic.deref(), "chat");
            assert_eq!(channel.lane, DataLane::Reliable);
            assert_eq!(channel.scope, None);
        }

        #[test]
        fn test_reliable_subscribe_rejects_publisher_scope() {
            let _rng = test_rng();
            let publisher_id = ParticipantId::new();
            let label = format!("v1/rel/sub/chat/{}", publisher_id.as_str());
            let err = DataTrackIntent::try_from(&rel_cfg(&label)).unwrap_err();
            assert_eq!(
                err,
                DataTrackIntentError::ScopeNotAllowedForReliableSubscribe
            );
        }

        #[test]
        fn test_reliable_wrong_channel_config() {
            let err = DataTrackIntent::try_from(&cfg("v1/rel/pub/chat")).unwrap_err();
            assert!(matches!(
                err,
                DataTrackIntentError::UnsupportedDataChannelConfig { .. }
            ));
        }

        #[test]
        fn test_reliable_display() {
            let pub_ch = DataTopicChannel {
                direction: DataTrackDirection::Publish,
                topic: Topic("chat".to_string()),
                scope: None,
                lane: DataLane::Reliable,
            };
            assert_eq!(pub_ch.to_string(), "v1/rel/pub/chat");

            let sub_ch = DataTopicChannel {
                direction: DataTrackDirection::Subscribe,
                topic: Topic("chat".to_string()),
                scope: None,
                lane: DataLane::Reliable,
            };
            assert_eq!(sub_ch.to_string(), "v1/rel/sub/chat");
        }

        #[test]
        fn test_scoped_subscribe_valid() {
            let _rng = test_rng();
            let participant_id = ParticipantId::new();
            let label = format!("v1/rt/sub/game-sync/{}", participant_id.as_str());
            let res = DataTrackIntent::try_from(&cfg(&label)).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Subscribe);
                assert_eq!(e.topic.deref(), "game-sync");
                assert_eq!(e.scope, Some(participant_id));
            } else {
                panic!("Expected UserTopic variant");
            }
        }

        #[test]
        fn test_scoped_publish_rejected() {
            let _rng = test_rng();
            let participant_id = ParticipantId::new();
            let label = format!("v1/rt/pub/game-sync/{}", participant_id.as_str());
            let err = DataTrackIntent::try_from(&cfg(&label)).unwrap_err();
            assert_eq!(err, DataTrackIntentError::ScopeNotAllowedForPublish);
        }

        #[test]
        fn test_scoped_subscribe_invalid_scope() {
            let err = DataTrackIntent::try_from(&cfg("v1/rt/sub/game-sync/not-a-participant-id"))
                .unwrap_err();
            assert!(matches!(err, DataTrackIntentError::InvalidScope(_)));
        }

        #[test]
        fn test_scoped_subscribe_trailing_garbage() {
            let _rng = test_rng();
            let participant_id = ParticipantId::new();
            let label = format!("v1/rt/sub/game-sync/{}/trailing", participant_id.as_str());
            let err = DataTrackIntent::try_from(&cfg(&label)).unwrap_err();
            assert!(matches!(err, DataTrackIntentError::InvalidScope(_)));
        }

        #[test]
        fn test_invalid_version_and_lane() {
            // Bad version prefix
            let err = DataTrackIntent::try_from(&cfg("v2/rt/pub/topic")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::InvalidVersion);

            // Unknown lane (neither sys nor rt)
            let err = DataTrackIntent::try_from(&cfg("v1/data/pub/topic")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::InvalidLane);
        }

        #[test]
        fn test_invalid_direction() {
            let err = DataTrackIntent::try_from(&cfg("v1/rt/broadcast/topic")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::InvalidDirection);
        }

        #[test]
        fn test_missing_or_empty_label() {
            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::MissingLabel);

            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::MissingLabel);
        }

        #[test]
        fn test_illegal_characters() {
            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/game/engine")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::ScopeNotAllowedForPublish);

            // Spaces and symbols
            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/my topic")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::IllegalCharacters);

            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/topic$")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::IllegalCharacters);
        }

        #[test]
        fn test_max_length_boundary() {
            let exact_valid = format!("v1/rt/pub/{}", "a".repeat(86));
            assert!(DataTrackIntent::try_from(&cfg(&exact_valid)).is_ok());

            // 1 byte over limit
            let one_byte_over = format!("v1/rt/pub/{}", "a".repeat(87));
            let err = DataTrackIntent::try_from(&cfg(&one_byte_over)).unwrap_err();
            assert_eq!(err, DataTrackIntentError::LabelTooLong);
        }
    }
}

#[cfg(test)]
mod dd_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use pulsebeam_core::dd::{
        DependencyDescriptor, DependencyDescriptorWriter, MAX_DD_LEN, test_utils,
    };

    fn layer() -> UpstreamTrackLayer {
        let state = StreamStats::new(true, 500_000, 360);
        UpstreamTrackLayer {
            mid: Mid::from("0"),
            rid: None,
            quality: LayerQuality::High,
            normalizer: StreamNormalizer::new(Mid::from("0"), None),
            monitor: StreamMonitor::new(TrackKind::Video, "test".to_string(), state),
        }
    }

    fn packet_carrying(bytes: &[u8]) -> RtpPacket {
        let mut pkt = RtpPacket::default();
        pkt.ext_vals
            .user_values
            .set(pulsebeam_core::dd::RawDependencyDescriptor(
                bytes.iter().copied().collect(),
            ));
        pkt
    }

    #[test]
    fn attaches_parsed_descriptor_to_ingress_packets() {
        let structure = test_utils::structure_l1t3();
        let mut writer = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];
        let mut layer = layer();

        let len = writer
            .write(&test_utils::keyframe(&structure), &mut buf)
            .unwrap();
        let mut pkt = packet_carrying(&buf[..len]);
        assert!(layer.process(&mut pkt));
        assert!(
            pkt.ext_vals
                .user_values
                .get::<DependencyDescriptor>()
                .is_some()
        );

        // A later delta frame carries only a template id, so it is decodable
        // only because the layer retained the structure.
        let sent = test_utils::delta(&structure, 2, 7);
        let len = writer.write(&sent, &mut buf).unwrap();
        let mut pkt = packet_carrying(&buf[..len]);
        assert!(layer.process(&mut pkt));

        let got = pkt.ext_vals.user_values.get::<DependencyDescriptor>();
        assert_eq!(got, Some(&sent));
        assert_eq!(layer.normalizer.dd_errors(), 0);
    }

    #[test]
    fn learns_decode_target_count_from_a_scalable_keyframe() {
        let structure = test_utils::structure_l1t3(); // three decode targets
        let mut writer = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];
        let mut layer = layer();
        assert_eq!(
            layer.monitor.stats().decode_target_count(),
            1,
            "no structure seen yet, so the encoding is one indivisible rung"
        );

        let len = writer
            .write(&test_utils::keyframe(&structure), &mut buf)
            .unwrap();
        let mut pkt = packet_carrying(&buf[..len]);
        layer.process(&mut pkt);

        assert_eq!(
            layer.monitor.stats().decode_target_count(),
            structure.decode_target_count,
            "the scalable keyframe's decode-target count is published for the allocator"
        );
    }

    #[test]
    fn derives_the_keyframe_flag_from_the_descriptor_under_an_opaque_payload() {
        // packet_carrying starts from an opaque payload (is_keyframe = false, empty
        // NAL flags) — the SFrame/E2EE case where from_str0m's H.264 probe sees
        // nothing. The descriptor's attached structure is then the only keyframe
        // signal, so ingress must set is_keyframe from it.
        let structure = test_utils::structure_l1t3();
        let mut writer = DependencyDescriptorWriter::new();
        let mut buf = [0u8; MAX_DD_LEN];
        let mut layer = layer();

        let len = writer
            .write(&test_utils::keyframe(&structure), &mut buf)
            .unwrap();
        let mut kf = packet_carrying(&buf[..len]);
        assert!(
            !kf.is_keyframe,
            "opaque payload gives no keyframe signal on its own"
        );
        layer.process(&mut kf);
        assert!(
            kf.is_keyframe,
            "the descriptor's structure marks the keyframe"
        );

        let len = writer
            .write(&test_utils::delta(&structure, 1, 1), &mut buf)
            .unwrap();
        let mut delta = packet_carrying(&buf[..len]);
        layer.process(&mut delta);
        assert!(
            !delta.is_keyframe,
            "a descriptor without a structure is a delta frame"
        );
    }

    #[test]
    fn drops_a_malformed_descriptor_before_learning_a_template() {
        let mut layer = layer();
        let mut pkt = packet_carrying(&[0xff; 12]);

        assert!(!layer.process(&mut pkt));
        assert!(
            pkt.ext_vals
                .user_values
                .get::<DependencyDescriptor>()
                .is_none()
        );
        assert_eq!(layer.normalizer.dd_errors(), 1);
    }

    #[test]
    fn packets_without_a_descriptor_are_untouched() {
        let mut layer = layer();
        let mut pkt = RtpPacket::default();

        assert!(layer.process(&mut pkt));
        assert_eq!(layer.normalizer.dd_errors(), 0);
    }
}

#[cfg(test)]
mod vla_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::vla_stream_target_bps;
    use str0m::rtp::vla::{
        SimulcastStreamAllocation, SpatialLayerAllocation, TemporalLayerAllocation,
        VideoLayersAllocation,
    };

    fn stream(cumulative_kbps: &[u64]) -> SimulcastStreamAllocation {
        SimulcastStreamAllocation {
            spatial_layers: vec![SpatialLayerAllocation {
                temporal_layers: cumulative_kbps
                    .iter()
                    .map(|&c| TemporalLayerAllocation { cumulative_kbps: c })
                    .collect(),
                resolution_and_framerate: None,
            }],
        }
    }

    #[test]
    fn target_is_top_cumulative_temporal_rate() {
        let vla = VideoLayersAllocation {
            current_simulcast_stream_index: 1,
            simulcast_streams: vec![stream(&[100, 150]), stream(&[300, 500, 800])],
        };
        assert_eq!(vla_stream_target_bps(&vla, 0), Some(150_000));
        assert_eq!(vla_stream_target_bps(&vla, 1), Some(800_000));
    }

    #[test]
    fn temporal_ladder_and_framerate_flow_into_stream_state() {
        use super::{vla_stream_framerate, vla_stream_temporal_cumulative_kbps};
        use crate::rtp::monitor::StreamStats;
        use str0m::rtp::vla::ResolutionAndFramerate;

        let mut s = stream(&[300, 450, 600]);
        s.spatial_layers
            .first_mut()
            .expect("fixture has a spatial layer")
            .resolution_and_framerate = Some(ResolutionAndFramerate {
            width: 1280,
            height: 720,
            framerate: 30,
        });
        let vla = VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: vec![s],
        };

        assert_eq!(
            vla_stream_temporal_cumulative_kbps(&vla, 0),
            vec![300, 450, 600]
        );
        assert_eq!(vla_stream_framerate(&vla, 0), Some(30));

        let mut state = StreamStats::new(false, 0, 720);
        state.set_temporal_ladder(&vla_stream_temporal_cumulative_kbps(&vla, 0), 30);
        assert_eq!(state.decode_target_bps(0), 300_000);
        assert_eq!(state.decode_target_bps(1), 450_000);
        assert_eq!(state.decode_target_bps(2), 600_000);
        assert_eq!(state.full_fps(), 30);
    }

    #[test]
    fn inactive_or_missing_stream_has_no_target() {
        let vla = VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: vec![SimulcastStreamAllocation {
                spatial_layers: vec![],
            }],
        };
        assert_eq!(vla_stream_target_bps(&vla, 0), None);
        assert_eq!(vla_stream_target_bps(&vla, 5), None);
    }

    #[test]
    fn height_is_tallest_declared_spatial_layer_or_none() {
        use str0m::rtp::vla::ResolutionAndFramerate;
        let with_height = |h: u16| SimulcastStreamAllocation {
            spatial_layers: vec![SpatialLayerAllocation {
                temporal_layers: vec![TemporalLayerAllocation {
                    cumulative_kbps: 500,
                }],
                resolution_and_framerate: Some(ResolutionAndFramerate {
                    width: 640,
                    height: h,
                    framerate: 30,
                }),
            }],
        };
        let vla = VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: vec![with_height(360), stream(&[100])],
        };
        assert_eq!(super::vla_stream_height_px(&vla, 0), Some(360));
        // Stream 1 carries no resolution → fall back to the height guess.
        assert_eq!(super::vla_stream_height_px(&vla, 1), None);
    }
}

#[cfg(test)]
mod simulcast_pause_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::entity::ParticipantId;
    use crate::rtp::RtpPacket;
    use std::time::Duration;
    use str0m::media::SimulcastLayer;

    /// A three-encoding video track and a starting instant.
    fn track() -> (UpstreamTrack, Instant) {
        let now = Instant::now();
        let participant = ParticipantId::new();
        let (upstream, _) = test_utils::make_video_track(
            participant,
            Mid::from("v"),
            vec![
                SimulcastLayer::new("q"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("f"),
            ],
        );
        (upstream, now)
    }

    fn feed(upstream: &mut UpstreamTrack, rid: &str, at: Instant) {
        let pkt = RtpPacket {
            arrival_ts: at,
            ..Default::default()
        };
        let _ = upstream.monitor.process(Some(&Rid::from(rid)), pkt);
    }

    fn inactive(upstream: &UpstreamTrack) -> Vec<bool> {
        upstream
            .monitor
            .layer_states()
            .iter()
            .map(|(_, s)| s.is_inactive())
            .collect()
    }

    /// The whole track going silent must settle, not oscillate.
    ///
    /// The sibling gate used to read the very `inactive` flags that `poll`
    /// writes: all encodings silent paused them together, which then read back
    /// as "no sibling active" and un-paused them all on the next tick. That ran
    /// at the poll rate for the entire window between the pause and dead
    /// timeouts, flapping every subscriber's allocation with it.
    #[test]
    fn an_entirely_silent_track_does_not_flap_its_encodings() {
        let (mut upstream, start) = track();
        for rid in ["q", "h", "f"] {
            feed(&mut upstream, rid, start);
        }

        // Poll across the whole pause..dead window, and past it, at a realistic tick.
        let mut transitions = 0usize;
        let mut previous = inactive(&upstream);
        for tick in 1..=40u32 {
            let now = start + Duration::from_millis(100) * tick;
            upstream.poll_stats(now);
            let current = inactive(&upstream);
            transitions += previous
                .iter()
                .zip(&current)
                .filter(|(a, b)| a != b)
                .count();
            previous = current;
        }

        assert!(
            transitions <= 3,
            "a silent track must settle into inactive once per encoding, saw {transitions} \
             active/inactive transitions across 3 encodings"
        );
        assert!(
            inactive(&upstream).iter().all(|&i| i),
            "every encoding of a silent track must end up inactive"
        );
    }

    /// The gate still does its real job: a layer the sender dropped while other
    /// encodings keep sending is paused promptly, without waiting out the much
    /// longer dead timeout.
    #[test]
    fn a_layer_dropped_while_siblings_send_is_paused() {
        let (mut upstream, start) = track();
        for rid in ["q", "h", "f"] {
            feed(&mut upstream, rid, start);
        }

        // "f" goes quiet; "q" and "h" keep sending past the pause timeout.
        for tick in 1..=25u32 {
            let now = start + Duration::from_millis(100) * tick;
            feed(&mut upstream, "q", now);
            feed(&mut upstream, "h", now);
            upstream.poll_stats(now);
        }

        let states = upstream.monitor.layer_states();
        let state_of = |rid: &str| {
            states
                .iter()
                .find(|(r, _)| r.as_deref() == Some(rid))
                .map(|(_, s)| s.is_inactive())
                .unwrap()
        };
        assert!(state_of("f"), "the dropped layer must be paused");
        assert!(!state_of("q"), "a sending layer must stay active");
        assert!(!state_of("h"), "a sending layer must stay active");
    }
}
