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
    /// Set when this slot carries audio, naming who is currently in it.
    ///
    /// Audio slots are shared: the SFU steals one the moment someone louder starts talking, so
    /// the occupant changes without the receiver asking for anything.
    speaker: Option<Speaker>,
}

/// A speaker the SFU is currently forwarding to us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speaker {
    pub participant_id: String,
    pub track_id: TrackId,
    /// 0 is the loudest currently-forwarded speaker.
    pub rank: u32,
    /// Most recent loudness in negative dBov: 0 is full scale, around -30 is ordinary speech.
    pub level_dbov: i32,
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
            speaker: None,
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

    /// Who is being heard, loudest first.
    pub fn speakers(&self) -> Vec<Speaker> {
        let mut speakers: Vec<Speaker> = self
            .slots
            .iter()
            .filter_map(|s| s.speaker.clone())
            .collect();
        speakers.sort_by_key(|s| s.rank);
        speakers
    }

    pub fn sync(&mut self, update: pulsebeam_proto::signaling::StateUpdate) -> SyncOutcome {
        let mut new_assignments: Vec<(Mid, Track)> = Vec::new();
        let mut audio_arrivals: Vec<(Mid, Track)> = Vec::new();
        let mut speakers_changed = false;
        let mut pause_changes: Vec<(TrackId, bool)> = Vec::new();
        let mut newly_discovered_tracks = Vec::new();
        let removed_tracks = update.tracks_remove.clone();

        for t in update.tracks_remove {
            self.pending_tracks.remove(&t);
            self.active_tracks.remove(&t);
            // And any slot still naming them. The assignment that vacates a slot usually arrives
            // too, but not always - a snapshot resync carries no removals by design - and a slot
            // left naming a departed track goes on reporting them as a speaker. The room saying
            // they are gone is enough on its own.
            for slot in &mut self.slots {
                if slot.track_id.as_deref() == Some(t.as_str()) {
                    speakers_changed |= slot.speaker.is_some();
                    slot.speaker = None;
                    slot.track_id = None;
                }
            }
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

        for a in update.audio_remove {
            if let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.as_bytes())
            {
                speakers_changed |= s.speaker.is_some();
                s.speaker = None;
                s.track_id = None;
            }
        }

        for a in update.audio_upsert {
            let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.mid.as_bytes())
            else {
                continue;
            };
            let speaker = Speaker {
                participant_id: a.participant_id.clone(),
                track_id: a.track_id.clone(),
                rank: a.rank,
                level_dbov: a.level_dbov,
            };
            speakers_changed |= s.speaker.as_ref() != Some(&speaker);
            s.speaker = Some(speaker);

            if s.track_id.as_deref() == Some(&a.track_id) {
                continue;
            }
            s.track_id = Some(a.track_id.clone());
            // Built from the assignment, not looked up in `tracks_upsert`: a speaker never
            // appears there. That list is video the client may subscribe to, and an audio entry
            // in it becomes a second tile for somebody who already has one.
            let mid = s.mid;
            audio_arrivals.push((
                mid,
                Track {
                    id: a.track_id.clone(),
                    kind: i32::from(pulsebeam_proto::signaling::TrackKind::Audio),
                    participant_id: a.participant_id.clone(),
                    meta: Default::default(),
                },
            ));
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
            speakers_changed,
            audio_arrivals,
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
    /// Whether who-is-being-heard moved. Read the ranking back with [`SlotManager::speakers`].
    pub speakers_changed: bool,
    /// Audio the SFU has decided to forward, per slot that changed occupant.
    ///
    /// Separate from `new_assignments` because nobody subscribed to these: the SFU chooses who is
    /// heard, and the speaker is described entirely by the assignment.
    pub audio_arrivals: Vec<(Mid, Track)>,
}

