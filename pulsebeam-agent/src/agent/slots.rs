use pulsebeam_proto::signaling::Track;
use std::collections::HashMap;
use str0m::media::Mid;

pub type TrackId = String;

pub struct ReceiverSlot {
    pub mid: Mid,
    pub track_id: Option<TrackId>,
}

pub struct SlotManager {
    pending_tracks: HashMap<TrackId, Track>,
    active_tracks: HashMap<TrackId, Track>,
    slots: Vec<ReceiverSlot>,
}

impl SlotManager {
    pub fn new() -> Self {
        Self {
            pending_tracks: HashMap::new(),
            slots: Vec::new(),
            active_tracks: HashMap::new(),
        }
    }

    pub fn register(&mut self, mid: Mid) {
        self.slots.push(ReceiverSlot {
            mid,
            track_id: None,
        });
    }

    pub fn mids(&self) -> Vec<Mid> {
        self.slots.iter().map(|s| s.mid).collect()
    }

    pub fn sync(
        &mut self,
        update: pulsebeam_proto::signaling::StateUpdate,
    ) -> (Vec<(Mid, Track)>, Vec<pulsebeam_proto::signaling::Track>) {
        let mut new_assignments: Vec<(Mid, Track)> = Vec::new();
        let mut newly_discovered_tracks = Vec::new();

        for t in update.tracks_remove {
            self.pending_tracks.remove(&t);
            self.active_tracks.remove(&t);
        }

        for t in update.tracks_upsert {
            if self.active_tracks.contains_key(&t.id) {
                continue;
            }
            if self.pending_tracks.contains_key(&t.id) {
                continue;
            }

            newly_discovered_tracks.push(t.clone());
            self.pending_tracks.insert(t.id.clone(), t);
        }

        for a in update.assignments_remove {
            if let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.as_bytes())
            {
                s.track_id = None;
            }
        }

        for a in update.assignments_upsert {
            let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.mid.as_bytes())
            else {
                continue;
            };

            if s.track_id.as_deref() == Some(&a.track_id) {
                continue;
            }

            s.track_id = Some(a.track_id.clone());

            if let Some(track) = self.pending_tracks.remove(&a.track_id) {
                let mid = s.mid;
                self.active_tracks.insert(a.track_id, track.clone());
                new_assignments.push((mid, track));
            }
        }

        for slot in &self.slots {
            let Some(track_id) = &slot.track_id else {
                continue;
            };

            if self.active_tracks.contains_key(track_id) {
                continue;
            }

            let Some(track) = self.pending_tracks.remove(track_id) else {
                continue;
            };

            let mid = slot.mid;
            self.active_tracks.insert(track_id.clone(), track.clone());
            new_assignments.push((mid, track));
        }

        (new_assignments, newly_discovered_tracks)
    }
}
