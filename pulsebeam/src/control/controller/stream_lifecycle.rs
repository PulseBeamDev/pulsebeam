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

fn install_stream_route_op(
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
        crate::control::publication::forwarding_plan(
            &binding.destinations,
            binding.publisher_shard,
            binding.reverse_route,
            groups,
            destination,
        )
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
        let registry = self.lanes.get(lane);
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => registry.ids_on_topic(&room_id, &topic),
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
        let registry = self.lanes.get(lane);
        let ids: Vec<_> = match publisher {
            Some(publisher) => vec![crate::shard::router::DataStreamId::new(
                room_id, publisher, topic,
            )],
            None => registry.ids_on_topic(&room_id, &topic),
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
        let Some(binding) = self.lanes.get(lane).get(&id) else {
            return true;
        };
        let publisher_shard = binding.publisher_shard;
        let source_key = binding.key;
        let destinations = binding.destinations.clone();
        let routes: Vec<_> = destinations
            .iter()
            .filter_map(|(destination, held)| Some((*destination, held.key.stream()?, held.route?)))
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
        for (destination, held) in destinations {
            let Some(key) = held.key.stream() else {
                continue;
            };
            self.lanes
                .get(lane)
                .retire_runtime(&mut self.state, destination, key);
        }
        self.lanes.get_mut(lane).remove(&id);
        true
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
            .lanes
            .get(lane)
            .get(&id)
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
        let destinations = wanted;
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
            let has_route = self.lanes.get(lane).get(&id).is_some_and(|binding| {
                binding
                    .destinations
                    .get(&destination)
                    .is_some_and(|d| d.route.is_some())
            });
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
            binding.destinations.insert(
                destination,
                crate::control::publication::Destination {
                    key: key.into(),
                    route: Some(route),
                },
            );
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
        let destinations = binding.destinations.clone();

        let mut targets = Vec::new();
        if let Some(key) = source_key {
            targets.push((
                publisher_shard,
                key,
                self.stream_plan(&id, lane, binding, publisher_shard),
                None,
            ));
        }
        for (destination, held) in &destinations {
            let (destination, Some(route)) = (*destination, held.route) else {
                continue;
            };
            let Some(key) = held.key.stream() else {
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
