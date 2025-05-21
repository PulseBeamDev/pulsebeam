use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::time::Instant;

use crate::entity::TrackId;

#[derive(Debug)]
struct ScoreEntry {
    score: f32,
    last_updated: Instant,
}

pub struct VoiceRanker {
    scores: HashMap<Arc<TrackId>, ScoreEntry>,
    top_tracks: Vec<(Arc<TrackId>, f32)>, // Sorted descending
    decay_half_life: Duration,
    now: Instant,
}

impl Default for VoiceRanker {
    fn default() -> Self {
        // higher = more stable/less flapping
        // lower = more responsive
        Self::new(Duration::from_millis(600))
    }
}

impl VoiceRanker {
    pub fn new(decay_half_life: Duration) -> Self {
        Self {
            scores: HashMap::new(),
            top_tracks: Vec::new(),
            decay_half_life,
            now: Instant::now(),
        }
    }

    /// audio_level is measured in negative decibel. 0 is max and a "normal" value might be -30.
    pub fn process_packet(&mut self, track_id: Arc<TrackId>, audio_level: i8) -> bool {
        self.now = Instant::now();
        let audio_level = audio_level as f32;
        let weight = audio_level.max(0.0); // 0 = loudest
        let decay = |score: f32, dt: Duration| {
            // exponential decay for stale audio
            let half_life_secs = self.decay_half_life.as_secs_f32();
            score * 0.5f32.powf(dt.as_secs_f32() / half_life_secs)
        };

        let entry = self.scores.entry(track_id.clone()).or_insert(ScoreEntry {
            score: 0.0,
            last_updated: self.now,
        });

        let elapsed = self.now.duration_since(entry.last_updated);
        entry.score = decay(entry.score, elapsed) + weight;
        entry.last_updated = self.now;

        let score = entry.score;

        let mut updated = false;
        if let Some(pos) = self.top_tracks.iter().position(|(id, _)| *id == track_id) {
            self.top_tracks[pos].1 = score;
            updated = true;
        } else if self.top_tracks.len() < 3 {
            self.top_tracks.push((track_id.clone(), score));
            updated = true;
        } else if score > self.top_tracks[2].1 {
            // Replace the least active track
            self.top_tracks[2] = (track_id.clone(), score);
            updated = true;
        }

        if updated {
            // audio_level is inverted in dB. So, the more positive is louder
            self.top_tracks.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        }

        self.top_tracks.iter().any(|(id, _)| *id == track_id)
    }

    pub fn add_track(&mut self, track_id: Arc<TrackId>) {
        self.scores.insert(
            track_id,
            ScoreEntry {
                score: 0.0,
                last_updated: self.now,
            },
        );
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        self.scores.remove(track_id);
        self.top_tracks.retain(|(id, _)| id != track_id);
    }

    pub fn current_top_tracks(&self) -> Vec<Arc<TrackId>> {
        self.top_tracks.iter().map(|(id, _)| id.clone()).collect()
    }
}
