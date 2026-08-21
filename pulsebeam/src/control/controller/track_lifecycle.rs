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

    pub(super) async fn install_video_runtime(
        &mut self,
        track_id: crate::entity::TrackId,
        destination: crate::id::ShardId,
    ) {
        let Some(binding) = self.catalog.get(&track_id) else {
            debug_assert!(false, "video runtime installation requires a track binding");
            return;
        };
        if destination == binding.publisher_shard || binding.destinations.contains_key(&destination)
        {
            return;
        }
        let origin = binding.publisher;
        let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
            debug_assert!(false, "a subscriber shard must accept a track runtime");
            return;
        };
        let Some(binding) = self.catalog.get_mut(&track_id) else {
            debug_assert!(
                false,
                "track binding disappeared during runtime installation"
            );
            self.state.remove_track(destination, key);
            return;
        };
        binding.destinations.insert(
            destination,
            crate::control::publication::Destination::Discovery {
                key: crate::keys::VideoTrackKey::new(key),
            },
        );
    }

    pub(super) async fn install_video_runtimes_for_room(
        &mut self,
        track_id: crate::entity::TrackId,
        room_id: crate::entity::RoomId,
    ) {
        let publisher_shard = self.catalog.get(&track_id).map(|p| p.publisher_shard);
        let Some(publisher_shard) = publisher_shard else {
            debug_assert!(false, "a video runtime installation requires a publication");
            return;
        };
        let destinations: Vec<_> = self
            .core
            .registry
            .shards_in_room(&room_id)
            .filter(|shard| *shard != publisher_shard)
            .collect();
        for destination in destinations {
            self.install_video_runtime(track_id, destination).await;
        }
    }

    pub(super) async fn retire_track_binding(&mut self, track_id: crate::entity::TrackId) -> bool {
        self.retire_publication(track_id).await
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
        if track.meta.id.kind() == crate::entity::TrackKind::Data {
            debug_assert!(false, "data does not publish through the track path");
            return None;
        }
        if track.meta.id.kind() == crate::entity::TrackKind::Video {
            let handle = self
                .grant_route(
                    shard_id,
                    crate::route::RouteAction::Reverse {
                        target: crate::route::ReverseTarget::Track {
                            track: crate::keys::VideoTrackKey::new(fanout),
                        },
                    },
                )
                .await?;
            track.reverse = Some(handle);
        } else {
            track.reverse = None;
        }
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
        let complete = self
            .install_destinations(track_id, |actor, destination| {
                actor.mint_audio_destination(track_id, destination)
            })
            .await;
        if !complete {
            self.defer_audio(track_id);
        }
        if !self.publish_publication(track_id).await {
            debug_assert!(false, "audio route installation must publish");
        }
    }

    fn mint_audio_destination(
        &mut self,
        track_id: crate::entity::TrackId,
        destination: crate::id::ShardId,
    ) -> Option<(
        crate::control::publication::RuntimeKey,
        crate::route::RouteAction,
    )> {
        let publication = self.catalog.get(&track_id)?;
        let (id, origin) = (publication.id, publication.publisher);
        let key = self.prepare_track_key(destination, id, origin)?;
        Some((
            crate::control::publication::RuntimeKey::Audio(crate::keys::AudioTrackKey::new(key)),
            RouteAction::Audio {
                track: crate::keys::AudioTrackKey::new(key),
            },
        ))
    }

    pub(super) async fn install_audio_destination(
        &mut self,
        track_id: crate::entity::TrackId,
        destination: crate::id::ShardId,
    ) -> bool {
        self.install_destination(track_id, destination, &|actor, destination| {
            actor.mint_audio_destination(track_id, destination)
        })
        .await
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
        let pattern =
            crate::control::patterns::Pattern::exact(track.room_id, track.origin, track.id);
        let previous_member = self.video_patterns.member_key(&pattern, &subscriber);
        let (fanout, new_destination) = {
            let Some(binding) = self.catalog.get(&track.id) else {
                debug_assert!(false, "a subscription must name a published track");
                return;
            };
            if let Some(fanout) = binding
                .origin_key
                .track()
                .filter(|_| shard_id == binding.publisher_shard)
            {
                (fanout, false)
            } else if let Some(destination) = binding.destinations.get(&shard_id) {
                let (fanout, new_destination) = match destination {
                    crate::control::publication::Destination::Discovery { key } => {
                        (key.raw(), true)
                    }
                    crate::control::publication::Destination::Forwarding {
                        key: crate::control::publication::RuntimeKey::Video(key),
                        ..
                    } => (key.raw(), false),
                    crate::control::publication::Destination::Forwarding { .. } => {
                        debug_assert!(false, "a video destination must carry a video key");
                        return;
                    }
                };
                (fanout, new_destination)
            } else {
                let Some(fanout) = self.prepare_track_key(shard_id, track.id, track.origin) else {
                    debug_assert!(false, "a subscriber shard must accept a track runtime");
                    return;
                };
                let Some(binding) = self.catalog.get_mut(&track.id) else {
                    self.state.remove_track(shard_id, fanout);
                    return;
                };
                binding.destinations.insert(
                    shard_id,
                    crate::control::publication::Destination::Discovery {
                        key: crate::keys::VideoTrackKey::new(fanout),
                    },
                );
                (fanout, true)
            }
        };
        let (membership, displaced) = crate::control::patterns::declare_audience(
            &mut self.video_patterns,
            pattern.clone(),
            subscriber,
            crate::control::patterns::Member {
                shard: shard_id,
                key: subscriber_key,
                delivery: slot,
            },
        );
        self.index_publication(track.id);
        let membership_ops = vec![(
            shard_id,
            crate::view::ViewOp::BindSubscribedTrack {
                participant: subscriber_key,
                track: track.id,
                fanout,
            },
        )];
        self.publish_ops(membership_ops);

        let needs_remote_route = self
            .catalog
            .get(&track.id)
            .is_some_and(|binding| binding.publisher_shard != shard_id);
        let mut route_installed = false;
        if membership == crate::control::patterns::Membership::FirstOnShard
            && needs_remote_route
            && new_destination
        {
            let Some(plan) = self.video_plan_for(track.id, shard_id) else {
                pulsebeam_runtime::fatal!("a first subscription must have a compiled video plan");
            };
            let Some(handle) = self
                .install_video_route(track.id, shard_id, fanout, plan)
                .await
            else {
                let _ = crate::control::patterns::retract_audience(
                    &mut self.video_patterns,
                    &pattern,
                    &subscriber,
                );
                let rollback = vec![(
                    shard_id,
                    crate::view::ViewOp::UnbindSubscribedTrack {
                        participant: subscriber_key,
                        track: track.id,
                        fanout,
                    },
                )];
                self.publish_ops(rollback);
                if new_destination {
                    if let Some(binding) = self.catalog.get_mut(&track.id) {
                        binding.destinations.shift_remove(&shard_id);
                    }
                    self.state.remove_track(shard_id, fanout);
                }
                self.defer_subscribe(crate::control::pending::PendingSubscription::new(
                    shard_id,
                    subscriber,
                    subscriber_key,
                    slot,
                    track,
                ));
                return;
            };
            let Some(binding) = self.catalog.get_mut(&track.id) else {
                pulsebeam_runtime::fatal!("a video route must belong to its publication");
            };
            let Some(destination) = binding.destinations.get_mut(&shard_id) else {
                pulsebeam_runtime::fatal!("a video route must have a destination binding");
            };
            let crate::control::publication::Destination::Discovery { key } = *destination else {
                pulsebeam_runtime::fatal!("a first video route must start from discovery");
            };
            *destination = crate::control::publication::Destination::Forwarding {
                key: crate::control::publication::RuntimeKey::Video(key),
                route: handle,
            };
            route_installed = true;
        }

        if route_installed || membership != crate::control::patterns::Membership::Unchanged {
            let Some(publisher_shard) = self.catalog.get(&track.id).map(|p| p.publisher_shard)
            else {
                pulsebeam_runtime::fatal!("a subscribed video must belong to a publication");
            };
            let mut plans_ready = self.publish_plan_to(track.id, shard_id);
            if !plans_ready {
                debug_assert!(false, "a changed video audience must publish its plan");
            }
            if publisher_shard != shard_id && !self.publish_plan_to(track.id, publisher_shard) {
                debug_assert!(false, "a changed video audience must publish its plan");
                plans_ready = false;
            }
            if !plans_ready {
                return;
            }
        }
        for (_, departure, previous_shard, _) in displaced {
            if previous_shard == shard_id {
                continue;
            }
            if departure == crate::control::patterns::Departure::LastOnShard {
                let Some(route) = self.catalog.get(&track.id).and_then(|binding| {
                    binding
                        .destinations
                        .get(&previous_shard)
                        .and_then(|destination| match destination {
                            crate::control::publication::Destination::Forwarding {
                                route, ..
                            } => Some(*route),
                            crate::control::publication::Destination::Discovery { .. } => None,
                        })
                }) else {
                    continue;
                };
                if !self
                    .retire_video_route(previous_shard, route, track.id)
                    .await
                {
                    debug_assert!(false, "a displaced video route must retire");
                    return;
                }
            } else if !self.publish_plan_to(track.id, previous_shard) {
                debug_assert!(false, "a displaced video audience must publish its plan");
                return;
            }
        }
        if let Some((previous_shard, previous_key)) = previous_member
            && (previous_shard != shard_id || previous_key != subscriber_key)
        {
            let Some(previous_fanout) = self.track_fanout(track.id, previous_shard) else {
                pulsebeam_runtime::fatal!("a previous video subscription must have a fanout");
            };
            self.publish_ops(vec![(
                previous_shard,
                crate::view::ViewOp::UnbindSubscribedTrack {
                    participant: previous_key,
                    track: track.id,
                    fanout: previous_fanout,
                },
            )]);
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
        let departure = crate::control::patterns::retract_audience(
            &mut self.video_patterns,
            &pattern,
            &subscriber,
        );
        let unbind = subscriber_key
            .zip(self.track_fanout(track.id, shard_id))
            .map(|(key, fanout)| {
                (
                    shard_id,
                    crate::view::ViewOp::UnbindSubscribedTrack {
                        participant: key,
                        track: track.id,
                        fanout,
                    },
                )
            });
        if departure != crate::control::patterns::Departure::LastOnShard {
            if !self.publish_publication(track.id).await {
                debug_assert!(false, "a changed video audience must publish its plan");
                return;
            }
            if let Some(unbind) = unbind {
                self.publish_ops(vec![unbind]);
            }
            return;
        }
        let Some(route) = self.catalog.get(&track.id).and_then(|binding| {
            binding
                .destinations
                .get(&shard_id)
                .and_then(|destination| match destination {
                    crate::control::publication::Destination::Forwarding { route, .. } => {
                        Some(*route)
                    }
                    crate::control::publication::Destination::Discovery { .. } => None,
                })
        }) else {
            if !self.publish_publication(track.id).await {
                debug_assert!(false, "a changed video audience must publish its plan");
                return;
            }
            if let Some(unbind) = unbind {
                self.publish_ops(vec![unbind]);
            }
            return;
        };
        if !self.retire_video_route(shard_id, route, track.id).await {
            debug_assert!(false, "track route retirement must complete");
            return;
        }
        if let Some(unbind) = unbind {
            self.publish_ops(vec![unbind]);
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
                .and_then(|d| d.key().track())
        }
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
        })
    }

    pub(super) async fn install_video_route(
        &mut self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
        fanout: crate::shard::router::TrackKey,
        plan: crate::plan::FlatTrackPlan,
    ) -> Option<RouteHandle> {
        let descriptor = self.track_descriptor(track_id, shard_id)?;
        self.publish_with_route(shard_id, "video", move |_, handle| {
            vec![
                (
                    shard_id,
                    crate::view::ViewOp::InsertTrackRuntime {
                        key: crate::keys::TrackRuntimeKey::Video(crate::keys::VideoTrackKey::new(
                            fanout,
                        )),
                        descriptor,
                    },
                ),
                (
                    shard_id,
                    crate::view::ViewOp::InstallRoute {
                        binding: crate::view::RouteBinding {
                            handle: *handle,
                            action: crate::route::RouteAction::Video {
                                local_track: crate::keys::VideoTrackKey::new(fanout),
                            },
                        },
                    },
                ),
                (
                    shard_id,
                    crate::view::ViewOp::SetPlan {
                        key: crate::plan::PlanKey::Track(fanout),
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
        let Some(destination) = self
            .catalog
            .get_mut(&track_id)
            .and_then(|binding| binding.destinations.get_mut(&shard_id))
        else {
            debug_assert!(false, "a video route must have a destination binding");
            return false;
        };
        let crate::control::publication::Destination::Forwarding {
            key: crate::control::publication::RuntimeKey::Video(key),
            route: current,
        } = *destination
        else {
            debug_assert!(false, "a video route must be forwarding");
            return false;
        };
        debug_assert_eq!(current, handle);
        *destination = crate::control::publication::Destination::Discovery { key };
        let mut ops = vec![(shard_id, crate::view::ViewOp::RetireRoute { handle })];
        for index in 0..self.views.len() {
            let target = crate::id::ShardId::new(index);
            if let (Some(key), Some(plan)) = (
                self.track_fanout(track_id, target),
                self.video_plan_for(track_id, target),
            ) {
                ops.push((
                    target,
                    crate::view::ViewOp::SetPlan {
                        key: crate::plan::PlanKey::Track(key),
                        plan,
                    },
                ));
            }
        }
        self.publish_ops(ops);
        self.state
            .release_endpoint(shard_id, handle.route.slot(), tokio::time::Instant::now());
        true
    }
}
