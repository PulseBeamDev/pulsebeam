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
use str0m::media::{KeyframeRequestKind, Mid, Pt, Rid, SimulcastLayer};
use str0m::rtp::RtpWrite;
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

pub struct StreamWriter<'a>(pub &'a mut str0m::Rtc);

impl<'a> StreamWriter<'a> {
    pub fn write_video_owned(&mut self, pkt: RtpPacket, mid: Mid, rid: Option<Rid>, pt: Pt) {
        let mut api = self.0.direct_api();
        let Some(stream) = api.stream_tx_by_mid(mid, rid) else {
            tracing::warn!(
                target: crate::log::TARGET_VIDEO,
                %mid, ?rid,
                "no stream_tx_by_mid found"
            );
            return;
        };
        let ssrc = stream.ssrc();
        tracing::trace!(
            target: crate::log::TARGET_VIDEO,
            %mid, ?rid, %ssrc, %pt, seq = %pkt.seq_no, len = pkt.payload.len(), marker = pkt.marker, "Writing RTP packet");
        let rtp = RtpWrite::new(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.payload,
        )
        .nackable(true)
        .marker(pkt.marker)
        .ext_vals(pkt.ext_vals);
        stream.write_rtp(rtp);
    }

    pub fn write_audio_owned(&mut self, pkt: RtpPacket, mid: Mid, pt: Pt) {
        let mut api = self.0.direct_api();
        let Some(stream) = api.stream_tx_by_mid(mid, None) else {
            tracing::warn!(
                target: crate::log::TARGET_AUDIO,
                %mid,
                "no stream_tx_by_mid found"
            );
            return;
        };
        let ssrc = stream.ssrc();
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            %mid, %ssrc, %pt, seq = %pkt.seq_no, ts = pkt.rtp_ts.numer(), len = pkt.payload.len(), marker = pkt.marker, "Writing RTP packet");

