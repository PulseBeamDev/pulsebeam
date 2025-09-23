use str0m::media::Mid;

use crate::entity::{self, TrackId};
use crate::participant::effect::{self, Effect};
use crate::track;

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone)]
pub struct AudioTrackData {
    /// Audio level in negative decibels (0 is max, -30 is typical)
    pub audio_level: Option<i8>,
    /// Indication of voice activity
    pub voice_activity: Option<bool>,
}

/// Metadata for a track
#[derive(Clone)]
struct TrackState {
    score: TrackScore,
    smoothed_level: i8,
}

impl TrackState {
    fn update(&mut self, data: &AudioTrackData, insertion_order: u64) {
        let level = data.audio_level.unwrap_or(-127);

        // exponential moving average for smoothing
        self.smoothed_level = ((self.smoothed_level as i16 * 3 + level as i16) / 4) as i8;

        self.score.voice_activity = data.voice_activity.unwrap_or(false);
        self.score.audio_level = self.smoothed_level;
        self.score.insertion_order = insertion_order;
    }
}

/// Scoring struct for top N tracks
#[derive(Clone)]
struct TrackScore {
    track_id: Arc<TrackId>,
    voice_activity: bool,
    audio_level: i8,      // Higher is louder (less negative)
    insertion_order: u64, // Deterministic tie-breaker
}

impl PartialEq for TrackScore {
    fn eq(&self, other: &Self) -> bool {
        self.track_id == other.track_id
    }
}
impl Eq for TrackScore {}

impl PartialOrd for TrackScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TrackScore {
    fn cmp(&self, other: &Self) -> Ordering {
        self.voice_activity
            .cmp(&other.voice_activity)
            .reverse()
            .then_with(|| self.audio_level.cmp(&other.audio_level))
            .then_with(|| self.insertion_order.cmp(&other.insertion_order))
    }
}

impl Default for AudioAllocator {
    fn default() -> Self {
        Self::new()
    }
}

struct Slot {
    mid: Mid,
    track_id: Option<Arc<entity::TrackId>>,
    hit_count: u64,
}

pub struct AudioAllocator {
    slots: Vec<Slot>,
    tracks: HashMap<Arc<TrackId>, TrackState>,
    top_n: BTreeSet<TrackScore>,
    active_ids: HashSet<Arc<TrackId>>,
    insertion_counter: u64,
}

impl AudioAllocator {
    pub fn new() -> Self {
        AudioAllocator {
            slots: Vec::new(),
            tracks: HashMap::new(),
            top_n: BTreeSet::new(),
            active_ids: HashSet::new(),
            insertion_counter: 0,
        }
    }

    pub fn add_track(&mut self, effects: &mut effect::Queue, track_handle: track::TrackHandle) {
        assert!(track_handle.meta.kind.is_audio());

        self.insertion_counter += 1;

        let track_id = track_handle.meta.id.clone();
        let score = TrackScore {
            track_id: track_id.clone(),
            voice_activity: false,
            audio_level: -127,
            insertion_order: self.insertion_counter,
        };

        let state = TrackState {
            score,
            smoothed_level: -127,
        };

        self.tracks.insert(track_id, state);
        effects.push_back(Effect::Subscribe(track_handle));
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if let Some(state) = self.tracks.remove(track_id) {
            self.top_n.remove(&state.score);
            self.rebuild_top_n();
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.push(Slot {
            track_id: None,
            mid,
            hit_count: 0,
        });
        tracing::info!("added audio slot: {}", mid);
    }

    pub fn get_slot(
        &mut self,
        track_id: &Arc<entity::TrackId>,
        data: &AudioTrackData,
    ) -> Option<&Mid> {
        if !self.should_forward(track_id, data) {
            return None;
        }

        let mut lowest_hit: Option<&Mid> = None;
        let mut lowest_hit_count = u64::MAX;
        let mut matched_mid: Option<&Mid> = None;

        for slot in self.slots.iter_mut() {
            // 1. Matched track id
            if let Some(slot_track_id) = &slot.track_id {
                if slot_track_id == track_id {
                    matched_mid = Some(&slot.mid);
                    break; // Exit loop once we find a matching track ID
                }
            } else if matched_mid.is_none() {
                // 2. Empty slot
                matched_mid = Some(&slot.mid);
            }

            // 3. Track lowest hit count
            if slot.hit_count < lowest_hit_count {
                lowest_hit_count = slot.hit_count;
                lowest_hit = Some(&slot.mid);
            }
        }

        // Return the matched slot's Mid, or the lowest hit count Mid if no match
        let res = matched_mid.or(lowest_hit);
        tracing::debug!("forwarded audio from {} to {:?}", track_id, res);
        res
    }

    pub fn should_forward(
        &mut self,
        track_id: &Arc<entity::TrackId>,
        track_data: &AudioTrackData,
    ) -> bool {
        if let Some(state) = self.tracks.get_mut(track_id) {
            self.insertion_counter += 1;
            let old_score = state.score.clone();
            state.update(track_data, self.insertion_counter);

            if self.top_n.contains(&old_score) {
                self.top_n.remove(&old_score);
                self.top_n.insert(state.score.clone());
            } else if self.top_n.len() < self.slots.len() {
                self.top_n.insert(state.score.clone());
            } else if let Some(min_score) = self.top_n.iter().next()
                && state.score > *min_score
            {
                self.top_n.pop_first();
                self.top_n.insert(state.score.clone());
            }

            self.refresh_active_ids();
        }
        self.active_ids.contains(track_id)
    }

    fn rebuild_top_n(&mut self) {
        self.top_n.clear();
        let mut all_scores: Vec<_> = self
            .tracks
            .values()
            .map(|state| state.score.clone())
            .collect();
        all_scores.sort_unstable();
        for score in all_scores.into_iter().take(self.slots.len()) {
            self.top_n.insert(score);
        }
        self.refresh_active_ids();
    }

    fn refresh_active_ids(&mut self) {
        self.active_ids.clear();
        for s in &self.top_n {
            self.active_ids.insert(s.track_id.clone());
        }
    }
}
