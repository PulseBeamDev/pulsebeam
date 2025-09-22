use std::{collections::HashMap, sync::Arc};

use str0m::media::Mid;

use crate::{entity, participant::effect, track};

#[derive(Clone)]
struct TrackOut {
    handle: track::TrackHandle,
    mid: Option<Mid>,
}

struct Slot {
    track_id: Option<Arc<entity::TrackId>>,
}

#[derive(Default)]
pub struct VideoAllocator {
    tracks: HashMap<Arc<entity::TrackId>, TrackOut>,
    slots: HashMap<Mid, Slot>,

    pub effects: Vec<effect::Effect>,
}

impl VideoAllocator {
    pub fn add_track(&mut self, track_handle: track::TrackHandle) {
        assert!(track_handle.meta.kind.is_video());

        self.tracks.insert(
            track_handle.meta.id.clone(),
            TrackOut {
                handle: track_handle,
                mid: None,
            },
        );
        self.auto_subscribe();
    }

    pub fn remove_track(&mut self, track_id: &entity::TrackId) -> Option<track::TrackHandle> {
        let res = self.tracks.remove(track_id).map(|t| t.handle);
        self.auto_subscribe();
        res
    }

    pub fn add_slot(&mut self, mid: Mid) {
        self.slots.insert(mid, Slot { track_id: None });
        self.auto_subscribe();
    }

    pub fn remove_slot(&mut self, mid: &Mid) {
        self.slots.remove(mid);
        self.auto_subscribe();
    }

    pub fn subscribe(&mut self, _track_id: &Arc<entity::TrackId>, _mid: &Mid) {
        todo!();
    }

    /// Automatically subscribe available tracks to open slots
    pub fn auto_subscribe(&mut self) {
        let mut available_tracks = self.tracks.iter_mut();
        for (slot_id, slot) in self.slots.iter_mut() {
            if slot.track_id.is_some() {
                continue;
            }

            for (track_id, track) in &mut available_tracks {
                if track.mid.is_some() {
                    continue;
                }

                self.effects
                    .push(effect::Effect::Subscribe(track.handle.clone()));
                track.mid.replace(*slot_id);
                let meta = &track.handle.meta;
                slot.track_id.replace(meta.id.clone());
                tracing::info!("allocated video slot: {track_id} -> {slot_id}");
                break;
            }
        }
    }
}
