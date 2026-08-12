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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviousAssignment {
    track_id: String,
    paused: bool,
}

/// What was last signalled for one audio slot.
///
/// Loudness is deliberately absent: it moves with every packet, so including it would make every
/// packet a change. See `audio_assignment_changed`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviousAudioAssignment {
    track_id: String,
    rank: u32,
}

fn audio_assignment_changed(
    previous: Option<&PreviousAudioAssignment>,
    current: &signaling::AudioAssignment,
) -> bool {
    debug_assert!(!current.mid.is_empty());
    debug_assert!(!current.track_id.is_empty());
    previous.is_none_or(|previous| {
        previous.track_id != current.track_id || previous.rank != current.rank
    })
}

fn assignment_changed(
    previous: Option<&PreviousAssignment>,
    current: &signaling::VideoAssignment,
) -> bool {
    debug_assert!(!current.mid.is_empty());
    debug_assert!(!current.track_id.is_empty());
    previous.is_none_or(|previous| {
        previous.track_id != current.track_id || previous.paused != current.paused
    })
}

pub struct Signaling {
    ctx: LogCtx,
    pub cid: Option<ChannelId>,
    seq: u64,
    slot_count: usize,

    // Dirty flags allow us to batch updates and only serialize when necessary
    dirty_tracks: bool,
    dirty_assignments: bool,

    // Forces the next update to be a full snapshot (e.g. on connect or resync)
    pending_snapshot_request: bool,

    // STATE CACHE: Required to calculate removals (deltas)
    // We store the IDs of the objects sent in the last successful update.
    previous_track_ids: HashSet<String>,
    previous_assignments: HashMap<String, PreviousAssignment>,
    previous_audio_assignments: HashMap<String, PreviousAudioAssignment>,
    last_client_intents: Option<HashMap<Mid, Intent>>,
}

impl Signaling {
    pub(crate) fn new(ctx: LogCtx) -> Self {
        Self {
            ctx,
            cid: None,
            seq: 0,
            dirty_tracks: false,
            dirty_assignments: false,
            pending_snapshot_request: true,
            // Initialize empty sets
            previous_track_ids: HashSet::new(),
            previous_assignments: HashMap::new(),
            previous_audio_assignments: HashMap::new(),
            last_client_intents: None,

            slot_count: 0,
        }
    }

    pub fn set_cid(&mut self, cid: ChannelId) {
        self.cid = Some(cid);
    }

    pub fn set_slot_count(&mut self, slot_count: usize) {
        self.slot_count = slot_count;
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
                if intent.downstream_requests.len() > self.slot_count {
                    plog_warn!(self.ctx, "Fatal: Complexity limit exceeded");
                    return Err(SignalingError::ComplexityExceeded);
                }
                for state in &intent.upstream_intents {
                    events.push(SignalingInputEvent::UpstreamTrackState {
                        mid: Mid::from(state.mid.as_str()),
                        active: state.active,
                    });
                }
                plog_info!(self.ctx, "received client intent: {:?}", intent);
                self.apply_client_intent(intent, downstream);
                self.dirty_assignments = true;
            }
            Some(signaling::client_message::Payload::RequestSync(_)) => {
                self.request_full_sync();
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
        let mut intents = HashMap::with_capacity(intent.downstream_requests.len());
        for req in intent.downstream_requests {
            let track_id_str = req.track_id.clone();
            let Ok(track_id) = TrackId::try_from(track_id_str.clone()) else {
                plog_warn!(self.ctx, track_id = %track_id_str, "invalid track_id in client intent");
                continue;
            };

            // `Mid` is a fixed-size (16-byte) identifier and will truncate longer strings.
            let mid = Mid::from(req.mid.as_str());

            if req.target_height == 0 {
                continue;
            }

            intents.insert(
                mid,
                Intent {
                    track_id,
                    target_height: req.target_height,
                    min_height: req.min_height.min(req.target_height),
                    min_fps: req.min_fps,
                    priority: req.priority,
                },
            );
        }
        downstream.set_playout_delay(intent.playout_delay.map(|p| (p.min_ms, p.max_ms)));
        self.last_client_intents = Some(intents);
        self.reconcile(downstream);
    }

    pub fn mark_tracks_dirty(&mut self) {
        self.dirty_tracks = true;
    }

