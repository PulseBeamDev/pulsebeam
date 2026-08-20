use super::*;

fn installs_video_route(change: crate::control::subscriptions::InterestChange) -> bool {
    matches!(
        change,
        crate::control::subscriptions::InterestChange::Install
    )
}

impl ControllerActor {
    pub(super) async fn drain_pending_track_subscriptions(
        &mut self,
        track_id: crate::entity::TrackId,
    ) {
        let pending = self
            .pending_track_subscriptions
            .remove(&track_id)
            .unwrap_or_default();
        for subscription in pending {
            if let Some(count) = self.pending_track_counts.get_mut(&subscription.subscriber) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.pending_track_counts.remove(&subscription.subscriber);
                }
            }
            self.on_track_subscribed(
                subscription.shard_id,
                subscription.subscriber,
                subscription.subscriber_key,
                subscription.slot,
                subscription.track,
            )
            .await;
        }
    }

    pub(super) fn remove_pending_track_subscription(
        &mut self,
        track_id: crate::entity::TrackId,
        subscriber: ParticipantId,
        slot: crate::keys::DownstreamSlotKey,
    ) {
        let mut removed = false;
        if let Some(pending) = self.pending_track_subscriptions.get_mut(&track_id) {
            pending.retain(|entry| {
                let matches = entry.subscriber == subscriber && entry.slot == slot;
                removed |= matches;
                !matches
            });
        }
        if self
            .pending_track_subscriptions
            .get(&track_id)
            .is_some_and(Vec::is_empty)
        {
            self.pending_track_subscriptions.remove(&track_id);
        }
        if removed && let Some(count) = self.pending_track_counts.get_mut(&subscriber) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pending_track_counts.remove(&subscriber);
            }
        }
    }

    pub(super) async fn install_video_runtimes(&mut self, track_id: crate::entity::TrackId) {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            debug_assert!(false, "video runtime installation requires a track binding");
            return;
        };
        let publisher_shard = binding.publisher_shard;
        let origin = binding.meta.origin;
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return;
        };
        let destinations: Vec<_> = self
            .core
            .registry
            .participants_in_room(&room_id)
            .into_iter()
            .map(|(_, shard, _)| shard)
            .filter(|shard| *shard != publisher_shard)
            .collect();
        for destination in destinations {
            if self
                .track_bindings
                .get(&track_id)
                .is_some_and(|binding| binding.fanouts.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                debug_assert!(false, "a subscriber shard must accept a track runtime");
                continue;
            };
            let Some(binding) = self.track_bindings.get_mut(&track_id) else {
                debug_assert!(
                    false,
                    "track binding disappeared during runtime installation"
                );
                return;
            };
            binding.fanouts.insert(destination, key);
        }
    }

    pub(super) async fn retire_track_binding(&mut self, track_id: crate::entity::TrackId) -> bool {
        let video_routes = self.subscriptions.remove_stream(&track_id);
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return true;
        };
        let publisher_shard = binding.publisher_shard;
        let publisher_fanout = binding.publisher_fanout;
        let reverse_route = binding.reverse_route;
        let fanouts = binding.fanouts.clone();
        let audio_fanouts = binding.audio_fanouts.clone();
        let audio_routes = binding.audio_routes.clone();

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            debug_assert!(false, "begin creates a pending lifecycle transaction");
            return false;
        };

        let mut endpoint_releases = Vec::new();
        for retired in &video_routes {
            let Some(view) = self.view_mut(retired.destination) else {
                debug_assert!(false, "a video route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: retired.route.route,
                    epoch: retired.route.epoch,
                },
            );
            if let Some(key) = fanouts.get(&retired.destination).copied() {
                view.stage(generation, crate::view::ViewOp::RemoveTrackPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
            endpoint_releases.push((retired.destination, retired.route));
        }
        for (destination, route) in &audio_routes {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "an audio route must name a local view");
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
            if let Some(key) = audio_fanouts.get(destination).copied() {
                view.stage(generation, crate::view::ViewOp::RemoveAudioPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
            endpoint_releases.push((*destination, *route));
        }
        if let Some(route) = reverse_route {
            let Some(view) = self.view_mut(publisher_shard) else {
                debug_assert!(false, "a reverse route must name a local view");
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
            endpoint_releases.push((publisher_shard, route));
        }

        let Some(view) = self.view_mut(publisher_shard) else {
            debug_assert!(false, "a track publisher must name a local view");
            self.abort_transaction(now);
            return false;
        };
        view.stage(
            generation,
            crate::view::ViewOp::RemoveTrackPlan {
                key: publisher_fanout,
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::RemoveTrackRuntime {
                key: publisher_fanout,
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::RemoveAudioPlan {
                key: publisher_fanout,
            },
        );
        for (&destination, &key) in &fanouts {
            if destination == publisher_shard {
                continue;
            }
            if let Some(view) = self.view_mut(destination) {
                view.stage(generation, crate::view::ViewOp::RemoveTrackPlan { key });
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
        }
        for (&destination, &key) in &audio_fanouts {
            if let Some(view) = self.view_mut(destination) {
                view.stage(generation, crate::view::ViewOp::RemoveAudioPlan { key });
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
            debug_assert!(false, "a published track retirement must commit");
            self.abort_transaction(now);
            return false;
        }
        for (shard, route) in endpoint_releases {
            self.state.release_endpoint(shard, route.route.slot(), now);
        }
        self.state.remove_track(publisher_shard, publisher_fanout);
        for (&destination, &key) in &fanouts {
            self.state.remove_track(destination, key);
        }
        for (&destination, &key) in &audio_fanouts {
            self.state.remove_track(destination, key);
        }
        self.track_bindings.remove(&track_id);
        true
    }

    /// A shard reported a newly published track.
    ///
    /// Its reverse route is opened here, before anything learns the track
    /// exists — a subscriber that heard about it first would have nowhere to
    /// send a keyframe request. Returns the descriptor with the handle
    /// stamped on, for the ordinary topology projection to distribute.
    pub(super) async fn on_track_published(
        &mut self,
        shard_id: crate::id::ShardId,
        mut track: crate::track::Track,
        fanout: crate::shard::router::TrackKey,
    ) -> Option<crate::track::Track> {
        let handle = self
            .grant_route(
                shard_id,
                crate::route::RouteAction::Reverse {
                    target: crate::route::ReverseTarget::Track { track: fanout },
                },
            )
            .await?;
        track.reverse = Some(handle);
        Some(track)
    }

    pub(super) fn prepare_track_key(
        &mut self,
        shard_id: crate::id::ShardId,
        track_id: crate::entity::TrackId,
        origin: ParticipantId,
    ) -> Option<crate::shard::router::TrackKey> {
        self.state.mint_track(shard_id, track_id, origin)
    }

    pub(super) async fn install_audio_routes(&mut self, track_id: crate::entity::TrackId) {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return;
        };
        let origin = binding.meta.origin;
        let publisher_shard = binding.publisher_shard;
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return;
        };
        let mut local_subscribers: HashMap<
            crate::id::ShardId,
            Vec<crate::shard::participants::ParticipantKey>,
        > = HashMap::new();
        for (participant, shard, key) in self.core.registry.participants_in_room(&room_id) {
            if participant != origin
                && let Some(key) = key
            {
                local_subscribers.entry(shard).or_default().push(key);
            }
        }
        let destinations: Vec<_> = local_subscribers.keys().copied().collect();
        for destination in destinations {
            if destination == publisher_shard {
                continue;
            }
            if self
                .track_bindings
                .get(&track_id)
                .is_some_and(|binding| binding.audio_fanouts.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                continue;
            };
            let plan = crate::view::AudioForwardingPlan {
                track_id,
                origin,
                local_subscribers: local_subscribers
                    .get(&destination)
                    .cloned()
                    .unwrap_or_default(),
                remote_routes: Vec::new(),
                reverse_route: None,
            };
            let Some(route) = self
                .grant_route_binding(
                    destination,
                    RouteAction::Audio { track: key },
                    None,
                    Some(plan),
                    None,
                    None,
                )
                .await
            else {
                continue;
            };
            let Some(binding) = self.track_bindings.get_mut(&track_id) else {
                return;
            };
            binding.audio_fanouts.insert(destination, key);
            binding.audio_routes.insert(destination, route);
        }
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return;
        };
        let remote_routes = binding
            .audio_routes
            .iter()
            .map(|(shard_id, route)| crate::view::RemoteRoutePlan {
                shard_id: *shard_id,
                route: route.route,
                epoch: route.epoch,
            })
            .collect();
        let source_plan = crate::view::AudioForwardingPlan {
            track_id,
            origin,
            local_subscribers: local_subscribers
                .get(&publisher_shard)
                .cloned()
                .unwrap_or_default(),
            remote_routes,
            reverse_route: binding
                .reverse_route
                .map(|route| crate::view::RemoteRoutePlan {
                    shard_id: publisher_shard,
                    route: route.route,
                    epoch: route.epoch,
                }),
        };
        let mut targets = vec![(publisher_shard, binding.publisher_fanout, source_plan, None)];
        for (destination, key) in &binding.audio_fanouts {
            let Some(route) = binding.audio_routes.get(destination).copied() else {
                continue;
            };
            targets.push((
                *destination,
                *key,
                crate::view::AudioForwardingPlan {
                    track_id,
                    origin,
                    local_subscribers: local_subscribers
                        .get(destination)
                        .cloned()
                        .unwrap_or_default(),
                    remote_routes: Vec::new(),
                    reverse_route: None,
                },
                Some(route),
            ));
        }
        self.publish_audio_plans(targets).await;
    }

    pub(super) async fn publish_audio_plans(
        &mut self,
        targets: Vec<(
            crate::id::ShardId,
            crate::shard::router::TrackKey,
            crate::view::AudioForwardingPlan,
            Option<RouteHandle>,
        )>,
    ) {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return;
        };
        for (shard, key, plan, route) in targets {
            let Some(binding) = self.track_bindings.get(&plan.track_id) else {
                self.abort_transaction(now);
                return;
            };
            let publisher_fanout = binding.publisher_fanout;
            let descriptor = crate::view::TrackDescriptor {
                id: binding.meta.id,
                origin_key: binding.publisher_participant,
                participant: (shard == binding.publisher_shard)
                    .then_some(binding.publisher_participant),
                encodings: binding.encodings.clone(),
                states: binding.states.clone(),
                publication: binding.publication.clone(),
                audience: self.track_audience_on_shard(binding.meta.origin, shard),
            };
            let Some(view) = self.view_mut(shard) else {
                self.abort_transaction(now);
                return;
            };
            if key != publisher_fanout {
                view.stage(
                    generation,
                    crate::view::ViewOp::InsertTrackRuntime { key, descriptor },
                );
            }
            view.stage(
                generation,
                crate::view::ViewOp::SetAudioPlan {
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
                            action: RouteAction::Audio { track: key },
                        },
                    },
                );
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

    /// A shard reported a new local consumer for a track.
    ///
    /// The shard did not ask for anything. This decides whether that shard
    /// now needs a route, installs one if so, and tells the publisher's shard
    /// to start forwarding — the three things the shard used to do by asking.
    pub(super) async fn on_track_subscribed(
        &mut self,
        shard_id: crate::id::ShardId,
        subscriber: ParticipantId,
        subscriber_key: crate::shard::participants::ParticipantKey,
        slot: crate::keys::DownstreamSlotKey,
        track: crate::track::TrackMeta,
    ) {
        let Some(subscriber_room) = self
            .core
            .registry
            .get_participant(&subscriber)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a subscription must come from a live participant");
            return;
        };
        if subscriber_room != track.room_id {
            metrics::counter!("track_subscription_room_rejected").increment(1);
            return;
        }
        let Some(origin_room) = self
            .core
            .registry
            .get_participant(&track.origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a live origin");
            return;
        };
        if origin_room != track.room_id {
            debug_assert!(false, "track metadata room must match its origin");
            return;
        }
        let fanout = {
            let Some(binding) = self.track_bindings.get(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            if shard_id == binding.publisher_shard {
                binding.publisher_fanout
            } else if let Some(&fanout) = binding.fanouts.get(&shard_id) {
                fanout
            } else {
                let Some(fanout) = self.prepare_track_key(shard_id, track.id, track.origin) else {
                    debug_assert!(false, "a subscriber shard must accept a track runtime");
                    return;
                };
                let Some(binding) = self.track_bindings.get_mut(&track.id) else {
                    return;
                };
                binding.fanouts.insert(shard_id, fanout);
                fanout
            }
        };
        let change = self.subscriptions.subscribe(
            shard_id,
            track.id,
            subscriber,
            subscriber_key,
            slot,
            track.shard_id,
        );
        {
            let Some(binding) = self.track_bindings.get_mut(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            binding.fanouts.insert(shard_id, fanout);
        }

        if installs_video_route(change) {
            let Some((_, plan)) = self.track_plan(track.id, shard_id) else {
                debug_assert!(false, "a first subscription must have a compiled plan");
                return;
            };
            let Some(handle) = self.install_video_route(shard_id, fanout, plan).await else {
                // The fanout key stays. Only the route failed, and the shard is
                // told which track a route carries by the route itself, so the
                // retry re-stages this same key — dropping it here would mint a
                // fresh one on every attempt and abandon the last in the arena.
                self.subscriptions
                    .unsubscribe(shard_id, &track.id, &subscriber);
                self.defer_subscribe(DeferredSubscribe {
                    shard_id,
                    subscriber,
                    subscriber_key,
                    slot,
                    track,
                });
                return;
            };
            self.subscriptions.installed(shard_id, track.id, handle);
        }

        if !self.publish_track_plans(track.id).await {
            debug_assert!(false, "track plan publication must complete");
        }
    }

    /// The inverse. Only the last consumer on a shard retires its route, and
    /// the publisher is told to stop before the route leaves the view.
    pub(super) async fn on_track_unsubscribed(
        &mut self,
        shard_id: crate::id::ShardId,
        subscriber: ParticipantId,
        track: crate::track::TrackMeta,
    ) {
        let crate::control::subscriptions::InterestChange::Retire { route } = self
            .subscriptions
            .unsubscribe(shard_id, &track.id, &subscriber)
        else {
            let _ = self.publish_track_plans(track.id).await;
            return;
        };
        if !self.retire_video_route(shard_id, route, track.id).await {
            debug_assert!(false, "track route retirement must complete");
        }
    }

    pub(super) fn track_plan(
        &self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
    ) -> Option<(
        crate::shard::router::TrackKey,
        crate::view::TrackForwardingPlan,
    )> {
        let binding = self.track_bindings.get(&track_id)?;
        let fanout = if shard_id == binding.publisher_shard {
            binding.publisher_fanout
        } else {
            *binding.fanouts.get(&shard_id)?
        };
        let mut local_subscribers = Vec::new();
        let mut remote_routes = Vec::new();
        for (destination, route, subscribers) in self.subscriptions.plan_destinations(&track_id) {
            if destination == shard_id {
                local_subscribers.extend(subscribers);
            }
            if shard_id == binding.publisher_shard
                && destination != shard_id
                && let Some(route) = route
            {
                remote_routes.push(crate::view::RemoteRoutePlan {
                    shard_id: destination,
                    route: route.route,
                    epoch: route.epoch,
                });
            }
        }
        Some((
            fanout,
            crate::view::TrackForwardingPlan {
                track_id: binding.meta.id,
                origin: binding.meta.origin,
                local_subscribers,
                remote_routes,
                reverse_route: binding
                    .reverse_route
                    .map(|route| crate::view::RemoteRoutePlan {
                        shard_id: binding.publisher_shard,
                        route: route.route,
                        epoch: route.epoch,
                    }),
            },
        ))
    }

    pub(super) fn track_audience_on_shard(
        &self,
        origin: ParticipantId,
        shard: crate::id::ShardId,
    ) -> Vec<crate::shard::participants::ParticipantKey> {
        let Some(room_id) = self
            .core
            .registry
            .get_participant(&origin)
            .map(|meta| meta.room_id)
        else {
            debug_assert!(false, "a published track must have a room");
            return Vec::new();
        };
        self.core
            .registry
            .participants_in_room(&room_id)
            .into_iter()
            .filter(|(_, owner, key)| *owner == shard && key.is_some())
            .filter_map(|(_, _, key)| key)
            .collect()
    }

    pub(super) async fn publish_track_plans(&mut self, track_id: crate::entity::TrackId) -> bool {
        let Some(binding) = self.track_bindings.get(&track_id) else {
            return false;
        };
        let publisher_shard = binding.publisher_shard;
        let fanout_shards: Vec<_> = binding.fanouts.keys().copied().collect();
        let mut plans = Vec::new();
        for shard_id in fanout_shards {
            if let Some(plan) = self.track_plan(track_id, shard_id) {
                plans.push((shard_id, plan));
            }
        }
        if let Some(plan) = self.track_plan(track_id, publisher_shard) {
            plans.push((publisher_shard, plan));
        }
        if plans.is_empty() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return false;
        };
        for (shard_id, (fanout, plan)) in plans {
            let Some(binding) = self.track_bindings.get(&track_id) else {
                self.abort_transaction(now);
                return false;
            };
            let descriptor = crate::view::TrackDescriptor {
                id: binding.meta.id,
                origin_key: binding.publisher_participant,
                participant: (shard_id == binding.publisher_shard)
                    .then_some(binding.publisher_participant),
                encodings: binding.encodings.clone(),
                states: binding.states.clone(),
                publication: binding.publication.clone(),
                audience: self.track_audience_on_shard(binding.meta.origin, shard_id),
            };
            let Some(view) = self.view_mut(shard_id) else {
                self.abort_transaction(now);
                return false;
            };
            view.stage(
                generation,
                crate::view::ViewOp::InsertTrackRuntime {
                    key: fanout,
                    descriptor,
                },
            );
            view.stage(
                generation,
                crate::view::ViewOp::SetTrackPlan { key: fanout, plan },
            );
        }
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let shard_id = crate::id::ShardId::new(index);
            if let Some(view) = self.view_mut(shard_id)
                && view.publish().is_some()
            {
                affected.push(shard_id);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return false;
        }
        true
    }

    pub(super) async fn install_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        fanout: crate::shard::router::TrackKey,
        plan: crate::view::TrackForwardingPlan,
    ) -> Option<RouteHandle> {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }
        let Some(handle) = self.reserve_endpoint_retrying(shard_id, now, "video") else {
            self.abort_transaction(now);
            return None;
        };
        let generation = self.state.pending()?.generation;
        let Some(view) = self.view_mut(shard_id) else {
            self.abort_transaction(now);
            return None;
        };
        view.stage(
            generation,
            crate::view::ViewOp::InstallRoute {
                route: handle.route,
                binding: crate::view::RouteBinding {
                    epoch: handle.epoch,
                    action: crate::route::RouteAction::Video {
                        local_track: fanout,
                    },
                },
            },
        );
        view.stage(
            generation,
            crate::view::ViewOp::SetTrackPlan { key: fanout, plan },
        );
        let mut affected = Vec::new();
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let Some(view) = self.view_mut(target)
                && view.publish().is_some()
            {
                affected.push(target);
            }
        }
        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        Some(handle)
    }

    pub(super) async fn retire_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: RouteHandle,
        track_id: crate::entity::TrackId,
    ) -> bool {
        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            return false;
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            return false;
        };
        if let Some(view) = self.view_mut(shard_id) {
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: handle.route,
                    epoch: handle.epoch,
                },
            );
        }
        let mut affected = vec![shard_id];
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let Some(plan) = self.track_plan(track_id, target)
                && let Some(view) = self.view_mut(target)
            {
                view.stage(
                    generation,
                    crate::view::ViewOp::SetTrackPlan {
                        key: plan.0,
                        plan: plan.1,
                    },
                );
                if target != shard_id {
                    affected.push(target);
                }
            }
        }
        affected.sort_by_key(|shard| shard.index());
        affected.dedup();
        let mut published = Vec::new();
        for target in affected {
            if let Some(view) = self.view_mut(target)
                && view.publish().is_some()
            {
                published.push(target);
            }
        }
        if self.state.commit().is_err() {
            return false;
        }
        self.state
            .release_endpoint(shard_id, handle.route.slot(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::subscriptions::InterestChange;

    #[test]
    fn only_first_interest_installs_a_video_route() {
        assert!(installs_video_route(InterestChange::Install));
        assert!(!installs_video_route(InterestChange::None));
    }
}
