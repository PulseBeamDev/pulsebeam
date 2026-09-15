use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};

use crate::entity::TrackId;
use crate::log::{LogCtx, plog_info, plog_warn};
use crate::participant::intent::{AudioIntent, VideoIntent as Intent};
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling;
use pulsebeam_proto::signaling_v1 as media_signaling;
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
    pub(crate) participants: HashMap<String, String>,
    pub(crate) video: Vec<SignalingVideoBinding>,
    pub(crate) audio: Vec<SignalingAudioBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(
    dead_code,
    reason = "candidate replacement signaling is intentionally not routed in this slice"
)]
pub(crate) enum CatalogBuildError {
    #[error("catalog publication has no application label")]
    MissingLabel,
    #[error("catalog publication references a participant outside the room view")]
    UnknownParticipant,
    #[error("catalog contains duplicate application identity")]
    DuplicateIdentity,
}

#[allow(
    dead_code,
    reason = "candidate replacement signaling is intentionally not routed in this slice"
)]
pub(crate) fn build_catalog(
    recipient: crate::entity::ParticipantId,
    snapshot: &SignalingSnapshot,
) -> Result<media_signaling::CatalogSnapshot, CatalogBuildError> {
    let mut external_ids = HashSet::new();
    let mut participants: Vec<_> = snapshot
        .participants
        .iter()
        .filter(|(id, _)| id.as_str() != recipient.as_str())
        .map(|(id, external_id)| {
            if !external_ids.insert(external_id.clone()) {
                return Err(CatalogBuildError::DuplicateIdentity);
            }
            Ok(media_signaling::Participant {
                participant_id: id.clone(),
                participant_external_id: external_id.clone(),
            })
        })
        .collect::<Result<_, _>>()?;
    participants.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));

    let mut track_ids = HashSet::new();
    let mut selectors = HashSet::new();
    let mut tracks = Vec::new();
    for meta in &snapshot.publications {
        if meta.origin == recipient || meta.id.kind() == crate::entity::TrackKind::Data {
            continue;
        }
        let participant_id = meta.origin.as_str();
        if !snapshot.participants.contains_key(&participant_id) {
            return Err(CatalogBuildError::UnknownParticipant);
        }
        let Some(label) = meta.label.clone() else {
            return Err(CatalogBuildError::MissingLabel);
        };
        let kind = match meta.id.kind() {
            crate::entity::TrackKind::Audio => media_signaling::TrackKind::Audio,
            crate::entity::TrackKind::Video => media_signaling::TrackKind::Video,
            crate::entity::TrackKind::Data => continue,
        };
        if !track_ids.insert(meta.id)
            || !selectors.insert((meta.origin, kind as i32, label.clone()))
        {
            return Err(CatalogBuildError::DuplicateIdentity);
        }
        tracks.push(media_signaling::RemoteTrack {
            track_id: meta.id.as_str(),
            participant_id,
            kind: kind.into(),
            label,
        });
    }
    tracks.sort_by(|left, right| left.track_id.cmp(&right.track_id));

    Ok(media_signaling::CatalogSnapshot {
        participants,
        tracks,
    })
}

/// A decoded v1 Intent after omission/default and first-occurrence handling.
/// Track identities deliberately remain wire strings here: unavailable and
/// wrong-kind entries are retained until a later catalog reconciliation.
#[derive(Clone)]
pub(crate) struct V1Intent {
    pub(crate) revision: u64,
    pub(crate) publications: Vec<crate::participant::intent::NativePublication>,
    pub(crate) video: Vec<V1VideoIntent>,
    pub(crate) audio: Vec<V1AudioIntent>,
    pub(crate) audio_auto: bool,
}

#[derive(Clone)]
pub(crate) struct V1VideoIntent {
    pub(crate) track_id: String,
    pub(crate) target_height: u32,
    pub(crate) min_height: u32,
    pub(crate) min_fps: u32,
    pub(crate) priority: u32,
    pub(crate) playout: crate::participant::downstream::PlayoutPolicy,
}

#[derive(Clone)]
pub(crate) struct V1AudioIntent {
    pub(crate) track_id: String,
    pub(crate) playout: crate::participant::downstream::PlayoutPolicy,
}

pub(crate) enum V1IntentResult {
    Mapping(media_signaling::Mapping),
    ProtocolError(media_signaling::Error),
    Reconnect,
}

fn v1_playout(
    delay: Option<media_signaling::PlayoutDelay>,
) -> crate::participant::downstream::PlayoutPolicy {
    delay
        .map(|delay| {
            crate::participant::downstream::PlayoutPolicy::fixed((delay.min_ms, delay.max_ms))
        })
        .unwrap_or(crate::participant::downstream::PlayoutPolicy::Default)
}

