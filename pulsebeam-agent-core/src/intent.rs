use std::collections::BTreeMap;
use std::fmt;

use pulsebeam_proto::signaling;

use crate::preset::{LatencyLock, LatencyLockError};
use crate::types::TrackId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoIntent {
    pub mid: String,
    pub track_id: TrackId,
    pub height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}

impl VideoIntent {
    pub fn new(
        mid: impl Into<String>,
        track_id: TrackId,
        height: u32,
        min_height: u32,
        min_fps: u32,
        priority: u32,
    ) -> Result<Self, IntentError> {
        let mid = mid.into();
        if mid.is_empty() {
            return Err(IntentError::EmptyMid);
        }
        if min_height > height {
            return Err(IntentError::InvalidHeightRange { min_height, height });
        }
        Ok(Self {
            mid,
            track_id,
            height,
            min_height,
            min_fps,
            priority,
        })
    }

    fn to_proto(&self) -> signaling::VideoIntent {
        signaling::VideoIntent {
            mid: self.mid.clone(),
            track_id: self.track_id.as_str().to_owned(),
            height: self.height,
            min_height: self.min_height,
            min_fps: self.min_fps,
            priority: self.priority,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioIntent {
    pub pinned: Vec<TrackId>,
    pub auto: bool,
}

impl Default for AudioIntent {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            auto: true,
        }
    }
}

impl AudioIntent {
    pub fn new(pinned: Vec<TrackId>, auto: bool) -> Result<Self, IntentError> {
        let mut seen = std::collections::BTreeSet::new();
        for track_id in &pinned {
            if track_id.as_str().is_empty() {
                return Err(IntentError::EmptyTrackId);
            }
            if !seen.insert(track_id) {
                return Err(IntentError::DuplicateAudioPin(track_id.clone()));
            }
        }
        Ok(Self { pinned, auto })
    }

