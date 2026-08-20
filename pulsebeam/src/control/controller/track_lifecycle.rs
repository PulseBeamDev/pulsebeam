use super::*;

impl ControllerActor {
    pub(super) async fn drain_pending_track_subscriptions(
        &mut self,
        track_id: crate::entity::TrackId,
    ) {
        let pending = self.pending.take_published(track_id);
        for subscription in pending {
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
        self.pending.remove(track_id, subscriber, slot);
    }

    pub(super) async fn install_video_runtimes(&mut self, track_id: crate::entity::TrackId) {
        let Some(binding) = self.catalog.get(&track_id) else {
            debug_assert!(false, "video runtime installation requires a track binding");
            return;
        };
        let publisher_shard = binding.publisher_shard;
        let origin = binding.publisher;
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
                .catalog
                .get(&track_id)
                .is_some_and(|binding| binding.destinations.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                debug_assert!(false, "a subscriber shard must accept a track runtime");
                continue;
            };
            let Some(binding) = self.catalog.get_mut(&track_id) else {
                debug_assert!(
                    false,
                    "track binding disappeared during runtime installation"
                );
                return;
            };
            binding.destinations.insert(
                destination,
                crate::control::publication::Destination {
                    key: crate::control::publication::RuntimeKey::Track(key),
                    route: None,
                },
            );
        }
    }

