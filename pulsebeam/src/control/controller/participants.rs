use super::*;

impl ControllerActor {
    /// Release everything a departing participant was consuming.
    ///
    /// Its subscriptions go with it, and any route that only it kept alive is
    /// retired — otherwise a shard keeps a route for a stream nobody there
    /// receives any more, and the slot never returns to the allocator.
    pub(super) async fn retire_participant_streams(&mut self, participant_id: &ParticipantId) {
        // What it consumed is what it declared. The streams affected have to be
        // collected before the declarations go, since they are what says which
        // ones to reconcile.
        let mut affected: Vec<(crate::shard::router::DataStreamId, StreamLane)> = Vec::new();
        for pattern in self.data_patterns.declarations_of(participant_id) {
            // A data declaration always names its topic; only the publisher
            // position may wildcard.
            let Some((topic, lane)) = pattern.name else {
                debug_assert!(false, "a data declaration names its topic");
                continue;
            };
            let ids = match pattern.publisher {
                Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                    pattern.room,
                    publisher,
                    topic,
                )],
                None => self.lanes.get(lane).ids_on_topic(&pattern.room, &topic),
            };
            for id in ids {
                if !affected.iter().any(|(held, _)| *held == id) {
                    affected.push((id, lane));
                }
            }
        }
        let placement = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding)
            .zip(
                self.core
                    .registry
                    .transport_of(participant_id)
                    .map(|(s, _)| s),
            );
        let retracted = self.data_patterns.remove_participant(participant_id);
        if let Some((key, shard)) = placement {
            let ops = retracted
                .into_iter()
                .map(|(group, _)| (shard, crate::view::ViewOp::DataGroupRemove { group, key }))
                .collect();
            if !self.publish_ops(ops) {
                debug_assert!(false, "data group retraction must publish");
            }
        }
        for (id, lane) in affected {
            self.reconcile_stream(id, lane).await;
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
        // Video: the tracks it subscribed to are named by its own declarations,
        // and a route only retires when it was that participant's alone.
        let watched = self.video_patterns.declarations_of(participant_id);
        let placement = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding)
            .zip(
                self.core
                    .registry
                    .transport_of(participant_id)
                    .map(|(s, _)| s),
            );
        let mut departures = Vec::new();
        for pattern in &watched {
            let Some(track_id) = pattern.name else {
                continue;
            };
            let departure = self.video_patterns.undeclare(pattern, participant_id);
            departures.push((track_id, departure));
        }
        if let Some((key, shard)) = placement {
            let ops: Vec<_> = watched
                .iter()
                .filter_map(|pattern| {
                    let group = self.video_patterns.group_of(pattern)?;
                    Some((shard, crate::view::ViewOp::VideoGroupRemove { group, key }))
                })
                .collect();
            if !self.publish_ops(ops) {
                debug_assert!(false, "video group retraction must publish");
            }
            for (track_id, departure) in departures {
                if departure != crate::control::patterns::Departure::LastOnShard {
                    continue;
                }
                let Some(route) = self
                    .track_bindings
                    .get_mut(&track_id)
                    .and_then(|binding| binding.video_routes.remove(&shard))
                else {
                    continue;
                };
                self.release_route(shard, route).await;
            }
        }
        self.pending.remove_participant(*participant_id);
        let key = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding);
        let shard = self
            .core
            .registry
            .transport_of(participant_id)
            .map(|(s, _)| s);
        let retracted = self.audio_patterns.remove_participant(participant_id);
        if let (Some(key), Some(shard)) = (key, shard) {
            let ops = retracted
                .into_iter()
                .map(|(group, _)| (shard, crate::view::ViewOp::AudioGroupRemove { group, key }))
                .collect();
            if !self.publish_ops(ops) {
                debug_assert!(false, "audio group retraction must publish");
            }
        }
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
