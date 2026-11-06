use super::*;
use crate::participant::policy::VideoPolicy;

impl DownstreamVideo {
    pub(crate) fn owned_slot_bytes() -> usize {
        std::mem::size_of::<Slot>()
    }
}

impl VideoAllocator {
    pub(crate) fn install_owned_slot(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: Ssrc,
        pt: Pt,
    ) -> bool {
        if self.has_slot(mid) {
            return false;
        }
        let key = self
            .slots
            .insert(Slot::output(self.ctx, None, mid, rid, ssrc, pt));
        self.owned_slots.insert(mid, key);
        true
    }

    pub(crate) fn configure_owned_slot(
        &mut self,
        mid: Mid,
        source: TrackId,
        options: &VideoPolicy,
        active: bool,
    ) {
        let Some(slot) = self
            .owned_slots
            .get(&mid)
            .and_then(|key| self.slots.get_mut(*key))
        else {
            return;
        };
        let previous_ceiling = slot.height_ceiling;
        slot.height_ceiling = options.height_ceiling;
        let intent = Intent {
            track_id: source,
            target_height: options.height_ceiling.unwrap_or(u32::MAX),
            min_height: options.minimum_height,
            min_fps: options.minimum_fps,
            priority: options.priority,
        };
        Self::configure_slot(
            &self.tracks,
            &self.track_handles,
            slot,
            active.then_some(&intent),
        );
        // Allocation must enforce a new ceiling before another packet can flow.
        if active
            && previous_ceiling != slot.height_ceiling
            && let Some(layer) = slot.target().cloned()
        {
            slot.pause_at(&layer);
        }
    }

    pub(crate) fn remove_owned_slot(&mut self, mid: Mid) {
        if let Some(key) = self.owned_slots.remove(&mid) {
            self.slots.remove(key);
        }
    }

    pub(crate) fn forget_owned_track(&mut self, key: TrackHandle, track_id: TrackId) {
        if self.track_handles.get(&track_id) == Some(&key) {
            self.track_handles.remove(&track_id);
            self.active_track_handles.remove(&track_id);
            self.tracks.remove(key);
        }
    }

    pub(crate) fn on_owned_rtp(
        &mut self,
        mid: Mid,
        track: TrackId,
        arrival: Instant,
        cache: Option<&TrackStreamCache>,
        writer: &mut StreamWriter,
    ) -> bool {
        self.owned_slots
            .get(&mid)
            .and_then(|key| self.slots.get_mut(*key))
            .is_some_and(|slot| {
                slot.assignment == Some(track) && slot.on_rtp(track, arrival, cache, writer)
            })
    }
}
