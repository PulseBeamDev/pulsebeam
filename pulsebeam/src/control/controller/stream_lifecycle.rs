use super::*;

/// The view ops a stream key implies.
///
/// None of these take a lane: `RuntimeStreamKey`'s variant *is* the lane, and
/// passing one alongside meant every call site carried a second source of truth
/// plus a `debug_assert` to catch the two disagreeing.
pub(super) fn insert_stream_runtime_op(
    key: crate::shard::router::RuntimeStreamKey,
    id: crate::shard::router::DataStreamId,
    publisher: Option<crate::shard::participants::ParticipantKey>,
) -> crate::view::ViewOp {
    match key {
        crate::shard::router::RuntimeStreamKey::Unreliable(key) => {
            crate::view::ViewOp::InsertUnreliableRuntime { key, id, publisher }
        }
        crate::shard::router::RuntimeStreamKey::Reliable(key) => {
            crate::view::ViewOp::InsertReliableRuntime { key, id, publisher }
        }
    }
}

pub(super) fn install_stream_route_op(
    key: crate::shard::router::RuntimeStreamKey,
    route: RouteHandle,
) -> crate::view::ViewOp {
    let action = match key {
        crate::shard::router::RuntimeStreamKey::Unreliable(stream) => {
            RouteAction::Unreliable { stream }
        }
        crate::shard::router::RuntimeStreamKey::Reliable(stream) => {
            RouteAction::Reliable { stream }
        }
    };
    crate::view::ViewOp::InstallRoute {
        binding: crate::view::RouteBinding {
            handle: route,
            action,
        },
    }
}

/// The label a data stream is published under.
fn data_label(topic: &crate::track::Topic, lane: StreamLane) -> String {
    crate::track::publication_label(lane.into(), topic)
}

/// The catalog identity of a data stream.
///
/// `StreamLane` is the control plane's name for the lane and `DataLane` the
/// label grammar's; they are the same two lanes, and this is the one place the
/// two spellings meet.
pub(super) fn data_publication_id(
    id: &crate::shard::router::DataStreamId,
    lane: StreamLane,
) -> crate::entity::TrackId {
    let label = data_label(&id.topic, lane);
    id.publisher_id
        .derive_track_id(crate::entity::TrackKind::Data, &label)
}

impl ControllerActor {
    /// Every stream on a topic and lane in a room. The catalog indexes data by
    /// label precisely for this: a wildcard subscription names a topic across
    /// publishers, which an id-keyed lookup cannot answer.
    pub(super) fn streams_on_topic(
        &self,
        room: crate::entity::RoomId,
        topic: &crate::track::Topic,
        lane: StreamLane,
    ) -> Vec<crate::shard::router::DataStreamId> {
        self.catalog
            .on_label(room, &data_label(topic, lane))
            .filter_map(|id| {
                let held = self.catalog.get(&id)?;
                Some(crate::shard::router::DataStreamId::new(
                    held.room,
                    held.publisher,
                    topic.clone(),
                ))
            })
            .collect()
    }