#[cfg(test)]
mod tests {
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
            audio_upsert: Vec::new(),
            audio_remove: Vec::new(),
        }
    }

    fn audio_update(
        audio: Vec<pulsebeam_proto::signaling::AudioAssignment>,
        tracks: Vec<ProtoTrack>,
    ) -> StateUpdate {
        StateUpdate {
            seq: 1,
            is_snapshot: false,
            tracks_upsert: tracks,
            tracks_remove: Vec::new(),
            assignments_upsert: Vec::new(),
            assignments_remove: Vec::new(),
            audio_upsert: audio,
            audio_remove: Vec::new(),
        }
    }

    fn speaking(
        mid: &str,
        track_id: &str,
        rank: u32,
        level_dbov: i32,
    ) -> pulsebeam_proto::signaling::AudioAssignment {
        // Mirrors the SFU: the speaker's identity travels in the assignment, never in a track.
        let participant_id = track_id.trim_start_matches("audio-").to_owned();
        pulsebeam_proto::signaling::AudioAssignment {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            participant_id,
            rank,
            level_dbov,
        }
    }

    fn audio_track(id: &str, publisher: &str) -> ProtoTrack {
        ProtoTrack {
            id: id.to_owned(),
            kind: 2,
            participant_id: publisher.to_owned(),
            meta: Default::default(),
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

    /// An audio slot binds its track the same way a video slot does, so the media has somewhere
    /// to go. Without this the packets arrive on a mid nothing is listening to and are dropped.
    #[test]
    fn an_audio_assignment_binds_the_track_to_its_slot() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));

        let sync = slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", 0, -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        assert_eq!(
            sync.new_assignments.len(),
            1,
            "the audio track must be bound to its mid"
        );
        assert!(sync.speakers_changed);
        assert_eq!(
            slots.speakers(),
            vec![Speaker {
                participant_id: "alice".to_owned(),
                track_id: "audio-alice".to_owned(),
                rank: 0,
                level_dbov: -30,
            }]
        );
    }

    /// The SFU steals a slot as soon as someone louder talks. The mid does not move, so unless
    /// the steal is applied the receiver goes on attributing the new voice to the old speaker.
    #[test]
    fn a_stolen_slot_rebinds_to_the_new_speaker() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", 0, -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        let sync = slots.sync(audio_update(
            vec![speaking("a0", "audio-bob", 0, -12)],
            vec![audio_track("audio-bob", "bob")],
        ));

        assert!(sync.speakers_changed);
        assert_eq!(
            sync.new_assignments.len(),
            1,
            "the new speaker needs its own delivery, not the one the old speaker held"
        );
        let speakers = slots.speakers();
        assert_eq!(speakers.len(), 1, "one slot holds one speaker");
        assert_eq!(speakers[0].participant_id, "bob");
    }

    /// Ranking is what a UI draws, and it must not depend on which order the slots were filled.
    #[test]
    fn speakers_are_reported_loudest_first() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.register(Mid::from("a1"));

        slots.sync(audio_update(
            vec![
                speaking("a0", "audio-alice", 1, -40),
                speaking("a1", "audio-bob", 0, -12),
            ],
            vec![
                audio_track("audio-alice", "alice"),
                audio_track("audio-bob", "bob"),
            ],
        ));

        let speakers = slots.speakers();
        assert_eq!(
            speakers
                .iter()
                .map(|s| s.participant_id.as_str())
                .collect::<Vec<_>>(),
            vec!["bob", "alice"]
        );
    }

    /// A speaker who stops talking frees the slot, and the receiver has to stop showing them.
    #[test]
    fn a_vacated_slot_stops_being_reported() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", 0, -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        let mut update = audio_update(Vec::new(), Vec::new());
        update.audio_remove = vec!["a0".to_owned()];
        let sync = slots.sync(update);

        assert!(sync.speakers_changed);
        assert!(slots.speakers().is_empty(), "nobody is being heard");
    }

    /// A speaker who leaves stops being reported, even if the slot is never explicitly vacated.
    ///
    /// The SFU normally sends both: the track goes from `tracks_remove` and the slot from
    /// `audio_remove`. A snapshot resync carries no removals at all, though, so a client that
    /// waited for the second would keep naming somebody who had left - a ghost speaker, and a
    /// tile for a person who is not in the room.
    #[test]
    fn a_departed_track_is_dropped_from_its_slot() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", 0, -30)],
            vec![audio_track("audio-alice", "alice")],
        ));
        assert_eq!(slots.speakers().len(), 1, "alice is being heard");

        let mut gone = audio_update(Vec::new(), Vec::new());
        gone.tracks_remove = vec!["audio-alice".to_owned()];
        let sync = slots.sync(gone);

        assert!(sync.speakers_changed, "the application has to be told");
        assert!(
            slots.speakers().is_empty(),
            "a speaker who left the room is not still being heard"
        );
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
