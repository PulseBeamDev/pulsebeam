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

#[derive(Ord, PartialOrd, PartialEq, Eq, Debug)]
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

    pub fn add_track(&mut self, effects: &mut effect::Queue, track_handle: track::TrackReceiver) {
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

        let mid = if let Some(slot) = self
            .active_slots
            .iter_mut()
            .find(|s| s.track_id == *track_id)
        {
            slot.level = *level;
            slot.mid
        } else if let Some(mid) = self.available_slots.pop_front() {
            self.active_slots.push(Slot {
                level: *level,
                mid,
                track_id: track_id.clone(),
            });
            mid
        } else if let Some(slot) = self.active_slots.first_mut() {
            // swap slot with new track only when it is louder
            if *level > slot.level {
                slot.track_id = track_id.clone();
                slot.level = *level;
                slot.mid
            } else {
                return None;
            }
        } else {
            return None;
        };
        self.active_slots.sort();

        tracing::debug!("forwarded audio from {} to {:?}", track_id, mid);
        Some(mid)
    }

    pub fn get_active_tracks(&self) -> Vec<Arc<TrackId>> {
        self.active_slots
            .iter()
            .map(|s| s.track_id.clone())
            .collect()
    }
}

// #[cfg(test)]
// mod test {
//     use super::*;
//     use str0m::media::MediaKind;
//
//     #[tokio::test]
//     async fn assigns_only_top_tracks_when_slots_are_limited() {
//         let mut allocator = AudioAllocator::new();
//         let mut effects = effect::Queue::default();
//
//         // Two slots available
//         allocator.add_slot(Mid::new());
//         allocator.add_slot(Mid::new());
//
//         // Create four tracks
//         let (t0, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t2, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t3, _) = track::test::spawn_fake(MediaKind::Audio);
//
//         allocator.add_track(&mut effects, t0.clone());
//         allocator.add_track(&mut effects, t1.clone());
//         allocator.add_track(&mut effects, t2.clone());
//         allocator.add_track(&mut effects, t3.clone());
//
//         // Assign audio levels
//         allocator.get_slot(
//             &t0.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-5),
//                 voice_activity: Some(true),
//             },
//         );
//         allocator.get_slot(
//             &t1.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-10),
//                 voice_activity: Some(true),
//             },
//         );
//         allocator.get_slot(
//             &t2.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-20),
//                 voice_activity: Some(true),
//             },
//         );
//         allocator.get_slot(
//             &t3.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-50),
//                 voice_activity: Some(true),
//             },
//         );
//
//         // Only the two loudest tracks should be active
//         let active = allocator.get_active_tracks();
//         assert!(active.contains(&t0.meta.id));
//         assert!(active.contains(&t1.meta.id));
//         assert!(!active.contains(&t2.meta.id));
//         assert!(!active.contains(&t3.meta.id));
//     }
//
//     #[tokio::test]
//     async fn prioritizes_vad_over_loud_non_vad() {
//         let mut allocator = AudioAllocator::new();
//         let mut effects = effect::Queue::default();
//
//         allocator.add_slot(Mid::new());
//         allocator.add_slot(Mid::new());
//
//         let (t0, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t2, _) = track::test::spawn_fake(MediaKind::Audio);
//
//         allocator.add_track(&mut effects, t0.clone());
//         allocator.add_track(&mut effects, t1.clone());
//         allocator.add_track(&mut effects, t2.clone());
//
//         // Track0 loud but not speaking
//         allocator.get_slot(
//             &t0.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-50), // tie with VAD
//                 voice_activity: Some(false),
//             },
//         );
//         // Track1 quiet but speaking
//         allocator.get_slot(
//             &t1.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-50),
//                 voice_activity: Some(true),
//             },
//         );
//         // Track2 mid loudness and speaking
//         allocator.get_slot(
//             &t2.meta.id,
//             &AudioTrackData {
//                 audio_level: Some(-20),
//                 voice_activity: Some(true),
//             },
//         );
//
//         let active = allocator.get_active_tracks();
//         assert!(active.contains(&t1.meta.id));
//         assert!(active.contains(&t2.meta.id));
//         assert!(!active.contains(&t0.meta.id));
//     }
//
//     #[tokio::test]
//     async fn reuses_slots_for_louder_new_tracks() {
//         let mut allocator = AudioAllocator::new();
//         let mut effects = effect::Queue::default();
//
//         allocator.add_slot(Mid::new());
//         allocator.add_slot(Mid::new());
//
//         let (t0, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t1, _) = track::test::spawn_fake(MediaKind::Audio);
//         let (t2, _) = track::test::spawn_fake(MediaKind::Audio);
//
//         allocator.add_track(&mut effects, t0.clone());
//         allocator.add_track(&mut effects, t1.clone());
//         allocator.add_track(&mut effects, t2.clone());
//
//         // Step 1: t0 and t1 occupy slots
//         let _slot0 = allocator
//             .get_slot(
//                 &t0.meta.id,
//                 &AudioTrackData {
//                     audio_level: Some(-10),
//                     voice_activity: Some(true),
//                 },
//             )
//             .unwrap();
//         let slot1 = allocator
//             .get_slot(
//                 &t1.meta.id,
//                 &AudioTrackData {
//                     audio_level: Some(-20),
//                     voice_activity: Some(true),
//                 },
//             )
//             .unwrap();
//
//         let active = allocator.get_active_tracks();
//         assert!(active.contains(&t0.meta.id));
//         assert!(active.contains(&t1.meta.id));
//
//         // Step 2: t2 louder than t1 replaces t1
//         let slot2 = allocator
//             .get_slot(
//                 &t2.meta.id,
//                 &AudioTrackData {
//                     audio_level: Some(-5),
//                     voice_activity: Some(true),
//                 },
//             )
//             .unwrap();
//
//         let active = allocator.get_active_tracks();
//         assert!(active.contains(&t0.meta.id));
//         assert!(active.contains(&t2.meta.id));
//         assert!(!active.contains(&t1.meta.id));
//
//         // Verify slot reuse
//         assert_eq!(slot2, slot1);
//     }
// }
