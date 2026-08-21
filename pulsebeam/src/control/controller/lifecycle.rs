//! One publication lifecycle, whatever kind the publication is.
//!
//! Announce, install destinations, plan, publish, retire. Video, audio and the
//! two data lanes ran their own copy of each of these; what is left of the
//! difference between them is passed in - which arena a destination key comes
//! from, which route action reaches it - and everything else is written once.

use super::*;

pub(super) fn plan_removal(key: crate::control::publication::RuntimeKey) -> crate::plan::PlanKey {
    use crate::control::publication::RuntimeKey;
    match key {
        RuntimeKey::Video(key) => crate::plan::PlanKey::Track(key.raw()),
        RuntimeKey::Audio(key) => crate::plan::PlanKey::Track(key.raw()),
        RuntimeKey::Unreliable(key) => crate::plan::PlanKey::Unreliable(key),
        RuntimeKey::Reliable(key) => crate::plan::PlanKey::Reliable(key),
    }
}

/// The op that removes a publication's runtime from the arena its key names.
pub(super) fn runtime_removal_op(
    key: crate::control::publication::RuntimeKey,
) -> crate::view::ViewOp {
    use crate::control::publication::RuntimeKey;
    match key {
        RuntimeKey::Video(key) => crate::view::ViewOp::RemoveTrackRuntime {
            key: crate::keys::TrackRuntimeKey::Video(key),
        },
        RuntimeKey::Audio(key) => crate::view::ViewOp::RemoveTrackRuntime {
            key: crate::keys::TrackRuntimeKey::Audio(key),
        },
        RuntimeKey::Unreliable(key) => crate::view::ViewOp::RemoveUnreliableRuntime { key },
        RuntimeKey::Reliable(key) => crate::view::ViewOp::RemoveReliableRuntime { key },
    }
}

pub(super) fn remove_runtime_key(
    state: &mut crate::control::state::ControlPlaneState,
    shard: crate::id::ShardId,
    key: crate::control::publication::RuntimeKey,
) {
    match key {
        crate::control::publication::RuntimeKey::Video(key) => {
            state.remove_track(shard, key.raw());
        }
        crate::control::publication::RuntimeKey::Audio(key) => {
            state.remove_track(shard, key.raw());
        }
        crate::control::publication::RuntimeKey::Unreliable(key) => {
            state.remove_data(shard, key);
        }
        crate::control::publication::RuntimeKey::Reliable(key) => {
            state.remove_reliable(shard, key);
        }
    }
}

impl ControllerActor {
    pub(super) fn index_publication(&mut self, id: crate::entity::TrackId) {
        let Some(publication) = self.catalog.get(&id) else {
            debug_assert!(false, "an indexed publication must exist in the catalog");
            return;
        };
        let room = publication.room;
        let publisher = publication.publisher;
        match &publication.media {
            crate::control::publication::Media::Video { .. } => {
                self.video_patterns.attach_publication(
                    &crate::control::patterns::Subject {
                        room,
                        publisher,
                        name: id,
                    },
                    id,
                );
            }
            crate::control::publication::Media::Audio => {
                self.audio_patterns.attach_publication(
                    &crate::control::patterns::Subject {
                        room,
                        publisher,
                        name: id,
                    },
                    id,
                );
            }
            crate::control::publication::Media::Data { lane, topic } => {
                self.data_patterns.attach_publication(
                    &crate::control::patterns::Subject {
                        room,
                        publisher,
                        name: (topic.clone(), (*lane).into()),
                    },
                    id,
                );
            }
        }
    }

    pub(super) fn unindex_publication(&mut self, id: crate::entity::TrackId) {
        let Some(publication) = self.catalog.get(&id) else {
            return;
        };
        let room = publication.room;
        let publisher = publication.publisher;
        match &publication.media {
            crate::control::publication::Media::Video { .. } => {
                self.video_patterns.detach_publication(
                    &crate::control::patterns::Subject {
                        room,
                        publisher,
                        name: id,
                    },
                    id,
                );
            }
            crate::control::publication::Media::Audio => self.audio_patterns.detach_publication(
                &crate::control::patterns::Subject {
                    room,
                    publisher,
                    name: id,
                },
                id,
            ),
            crate::control::publication::Media::Data { lane, topic } => {
                self.data_patterns.detach_publication(
                    &crate::control::patterns::Subject {
                        room,
                        publisher,
                        name: (topic.clone(), (*lane).into()),
                    },
                    id,
                );
            }
        }
    }

