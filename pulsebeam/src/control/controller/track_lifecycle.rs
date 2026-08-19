use super::*;

/// Which image a publication's plan belongs to. Video and audio share an arena
/// and a key type, so the kind is what tells them apart — the one thing about a
/// media kind the routing layer still has to know.
fn plan_target(
    kind: crate::entity::TrackKind,
    key: crate::control::publication::RuntimeKey,
) -> crate::view::PlanTarget {
    use crate::control::publication::RuntimeKey;
    match (kind, key) {
        (crate::entity::TrackKind::Audio, RuntimeKey::Track(key)) => {
            crate::view::PlanTarget::Audio(key)
        }
        (_, RuntimeKey::Track(key)) => crate::view::PlanTarget::Video(key),
        (_, RuntimeKey::Unreliable(key)) => crate::view::PlanTarget::Unreliable(key),
        (_, RuntimeKey::Reliable(key)) => crate::view::PlanTarget::Reliable(key),
    }
}

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

    /// Stage the retirement of one kind's destination routes for a track.
    ///
    /// Video and audio differ only in which fanout map names the destination's
    /// key and which image the plan lives in, so the walk is written once.
    fn stage_destination_retirement(
        &mut self,
        generation: u64,
        now: tokio::time::Instant,
        kind: crate::entity::TrackKind,
        destinations: &indexmap::IndexMap<
            crate::id::ShardId,
            crate::control::publication::Destination,
        >,
        releases: &mut Vec<(crate::id::ShardId, RouteHandle)>,
    ) -> bool {
        for (destination, held) in destinations {
            let Some(view) = self.view_mut(*destination) else {
                debug_assert!(false, "a track route must name a local view");
                self.abort_transaction(now);
                return false;
            };
            if let Some(route) = held.route {
                view.stage(
                    generation,
                    crate::view::ViewOp::RetireRoute {
                        route: route.route,
                        epoch: route.epoch,
                    },
                );
                releases.push((*destination, route));
            }
            if let Some(key) = held.key.track() {
                view.stage(
                    generation,
                    crate::view::ViewOp::RemovePlan {
                        target: plan_target(kind, held.key),
                    },
                );
                view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
            }
        }
        true
    }

    pub(super) async fn retire_track_binding(&mut self, track_id: crate::entity::TrackId) -> bool {
        let Some(binding) = self.catalog.get(&track_id) else {
            return true;
        };
        // Its subscribers never unsubscribe from a track that goes away, so the
        // declarations naming it have to be retired with it.
        let retired_pattern =
            crate::control::patterns::Pattern::exact(binding.room, binding.publisher, track_id);
        let destinations = binding.destinations.clone();
        let publisher_shard = binding.publisher_shard;
        let Some(publisher_fanout) = binding.origin_key.track() else {
            debug_assert!(false, "a media publication holds a media key");
            return false;
        };
        let reverse_route = binding.reverse_route;
        let kind = track_id.kind();
        if let Some((group, members)) = self.video_patterns.retire_pattern(&retired_pattern) {
            let ops = members
                .into_iter()
                .map(|(_, shard, key)| {
                    (
                        shard,
                        crate::view::ViewOp::GroupRemove {
                            group,
                            key,
                            kind: crate::view::AudienceKind::Video,
                        },
                    )
                })
                .collect();
            if !self.publish_ops(ops) {
                debug_assert!(false, "video group retirement must publish");
            }
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

        let mut endpoint_releases = Vec::new();
        if !self.stage_destination_retirement(
            generation,
            now,
            kind,
            &destinations,
            &mut endpoint_releases,
        ) {
            return false;
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
            crate::view::ViewOp::RemovePlan {
                target: crate::view::PlanTarget::Video(publisher_fanout),
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
            crate::view::ViewOp::RemovePlan {
                target: crate::view::PlanTarget::Audio(publisher_fanout),
            },
        );
        for (&destination, held) in &destinations {
            if destination == publisher_shard {
                continue;
            }
            if let Some(view) = self.view_mut(destination) {
                view.stage(
                    generation,
                    crate::view::ViewOp::RemovePlan {
                        target: plan_target(kind, held.key),
                    },
                );
                if let Some(key) = held.key.track() {
                    view.stage(generation, crate::view::ViewOp::RemoveTrackRuntime { key });
                }
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
        for (&destination, held) in &destinations {
            if let Some(key) = held.key.track() {
                self.state.remove_track(destination, key);
            }
        }
        self.catalog.remove(&track_id);
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
        let Some(binding) = self.catalog.get(&track_id) else {
            return;
        };
        let origin = binding.publisher;
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
        let subject = crate::control::patterns::Subject {
            room: room_id,
            publisher: origin,
            name: track_id,
        };
        // The plan names its audiences rather than listing them. Membership
        // lives on the shard, so a participant joining the room is one insert
        // there instead of a rewrite of every audio plan in it.
        let groups = self.audio_patterns.match_subject(&subject);
        let mut destinations: Vec<crate::id::ShardId> = Vec::new();
        for group in &groups {
            for shard in self.audio_patterns.shards_of(*group) {
                if !destinations.contains(&shard) {
                    destinations.push(shard);
                }
            }
        }
        for destination in destinations {
            if destination == publisher_shard {
                continue;
            }
            if self
                .catalog
                .get(&track_id)
                .is_some_and(|binding| binding.destinations.contains_key(&destination))
            {
                continue;
            }
            let Some(key) = self.prepare_track_key(destination, track_id, origin) else {
                continue;
            };
            let plan = crate::view::AudioPlan {
                groups: groups.clone(),
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
            let Some(binding) = self.catalog.get_mut(&track_id) else {
                return;
            };
            binding.destinations.insert(
                destination,
                crate::control::publication::Destination {
                    key: crate::control::publication::RuntimeKey::Track(key),
                    route: None,
                },
            );
            if let Some(destination) = binding.destinations.get_mut(&destination) {
                destination.route = Some(route);
            }
        }
        if !self.publish_track_plans(track_id).await {
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
            let Some((_, plan)) = self.track_plan(track.id, shard_id) else {
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
            let _ = self.publish_track_plans(track.id).await;
            return;
        }
        let Some(route) = self.catalog.get_mut(&track.id).and_then(|binding| {
            binding
                .destinations
                .get_mut(&shard_id)
                .and_then(|d| d.route.take())
        }) else {
            let _ = self.publish_track_plans(track.id).await;
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

    /// The audiences a publication reaches.
    ///
    /// The tables are per kind because the delivery key is - a video slot, an
    /// audio slot chosen per packet, an SCTP channel - and the data table is
    /// keyed by topic because a data declaration names one across publishers.
    /// Selecting the table is the whole of what differs; a match yields the
    /// same group ids either way.
    pub(super) fn groups_of(
        &self,
        publication: &crate::control::publication::Publication,
    ) -> arrayvec::ArrayVec<crate::view::GroupId, 4> {
        use crate::control::publication::Media;
        match &publication.media {
            Media::Audio => self
                .audio_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: publication.id,
                }),
            Media::Video { .. } => {
                self.video_patterns
                    .match_subject(&crate::control::patterns::Subject {
                        room: publication.room,
                        publisher: publication.publisher,
                        name: publication.id,
                    })
            }
            Media::Data { lane, topic } => {
                let lane = match lane {
                    crate::track::DataLane::Realtime => {
                        crate::control::lanes::StreamLane::Unreliable
                    }
                    crate::track::DataLane::Reliable => crate::control::lanes::StreamLane::Reliable,
                };
                self.data_patterns
                    .match_subject(&crate::control::patterns::Subject {
                        room: publication.room,
                        publisher: publication.publisher,
                        name: (topic.clone(), lane),
                    })
            }
        }
    }

    /// One publication's plan for one shard, whatever kind it is.
    pub(super) fn plan_for(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> Option<crate::view::ForwardingPlan> {
        let publication = self.catalog.get(&id)?;
        Some(crate::control::publication::forwarding_plan(
            &publication.destinations,
            publication.publisher_shard,
            publication.reverse_route,
            self.groups_of(publication),
            shard,
        ))
    }

    pub(super) fn track_plan(
        &self,
        track_id: crate::entity::TrackId,
        shard_id: crate::id::ShardId,
    ) -> Option<(crate::shard::router::TrackKey, crate::view::VideoPlan)> {
        Some((
            self.track_fanout(track_id, shard_id)?,
            self.plan_for(track_id, shard_id)?,
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

    /// What a shard needs to hold a runtime entry for this track.
    ///
    /// Identical for video and audio: the fanout key differs, the track behind
    /// it does not.
    fn track_descriptor(
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

    pub(super) async fn publish_track_plans(&mut self, track_id: crate::entity::TrackId) -> bool {
        let Some(binding) = self.catalog.get(&track_id) else {
            return false;
        };
        let mut shards: Vec<_> = binding.destinations.keys().copied().collect();
        shards.push(binding.publisher_shard);
        let mut ops = Vec::new();
        for shard_id in shards {
            let Some((fanout, plan)) = self.track_plan(track_id, shard_id) else {
                continue;
            };
            let Some(descriptor) = self.track_descriptor(track_id, shard_id) else {
                continue;
            };
            ops.push((
                shard_id,
                crate::view::ViewOp::InsertTrackRuntime {
                    key: fanout,
                    descriptor,
                },
            ));
            ops.push((
                shard_id,
                crate::view::ViewOp::SetPlan {
                    target: plan_target(
                        track_id.kind(),
                        crate::control::publication::RuntimeKey::Track(fanout),
                    ),
                    plan,
                },
            ));
        }
        self.publish_ops(ops)
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
            if let Some((key, plan)) = self.track_plan(track_id, target) {
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
