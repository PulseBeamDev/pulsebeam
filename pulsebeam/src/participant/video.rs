use std::{collections::HashMap, sync::Arc};

use str0m::media::Mid;

use crate::{
    entity::{self, TrackId},
    participant::effect,
    track,
};

#[derive(Clone)]
struct TrackOut {
    mid: Option<Mid>,
}

struct Slot {
    track_id: Option<Arc<entity::TrackId>>,
}

#[derive(Default)]
pub struct VideoAllocator {
    tracks: HashMap<Arc<entity::TrackId>, TrackOut>,
    slots: HashMap<Mid, Slot>,
}

impl VideoAllocator {
    pub fn add_track(&mut self, effects: &mut effect::Queue, track_id: Arc<TrackId>) {
        tracing::info!("added video track: {}", track_id);
        self.tracks.insert(track_id, TrackOut { mid: None });

        self.auto_subscribe(effects);
    }

    pub fn remove_track(&mut self, effects: &mut effect::Queue, track_id: &entity::TrackId) {
        let Some(mut track) = self.tracks.remove(track_id) else {
            return;
        };

        if let Some(mid) = track.mid.take()
            && let Some(slot) = self.slots.get_mut(&mid)
        {
            slot.track_id = None;
        }

        tracing::info!("removed video track: {:?}", track_id);
        self.auto_subscribe(effects);
    }

    pub fn add_slot(&mut self, effects: &mut effect::Queue, mid: Mid) {
        self.slots.insert(mid, Slot { track_id: None });
        tracing::info!("added video slot: {}", mid);
        self.auto_subscribe(effects);
    }

    pub fn get_slot(&mut self, track_id: &Arc<entity::TrackId>) -> Option<Mid> {
        let track = self.tracks.get(track_id)?;
        let mid = track.mid.as_ref()?;

        Some(*mid)
    }

    pub fn subscribe(&mut self, _track_id: &Arc<entity::TrackId>, _mid: &Mid) {
        todo!();
    }

    /// Automatically subscribe available tracks to open slots
    pub fn auto_subscribe(&mut self, effects: &mut effect::Queue) {
        let mut available_tracks = self.tracks.iter_mut();
        for (slot_id, slot) in self.slots.iter_mut() {
            if slot.track_id.is_some() {
                continue;
            }

            for (track_id, track) in &mut available_tracks {
                if track.mid.is_some() {
                    continue;
                }

                effects.push_back(effect::Effect::Subscribe(track_id.clone()));
                track.mid.replace(*slot_id);
                slot.track_id.replace(track_id.clone());
                tracing::info!("allocated video slot: {track_id} -> {slot_id}");
                break;
            }
        }
    }
}
