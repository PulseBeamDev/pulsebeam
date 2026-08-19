use super::*;

fn should_publish_stream_views(retired: bool, added: bool) -> bool {
    !retired || added
}

/// The view ops a stream key implies.
///
/// None of these take a lane: `RuntimeStreamKey`'s variant *is* the lane, and
/// passing one alongside meant every call site carried a second source of truth
/// plus a `debug_assert` to catch the two disagreeing.
pub(super) fn insert_stream_runtime_op(
    key: crate::shard::router::RuntimeStreamKey,
    id: crate::shard::router::DataStreamId,
    publisher: crate::shard::participants::ParticipantKey,
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

fn set_stream_plan_op(
    key: crate::shard::router::RuntimeStreamKey,
    plan: crate::view::StreamPlan,
) -> crate::view::ViewOp {
    match key {
        crate::shard::router::RuntimeStreamKey::Unreliable(key) => crate::view::ViewOp::SetPlan {
            target: crate::view::PlanTarget::Unreliable(key),
            plan,
        },
        crate::shard::router::RuntimeStreamKey::Reliable(key) => crate::view::ViewOp::SetPlan {
            target: crate::view::PlanTarget::Reliable(key),
            plan,
        },
    }
}

fn remove_stream_ops(key: crate::shard::router::RuntimeStreamKey) -> [crate::view::ViewOp; 2] {
    match key {
        crate::shard::router::RuntimeStreamKey::Unreliable(key) => [
            crate::view::ViewOp::RemovePlan {
                target: crate::view::PlanTarget::Unreliable(key),
            },
            crate::view::ViewOp::RemoveUnreliableRuntime { key },
        ],
        crate::shard::router::RuntimeStreamKey::Reliable(key) => [
            crate::view::ViewOp::RemovePlan {
                target: crate::view::PlanTarget::Reliable(key),
            },
            crate::view::ViewOp::RemoveReliableRuntime { key },
        ],
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
        route: route.route,
        binding: crate::view::RouteBinding {
            epoch: route.epoch,
            action,
        },
    }
}

fn retire_route_op(route: RouteHandle) -> crate::view::ViewOp {
    crate::view::ViewOp::RetireRoute {
        route: route.route,
        epoch: route.epoch,
    }
}

/// The label a data stream is published under.
fn data_label(topic: &crate::track::Topic, lane: StreamLane) -> String {
    let lane = match lane {
        StreamLane::Unreliable => crate::track::DataLane::Realtime,
        StreamLane::Reliable => crate::track::DataLane::Reliable,
    };
    crate::track::publication_label(lane, topic)
}

/// The catalog identity of a data stream.
///
/// `StreamLane` is the control plane's name for the lane and `DataLane` the
/// label grammar's; they are the same two lanes, and this is the one place the
/// two spellings meet.
fn data_publication_id(
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
            .into_iter()
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
        key: crate::shard::router::RuntimeStreamKey,
    ) {
        let lane = StreamLane::of(key);
        let Some(publisher) = self
            .core
            .registry
            .get_participant(&id.publisher_id)
            .and_then(|meta| meta.binding)
        else {
            debug_assert!(false, "a stream publisher must have a participant key");
            return;
        };
        let publication_id = data_publication_id(&id, lane);
        let existing_reverse = self
            .catalog
            .get(&publication_id)
            .and_then(|held| held.reverse_route);
        let lane_label = match lane {
            StreamLane::Unreliable => crate::track::DataLane::Realtime,
            StreamLane::Reliable => crate::track::DataLane::Reliable,
        };
        self.catalog
            .insert(crate::control::publication::Publication {
                id: publication_id,
                room: id.room_id,
                publisher: id.publisher_id,
                publisher_shard: shard_id,
                publisher_key: publisher,
                origin_key: key.into(),
                reverse_route: existing_reverse,
                destinations: self
                    .catalog
                    .get(&publication_id)
                    .map(|held| held.destinations.clone())
                    .unwrap_or_default(),
                media: crate::control::publication::Media::Data {
                    lane: lane_label,
                    topic: id.topic.clone(),
                },
            });

        if matches!(lane, StreamLane::Reliable) && existing_reverse.is_none() {
            let Some(stream) = (match key {
                crate::shard::router::RuntimeStreamKey::Reliable(stream) => Some(stream),
                _ => None,
            }) else {
                debug_assert!(false, "reliable readiness must carry a reliable key");
                return;
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
                return;
            };
            let Some(binding) = self.catalog.get_mut(&data_publication_id(&id, lane)) else {
                debug_assert!(false, "stream binding must survive reverse route install");
                return;
            };
            binding.reverse_route = Some(route);
        }

        self.reconcile_stream(id, lane).await;
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
        let (_, membership_ops) = crate::control::patterns::declare_audience(
            &mut self.data_patterns,
            pattern,
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: channel,
            },
            crate::view::Delivery::Data(channel),
            crate::view::AudienceKind::Data,
        );
        if !self.publish_ops(membership_ops) {
            debug_assert!(false, "data group membership must publish");
        }
        // Which streams this reaches is a catalog question now, not something
        // the registry has to remember on the subscriber's behalf.
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => self.streams_on_topic(room_id, &topic, lane),
        };
        for id in ids {
            self.reconcile_stream(id, lane).await;
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
        let (_, membership_ops) = crate::control::patterns::retract_audience(
            &mut self.data_patterns,
            &pattern,
            &subscriber,
            crate::view::AudienceKind::Data,
        );
        if !self.publish_ops(membership_ops) {
            debug_assert!(false, "data group retraction must publish");
        }
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => self.streams_on_topic(room_id, &topic, lane),
        };
        for id in ids {
            self.reconcile_stream(id, lane).await;
        }
    }

    pub(super) async fn retire_stream_destinations(
        &mut self,
        id: &crate::shard::router::DataStreamId,
        lane: StreamLane,
        stale: &[(
            crate::id::ShardId,
            crate::shard::router::RuntimeStreamKey,
            RouteHandle,
        )],
    ) -> bool {
        let Some(binding) = self.catalog.get(&data_publication_id(&id, lane)) else {
            debug_assert!(false, "stale routes must belong to a stream binding");
            return false;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.origin_key.stream();
        let mut source_plan =
            source_key.and_then(|_| self.plan_for(data_publication_id(id, lane), publisher_shard));
        if let Some(plan) = source_plan.as_mut() {
            plan.remote_routes
                .retain(|route| !stale.iter().any(|(shard, _, _)| route.shard_id == *shard));
        }

        let mut ops = Vec::new();
        for (destination, key, route) in stale {
            ops.push((*destination, retire_route_op(*route)));
            ops.extend(remove_stream_ops(*key).map(|op| (*destination, op)));
        }
        if let Some((key, plan)) = source_key.zip(source_plan) {
            ops.push((publisher_shard, set_stream_plan_op(key, plan)));
        }
        if !self.publish_ops(ops) {
            return false;
        }

        let now = tokio::time::Instant::now();
        for (destination, _, route) in stale {
            let Some(binding) = self.catalog.get_mut(&data_publication_id(&id, lane)) else {
                debug_assert!(false, "stream binding must survive route retirement");
                return false;
            };
            binding.destinations.shift_remove(destination);
            self.state
                .release_endpoint(*destination, route.route.slot(), now);
        }
        true
    }

    pub(super) async fn retire_stream_binding(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> bool {
        self.retire_publication(data_publication_id(&id, lane), move |actor, shard, key| {
            if let Some(key) = key.stream() {
                actor
                    .lanes
                    .get(lane)
                    .retire_runtime(&mut actor.state, shard, key);
            }
        })
        .await
    }

    /// The shards holding at least one declared consumer of this stream.
    fn stream_destinations(
        &self,
        id: &crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) -> Vec<crate::id::ShardId> {
        let subject = crate::control::patterns::Subject {
            room: id.room_id,
            publisher: id.publisher_id,
            name: (id.topic.clone(), lane),
        };
        let mut shards = Vec::new();
        for group in self.data_patterns.match_subject(&subject) {
            for shard in self.data_patterns.shards_of(group) {
                if !shards.contains(&shard) {
                    shards.push(shard);
                }
            }
        }
        shards
    }

    pub(super) async fn reconcile_stream(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) {
        // Which shards consume this stream is a question about declarations,
        // not about what the binding happens to remember. Asking the pattern
        // table is what lets the binding stop tracking subscribers at all.
        let wanted = self.stream_destinations(&id, lane);
        let stale: Vec<_> = self
            .catalog
            .get(&data_publication_id(&id, lane))
            .map(|binding| {
                binding
                    .destinations
                    .iter()
                    .filter_map(|(destination, held)| {
                        if wanted.contains(destination) {
                            return None;
                        }
                        Some((*destination, held.key.stream()?, held.route?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let retired = !stale.is_empty();
        if retired && !self.retire_stream_destinations(&id, lane, &stale).await {
            debug_assert!(false, "stream route retirement must complete");
            return;
        }
        let publication_id = data_publication_id(&id, lane);
        let stream = id.clone();
        let added = self
            .install_destinations(publication_id, move |actor, destination| {
                let key = actor
                    .lanes
                    .get(lane)
                    .mint(&mut actor.state, destination, &stream)?;
                let action = actor.lanes.get(lane).route_action(key)?;
                Some((key.into(), action))
            })
            .await;

        if should_publish_stream_views(retired, added) {
            self.publish_publication(data_publication_id(&id, lane))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_views_publish_when_topology_changes() {
        assert!(should_publish_stream_views(false, false));
        assert!(should_publish_stream_views(false, true));
        assert!(should_publish_stream_views(true, true));
        assert!(!should_publish_stream_views(true, false));
    }
}
