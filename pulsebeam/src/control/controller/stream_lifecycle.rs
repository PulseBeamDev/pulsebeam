use super::*;

fn needs_stream_route(
    publisher_shard: crate::id::ShardId,
    destination: crate::id::ShardId,
    has_route: bool,
) -> bool {
    destination != publisher_shard && !has_route
}

fn should_publish_stream_views(retired: bool, added: bool) -> bool {
    !retired || added
}

/// The view ops a stream key implies.
///
/// None of these take a lane: `RuntimeStreamKey`'s variant *is* the lane, and
/// passing one alongside meant every call site carried a second source of truth
/// plus a `debug_assert` to catch the two disagreeing.
fn insert_stream_runtime_op(
    key: crate::shard::router::RuntimeStreamKey,
    id: crate::shard::router::DataStreamId,
    publisher: crate::shard::participants::ParticipantKey,
) -> crate::view::ViewOp {
    match key {
        crate::shard::router::RuntimeStreamKey::Data(key) => {
            crate::view::ViewOp::InsertDataRuntime { key, id, publisher }
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
        crate::shard::router::RuntimeStreamKey::Data(key) => {
            crate::view::ViewOp::SetDataPlan { key, plan }
        }
        crate::shard::router::RuntimeStreamKey::Reliable(key) => {
            crate::view::ViewOp::SetReliablePlan { key, plan }
        }
    }
}

fn remove_stream_ops(key: crate::shard::router::RuntimeStreamKey) -> [crate::view::ViewOp; 2] {
    match key {
        crate::shard::router::RuntimeStreamKey::Data(key) => [
            crate::view::ViewOp::RemoveDataPlan { key },
            crate::view::ViewOp::RemoveDataRuntime { key },
        ],
        crate::shard::router::RuntimeStreamKey::Reliable(key) => [
            crate::view::ViewOp::RemoveReliablePlan { key },
            crate::view::ViewOp::RemoveReliableRuntime { key },
        ],
    }
}

fn install_stream_route_op(
    key: crate::shard::router::RuntimeStreamKey,
    route: RouteHandle,
) -> crate::view::ViewOp {
    let action = match key {
        crate::shard::router::RuntimeStreamKey::Data(stream) => RouteAction::Data { stream },
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

impl ControllerActor {
    pub(super) fn stream_plan(
        &self,
        id: &crate::shard::router::DataStreamId,
        lane: StreamLane,
        binding: &crate::control::lanes::StreamBinding,
        destination: crate::id::ShardId,
    ) -> crate::view::StreamPlan {
        // As with audio: the plan names its audiences and the shard holds the
        // membership, so a subscriber arriving does not rewrite the plan of
        // every stream already published on the topic.
        let subject = crate::control::patterns::Subject {
            room: id.room_id,
            publisher: id.publisher_id,
            name: (id.topic.clone(), lane),
        };
        let groups = self.data_patterns.match_subject(&subject);
        let remote_routes = if destination == binding.publisher_shard {
            binding
                .routes
                .iter()
                .map(|(shard_id, handle)| crate::view::RemoteRoutePlan {
                    shard_id: *shard_id,
                    route: handle.route,
                    epoch: handle.epoch,
                })
                .collect()
        } else {
            Vec::new()
        };
        let reverse_route = binding
            .reverse_route
            .map(|handle| crate::view::RemoteRoutePlan {
                shard_id: binding.publisher_shard,
                route: handle.route,
                epoch: handle.epoch,
            });
        crate::view::StreamPlan {
            groups,
            local_subscribers: Vec::new(),
            remote_routes,
            reverse_route,
        }
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
        let binding = self
            .lanes
            .get_mut(lane)
            .declare(id.clone(), shard_id, publisher, key);

        if matches!(lane, StreamLane::Reliable) && binding.reverse_route.is_none() {
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
            let Some(binding) = self.lanes.get_mut(lane).get_mut(&id) else {
                debug_assert!(false, "stream binding must survive reverse route install");
                return;
            };
            binding.reverse_route = Some(route);
        }

        self.lanes.get_mut(lane).apply_wildcards(&id);
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
        let entry = Subscriber {
            key: subscriber_key,
            channel,
        };
        let pattern = match publisher {
            Some(publisher) => {
                crate::control::patterns::Pattern::exact(room_id, publisher, (topic.clone(), lane))
            }
            None => {
                crate::control::patterns::Pattern::any_publisher(room_id, (topic.clone(), lane))
            }
        };
        let (_, displaced) = self.data_patterns.declare(
            pattern.clone(),
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: channel,
            },
        );
        let mut membership: Vec<(crate::id::ShardId, crate::view::ViewOp)> = displaced
            .into_iter()
            .map(|(group, _)| {
                (
                    shard_id,
                    crate::view::ViewOp::DataGroupRemove {
                        group,
                        key: subscriber_key,
                    },
                )
            })
            .collect();
        if let Some(group) = self.data_patterns.group_of(&pattern) {
            membership.push((
                shard_id,
                crate::view::ViewOp::DataGroupInsert {
                    group,
                    key: subscriber_key,
                    channel,
                },
            ));
        }
        if !self.publish_ops(membership) {
            debug_assert!(false, "data group membership must publish");
        }
        let registry = self.lanes.get_mut(lane);
        let ids = match publisher {
            Some(publisher) => {
                let id = crate::shard::router::DataStreamId::new(room_id, publisher, topic);
                registry
                    .subscribe(id, shard_id, subscriber, entry)
                    .into_iter()
                    .collect()
            }
            None => registry.subscribe_wildcard(
                room_id,
                topic,
                subscriber,
                WildcardSubscriber {
                    shard: shard_id,
                    key: subscriber_key,
                    channel,
                },
            ),
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
        let departing = self
            .data_patterns
            .group_of(&pattern)
            .zip(self.data_patterns.member_key(&pattern, &subscriber));
        self.data_patterns.undeclare(&pattern, &subscriber);
        if let Some((group, (shard, key))) = departing {
            let ops = vec![(shard, crate::view::ViewOp::DataGroupRemove { group, key })];
            if !self.publish_ops(ops) {
                debug_assert!(false, "data group retraction must publish");
            }
        }
        let registry = self.lanes.get_mut(lane);
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => {
                registry.unsubscribe_wildcard(room_id, topic.clone(), &subscriber);
                registry.ids_on_topic(&room_id, &topic)
            }
        };
        // A named publisher also clears a subscription still parked against a
        // stream that never became ready; a wildcard has nothing parked.
        let drop_pending = publisher.is_some();
        let changed: Vec<_> = ids
            .into_iter()
            .filter(|id| registry.unsubscribe(id, &subscriber, drop_pending))
            .collect();
        for id in changed {
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
        let Some(binding) = self.lanes.get(lane).get(id) else {
            debug_assert!(false, "stale routes must belong to a stream binding");
            return false;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let mut source_plan =
            source_key.map(|_| self.stream_plan(id, lane, binding, publisher_shard));
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
            let Some(binding) = self.lanes.get_mut(lane).get_mut(id) else {
                debug_assert!(false, "stream binding must survive route retirement");
                return false;
            };
            binding.routes.remove(destination);
            binding.destination_keys.remove(destination);
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
        let Some(binding) = self.lanes.get(lane).get(&id) else {
            self.lanes.get_mut(lane).forget_pending(&id);
            return true;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let destination_keys = binding.destination_keys.clone();
        let routes: Vec<_> = binding
            .routes
            .iter()
            .filter_map(|(destination, route)| {
                let Some(key) = destination_keys.get(destination).copied() else {
                    debug_assert!(false, "a stream route must have a destination key");
                    return None;
                };
                Some((*destination, key, *route))
            })
            .collect();

        let mut ops = Vec::new();
        for (destination, key, route) in &routes {
            ops.push((*destination, retire_route_op(*route)));
            ops.extend(remove_stream_ops(*key).map(|op| (*destination, op)));
        }
        if let Some(key) = source_key {
            ops.extend(remove_stream_ops(key).map(|op| (publisher_shard, op)));
        }
        if !self.publish_ops(ops) {
            return false;
        }

        let now = tokio::time::Instant::now();
        for (destination, _, route) in &routes {
            self.state
                .release_endpoint(*destination, route.route.slot(), now);
        }
        if let Some(key) = source_key {
            self.lanes
                .get(lane)
                .retire_runtime(&mut self.state, publisher_shard, key);
        }
        for (destination, key) in destination_keys {
            self.lanes
                .get(lane)
                .retire_runtime(&mut self.state, destination, key);
        }
        self.lanes.get_mut(lane).remove(&id);
        true
    }

    pub(super) async fn reconcile_stream(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) {
        let stale: Vec<_> = self
            .lanes
            .get(lane)
            .get(&id)
            .map(|binding| {
                binding
                    .routes
                    .iter()
                    .filter_map(|(destination, route)| {
                        if binding.subscribers.contains_key(destination) {
                            return None;
                        }
                        let key = binding.destination_keys.get(destination).copied()?;
                        Some((*destination, key, *route))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let retired = !stale.is_empty();
        if retired && !self.retire_stream_destinations(&id, lane, &stale).await {
            debug_assert!(false, "stream route retirement must complete");
            return;
        }
        let destinations: Vec<_> = self
            .lanes
            .get(lane)
            .get(&id)
            .map(|binding| binding.subscribers.keys().copied().collect())
            .unwrap_or_default();
        let Some(publisher_shard) = self
            .lanes
            .get(lane)
            .get(&id)
            .map(|binding| binding.publisher_shard)
        else {
            return;
        };
        let mut added = false;
        for destination in destinations {
            let has_route = self
                .lanes
                .get(lane)
                .get(&id)
                .is_some_and(|binding| binding.routes.contains_key(&destination));
            if !needs_stream_route(publisher_shard, destination, has_route) {
                continue;
            }
            let Some(key) = self.lanes.get(lane).mint(&mut self.state, destination, &id) else {
                continue;
            };
            let Some(action) = self.lanes.get(lane).route_action(key) else {
                debug_assert!(false, "stream preparation returned the wrong lane");
                continue;
            };
            let Some(binding) = self.lanes.get(lane).get(&id) else {
                return;
            };
            let plan = self.stream_plan(&id, lane, binding, destination);
            let Some(route) = self
                .grant_route_with_plan(destination, action, lane, plan.clone())
                .await
            else {
                continue;
            };
            let Some(binding) = self.lanes.get_mut(lane).get_mut(&id) else {
                return;
            };
            binding.destination_keys.insert(destination, key);
            binding.routes.insert(destination, route);
            added = true;
        }
        if should_publish_stream_views(retired, added) {
            self.publish_stream_views(id, lane).await;
        }
    }

    pub(super) async fn publish_stream_views(
        &mut self,
        id: crate::shard::router::DataStreamId,
        lane: StreamLane,
    ) {
        let Some(binding) = self.lanes.get(lane).get(&id) else {
            return;
        };
        let publisher = binding.publisher;
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let destination_keys = binding.destination_keys.clone();
        let routes = binding.routes.clone();

        let mut targets = Vec::new();
        if let Some(key) = source_key {
            targets.push((
                publisher_shard,
                key,
                self.stream_plan(&id, lane, binding, publisher_shard),
                None,
            ));
        }
        for (destination, key) in destination_keys {
            let Some(route) = routes.get(&destination).copied() else {
                continue;
            };
            targets.push((
                destination,
                key,
                self.stream_plan(&id, lane, binding, destination),
                Some(route),
            ));
        }

        let mut ops = Vec::new();
        for (shard, key, plan, route) in targets {
            ops.push((shard, insert_stream_runtime_op(key, id.clone(), publisher)));
            ops.push((shard, set_stream_plan_op(key, plan)));
            ops.extend(route.map(|route| (shard, install_stream_route_op(key, route))));
        }
        self.publish_ops(ops);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_decision_excludes_the_publisher_and_existing_routes() {
        let publisher = crate::id::ShardId::new(0);
        let destination = crate::id::ShardId::new(1);

        assert!(!needs_stream_route(publisher, publisher, false));
        assert!(needs_stream_route(publisher, destination, false));
        assert!(!needs_stream_route(publisher, destination, true));
    }

    #[test]
    fn stream_views_publish_when_topology_changes() {
        assert!(should_publish_stream_views(false, false));
        assert!(should_publish_stream_views(false, true));
        assert!(should_publish_stream_views(true, true));
        assert!(!should_publish_stream_views(true, false));
    }
}
