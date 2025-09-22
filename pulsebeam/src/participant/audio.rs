use str0m::media::Mid;

use crate::entity::{self, TrackId};
use crate::participant::effect::{self, Effect};
use crate::track;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

#[derive(Clone)]
pub struct AudioTrackData {
    /// Unique identifier for the track
    pub track_id: Arc<TrackId>,
    /// Audio level in negative decibels (0 is max, -30 is typical)
    pub audio_level: Option<i8>,
    /// Indication of voice activity
    pub voice_activity: Option<bool>,
}

/// Metadata for a track
#[derive(Clone)]
struct TrackState {
    score: TrackScore,
    update_sequence: u64,
    handle: track::TrackHandle,
}

/// Scoring struct for top N tracks
#[derive(Clone)]
struct TrackScore {
    track_id: Arc<TrackId>,
    voice_activity: bool,
    audio_level: i8,      // Higher is louder (less negative)
    update_sequence: u64, // Sequence number for ordering updates
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
        // Prioritize voice_activity == true
        self.voice_activity
            .cmp(&other.voice_activity)
            .reverse()
            .then_with(|| self.audio_level.cmp(&other.audio_level))
            .then_with(|| self.update_sequence.cmp(&other.update_sequence).reverse())
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
    top_n: BTreeSet<TrackScore>, // Use a BTreeSet to keep the top N tracks sorted
    sequence_counter: u64,       // Counter for update ordering
}

impl AudioAllocator {
    /// Create a new AudioSelector with the specified number of streams (N)
    pub fn new() -> Self {
        AudioAllocator {
            slots: Vec::new(),
            tracks: HashMap::new(),
            top_n: BTreeSet::new(),
            sequence_counter: 0,
        }
    }

    /// Add a new track to the selector
    pub fn add_track(&mut self, effects: &mut effect::Queue, track_handle: track::TrackHandle) {
        assert!(track_handle.meta.kind.is_audio());

        let track_id = track_handle.meta.id.clone();
        self.sequence_counter += 1;
        let score = TrackScore {
            track_id: track_id.clone(),
            voice_activity: false,
            audio_level: -127, // Start with the lowest possible audio level
            update_sequence: self.sequence_counter,
        };
        self.tracks.insert(
            track_id,
            TrackState {
                score,
                update_sequence: self.sequence_counter,
                handle: track_handle.clone(),
            },
        );
        effects.push_back(Effect::Subscribe(track_handle));
    }

    /// Remove a track
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

    /// Check if an AudioTrackData packet should be forwarded
    pub fn should_forward(&mut self, track_data: &AudioTrackData) -> bool {
        if let Some(state) = self.tracks.get_mut(&track_data.track_id) {
            let old_score = state.score.clone();
            self.sequence_counter += 1;
            let new_score = TrackScore {
                track_id: track_data.track_id.clone(),
                voice_activity: track_data.voice_activity.unwrap_or(false),
                audio_level: track_data.audio_level.unwrap_or(-127),
                update_sequence: self.sequence_counter,
            };

            state.score = new_score.clone();
            state.update_sequence = self.sequence_counter;

            if self.top_n.contains(&old_score) {
                self.top_n.remove(&old_score);
                self.top_n.insert(new_score);
            } else if self.top_n.len() < self.slots.len() {
                self.top_n.insert(new_score);
            } else if let Some(min_score) = self.top_n.iter().next()
                && new_score > *min_score
            {
                self.top_n.pop_first();
                self.top_n.insert(new_score);
            }
        }
        self.top_n.contains(&TrackScore {
            track_id: track_data.track_id.clone(),
            voice_activity: false, // These fields don't matter for contains check
            audio_level: 0,
            update_sequence: 0,
        })
    }

    /// Rebuilds the top_n set from all tracked tracks.
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
    }

    /// Get the current top N track IDs
    pub fn get_selected_tracks(&self) -> Vec<Arc<TrackId>> {
        self.top_n
            .iter()
            .map(|score| score.track_id.clone())
            .collect()
    }
}
