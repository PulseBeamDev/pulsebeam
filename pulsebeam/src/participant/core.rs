use std::{collections::HashMap, sync::Arc};

use str0m::media::{Direction, MediaAdded, MediaKind, Mid};

use super::effect::Effect;
use crate::{
    entity, message,
    participant::{audio::AudioAllocator, effect, video::VideoAllocator},
    track,
};

pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    published_tracks: HashMap<Mid, track::TrackHandle>,
    video_allocator: VideoAllocator,
    audio_allocator: AudioAllocator,
}

impl ParticipantCore {
    pub fn new(participant_id: Arc<entity::ParticipantId>) -> Self {
        Self {
            participant_id,
            published_tracks: HashMap::new(),
            video_allocator: VideoAllocator::default(),
            audio_allocator: AudioAllocator::with_chromium_limit(),
        }
    }

    pub fn handle_track_finished(&mut self, track_meta: Arc<message::TrackMeta>) {
        self.published_tracks.remove(&track_meta.id.origin_mid);
        tracing::info!("Track finished: {}", track_meta.id);
    }

    pub fn handle_published_tracks(
        &mut self,
        effects: &mut effect::Queue,
        tracks: &HashMap<Arc<entity::TrackId>, track::TrackHandle>,
    ) {
        for track_handle in tracks.values() {
            if track_handle.meta.id.origin_participant == self.participant_id {
                // Our own track - add to published
                self.add_published_track(effects, track_handle);
            } else {
                // Track from another participant - add to available
                self.add_available_track(effects, track_handle);
            }
        }
    }

    fn add_published_track(
        &mut self,
        _effects: &mut effect::Queue,
        track_handle: &track::TrackHandle,
    ) {
        let track_meta = &track_handle.meta;
        self.published_tracks
            .insert(track_meta.id.origin_mid, track_handle.clone());
        tracing::info!("Published track: {}", track_meta.id);
    }

    fn add_available_track(
        &mut self,
        effects: &mut effect::Queue,
        track_handle: &track::TrackHandle,
    ) {
        match track_handle.meta.kind {
            MediaKind::Video => {
                self.video_allocator
                    .add_track(effects, track_handle.clone());
            }
            MediaKind::Audio => {
                todo!();
            }
        }
    }

    pub fn remove_available_tracks(
        &mut self,
        effects: &mut effect::Queue,
        track_ids: &HashMap<Arc<entity::TrackId>, track::TrackHandle>,
    ) {
        for (track_id, track_handle) in track_ids.into_iter() {
            match track_handle.meta.kind {
                MediaKind::Video => {
                    self.video_allocator.remove_track(effects, track_id);
                }
                MediaKind::Audio => todo!(),
            }
        }
    }

    pub fn handle_media_added(&mut self, effects: &mut effect::Queue, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => {
                // Client publishing to us
                self.handle_incoming_media(effects, media);
            }
            Direction::SendOnly => {
                // We're sending to client
                self.allocate_outgoing_slot(effects, media);
            }
            dir => {
                tracing::warn!("Unsupported direction {:?}, disconnecting", dir);
                effects.push_back(Effect::Disconnect);
            }
        }
    }

    fn handle_incoming_media(&mut self, effects: &mut effect::Queue, media: MediaAdded) {
        let track_id = Arc::new(entity::TrackId::new(self.participant_id.clone(), media.mid));
        let track_meta = Arc::new(message::TrackMeta {
            id: track_id,
            kind: media.kind,
            simulcast_rids: media.simulcast.map(|s| s.recv),
        });

        tracing::info!("Published new track: {:?}", track_meta);
        effects.push_back(Effect::SpawnTrack(track_meta));
    }

    fn allocate_outgoing_slot(&mut self, effects: &mut effect::Queue, media: MediaAdded) {
        match media.kind {
            MediaKind::Video => self.video_allocator.add_slot(effects, media.mid),
            MediaKind::Audio => {
                todo!()
            }
        }
    }
}
