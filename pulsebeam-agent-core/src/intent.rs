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
    NoLayerMeetsFloor(TrackId),
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
            Self::NoLayerMeetsFloor(track_id) => {
                write!(formatter, "no layer meets the floor for track {track_id}")
            }
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
                .then_with(|| left.mid.cmp(&right.mid))
        });
        let mut available = budget_bps;
        let mut allocations = Vec::with_capacity(ordered.len());
        for intent in &ordered {
            let options = layers
                .get(&intent.track_id)
                .ok_or_else(|| IntentError::UnknownTrack(intent.track_id.clone()))?;
            let mut options = options.clone();
            options.sort_by_key(|option| (option.height, option.id));
            let base = floor_option(intent, &options)?;
            let selected = if base.bitrate_bps <= available {
                available = available.saturating_sub(base.bitrate_bps);
                base
            } else if intent.min_height == 0 {
                LayerOption {
                    id: 0,
                    height: 0,
                    bitrate_bps: 0,
                }
            } else {
                return Err(IntentError::NoLayerMeetsFloor(intent.track_id.clone()));
            };
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
            let Some(options) = layers.get(&allocation.track_id) else {
                debug_assert!(false, "allocation track must have layers");
                continue;
            };
            let mut options = options.clone();
            options.sort_by_key(|option| (option.height, option.id));
            let current_index = options
                .iter()
                .position(|option| Some(option.id) == allocation.layer)
                .unwrap_or(0);
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

fn floor_option(intent: &VideoIntent, options: &[LayerOption]) -> Result<LayerOption, IntentError> {
    debug_assert!(!options.is_empty());
    let Some(option) = options
        .iter()
        .filter(|option| option.height >= intent.min_height && option.height <= intent.height)
        .min_by(|left, right| {
            left.height
                .cmp(&right.height)
                .then_with(|| left.id.cmp(&right.id))
        })
        .copied()
    else {
        return Err(IntentError::NoLayerMeetsFloor(intent.track_id.clone()));
    };
    Ok(option)
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
                    },
                    LayerOption {
                        id: 2,
                        height: 720,
                        bitrate_bps: 400,
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
                    },
                    LayerOption {
                        id: 2,
                        height: 720,
                        bitrate_bps: 400,
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
}
