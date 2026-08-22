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
        let published: Vec<_> = self
            .catalog
            .published_by_participant(*participant_id)
            .filter_map(|id| {
                let publication = self.catalog.get(&id)?;
                let crate::control::publication::Media::Data { lane, topic } = &publication.media
                else {
                    return None;
                };
                Some((
                    crate::control::state::DataStreamId::new(
                        publication.room,
                        publication.publisher,
                        topic.clone(),
                    ),
                    *lane,
                ))
            })
            .collect();
        for (id, lane) in published {
            if !self.retire_stream_binding(id, lane.into()).await {
                debug_assert!(
                    false,
                    "a published data stream must retire with its publisher"
                );
            }
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
        let departures =
            crate::control::patterns::retract_participant(&mut self.audiences, participant_id);
        let video: Vec<_> = departures
            .iter()
            .filter_map(|(pattern, departure)| match pattern.name {
                Some(crate::control::publication::AudienceName::Track(track_id))
                    if self
                        .catalog
                        .get(&track_id)
                        .is_some_and(|p| p.kind() == crate::entity::TrackKind::Video) =>
                {
                    Some((
                        crate::control::patterns::Pattern {
                            room: pattern.room,
                            publisher: pattern.publisher,
                            name: Some(crate::control::publication::AudienceName::Track(track_id)),
                        },
                        *departure,
                    ))
                }
                _ => None,
            })
            .collect();
        let audio_tracks: indexmap::IndexSet<_> = departures
            .iter()
            .filter_map(|(pattern, _)| match pattern.name {
                Some(crate::control::publication::AudienceName::Track(track_id))
                    if self
                        .catalog
                        .get(&track_id)
                        .is_some_and(|p| p.kind() == crate::entity::TrackKind::Audio) =>
                {
                    Some(track_id)
                }
                _ => None,
            })
            .collect();
        let mut data_streams = indexmap::IndexSet::new();
        for (pattern, _) in &departures {
            let Some(crate::control::publication::AudienceName::Data { topic, lane }) =
                pattern.name.clone()
            else {
                continue;
            };
            let ids = match pattern.publisher {
                Some(publisher) => vec![crate::control::state::DataStreamId::new(
                    pattern.room,
                    publisher,
                    topic,
                )],
                None => self.streams_on_topic(pattern.room, &topic, lane.into()),
            };
            data_streams.extend(ids.into_iter().map(|id| (id, lane)));
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
                let Some(crate::control::publication::AudienceName::Track(track_id)) = pattern.name
                else {
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
        for (id, lane) in data_streams {
            self.reconcile_stream(id, lane.into()).await;
        }
        self.pending.remove_participant(*participant_id);
    }

    /// Retire whatever transport route a participant holds, if the registry
    /// still knows about one.
    pub(super) async fn retire_participant_transport(&mut self, participant_id: &ParticipantId) {
        let Some((shard_id, handle)) = self.core.registry.transport_of(participant_id) else {
            return;
        };
        let Some(meta) = self.core.registry.get_participant(participant_id) else {
            debug_assert!(false, "a transport must have a participant registry entry");
            return;
        };
        let room_id = meta.room_id;
        let key = self
            .core
            .registry
            .get_participant(participant_id)
            .and_then(|meta| meta.binding);
        self.retire_transport(shard_id, handle, key, Some((room_id, *participant_id)))
            .await;
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
        departing: Option<(RoomId, ParticipantId)>,
    ) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return;
        }
        let generation = self.state.pending().map(|tx| tx.generation);
        let Some(generation) = generation else {
            self.abort_transaction(now);
            return;
        };
        {
            let Some(view) = self.view_mut(shard_id) else {
                debug_assert!(false, "a transport must target a live shard view");
                self.abort_transaction(now);
                return;
            };
            view.stage(generation, crate::view::ViewOp::RetireTransport { handle });
            if let Some(key) = key {
                view.stage(generation, crate::view::ViewOp::RemoveParticipant { key });
            }
        }
        if let Some((room_id, participant_id)) = departing {
            let remaining: Vec<_> = self
                .core
                .registry
                .participants_in_room(&room_id)
                .into_iter()
                .filter(|(id, _, _)| *id != participant_id)
                .filter_map(|(_, shard, key)| key.map(|key| (shard, key)))
                .collect();
            for (shard, key) in remaining {
                let Some(view) = self.view_mut(shard) else {
                    debug_assert!(false, "a room participant must have a live shard view");
                    self.abort_transaction(now);
                    return;
                };
                view.stage_participant_effect(
                    generation,
                    key,
                    crate::participant::ParticipantEffect::ParticipantsChanged {
                        added: Vec::new(),
                        removed: vec![participant_id],
                    },
                );
            }
        }
        if !self.publish_staged_views() {
            self.abort_transaction(now);
            return;
        }
        let _ = self.state.commit();
        self.state
            .release_transport(shard_id, handle.route.slot(), now);
    }
}
