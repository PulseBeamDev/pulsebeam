use super::*;

impl ControllerActor {
    /// Release everything a departing participant was consuming.
    ///
    /// Its subscriptions go with it, and any route that only it kept alive is
    /// retired — otherwise a shard keeps a route for a stream nobody there
    /// receives any more, and the slot never returns to the allocator.
    pub(super) async fn retire_participant_streams(&mut self, participant_id: &ParticipantId) {
        for lane in StreamLane::ALL {
            let changed = self.lanes.get_mut(lane).retire_participant(participant_id);
            for id in changed {
                self.reconcile_stream(id, lane).await;
            }
        }
    }

    pub(super) async fn retire_participant_tracks(&mut self, participant_id: &ParticipantId) {
        let tracks: Vec<_> = self
            .track_bindings
            .iter()
            .filter(|(_, binding)| binding.meta.origin == *participant_id)
            .map(|(track_id, _)| *track_id)
            .collect();
        for track_id in tracks {
            if !self.retire_track_binding(track_id).await {
                debug_assert!(false, "publisher track retirement must complete");
                continue;
            }
            let _ = self.core.registry.remove_track(*participant_id, track_id);
        }
    }

    pub(super) async fn retire_participant_subscriptions(
        &mut self,
        participant_id: &ParticipantId,
    ) {
        for retired in self.subscriptions.remove_participant(participant_id) {
            self.release_route(retired.destination, retired.route).await;
        }
        self.pending.remove_participant(*participant_id);
        self.audio_patterns.remove_participant(participant_id);
    }

    /// Retire whatever transport route a participant holds, if the registry
    /// still knows about one.
    pub(super) async fn retire_participant_transport(&mut self, participant_id: &ParticipantId) {
        let Some((shard_id, handle)) = self.core.registry.transport_of(participant_id) else {
            return;
        };
        let key = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding);
        self.retire_transport(shard_id, handle, key).await;
        if let Some(key) = key {
            self.state.remove_participant(shard_id, key);
        }
    }

    /// Retire a transport route as its own generation, so the route is gone
    /// from the published view before the allocator may hand its slot out.
    pub(super) async fn retire_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: TransportHandle,
        key: Option<crate::shard::participants::ParticipantKey>,
    ) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return;
        }
        let generation = self.state.pending().map(|tx| tx.generation);
        let published = generation.and_then(|generation| {
            let view = self.view_mut(shard_id)?;
            view.stage(generation, crate::view::ViewOp::RetireTransport { handle });
            if let Some(key) = key {
                view.stage(generation, crate::view::ViewOp::RemoveParticipant { key });
            }
            view.publish()
        });
        let Some(_) = published else {
            self.abort_transaction(now);
            return;
        };
        let _ = self.state.commit();
        self.state
            .release_transport(shard_id, handle.route.slot(), now);
    }
}