    /// Retire a publication of any kind: its declarations, its routes, its
    /// plans and its runtimes.
    ///
    /// Staged as one transaction so a route never outlives the runtime it
    /// resolves to.
    pub(super) async fn retire_publication(&mut self, id: crate::entity::TrackId) -> bool {
        self.pending_audio.retain(|pending| *pending != id);
        let Some(publication) = self.catalog.get(&id) else {
            return true;
        };
        let kind = publication.kind();
        let publisher_shard = publication.publisher_shard;
        let origin_key = publication.origin_key;
        let publisher_key = publication.publisher_key;
        let reverse_route = publication.reverse_route;
        let destinations = publication.destinations.clone();
        let room = publication.room;
        let publisher = publication.publisher;
        self.unindex_publication(id);
        let retired_pattern = crate::control::patterns::Pattern::exact(room, publisher, id);

        // A video track's subscribers named it directly and never unsubscribe
        // from something that goes away, so their declarations go with it.
        if kind == crate::entity::TrackKind::Video {
            let _ = self.video_patterns.retire_pattern(&retired_pattern);
        }

        let mut releases = Vec::new();
        let mut generation_ops = super::GenerationOps::lifecycle(Vec::new());
        self.stage_destination_retirement(
            id,
            room,
            &destinations,
            &mut releases,
            &mut generation_ops,
        );
        if let Some(route) = reverse_route {
            generation_ops.lifecycle.push((
                publisher_shard,
                crate::view::ViewOp::RetireRoute { handle: route },
            ));
            releases.push((publisher_shard, route));
        }
        generation_ops = generation_ops.remove_plan(publisher_shard, plan_removal(origin_key));
        if let Some(track_key) = origin_key.track() {
            generation_ops = generation_ops.participant_effect(
                publisher_shard,
                publisher_key,
                crate::participant::ParticipantEffect::TrackRemoved {
                    key: track_key,
                    role: crate::participant::TrackRole::Published,
                    kind,
                },
            );
        }
        generation_ops
            .lifecycle
            .push((publisher_shard, runtime_removal_op(origin_key)));
        self.publish_generation(generation_ops);
        let now = tokio::time::Instant::now();
        for (shard, route) in releases {
            self.state.release_endpoint(shard, route.route.slot(), now);
        }
        remove_runtime_key(&mut self.state, publisher_shard, origin_key);
        for (&destination, held) in &destinations {
            remove_runtime_key(&mut self.state, destination, held.key());
        }
        self.catalog.remove(&id);
        true
    }

    /// Publish one publication's compiled view to every shard that holds it.
    ///
    /// The same walk for every kind - the publisher's own shard plus each
    /// destination, a runtime and a plan for each. What differs is which
    /// runtime op names the arena, and that a data destination carries its
    /// route install here rather than at grant time.
    pub(super) async fn publish_publication(&mut self, id: crate::entity::TrackId) -> bool {
        let Some(publication) = self.catalog.get(&id) else {
            return false;
        };
        let publisher_shard = publication.publisher_shard;
        let mut targets = vec![(publisher_shard, publication.origin_key, None)];
        for (shard, held) in &publication.destinations {
            let (key, route) = match held {
                crate::control::publication::Destination::Discovery { key } => {
                    (crate::control::publication::RuntimeKey::Video(*key), None)
                }
                crate::control::publication::Destination::Forwarding { key, route } => {
                    (*key, Some(*route))
                }
            };
            targets.push((*shard, key, route));
        }

        let mut ops = super::GenerationOps::lifecycle(Vec::new());
        for (shard, key, route) in targets {
            let Some(target_ops) = self.publication_ops(id, shard, key, route) else {
                return false;
            };
            ops.extend(target_ops);
        }
        self.publish_generation(ops);
        true
    }

