use crate::entity::TrackId;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

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
    last_updated: Instant,
}

/// Scoring struct for top N tracks
#[derive(Clone)]
struct TrackScore {
    track_id: Arc<TrackId>,
    voice_activity: bool,
    audio_level: i8, // Higher is louder (less negative)
    last_updated: Instant,
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
            .then_with(|| self.last_updated.cmp(&other.last_updated).reverse())
    }
}

pub struct AudioSelector {
    n: usize, // Number of streams to select
    tracks: HashMap<Arc<TrackId>, TrackState>,
    top_n: BTreeSet<TrackScore>, // Use a BTreeSet to keep the top N tracks sorted
}

impl AudioSelector {
    /// Create a new AudioSelector with the specified number of streams (N)
    pub fn new(n: usize) -> Self {
        AudioSelector {
            n: n.min(3), // Respect Chromium's limit of 3
            tracks: HashMap::new(),
            top_n: BTreeSet::new(),
        }
    }

    /// Add a new track to the selector
    pub fn add_track(&mut self, track_id: Arc<TrackId>) {
        let now = Instant::now();
        let score = TrackScore {
            track_id: track_id.clone(),
            voice_activity: false,
            audio_level: -127, // Start with the lowest possible audio level
            last_updated: now,
        };
        self.tracks.insert(
            track_id,
            TrackState {
                score,
                last_updated: now,
            },
        );
    }

    /// Remove a track
    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if let Some(state) = self.tracks.remove(track_id) {
            self.top_n.remove(&state.score);
            self.rebuild_top_n();
        }
    }

    /// Check if an AudioTrackData packet should be forwarded
    pub fn should_forward(&mut self, track_data: &AudioTrackData) -> bool {
        let now = Instant::now();
        if let Some(state) = self.tracks.get_mut(&track_data.track_id) {
            let old_score = state.score.clone();
            let new_score = TrackScore {
                track_id: track_data.track_id.clone(),
                voice_activity: track_data.voice_activity.unwrap_or(false),
                audio_level: track_data.audio_level.unwrap_or(-127),
                last_updated: now,
            };

            state.score = new_score.clone();
            state.last_updated = now;

            if self.top_n.contains(&old_score) {
                self.top_n.remove(&old_score);
                self.top_n.insert(new_score);
            } else if self.top_n.len() < self.n {
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
            last_updated: now,
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
        for score in all_scores.into_iter().take(self.n) {
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
