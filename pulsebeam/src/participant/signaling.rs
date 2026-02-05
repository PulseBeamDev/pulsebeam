use std::collections::{HashMap, HashSet};

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

pub struct Signaling {
    pub cid: ChannelId,
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
    previous_assignment_mids: HashSet<String>,
    last_client_intents: Option<HashMap<Mid, Intent>>,
}

impl Signaling {
    pub fn new(cid: ChannelId) -> Self {
        Self {
            cid,
            seq: 0,
            dirty_tracks: false,
            dirty_assignments: false,
            pending_snapshot_request: true,
            // Initialize empty sets
            previous_track_ids: HashSet::new(),
            previous_assignment_mids: HashSet::new(),
            last_client_intents: None,

            slot_count: 0,
        }
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
    ) -> Result<(), SignalingError> {
        if data.len() > MAX_SIGNALING_MSG_SIZE {
            tracing::warn!(len = data.len(), "Fatal: Oversized signaling message");
            return Err(SignalingError::OversizedPacket);
        }

        let Ok(msg) = signaling::ClientMessage::decode(data) else {
            tracing::warn!("Fatal: Invalid Protobuf");
            return Err(SignalingError::DecodeFailed);
        };

        match msg.payload {
            Some(signaling::client_message::Payload::Intent(intent)) => {
                if intent.requests.len() > self.slot_count {
                    tracing::warn!("Fatal: Complexity limit exceeded");
                    return Err(SignalingError::ComplexityExceeded);
                }
                tracing::info!("received client intent: {:?}", intent);
                self.apply_client_intent(intent, downstream);
                self.dirty_assignments = true;
            }
            Some(signaling::client_message::Payload::RequestSync(_)) => {
                self.request_full_sync();
            }
            None => {}
        }

        Ok(())
    }

    fn apply_client_intent(
        &mut self,
        intent: signaling::ClientIntent,
        downstream: &mut DownstreamAllocator,
    ) {
        let mut intents = HashMap::with_capacity(intent.requests.len());
        for req in intent.requests {
            if req.mid.len() > 16 {
                continue;
            }
            let Ok(track_id) = req.track_id.try_into() else {
                continue;
            };

            let mid = Mid::from(req.mid.as_str());

            if req.height == 0 {
                continue;
            }

            intents.insert(
                mid,
                Intent {
                    track_id,
                    max_height: req.height,
                },
            );
        }
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

        let Some(mut channel) = rtc.channel(self.cid) else {
            return false;
        };

        // 1. Prepare Current State (The "Truth")
        // We gather all currently active tracks and assignments.
        let current_tracks: Vec<signaling::Track> = downstream
            .video
            .tracks()
            .map(|t| signaling::Track {
                id: t.id.as_str(),
                kind: signaling::TrackKind::Video.into(),
                participant_id: t.origin_participant.as_str(),
                meta: Default::default(),
            })
            .collect();

        let current_assignments: Vec<signaling::VideoAssignment> = downstream
            .video
            .slots()
            .map(|s| signaling::VideoAssignment {
                mid: s.mid.to_string(),
                track_id: s.track.id.as_str(),
                paused: false,
            })
            .collect();

        // 2. Identify Keys for Diffing
        let current_track_ids: HashSet<String> =
            current_tracks.iter().map(|t| t.id.clone()).collect();
        let current_assign_mids: HashSet<String> =
            current_assignments.iter().map(|a| a.mid.clone()).collect();

        // 3. Compute Deltas
        // If snapshot: removals are empty.
        // If delta: removals = previous - current.
        let (tracks_remove, assignments_remove) = if self.pending_snapshot_request {
            (vec![], vec![])
        } else {
            (
                self.previous_track_ids
                    .difference(&current_track_ids)
                    .cloned()
                    .collect(),
                self.previous_assignment_mids
                    .difference(&current_assign_mids)
                    .cloned()
                    .collect(),
            )
        };
        let (tracks_upsert, assignments_upsert) = if self.pending_snapshot_request {
            (current_tracks, current_assignments)
        } else {
            let track_ids_upsert: HashSet<String> = current_track_ids
                .difference(&self.previous_track_ids)
                .cloned()
                .collect();
            let assignment_mids_upsert: HashSet<String> = current_assign_mids
                .difference(&self.previous_assignment_mids)
                .cloned()
                .collect();

            (
                current_tracks
                    .into_iter()
                    .filter(|t| track_ids_upsert.contains(&t.id))
                    .collect(),
                current_assignments
                    .into_iter()
                    .filter(|a| assignment_mids_upsert.contains(&a.mid))
                    .collect(),
            )
        };

        // 4. Construct the Update
        self.seq += 1;
        let update = signaling::StateUpdate {
            seq: self.seq,
            is_snapshot: self.pending_snapshot_request,

            tracks_upsert,
            tracks_remove,

            assignments_upsert,
            assignments_remove,
        };

        let msg = signaling::ServerMessage {
            payload: Some(signaling::server_message::Payload::Update(update)),
        };

        let buf = msg.encode_to_vec();

        // 5. Send and Commit State
        if let Err(e) = channel.write(true, &buf) {
            tracing::warn!("Failed to write signaling: {:?}", e);
            // DO NOT reset flags or state; retry next poll
            return false;
        }

        // Write succeeded: Update our "Previous" state to match "Current"
        self.previous_track_ids = current_track_ids;
        self.previous_assignment_mids = current_assign_mids;

        // Reset flags
        self.dirty_tracks = false;
        self.dirty_assignments = false;
        self.pending_snapshot_request = false;
        true
    }
}