    fn to_proto(&self) -> signaling::AudioIntent {
        signaling::AudioIntent {
            pinned: self
                .pinned
                .iter()
                .map(|track_id| track_id.as_str().to_owned())
                .collect(),
            auto: self.auto,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentError {
    EmptyMid,
    EmptyTrackId,
    InvalidHeightRange { min_height: u32, height: u32 },
    DuplicateAudioPin(TrackId),
    UnknownTrack(TrackId),
    Latency(LatencyLockError),
}

impl fmt::Display for IntentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMid => formatter.write_str("video intent mid must not be empty"),
            Self::EmptyTrackId => formatter.write_str("track id must not be empty"),
            Self::InvalidHeightRange { min_height, height } => {
                write!(
                    formatter,
                    "minimum height {min_height} exceeds target {height}"
                )
            }
            Self::DuplicateAudioPin(track_id) => {
                write!(formatter, "duplicate audio pin {track_id}")
            }
            Self::UnknownTrack(track_id) => write!(formatter, "no layers for track {track_id}"),
            Self::Latency(error) => write!(formatter, "latency lock: {error}"),
        }
    }
}

impl std::error::Error for IntentError {}

impl From<LatencyLockError> for IntentError {
    fn from(error: LatencyLockError) -> Self {
        Self::Latency(error)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntentState {
    video: BTreeMap<String, VideoIntent>,
    audio: Option<AudioIntent>,
    publish: BTreeMap<String, bool>,
}

impl IntentState {
    pub fn set_video(&mut self, intent: VideoIntent) {
        self.video.insert(intent.mid.clone(), intent);
    }

    pub fn set_audio(&mut self, intent: AudioIntent) {
        self.audio = Some(intent);
    }

    pub fn set_publish(&mut self, mid: impl Into<String>, active: bool) -> Result<(), IntentError> {
        let mid = mid.into();
        if mid.is_empty() {
            return Err(IntentError::EmptyMid);
        }
        self.publish.insert(mid, active);
        Ok(())
    }

    pub fn clear_video(&mut self, mid: &str) {
        self.video.remove(mid);
    }

    pub fn clear_publish(&mut self, mid: &str) {
        self.publish.remove(mid);
    }

    pub fn set_latency(
        &self,
        lock: &mut LatencyLock,
        min_ms: u32,
        max_ms: u32,
    ) -> Result<(), IntentError> {
        lock.set(min_ms, max_ms).map_err(IntentError::from)
    }

    pub fn to_proto(&self, latency: Option<(u32, u32)>) -> signaling::ClientIntent {
        signaling::ClientIntent {
            video: self.video.values().map(VideoIntent::to_proto).collect(),
            audio: self.audio.as_ref().map(AudioIntent::to_proto),
            publish: self
                .publish
                .iter()
                .map(|(mid, active)| signaling::PublishIntent {
                    mid: mid.clone(),
                    active: *active,
                })
                .collect(),
            ext: latency.map(|(min_ms, max_ms)| signaling::Extensions {
                playout_delay: Some(signaling::PlayoutDelay { min_ms, max_ms }),
            }),
        }
    }

    pub fn video(&self) -> impl Iterator<Item = &VideoIntent> {
        self.video.values()
    }

    pub fn audio(&self) -> Option<&AudioIntent> {
        self.audio.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerOption {
    pub id: u8,
    pub height: u32,
    pub bitrate_bps: u64,
    pub fps: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyAllocation {
    pub mid: String,
    pub track_id: TrackId,
    pub layer: Option<u8>,
    pub height: u32,
    pub bitrate_bps: u64,
    pub paused: bool,
}

pub struct StickyAllocator {
    previous: BTreeMap<String, StickyAllocation>,
}

impl StickyAllocator {
    pub fn new() -> Self {
        Self {
            previous: BTreeMap::new(),
        }
    }

    pub fn allocate(
        &mut self,
        intents: &[VideoIntent],
        layers: &BTreeMap<TrackId, Vec<LayerOption>>,
        budget_bps: u64,
    ) -> Result<Vec<StickyAllocation>, IntentError> {
        let mut ordered: Vec<&VideoIntent> = intents.iter().collect();
        ordered.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| (right.min_height > 0).cmp(&(left.min_height > 0)))
                .then_with(|| left.mid.cmp(&right.mid))
        });
        let mut available = budget_bps;
        let mut allocations = Vec::with_capacity(ordered.len());
        for intent in &ordered {
            if intent.height == 0 {
                allocations.push(StickyAllocation {
                    mid: intent.mid.clone(),
                    track_id: intent.track_id.clone(),
                    layer: None,
                    height: 0,
                    bitrate_bps: 0,
                    paused: true,
                });
                continue;
            }
            let options = layers
                .get(&intent.track_id)
                .ok_or_else(|| IntentError::UnknownTrack(intent.track_id.clone()))?;
            let mut options = options.clone();
            options.retain(|option| {
                option.height <= intent.height && option.fps.is_none_or(|fps| fps >= intent.min_fps)
            });
            options.sort_by_key(|option| (option.height, option.id));
            let selected = floor_option(intent, &options)
                .filter(|option| option.bitrate_bps <= available)
                .or_else(|| {
                    options
                        .iter()
                        .filter(|option| {
                            option.height >= intent.min_height && option.bitrate_bps <= available
                        })
                        .max_by_key(|option| (option.height, option.id))
                        .copied()
                })
                .unwrap_or(LayerOption {
                    id: 0,
                    height: 0,
                    bitrate_bps: 0,
                    fps: None,
                });
            available = available.saturating_sub(selected.bitrate_bps);
            allocations.push(StickyAllocation {
                mid: intent.mid.clone(),
                track_id: intent.track_id.clone(),
                layer: (selected.bitrate_bps > 0).then_some(selected.id),
                height: selected.height,
                bitrate_bps: selected.bitrate_bps,
                paused: selected.bitrate_bps == 0,
            });
        }
        for allocation in &mut allocations {
            let Some(intent) = ordered.iter().find(|intent| intent.mid == allocation.mid) else {
                debug_assert!(false, "allocation must have a matching intent");
                continue;
            };
            if allocation.paused {
                continue;
            }
            let Some(options) = layers.get(&allocation.track_id) else {
                debug_assert!(false, "allocation track must have layers");
                continue;
            };
            let mut options = options.clone();
            options.retain(|option| {
                option.height <= intent.height && option.fps.is_none_or(|fps| fps >= intent.min_fps)
            });
            options.sort_by_key(|option| (option.height, option.id));
            if options.is_empty() {
                continue;
            }
            let current_index = options
                .iter()
                .position(|option| Some(option.id) == allocation.layer)
                .unwrap_or_else(|| {
                    debug_assert!(false, "active allocation must name an eligible layer");
                    0
                });
            let sticky_index = self
                .previous
                .get(&allocation.mid)
                .and_then(|previous| {
                    options
                        .iter()
                        .position(|option| Some(option.id) == previous.layer)
                })
                .unwrap_or(current_index);
            let mut start = current_index;
            if sticky_index > current_index {
                let Some(sticky) = options.get(sticky_index) else {
                    debug_assert!(false, "sticky layer index must remain in bounds");
                    continue;
                };
                let delta = sticky.bitrate_bps.saturating_sub(allocation.bitrate_bps);
                if sticky.height <= intent.height && delta <= available {
                    available = available.saturating_sub(delta);
                    allocation.layer = Some(sticky.id);
                    allocation.height = sticky.height;
                    allocation.bitrate_bps = sticky.bitrate_bps;
                    allocation.paused = false;
                    start = sticky_index;
                }
            }
            for option in options.iter().skip(start.saturating_add(1)) {
                if option.height > intent.height {
                    break;
                }
                if option.bitrate_bps.saturating_sub(allocation.bitrate_bps) > available {
                    break;
                }
                available = available
                    .saturating_sub(option.bitrate_bps.saturating_sub(allocation.bitrate_bps));
                allocation.layer = Some(option.id);
                allocation.height = option.height;
                allocation.bitrate_bps = option.bitrate_bps;
                allocation.paused = false;
            }
        }
        allocations.sort_by(|left, right| {
            left.mid
                .cmp(&right.mid)
                .then_with(|| left.track_id.cmp(&right.track_id))
        });
        self.previous = allocations
            .iter()
            .cloned()
            .map(|allocation| (allocation.mid.clone(), allocation))
            .collect();
        Ok(allocations)
    }
}

impl Default for StickyAllocator {
    fn default() -> Self {
        Self::new()
    }
}

fn floor_option(intent: &VideoIntent, options: &[LayerOption]) -> Option<LayerOption> {
    if options.is_empty() {
        return None;
    }
    options
        .iter()
        .filter(|option| option.height >= intent.min_height && option.height <= intent.height)
        .min_by(|left, right| {
            left.height
                .cmp(&right.height)
                .then_with(|| left.id.cmp(&right.id))
        })
        .copied()
        .or_else(|| {
            options
                .iter()
                .filter(|option| option.height <= intent.height)
                .max_by_key(|option| (option.height, option.id))
                .copied()
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn intent(mid: &str, track: &str, priority: u32) -> VideoIntent {
        VideoIntent::new(mid, TrackId::from(track), 720, 180, 0, priority).unwrap()
    }

    #[test]
    fn allocation_is_priority_aware_and_stable() {
        let mut allocator = StickyAllocator::new();
        let intents = vec![intent("b", "b", 1), intent("a", "a", 10)];
        let layers = BTreeMap::from([
            (
                TrackId::from("a"),
                vec![
                    LayerOption {
                        id: 1,
                        height: 180,
                        bitrate_bps: 100,
                        fps: None,
                    },
                    LayerOption {
                        id: 2,
                        height: 720,
                        bitrate_bps: 400,
                        fps: None,
                    },
                ],
            ),
            (
                TrackId::from("b"),
                vec![
                    LayerOption {
                        id: 1,
                        height: 180,
                        bitrate_bps: 100,
                        fps: None,
                    },
                    LayerOption {
                        id: 2,
                        height: 720,
                        bitrate_bps: 400,
                        fps: None,
                    },
                ],
            ),
        ]);
        let first = allocator.allocate(&intents, &layers, 500).unwrap();
        assert_eq!(
            first.iter().find(|item| item.mid == "a").unwrap().layer,
            Some(2)
        );
        assert_eq!(
            first.iter().find(|item| item.mid == "b").unwrap().layer,
            Some(1)
        );
        let second = allocator.allocate(&intents, &layers, 500).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn proto_intent_is_complete_and_omits_removed_video() {
        let mut state = IntentState::default();
        state.set_video(intent("0", "track", 1));
        state.clear_video("0");
        assert!(state.to_proto(None).video.is_empty());
    }

    #[test]
    fn hidden_intents_pause_without_a_layer_catalogue() {
        let mut allocator = StickyAllocator::new();
        let hidden = VideoIntent::new("0", TrackId::from("missing"), 0, 0, 0, 1).unwrap();
        let allocations = allocator.allocate(&[hidden], &BTreeMap::new(), 0).unwrap();
        assert_eq!(allocations[0].height, 0);
        assert!(allocations[0].paused);
    }

    #[test]
    fn minimum_fps_filters_layers_before_allocation() {
        let mut allocator = StickyAllocator::new();
        let intent = VideoIntent::new("0", TrackId::from("track"), 720, 180, 30, 1).unwrap();
        let layers = BTreeMap::from([(
            TrackId::from("track"),
            vec![
                LayerOption {
                    id: 1,
                    height: 180,
                    bitrate_bps: 100,
                    fps: Some(15),
                },
                LayerOption {
                    id: 2,
                    height: 360,
                    bitrate_bps: 200,
                    fps: Some(30),
                },
            ],
        )]);
        let allocations = allocator.allocate(&[intent], &layers, 200).unwrap();
        assert_eq!(allocations[0].layer, Some(2));
        assert_eq!(allocations[0].height, 360);
    }

    #[test]
    fn unaffordable_minimum_floor_pauses_instead_of_dropping_below_it() {
        let mut allocator = StickyAllocator::new();
        let intent = VideoIntent::new("0", TrackId::from("track"), 720, 360, 0, 1).unwrap();
        let layers = BTreeMap::from([(
            TrackId::from("track"),
            vec![
                LayerOption {
                    id: 1,
                    height: 180,
                    bitrate_bps: 50,
                    fps: None,
                },
                LayerOption {
                    id: 2,
                    height: 360,
                    bitrate_bps: 100,
                    fps: None,
                },
            ],
        )]);
        let allocations = allocator.allocate(&[intent], &layers, 75).unwrap();
        assert!(allocations[0].paused);
        assert_eq!(allocations[0].height, 0);
    }
}
