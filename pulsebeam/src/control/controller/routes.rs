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
        self.grant_route_binding(shard_id, action, None, None, None, None)
            .await
    }

    pub(super) async fn grant_route_with_plan(
        &mut self,
        shard_id: crate::id::ShardId,
        action: RouteAction,
        lane: StreamLane,
        plan: crate::view::StreamPlan,
    ) -> Option<RouteHandle> {
        match lane {
            StreamLane::Data => {
                self.grant_route_binding(shard_id, action, None, None, Some(plan), None)
                    .await
            }
            StreamLane::Reliable => {
                self.grant_route_binding(shard_id, action, None, None, None, Some(plan))
                    .await
            }
        }
    }

    pub(super) async fn grant_route_binding(
        &mut self,
        shard_id: crate::id::ShardId,
        action: crate::route::RouteAction,
        video_plan: Option<crate::view::VideoPlan>,
        audio_plan: Option<crate::view::AudioPlan>,
        data_plan: Option<crate::view::StreamPlan>,
        reliable_plan: Option<crate::view::StreamPlan>,
    ) -> Option<RouteHandle> {
        // A stream granted here — a track's reverse route, an audio fanout, a
        // data lane — is announced once and never re-offered. Losing the
        // allocation loses the stream for the rest of the session, so this
        // retries for the same reason connection setup does.
        self.publish_with_route(shard_id, "endpoint", move |_, handle| {
            let mut ops = vec![(
                shard_id,
                crate::view::ViewOp::InstallRoute {
                    route: handle.route,
                    binding: crate::view::RouteBinding {
                        epoch: handle.epoch,
                        action,
                    },
                },
            )];
            let plan_op = match (action, video_plan, audio_plan, data_plan, reliable_plan) {
                (RouteAction::Video { local_track }, Some(plan), _, _, _) => {
                    Some(crate::view::ViewOp::SetTrackPlan {
                        key: local_track,
                        plan,
                    })
                }
                (RouteAction::Audio { track }, _, Some(plan), _, _) => {
                    Some(crate::view::ViewOp::SetAudioPlan { key: track, plan })
                }
                (RouteAction::Data { stream }, _, _, Some(plan), _) => {
                    Some(crate::view::ViewOp::SetDataPlan { key: stream, plan })
                }
                (RouteAction::Reliable { stream }, _, _, _, Some(plan)) => {
                    Some(crate::view::ViewOp::SetReliablePlan { key: stream, plan })
                }
                (RouteAction::Reverse { .. }, None, None, None, None) => None,
                _ => {
                    debug_assert!(false, "route action and compiled plan disagree");
                    None
                }
            };
            ops.extend(plan_op.map(|op| (shard_id, op)));
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
        let retire = vec![(
            shard_id,
            crate::view::ViewOp::RetireRoute {
                route: handle.route,
                epoch: handle.epoch,
            },
        )];
        if !self.publish_ops(retire) {
            return;
        }
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

        let Some(_) = self.publish_pending(shard_id, handle, participant_key) else {
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
        handle: TransportHandle,
        participant: crate::shard::participants::ParticipantKey,
    ) -> Option<u64> {
        let generation = self.state.pending().map(|tx| tx.generation)?;
        let view = self.view_mut(shard_id)?;
        view.stage(
            generation,
            crate::view::ViewOp::InsertParticipant { key: participant },
        );
        view.stage(
            generation,
            crate::view::ViewOp::InstallTransport {
                route: handle.route,
                binding: crate::view::TransportBinding {
                    epoch: handle.epoch,
                    participant,
                },
            },
        );
        view.publish()
    }
}