pub(crate) fn normalize_v1_intent(intent: media_signaling::Intent) -> V1Intent {
    let publications = intent
        .send
        .unwrap_or_default()
        .tracks
        .into_iter()
        .map(|track| crate::participant::intent::NativePublication {
            sender_index: Some(track.sender_index),
            kind: match media_signaling::TrackKind::try_from(track.kind) {
                Ok(media_signaling::TrackKind::Audio) => Some(crate::entity::TrackKind::Audio),
                Ok(media_signaling::TrackKind::Video) => Some(crate::entity::TrackKind::Video),
                Ok(media_signaling::TrackKind::Unspecified) | Err(_) => None,
            },
            label: track.label,
        })
        .collect();
    let receive = intent.receive.unwrap_or_default();
    let mut video_ids = HashSet::new();
    let video = receive
        .video
        .unwrap_or_default()
        .tracks
        .into_iter()
        .filter_map(|track| {
            video_ids.insert(track.track_id.clone()).then(|| {
                let options = track.options.unwrap_or_default();
                V1VideoIntent {
                    track_id: track.track_id,
                    target_height: options.height,
                    min_height: options.min_height,
                    min_fps: options.min_fps,
                    priority: options.priority,
                    playout: v1_playout(options.playout_delay),
                }
            })
        })
        .collect();
    let audio = receive.audio.unwrap_or_default();
    let audio_auto = !matches!(
        media_signaling::AudioMode::try_from(audio.mode),
        Ok(media_signaling::AudioMode::ExplicitOnly)
    );
    let mut audio_ids = HashSet::new();
    let audio = audio
        .tracks
        .into_iter()
        .filter_map(|track| {
            audio_ids
                .insert(track.track_id.clone())
                .then(|| V1AudioIntent {
                    track_id: track.track_id,
                    playout: v1_playout(track.options.and_then(|options| options.playout_delay)),
                })
        })
        .collect();
    V1Intent {
        revision: intent.revision,
        publications,
        video,
        audio,
        audio_auto,
    }
}

#[cfg(test)]
mod v1_intent_tests {
    use super::*;

    #[test]
    fn omission_replaces_every_section_with_defaults() {
        let normalized = normalize_v1_intent(media_signaling::Intent {
            revision: 1,
            send: None,
            receive: None,
        });

        assert!(normalized.publications.is_empty());
        assert!(normalized.video.is_empty());
        assert!(normalized.audio.is_empty());
        assert!(normalized.audio_auto);
    }
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
    participants: HashMap<String, String>,
    previous_publications: HashSet<String>,
    previous_video: Vec<signaling::VideoBinding>,
    previous_audio: Vec<(String, String)>,

    last_client_intents: Option<HashMap<Mid, Intent>>,
    last_audio_intent: Option<AudioIntent>,
    last_playout_delay: Option<(u32, u32)>,
    pending_commit: Option<SignalingCommit>,
    v1_revision: u64,
    v1_intent: Option<V1Intent>,
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
            participants: HashMap::new(),
            previous_publications: HashSet::new(),
            previous_video: Vec::new(),
            previous_audio: Vec::new(),
            last_client_intents: None,
            last_audio_intent: None,
            last_playout_delay: None,
            pending_commit: None,
            v1_revision: 0,
            v1_intent: None,

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

    pub(crate) fn v1_is_fresh(&self, revision: u64) -> bool {
        revision != 0 && revision > self.v1_revision
    }

    pub(crate) fn accept_v1_intent(&mut self, intent: V1Intent) {
        debug_assert!(self.v1_is_fresh(intent.revision));
        self.v1_revision = intent.revision;
        self.v1_intent = Some(intent);
    }

    pub(crate) fn v1_revision(&self) -> u64 {
        self.v1_revision
    }

