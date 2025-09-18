use std::collections::HashMap;
use std::sync::Arc;

use str0m::media::{MediaKind, Mid};

use crate::{
    entity::{EntityId, ParticipantId, TrackId},
    message::TrackMeta,
    proto::sfu as proto_sfu,
    track,
};

/// Represents an outgoing track slot that a client can subscribe to.
#[derive(Debug)]
pub struct MidOutSlot {
    pub track_id: Option<Arc<TrackId>>,
}

/// Represents a track (usually from another participant) that is available for subscription.
#[derive(Debug, Clone)]
pub struct TrackOut {
    pub track: track::TrackHandle,
    /// The MID assigned to this track if it is actively being sent to this participant.
    pub mid: Option<Mid>,
}

/// Manages all state related to a participant's tracks.
#[derive(Debug, Default)]
pub struct ParticipantState {
    pub participant_id: Arc<ParticipantId>,

    // Tracks published BY this participant
    pub published_video_tracks: HashMap<Mid, track::TrackHandle>,
    pub published_audio_tracks: HashMap<Mid, track::TrackHandle>,

    // Subscription slots requested BY this participant
    pub subscribed_video_tracks: HashMap<Mid, MidOutSlot>,
    pub subscribed_audio_tracks: HashMap<Mid, MidOutSlot>,

    // Tracks available to be subscribed to (from the Room)
    pub available_video_tracks: HashMap<Arc<EntityId>, TrackOut>,
    pub available_audio_tracks: HashMap<Arc<EntityId>, TrackOut>,

    // State sync with client
    pub pending_published_tracks: Vec<proto_sfu::TrackInfo>,
    pub pending_unpublished_tracks: Vec<EntityId>,
    pub should_resync: bool,
}

impl ParticipantState {
    pub fn new(participant_id: Arc<ParticipantId>) -> Self {
        Self {
            participant_id,
            ..Default::default()
        }
    }

    /// Handles a batch of newly available tracks from the room.
    pub fn handle_new_tracks(&mut self, tracks: &HashMap<Arc<TrackId>, track::TrackHandle>) {
        for track in tracks.values() {
            if track.meta.id.origin_participant == self.participant_id {
                self.add_self_published_track(track.clone());
            } else {
                self.add_available_track(track.clone());
            }
        }
    }

    fn add_self_published_track(&mut self, track: track::TrackHandle) {
        let meta = &track.meta;
        let published_map = match meta.kind {
            MediaKind::Video => &mut self.published_video_tracks,
            MediaKind::Audio => &mut self.published_audio_tracks,
        };
        published_map.insert(meta.id.origin_mid, track);
    }

    fn add_available_track(&mut self, track: track::TrackHandle) {
        let track_id = track.meta.id.clone();
        let track_out = TrackOut { track, mid: None };

        let (available_map, kind) = match track_out.track.meta.kind {
            MediaKind::Video => (
                &mut self.available_video_tracks,
                proto_sfu::TrackKind::Video,
            ),
            MediaKind::Audio => (
                &mut self.available_audio_tracks,
                proto_sfu::TrackKind::Audio,
            ),
        };

        // If we haven't seen this track before, stage it for client sync.
        if available_map
            .insert(track_id.internal.clone(), track_out)
            .is_none()
        {
            self.queue_published_sync(track_id, kind);
        }
    }

    pub fn remove_available_tracks(&mut self, tracks: &HashMap<Arc<TrackId>, track::TrackHandle>) {
        for track_id in tracks.keys() {
            let track_out = if let Some(t) = self.available_video_tracks.remove(&track_id.internal)
            {
                t
            } else if let Some(t) = self.available_audio_tracks.remove(&track_id.internal) {
                t
            } else {
                continue;
            };

            // If the track was occupying a subscription slot, free it up.
            if let Some(mid) = track_out.mid {
                let subscribed_map = match track_out.track.meta.kind {
                    MediaKind::Video => &mut self.subscribed_video_tracks,
                    MediaKind::Audio => &mut self.subscribed_audio_tracks,
                };
                if let Some(slot) = subscribed_map.get_mut(&mid) {
                    tracing::info!(%track_id, ?mid, "Freed subscription slot");
                    slot.track_id = None;
                }
            }

            self.queue_unpublished_sync(track_id.clone());
        }
    }

    fn queue_published_sync(&mut self, track_id: Arc<TrackId>, kind: proto_sfu::TrackKind) {
        self.pending_published_tracks.push(proto_sfu::TrackInfo {
            track_id: track_id.to_string(),
            kind: kind as i32,
            participant_id: track_id.origin_participant.to_string(),
        });
        self.should_resync = true;
    }

    fn queue_unpublished_sync(&mut self, track_id: Arc<TrackId>) {
        self.pending_unpublished_tracks.push(track_id.to_string());
        self.should_resync = true;
    }

    // --- HOT PATH HELPERS ---

    #[inline]
    pub fn get_published_track(&self, mid: Mid) -> Option<&track::TrackHandle> {
        self.published_video_tracks
            .get(&mid)
            .or_else(|| self.published_audio_tracks.get(&mid))
    }

    #[inline]
    pub fn get_egress_mid(&self, track_id: &Arc<TrackId>) -> Option<Mid> {
        self.available_video_tracks
            .get(&track_id.internal)
            .and_then(|t| t.mid)
            .or_else(|| {
                self.available_audio_tracks
                    .get(&track_id.internal)
                    .and_then(|t| t.mid)
            })
    }

    #[inline]
    pub fn find_track_out_mut(&mut self, track_id: &Arc<TrackId>) -> Option<&mut TrackOut> {
        self.available_video_tracks
            .get_mut(&track_id.internal)
            .or_else(|| self.available_audio_tracks.get_mut(&track_id.internal))
    }
}
