use std::collections::VecDeque;
use std::fmt::{Debug, Display};
use std::time::Duration;

use crate::entity::TrackId;
use crate::entity::{ParticipantId, TrackKind};
use crate::id::ShardId;
use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};
pub use data_track::*;
use pulsebeam_core::dd::{DependencyDescriptorReader, RawDependencyDescriptor};
pub use pulsebeam_core::simulcast::LayerQuality;
use str0m::media::{KeyframeRequestKind, Mid, Pt, Rid, SimulcastLayer};
use str0m::rtp::Ssrc;
use str0m::rtp::rtcp::SenderInfo;
use tokio::time::Instant;

pub type StreamId = (TrackId, Option<Rid>);

/// Leading-edge debounce interval for keyframe requests forwarded upstream.
pub const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(500);
pub const MAX_SIMULCAST_LAYERS: usize = 3;

#[derive(Debug, Clone)]
pub struct GlobalKeyframeRequest {
    pub shard_id: ShardId,
    pub origin: ParticipantId,
    pub stream_id: StreamId,
    pub kind: KeyframeRequestKind,
}

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
    /// The shard ID that hosts this track's publisher.
    pub shard_id: ShardId,
    pub id: crate::entity::TrackId,
    pub origin: crate::entity::ParticipantId,
}

