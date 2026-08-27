use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};

use crate::entity::TrackId;
use crate::log::{LogCtx, plog_info, plog_warn};
use crate::participant::intent::{AudioIntent, VideoIntent as Intent};
use crate::rtp::MediaSectionId as Mid;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling;
use pulsebeam_rtc::ChannelId;

const MAX_SIGNALING_MSG_SIZE: usize = 16 * 1024; // 16 KB (Signaling shouldn't be huge)

#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    #[error("Packet too large")]
    OversizedPacket,
    #[error("Invalid Protobuf format")]
    DecodeFailed,
    #[error("Request complexity limit exceeded")]
    ComplexityExceeded,
}

pub enum SignalingInputEvent {
    UpstreamTrackState { mid: Mid, active: bool },
}

#[derive(Clone)]
pub(crate) struct SignalingVideoBinding {
    pub(crate) mid: String,
    pub(crate) track_id: String,
    pub(crate) paused: bool,
}

#[derive(Clone)]
pub(crate) struct SignalingAudioBinding {
    pub(crate) mid: String,
    pub(crate) track_id: String,
    pub(crate) level_dbov: i32,
}

pub(crate) struct SignalingSnapshot {
    pub(crate) publications: Vec<crate::track::TrackMeta>,
    pub(crate) participants: HashSet<String>,
    pub(crate) video: Vec<SignalingVideoBinding>,
    pub(crate) audio: Vec<SignalingAudioBinding>,
}

pub(crate) struct SignalingIntents {
    pub(crate) video: Option<HashMap<Mid, Intent>>,
    pub(crate) audio: Option<AudioIntent>,
    pub(crate) playout_delay: Option<(u32, u32)>,
}

pub(crate) struct SignalingOutput {
    pub(crate) cid: ChannelId,
    pub(crate) bytes: Vec<u8>,
}

struct SignalingCommit {
    participants: HashSet<String>,
    publications: HashSet<String>,
    video: Vec<signaling::VideoBinding>,
    audio: Vec<(String, String)>,
    video_changed: bool,
    audio_changed: bool,
    force_full: bool,
}

/// The shape of the audio group, for deciding whether to resend it.
///
/// Loudness is deliberately absent: it moves with every packet, so including it
/// would make every packet a change. The client gets a fresh level whenever the
/// set or ordering of speakers moves, which is when it has something to redraw.
fn audio_shape(items: &[signaling::AudioBinding]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|binding| {
            debug_assert!(!binding.mid.is_empty());
            debug_assert!(!binding.track_id.is_empty());
            (binding.track_id.clone(), binding.mid.clone())
        })
        .collect()
}

pub struct Signaling {
    ctx: LogCtx,
    pub cid: Option<ChannelId>,
    slot_count: usize,
    audio_slot_count: usize,

    // Batch updates and only serialize when something moved.
    dirty_roster: bool,
    dirty_bindings: bool,
    full_state_retries: u8,

    /// What the client has been told. The roster is carried as a diff because it
    /// is the large set; the bindings are bounded by the subscriber's slots and
    /// are sent whole, so only their shape is kept, to skip an unchanged group.
    previous_participants: HashSet<String>,
    participants: HashSet<String>,
    previous_publications: HashSet<String>,
    previous_video: Vec<signaling::VideoBinding>,
    previous_audio: Vec<(String, String)>,

    last_client_intents: Option<HashMap<Mid, Intent>>,
    last_audio_intent: Option<AudioIntent>,
    last_playout_delay: Option<(u32, u32)>,
    pending_commit: Option<SignalingCommit>,
}

impl Signaling {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            cid: None,
            dirty_roster: true,
            dirty_bindings: true,
            full_state_retries: 0,
            previous_participants: HashSet::new(),
            participants: HashSet::new(),
            previous_publications: HashSet::new(),
            previous_video: Vec::new(),
            previous_audio: Vec::new(),
            last_client_intents: None,
            last_audio_intent: None,
            last_playout_delay: None,
            pending_commit: None,