    pub fn mark_assignments_dirty(&mut self) {
        self.dirty_assignments = true;
    }

    pub fn request_full_sync(&mut self) {
        self.pending_snapshot_request = true;
        self.dirty_tracks = true;
        self.dirty_assignments = true;
    }

    pub fn poll(&mut self, rtc: &mut Rtc, downstream: &DownstreamAllocator) -> bool {
        // If nothing is dirty, do nothing
        if !self.dirty_tracks && !self.dirty_assignments {
            return false;
        }

        let Some(cid) = self.cid else {
            return false;
        };

        let Some(mut channel) = rtc.channel(cid) else {
            return false;
        };

        // 1. Prepare Current State (The "Truth")
        // We gather all currently active tracks and assignments.
        let heard = downstream.audio_assignments();
        let current_tracks: Vec<signaling::Track> = downstream
            .video
            .tracks()
            .map(|t| signaling::Track {
                id: t.id.as_str(),
                kind: signaling::TrackKind::Video.into(),
                participant_id: t.origin.as_str(),
                meta: Default::default(),
            })
            .collect();
        // Audio deliberately does *not* appear in `tracks_upsert`. That list means "video tracks
        // you can subscribe to", and clients treat every entry as one - announcing a speaker there
        // gave them a second, video-shaped track and put the same person on screen twice. Who is
        // speaking rides in the assignment instead, which needs no subscription because the SFU
        // decides who is forwarded.

        let current_audio: Vec<signaling::AudioAssignment> = heard
            .iter()
            .enumerate()
            .map(|(rank, h)| signaling::AudioAssignment {
                mid: h.mid.to_string(),
                track_id: h.origin.track.as_str(),
                participant_id: h.origin.participant.as_str(),
                rank: u32::try_from(rank).unwrap_or(u32::MAX),
                level_dbov: i32::from(h.level_dbov),
            })
            .collect();

        let current_assignments: Vec<signaling::VideoAssignment> = downstream
            .video
            .slots()
            .map(|s| signaling::VideoAssignment {
                mid: s.mid.to_string(),
                track_id: s.track.id.as_str(),
                paused: s.paused,
            })
            .collect();

        // 2. Identify Keys for Diffing
        let current_track_ids: HashSet<String> =
            current_tracks.iter().map(|t| t.id.clone()).collect();
        let current_assign_map: HashMap<String, PreviousAssignment> = current_assignments
            .iter()
            .map(|a| {
                debug_assert!(!a.mid.is_empty());
                debug_assert!(!a.track_id.is_empty());
                (
                    a.mid.clone(),
                    PreviousAssignment {
                        track_id: a.track_id.clone(),
                        paused: a.paused,
                    },
                )
            })
            .collect();
        debug_assert_eq!(current_assign_map.len(), current_assignments.len());
        let current_audio_map: HashMap<String, PreviousAudioAssignment> = current_audio
            .iter()
            .map(|a| {
                (
                    a.mid.clone(),
                    PreviousAudioAssignment {
                        track_id: a.track_id.clone(),
                        rank: a.rank,
                    },
                )
            })
            .collect();
        debug_assert_eq!(current_audio_map.len(), current_audio.len());

        // 3. Compute Deltas
        // If snapshot: removals are empty.
        // If delta: removals = previous - current.
        let (tracks_remove, assignments_remove, audio_remove) = if self.pending_snapshot_request {
            (vec![], vec![], vec![])
        } else {
            (
                self.previous_track_ids
                    .difference(&current_track_ids)
                    .cloned()
                    .collect(),
                self.previous_assignments
                    .keys()
                    .filter(|mid| !current_assign_map.contains_key(*mid))
                    .cloned()
                    .collect(),
                self.previous_audio_assignments
                    .keys()
                    .filter(|mid| !current_audio_map.contains_key(*mid))
                    .cloned()
                    .collect(),
            )
        };
        let (tracks_upsert, assignments_upsert, audio_upsert) = if self.pending_snapshot_request {
            (current_tracks, current_assignments, current_audio)
        } else {
            let track_ids_upsert: HashSet<String> = current_track_ids
                .difference(&self.previous_track_ids)
                .cloned()
                .collect();

            (
                current_tracks
                    .into_iter()
                    .filter(|t| track_ids_upsert.contains(&t.id))
                    .collect(),
                // Upsert when the mid, track, or paused state changed.
                current_assignments
                    .into_iter()
                    .filter(|a| assignment_changed(self.previous_assignments.get(&a.mid), a))
                    .collect(),
                current_audio
                    .into_iter()
                    .filter(|a| {
                        audio_assignment_changed(self.previous_audio_assignments.get(&a.mid), a)
                    })
                    .collect(),
            )
        };

        // 4. Construct the Update
        self.seq = self.seq.saturating_add(1);
        let update = signaling::StateUpdate {
            seq: self.seq,
            is_snapshot: self.pending_snapshot_request,

            tracks_upsert,
            tracks_remove,

            assignments_upsert,
            assignments_remove,

            audio_upsert,
            audio_remove,
        };

        let msg = signaling::ServerMessage {
            payload: Some(signaling::server_message::Payload::Update(update)),
        };

        let buf = msg.encode_to_vec();

        // 5. Send and Commit State
        if let Err(e) = channel.write(true, &buf) {
            plog_warn!(self.ctx, "Failed to write signaling: {:?}", e);
            // DO NOT reset flags or state; retry next poll
            return false;
        }

        // Write succeeded: Update our "Previous" state to match "Current"
        self.previous_track_ids = current_track_ids;
        self.previous_assignments = current_assign_map;
        self.previous_audio_assignments = current_audio_map;

        // Reset flags
        self.dirty_tracks = false;
        self.dirty_assignments = false;
        self.pending_snapshot_request = false;
        true
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
    use super::*;

    fn audio(mid: &str, track_id: &str, rank: u32, level_dbov: i32) -> signaling::AudioAssignment {
        signaling::AudioAssignment {
            mid: mid.to_owned(),
            track_id: track_id.to_owned(),
            participant_id: "pa_test".to_owned(),
            rank,
            level_dbov,
        }
    }

    /// A slot steal has to reach the client: the mid and the SSRC do not move, so nothing else
    /// tells it the voice it is hearing belongs to someone new.
    #[test]
    fn a_new_speaker_in_a_slot_is_an_audio_change() {
        let previous = PreviousAudioAssignment {
            track_id: "audio-a".to_owned(),
            rank: 0,
        };
        assert!(audio_assignment_changed(
            Some(&previous),
            &audio("a0", "audio-b", 0, -30)
        ));
    }

    /// Reordering is what a UI draws, so it counts even when nobody was replaced.
    #[test]
    fn a_reordering_is_an_audio_change() {
        let previous = PreviousAudioAssignment {
            track_id: "audio-a".to_owned(),
            rank: 0,
        };
        assert!(audio_assignment_changed(
            Some(&previous),
            &audio("a0", "audio-a", 1, -30)
        ));
    }

    /// Loudness moves with every packet. If it were a trigger, a room with two people talking
    /// would produce a signalling message per packet - so it rides along on updates caused by
    /// something else and never causes one itself.
    #[test]
    fn loudness_alone_is_not_an_audio_change() {
        let previous = PreviousAudioAssignment {
            track_id: "audio-a".to_owned(),
            rank: 0,
        };
        assert!(!audio_assignment_changed(
            Some(&previous),
            &audio("a0", "audio-a", 0, -12)
        ));
    }

    #[test]
    fn a_first_sighting_is_an_audio_change() {
        assert!(audio_assignment_changed(
            None,
            &audio("a0", "audio-a", 0, -30)
        ));
    }

    #[test]
    fn track_replacement_is_an_assignment_change() {
        let previous = PreviousAssignment {
            track_id: "track-a".to_owned(),
            paused: false,
        };
        let replacement = signaling::VideoAssignment {
            mid: "7".to_owned(),
            track_id: "track-b".to_owned(),
            paused: false,
        };

        assert!(assignment_changed(Some(&previous), &replacement));
    }

    #[test]
    fn paused_transition_is_an_assignment_change() {
        let previous = PreviousAssignment {
            track_id: "track-a".to_owned(),
            paused: true,
        };
        let resumed = signaling::VideoAssignment {
            mid: "7".to_owned(),
            track_id: "track-a".to_owned(),
            paused: false,
        };

        assert!(assignment_changed(Some(&previous), &resumed));
    }
}
