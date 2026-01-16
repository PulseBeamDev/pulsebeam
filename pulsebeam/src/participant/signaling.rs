use std::collections::HashSet;

use crate::entity;
use crate::participant::downstream::DownstreamAllocator;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling;
use str0m::Rtc;
use str0m::channel::ChannelId;
use str0m::media::Mid;

const CHANNEL_LABEL: &str = "__internal/v1/signaling";

const MAX_SIGNALING_MSG_SIZE: usize = 16 * 1024; // 16 KB (Signaling shouldn't be huge)
const MAX_INTENT_REQUESTS: usize = 50; // Max video tracks a user can subscribe to at once

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
    pub cid: Option<ChannelId>,
    seq: u64,

    // Dirty flags allow us to batch updates and only serialize when necessary
    dirty_tracks: bool,
    dirty_assignments: bool,

    // Forces the next update to be a full snapshot (e.g. on connect or resync)
    pending_snapshot_request: bool,

    // STATE CACHE: Required to calculate removals (deltas)
    // We store the IDs of the objects sent in the last successful update.
    previous_track_ids: HashSet<String>,
    previous_assignment_mids: HashSet<String>,
}

impl Signaling {
    pub fn new() -> Self {
        Self {
            cid: None,
            seq: 0,
            dirty_tracks: false,
            dirty_assignments: false,
            pending_snapshot_request: false,
            // Initialize empty sets
            previous_track_ids: HashSet::new(),
            previous_assignment_mids: HashSet::new(),
        }
    }

    pub fn handle_channel_open(&mut self, cid: ChannelId, label: String) {
        if label == CHANNEL_LABEL {
            self.cid = Some(cid);
            self.pending_snapshot_request = true;
            self.dirty_tracks = true;
            self.dirty_assignments = true;
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
                if intent.requests.len() > MAX_INTENT_REQUESTS {
                    tracing::warn!("Fatal: Complexity limit exceeded");
                    return Err(SignalingError::ComplexityExceeded);
                }
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
        let mut requested_mids = std::collections::HashSet::new();

        // Configure requested slots
        for req in intent.requests {
            if req.mid.len() > 16 {
                continue;
            }
            if entity::validate_track_id(&req.track_id).is_err() {
                continue;
            }

            let mid = Mid::from(req.mid.as_str());
            requested_mids.insert(mid);

            downstream
                .video
                .configure_slot(mid, req.track_id, req.height);
        }

        // Clear unrequested slots (Garbage Collect)
        // We query the allocator for what is CURRENTLY active
        let active_mids: Vec<Mid> = downstream.video.slots().map(|s| s.mid).collect();

        for mid in active_mids {
            // If the client didn't ask for it in this intent, kill it.
            if !requested_mids.contains(&mid) {
                downstream.video.clear_slot(mid);
            }
        }

        self.mark_assignments_dirty();
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
        let Some(cid) = self.cid else {
            return false;
        };

        // If nothing is dirty, do nothing
        if !self.dirty_tracks && !self.dirty_assignments {
            return false;
        }

        let Some(mut channel) = rtc.channel(cid) else {
            return false;
        };

        // 1. Prepare Current State (The "Truth")
        // We gather all currently active tracks and assignments.
        let current_tracks: Vec<signaling::Track> = downstream
            .video
            .tracks()
            .map(|t| signaling::Track {
                id: t.id.to_string(),
                kind: signaling::TrackKind::Video.into(),
                participant_id: t.origin_participant.to_string(),
                meta: Default::default(),
            })
            .collect();

        let current_assignments: Vec<signaling::VideoAssignment> = downstream
            .video
            .slots()
            .map(|s| signaling::VideoAssignment {
                mid: s.mid.to_string(),
                track_id: s.track.id.to_string(),
                paused: false,
            })
            .collect();

        // 2. Identify Keys for Diffing
        let current_track_ids: HashSet<String> =
            current_tracks.iter().map(|t| t.id.clone()).collect();
        let current_assign_mids: HashSet<String> =
            current_assignments.iter().map(|a| a.mid.clone()).collect();

        // 3. Compute Deltas (Removals)
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

        // 4. Construct the Update
        self.seq += 1;
        let update = signaling::StateUpdate {
            seq: self.seq,
            is_snapshot: self.pending_snapshot_request,

            // Upserts: In this model, we send ALL active items.
            // This self-heals any metadata changes (paused state, display names, etc).
            // A "Strict" delta would also filter upserts, but that requires deep equality checks.
            tracks_upsert: current_tracks,
            tracks_remove,

            assignments_upsert: current_assignments,
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
