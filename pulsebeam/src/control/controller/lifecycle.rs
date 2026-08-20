//! One publication lifecycle, whatever kind the publication is.
//!
//! Announce, install destinations, plan, publish, retire. Video, audio and the
//! two data lanes ran their own copy of each of these; what is left of the
//! difference between them is passed in - which arena a destination key comes
//! from, which route action reaches it - and everything else is written once.

use super::*;

pub(super) fn plan_removal(
    key: crate::control::publication::RuntimeKey,
) -> crate::view::PlanRemoval {
    use crate::control::publication::RuntimeKey;
    match key {
        RuntimeKey::Video(key) => crate::view::PlanRemoval::Video(key),
        RuntimeKey::Audio(key) => crate::view::PlanRemoval::Audio(key),
        RuntimeKey::Unreliable(key) => crate::view::PlanRemoval::Unreliable(key),
        RuntimeKey::Reliable(key) => crate::view::PlanRemoval::Reliable(key),
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
        let reverse_route = publication.reverse_route;
        let destinations = publication.destinations.clone();
        let room = publication.room;
        let publisher = publication.publisher;
        self.unindex_publication(id);
        let retired_pattern = crate::control::patterns::Pattern::exact(room, publisher, id);

        // A video track's subscribers named it directly and never unsubscribe
        // from something that goes away, so their declarations go with it.
        if kind == crate::entity::TrackKind::Video
            && let Some((group, members)) = self.video_patterns.retire_pattern(&retired_pattern)
        {
            let ops = members
                .into_iter()
                .map(|(_, shard, key)| {
                    (shard, crate::view::ViewOp::RemoveVideoMember { group, key })
                })
                .collect();
            self.publish_ops(ops);
        }

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            pulsebeam_runtime::fatal!("lifecycle transactions must be serial");
        }
        let Some(generation) = self.state.pending().map(|tx| tx.generation) else {
            pulsebeam_runtime::fatal!("a begun lifecycle transaction must be pending");
        };

        let mut releases = Vec::new();
        self.stage_destination_retirement(id, room, generation, &destinations, &mut releases);
        let Some(view) = self.view_mut(publisher_shard) else {
            pulsebeam_runtime::fatal!("a publisher must name a local view");
        };
        if let Some(route) = reverse_route {
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute { handle: route },
            );
            releases.push((publisher_shard, route));
        }
        view.stage(
            generation,
            crate::view::ViewOp::RemovePlan {
                target: plan_removal(origin_key),
            },
        );
        view.stage(generation, runtime_removal_op(origin_key));

        if !self.publish_staged_views() {
            self.abort_transaction(now);
            return false;
        }
        if self.state.commit().is_err() {
            pulsebeam_runtime::fatal!("a published retirement must commit");
        }
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

        let mut ops = Vec::new();
        for (shard, key, route) in targets {
            let Some(target_ops) = self.publication_ops(id, shard, key, route) else {
                return false;
            };
            ops.extend(target_ops);
        }
        self.publish_ops(ops);
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
        self.publish_ops(ops);
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
        let Some(plan) = self.plan_for(id, shard, key) else {
            return false;
        };
        self.publish_ops(vec![(shard, crate::view::ViewOp::SetPlan { update: plan })]);
        true
    }

    fn publication_ops(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
        key: crate::control::publication::RuntimeKey,
        route: Option<RouteHandle>,
    ) -> Option<Vec<(crate::id::ShardId, crate::view::ViewOp)>> {
        let publication = self.catalog.get(&id)?;
        let plan = self.plan_for(id, shard, key)?;
        let mut ops = Vec::with_capacity(3);
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
                ops.push((
                    shard,
                    super::stream_lifecycle::insert_stream_runtime_op(
                        stream,
                        stream_id,
                        (shard == publication.publisher_shard).then_some(publication.publisher_key),
                    ),
                ));
                if let Some(route) = route {
                    ops.push((
                        shard,
                        super::stream_lifecycle::install_stream_route_op(stream, route),
                    ));
                }
            }
            (
                crate::control::publication::Media::Video { .. },
                crate::control::publication::RuntimeKey::Video(fanout),
            ) => {
                let descriptor = self.track_descriptor(id, shard)?;
                if shard != publication.publisher_shard && route.is_none() {
                    install_plan = false;
                    ops.push((
                        shard,
                        crate::view::ViewOp::AnnounceTrack {
                            publication: Box::new(descriptor.publication),
                        },
                    ));
                } else {
                    ops.push((
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
                ops.push((
                    shard,
                    crate::view::ViewOp::InsertTrackRuntime {
                        key: crate::keys::TrackRuntimeKey::Audio(fanout),
                        descriptor: self.track_descriptor(id, shard)?,
                    },
                ));
            }
            _ => {
                debug_assert!(false, "a publication target must match its media kind");
                return None;
            }
        }
        if install_plan {
            ops.push((shard, crate::view::ViewOp::SetPlan { update: plan }));
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
        let Some(plan) = self.plan_for(id, destination, key) else {
            remove_runtime_key(&mut self.state, destination, key);
            debug_assert!(false, "a destination key must match its publication plan");
            return false;
        };
        let Some(route) = self
            .grant_route_binding(destination, action, Some(plan))
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
        let mut ops = Vec::with_capacity(3);
        let key = held.key();
        let route = match held {
            crate::control::publication::Destination::Discovery { .. } => {
                ops.push((
                    destination,
                    crate::view::ViewOp::WithdrawTrack { id, room_id },
                ));
                None
            }
            crate::control::publication::Destination::Forwarding { route, .. } => {
                ops.push((
                    destination,
                    crate::view::ViewOp::RetireRoute { handle: route },
                ));
                ops.push((
                    destination,
                    crate::view::ViewOp::RemovePlan {
                        target: plan_removal(key),
                    },
                ));
                ops.push((destination, runtime_removal_op(key)));
                Some(route)
            }
        };
        self.publish_ops(ops);

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
        generation: u64,
        destinations: &indexmap::IndexMap<
            crate::id::ShardId,
            crate::control::publication::Destination,
        >,
        releases: &mut Vec<(crate::id::ShardId, RouteHandle)>,
    ) {
        for (destination, held) in destinations {
            let Some(view) = self.view_mut(*destination) else {
                pulsebeam_runtime::fatal!("a destination must name a local view");
            };
            match *held {
                crate::control::publication::Destination::Discovery { .. } => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::WithdrawTrack { id, room_id },
                    );
                }
                crate::control::publication::Destination::Forwarding { key, route } => {
                    view.stage(
                        generation,
                        crate::view::ViewOp::RetireRoute { handle: route },
                    );
                    releases.push((*destination, route));
                    view.stage(
                        generation,
                        crate::view::ViewOp::RemovePlan {
                            target: plan_removal(key),
                        },
                    );
                    view.stage(generation, runtime_removal_op(key));
                }
            }
        }
    }

    /// The audiences a publication reaches.
    ///
    /// The tables are per kind because the delivery key is - a video slot, an
    /// audio slot chosen per packet, an SCTP channel - and the data table is
    /// keyed by topic because a data declaration names one across publishers.
    /// Selecting the table is the whole of what differs; a match yields the
    /// same group ids either way.
    pub(super) fn plan_for(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
        key: crate::control::publication::RuntimeKey,
    ) -> Option<crate::view::PlanUpdate> {
        let publication = self.catalog.get(&id)?;
        match (&publication.media, key) {
            (
                crate::control::publication::Media::Video { .. },
                crate::control::publication::RuntimeKey::Video(key),
            ) => Some(crate::view::PlanUpdate::Video {
                key,
                plan: crate::control::publication::forwarding_plan(
                    &publication.destinations,
                    publication.publisher_shard,
                    publication.reverse_route,
                    self.video_patterns
                        .match_subject(&crate::control::patterns::Subject {
                            room: publication.room,
                            publisher: publication.publisher,
                            name: id,
                        }),
                    shard,
                ),
            }),
            (
                crate::control::publication::Media::Audio,
                crate::control::publication::RuntimeKey::Audio(key),
            ) => Some(crate::view::PlanUpdate::Audio {
                key,
                plan: crate::control::publication::forwarding_plan(
                    &publication.destinations,
                    publication.publisher_shard,
                    publication.reverse_route,
                    self.audio_patterns
                        .match_subject(&crate::control::patterns::Subject {
                            room: publication.room,
                            publisher: publication.publisher,
                            name: id,
                        }),
                    shard,
                ),
            }),
            (
                crate::control::publication::Media::Data { lane, topic },
                crate::control::publication::RuntimeKey::Unreliable(key),
            ) if crate::control::lanes::StreamLane::from(*lane)
                == crate::control::lanes::StreamLane::Unreliable =>
            {
                Some(crate::view::PlanUpdate::Unreliable {
                    key,
                    plan: crate::control::publication::forwarding_plan(
                        &publication.destinations,
                        publication.publisher_shard,
                        publication.reverse_route,
                        self.data_patterns
                            .match_subject(&crate::control::patterns::Subject {
                                room: publication.room,
                                publisher: publication.publisher,
                                name: (topic.clone(), (*lane).into()),
                            }),
                        shard,
                    ),
                })
            }
            (
                crate::control::publication::Media::Data { lane, topic },
                crate::control::publication::RuntimeKey::Reliable(key),
            ) if crate::control::lanes::StreamLane::from(*lane)
                == crate::control::lanes::StreamLane::Reliable =>
            {
                Some(crate::view::PlanUpdate::Reliable {
                    key,
                    plan: crate::control::publication::forwarding_plan(
                        &publication.destinations,
                        publication.publisher_shard,
                        publication.reverse_route,
                        self.data_patterns
                            .match_subject(&crate::control::patterns::Subject {
                                room: publication.room,
                                publisher: publication.publisher,
                                name: (topic.clone(), (*lane).into()),
                            }),
                        shard,
                    ),
                })
            }
            _ => {
                debug_assert!(false, "a runtime key must match its publication plan");
                None
            }
        }
    }

    pub(super) fn video_plan_for(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> Option<crate::view::VideoPlan> {
        let key = self.catalog.get(&id).and_then(|publication| {
            if publication.publisher_shard == shard {
                publication.origin_key
            } else {
                publication.destinations.get(&shard)?.key()
            }
            .track()
        })?;
        match self.plan_for(
            id,
            shard,
            crate::control::publication::RuntimeKey::Video(crate::keys::VideoTrackKey::new(key)),
        )? {
            crate::view::PlanUpdate::Video { plan, .. } => Some(plan),
            _ => {
                debug_assert!(false, "a video route must have a video plan");
                None
            }
        }
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

    fn publication_reaches_shard_inner(
        &self,
        publication: &crate::control::publication::Publication,
        shard: crate::id::ShardId,
    ) -> bool {
        match &publication.media {
            crate::control::publication::Media::Video { .. } => self
                .video_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: publication.id,
                })
                .into_iter()
                .any(|group| self.video_patterns.has_shard(group, shard)),
            crate::control::publication::Media::Audio => self
                .audio_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: publication.id,
                })
                .into_iter()
                .any(|group| self.audio_patterns.has_shard(group, shard)),
            crate::control::publication::Media::Data { lane, topic } => self
                .data_patterns
                .match_subject(&crate::control::patterns::Subject {
                    room: publication.room,
                    publisher: publication.publisher,
                    name: (topic.clone(), (*lane).into()),
                })
                .into_iter()
                .any(|group| self.data_patterns.has_shard(group, shard)),
        }
    }

    pub(super) fn publication_reaches_shard(
        &self,
        id: crate::entity::TrackId,
        shard: crate::id::ShardId,
    ) -> bool {
        let Some(publication) = self.catalog.get(&id) else {
            return false;
        };
        publication.publisher_shard != shard
            && self.publication_reaches_shard_inner(publication, shard)
    }
}
