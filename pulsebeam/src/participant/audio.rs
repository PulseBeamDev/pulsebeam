use str0m::media::Mid;

use crate::entity::{self, TrackId};
use crate::participant::effect::{self, Effect};
use crate::track;

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

const SILENCE_BASELINE: f32 = -127.0;

#[derive(Clone)]
pub struct AudioTrackData {
    /// Audio level in negative decibels (0 is max, -30 is typical)
    pub audio_level: Option<i8>,
    /// Indication of voice activity
    pub voice_activity: Option<bool>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
struct Level(f32);

impl Eq for Level {}

impl PartialOrd for Level {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Level {
    fn cmp(&self, other: &Self) -> Ordering {
        // Handle NaN deterministically: treat NaN as the smallest possible value
        let a = if self.0.is_nan() {
            f32::NEG_INFINITY
        } else {
            self.0
        };
        let b = if other.0.is_nan() {
            f32::NEG_INFINITY
        } else {
            other.0
        };
        a.partial_cmp(&b).unwrap()
    }
}

impl Level {
    fn update(&mut self, data: &AudioTrackData) {
        const ALPHA: f32 = 0.2; // smoothing factor
        const VAD_PENALTY: f32 = 0.33; // fraction toward silence if VAD=0

        // Input
        let raw_level = data.audio_level.unwrap_or(-30) as f32;

        // Soft VAD gate: nudge toward silence
        let current_adj = if data.voice_activity == Some(true) {
            raw_level
        } else {
            raw_level * (1.0 - VAD_PENALTY) + SILENCE_BASELINE * VAD_PENALTY
        };

        // EMA smoothing
        let smoothed = ALPHA * current_adj + (1.0 - ALPHA) * self.0;
        self.0 = smoothed;
    }
}

#[derive(Ord, PartialOrd, PartialEq, Eq)]
struct Slot {
    level: Level,
    mid: Mid,
    track_id: Arc<entity::TrackId>,
}

pub struct AudioAllocator {
    available_slots: VecDeque<Mid>,
    active_slots: Vec<Slot>,
    tracks: HashMap<Arc<TrackId>, Level>,
}

impl Default for AudioAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioAllocator {
    pub fn new() -> Self {
        AudioAllocator {
            available_slots: VecDeque::new(),
            active_slots: Vec::new(),
            tracks: HashMap::new(),
        }
    }

    pub fn add_track(&mut self, effects: &mut effect::Queue, track_handle: track::TrackHandle) {
        assert!(track_handle.meta.kind.is_audio());
        self.tracks
            .insert(track_handle.meta.id.clone(), Level(SILENCE_BASELINE));
        effects.push_back(Effect::Subscribe(track_handle));
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        let Some(_) = self.tracks.remove(track_id) else {
            return;
        };

        let Some(slot) = self.active_slots.iter().find(|s| s.track_id == *track_id) else {
            return;
        };

        self.available_slots.push_back(slot.mid);
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.available_slots.push_back(mid);
        tracing::info!("added audio slot: {}", mid);
    }

    pub fn get_slot(
        &mut self,
        track_id: &Arc<entity::TrackId>,
        data: &AudioTrackData,
    ) -> Option<Mid> {
        let level = self.tracks.get_mut(track_id)?;
        level.update(data);

        let matched_idx = self
            .active_slots
            .iter()
            .position(|s| s.track_id == *track_id)
            .unwrap_or(0); // or lowest

        let mid = if let Some(slot) = self.active_slots.get_mut(matched_idx) {
            slot.level = *level;
            slot.mid
        } else {
            let mid = self.available_slots.pop_front()?;
            self.active_slots.push(Slot {
                level: *level,
                mid,
                track_id: track_id.clone(),
            });
            mid
        };
        self.active_slots.sort();

        tracing::debug!("forwarded audio from {} to {:?}", track_id, mid);
        Some(mid)
    }
}

// #[cfg(test)]
// mod tests {
//     use str0m::media::MediaKind;
//
//     use super::*;
//
//     fn make_data(level: i8, vad: bool) -> AudioTrackData {
//         AudioTrackData {
//             audio_level: Some(level),
//             voice_activity: Some(vad),
//         }
//     }
//
//     #[tokio::test]
//     async fn louder_tracks_win() {
//         let mut alloc = AudioAllocator::new();
//         alloc.add_slot(Mid::new());
//         alloc.add_slot(Mid::new());
//
//         let mut effects = effect::Queue::default();
//
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t2, _) = track::test::spawn_fake(MediaKind::Audio);
//         alloc.add_track(&mut effects, t1.clone());
//         alloc.add_track(&mut effects, t2.clone());
//
//         // t1 quiet, t2 loud
//         assert!(alloc.should_forward(&t1.meta.id, &make_data(-50, true)));
//         assert!(alloc.should_forward(&t2.meta.id, &make_data(-10, true)));
//
//         let selected = alloc.get_selected_tracks();
//         assert_eq!(selected, vec![t2.meta.id.clone(), t1.meta.id.clone()]);
//     }
//
//     #[tokio::test]
//     async fn vad_beats_non_vad() {
//         let mut alloc = AudioAllocator::new();
//         alloc.add_slot(Mid::new());
//
//         let mut effects = effect::Queue::default();
//
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t2, _) = track::test::spawn_fake(MediaKind::Audio);
//
//         alloc.add_track(&mut effects, t1.clone());
//         alloc.add_track(&mut effects, t2.clone());
//
//         // t1 loud but no VAD, t2 quiet but VAD
//         assert!(alloc.should_forward(&t1.meta.id, &make_data(-5, false)));
//         assert!(alloc.should_forward(&t2.meta.id, &make_data(-30, true)));
//
//         let selected = alloc.get_selected_tracks();
//         assert_eq!(selected, vec![t2.meta.id.clone()]); // VAD wins
//     }
//
//     #[tokio::test]
//     async fn smoothing_reduces_flapping() {
//         let mut alloc = AudioAllocator::new();
//         alloc.add_slot(Mid::new());
//
//         let mut effects = effect::Queue::default();
//
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         alloc.add_track(&mut effects, t1.clone());
//
//         // Start very quiet
//         assert!(!alloc.should_forward(&t1.meta.id, &make_data(-127, false)));
//
//         // Brief loud spike
//         assert!(alloc.should_forward(&t1.meta.id, &make_data(-5, true)));
//
//         // Immediately back to quiet
//         // Smoothed level should prevent immediate rejection
//         assert!(alloc.should_forward(&t1.meta.id, &make_data(-127, false)));
//     }
// }
