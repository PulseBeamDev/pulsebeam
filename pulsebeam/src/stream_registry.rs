//! Node-local directory of published encodings' measurement handles.
//!
//! A [`StreamState`] is an `Arc` of atomics shared by whoever measures a stream
//! and whoever allocates against it. That makes it unroutable in both
//! directions: the control plane must hold no media-path state, and the data
//! plane carries only packets it can address by route and put on a wire. So the
//! handle travels in neither — a destination shard resolves it here, using the
//! `TrackId` the control plane already gave it when it installed the route.
//!
//! Cross-node this stays correct without becoming a distributed lookup: a
//! remote stream's handle is created locally by the ingress that admits it, so
//! every shard still finds its measurements on its own node.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::entity::TrackId;
use crate::track::TrackStates;

#[derive(Debug, Default)]
pub struct StreamRegistry {
    tracks: RwLock<HashMap<TrackId, TrackStates>>,
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a track's encodings. Idempotent: republishing the same track
    /// replaces its handles, which is what a re-negotiated encoding set needs.
    pub fn publish(&self, track_id: TrackId, states: TrackStates) {
        if states.is_empty() {
            return;
        }
        self.tracks
            .write()
            .expect("stream registry poisoned")
            .insert(track_id, states);
    }

    pub fn unpublish(&self, track_id: &TrackId) {
        self.tracks
            .write()
            .expect("stream registry poisoned")
            .remove(track_id);
    }

    /// Handles for a track's encodings, empty when it is not published here.
    pub fn states_for(&self, track_id: &TrackId) -> TrackStates {
        self.tracks
            .read()
            .expect("stream registry poisoned")
            .get(track_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::entity::{ParticipantId, TrackKind};
    use crate::rtp::monitor::StreamState;
    use str0m::media::{Mid, Rid};

    fn track_id(seed: u64) -> TrackId {
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(seed))
            .derive_track_id(TrackKind::Video, &Mid::from("v"))
    }

    fn states() -> TrackStates {
        vec![
            (Some(Rid::from("q")), StreamState::new(false, 150_000)),
            (Some(Rid::from("f")), StreamState::new(false, 900_000)),
        ]
    }

    /// The point of the registry: a shard that never received the handles still
    /// resolves the very same atomics the publisher is writing to.
    #[test]
    fn a_reader_resolves_the_publishers_own_handles() {
        let registry = StreamRegistry::new();
        let id = track_id(1);
        let published = states();
        registry.publish(id, published.clone());

        let resolved = registry.states_for(&id);
        assert_eq!(resolved.len(), 2);
        for ((rid, state), (published_rid, published_state)) in resolved.iter().zip(&published) {
            assert_eq!(rid, published_rid);
            published_state.update_for_test().bitrate(4_242);
            assert_eq!(
                state.bitrate_bps(),
                4_242.0,
                "the resolved handle must alias the publisher's, not copy it"
            );
        }
    }

    /// A `TrackId` is derived from the participant and the label, so
    /// republishing the same label reuses it. The registry must hand out the
    /// new incarnation's handles — keeping the old ones would allocate against
    /// a monitor nothing writes to any more.
    #[test]
    fn republishing_a_track_replaces_its_handles() {
        let registry = StreamRegistry::new();
        let id = track_id(3);

        let first = states();
        registry.publish(id, first.clone());
        registry.unpublish(&id);

        let second = states();
        registry.publish(id, second.clone());

        let resolved = registry.states_for(&id);
        second[0].1.update_for_test().bitrate(999);
        first[0].1.update_for_test().bitrate(111);
        assert_eq!(
            resolved[0].1.bitrate_bps(),
            999.0,
            "a resolved handle must alias the live incarnation, not the retired one"
        );
    }

    #[test]
    fn an_unpublished_track_resolves_to_nothing() {
        let registry = StreamRegistry::new();
        let id = track_id(2);
        assert!(registry.states_for(&id).is_empty());

        registry.publish(id, states());
        assert!(!registry.states_for(&id).is_empty());

        registry.unpublish(&id);
        assert!(
            registry.states_for(&id).is_empty(),
            "an unpublished track must not leave its handles behind"
        );
    }
}