#[derive(Debug)]
pub struct UpstreamTrackLayer {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub quality: LayerQuality,
    pub monitor: StreamMonitor,
    synchronizer: Synchronizer,
    /// The Video Layers Allocation simulcast-stream index this layer is sent on,
    /// learned from its own packets. Used to read this layer's state out of a
    /// VLA carried on any sibling's packet.
    vla_index: Option<u8>,
    /// Per-RTP-stream dependency descriptor state; templates only arrive on
    /// keyframes and are referenced by later packets.
    dd: DependencyDescriptorReader,
    dd_errors: u64,
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
        self.monitor.process_packet(pkt);
        // Learn which VLA simulcast-stream index this layer is sent on, so a VLA
        // carried on any sibling's packet can address this layer's state.
        if let Some(vla) = pkt
            .ext_vals
            .user_values
            .get::<str0m::rtp::vla::VideoLayersAllocation>()
        {
            self.vla_index = Some(vla.current_simulcast_stream_index);
        }
        self.parse_dependency_descriptor(pkt);
        // audio will only be filtered at the centralized audio_selector
        true
    }

    fn parse_dependency_descriptor(&mut self, pkt: &mut RtpPacket) {
        let Some(raw) = pkt.ext_vals.user_values.get::<RawDependencyDescriptor>() else {
            return;
        };
        match self.dd.read(&raw.0) {
            Ok(dd) => pkt.ext_vals.user_values.set_arc(std::sync::Arc::new(dd)),
            Err(err) => {
                self.dd_errors += 1;
                if self.dd_errors.is_power_of_two() {
                    tracing::warn!(
                        mid = %self.mid,
                        rid = ?self.rid,
                        errors = self.dd_errors,
                        %err,
                        "dependency descriptor parse failed"
                    );
                }
            }
        }
    }

    /// Apply a (track-wide) Video Layers Allocation to this layer using its
    /// learned stream index: the sender's declared target bitrate, resolution,
    /// and active/inactive state.
    fn apply_vla(&mut self, vla: &str0m::rtp::vla::VideoLayersAllocation) {
        let Some(idx) = self.vla_index.map(usize::from) else {
            return;
        };
        let target_bps = vla_stream_target_bps(vla, idx).unwrap_or(0);
        let height = vla_stream_height_px(vla, idx).map(u32::from);
        let first_declaration = self.monitor.apply_vla(target_bps, height);
        if first_declaration {
            tracing::info!(
                mid = %self.mid,
                rid = ?self.rid,
                target_kbps = target_bps / 1000,
                height = self.monitor.shared_state().height(),
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
        let processed = self
            .layers
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always be available")
            .process(packet, sr);

        // A VLA on any layer's packet describes every simulcast stream; push it
        // to every layer whose stream index we've already learned.
        if let Some(vla) = packet
            .ext_vals
            .user_values
            .get::<str0m::rtp::vla::VideoLayersAllocation>()
        {
            for layer in &mut self.layers {
                layer.apply_vla(vla);
            }
        }
        processed
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

    /// Lowest layer that is currently healthy, falling back to the absolute
    /// lowest when no layer is healthy yet. Prefer this over `lowest_quality`
    /// when staging an initial layer so the slot can actually receive a keyframe
    /// (an inactive layer never produces packets and the slot would stall).
    pub fn lowest_healthy_quality(&self) -> &TrackLayer {
        self.layers
            .iter()
            .filter(|l| l.state.is_healthy())
            .min_by_key(|l| l.quality)
            .unwrap_or_else(|| self.lowest_quality())
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
    debug_assert_eq!(meta.id.kind(), TrackKind::Audio);
    let bitrate = 64_000;
    let stream_state = StreamState::new(true, bitrate);
    let stream_id = format!("{}:_", meta.id);
    let monitor = StreamMonitor::new(meta.id.kind(), stream_id, stream_state.clone());

    let sender = UpstreamTrack {
        meta: meta.clone(),
        layers: vec![UpstreamTrackLayer {
            mid,
            rid: None,
            quality: LayerQuality::Low,
            synchronizer: Synchronizer::new(rtp::AUDIO_FREQUENCY),
            monitor,
            vla_index: None,
            dd: DependencyDescriptorReader::new(),
            dd_errors: 0,
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
        let stream_state = StreamState::new_with_height(true, bitrate, fallback_height);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.id.kind(), stream_id, stream_state.clone());

        senders.push(UpstreamTrackLayer {
            mid,
            rid,
            quality,
            synchronizer: Synchronizer::new(rtp::VIDEO_FREQUENCY),
            monitor,
            vla_index: None,
            dd: DependencyDescriptorReader::new(),
            dd_errors: 0,
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
        let track_id = participant_id.derive_track_id(TrackKind::Video, &mid);
        let meta = TrackMeta {
            shard_id: ShardId::new(0),
            id: track_id,
            origin: participant_id,
        };
        crate::track::new_video(mid, meta, layers)
    }

    pub fn make_audio_track(participant_id: ParticipantId, mid: Mid) -> (UpstreamTrack, Track) {
        let track_id = participant_id.derive_track_id(TrackKind::Audio, &mid);
        let meta = TrackMeta {
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
    pub struct Topic(String);

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
        fn as_str(self) -> &'static str {
            match self {
                DataLane::Realtime => "rt",
                DataLane::Reliable => "rel",
            }
        }
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
                        (_, DataTrackDirection::Publish, None) => None,
                        (_, DataTrackDirection::Subscribe, None) => None,
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
            let mut rng = test_rng();
            let publisher_id = ParticipantId::new(&mut rng);
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
            let mut rng = test_rng();
            let participant_id = ParticipantId::new(&mut rng);
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
            let mut rng = test_rng();
            let participant_id = ParticipantId::new(&mut rng);
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
            let mut rng = test_rng();
            let participant_id = ParticipantId::new(&mut rng);
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
    use super::*;
    use pulsebeam_core::dd::{
        DependencyDescriptor, DependencyDescriptorWriter, MAX_DD_LEN, test_utils,
    };

    fn layer() -> UpstreamTrackLayer {
        let state = StreamState::new_with_height(true, 500_000, 360);
        UpstreamTrackLayer {
            mid: Mid::from("0"),
            rid: None,
            quality: LayerQuality::High,
            monitor: StreamMonitor::new(TrackKind::Video, "test".to_string(), state),
            synchronizer: Synchronizer::new(rtp::VIDEO_FREQUENCY),
            vla_index: None,
            dd: DependencyDescriptorReader::new(),
            dd_errors: 0,
        }
    }

    fn packet_carrying(bytes: &[u8]) -> RtpPacket {
        let mut pkt = RtpPacket::default();
        pkt.ext_vals
            .user_values
            .set(RawDependencyDescriptor(bytes.iter().copied().collect()));
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
        assert!(layer.process(&mut pkt, None));
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
        assert!(layer.process(&mut pkt, None));

        let got = pkt.ext_vals.user_values.get::<DependencyDescriptor>();
        assert_eq!(got, Some(&sent));
        assert_eq!(layer.dd_errors, 0);
    }

    #[test]
    fn forwards_packets_despite_malformed_descriptor() {
        let mut layer = layer();
        let mut pkt = packet_carrying(&[0xff; 12]);

        assert!(layer.process(&mut pkt, None));
        assert!(
            pkt.ext_vals
                .user_values
                .get::<DependencyDescriptor>()
                .is_none()
        );
        assert_eq!(layer.dd_errors, 1);
    }

    #[test]
    fn packets_without_a_descriptor_are_untouched() {
        let mut layer = layer();
        let mut pkt = RtpPacket::default();

        assert!(layer.process(&mut pkt, None));
        assert_eq!(layer.dd_errors, 0);
    }
}

#[cfg(test)]
mod vla_tests {
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