    pub(super) async fn on_stream_ready(
        &mut self,
        shard_id: crate::id::ShardId,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        let publication_id = data_publication_id(&id, lane);
        if self.catalog.contains(&publication_id) {
            let Some(publication) = self.catalog.get(&publication_id) else {
                debug_assert!(false, "a catalog entry must remain addressable");
                return false;
            };
            if publication.publisher_shard != shard_id {
                pulsebeam_runtime::fatal!("a data publication cannot move between shards");
            }
            return self.reconcile_stream(id, lane).await;
        }
        let Some(key) = self.lanes.get(lane).mint(&mut self.state, shard_id, &id) else {
            return false;
        };
        let runtime_key: crate::control::publication::RuntimeKey = key.into();
        let Some(publisher) = self
            .core
            .registry
            .get_participant(&id.publisher_id)
            .and_then(|meta| meta.binding)
        else {
            debug_assert!(false, "a stream publisher must have a participant key");
            crate::control::controller::lifecycle::remove_runtime_key(
                &mut self.state,
                shard_id,
                runtime_key,
            );
            return false;
        };
        self.catalog
            .insert(crate::control::publication::Publication {
                id: publication_id,
                room: id.room_id,
                publisher: id.publisher_id,
                publisher_shard: shard_id,
                publisher_key: publisher,
                origin_key: runtime_key,
                reverse_route: None,
                destinations: indexmap::IndexMap::new(),
                media: crate::control::publication::Media::Data {
                    lane: lane.into(),
                    topic: id.topic.clone(),
                },
            });
        self.index_publication(publication_id);

        if matches!(lane, StreamLane::Reliable) {
            let Some(stream) = (match key {
                crate::shard::router::RuntimeStreamKey::Reliable(stream) => Some(stream),
                _ => None,
            }) else {
                debug_assert!(false, "reliable readiness must carry a reliable key");
                self.catalog.remove(&publication_id);
                crate::control::controller::lifecycle::remove_runtime_key(
                    &mut self.state,
                    shard_id,
                    runtime_key,
                );
                return false;
            };
            let Some(route) = self
                .grant_route(
                    shard_id,
                    RouteAction::Reverse {
                        target: ReverseTarget::Topic { stream },
                    },
                )
                .await
            else {
                self.catalog.remove(&publication_id);
                crate::control::controller::lifecycle::remove_runtime_key(
                    &mut self.state,
                    shard_id,
                    runtime_key,
                );
                return false;
            };
            let Some(binding) = self.catalog.get_mut(&data_publication_id(&id, lane)) else {
                debug_assert!(false, "stream binding must survive reverse route install");
                self.release_route(shard_id, route).await;
                crate::control::controller::lifecycle::remove_runtime_key(
                    &mut self.state,
                    shard_id,
                    runtime_key,
                );
                return false;
            };
            binding.reverse_route = Some(route);
        }

        self.reconcile_stream(id, lane).await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "data and reliable subscriptions share one lifecycle path, and these fields are the complete event identity"
    )]
    pub(super) async fn on_stream_subscription(
        &mut self,
        shard_id: crate::id::ShardId,
        room_id: RoomId,
        subscriber: ParticipantId,
        subscriber_key: crate::shard::participants::ParticipantKey,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
        channel: str0m::channel::ChannelId,
        lane: StreamLane,
    ) {
        let pattern = match publisher {
            Some(publisher) => {
                crate::control::patterns::Pattern::exact(room_id, publisher, (topic.clone(), lane))
            }
            None => {
                crate::control::patterns::Pattern::any_publisher(room_id, (topic.clone(), lane))
            }
        };
        let displaced_ids: indexmap::IndexSet<_> = self
            .data_patterns
            .declarations_of(&subscriber)
            .into_iter()
            .filter(|held| pattern.subsumes(held))
            .flat_map(|held| self.data_patterns.publications_of_pattern(&held))
            .collect();
        let _ = crate::control::patterns::declare_audience(
            &mut self.data_patterns,
            pattern,
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: channel,
            },
        );
        // Which streams this reaches is a catalog question now, not something
        // the registry has to remember on the subscriber's behalf.
        let mut ids: indexmap::IndexSet<_> = match publisher {
            Some(publisher) => [crate::shard::router::DataStreamId::new(
                room_id,
                publisher,
                topic.clone(),
            )]
            .into_iter()
            .collect(),
            None => self
                .streams_on_topic(room_id, &topic, lane)
                .into_iter()
                .collect(),
        };
        for publication_id in displaced_ids {
            if let Some(publication) = self.catalog.get(&publication_id) {
                ids.insert(crate::shard::router::DataStreamId::new(
                    publication.room,
                    publication.publisher,
                    topic.clone(),
                ));
            }
        }
        for id in &ids {
            let publication_id = data_publication_id(id, lane);
            if self.catalog.contains(&publication_id) {
                self.index_publication(publication_id);
            }
        }
        for id in ids {
            let _ = self.reconcile_stream(id, lane).await;
        }
    }

    pub(super) async fn on_stream_unsubscription(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: crate::track::Topic,
        publisher: Option<ParticipantId>,
        lane: StreamLane,
    ) {
        let pattern = match publisher {
            Some(publisher) => {
                crate::control::patterns::Pattern::exact(room_id, publisher, (topic.clone(), lane))
            }
            None => {
                crate::control::patterns::Pattern::any_publisher(room_id, (topic.clone(), lane))
            }
        };
        let _ = crate::control::patterns::retract_audience(
            &mut self.data_patterns,
            &pattern,
            &subscriber,
        );
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => self.streams_on_topic(room_id, &topic, lane),
        };
        for id in ids {
            let _ = self.reconcile_stream(id, lane).await;
        }
    }

    pub(super) async fn retire_stream_binding(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        self.retire_publication(data_publication_id(&id, lane))
            .await
    }

    /// Make a stream's destinations match its declarations.
    pub(super) async fn reconcile_stream(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        let publication_id = data_publication_id(&id, lane);
        self.retire_stale_destinations(publication_id).await;
        let stream = id.clone();
        let complete = self
            .install_destinations(publication_id, move |actor, destination| {
                let key = actor
                    .lanes
                    .get(lane)
                    .mint(&mut actor.state, destination, &stream)?;
                let Some(action) = actor.lanes.get(lane).route_action(key) else {
                    crate::control::controller::lifecycle::remove_runtime_key(
                        &mut actor.state,
                        destination,
                        key.into(),
                    );
                    debug_assert!(false, "a lane must only mint its own route action");
                    return None;
                };
                Some((key.into(), action))
            })
            .await;
        let published = self.publish_publication(publication_id).await;
        if !complete
            && published
            && let Some(shard_id) = self.catalog.get(&publication_id).map(|p| p.publisher_shard)
        {
            self.defer_stream(shard_id, id, lane);
        }
        complete && published
    }
}
