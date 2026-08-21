use super::*;

impl ControllerActor {
    /// Allocate, publish and confirm one endpoint route.
    ///
    /// The action is opaque here: it holds keys that mean something only on
    /// the shard that asked, and the control plane's job is to give them an
    /// address and put them in that shard's view — not to interpret them.
    pub(super) async fn grant_route(
        &mut self,
        shard_id: crate::id::ShardId,
        action: crate::route::RouteAction,
    ) -> Option<RouteHandle> {
        self.grant_route_binding(shard_id, action, None).await
    }

    /// Grant a route and publish the plan it serves, in one generation.
    ///
    /// A stream granted here — a track's reverse route, an audio fanout, a data
    /// lane — is announced once and never re-offered. Losing the allocation
    /// loses the stream for the rest of the session, so this retries for the
    /// same reason connection setup does.
    ///
    /// The plan is one type for every kind now, so where this took four
    /// mutually exclusive `Option`s and a match to prove only one was set, it
    /// takes the plan and the image it belongs to.
    pub(super) async fn grant_route_binding(
        &mut self,
        shard_id: crate::id::ShardId,
        action: crate::route::RouteAction,
        plan: Option<(crate::plan::PlanKey, crate::plan::FlatTrackPlan)>,
    ) -> Option<RouteHandle> {
        self.publish_with_route(shard_id, "endpoint", move |_, handle| {
            let lifecycle = vec![(
                shard_id,
                crate::view::ViewOp::InstallRoute {
                    binding: crate::view::RouteBinding {
                        handle: *handle,
                        action,
                    },
                },
            )];
            let mut ops = super::GenerationOps::lifecycle(lifecycle);
            if let Some((key, plan)) = plan {
                ops = ops.plan(shard_id, key, plan);
            }
            ops
        })
    }

    /// Take a route out of the published view, then return its slot.
    ///
    /// The order is the whole point: a slot handed back before the route is
    /// absent from the view could be granted again while a packet addressed
    /// to its predecessor is still arriving.
    pub(super) async fn release_route(
        &mut self,
        shard_id: crate::id::ShardId,
        handle: RouteHandle,
    ) {
        debug_assert_eq!(handle.shard(), shard_id);
        let retire = vec![(shard_id, crate::view::ViewOp::RetireRoute { handle })];
        self.publish_ops(retire);
        self.state
            .release_endpoint(shard_id, handle.route.slot(), tokio::time::Instant::now());
    }

    /// Stage a participant's transport route as one lifecycle generation.
    ///
    /// The control plane allocates the address, the owning shard prepares
    /// only the runtime binding it must build on its own core, and the route
    /// becomes resolvable when the view carrying it is applied on the owning
    /// shard. The caller advertises the route after queuing the delta; a first
    /// packet racing that apply is a counted, recoverable drop.
    ///
    /// `drain_core_events` runs first so a `RemoveParticipant` queued by a
    /// preceding delete (a reconnect's teardown-then-recreate) reaches the
    /// shard's mailbox before this does; otherwise the prepare below could
    /// race the old entry under the same id and trip the registry's
    /// duplicate-reservation assertion.
    pub(super) async fn stage_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        participant_id: ParticipantId,
        room_id: crate::entity::RoomId,
    ) -> Option<(TransportHandle, crate::shard::participants::ParticipantKey)> {
        self.drain_core_events().await;

        let now = tokio::time::Instant::now();
        if self.state.begin().is_err() {
            debug_assert!(false, "lifecycle transactions serialise through this actor");
            return None;
        }

        // Bounded retries for a transient allocation failure. Every other
        // install failure here recovers on a later externally-triggered retry
        // (the next subscribe, the next publish); connection setup has no such
        // later trigger — a client gets this one attempt before it sees the
        // join itself fail — so the retry has to happen here.
        let mut reserved = None;
        for attempt in 1..=Self::ROUTE_ALLOCATION_ATTEMPTS {
            match self.state.reserve_transport(shard_id, now) {
                Ok(handle) => {
                    reserved = Some(handle);
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        %participant_id,
                        attempt,
                        "transport route allocation failed, retrying"
                    );
                }
            }
        }
        let Some(handle) = reserved else {
            self.abort_transaction(now);
            return None;
        };

        let Some(participant_key) = self
            .prepare_transport(shard_id, participant_id, handle)
            .await
        else {
            self.abort_transaction(now);
            return None;
        };

        let Some(_) =
            self.publish_pending(shard_id, participant_id, room_id, handle, participant_key)
        else {
            self.abort_transaction(now);
            return None;
        };

        if self.state.commit().is_err() {
            self.abort_transaction(now);
            return None;
        }
        Some((handle, participant_key))
    }

    /// Ask the owning shard to build the runtime binding this route will
    /// point at. The shard decides nothing here — it reserves a key on its
    /// own core and reports it back.
    pub(super) async fn prepare_transport(
        &mut self,
        shard_id: crate::id::ShardId,
        participant_id: ParticipantId,
        handle: TransportHandle,
    ) -> Option<crate::shard::participants::ParticipantKey> {
        debug_assert_eq!(handle.shard(), shard_id);
        self.state.mint_participant(shard_id, participant_id)
    }

    /// Compile and publish the staged generation. One publish, one shard.
    pub(super) fn publish_pending(
        &mut self,
        shard_id: crate::id::ShardId,
        participant_id: ParticipantId,
        room_id: crate::entity::RoomId,
        handle: TransportHandle,
        participant: crate::shard::participants::ParticipantKey,
    ) -> Option<u64> {
        let generation = self.state.pending().map(|tx| tx.generation)?;
        let existing = self.core.registry.participants_in_room(&room_id);
        {
            let view = self.view_mut(shard_id)?;
            view.stage(generation, crate::view::ViewOp::InsertParticipant);
            view.stage(
                generation,
                crate::view::ViewOp::InstallTransport {
                    binding: crate::view::TransportBinding {
                        handle,
                        participant,
                    },
                },
            );
        }
        for (existing_id, existing_shard, existing_key) in existing {
            let Some(existing_key) = existing_key else {
                debug_assert!(false, "a registered participant must have a binding");
                return None;
            };
            let Some(existing_view) = self.view_mut(existing_shard) else {
                debug_assert!(false, "a room participant must have a live shard view");
                return None;
            };
            existing_view.stage_participant_effect(
                generation,
                existing_key,
                crate::participant::ParticipantEffect::ParticipantsChanged {
                    added: vec![participant_id],
                    removed: Vec::new(),
                },
            );
            debug_assert_ne!(existing_id, participant_id);
        }
        if !self.publish_staged_views() {
            return None;
        }
        Some(generation)
    }

    pub(super) fn publish_participant_roster(
        &mut self,
        room_id: crate::entity::RoomId,
        participant_id: ParticipantId,
        participant: crate::shard::participants::ParticipantKey,
    ) {
        let Some(shard_id) = self
            .core
            .registry
            .get_participant(&participant_id)
            .map(|meta| meta.shard_id)
        else {
            debug_assert!(false, "a materialized participant must be registered");
            return;
        };
        let participant_ids = self
            .core
            .registry
            .participant_ids_in_room(&room_id)
            .into_iter()
            .filter(|id| *id != participant_id)
            .collect();
        let ops = super::GenerationOps::lifecycle(Vec::new()).participant_effect(
            shard_id,
            participant,
            crate::participant::ParticipantEffect::ParticipantsChanged {
                added: participant_ids,
                removed: Vec::new(),
            },
        );
        self.publish_generation(ops);
    }
}
