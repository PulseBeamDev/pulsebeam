use super::*;

pub(super) fn insert_stream_runtime_op(
    key: crate::keys::TrackKey,
    id: crate::control::state::DataStreamId,
    lane: StreamLane,
    publisher: Option<crate::shard::participants::ParticipantKey>,
) -> crate::view::ViewOp {
    crate::view::ViewOp::InsertTrackRuntime {
        key,
        runtime: crate::view::TrackRuntime {
            publisher,
            publisher_effect: publisher.map(|_| {
                crate::participant::ParticipantEffect::TrackPublished {
                    topic: id.topic,
                    key,
                    lane: lane.into(),
                }
            }),
            ..Default::default()
        },
    }
}

pub(super) fn install_stream_route_op(
    key: crate::keys::TrackKey,
    route: RouteHandle,
) -> crate::view::ViewOp {
    crate::view::ViewOp::InstallRoute {
        binding: crate::view::RouteBinding {
            handle: route,
            action: RouteAction::Forward { target: key },
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
    id: &crate::control::state::DataStreamId,
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
    ) -> Vec<crate::control::state::DataStreamId> {
        self.catalog
            .on_label(room, &data_label(topic, lane))
            .filter_map(|id| {
                let held = self.catalog.get(&id)?;
                Some(crate::control::state::DataStreamId::new(
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
        id: crate::control::state::DataStreamId,
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
        let runtime_key = key;
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
            let stream = key;
            let Some(route) = self
                .grant_route(shard_id, RouteAction::Reverse { target: stream })
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
            Some(publisher) => crate::control::patterns::Pattern::exact(
                room_id,
                publisher,
                crate::control::publication::AudienceName::Data {
                    topic: topic.clone(),
                    lane: lane.into(),
                },
            ),
            None => crate::control::patterns::Pattern::any_publisher(
                room_id,
                crate::control::publication::AudienceName::Data {
                    topic: topic.clone(),
                    lane: lane.into(),
                },
            ),
        };
        let displaced_ids: indexmap::IndexSet<_> = self
            .audiences
            .declarations_of(&subscriber)
            .into_iter()
            .filter(|held| pattern.subsumes(held))
            .flat_map(|held| self.audiences.publications_of_pattern(&held))
            .collect();
        let _ = crate::control::patterns::declare_audience_with_kind(
            &mut self.audiences,
            pattern,
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: crate::control::publication::AudienceDelivery::Data {
                    channel,
                    lane: lane.into(),
                },
            },
            crate::control::publication::AudienceDelivery::same_kind,
        );
        // Which streams this reaches is a catalog question now, not something
        // the registry has to remember on the subscriber's behalf.
        let mut ids: indexmap::IndexSet<_> = match publisher {
            Some(publisher) => [crate::control::state::DataStreamId::new(
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
                ids.insert(crate::control::state::DataStreamId::new(
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
            Some(publisher) => crate::control::patterns::Pattern::exact(
                room_id,
                publisher,
                crate::control::publication::AudienceName::Data {
                    topic: topic.clone(),
                    lane: lane.into(),
                },
            ),
            None => crate::control::patterns::Pattern::any_publisher(
                room_id,
                crate::control::publication::AudienceName::Data {
                    topic: topic.clone(),
                    lane: lane.into(),
                },
            ),
        };
        let _ =
            crate::control::patterns::retract_audience(&mut self.audiences, &pattern, &subscriber);
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::control::state::DataStreamId::new(
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
        id: crate::control::state::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        self.retire_publication(data_publication_id(&id, lane))
            .await
    }

    /// Make a stream's destinations match its declarations.
    pub(super) async fn reconcile_stream(
        &mut self,
        id: crate::control::state::DataStreamId,
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
                Some((key, actor.lanes.get(lane).route_action(key)))
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