    pub(super) async fn publish_publication_to(
        &mut self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> bool {
        let Some((key, route)) = self.catalog.get(&id).and_then(|publication| {
            if shard == publication.publisher_shard {
                Some((publication.origin_key, None))
            } else {
                publication.destinations.get(&shard).map(|destination| {
                    let key = destination.key();
                    let route = match destination {
                        crate::control::publication::Destination::Discovery { .. } => None,
                        crate::control::publication::Destination::Forwarding { route, .. } => {
                            Some(*route)
                        }
                    };
                    (key, route)
                })
            }
        }) else {
            return false;
        };
        let Some(ops) = self.publication_ops(id, shard, key, route) else {
            return false;
        };
        self.publish_generation(ops);
        true
    }

    pub(super) fn publish_plan_to(
        &mut self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> bool {
        let Some(key) = self.catalog.get(&id).and_then(|publication| {
            if shard == publication.publisher_shard {
                Some(publication.origin_key)
            } else {
                publication
                    .destinations
                    .get(&shard)
                    .map(|destination| destination.key())
            }
        }) else {
            return false;
        };
        let Some((plan_key, plan)) = self.plan_for(id, shard, key) else {
            return false;
        };
        let mut ops = super::GenerationOps::lifecycle(Vec::new()).plan(shard, plan_key, plan);
        if let Some(publication) = self.catalog.get(&id) {
            ops.extend_lifecycle(self.data_binding_ops(publication, shard, key));
        }
        self.publish_generation(ops);
        true
    }

    fn data_binding_ops(
        &self,
        publication: &crate::control::publication::Publication,
        shard: crate::id::ShardId,
        key: crate::control::publication::RuntimeKey,
    ) -> Vec<(crate::id::ShardId, crate::view::ViewOp)> {
        let crate::control::publication::Media::Data { lane, topic } = &publication.media else {
            return Vec::new();
        };
        let subject = crate::control::patterns::Subject {
            room: publication.room,
            publisher: publication.publisher,
            name: (topic.clone(), (*lane).into()),
        };
        let members = self.data_patterns.members_for(
            self.data_patterns.match_subject(&subject),
            shard,
            publication.publisher,
        );
        match key.stream() {
            Some(crate::shard::router::RuntimeStreamKey::Unreliable(stream)) => members
                .into_iter()
                .map(|(participant, channel)| {
                    (
                        shard,
                        crate::view::ViewOp::BindSubscribedData {
                            participant,
                            stream,
                            channel,
                        },
                    )
                })
                .collect(),
            Some(crate::shard::router::RuntimeStreamKey::Reliable(stream)) => members
                .into_iter()
                .map(|(participant, channel)| {
                    (
                        shard,
                        crate::view::ViewOp::BindSubscribedReliable {
                            participant,
                            stream,
                            channel,
                        },
                    )
                })
                .collect(),
            None => Vec::new(),
        }
    }

    fn publication_ops(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
        key: crate::control::publication::RuntimeKey,
        route: Option<RouteHandle>,
    ) -> Option<super::GenerationOps> {
        let publication = self.catalog.get(&id)?;
        let (plan_key, plan) = self.plan_for(id, shard, key)?;
        let mut ops = super::GenerationOps::lifecycle(Vec::with_capacity(4));
        let mut install_plan = true;
        match (&publication.media, key) {
            (
                crate::control::publication::Media::Data { topic, .. },
                crate::control::publication::RuntimeKey::Unreliable(_)
                | crate::control::publication::RuntimeKey::Reliable(_),
            ) => {
                let stream = key.stream()?;
                let stream_id = crate::shard::router::DataStreamId::new(
                    publication.room,
                    publication.publisher,
                    topic.clone(),
                );
                ops.lifecycle.push((
                    shard,
                    super::stream_lifecycle::insert_stream_runtime_op(
                        stream,
                        stream_id,
                        (shard == publication.publisher_shard).then_some(publication.publisher_key),
                    ),
                ));
                if let Some(route) = route {
                    ops.lifecycle.push((
                        shard,
                        super::stream_lifecycle::install_stream_route_op(stream, route),
                    ));
                }
                ops.extend_lifecycle(self.data_binding_ops(publication, shard, key));
            }
            (
                crate::control::publication::Media::Video { .. },
                crate::control::publication::RuntimeKey::Video(fanout),
            ) => {
                let descriptor = self.track_descriptor(id, shard)?;
                if shard != publication.publisher_shard && route.is_none() {
                    install_plan = false;
                    let recipients = self.room_recipients(publication.room, shard);
                    for participant in recipients {
                        ops = ops.participant_effect(
                            shard,
                            participant,
                            crate::participant::ParticipantEffect::TrackInstalled(
                                crate::participant::CompiledTrack {
                                    key: fanout.raw(),
                                    role: crate::participant::TrackRole::Subscribed,
                                    track: descriptor.publication.clone(),
                                },
                            ),
                        );
                    }
                } else {
                    let recipients = self.room_recipients(publication.room, shard);
                    if let Some(participant) = descriptor.participant {
                        ops = ops.participant_effect(
                            shard,
                            participant,
                            crate::participant::ParticipantEffect::TrackInstalled(
                                crate::participant::CompiledTrack {
                                    key: fanout.raw(),
                                    role: crate::participant::TrackRole::Published,
                                    track: descriptor.publication.clone(),
                                },
                            ),
                        );
                    }
                    for participant in recipients {
                        if Some(participant) == descriptor.participant {
                            continue;
                        }
                        ops = ops.participant_effect(
                            shard,
                            participant,
                            crate::participant::ParticipantEffect::TrackInstalled(
                                crate::participant::CompiledTrack {
                                    key: fanout.raw(),
                                    role: crate::participant::TrackRole::Subscribed,
                                    track: descriptor.publication.clone(),
                                },
                            ),
                        );
                    }
                    ops.lifecycle.push((
                        shard,
                        crate::view::ViewOp::InsertTrackRuntime {
                            key: crate::keys::TrackRuntimeKey::Video(fanout),
                            descriptor,
                        },
                    ));
                }
            }
            (
                crate::control::publication::Media::Audio,
                crate::control::publication::RuntimeKey::Audio(fanout),
            ) => {
                let descriptor = self.track_descriptor(id, shard)?;
                let recipients = self.room_recipients(publication.room, shard);
                if let Some(participant) = descriptor.participant {
                    ops = ops.participant_effect(
                        shard,
                        participant,
                        crate::participant::ParticipantEffect::TrackInstalled(
                            crate::participant::CompiledTrack {
                                key: fanout.raw(),
                                role: crate::participant::TrackRole::Published,
                                track: descriptor.publication.clone(),
                            },
                        ),
                    );
                }
                for participant in recipients {
                    if Some(participant) == descriptor.participant {
                        continue;
                    }
                    ops = ops.participant_effect(
                        shard,
                        participant,
                        crate::participant::ParticipantEffect::TrackInstalled(
                            crate::participant::CompiledTrack {
                                key: fanout.raw(),
                                role: crate::participant::TrackRole::Subscribed,
                                track: descriptor.publication.clone(),
                            },
                        ),
                    );
                }
                ops.lifecycle.push((
                    shard,
                    crate::view::ViewOp::InsertTrackRuntime {
                        key: crate::keys::TrackRuntimeKey::Audio(fanout),
                        descriptor,
                    },
                ));
            }
            _ => {
                debug_assert!(false, "a publication target must match its media kind");
                return None;
            }
        }
        if install_plan {
            ops = ops.plan(shard, plan_key, plan);
        }
        Some(ops)
    }

    pub(super) async fn install_destinations(
        &mut self,
        id: crate::entity::TrackId,
        mint: impl Fn(
            &mut Self,
            crate::id::ShardId,
        ) -> Option<(
            crate::control::publication::RuntimeKey,
            crate::route::RouteAction,
        )>,
    ) -> bool {
        let Some(publication) = self.catalog.get(&id) else {
            return true;
        };
        let publisher_shard = publication.publisher_shard;
        let mut wanted = indexmap::IndexSet::new();
        for shard in self.publication_shards(publication) {
            if shard != publisher_shard {
                wanted.insert(shard);
            }
        }
        let mut complete = true;
        for destination in wanted {
            complete &= self.install_destination(id, destination, &mint).await;
        }
        complete
    }

    pub(super) async fn install_destination(
        &mut self,
        id: crate::entity::TrackId,
        destination: crate::id::ShardId,
        mint: &impl Fn(
            &mut Self,
            crate::id::ShardId,
        ) -> Option<(
            crate::control::publication::RuntimeKey,
            crate::route::RouteAction,
        )>,
    ) -> bool {
        if self
            .catalog
            .get(&id)
            .is_some_and(|publication| publication.destinations.contains_key(&destination))
        {
            return true;
        }
        let Some((key, action)) = mint(self, destination) else {
            return false;
        };
        let Some((plan_key, plan)) = self.plan_for(id, destination, key) else {
            remove_runtime_key(&mut self.state, destination, key);
            debug_assert!(false, "a destination key must match its publication plan");
            return false;
        };
        let Some(route) = self
            .grant_route_binding(destination, action, Some((plan_key, plan)))
            .await
        else {
            remove_runtime_key(&mut self.state, destination, key);
            return false;
        };
        let Some(publication) = self.catalog.get_mut(&id) else {
            pulsebeam_runtime::fatal!("a destination must belong to its publication");
        };
        publication.destinations.insert(
            destination,
            crate::control::publication::Destination::Forwarding { key, route },
        );
        true
    }

    pub(super) async fn retire_stale_destinations(&mut self, id: crate::entity::TrackId) {
        let stale = {
            let Some(publication) = self.catalog.get(&id) else {
                return;
            };
            let wanted: indexmap::IndexSet<_> =
                self.publication_shards(publication).into_iter().collect();
            publication
                .destinations
                .keys()
                .filter(|destination| !wanted.contains(*destination))
                .copied()
                .collect::<Vec<_>>()
        };
        for destination in stale {
            self.retire_destination(id, destination).await;
        }
    }

    pub(super) async fn retire_destination(
        &mut self,
        id: crate::entity::TrackId,
        destination: crate::id::ShardId,
    ) {
        let Some(room_id) = self.catalog.get(&id).map(|publication| publication.room) else {
            return;
        };
        let Some(held) = self
            .catalog
            .get_mut(&id)
            .and_then(|publication| publication.destinations.shift_remove(&destination))
        else {
            return;
        };
        let mut ops = super::GenerationOps::lifecycle(Vec::with_capacity(3));
        let key = held.key();
        if let Some(track_key) = key.track() {
            for participant in self.room_recipients(room_id, destination) {
                ops.push_participant_effect(
                    destination,
                    participant,
                    crate::participant::ParticipantEffect::TrackRemoved {
                        key: track_key,
                        role: crate::participant::TrackRole::Subscribed,
                        kind: id.kind(),
                    },
                );
            }
        }
        let route = match held {
            crate::control::publication::Destination::Discovery { .. } => None,
            crate::control::publication::Destination::Forwarding { route, .. } => {
                ops.lifecycle.push((
                    destination,
                    crate::view::ViewOp::RetireRoute { handle: route },
                ));
                ops = ops.remove_plan(destination, plan_removal(key));
                ops.lifecycle.push((destination, runtime_removal_op(key)));
                Some(route)
            }
        };
        self.publish_generation(ops);

        let now = tokio::time::Instant::now();
        if let Some(route) = route {
            self.state
                .release_endpoint(destination, route.route.slot(), now);
        }
        remove_runtime_key(&mut self.state, destination, key);
    }

    /// Stage the retirement of a publication's destinations, whatever arena
    /// they live in.
    ///
    /// Staged rather than published directly, because a route pointing at a
    /// runtime that has already gone is a packet dropped at the destination
    /// with nothing to say why. The data lane used to retire without this and
    /// carried the same hazard.
    pub(super) fn stage_destination_retirement(
        &mut self,
        id: crate::entity::TrackId,
        room_id: crate::entity::RoomId,
        destinations: &indexmap::IndexMap<
            crate::id::ShardId,
            crate::control::publication::Destination,
        >,
        releases: &mut Vec<(crate::id::ShardId, RouteHandle)>,
        generation_ops: &mut super::GenerationOps,
    ) {
        for (destination, held) in destinations {
            match *held {
                crate::control::publication::Destination::Discovery { .. } => {
                    if let Some(track_key) = held.key().track() {
                        for participant in self.room_recipients(room_id, *destination) {
                            generation_ops.push_participant_effect(
                                *destination,
                                participant,
                                crate::participant::ParticipantEffect::TrackRemoved {
                                    key: track_key,
                                    role: crate::participant::TrackRole::Subscribed,
                                    kind: id.kind(),
                                },
                            );
                        }
                    }
                }
                crate::control::publication::Destination::Forwarding { key, route } => {
                    if let Some(track_key) = key.track() {
                        for participant in self.room_recipients(room_id, *destination) {
                            generation_ops.push_participant_effect(
                                *destination,
                                participant,
                                crate::participant::ParticipantEffect::TrackRemoved {
                                    key: track_key,
                                    role: crate::participant::TrackRole::Subscribed,
                                    kind: id.kind(),
                                },
                            );
                        }
                    }
                    generation_ops.lifecycle.push((
                        *destination,
                        crate::view::ViewOp::RetireRoute { handle: route },
                    ));
                    releases.push((*destination, route));
                    generation_ops.push_remove_plan(*destination, plan_removal(key));
                    generation_ops
                        .lifecycle
                        .push((*destination, runtime_removal_op(key)));
                }
            }
        }
    }

    /// Compile the final local recipients and inter-shard routes for one shard.
    pub(super) fn plan_for(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
        key: crate::control::publication::RuntimeKey,
    ) -> Option<(crate::plan::PlanKey, crate::plan::FlatTrackPlan)> {
        let publication = self.catalog.get(&id)?;
        let (plan_key, recipients) = match (&publication.media, key) {
            (
                crate::control::publication::Media::Video { .. },
                crate::control::publication::RuntimeKey::Video(key),
            ) => (
                crate::plan::PlanKey::Track(key.raw()),
                self.video_patterns
                    .members_for(
                        self.video_patterns
                            .match_subject(&crate::control::patterns::Subject {
                                room: publication.room,
                                publisher: publication.publisher,
                                name: id,
                            }),
                        shard,
                        publication.publisher,
                    )
                    .into_iter()
                    .map(|member| member.0)
                    .collect(),
            ),
            (
                crate::control::publication::Media::Audio,
                crate::control::publication::RuntimeKey::Audio(key),
            ) => (
                crate::plan::PlanKey::Track(key.raw()),
                self.audio_patterns
                    .members_for(
                        self.audio_patterns
                            .match_subject(&crate::control::patterns::Subject {
                                room: publication.room,
                                publisher: publication.publisher,
                                name: id,
                            }),
                        shard,
                        publication.publisher,
                    )
                    .into_iter()
                    .map(|member| member.0)
                    .collect(),
            ),
            (
                crate::control::publication::Media::Data { lane, topic },
                crate::control::publication::RuntimeKey::Unreliable(key),
            ) if crate::control::lanes::StreamLane::from(*lane)
                == crate::control::lanes::StreamLane::Unreliable =>
            {
                (
                    crate::plan::PlanKey::Unreliable(key),
                    self.data_patterns
                        .members_for(
                            self.data_patterns
                                .match_subject(&crate::control::patterns::Subject {
                                    room: publication.room,
                                    publisher: publication.publisher,
                                    name: (topic.clone(), (*lane).into()),
                                }),
                            shard,
                            publication.publisher,
                        )
                        .into_iter()
                        .map(|member| member.0)
                        .collect(),
                )
            }
            (
                crate::control::publication::Media::Data { lane, topic },
                crate::control::publication::RuntimeKey::Reliable(key),
            ) if crate::control::lanes::StreamLane::from(*lane)
                == crate::control::lanes::StreamLane::Reliable =>
            {
                (
                    crate::plan::PlanKey::Reliable(key),
                    self.data_patterns
                        .members_for(
                            self.data_patterns
                                .match_subject(&crate::control::patterns::Subject {
                                    room: publication.room,
                                    publisher: publication.publisher,
                                    name: (topic.clone(), (*lane).into()),
                                }),
                            shard,
                            publication.publisher,
                        )
                        .into_iter()
                        .map(|member| member.0)
                        .collect(),
                )
            }
            _ => {
                debug_assert!(false, "a runtime key must match its publication plan");
                return None;
            }
        };
        Some((
            plan_key,
            crate::control::publication::forwarding_plan(
                &publication.destinations,
                publication.publisher_shard,
                publication.publisher_key,
                publication.reverse_route,
                recipients,
                shard,
            ),
        ))
    }

    pub(super) fn video_plan_for(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> Option<crate::plan::FlatTrackPlan> {
        let key = self.catalog.get(&id).and_then(|publication| {
            if publication.publisher_shard == shard {
                publication.origin_key
            } else {
                publication.destinations.get(&shard)?.key()
            }
            .track()
        })?;
        let (plan_key, plan) = self.plan_for(
            id,
            shard,
            crate::control::publication::RuntimeKey::Video(crate::keys::VideoTrackKey::new(key)),
        )?;
        debug_assert_eq!(plan_key, crate::plan::PlanKey::Track(key));
        Some(plan)
    }

    fn publication_shards(
        &self,
        publication: &crate::control::publication::Publication,
    ) -> Vec<crate::id::ShardId> {
        match &publication.media {
            crate::control::publication::Media::Video { .. } => self
                .video_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: publication.id,
                })
                .into_iter()
                .flat_map(|group| self.video_patterns.shards_of(group))
                .collect(),
            crate::control::publication::Media::Audio => self
                .audio_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: publication.id,
                })
                .into_iter()
                .flat_map(|group| self.audio_patterns.shards_of(group))
                .collect(),
            crate::control::publication::Media::Data { lane, topic } => self
                .data_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: (topic.clone(), (*lane).into()),
                })
                .into_iter()
                .flat_map(|group| self.data_patterns.shards_of(group))
                .collect(),
        }
    }
}