    pub(super) async fn retire_track_binding(&mut self, track_id: crate::entity::TrackId) -> bool {
        self.retire_publication(track_id, |actor, shard, key| {
            if let Some(key) = key.track() {
                actor.state.remove_track(shard, key);
            }
        })
        .await
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
        let installed = self
            .install_destinations(track_id, |actor, destination| {
                let publication = actor.catalog.get(&track_id)?;
                let (id, origin) = (publication.id, publication.publisher);
                let key = actor.prepare_track_key(destination, id, origin)?;
                Some((
                    crate::control::publication::RuntimeKey::Track(key),
                    RouteAction::Audio { track: key },
                ))
            })
            .await;
        let _ = installed;
        if !self.publish_publication(track_id).await {
            debug_assert!(false, "audio route installation must publish");
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
            let Some(binding) = self.catalog.get(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            if let Some(fanout) = binding
                .origin_key
                .track()
                .filter(|_| shard_id == binding.publisher_shard)
            {
                fanout
            } else if let Some(fanout) = binding
                .destinations
                .get(&shard_id)
                .and_then(|d| d.key.track())
            {
                fanout
            } else {
                let Some(fanout) = self.prepare_track_key(shard_id, track.id, track.origin) else {
                    debug_assert!(false, "a subscriber shard must accept a track runtime");
                    return;
                };
                let Some(binding) = self.catalog.get_mut(&track.id) else {
                    return;
                };
                binding.destinations.insert(
                    shard_id,
                    crate::control::publication::Destination {
                        key: crate::control::publication::RuntimeKey::Track(fanout),
                        route: None,
                    },
                );
                fanout
            }
        };
        let pattern =
            crate::control::patterns::Pattern::exact(track.room_id, track.origin, track.id);
        let (membership, mut membership_ops) = crate::control::patterns::declare_audience(
            &mut self.video_patterns,
            pattern.clone(),
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: slot,
            },
            crate::view::Delivery::Video(slot),
            crate::view::AudienceKind::Video,
        );
        membership_ops.push((
            shard_id,
            crate::view::ViewOp::BindSubscribedTrack {
                participant: subscriber_key,
                track: track.id,
                fanout,
            },
        ));
        if !self.publish_ops(membership_ops) {
            debug_assert!(false, "video subscription must publish");
        }

        if membership == crate::control::patterns::Membership::FirstOnShard {
            let Some(plan) = self.plan_for(track.id, shard_id) else {
                debug_assert!(false, "a first subscription must have a compiled plan");
                return;
            };
            let Some(handle) = self.install_video_route(shard_id, fanout, plan).await else {
                // The fanout key stays. Only the route failed, and the shard is
                // told which track a route carries by the route itself, so the
                // retry re-stages this same key — dropping it here would mint a
                // fresh one on every attempt and abandon the last in the arena.
                let (_, mut rollback) = crate::control::patterns::retract_audience(
                    &mut self.video_patterns,
                    &pattern,
                    &subscriber,
                    crate::view::AudienceKind::Video,
                );
                rollback.push((
                    shard_id,
                    crate::view::ViewOp::UnbindSubscribedTrack {
                        participant: subscriber_key,
                        track: track.id,
                        fanout,
                    },
                ));
                let _ = self.publish_ops(rollback);
                self.defer_subscribe(crate::control::pending::PendingSubscription::new(
                    shard_id,
                    subscriber,
                    subscriber_key,
                    slot,
                    track,
                ));
                return;
            };
            if let Some(binding) = self.catalog.get_mut(&track.id) {
                if let Some(destination) = binding.destinations.get_mut(&shard_id) {
                    destination.route = Some(handle);
                }
            }
        }

        if !self.publish_publication(track.id).await {
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
        let pattern =
            crate::control::patterns::Pattern::exact(track.room_id, track.origin, track.id);
        let subscriber_key = self
            .video_patterns
            .member_key(&pattern, &subscriber)
            .map(|(_, key)| key);
        let (departure, mut membership_ops) = crate::control::patterns::retract_audience(
            &mut self.video_patterns,
            &pattern,
            &subscriber,
            crate::view::AudienceKind::Video,
        );
        if let (Some(key), Some(fanout)) = (subscriber_key, self.track_fanout(track.id, shard_id)) {
            membership_ops.push((
                shard_id,
                crate::view::ViewOp::UnbindSubscribedTrack {
                    participant: key,
                    track: track.id,
                    fanout,
                },
            ));
        }
        if !self.publish_ops(membership_ops) {
            debug_assert!(false, "video unsubscription must publish");
        }
        if departure != crate::control::patterns::Departure::LastOnShard {
            let _ = self.publish_publication(track.id).await;
            return;
        }
        let Some(route) = self.catalog.get_mut(&track.id).and_then(|binding| {
            binding
                .destinations
                .get_mut(&shard_id)
                .and_then(|d| d.route.take())
        }) else {
            let _ = self.publish_publication(track.id).await;
            return;
        };
        if !self.retire_video_route(shard_id, route, track.id).await {
            debug_assert!(false, "track route retirement must complete");
        }
    }

    fn track_fanout(
        &self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
    ) -> Option<crate::shard::router::TrackKey> {
        let binding = self.catalog.get(&track_id)?;
        if shard_id == binding.publisher_shard {
            binding.origin_key.track()
        } else {
            binding
                .destinations
                .get(&shard_id)
                .and_then(|d| d.key.track())
        }
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

    /// What a shard needs to hold a runtime entry for this track.
    ///
    /// Identical for video and audio: the fanout key differs, the track behind
    /// it does not.
    pub(super) fn track_descriptor(
        &self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
    ) -> Option<crate::view::TrackDescriptor> {
        let binding = self.catalog.get(&track_id)?;
        // Only video carries encodings and layer states; audio's descriptor is
        // the same shape with none of them.
        let (encodings, states, publication) = match &binding.media {
            crate::control::publication::Media::Video {
                publication,
                encodings,
                states,
            } => (encodings.clone(), states.clone(), publication.clone()),
            _ => (
                Vec::new(),
                crate::track::TrackStates::default(),
                crate::track::Track {
                    meta: binding.meta(),
                    layers: Vec::new(),
                    reverse: binding.reverse_route,
                },
            ),
        };
        Some(crate::view::TrackDescriptor {
            id: binding.id,
            origin_key: binding.publisher_key,
            participant: (shard_id == binding.publisher_shard).then_some(binding.publisher_key),
            encodings,
            states,
            publication,
            audience: self.track_audience_on_shard(binding.publisher, shard_id),
        })
    }

    pub(super) async fn install_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        fanout: crate::shard::router::TrackKey,
        plan: crate::view::VideoPlan,
    ) -> Option<RouteHandle> {
        self.publish_with_route(shard_id, "video", move |_, handle| {
            vec![
                (
                    shard_id,
                    crate::view::ViewOp::InstallRoute {
                        route: handle.route,
                        binding: crate::view::RouteBinding {
                            epoch: handle.epoch,
                            action: crate::route::RouteAction::Video {
                                local_track: fanout,
                            },
                        },
                    },
                ),
                (
                    shard_id,
                    crate::view::ViewOp::SetPlan {
                        target: crate::view::PlanTarget::Video(fanout),
                        plan,
                    },
                ),
            ]
        })
    }

    /// Take a shard's video route out of the view, then return its slot.
    ///
    /// Every other shard's plan is restaged in the same generation, so no shard
    /// keeps forwarding to a route that is no longer there.
    pub(super) async fn retire_video_route(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: RouteHandle,
        track_id: crate::entity::TrackId,
    ) -> bool {
        let mut ops = vec![(
            shard_id,
            crate::view::ViewOp::RetireRoute {
                route: handle.route,
                epoch: handle.epoch,
            },
        )];
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let (Some(key), Some(plan)) = (
                self.track_fanout(track_id, target),
                self.plan_for(track_id, target),
            ) {
                ops.push((
                    target,
                    crate::view::ViewOp::SetPlan {
                        target: crate::view::PlanTarget::Video(key),
                        plan,
                    },
                ));
            }
        }
        if !self.publish_ops(ops) {
            return false;
        }
        self.state
            .release_endpoint(shard_id, handle.route.slot(), tokio::time::Instant::now());
        true
    }
}