    pub(crate) fn v1_intent(&self) -> Option<&V1Intent> {
        self.v1_intent.as_ref()
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

    pub(crate) fn participants_snapshot(&self) -> HashMap<String, String> {
        self.participants.clone()
    }

    pub(crate) fn needs_poll(&self) -> bool {
        self.cid.is_some()
            && self.pending_commit.is_none()
            && (self.dirty_roster || self.dirty_bindings)
    }

    pub fn apply_participants(
        &mut self,
        added: impl IntoIterator<Item = crate::participant::RoomParticipant>,
        removed: impl IntoIterator<Item = crate::entity::ParticipantId>,
    ) {
        for participant in added {
            self.participants.insert(
                participant.id.as_str(),
                participant.external_id.as_str().to_owned(),
            );
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
        let seen_participant_ids: HashSet<String> = snapshot.participants.keys().cloned().collect();
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
        participants.extend(seen_participant_ids.iter().cloned());

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
            .difference(&seen_participant_ids)
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
            participants: seen_participant_ids,
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
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
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
        let room = crate::entity::RoomExternalId::new("room").expect("valid room");
        let ctx = LogCtx {
            room_id: crate::entity::RoomId::from_external(&room),
            participant_id: crate::entity::ParticipantId::new(),
        };
        let mut signaling = Signaling::new(ctx);
        let snapshot = SignalingSnapshot {
            publications: Vec::new(),
            participants: HashMap::new(),
            video: Vec::new(),
            audio: Vec::new(),
        };

        assert!(!signaling.needs_poll(), "a channel is required");

        let mut rtc = str0m::Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        signaling.set_cid(cid);
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

    #[test]
    fn replacement_catalog_is_complete_labeled_remote_state() {
        let room = crate::entity::RoomId::from_external(
            &crate::entity::RoomExternalId::new("room").unwrap(),
        );
        let recipient = crate::entity::ParticipantId::derive(
            &room,
            &crate::entity::ParticipantExternalId::new("self").unwrap(),
        );
        let remote = crate::entity::ParticipantId::derive(
            &room,
            &crate::entity::ParticipantExternalId::new("alice").unwrap(),
        );
        let video = crate::track::TrackMeta::labeled_media(
            room,
            crate::id::ShardId::new(1),
            remote,
            crate::entity::TrackKind::Video,
            "camera".to_owned(),
        );
        let audio = crate::track::TrackMeta::labeled_media(
            room,
            crate::id::ShardId::new(2),
            remote,
            crate::entity::TrackKind::Audio,
            "camera".to_owned(),
        );
        let self_track = crate::track::TrackMeta::labeled_media(
            room,
            crate::id::ShardId::new(0),
            recipient,
            crate::entity::TrackKind::Audio,
            "mic".to_owned(),
        );
        let snapshot = SignalingSnapshot {
            publications: vec![self_track, video.clone(), audio.clone()],
            participants: HashMap::from_iter([
                (remote.as_str(), "alice".to_owned()),
                (recipient.as_str(), "self".to_owned()),
            ]),
            video: Vec::new(),
            audio: Vec::new(),
        };

        let catalog = build_catalog(recipient, &snapshot).unwrap();

        assert_eq!(catalog.participants.len(), 1);
        assert_eq!(catalog.participants[0].participant_external_id, "alice");
        assert_eq!(catalog.tracks.len(), 2);
        assert!(
            catalog
                .tracks
                .iter()
                .all(|track| track.participant_id == remote.as_str())
        );
        assert!(catalog.tracks.iter().any(|track| {
            track.kind == media_signaling::TrackKind::Audio as i32 && track.label == "camera"
        }));
        assert!(catalog.tracks.iter().any(|track| {
            track.kind == media_signaling::TrackKind::Video as i32 && track.label == "camera"
        }));
        assert_eq!(
            video.id,
            remote.derive_track_id(crate::entity::TrackKind::Video, "camera")
        );
        assert_eq!(
            audio.id,
            remote.derive_track_id(crate::entity::TrackKind::Audio, "camera")
        );

        let republished = crate::track::TrackMeta::labeled_media(
            room,
            crate::id::ShardId::new(9),
            remote,
            crate::entity::TrackKind::Video,
            "camera".to_owned(),
        );
        let renamed = crate::track::TrackMeta::labeled_media(
            room,
            crate::id::ShardId::new(9),
            remote,
            crate::entity::TrackKind::Video,
            "screen".to_owned(),
        );
        assert_eq!(republished.id, video.id);
        assert_ne!(renamed.id, video.id);
    }

    #[test]
    fn unlabeled_legacy_media_cannot_enter_replacement_catalog() {
        let participant = crate::entity::ParticipantId::new();
        let (upstream, _) = crate::track::test_utils::make_audio_track(
            participant,
            str0m::media::Mid::from("legacy-mid"),
        );
        let snapshot = SignalingSnapshot {
            publications: vec![upstream.meta],
            participants: HashMap::from_iter([(participant.as_str(), "legacy".to_owned())]),
            video: Vec::new(),
            audio: Vec::new(),
        };

        assert_eq!(
            build_catalog(crate::entity::ParticipantId::new(), &snapshot),
            Err(CatalogBuildError::MissingLabel)
        );
    }
}
