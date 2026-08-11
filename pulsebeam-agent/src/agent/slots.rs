use pulsebeam_proto::signaling::Track;
use std::collections::HashMap;
use str0m::media::Mid;

pub type TrackId = String;

struct ReceiverSlot {
    mid: Mid,
    track_id: Option<TrackId>,
    /// Whether the SFU last told us it had stopped forwarding this slot.
    ///
    /// Kept so a repeat of the same state is not reported as a change. The server only upserts an
    /// assignment when something about it moved, but it also upserts for reasons other than pause,
    /// and an application that redraws a placeholder on every notification would flicker.
    paused: bool,
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
            paused: false,
            mid,
            track_id: None,
        });
    }

    /// A track the SFU has told us about, whether or not it currently occupies a slot.
    ///
    /// A subscription the viewer has hidden (`target_height = 0`) is never assigned a slot, so
    /// `assigned` cannot see it, but the publication itself is still known.
    pub fn known(&self, track_id: &str) -> Option<Track> {
        self.active_tracks
            .get(track_id)
            .or_else(|| self.pending_tracks.get(track_id))
            .cloned()
    }

    pub fn assigned(&self, track_id: &str) -> Option<(Mid, Track)> {
        let track = self.active_tracks.get(track_id)?.clone();
        let slot = self
            .slots
            .iter()
            .find(|slot| slot.track_id.as_deref() == Some(track_id))?;
        Some((slot.mid, track))
    }

    /// Whether the SFU has stopped forwarding this track.
    ///
    /// Test-only: production reads the same state through `Publication::is_paused`, which is what
    /// the application-facing `Participant::video_paused` is built on. Kept here because it is the
    /// natural way to assert what `sync` recorded.
    #[cfg(test)]
    pub fn is_paused(&self, track_id: &str) -> bool {
        self.slots
            .iter()
            .find(|s| s.track_id.as_deref() == Some(track_id))
            .is_some_and(|s| s.paused)
    }

    pub fn sync(&mut self, update: pulsebeam_proto::signaling::StateUpdate) -> SyncOutcome {
        let mut new_assignments: Vec<(Mid, Track)> = Vec::new();
        let mut pause_changes: Vec<(TrackId, bool)> = Vec::new();
        let mut newly_discovered_tracks = Vec::new();
        let removed_tracks = update.tracks_remove.clone();

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
                // Same track in the same slot, so this upsert exists because something else about
                // the assignment moved - in practice, because the SFU started or stopped
                // forwarding it. Returning early here is what dropped every pause transition on
                // the floor, leaving a client unable to tell a paused stream from a dead network.
                if s.paused != a.paused {
                    s.paused = a.paused;
                    pause_changes.push((a.track_id.clone(), a.paused));
                }
                continue;
            }

            s.track_id = Some(a.track_id.clone());
            s.paused = a.paused;
            if a.paused {
                pause_changes.push((a.track_id.clone(), true));
            }

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

        SyncOutcome {
            new_assignments,
            newly_discovered_tracks,
            removed_tracks,
            pause_changes,
        }
    }
}

/// What changed in the room, from one server state update.
pub struct SyncOutcome {
    pub new_assignments: Vec<(Mid, Track)>,
    pub newly_discovered_tracks: Vec<pulsebeam_proto::signaling::Track>,
    pub removed_tracks: Vec<TrackId>,
    /// Tracks whose forwarding state changed, and what it changed to.
    pub pause_changes: Vec<(TrackId, bool)>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use pulsebeam_proto::signaling::{StateUpdate, Track as ProtoTrack, VideoAssignment};

    fn update(assignments: Vec<VideoAssignment>, tracks: Vec<ProtoTrack>) -> StateUpdate {
        StateUpdate {
            seq: 1,
            is_snapshot: false,
            tracks_upsert: tracks,
            tracks_remove: Vec::new(),
            assignments_upsert: assignments,
            assignments_remove: Vec::new(),
        }
    }

    fn track(id: &str) -> ProtoTrack {
        ProtoTrack {
            id: id.to_owned(),
            kind: 1,
            participant_id: "pub".to_owned(),
            meta: Default::default(),
        }
    }

    fn assignment(mid: &str, track_id: &str, paused: bool) -> VideoAssignment {
        VideoAssignment {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            paused,
        }
    }

    /// A stream the SFU stops forwarding must reach the application as a pause.
    ///
    /// The server upserts the assignment when `paused` flips, with the same mid and the same
    /// track. `sync` used to return early on exactly that shape - "this slot already holds this
    /// track, nothing to do" - so every pause and resume was discarded before anything could see
    /// it. A client was then unable to tell a paused stream from a dead network, and rendered a
    /// blank tile where a placeholder belongs.
    #[test]
    fn a_pause_reaches_the_application_rather_than_being_swallowed() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("v0"));

        let first = slots.sync(update(
            vec![assignment("v0", "t1", false)],
            vec![track("t1")],
        ));
        assert_eq!(first.new_assignments.len(), 1, "the track is assigned");
        assert!(first.pause_changes.is_empty(), "nothing paused yet");
        assert!(!slots.is_paused("t1"));

        let paused = slots.sync(update(vec![assignment("v0", "t1", true)], Vec::new()));
        assert_eq!(
            paused.pause_changes,
            vec![("t1".to_owned(), true)],
            "the SFU stopped forwarding and the application must be told"
        );
        assert!(slots.is_paused("t1"));

        let resumed = slots.sync(update(vec![assignment("v0", "t1", false)], Vec::new()));
        assert_eq!(resumed.pause_changes, vec![("t1".to_owned(), false)]);
        assert!(!slots.is_paused("t1"));
    }

    /// Repeating a state is not a change.
    ///
    /// The server upserts assignments for reasons other than pause, and an application that
    /// redrew a placeholder on every notification would flicker.
    #[test]
    fn an_unchanged_pause_state_is_not_reported() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("v0"));
        slots.sync(update(
            vec![assignment("v0", "t1", true)],
            vec![track("t1")],
        ));

        let again = slots.sync(update(vec![assignment("v0", "t1", true)], Vec::new()));
        assert!(
            again.pause_changes.is_empty(),
            "the state did not move, so there is nothing to tell anyone"
        );
    }
}
