use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};

use crate::entity::TrackId;
use crate::log::{LogCtx, plog_info, plog_warn};
use crate::participant::downstream::{DownstreamAllocator, Intent};
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling;
use str0m::Rtc;
use str0m::channel::ChannelId;
use str0m::media::Mid;

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

use crate::participant::downstream::AudioIntent;

pub struct Signaling {
    ctx: LogCtx,
    pub cid: Option<ChannelId>,
    slot_count: usize,
    audio_slot_count: usize,

    // Batch updates and only serialize when something moved.
    dirty_roster: bool,
    dirty_bindings: bool,

    /// What the client has been told. The roster is carried as a diff because it
    /// is the large set; the bindings are bounded by the subscriber's slots and
    /// are sent whole, so only their shape is kept, to skip an unchanged group.
    previous_participants: HashSet<String>,
    previous_publications: HashSet<String>,
    previous_video: Vec<signaling::VideoBinding>,
    previous_audio: Vec<(String, String)>,

    last_client_intents: Option<HashMap<Mid, Intent>>,
    last_audio_intent: Option<AudioIntent>,
}

impl Signaling {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            cid: None,
            dirty_roster: true,
            dirty_bindings: true,
            previous_participants: HashSet::new(),
            previous_publications: HashSet::new(),
            previous_video: Vec::new(),
            previous_audio: Vec::new(),
            last_client_intents: None,
            last_audio_intent: None,

            slot_count: 0,
            audio_slot_count: 0,
        }
    }

    pub fn set_cid(&mut self, cid: ChannelId) {
        self.cid = Some(cid);
    }

    pub fn set_slot_count(&mut self, slot_count: usize) {
        self.slot_count = slot_count;
    }

    pub fn set_audio_slot_count(&mut self, slot_count: usize) {
        self.audio_slot_count = slot_count;
    }

    pub fn reconcile(&mut self, downstream: &mut DownstreamAllocator) {
        if let Some(last_client_intents) = &self.last_client_intents {
            downstream.video.configure(last_client_intents);
            self.mark_assignments_dirty();
        }
    }

    pub fn handle_input(
        &mut self,
        data: &[u8],
        downstream: &mut DownstreamAllocator,
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
                self.apply_client_intent(intent, downstream);
                self.dirty_bindings = true;
            }
            None => {}
        }

        Ok(events)
    }

    fn apply_client_intent(
        &mut self,
        intent: signaling::ClientIntent,
        downstream: &mut DownstreamAllocator,
    ) {
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
            downstream.set_audio_intent(audio.clone());
            self.last_audio_intent = Some(audio);
        }
        downstream.set_playout_delay(
            intent
                .ext
                .and_then(|ext| ext.playout_delay)
                .map(|p| (p.min_ms, p.max_ms)),
        );
        self.last_client_intents = Some(intents);
        self.reconcile(downstream);
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
    }

    pub fn mark_assignments_dirty(&mut self) {
        self.dirty_bindings = true;
    }

    pub fn poll(&mut self, rtc: &mut Rtc, downstream: &DownstreamAllocator) -> bool {
        if !self.dirty_roster && !self.dirty_bindings {
            return false;
        }

        let Some(cid) = self.cid else {
            return false;
        };

        let Some(mut channel) = rtc.channel(cid) else {
            return false;
        };

        // The roster: every publication the client could ask for, and the people
        // behind them. Video and audio both, because a pin has to be able to
        // name an audio track before anybody has heard it.
        let mut publications = Vec::new();
        let mut participants = Vec::new();
        let mut seen_participants = HashSet::new();
        for meta in downstream.video.tracks().chain(downstream.audio_tracks()) {
            let participant_id = meta.origin.as_str();
            if seen_participants.insert(participant_id.clone()) {
                participants.push(participant_id.clone());
            }
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

        let current_publication_ids: HashSet<String> = publications
            .iter()
            .map(|publication| publication.track_id.clone())
            .collect();
        debug_assert_eq!(current_publication_ids.len(), publications.len());

        let participants_added: Vec<signaling::Participant> = participants
            .iter()
            .filter(|id| !self.previous_participants.contains(*id))
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
            .filter(|publication| !self.previous_publications.contains(&publication.track_id))
            .collect();
        let publications_removed: Vec<String> = self
            .previous_publications
            .difference(&current_publication_ids)
            .cloned()
            .collect();

        // The bindings: bounded by the subscriber's slots, so each group is sent
        // whole or not at all. Audio moves an order of magnitude more often than
        // video, which is why they are separate groups.
        let current_video: Vec<signaling::VideoBinding> = downstream
            .video
            .slots()
            .map(|s| signaling::VideoBinding {
                mid: s.mid.to_string(),
                track_id: s.track.id.as_str(),
                paused: s.paused,
            })
            .collect();
        let current_audio: Vec<signaling::AudioBinding> = downstream
            .audio_assignments()
            .iter()
            .map(|h| signaling::AudioBinding {
                mid: h.mid.to_string(),
                track_id: h.origin.track.as_str(),
                level_dbov: i32::from(h.level_dbov),
            })
            .collect();
        let current_audio_shape = audio_shape(&current_audio);

        let video_changed = current_video != self.previous_video;
        let audio_changed = current_audio_shape != self.previous_audio;

        let roster_changed = !participants_added.is_empty()
            || !participants_removed.is_empty()
            || !publications_added.is_empty()
            || !publications_removed.is_empty();
        if !roster_changed && !video_changed && !audio_changed {
            self.dirty_roster = false;
            self.dirty_bindings = false;
            return false;
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
        };

        let msg = signaling::ServerMessage {
            payload: Some(signaling::server_message::Payload::State(state)),
        };
        let buf = msg.encode_to_vec();

        if let Err(e) = channel.write(true, &buf) {
            plog_warn!(self.ctx, "Failed to write signaling: {:?}", e);
            // Nothing is committed, so the next poll rebuilds and retries. This
            // is the only reason the diff is safe without a resync path: an
            // unsent update is not a lost one.
            return false;
        }

        self.previous_participants = seen_participants;
        self.previous_publications = current_publication_ids;
        if video_changed {
            self.previous_video = current_video;
        }
        if audio_changed {
            self.previous_audio = current_audio_shape;
        }
        self.dirty_roster = false;
        self.dirty_bindings = false;
        true
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
}