        let rtp = RtpWrite::new(
            pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.payload,
        )
        .nackable(false)
        .marker(pkt.marker)
        .ext_vals(pkt.ext_vals);
        stream.write_rtp(rtp);
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
/// # Layer Index Mapping Behavior
///
/// This constructor assigns bitrates and `LayerQuality` profiles positionally based on
/// the insertion order of the `layers` parameter:
///
/// | Vector Index | Layer Quality | Target Bitrate | Typical Target RID |
/// | :--- | :--- | :--- | :--- |
/// | `0` | `LayerQuality::High` | 1,250,000 bits/s | `"f"` (Full) |
/// | `1` | `LayerQuality::Medium` | 400,000 bits/s | `"h"` (Half) |
/// | `2+` or Empty | `LayerQuality::Low` | 150,000 bits/s | `"q"` (Quarter) |
///
/// ### Sorting Post-Processing
/// After initialization, both the internal `UpstreamTrack` and `Track` layers are
/// **sorted in descending order** by their `LayerQuality` enum fields (`High -> Medium -> Low`).
pub fn new_video(mid: Mid, meta: TrackMeta, layers: Vec<SimulcastLayer>) -> (UpstreamTrack, Track) {
    debug_assert_eq!(meta.id.kind(), TrackKind::Video);
    let simulcast_rids: Vec<Option<Rid>> = if layers.is_empty() {
        vec![None]
    } else {
        layers.iter().map(|l| Some(l.rid)).collect()
    };

    let mut senders = Vec::new();
    let mut layers = Vec::with_capacity(simulcast_rids.len());

    for (index, &rid) in simulcast_rids.iter().enumerate() {
        let (bitrate, quality) = match (rid, index) {
            (None, _) => (150_000, LayerQuality::Low),
            (Some(_), 0) => (1_250_000, LayerQuality::High),
            (Some(_), 1) => (400_000, LayerQuality::Medium),
            (Some(_), _) => (150_000, LayerQuality::Low),
        };

        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(meta.id.kind(), stream_id, stream_state.clone());

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

    use str0m::channel::{ChannelConfig, Reliability};

    const MAX_DATA_TRACK_NAMESPACE_LEN: usize = 64;

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
    pub enum DeliveryClass {
        Lossy,
        SemiReliable,
        Reliable,
    }

    impl From<&ChannelConfig> for DeliveryClass {
        fn from(cfg: &ChannelConfig) -> Self {
            match cfg.reliability {
                Reliability::Reliable => Self::Reliable,
                Reliability::MaxRetransmits { retransmits: 0 } => Self::Lossy,
                Reliability::MaxRetransmits { .. } | Reliability::MaxPacketLifetime { .. } => {
                    Self::SemiReliable
                }
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct DataTopicChannel {
        pub direction: DataTrackDirection,
        pub topic: crate::track::Topic,
        pub delivery: DeliveryClass,
    }

    impl Display for DataTopicChannel {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "v1/rt/{}/{}", self.direction, self.topic)
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

        #[error("Invalid transport lane identifier (expected 'sys' or 'rt')")]
        InvalidLane,

        #[error("Invalid routing direction parameter (expected 'pub' or 'sub')")]
        InvalidDirection,

        #[error("The target user asset label component is missing or empty")]
        MissingLabel,

        #[error(
            "The label contains illegal characters (only alphanumeric, dashes, and underscores allowed)"
        )]
        IllegalCharacters,
    }

    impl TryFrom<&ChannelConfig> for DataTrackIntent {
        type Error = DataTrackIntentError;

        fn try_from(cfg: &ChannelConfig) -> Result<Self, Self::Error> {
            let s = &cfg.label;
            if s.len() > MAX_DATA_TRACK_NAMESPACE_LEN {
                return Err(DataTrackIntentError::LabelTooLong);
            }

            let mut parts = s.splitn(4, '/');

            if parts.next() != Some("v1") {
                return Err(DataTrackIntentError::InvalidVersion);
            }

            // Branch cleanly based on the lane type (sys vs rt)
            match parts.next() {
                Some("sys") => {
                    if parts.next() == Some("signaling") && parts.next().is_none() {
                        Ok(Self::InternalSignaling)
                    } else {
                        Err(DataTrackIntentError::InvalidDirection)
                    }
                }
                Some("rt") => {
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

                    let topic = DataTopicChannel {
                        direction,
                        topic: Topic(topic_slice.to_string()),
                        delivery: DeliveryClass::from(cfg),
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
        fn cfg(label: &str) -> ChannelConfig {
            ChannelConfig {
                label: label.to_string(),
                ordered: false,
                reliability: Reliability::MaxRetransmits { retransmits: 0 },
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
            // Publish path
            let res = DataTrackIntent::try_from(&cfg("v1/rt/pub/game-sync")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Publish);
                assert_eq!(e.topic.deref(), "game-sync");
            } else {
                panic!("Expected UserTopic variant");
            }

            // Subscribe path
            let res = DataTrackIntent::try_from(&cfg("v1/rt/sub/audio_stream_12")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.direction, DataTrackDirection::Subscribe);
                assert_eq!(e.topic.deref(), "audio_stream_12");
            } else {
                panic!("Expected UserTopic variant");
            }
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
            // Forward slashes are caught by char check since splitn stops at 4
            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/game/engine")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::IllegalCharacters);

            // Spaces and symbols
            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/my topic")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::IllegalCharacters);

            let err = DataTrackIntent::try_from(&cfg("v1/rt/pub/topic$")).unwrap_err();
            assert_eq!(err, DataTrackIntentError::IllegalCharacters);
        }

        #[test]
        fn test_delivery_class_from_config() {
            let mut reliable = cfg("v1/rt/pub/operation");
            reliable.ordered = true;
            reliable.reliability = Reliability::Reliable;
            let res = DataTrackIntent::try_from(&reliable).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.delivery, DeliveryClass::Reliable);
            } else {
                panic!("Expected UserTopic variant");
            }

            let mut semi = cfg("v1/rt/sub/telemetry");
            semi.reliability = Reliability::MaxRetransmits { retransmits: 5 };
            let res = DataTrackIntent::try_from(&semi).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.delivery, DeliveryClass::SemiReliable);
            } else {
                panic!("Expected UserTopic variant");
            }

            let mut lifetime = cfg("v1/rt/sub/telemetry");
            lifetime.reliability = Reliability::MaxPacketLifetime { lifetime: 200 };
            let res = DataTrackIntent::try_from(&lifetime).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.delivery, DeliveryClass::SemiReliable);
            } else {
                panic!("Expected UserTopic variant");
            }

            let res = DataTrackIntent::try_from(&cfg("v1/rt/pub/control_input")).unwrap();
            if let DataTrackIntent::UserTopic(e) = res {
                assert_eq!(e.delivery, DeliveryClass::Lossy);
            } else {
                panic!("Expected UserTopic variant");
            }
        }

        #[test]
        fn test_max_length_boundary() {
            // Exact boundary (10 bytes prefix + 54 bytes topic = 64 total)
            let exact_valid = format!("v1/rt/pub/{}", "a".repeat(54));
            assert!(DataTrackIntent::try_from(&cfg(&exact_valid)).is_ok());

            // 1 byte over limit
            let one_byte_over = format!("v1/rt/pub/{}", "a".repeat(55));
            let err = DataTrackIntent::try_from(&cfg(&one_byte_over)).unwrap_err();
            assert_eq!(err, DataTrackIntentError::LabelTooLong);
        }
    }
}
