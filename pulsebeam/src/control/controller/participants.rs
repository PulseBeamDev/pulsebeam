use super::*;

impl ControllerActor {
    /// Release everything a departing participant was consuming.
    ///
    /// Its subscriptions go with it, and any route that only it kept alive is
    /// retired — otherwise a shard keeps a route for a stream nobody there
    /// receives any more, and the slot never returns to the allocator.
    /// Release everything a departing participant was consuming on the data
    /// lanes.
    ///
    /// Its declarations name the streams, so they have to be resolved before
    /// the declarations go. Any route it alone kept alive is retired by the
    /// reconcile that follows — otherwise a shard keeps a route for a stream
    /// nobody there receives, and the slot never returns to the allocator.
    pub(super) async fn retire_participant_streams(&mut self, participant_id: &ParticipantId) {
        let (departures, ops) = crate::control::patterns::retract_participant(
            &mut self.data_patterns,
            participant_id,
            crate::view::AudienceKind::Data,
        );
        if !self.publish_ops(ops) {
            debug_assert!(false, "data group retraction must publish");
        }
        let mut affected: Vec<(crate::shard::router::DataStreamId, StreamLane)> = Vec::new();
        for (pattern, _) in departures {
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
        for (id, lane) in affected {
            self.reconcile_stream(id, lane).await;
        }
    }

    pub(super) async fn retire_participant_tracks(&mut self, participant_id: &ParticipantId) {
        let tracks: Vec<_> = self
            .catalog
            .iter()
            .filter(|(_, binding)| binding.publisher == *participant_id)
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
        let (video, mut ops) = crate::control::patterns::retract_participant(
            &mut self.video_patterns,
            participant_id,
            crate::view::AudienceKind::Video,
        );
        let (_, audio_ops) = crate::control::patterns::retract_participant(
            &mut self.audio_patterns,
            participant_id,
            crate::view::AudienceKind::Audio,
        );
        ops.extend(audio_ops);
        if !self.publish_ops(ops) {
            debug_assert!(false, "group retraction must publish");
        }

        // A video route is per (track, shard) and only retires with the last
        // consumer on that shard, which is what LastOnShard reports.
        let shard = self
            .core
            .registry
            .transport_of(participant_id)
            .map(|(shard, _)| shard);
        if let Some(shard) = shard {
            for (pattern, departure) in video {
                if departure != crate::control::patterns::Departure::LastOnShard {
                    continue;
                }
                let Some(track_id) = pattern.name else {
                    continue;
                };
                let Some(route) = self.catalog.get_mut(&track_id).and_then(|binding| {
                    binding
                        .destinations
                        .get_mut(&shard)
                        .and_then(|d| d.route.take())
                }) else {
                    continue;
                };
                self.release_route(shard, route).await;
            }
        }
        self.pending.remove_participant(*participant_id);
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
