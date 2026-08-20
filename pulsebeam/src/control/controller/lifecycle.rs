//! One publication lifecycle, whatever kind the publication is.
//!
//! Announce, install destinations, plan, publish, retire. Video, audio and the
//! two data lanes ran their own copy of each of these; what is left of the
//! difference between them is passed in - which arena a destination key comes
//! from, which route action reaches it - and everything else is written once.

use super::*;

/// Which image a publication's plan belongs to. Video and audio share an arena
/// and a key type, so the kind is what tells them apart — the one thing about a
/// media kind the routing layer still has to know.
pub(super) fn plan_target(
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

/// The op that removes a publication's runtime from the arena its key names.
pub(super) fn runtime_removal_op(
    key: crate::control::publication::RuntimeKey,
) -> crate::view::ViewOp {
    use crate::control::publication::RuntimeKey;
    match key {
        RuntimeKey::Track(key) => crate::view::ViewOp::RemoveTrackRuntime { key },
        RuntimeKey::Unreliable(key) => crate::view::ViewOp::RemoveUnreliableRuntime { key },
        RuntimeKey::Reliable(key) => crate::view::ViewOp::RemoveReliableRuntime { key },
    }
}

impl ControllerActor {
    /// Retire a publication of any kind: its declarations, its routes, its
    /// plans and its runtimes.
    ///
    /// Staged as one transaction so a route never outlives the runtime it
    /// resolves to. `arena` is the only per-kind step left - a media key is
    /// returned to the track arena, a data key to the lane's.
    pub(super) async fn retire_publication(
        &mut self,
        id: crate::entity::TrackId,
        arena: impl Fn(&mut Self, crate::id::ShardId, crate::control::publication::RuntimeKey),
    ) -> bool {
        let Some(publication) = self.catalog.get(&id) else {
            return true;
        };
        let kind = publication.kind();
        let publisher_shard = publication.publisher_shard;
        let origin_key = publication.origin_key;
        let reverse_route = publication.reverse_route;
        let destinations = publication.destinations.clone();
        let retired_pattern =
            crate::control::patterns::Pattern::exact(publication.room, publication.publisher, id);

        // A video track's subscribers named it directly and never unsubscribe
        // from something that goes away, so their declarations go with it.
        if kind == crate::entity::TrackKind::Video
            && let Some((group, members)) = self.video_patterns.retire_pattern(&retired_pattern)
        {
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
                debug_assert!(false, "group retirement must publish");
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

        let mut releases = Vec::new();
        if !self.stage_destination_retirement(generation, now, kind, &destinations, &mut releases) {
            return false;
        }
        let Some(view) = self.view_mut(publisher_shard) else {
            debug_assert!(false, "a publisher must name a local view");
            self.abort_transaction(now);
            return false;
        };
        if let Some(route) = reverse_route {
            view.stage(
                generation,
                crate::view::ViewOp::RetireRoute {
                    route: route.route,
                    epoch: route.epoch,
                },
            );
            releases.push((publisher_shard, route));
        }
        view.stage(
            generation,
            crate::view::ViewOp::RemovePlan {
                target: plan_target(kind, origin_key),
            },
        );
        view.stage(generation, runtime_removal_op(origin_key));

        for index in 0..self.views.len() {
            if let Some(view) = self.view_mut(crate::id::ShardId::new(index)) {
                let _ = view.publish();
            }
        }
        if self.state.commit().is_err() {
            debug_assert!(false, "a retirement must commit");
            self.abort_transaction(now);
            return false;
        }
        for (shard, route) in releases {
            self.state.release_endpoint(shard, route.route.slot(), now);
        }
        arena(self, publisher_shard, origin_key);
        for (&destination, held) in &destinations {
            arena(self, destination, held.key);
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
        let kind = publication.kind();
        let publisher_shard = publication.publisher_shard;
        let publisher_key = publication.publisher_key;
        let stream_id = match &publication.media {
            crate::control::publication::Media::Data { topic, .. } => {
                Some(crate::shard::router::DataStreamId::new(
                    publication.room,
                    publication.publisher,
                    topic.clone(),
                ))
            }
            _ => None,
        };
        let mut targets = vec![(publisher_shard, publication.origin_key, None)];
        for (shard, held) in &publication.destinations {
            targets.push((*shard, held.key, held.route));
        }

        let mut ops = Vec::new();
        for (shard, key, route) in targets {
            let Some(plan) = self.plan_for(id, shard) else {
                continue;
            };
            match (&stream_id, key.track()) {
                (Some(stream_id), _) => {
                    let Some(key) = key.stream() else {
                        continue;
                    };
                    ops.push((
                        shard,
                        super::stream_lifecycle::insert_stream_runtime_op(
                            key,
                            stream_id.clone(),
                            publisher_key,
                        ),
                    ));
                    ops.extend(route.map(|route| {
                        (
                            shard,
                            super::stream_lifecycle::install_stream_route_op(key, route),
                        )
                    }));
                }
                (None, Some(fanout)) => {
                    let Some(descriptor) = self.track_descriptor(id, shard) else {
                        continue;
                    };
                    ops.push((
                        shard,
                        crate::view::ViewOp::InsertTrackRuntime {
                            key: fanout,
                            descriptor,
                        },
                    ));
                }
                (None, None) => continue,
            }
            ops.push((
                shard,
                crate::view::ViewOp::SetPlan {
                    target: plan_target(kind, key),
                    plan,
                },
            ));
        }
        self.publish_ops(ops)
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
            return false;
        };
        let publisher_shard = publication.publisher_shard;
        let kind = publication.kind();
        let mut wanted: Vec<crate::id::ShardId> = Vec::new();
        for group in self.groups_of(publication) {
            for shard in self.audience_shards(kind, group) {
                if shard != publisher_shard && !wanted.contains(&shard) {
                    wanted.push(shard);
                }
            }
        }

        if !self.retire_stale_destinations(id, &wanted).await {
            debug_assert!(false, "stale destination retirement must complete");
            return false;
        }

        let mut added = false;
        for destination in wanted {
            if self
                .catalog
                .get(&id)
                .is_some_and(|held| held.destinations.contains_key(&destination))
            {
                continue;
            }
            let Some((key, action)) = mint(self, destination) else {
                continue;
            };
            let Some(plan) = self.plan_for(id, destination) else {
                continue;
            };
            let Some(route) = self
                .grant_route_binding(destination, action, Some((plan_target(kind, key), plan)))
                .await
            else {
                continue;
            };
            let Some(publication) = self.catalog.get_mut(&id) else {
                return false;
            };
            publication.destinations.insert(
                destination,
                crate::control::publication::Destination {
                    key,
                    route: Some(route),
                },
            );
            added = true;
        }
        added
    }

    /// Give every shard that declared an interest a key and a route to reach
    /// this publication by, and publish the result.
    ///
    /// The publisher's own shard is skipped: it holds the source, and a route
    /// to itself would be a second hop to nowhere. `mint` is the per-kind step,
    /// naming which arena the destination's key comes from and which route
    /// action reaches it.
    /// Drop the destinations a publication no longer has an audience on.
    ///
    /// A route to a shard where the last member left is a slot the allocator
    /// cannot reuse and a runtime nothing resolves. Data retired these; audio
    /// did not, so an audio route outlived its last listener on a shard until
    /// the track itself went away.
    pub(super) async fn retire_stale_destinations(
        &mut self,
        id: crate::entity::TrackId,
        wanted: &[crate::id::ShardId],
    ) -> bool {
        let Some(publication) = self.catalog.get(&id) else {
            return true;
        };
        let kind = publication.kind();
        let stale: Vec<_> = publication
            .destinations
            .iter()
            .filter(|(destination, _)| !wanted.contains(destination))
            .map(|(destination, held)| (*destination, *held))
            .collect();
        if stale.is_empty() {
            return true;
        }

        let mut ops = Vec::new();
        for (destination, held) in &stale {
            if let Some(route) = held.route {
                ops.push((
                    *destination,
                    crate::view::ViewOp::RetireRoute {
                        route: route.route,
                        epoch: route.epoch,
                    },
                ));
            }
            ops.push((
                *destination,
                crate::view::ViewOp::RemovePlan {
                    target: plan_target(kind, held.key),
                },
            ));
            ops.push((*destination, runtime_removal_op(held.key)));
        }
        if !self.publish_ops(ops) {
            return false;
        }

        let now = tokio::time::Instant::now();
        for (destination, held) in &stale {
            if let Some(route) = held.route {
                self.state
                    .release_endpoint(*destination, route.route.slot(), now);
            }
            if let Some(publication) = self.catalog.get_mut(&id) {
                publication.destinations.shift_remove(destination);
            }
        }
        true
    }

    /// Stage the retirement of one kind's destination routes for a track.
    ///
    /// Video and audio differ only in which fanout map names the destination's
    /// key and which image the plan lives in, so the walk is written once.
    /// Stage the retirement of a publication's destinations, whatever arena
    /// they live in.
    ///
    /// Staged rather than published directly, because a route pointing at a
    /// runtime that has already gone is a packet dropped at the destination
    /// with nothing to say why. The data lane used to retire without this and
    /// carried the same hazard.
    pub(super) fn stage_destination_retirement(
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
                debug_assert!(false, "a destination must name a local view");
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
            view.stage(
                generation,
                crate::view::ViewOp::RemovePlan {
                    target: plan_target(kind, held.key),
                },
            );
            view.stage(generation, runtime_removal_op(held.key));
        }
        true
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

    /// The shards holding members of an audience, whichever image it lives in.
    fn audience_shards(
        &self,
        kind: crate::entity::TrackKind,
        group: crate::view::GroupId,
    ) -> Vec<crate::id::ShardId> {
        match kind {
            crate::entity::TrackKind::Audio => self.audio_patterns.shards_of(group),
            crate::entity::TrackKind::Video => self.video_patterns.shards_of(group),
            crate::entity::TrackKind::Data => self.data_patterns.shards_of(group),
        }
    }
}