            slot_count: 0,
            audio_slot_count: 0,
        }
    }

    pub fn set_cid(&mut self, cid: ChannelId) {
        self.cid = Some(cid);
        self.dirty_roster = true;
        self.dirty_bindings = true;
        self.full_state_retries = 2;
    }

    pub fn set_slot_count(&mut self, slot_count: usize) {
        self.slot_count = slot_count;
    }

    pub fn set_audio_slot_count(&mut self, slot_count: usize) {
        self.audio_slot_count = slot_count;
    }

    pub(crate) fn reconcile(&self) -> SignalingIntents {
        SignalingIntents {
            video: self.last_client_intents.clone(),
            audio: self.last_audio_intent.clone(),
            playout_delay: self.last_playout_delay,
        }
    }

    pub fn handle_input(
        &mut self,
        data: &[u8],
    ) -> Result<Vec<SignalingInputEvent>, SignalingError> {
        let mut events = Vec::new();
        if data.len() > MAX_SIGNALING_MSG_SIZE {
            plog_warn!(
                self.ctx,
                len = data.len(),
                "Fatal: Oversized signaling message"
            );
            return Err(SignalingError::OversizedPacket);
        }

        let Ok(msg) = signaling::ClientMessage::decode(data) else {
            plog_warn!(self.ctx, "Fatal: Invalid Protobuf");
            return Err(SignalingError::DecodeFailed);
        };

        match msg.payload {
            Some(signaling::client_message::Payload::Intent(intent)) => {
                if intent.video.len() > self.slot_count {
                    plog_warn!(self.ctx, "Fatal: Complexity limit exceeded");
                    return Err(SignalingError::ComplexityExceeded);
                }
                for state in &intent.publish {
                    events.push(SignalingInputEvent::UpstreamTrackState {
                        mid: Mid::from(state.mid.as_str()),
                        active: state.active,
                    });
                }
                plog_info!(self.ctx, "received client intent: {:?}", intent);
                self.apply_client_intent(intent);
                self.dirty_bindings = true;
            }
            None => {}
        }

        Ok(events)
    }

    fn apply_client_intent(&mut self, intent: signaling::ClientIntent) {
        let mut intents = HashMap::with_capacity(intent.video.len());
        for req in intent.video {
            let track_id_str = req.track_id.clone();
            let Ok(track_id) = TrackId::try_from(track_id_str.clone()) else {
                plog_warn!(self.ctx, track_id = %track_id_str, "invalid track_id in client intent");
                continue;
            };

            // `Mid` is a fixed-size (16-byte) identifier and will truncate longer strings.
            let mid = Mid::from(req.mid.as_str());

            if req.height == 0 {
                continue;
            }

            intents.insert(
                mid,
                Intent {
                    track_id,
                    target_height: req.height,
                    min_height: req.min_height.min(req.height),
                    min_fps: req.min_fps,
                    priority: req.priority,
                },
            );
        }
        if let Some(audio) = intent.audio {
            let audio = self.decode_audio_intent(audio);
            self.last_audio_intent = Some(audio);
        }
        self.last_playout_delay = intent
            .ext
            .and_then(|ext| ext.playout_delay)
            .map(|p| (p.min_ms, p.max_ms));
        self.last_client_intents = Some(intents);
    }

    /// Pins past the negotiated slot count are dropped rather than rejected.
    ///
    /// A client cannot hear more speakers than it has audio mids for, so the
    /// extras could never be honoured; failing the whole intent over them would
    /// take the client's video requests down with it.
    fn decode_audio_intent(&self, audio: signaling::AudioIntent) -> AudioIntent {
        let mut pinned = Vec::with_capacity(audio.pinned.len().min(self.audio_slot_count));
        for id in audio.pinned {
            if pinned.len() >= self.audio_slot_count {
                plog_warn!(
                    self.ctx,
                    slots = self.audio_slot_count,
                    "audio intent pins more tracks than there are slots; ignoring the rest"
                );
                break;
            }
            match TrackId::try_from(id.clone()) {
                Ok(track_id) => pinned.push(track_id),
                Err(_) => {
                    plog_warn!(self.ctx, track_id = %id, "invalid track_id in audio intent");
                }
            }
        }
        AudioIntent {
            pinned,
            auto: audio.auto,
        }
    }

    pub fn mark_tracks_dirty(&mut self) {
        self.dirty_roster = true;
        self.full_state_retries = 2;
    }

    pub fn mark_assignments_dirty(&mut self) {
        self.dirty_bindings = true;
    }

    pub(crate) fn participants_snapshot(&self) -> HashSet<String> {
        self.participants.clone()
    }

    pub(crate) fn needs_poll(&self) -> bool {
        self.cid.is_some()
            && self.pending_commit.is_none()
            && (self.dirty_roster || self.dirty_bindings)
    }

    pub fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = crate::entity::ParticipantId>,
        removed: impl IntoIterator<Item = crate::entity::ParticipantId>,
    ) {
        for participant in added {
            self.participants.insert(participant.as_str());
        }
        for participant in removed {
            self.participants.remove(&participant.as_str());
        }
        self.dirty_roster = true;
        self.full_state_retries = 2;
    }

    pub(crate) fn poll(&mut self, snapshot: &SignalingSnapshot) -> Option<SignalingOutput> {
        if !self.needs_poll() {
            return None;
        }

        let cid = self.cid?;

        // The roster: every publication the client could ask for, and the people
        // behind them. Video and audio both, because a pin has to be able to
        // name an audio track before anybody has heard it.
        let mut publications = Vec::new();
        let mut participants = Vec::new();
        let seen_participants = snapshot.participants.clone();
        for meta in &snapshot.publications {
            let participant_id = meta.origin.as_str();
            publications.push(signaling::Publication {
                track_id: meta.id.as_str(),
                participant_id,
                kind: match meta.id.kind() {
                    crate::entity::TrackKind::Video => signaling::TrackKind::Video,
                    crate::entity::TrackKind::Audio => signaling::TrackKind::Audio,
                    // Data does not travel as a track; it has its own lanes.
                    crate::entity::TrackKind::Data => continue,
                }
                .into(),
            });
        }
        participants.extend(seen_participants.iter().cloned());

        let current_publication_ids: HashSet<String> = publications
            .iter()
            .map(|publication| publication.track_id.clone())
            .collect();
        debug_assert_eq!(current_publication_ids.len(), publications.len());

        let force_full = self.full_state_retries != 0;
        let participants_added: Vec<signaling::Participant> = participants
            .iter()
            .filter(|id| force_full || !self.previous_participants.contains(*id))
            .map(|id| signaling::Participant {
                participant_id: id.clone(),
            })
            .collect();
        let participants_removed: Vec<String> = self
            .previous_participants
            .difference(&seen_participants)
            .cloned()
            .collect();
        let publications_added: Vec<signaling::Publication> = publications
            .into_iter()
            .filter(|publication| {
                force_full || !self.previous_publications.contains(&publication.track_id)
            })
            .collect();
        let publications_removed: Vec<String> = self
            .previous_publications
            .difference(&current_publication_ids)
            .cloned()
            .collect();

        // The bindings: bounded by the subscriber's slots, so each group is sent
        // whole or not at all. Audio moves an order of magnitude more often than
        // video, which is why they are separate groups.
        let current_video: Vec<signaling::VideoBinding> = snapshot
            .video
            .iter()
            .map(|s| signaling::VideoBinding {
                mid: s.mid.clone(),
                track_id: s.track_id.clone(),
                paused: s.paused,
            })
            .collect();
        let current_audio: Vec<signaling::AudioBinding> = snapshot
            .audio
            .iter()
            .map(|h| signaling::AudioBinding {
                mid: h.mid.clone(),
                track_id: h.track_id.clone(),
                level_dbov: h.level_dbov,
            })
            .collect();
        let current_audio_shape = audio_shape(&current_audio);

        let video_changed = force_full || current_video != self.previous_video;
        let audio_changed = force_full || current_audio_shape != self.previous_audio;

        let roster_changed = !participants_added.is_empty()
            || !participants_removed.is_empty()
            || !publications_added.is_empty()
            || !publications_removed.is_empty();
        if !force_full && !roster_changed && !video_changed && !audio_changed {
            self.dirty_roster = false;
            self.dirty_bindings = false;
            return None;
        }

        let state = signaling::ServerState {
            participants_added,
            participants_removed,
            publications_added,
            publications_removed,
            video: video_changed.then(|| signaling::VideoBindings {
                items: current_video.clone(),
            }),
            audio: audio_changed.then_some(signaling::AudioBindings {
                items: current_audio,
            }),
            snapshot: force_full,
        };

        let msg = signaling::ServerMessage {
            payload: Some(signaling::server_message::Payload::State(state)),
        };
        let buf = msg.encode_to_vec();

        self.pending_commit = Some(SignalingCommit {
            participants: seen_participants,
            publications: current_publication_ids,
            video: current_video,
            audio: current_audio_shape,
            video_changed,
            audio_changed,
            force_full,
        });
        Some(SignalingOutput { cid, bytes: buf })
    }

    pub(crate) fn commit_sent(&mut self) {
        let Some(commit) = self.pending_commit.take() else {
            debug_assert!(false, "signaling commit requires a pending output");
            return;
        };
        self.previous_participants = commit.participants;
        self.previous_publications = commit.publications;
        if commit.video_changed {
            self.previous_video = commit.video;
        }
        if commit.audio_changed {
            self.previous_audio = commit.audio;
        }
        if commit.force_full {
            self.full_state_retries = self.full_state_retries.saturating_sub(1);
        }
        self.dirty_roster = self.full_state_retries != 0;
        self.dirty_bindings = self.full_state_retries != 0;
    }

    pub(crate) fn retry_pending(&mut self) {
        let _ = self.pending_commit.take();
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn audio(mid: &str, track_id: &str, level_dbov: i32) -> signaling::AudioBinding {
        signaling::AudioBinding {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            level_dbov,
        }
    }

    fn video(mid: &str, track_id: &str, paused: bool) -> signaling::VideoBinding {
        signaling::VideoBinding {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            paused,
        }
    }

    /// A slot steal has to reach the client: the mid and the SSRC do not move, so
    /// nothing else tells it the voice it is hearing belongs to someone new.
    #[test]
    fn a_new_speaker_in_a_slot_is_an_audio_change() {
        assert_ne!(
            audio_shape(&[audio("a0", "audio-a", -30)]),
            audio_shape(&[audio("a0", "audio-b", -30)])
        );
    }

    /// Reordering is what a UI draws, so it counts even when nobody was replaced.
    /// The list order is the rank, so this is the only thing that carries it.
    #[test]
    fn a_reordering_is_an_audio_change() {
        let louder_first = [audio("a0", "audio-a", -20), audio("a1", "audio-b", -40)];
        let swapped = [audio("a1", "audio-b", -20), audio("a0", "audio-a", -40)];
        assert_ne!(audio_shape(&louder_first), audio_shape(&swapped));
    }

    /// Loudness moves with every packet. If it were a trigger, a room with two
    /// people talking would produce a signalling message per packet - so it rides
    /// along on updates caused by something else and never causes one itself.
    #[test]
    fn loudness_alone_is_not_an_audio_change() {
        assert_eq!(
            audio_shape(&[audio("a0", "audio-a", -30)]),
            audio_shape(&[audio("a0", "audio-a", -12)])
        );
    }

    #[test]
    fn a_first_sighting_is_an_audio_change() {
        assert_ne!(
            audio_shape(&[]),
            audio_shape(&[audio("a0", "audio-a", -30)])
        );
    }

    /// A speaker falling silent empties the group, and an empty group is still
    /// sent - present-but-empty is how the client is told nothing is bound.
    #[test]
    fn a_slot_falling_silent_is_an_audio_change() {
        assert_ne!(
            audio_shape(&[audio("a0", "audio-a", -30)]),
            audio_shape(&[])
        );
    }

    /// Video bindings are compared whole, so both halves of an assignment count.
    #[test]
    fn track_replacement_is_a_video_change() {
        assert_ne!(
            vec![video("7", "track-a", false)],
            vec![video("7", "track-b", false)]
        );
    }

    #[test]
    fn paused_transition_is_a_video_change() {
        assert_ne!(
            vec![video("7", "track-a", true)],
            vec![video("7", "track-a", false)]
        );
    }

    /// The default is what a client that has never mentioned audio gets, and it
    /// has to be what the SFU did before the message existed.
    #[test]
    fn the_default_audio_intent_is_auto_with_no_pins() {
        let intent = AudioIntent::default();
        assert!(intent.auto);
        assert!(intent.pinned.is_empty());
    }

    #[test]
    fn snapshots_are_requested_only_while_signaling_can_emit() {
        let room = crate::entity::ExternalRoomId::new("room").expect("valid room");
        let ctx = LogCtx {
            room_id: crate::entity::RoomId::from_external(&room),
            participant_id: crate::entity::ParticipantId::new(),
        };
        let mut signaling = Signaling::new(ctx);
        let snapshot = SignalingSnapshot {
            publications: Vec::new(),
            participants: HashSet::new(),
            video: Vec::new(),
            audio: Vec::new(),
        };

        assert!(!signaling.needs_poll(), "a channel is required");

        signaling.set_cid(pulsebeam_rtc::ChannelId::new(1));
        assert!(signaling.needs_poll(), "a dirty channel needs a snapshot");

        assert!(signaling.poll(&snapshot).is_some(), "initial state emits");
        assert!(
            !signaling.needs_poll(),
            "the pending commit owns the snapshot"
        );

        signaling.retry_pending();
        assert!(signaling.needs_poll(), "a failed write retries the state");

        assert!(signaling.poll(&snapshot).is_some(), "first retry emits");
        signaling.commit_sent();
        assert!(signaling.needs_poll(), "full state is retried once more");

        assert!(signaling.poll(&snapshot).is_some(), "second retry emits");
        signaling.commit_sent();
        assert!(!signaling.needs_poll(), "a clean state needs no snapshot");
    }
}
