use std::fmt;
use std::time::Duration;

use pulsebeam_agent_core::{MediaKind, MonotonicTime, TransportGeneration};

pub const SIGNALING_LABEL: &str = "v1/sys/signaling";
pub const MAX_VIDEO_SLOTS: usize = 7;
pub const MAX_AUDIO_SLOTS: usize = 3;
pub const DEFAULT_SCALABILITY_MODE: &str = "L1T3";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerConfig {
    pub video_slots: usize,
    pub audio_slots: usize,
    pub ice_servers: Vec<String>,
    pub bundle_policy: &'static str,
    pub rtcp_mux_policy: &'static str,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            video_slots: MAX_VIDEO_SLOTS,
            audio_slots: MAX_AUDIO_SLOTS,
            ice_servers: Vec::new(),
            bundle_policy: "max-bundle",
            rtcp_mux_policy: "require",
        }
    }
}

impl PeerConfig {
    pub fn bounded(mut self) -> Self {
        self.video_slots = self.video_slots.min(MAX_VIDEO_SLOTS);
        self.audio_slots = self.audio_slots.min(MAX_AUDIO_SLOTS);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelConfig {
    pub label: String,
    pub ordered: bool,
    pub max_retransmits: Option<u16>,
}

impl DataChannelConfig {
    pub fn reliable(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ordered: true,
            max_retransmits: None,
        }
    }

    pub fn latest(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ordered: false,
            max_retransmits: Some(0),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingConfig {
    pub rid: String,
    pub active: bool,
    pub scale_resolution_down_by: Option<u32>,
    pub max_bitrate_bps: Option<u64>,
    pub max_framerate: Option<u32>,
    pub scalability_mode: String,
}

impl EncodingConfig {
    pub fn inactive(rid: impl Into<String>) -> Self {
        Self {
            rid: rid.into(),
            active: false,
            scale_resolution_down_by: None,
            max_bitrate_bps: None,
            max_framerate: None,
            scalability_mode: DEFAULT_SCALABILITY_MODE.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderPreset {
    pub video: Vec<EncodingConfig>,
    pub audio_max_bitrate_bps: u64,
    pub audio_content_hint: String,
}

impl SenderPreset {
    pub fn inactive() -> Self {
        Self {
            video: vec![
                EncodingConfig::inactive("q"),
                EncodingConfig::inactive("h"),
                EncodingConfig::inactive("f"),
            ],
            audio_max_bitrate_bps: 0,
            audio_content_hint: "speech".to_owned(),
        }
    }

    pub fn for_video(preset: pulsebeam_agent_core::VideoPreset, layers: usize) -> Self {
        let base = preset.base_bitrate_bps();
        let max_framerate = (preset == pulsebeam_agent_core::VideoPreset::Screen).then_some(15);
        let scales = [4, 2, 1];
        let weights = [15_u64, 35, 100];
        let video = ["q", "h", "f"]
            .into_iter()
            .zip(scales)
            .zip(weights)
            .enumerate()
            .map(|(index, ((rid, scale), weight))| EncodingConfig {
                rid: rid.to_owned(),
                active: index < layers,
                scale_resolution_down_by: Some(scale),
                max_bitrate_bps: Some(base.saturating_mul(weight) / 100),
                max_framerate,
                scalability_mode: DEFAULT_SCALABILITY_MODE.to_owned(),
            })
            .collect();
        Self {
            video,
            audio_max_bitrate_bps: 48_000,
            audio_content_hint: "speech".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserEvent {
    Connected,
    Closed,
    Failed(String),
    Signaling(Vec<u8>),
    Data { label: String, payload: Vec<u8> },
    RemoteTrack { mid: String, kind: MediaKind },
    Timer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationEvent<T> {
    pub generation: TransportGeneration,
    pub value: T,
}

impl<T> GenerationEvent<T> {
    pub fn new(generation: TransportGeneration, value: T) -> Self {
        Self { generation, value }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportAction {
    Send {
        generation: TransportGeneration,
        channel: String,
        payload: Vec<u8>,
    },
    Connect {
        generation: TransportGeneration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebError {
    Browser(String),
    Core(String),
    StaleGeneration {
        expected: TransportGeneration,
        received: TransportGeneration,
    },
    Topic(String),
    E2ee(String),
    Http(String),
    InvalidValue(&'static str),
}

impl fmt::Display for WebError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Browser(error) => write!(formatter, "browser: {error}"),
            Self::Core(error) => write!(formatter, "core: {error}"),
            Self::StaleGeneration { expected, received } => write!(
                formatter,
                "stale browser generation: expected {}, received {}",
                expected.value(),
                received.value()
            ),
            Self::Topic(error) => write!(formatter, "topic: {error}"),
            Self::E2ee(error) => write!(formatter, "E2EE: {error}"),
            Self::Http(error) => write!(formatter, "HTTP: {error}"),
            Self::InvalidValue(value) => write!(formatter, "invalid browser value: {value}"),
        }
    }
}

impl std::error::Error for WebError {}

pub fn topic_label(
    reliable: bool,
    publisher: bool,
    topic: &str,
    publisher_id: Option<&str>,
) -> String {
    debug_assert!(!topic.is_empty());
    let prefix = if reliable { "v1/rel" } else { "v1/rt" };
    let lane = if publisher { "pub" } else { "sub" };
    match publisher_id {
        Some(publisher_id) => format!("{prefix}/{lane}/{topic}/{publisher_id}"),
        None => format!("{prefix}/{lane}/{topic}"),
    }
}

pub fn duration_millis(duration: Duration) -> i32 {
    i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
}

pub fn monotonic_from_millis(millis: f64) -> MonotonicTime {
    if !millis.is_finite() || millis.is_sign_negative() {
        debug_assert!(
            false,
            "browser performance time must be finite and non-negative"
        );
        return MonotonicTime::ZERO;
    }
    MonotonicTime::from(std::time::Duration::from_secs_f64(
        (millis / 1_000.0).min(std::time::Duration::MAX.as_secs_f64()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_transport_labels_and_slot_limits_are_stable() {
        assert_eq!(SIGNALING_LABEL, "v1/sys/signaling");
        assert_eq!(topic_label(false, true, "chat", None), "v1/rt/pub/chat");
        assert_eq!(
            topic_label(true, false, "chat", Some("alice")),
            "v1/rel/sub/chat/alice"
        );
        assert_eq!(PeerConfig::default().bounded().video_slots, 7);
    }
}
