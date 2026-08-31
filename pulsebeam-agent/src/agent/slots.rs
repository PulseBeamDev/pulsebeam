use pulsebeam_proto::signaling::Publication;
use std::collections::{HashMap, HashSet};
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
    pending_tracks: HashMap<TrackId, Publication>,
    active_tracks: HashMap<TrackId, Publication>,
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
        debug_assert!(!self.slots.iter().any(|slot| slot.mid == mid));
        if self.slots.iter().any(|slot| slot.mid == mid) {
            return;
        }
        self.slots.push(ReceiverSlot {
            paused: false,
            speaker: None,
            mid,
            track_id: None,
        });
    }

    pub fn replace_slots(&mut self, mids: Vec<Mid>) {
        for (track_id, publication) in self.active_tracks.drain() {
            self.pending_tracks.entry(track_id).or_insert(publication);
        }
        self.slots = mids
            .into_iter()
            .map(|mid| ReceiverSlot {
                mid,
                track_id: None,
                paused: false,
                speaker: None,
            })
            .collect();
    }

    /// A track the SFU has told us about, whether or not it currently occupies a slot.
    ///
    /// A subscription the viewer has hidden (`target_height = 0`) is never assigned a slot, so
    /// `assigned` cannot see it, but the publication itself is still known.
    pub fn known(&self, track_id: &str) -> Option<Publication> {
        self.active_tracks
            .get(track_id)
            .or_else(|| self.pending_tracks.get(track_id))
            .cloned()
    }

    pub fn assigned(&self, track_id: &str) -> Option<(Mid, Publication)> {
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

    pub fn sync(&mut self, mut state: pulsebeam_proto::signaling::ServerState) -> SyncOutcome {
        let mut new_assignments: Vec<(Mid, Publication)> = Vec::new();
        let mut audio_arrivals: Vec<(Mid, Publication)> = Vec::new();
        let mut speakers_changed = false;
        let mut pause_changes: Vec<(TrackId, bool)> = Vec::new();
        let mut newly_discovered_tracks = Vec::new();
        if state.snapshot {
            let present: HashSet<_> = state
                .publications_added
                .iter()
                .map(|publication| publication.track_id.as_str())
                .collect();
            state.publications_removed.extend(
                self.pending_tracks
                    .keys()
                    .chain(self.active_tracks.keys())
                    .filter(|track_id| !present.contains(track_id.as_str()))
                    .cloned(),
            );
            state.publications_removed.sort_unstable();
            state.publications_removed.dedup();
        }
        let removed_tracks = state.publications_removed.clone();

        for id in state.publications_removed {
            self.pending_tracks.remove(&id);
            self.active_tracks.remove(&id);
            // And any slot still naming it. The binding that vacates the slot
            // usually arrives in the same message, but a publication being gone
            // is enough on its own - a slot left naming a departed track goes on
            // reporting them as a speaker.
            for slot in &mut self.slots {
                if slot.track_id.as_deref() == Some(id.as_str()) {
                    speakers_changed |= slot.speaker.is_some();
                    slot.speaker = None;
                    slot.track_id = None;
                }
            }
        }

        for publication in state.publications_added {
            if self.active_tracks.contains_key(&publication.track_id)
                || self.pending_tracks.contains_key(&publication.track_id)
            {
                continue;
            }
            newly_discovered_tracks.push(publication.clone());
            self.pending_tracks
                .insert(publication.track_id.clone(), publication);
        }

        // Each binding group is complete when present, so the slots it does not
        // name are vacant. That is what removes the need for a removal list.
        if let Some(video) = state.video {
            let mut bound = Vec::new();
            for slot in &mut self.slots {
                if slot.speaker.is_some() {
                    continue;
                }
                let binding = video
                    .items
                    .iter()
                    .find(|b| slot.mid.as_bytes() == b.mid.as_bytes());
                let Some(binding) = binding else {
                    slot.track_id = None;
                    continue;
                };
                let rebound = slot.track_id.as_deref() != Some(&binding.track_id);
                if !rebound {
                    if slot.paused != binding.paused {
                        slot.paused = binding.paused;
                        pause_changes.push((binding.track_id.clone(), binding.paused));
                    }
                } else {
                    slot.track_id = Some(binding.track_id.clone());
                    slot.paused = binding.paused;
                    if binding.paused {
                        pause_changes.push((binding.track_id.clone(), true));
                    }
                }
                bound.push((slot.mid, binding.track_id.clone(), rebound));
            }
            for (mid, track_id, rebound) in bound {
                if let Some(publication) = self.active_tracks.get(&track_id).cloned() {
                    if rebound {
                        new_assignments.push((mid, publication));
                    }
                    continue;
                }
                if let Some(publication) = self.pending_tracks.remove(&track_id) {
                    self.active_tracks.insert(track_id, publication.clone());
                    new_assignments.push((mid, publication));
                }
            }
        }

        // Ordered loudest first, so the index is the rank. There is no rank on
        // the wire to disagree with the ordering.
        if let Some(audio) = state.audio {
            let mut bound = Vec::new();
            for slot in &mut self.slots {
                let found = audio
                    .items
                    .iter()
                    .enumerate()
                    .find(|(_, b)| slot.mid.as_bytes() == b.mid.as_bytes());
                let Some((rank, binding)) = found else {
                    if slot.speaker.take().is_some() {
                        speakers_changed = true;
                        slot.track_id = None;
                    }
                    continue;
                };
                let rebound = slot.track_id.as_deref() != Some(&binding.track_id);
                slot.track_id = Some(binding.track_id.clone());
                bound.push((
                    slot.mid,
                    rank,
                    binding.track_id.clone(),
                    binding.level_dbov,
                    rebound,
                ));
            }
            for (mid, rank, track_id, level_dbov, rebound) in bound {
                let publication = self
                    .pending_tracks
                    .remove(&track_id)
                    .or_else(|| self.active_tracks.get(&track_id).cloned());
                let participant_id = publication
                    .as_ref()
                    .map(|p| p.participant_id.clone())
                    .unwrap_or_default();
                let speaker = Speaker {
                    participant_id,
                    track_id: track_id.clone(),
                    rank: u32::try_from(rank).unwrap_or(u32::MAX),
                    level_dbov,
                };
                if let Some(slot) = self.slots.iter_mut().find(|s| s.mid == mid) {
                    speakers_changed |= slot.speaker.as_ref() != Some(&speaker);
                    slot.speaker = Some(speaker);
                }
                // The publication is in the roster now, so this is a lookup
                // rather than a track synthesised from the binding.
                if let Some(publication) = publication {
                    self.active_tracks.insert(track_id, publication.clone());
                    if rebound {
                        audio_arrivals.push((mid, publication));
                    }
                }
            }
        }

        // A slot can be told which track it carries before the publication that
        // describes it arrives, so binding only at the moment the group is
        // applied loses the delivery entirely and the subscriber waits forever.
        // Restricted to slots with no speaker: an audio slot is served by
        // `audio_arrivals`, and reporting it here as well would insert a second
        // delivery target for one mid, silently replacing the first.
        let pending: Vec<(Mid, TrackId)> = self
            .slots
            .iter()
            .filter(|slot| slot.speaker.is_none())
            .filter_map(|slot| Some((slot.mid, slot.track_id.clone()?)))
            .filter(|(_, track_id)| !self.active_tracks.contains_key(track_id))
            .collect();
        for (mid, track_id) in pending {
            let Some(publication) = self.pending_tracks.remove(&track_id) else {
                continue;
            };
            self.active_tracks.insert(track_id, publication.clone());
            new_assignments.push((mid, publication));
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
    pub new_assignments: Vec<(Mid, Publication)>,
    pub newly_discovered_tracks: Vec<pulsebeam_proto::signaling::Publication>,
    pub removed_tracks: Vec<TrackId>,
    /// Tracks whose forwarding state changed, and what it changed to.
    pub pause_changes: Vec<(TrackId, bool)>,
    /// Whether who-is-being-heard moved. Read the ranking back with [`SlotManager::speakers`].
    pub speakers_changed: bool,
    /// Audio the SFU has decided to forward, per slot that changed occupant.
    ///
    /// Separate from `new_assignments` because nobody subscribed to these: the SFU chooses who is
    /// heard, and the speaker is described entirely by the assignment.
    pub audio_arrivals: Vec<(Mid, Publication)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_proto::signaling::{
        AudioBinding, AudioBindings, Publication as ProtoTrack, ServerState, VideoBinding,
        VideoBindings,
    };

    fn update(video: Vec<VideoBinding>, tracks: Vec<ProtoTrack>) -> ServerState {
        ServerState {
            participants_added: Vec::new(),
            participants_removed: Vec::new(),
            publications_added: tracks,
            publications_removed: Vec::new(),
            video: Some(VideoBindings { items: video }),
            audio: None,
            snapshot: false,
        }
    }

    fn audio_update(audio: Vec<AudioBinding>, tracks: Vec<ProtoTrack>) -> ServerState {
        ServerState {
            participants_added: Vec::new(),
            participants_removed: Vec::new(),
            publications_added: tracks,
            publications_removed: Vec::new(),
            video: None,
            audio: Some(AudioBindings { items: audio }),
            snapshot: false,
        }
    }

    /// Ordered loudest first by the caller: the index is the rank, so these
    /// helpers take no rank and the list order carries it.
    fn speaking(mid: &str, track_id: &str, level_dbov: i32) -> AudioBinding {
        AudioBinding {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            level_dbov,
        }
    }

    fn audio_track(id: &str, publisher: &str) -> ProtoTrack {
        ProtoTrack {
            track_id: id.to_owned(),
            kind: 2,
            participant_id: publisher.to_owned(),
        }
    }

    fn track(id: &str) -> ProtoTrack {
        ProtoTrack {
            track_id: id.to_owned(),
            kind: 1,
            participant_id: "pub".to_owned(),
        }
    }

    fn assignment(mid: &str, track_id: &str, paused: bool) -> VideoBinding {
        VideoBinding {
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

    #[test]
    fn an_empty_snapshot_withdraws_tracks_missing_from_the_roster() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("v0"));
        slots.sync(update(
            vec![assignment("v0", "t1", false)],
            vec![track("t1")],
        ));
        assert!(slots.known("t1").is_some());

        let mut snapshot = update(Vec::new(), Vec::new());
        snapshot.snapshot = true;
        let outcome = slots.sync(snapshot);

        assert_eq!(outcome.removed_tracks, vec!["t1"]);
        assert!(slots.known("t1").is_none());
        assert!(slots.assigned("t1").is_none());
    }

    #[test]
    fn returning_to_an_active_track_rebinds_the_receiver_slot() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("v0"));
        slots.sync(update(
            vec![assignment("v0", "a", false)],
            vec![track("a"), track("b")],
        ));
        slots.sync(update(vec![assignment("v0", "b", false)], Vec::new()));

        let returned = slots.sync(update(vec![assignment("v0", "a", false)], Vec::new()));

        assert_eq!(returned.new_assignments.len(), 1);
        assert_eq!(returned.new_assignments[0].0, Mid::from("v0"));
        assert_eq!(returned.new_assignments[0].1.track_id, "a");
    }

    /// An audio slot binds its track the same way a video slot does, so the media has somewhere
    /// to go. Without this the packets arrive on a mid nothing is listening to and are dropped.
    #[test]
    fn an_audio_assignment_binds_the_track_to_its_slot() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));

        let sync = slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        assert_eq!(
            sync.audio_arrivals.len(),
            1,
            "the audio track must be bound to its mid"
        );
        assert!(
            sync.new_assignments.is_empty(),
            "an audio binding is not a subscription: reporting it as both would insert two \
             delivery targets for one mid, the second silently replacing the sink the first \
             handed to the application"
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
            vec![speaking("a0", "audio-alice", -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        let sync = slots.sync(audio_update(
            vec![speaking("a0", "audio-bob", -12)],
            vec![audio_track("audio-bob", "bob")],
        ));

        assert!(sync.speakers_changed);
        assert_eq!(
            sync.audio_arrivals.len(),
            1,
            "the new speaker needs its own delivery, not the one the old speaker held"
        );
        let speakers = slots.speakers();
        assert_eq!(speakers.len(), 1, "one slot holds one speaker");
        assert_eq!(speakers[0].participant_id, "bob");
    }

    /// Ranking is what a UI draws, and the list order is the only thing carrying it.
    ///
    /// There is no rank field on the wire any more, so a client that reordered the group - by mid,
    /// by arrival, by anything - would silently invent its own ranking. The server sends loudest
    /// first and the client must preserve exactly that, whichever slots they landed in.
    #[test]
    fn speakers_are_reported_loudest_first() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.register(Mid::from("a1"));

        slots.sync(audio_update(
            vec![
                speaking("a1", "audio-bob", -12),
                speaking("a0", "audio-alice", -40),
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
            vec![speaking("a0", "audio-alice", -30)],
            vec![audio_track("audio-alice", "alice")],
        ));

        // The group is complete when present, so an empty one vacates every
        // slot. That is what removes the need for a removal list.
        let sync = slots.sync(audio_update(Vec::new(), Vec::new()));

        assert!(sync.speakers_changed);
        assert!(slots.speakers().is_empty(), "nobody is being heard");
    }

    /// A speaker who leaves stops being reported, even if the slot is never explicitly vacated.
    ///
    /// The publication going away is enough on its own. Waiting for the audio group to vacate the
    /// slot as well would leave a client naming somebody who had left in the window between the
    /// two - a ghost speaker, and a tile for a person who is not in the room.
    #[test]
    fn a_departed_track_is_dropped_from_its_slot() {
        let mut slots = SlotManager::new();
        slots.register(Mid::from("a0"));
        slots.sync(audio_update(
            vec![speaking("a0", "audio-alice", -30)],
            vec![audio_track("audio-alice", "alice")],
        ));
        assert_eq!(slots.speakers().len(), 1, "alice is being heard");

        let mut gone = audio_update(Vec::new(), Vec::new());
        gone.audio = None;
        gone.publications_removed = vec!["audio-alice".to_owned()];
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
