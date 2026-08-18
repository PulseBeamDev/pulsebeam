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

impl ControllerActor {
    pub(super) fn stream_plan(
        &self,
        binding: &crate::control::lanes::StreamBinding,
        destination: crate::id::ShardId,
    ) -> crate::view::StreamPlan {
        let local_subscribers = binding
            .subscribers
            .get(&destination)
            .map(|subscribers| {
                subscribers
                    .values()
                    .map(|subscriber| (subscriber.key, subscriber.channel))
                    .collect()
            })
            .unwrap_or_default();
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
            local_subscribers,
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

    pub(super) fn stage_remove_stream_plan(
        view: &mut crate::view::ShardViewWriter,
        generation: u64,
        lane: StreamLane,
        key: crate::shard::router::RuntimeStreamKey,
    ) {
        match (lane, key) {
            (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                view.stage(generation, crate::view::ViewOp::RemoveDataPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveDataRuntime { key });
            }
            (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                view.stage(generation, crate::view::ViewOp::RemoveReliablePlan { key });
                view.stage(
                    generation,
                    crate::view::ViewOp::RemoveReliableRuntime { key },
                );
            }
            _ => debug_assert!(false, "stream key and lane disagree"),
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
        let mut source_plan = source_key.map(|_| self.stream_plan(binding, publisher_shard));
        if let Some(plan) = source_plan.as_mut() {
            plan.remote_routes
                .retain(|route| !stale.iter().any(|(shard, _, _)| route.shard_id == *shard));
        }

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };
        for (destination, key, route) in stale {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "a stream route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            Self::stage_remove_stream_plan(view, generation, lane, *key);
        }
        if let Some((key, plan)) = source_key.zip(source_plan) {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a stream publisher must name a local view");
                self.abort_transaction(now);
                return false;
            };
            match (lane, key) {
                (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                    view.stage(generation, crate::view::ViewOp::SetDataPlan { key, plan });
                }
                (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetReliablePlan { key, plan },
                    );
                }
                _ => debug_assert!(false, "stream key and lane disagree"),
            }
        }

        let mut published = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                published.push(shard);
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a published stream retirement must commit");
            self.abort_transaction(now);
            return false;
        }

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
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };
        for (destination, key, route) in &routes {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "a stream route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            Self::stage_remove_stream_plan(view, generation, lane, *key);
        }
        if let Some(key) = source_key {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a stream publisher must name a local view");
                self.abort_transaction(now);
                return false;
            };
            Self::stage_remove_stream_plan(view, generation, lane, key);
        }
        let mut published = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                published.push(shard);
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a published stream retirement must commit");
            self.abort_transaction(now);
            return false;
        }
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
            let plan = self.stream_plan(binding, destination);
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
        let publisher_key = binding.publisher;
        let binding_data = (
            binding.publisher_shard,
            binding.key,
            binding.destination_keys.clone(),
            binding.routes.clone(),
        );
        let mut targets = Vec::new();
        if let Some(key) = binding_data.1 {
            targets.push((
                binding_data.0,
                key,
                self.stream_plan(binding, binding_data.0),
                None,
            ));
        }
        for (destination, key) in binding_data.2 {
            let Some(route) = binding_data.3.get(&destination).copied() else {
                continue;
            };
            targets.push((
                destination,
                key,
                self.stream_plan(binding, destination),
                Some(route),
            ));
        }
        if targets.is_empty() {
            return;
        }
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return;
        };
        for (shard, key, plan, route) in targets {
            let Some(view) = self.view_mut(shard) else {
                self.abort_transaction(now);
                return;
            };
            match (lane, key) {
                (StreamLane::Data, crate::shard::router::RuntimeStreamKey::Data(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::InsertDataRuntime {
                            key,
                            id: id.clone(),
                            publisher: publisher_key,
                        },
                    );
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetDataPlan {
                            key,
                            plan: plan.clone(),
                        },
                    );
                    if let Some(route) = route {
                        view.stage(
                            generation,
                            crate::view::ViewOp::InstallRoute {
                                route: route.route,
                                binding: crate::view::RouteBinding {
                                    epoch: route.epoch,
                                    action: RouteAction::Data { stream: key },
                                },
                            },
                        );
                    }
                }
                (StreamLane::Reliable, crate::shard::router::RuntimeStreamKey::Reliable(key)) => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::InsertReliableRuntime {
                            key,
                            id: id.clone(),
                            publisher: publisher_key,
                        },
                    );
                    view.stage(
                        generation,
                        crate::view::ViewOp::SetReliablePlan {
                            key,
                            plan: plan.clone(),
                        },
                    );
                    if let Some(route) = route {
                        view.stage(
                            generation,
                            crate::view::ViewOp::InstallRoute {
                                route: route.route,
                                binding: crate::view::RouteBinding {
                                    epoch: route.epoch,
                                    action: RouteAction::Reliable { stream: key },
                                },
                            },
                        );
                    }
                }
                _ => debug_assert!(false, "stream key and lane disagree"),
            }
        }
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let shard = crate::id::ShardId::new(index);
            if self
                .view_mut(shard)
                .is_some_and(|view| view.publish().is_some())
            {
                affected.push(shard);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
        }
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
