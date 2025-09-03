use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

// Assume TrackId is defined in your codebase
use crate::entity::TrackId;

// Minimal input struct for audio track data
#[derive(Clone)]
pub struct AudioTrackData {
    /// Unique identifier for the track
    pub track_id: Arc<TrackId>,
    /// Audio level in negative decibels (0 is max, -30 is typical)
    pub audio_level: Option<i8>,
    /// Indication of voice activity
    pub voice_activity: Option<bool>,
}

// Metadata for a track
#[derive(Clone)]
struct TrackState {
    audio_level: Option<i8>,
    voice_activity: Option<bool>,
    last_updated: Instant,
}

// Scoring struct for top N tracks
#[derive(Clone)]
struct TrackScore {
    track_id: Arc<TrackId>,
    voice_activity: bool,
    audio_level: i8, // Higher is louder (less negative)
    last_updated: Instant,
}

impl PartialEq for TrackScore {
    fn eq(&self, other: &Self) -> bool {
        self.voice_activity == other.voice_activity
            && self.audio_level == other.audio_level
            && self.last_updated == other.last_updated
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
        match (self.voice_activity, other.voice_activity) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        // If voice_activity is equal, sort by audio_level (higher is better)
        self.audio_level.cmp(&other.audio_level).then_with(|| {
            // Tiebreaker: more recent update wins
            self.last_updated.cmp(&other.last_updated).reverse()
        })
    }
}

pub struct AudioSelector {
    n: usize, // Number of streams to select (e.g., 3)
    tracks: HashMap<Arc<TrackId>, TrackState>,
    top_n: Vec<TrackScore>,          // Sorted array of top N tracks
    selected: HashSet<Arc<TrackId>>, // Tracks currently selected for forwarding
    next_best: Option<TrackScore>,   // Best track not in top N
}

impl AudioSelector {
    /// Create a new AudioSelector with the specified number of streams (N)
    pub fn new(n: usize) -> Self {
        AudioSelector {
            n: n.min(3), // Respect Chromium's limit
            tracks: HashMap::new(),
            top_n: Vec::with_capacity(n),
            selected: HashSet::with_capacity(n),
            next_best: None,
        }
    }

    /// Add a new track to the selector
    pub fn add_track(&mut self, track_id: Arc<TrackId>) {
        let now = Instant::now();
        self.tracks.insert(
            track_id,
            TrackState {
                audio_level: None,
                voice_activity: None,
                last_updated: now,
            },
        );
    }

    /// Check if an AudioTrackData packet should be forwarded (i.e., its track is in the top N)
    /// Updates the track state and top N if the track exists, otherwise returns false
    pub fn should_forward(&mut self, track_data: &AudioTrackData) -> bool {
        let track_id = track_data.track_id.clone();
        let now = Instant::now();

        // Check if track exists
        if let Some(state) = self.tracks.get_mut(&track_id) {
            // Update track state
            state.audio_level = track_data.audio_level;
            state.voice_activity = track_data.voice_activity;
            state.last_updated = now;

            // Create new score
            let new_score = TrackScore {
                track_id: track_id.clone(),
                voice_activity: track_data.voice_activity.unwrap_or(false),
                audio_level: track_data.audio_level.unwrap_or(-100), // Default to very quiet
                last_updated: now,
            };

            // Update top N
            let should_insert = if self.top_n.len() < self.n {
                true // Room for more tracks
            } else if let Some(worst) = self.top_n.last() {
                new_score > *worst // Better than the worst in top N
            } else {
                false
            };

            if should_insert {
                // Remove existing score for this track_id, if any
                self.top_n.retain(|score| score.track_id != track_id);
                // Insert new score in sorted order
                let insert_pos = self
                    .top_n
                    .iter()
                    .position(|score| new_score > *score)
                    .unwrap_or(self.top_n.len());
                self.top_n.insert(insert_pos, new_score);
                // Truncate to N
                if self.top_n.len() > self.n {
                    let popped = self.top_n.pop().unwrap();
                    // Update next_best if popped score is better
                    if self.next_best.as_ref().map_or(true, |nb| popped > *nb) {
                        self.next_best = Some(popped);
                    }
                }
                // Update selected set
                self.selected.clear();
                for score in &self.top_n {
                    self.selected.insert(score.track_id.clone());
                }
            } else {
                // Update next_best if not in top N and better than current next_best
                if !self.top_n.iter().any(|score| score.track_id == track_id) {
                    if self.next_best.as_ref().map_or(true, |nb| new_score > *nb) {
                        self.next_best = Some(new_score);
                    }
                }
            }

            // Check if the track is in the selected set
            self.selected.contains(&track_id)
        } else {
            // Track not added, filter it out
            false
        }
    }

    /// Remove a track (e.g., when a participant leaves)
    pub fn remove_track(&mut self, track_id: Arc<TrackId>) {
        self.tracks.remove(&track_id);
        if self.selected.remove(&track_id) {
            // Track was in top N, remove it
            self.top_n.retain(|score| score.track_id != track_id);
            // Insert next_best if available and top_n isn't full
            if self.top_n.len() < self.n {
                if let Some(next_best) = self.next_best.take() {
                    if self.tracks.contains_key(&next_best.track_id) {
                        let insert_pos = self
                            .top_n
                            .iter()
                            .position(|score| next_best > *score)
                            .unwrap_or(self.top_n.len());
                        self.top_n.insert(insert_pos, next_best);
                    }
                }
                // Update selected set
                self.selected.clear();
                for score in &self.top_n {
                    self.selected.insert(score.track_id.clone());
                }
            }
            // Find new next_best (amortized O(M))
            self.next_best = None;
            for (tid, state) in &self.tracks {
                if !self.top_n.iter().any(|s| s.track_id == *tid) {
                    let score = TrackScore {
                        track_id: tid.clone(),
                        voice_activity: state.voice_activity.unwrap_or(false),
                        audio_level: state.audio_level.unwrap_or(-100),
                        last_updated: state.last_updated,
                    };
                    if self.next_best.as_ref().map_or(true, |nb| score > *nb) {
                        self.next_best = Some(score);
                    }
                }
            }
        }
    }

    /// Get the current top N track IDs (for debugging or monitoring)
    pub fn get_selected(&self) -> Vec<Arc<TrackId>> {
        self.selected.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use tokio::time::{self, Duration as TokioDuration};

    #[tokio::test(start_paused = true)]
    async fn test_audio_selector() {
        let mut selector = AudioSelector::new(3);

        let track_ids = vec![
            test_utils::create_track_id(),
            test_utils::create_track_id(),
            test_utils::create_track_id(),
            test_utils::create_track_id(),
        ];
        // Add tracks explicitly
        for track_id in &track_ids {
            selector.add_track(track_id.clone());
        }
        for i in 0..5 {
            for track_id in &track_ids {
                let track_data = AudioTrackData {
                    track_id: track_id.clone(),
                    audio_level: Some(-(30 + i % 10) as i8),
                    voice_activity: Some(i % 2 == 0),
                };
                let should_forward = selector.should_forward(&track_data);
                println!(
                    "Track {:?}: should_forward = {}",
                    track_data.track_id, should_forward
                );
                let selected = selector.get_selected();
                println!("Selected tracks: {:?}", selected);
            }
            time::sleep(TokioDuration::from_millis(100)).await;
        }
    }
}
