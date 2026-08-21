use super::*;

impl ControllerActor {
    /// Release everything a departing participant was consuming on the data
    /// lanes.
    ///
    /// Its declarations name the streams, so they have to be resolved before
    /// the declarations go. Any route it alone kept alive is retired by the
    /// reconcile that follows — otherwise a shard keeps a route for a stream
    /// nobody there receives, and the slot never returns to the allocator.
    pub(super) async fn retire_participant_streams(&mut self, participant_id: &ParticipantId) {
        self.pending_streams.remove_participant(*participant_id);
        let departures =
            crate::control::patterns::retract_participant(&mut self.data_patterns, participant_id);
        let mut affected = indexmap::IndexSet::new();
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
                None => self.streams_on_topic(pattern.room, &topic, lane),
            };
            for id in ids {
                affected.insert((id, lane));
            }
        }
        for (id, lane) in affected {
            self.reconcile_stream(id, lane).await;
        }
    }

    pub(super) async fn retire_participant_tracks(&mut self, participant_id: &ParticipantId) {
        // Media only: a data stream retires through the lane path that owns
        // its arena. The room comes from the publication itself, since a
        // participant that has already left the registry still has tracks to
        // retire.
        let tracks: Vec<_> = self
            .catalog
            .published_by_participant(*participant_id)
            .filter(|id| {
                matches!(
                    id.kind(),
                    crate::entity::TrackKind::Video | crate::entity::TrackKind::Audio
                )
            })
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
        let video =
            crate::control::patterns::retract_participant(&mut self.video_patterns, participant_id);
        let audio_tracks: indexmap::IndexSet<_> = self
            .audio_patterns
            .declarations_of(participant_id)
            .into_iter()
            .flat_map(|pattern| self.audio_patterns.publications_of_pattern(&pattern))
            .collect();
        let _ =
            crate::control::patterns::retract_participant(&mut self.audio_patterns, participant_id);

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
                let Some(route) = self.catalog.get(&track_id).and_then(|binding| {
                    binding
                        .destinations
                        .get(&shard)
                        .and_then(|destination| match destination {
                            crate::control::publication::Destination::Forwarding {
                                route, ..
                            } => Some(*route),
                            crate::control::publication::Destination::Discovery { .. } => None,
                        })
                }) else {
                    continue;
                };
                if !self.retire_video_route(shard, route, track_id).await {
                    debug_assert!(false, "participant video route retirement must complete");
                }
            }
        }
        for track_id in audio_tracks {
            self.retire_stale_destinations(track_id).await;
            let _ = self.publish_publication(track_id).await;
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
