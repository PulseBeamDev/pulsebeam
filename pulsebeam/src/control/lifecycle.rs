use std::collections::{HashMap, HashSet};

use tokio::time::Instant;

use crate::{
    control::{
        registry::RoomRegistry,
        topology::{TrackAllocation, TrackAllocator, TrackIdentity, TrackTopology},
    },
    entity::{ParticipantId, RoomId, TrackId},
    id::ShardId,
    keys::{ParticipantKey, TrackKey},
    participant::ParticipantEffect,
    route::{RouteAction, RouteHandle},
    shard_update::{ShardUpdateOp, TrackDescriptor, TrackPlan, TrackPlanUpdate, TrackRuntime},
    track::{SelectionPolicy, Track, TrackSelector},
};

#[derive(Debug, Clone, Copy)]
struct ParticipantLocation {
    shard: ShardId,
    room_id: RoomId,
    binding: Option<ParticipantKey>,
}

impl ParticipantLocation {
    fn lookup(registry: &RoomRegistry, participant: &ParticipantId) -> Option<Self> {
        registry.get_participant(participant).map(|meta| Self {
            shard: meta.shard_id,
            room_id: meta.room_id,
            binding: meta.binding,
        })
    }

    fn bound_in_room(
        registry: &RoomRegistry,
        participant: &ParticipantId,
        room_id: RoomId,
    ) -> Option<BoundParticipantLocation> {
        let location = Self::lookup(registry, participant)?;
        if location.room_id != room_id {
            return None;
        }
        Some(BoundParticipantLocation {
            shard: location.shard,
            key: location.binding?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct BoundParticipantLocation {
    shard: ShardId,
    key: ParticipantKey,
}

/// Shard-local allocation held for one track.
///
/// Candidate shards reserve an allocation before they become active so participant
/// effects can refer to a stable shard-local TrackKey. `resident` means the shard
/// currently has the track runtime/plan installed. `route_installed` tracks the
/// route table separately because an origin shard only needs a route for tracks
/// with a reverse path.
#[derive(Debug, Clone, Copy)]
struct TrackDestination {
    allocation: TrackAllocation,
    resident: bool,
    route_installed: bool,
}

impl TrackDestination {
    fn reserved(allocation: TrackAllocation) -> Self {
        Self {
            allocation,
            resident: false,
            route_installed: false,
        }
    }
}

pub(crate) enum TrackLifecycleOperation {
    Update {
        shard: ShardId,
        op: ShardUpdateOp,
    },
    Plans {
        shard: ShardId,
        plans: Vec<TrackPlanUpdate>,
    },
    ParticipantEffect {
        shard: ShardId,
        participant: ParticipantKey,
        effect: ParticipantEffect,
    },
}

pub(crate) struct TrackLifecycleOutcome {
    pub(crate) generation: u64,
    pub(crate) operations: Vec<TrackLifecycleOperation>,
}

struct TrackLifecycleStager {
    generation: u64,
    operations: Vec<TrackLifecycleOperation>,
}

impl TrackLifecycleStager {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            operations: Vec::new(),
        }
    }

    fn update(&mut self, shard: ShardId, op: ShardUpdateOp) {
        self.operations
            .push(TrackLifecycleOperation::Update { shard, op });
    }

    fn plans(&mut self, shard: ShardId, plans: Vec<TrackPlanUpdate>) {
        if !plans.is_empty() {
            self.operations
                .push(TrackLifecycleOperation::Plans { shard, plans });
        }
    }

    fn plan(&mut self, shard: ShardId, key: TrackKey, plan: Option<TrackPlan>) {
        self.plans(shard, vec![TrackPlanUpdate { key, plan }]);
    }

    fn participant(
        &mut self,
        shard: ShardId,
        participant: ParticipantKey,
        effect: ParticipantEffect,
    ) {
        self.operations
            .push(TrackLifecycleOperation::ParticipantEffect {
                shard,
                participant,
                effect,
            });
    }

    fn finish(self) -> TrackLifecycleOutcome {
        TrackLifecycleOutcome {
            generation: self.generation,
            operations: self.operations,
        }
    }
}

struct DesiredTrackState {
    track: Track,
    origin: BoundParticipantLocation,
    candidates: HashSet<ParticipantId>,
    bindings: HashSet<ParticipantId>,
    locations: HashMap<ParticipantId, BoundParticipantLocation>,
}

impl DesiredTrackState {
    fn candidate_remote_shards(&self) -> Vec<ShardId> {
        let mut shards = self
            .candidates
            .iter()
            .filter_map(|participant| self.locations.get(participant))
            .map(|location| location.shard)
            .filter(|shard| *shard != self.origin.shard)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        shards.sort_by_key(|shard| shard.index());
        shards
    }

    fn active_remote_shards(&self) -> Vec<ShardId> {
        let mut shards = self
            .bindings
            .iter()
            .filter_map(|participant| self.locations.get(participant))
            .map(|location| location.shard)
            .filter(|shard| *shard != self.origin.shard)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        shards.sort_by_key(|shard| shard.index());
        shards
    }

    #[allow(
        clippy::expect_used,
        reason = "bindings are filtered from the same location map they are read from"
    )]
    fn local_bindings(&self, shard: ShardId) -> Vec<ParticipantKey> {
        let mut participants = self
            .bindings
            .iter()
            .copied()
            .filter(|participant| {
                self.locations
                    .get(participant)
                    .is_some_and(|location| location.shard == shard)
            })
            .collect::<Vec<_>>();
        participants.sort();
        participants
            .into_iter()
            .map(|participant| {
                self.locations
                    .get(&participant)
                    .expect("sorted binding must retain its location")
                    .key
            })
            .collect()
    }
}

struct RuntimePlan<'a> {
    shard: ShardId,
    origin_key: ParticipantKey,
    track: &'a Track,
    key: TrackKey,
    local: Vec<ParticipantKey>,
    remote: Vec<RouteHandle>,
    reverse: Option<RouteHandle>,
}

pub(crate) struct TrackLifecycle {
    topology: TrackTopology,
    allocator: TrackAllocator,
    allocations: HashMap<(TrackIdentity, ShardId), TrackDestination>,
    candidates: HashMap<TrackIdentity, HashSet<ParticipantId>>,
    bindings: HashMap<TrackIdentity, HashSet<ParticipantId>>,
    generation: u64,
}

impl TrackLifecycle {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            topology: TrackTopology::default(),
            allocator: TrackAllocator::new(shard_count),
            allocations: HashMap::new(),
            candidates: HashMap::new(),
            bindings: HashMap::new(),
            generation: 0,
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "generation exhaustion is fatal because update order must stay monotonic"
    )]
    pub(crate) fn next_generation(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("track lifecycle generation exhausted");
        self.generation
    }

    pub(crate) fn publish(
        &mut self,
        track: Track,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Option<TrackLifecycleOutcome> {
        let identity = self.topology.publish(track)?;
        Some(self.reconcile(identity, registry, now))
    }

    pub(crate) fn unpublish(
        &mut self,
        origin: ParticipantId,
        track_id: TrackId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Option<TrackLifecycleOutcome> {
        let room_id = ParticipantLocation::lookup(registry, &origin)?.room_id;
        let identity = TrackIdentity {
            room_id,
            publisher: origin,
            id: track_id,
        };

        if !self.topology.contains(identity) {
            return None;
        }

        let mut stager = TrackLifecycleStager::new(self.next_generation());
        self.stage_track_removal(identity, registry, &mut stager);

        let removed = self.topology.unpublish(identity);
        debug_assert!(
            removed.is_some(),
            "published track disappeared during unpublish"
        );

        self.release_allocations(identity, now);
        Some(stager.finish())
    }

    pub(crate) fn activate(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        track_id: TrackId,
        subscriber: ParticipantId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let identity = TrackIdentity {
            room_id,
            publisher,
            id: track_id,
        };
        let _ = self.topology.activate(identity, subscriber);
        self.reconcile(identity, registry, now)
    }

    pub(crate) fn deactivate(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        track_id: TrackId,
        subscriber: ParticipantId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let identity = TrackIdentity {
            room_id,
            publisher,
            id: track_id,
        };
        let _ = self.topology.deactivate(identity, subscriber);
        self.reconcile(identity, registry, now)
    }

    pub(crate) fn subscribe(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        selector: TrackSelector,
        selection: SelectionPolicy,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let identities = self.topology.matching_tracks(room_id, &selector);
        let _ = self
            .topology
            .subscribe(room_id, subscriber, selector, selection);
        self.reconcile_all(identities, registry, now)
    }

    pub(crate) fn unsubscribe(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        selector: TrackSelector,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let identities = self.topology.matching_tracks(room_id, &selector);
        if self
            .topology
            .remove_matching(room_id, subscriber, selector)
            .is_none()
        {
            return Vec::new();
        }
        self.reconcile_all(identities, registry, now)
    }

    pub(crate) fn subscribe_defaults(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        subscriptions: impl IntoIterator<Item = (TrackSelector, SelectionPolicy)>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        for (selector, selection) in subscriptions {
            let _ = self
                .topology
                .subscribe(room_id, subscriber, selector, selection);
        }

        let identities = self
            .topology
            .identities()
            .filter(|identity| identity.room_id == room_id)
            .collect::<Vec<_>>();
        self.reconcile_all(identities, registry, now)
    }

    pub(crate) fn remove_participant(
        &mut self,
        participant: ParticipantId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let mut affected = self
            .topology
            .identities()
            .filter(|identity| {
                identity.publisher == participant
                    || self
                        .topology
                        .candidate_subscribers(*identity)
                        .any(|subscriber| subscriber == participant)
            })
            .collect::<Vec<_>>();

        let retiring = affected
            .iter()
            .copied()
            .filter(|identity| identity.publisher == participant)
            .collect::<Vec<_>>();

        let mut outcomes = Vec::new();
        if !retiring.is_empty() {
            let mut stager = TrackLifecycleStager::new(self.next_generation());
            for identity in retiring.iter().copied() {
                self.stage_track_removal(identity, registry, &mut stager);
            }
            outcomes.push(stager.finish());
        }

        self.topology.remove_participant(participant);

        for identity in retiring {
            debug_assert!(
                !self.topology.contains(identity),
                "participant removal must remove every published track"
            );
            self.release_allocations(identity, now);
        }

        affected.retain(|identity| self.topology.contains(*identity));
        outcomes.extend(self.reconcile_all(affected, registry, now));
        outcomes
    }

    fn reconcile_all(
        &mut self,
        identities: impl IntoIterator<Item = TrackIdentity>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        identities
            .into_iter()
            .map(|identity| self.reconcile(identity, registry, now))
            .collect()
    }

    fn reconcile(
        &mut self,
        identity: TrackIdentity,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let mut stager = TrackLifecycleStager::new(self.next_generation());
        let mut desired = self.desired_state(identity, registry);

        let origin = self.ensure_destination(identity, desired.origin.shard, now);
        self.ensure_reverse_route(identity, &mut desired.track, origin.allocation.route);
        self.reserve_candidate_destinations(identity, &desired, now);

        self.stage_participant_transitions(identity, &desired, registry, &mut stager);
        self.reconcile_remote_residency(identity, &desired, &mut stager);
        self.stage_active_views(identity, &desired, &mut stager);
        self.release_unused_destinations(identity, &desired, now);

        self.candidates.insert(identity, desired.candidates);
        self.bindings.insert(identity, desired.bindings);
        stager.finish()
    }

    #[allow(
        clippy::expect_used,
        reason = "reconciliation only runs for a published track whose publisher remains bound"
    )]
    fn desired_state(&self, identity: TrackIdentity, registry: &RoomRegistry) -> DesiredTrackState {
        let track = self
            .topology
            .track(identity)
            .expect("reconcile must target a published track")
            .clone();
        let origin =
            ParticipantLocation::bound_in_room(registry, &identity.publisher, identity.room_id)
                .expect("a published track must have a bound live publisher in its room");

        let mut locations = HashMap::new();
        let candidates = self
            .topology
            .candidate_subscribers(identity)
            .filter(|participant| *participant != identity.publisher)
            .filter_map(|participant| {
                let location =
                    ParticipantLocation::bound_in_room(registry, &participant, identity.room_id)?;
                locations.insert(participant, location);
                Some(participant)
            })
            .collect::<HashSet<_>>();

        let bindings = self
            .topology
            .active_subscribers(identity)
            .filter(|participant| candidates.contains(participant))
            .collect::<HashSet<_>>();

        DesiredTrackState {
            track,
            origin,
            candidates,
            bindings,
            locations,
        }
    }

    fn ensure_destination(
        &mut self,
        identity: TrackIdentity,
        shard: ShardId,
        now: Instant,
    ) -> TrackDestination {
        if let Some(destination) = self.allocations.get(&(identity, shard)).copied() {
            return destination;
        }

        // Track allocation is an internal capacity invariant. Supported shard
        // capacity must be lower than the route/key space, so exhaustion is not
        // a recoverable lifecycle state and is asserted inside TrackAllocator.
        let allocation = self.allocator.allocate(shard, identity, now);
        let destination = TrackDestination::reserved(allocation);
        self.allocations.insert((identity, shard), destination);
        destination
    }

    #[allow(
        clippy::expect_used,
        reason = "the desired track is cloned from the topology entry updated here"
    )]
    fn ensure_reverse_route(
        &mut self,
        identity: TrackIdentity,
        desired_track: &mut Track,
        route: RouteHandle,
    ) {
        if !desired_track.requires_reverse_route() || desired_track.reverse().is_some() {
            return;
        }

        desired_track.set_reverse(Some(route));
        self.topology
            .track_mut(identity)
            .expect("published track disappeared while installing its reverse route")
            .set_reverse(Some(route));
    }

    fn reserve_candidate_destinations(
        &mut self,
        identity: TrackIdentity,
        desired: &DesiredTrackState,
        now: Instant,
    ) {
        for shard in desired.candidate_remote_shards() {
            let _ = self.ensure_destination(identity, shard, now);
        }
    }

    fn stage_participant_transitions(
        &self,
        identity: TrackIdentity,
        desired: &DesiredTrackState,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let previous_candidates = self.candidates.get(&identity).cloned().unwrap_or_default();
        let previous_bindings = self.bindings.get(&identity).cloned().unwrap_or_default();

        // Remove active state before removing candidate state. Sort participant
        // transitions so one semantic generation always compiles to one operation order.
        let mut removed_bindings = previous_bindings
            .difference(&desired.bindings)
            .copied()
            .collect::<Vec<_>>();
        removed_bindings.sort();
        for participant in removed_bindings {
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackUnsubscribed {
                    key,
                    track_id: identity.id,
                }
            });
        }

        let mut removed_candidates = previous_candidates
            .difference(&desired.candidates)
            .copied()
            .collect::<Vec<_>>();
        removed_candidates.sort();
        for participant in removed_candidates {
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackCandidateRemoved {
                    key,
                    track_id: identity.id,
                }
            });
        }

        // Establish candidate state before activating it.
        let mut added_candidates = desired
            .candidates
            .difference(&previous_candidates)
            .copied()
            .collect::<Vec<_>>();
        added_candidates.sort();
        for participant in added_candidates {
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackCandidateAdded {
                    key,
                    track: desired.track.clone(),
                }
            });
        }

        let mut added_bindings = desired
            .bindings
            .difference(&previous_bindings)
            .copied()
            .collect::<Vec<_>>();
        added_bindings.sort();
        for participant in added_bindings {
            debug_assert!(desired.candidates.contains(&participant));
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackSubscribed {
                    key,
                    track_id: identity.id,
                }
            });
        }
    }

    fn reconcile_remote_residency(
        &mut self,
        identity: TrackIdentity,
        desired: &DesiredTrackState,
        stager: &mut TrackLifecycleStager,
    ) {
        let active_shards = desired.active_remote_shards();
        let destinations = self.remote_destinations(identity, desired.origin.shard);

        for (shard, destination) in destinations {
            let should_be_resident = active_shards.contains(&shard);
            match (destination.resident, should_be_resident) {
                (false, true) => {
                    debug_assert!(!destination.route_installed);
                    stager.update(
                        shard,
                        ShardUpdateOp::InstallRoute {
                            handle: destination.allocation.route,
                            action: RouteAction::Forward {
                                target: destination.allocation.key,
                            },
                        },
                    );
                    self.set_route_installed(identity, shard, true);
                }
                (true, false) => {
                    debug_assert!(destination.route_installed);
                    self.stage_shard_withdrawal(shard, destination, stager);
                    self.set_destination_state(identity, shard, false, false);
                }
                (true, true) => {
                    debug_assert!(destination.route_installed);
                }
                (false, false) => {
                    debug_assert!(!destination.route_installed);
                }
            }
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "active views are staged only after their origin and remote allocations are reserved"
    )]
    fn stage_active_views(
        &mut self,
        identity: TrackIdentity,
        desired: &DesiredTrackState,
        stager: &mut TrackLifecycleStager,
    ) {
        let origin = self
            .allocations
            .get(&(identity, desired.origin.shard))
            .copied()
            .expect("origin allocation must exist before staging its track view");
        let origin_was_resident = origin.resident;

        if desired.track.requires_reverse_route() {
            if !origin.route_installed {
                stager.update(
                    desired.origin.shard,
                    ShardUpdateOp::InstallRoute {
                        handle: origin.allocation.route,
                        action: RouteAction::Reverse {
                            target: origin.allocation.key,
                        },
                    },
                );
                self.set_route_installed(identity, desired.origin.shard, true);
            }
        } else {
            debug_assert!(
                !origin.route_installed,
                "origin route must only exist for tracks with a reverse path"
            );
        }

        let remote = desired
            .active_remote_shards()
            .into_iter()
            .map(|shard| {
                self.allocations
                    .get(&(identity, shard))
                    .expect("active remote shard must have a reserved allocation")
                    .allocation
                    .route
            })
            .collect();

        self.stage_runtime_and_plan(
            stager,
            RuntimePlan {
                shard: desired.origin.shard,
                origin_key: desired.origin.key,
                track: &desired.track,
                key: origin.allocation.key,
                local: desired.local_bindings(desired.origin.shard),
                remote,
                reverse: desired.track.reverse(),
            },
        );
        self.set_resident(identity, desired.origin.shard, true);

        if !origin_was_resident {
            stager.participant(
                desired.origin.shard,
                desired.origin.key,
                ParticipantEffect::TrackPublished {
                    key: origin.allocation.key,
                    track_id: identity.id,
                },
            );
        }

        for shard in desired.active_remote_shards() {
            let destination = self
                .allocations
                .get(&(identity, shard))
                .copied()
                .expect("active remote shard must have a reserved allocation");
            debug_assert!(destination.route_installed);

            self.stage_runtime_and_plan(
                stager,
                RuntimePlan {
                    shard,
                    origin_key: desired.origin.key,
                    track: &desired.track,
                    key: destination.allocation.key,
                    local: desired.local_bindings(shard),
                    remote: Vec::new(),
                    reverse: desired.track.reverse(),
                },
            );
            self.set_resident(identity, shard, true);
        }
    }

    fn stage_runtime_and_plan(&self, stager: &mut TrackLifecycleStager, runtime: RuntimePlan<'_>) {
        stager.update(
            runtime.shard,
            ShardUpdateOp::InsertTrackRuntime {
                key: runtime.key,
                runtime: TrackRuntime {
                    descriptor: Some(TrackDescriptor {
                        origin_key: runtime.origin_key,
                        kind: runtime.track.kind(),
                        encodings: runtime
                            .track
                            .layers()
                            .iter()
                            .map(|layer| layer.rid)
                            .collect(),
                    }),
                    ..Default::default()
                },
            },
        );

        // Keep the generation/update-stream marker between preparing the runtime
        // and publishing the forwarding plan for that shard.
        stager.update(runtime.shard, ShardUpdateOp::Placeholder);
        stager.plan(
            runtime.shard,
            runtime.key,
            Some(TrackPlan::new(
                runtime.local,
                runtime.remote,
                runtime.reverse,
            )),
        );
    }

    fn stage_shard_withdrawal(
        &self,
        shard: ShardId,
        destination: TrackDestination,
        stager: &mut TrackLifecycleStager,
    ) {
        debug_assert!(destination.resident);

        // Stop publishing the view before retiring anything it references.
        stager.plan(shard, destination.allocation.key, None);
        if destination.route_installed {
            stager.update(
                shard,
                ShardUpdateOp::RetireRoute {
                    handle: destination.allocation.route,
                },
            );
        }
        stager.update(
            shard,
            ShardUpdateOp::RemoveTrackRuntime {
                key: destination.allocation.key,
            },
        );
    }

    #[allow(
        clippy::expect_used,
        reason = "a live bound participant has a destination reserved during reconciliation"
    )]
    fn stage_track_effect(
        &self,
        identity: TrackIdentity,
        participant: ParticipantId,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
        effect: impl FnOnce(TrackKey) -> ParticipantEffect,
    ) {
        let Some(location) = ParticipantLocation::lookup(registry, &participant) else {
            // The participant may already have been removed from the registry;
            // there is no local participant state left to update in that case.
            return;
        };
        let Some(participant_key) = location.binding else {
            return;
        };
        let destination = self
            .allocations
            .get(&(identity, location.shard))
            .expect("a live lifecycle participant must have a shard-local track allocation");

        stager.participant(
            location.shard,
            participant_key,
            effect(destination.allocation.key),
        );
    }

    fn stage_track_removal(
        &mut self,
        identity: TrackIdentity,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let bindings = self.bindings.remove(&identity).unwrap_or_default();
        let candidates = self.candidates.remove(&identity).unwrap_or_default();

        let mut bindings = bindings.into_iter().collect::<Vec<_>>();
        bindings.sort();
        for participant in bindings {
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackUnsubscribed {
                    key,
                    track_id: identity.id,
                }
            });
        }

        let mut candidates = candidates.into_iter().collect::<Vec<_>>();
        candidates.sort();
        for participant in candidates {
            self.stage_track_effect(identity, participant, registry, stager, |key| {
                ParticipantEffect::TrackCandidateRemoved {
                    key,
                    track_id: identity.id,
                }
            });
        }

        if let Some(origin) = ParticipantLocation::lookup(registry, &identity.publisher)
            && let Some(participant) = origin.binding
            && let Some(destination) = self.allocations.get(&(identity, origin.shard))
            && destination.resident
        {
            stager.participant(
                origin.shard,
                participant,
                ParticipantEffect::TrackUnpublished {
                    key: destination.allocation.key,
                    track_id: identity.id,
                },
            );
        }

        let mut allocations = self
            .allocations
            .iter()
            .filter_map(|((held, shard), destination)| {
                (*held == identity).then_some((*shard, *destination))
            })
            .collect::<Vec<_>>();
        allocations.sort_by_key(|(shard, _)| shard.index());

        for (shard, destination) in allocations {
            if destination.resident {
                self.stage_shard_withdrawal(shard, destination, stager);
            } else {
                debug_assert!(
                    !destination.route_installed,
                    "reserved non-resident destination must not have an installed route"
                );
            }
        }
    }

    fn release_unused_destinations(
        &mut self,
        identity: TrackIdentity,
        desired: &DesiredTrackState,
        now: Instant,
    ) {
        let candidate_shards = desired.candidate_remote_shards();
        let unused = self
            .remote_destinations(identity, desired.origin.shard)
            .into_iter()
            .filter(|(shard, _)| !candidate_shards.contains(shard))
            .collect::<Vec<_>>();

        for (shard, destination) in unused {
            debug_assert!(!destination.resident);
            debug_assert!(!destination.route_installed);
            let removed = self.allocations.remove(&(identity, shard));
            debug_assert!(removed.is_some());
            self.allocator.release(destination.allocation, now);
        }
    }

    fn release_allocations(&mut self, identity: TrackIdentity, now: Instant) {
        let allocations = self
            .allocations
            .extract_if(|(held, _), _| *held == identity)
            .map(|(_, destination)| destination.allocation)
            .collect::<Vec<_>>();

        for allocation in allocations {
            self.allocator.release(allocation, now);
        }

        self.candidates.remove(&identity);
        self.bindings.remove(&identity);
    }

    fn remote_destinations(
        &self,
        identity: TrackIdentity,
        origin_shard: ShardId,
    ) -> Vec<(ShardId, TrackDestination)> {
        let mut destinations = self
            .allocations
            .iter()
            .filter_map(|((held, shard), destination)| {
                (*held == identity && *shard != origin_shard).then_some((*shard, *destination))
            })
            .collect::<Vec<_>>();
        destinations.sort_by_key(|(shard, _)| shard.index());
        destinations
    }

    #[allow(
        clippy::expect_used,
        reason = "route state is changed only for an allocation reserved by this lifecycle"
    )]
    fn set_route_installed(&mut self, identity: TrackIdentity, shard: ShardId, installed: bool) {
        let destination = self
            .allocations
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.route_installed = installed;
    }

    #[allow(
        clippy::expect_used,
        reason = "residency is changed only for an allocation reserved by this lifecycle"
    )]
    fn set_resident(&mut self, identity: TrackIdentity, shard: ShardId, resident: bool) {
        let destination = self
            .allocations
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.resident = resident;
    }

    #[allow(
        clippy::expect_used,
        reason = "destination state is changed only for an allocation reserved by this lifecycle"
    )]
    fn set_destination_state(
        &mut self,
        identity: TrackIdentity,
        shard: ShardId,
        resident: bool,
        route_installed: bool,
    ) {
        let destination = self
            .allocations
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.resident = resident;
        destination.route_installed = route_installed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::KeyData;

    use crate::{
        entity::{ExternalRoomId, TrackKind},
        track::{DataLane, Topic, TrackMeta},
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EffectKind {
        CandidateAdded,
        CandidateRemoved,
        Subscribed,
        Unsubscribed,
        Published,
        Unpublished,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum OpKind {
        CandidateAdded,
        CandidateRemoved,
        Subscribed,
        Unsubscribed,
        Published,
        Unpublished,
        InstallForward,
        InstallReverse,
        RetireRoute,
        InsertRuntime,
        RemoveRuntime,
        Placeholder,
        PlanSet,
        PlanClear,
    }

    fn room(seed: u8) -> RoomId {
        let name = match seed {
            1 => "lifecycle-1",
            2 => "lifecycle-2",
            _ => "lifecycle-other",
        };
        RoomId::from_external(&ExternalRoomId::new(name).unwrap())
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn participant_key(seed: u8) -> ParticipantKey {
        ParticipantKey::from(KeyData::from_ffi((1_u64 << 32) | u64::from(seed)))
    }

    fn register(registry: &mut RoomRegistry, seed: u8, room_id: RoomId, shard: usize) {
        let participant = participant(seed);
        registry.add_participant(participant, room_id, ShardId::new(shard), None);
        registry.bind_participant(&participant, participant_key(seed));
    }

    fn track(kind: TrackKind, publisher: u8, room_seed: u8, label: &str) -> Track {
        let publisher = participant(publisher);
        let meta = TrackMeta {
            room_id: room(room_seed),
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(kind, label),
            origin: publisher,
        };
        match kind {
            TrackKind::Data => {
                let (lane, topic) = label
                    .strip_prefix("rel:")
                    .map_or((DataLane::Realtime, label), |topic| {
                        (DataLane::Reliable, topic)
                    });
                Track::data(meta, Topic::for_test(topic), lane, None)
            }
            TrackKind::Audio => Track::audio(meta, None),
            TrackKind::Video => Track::video(meta, Vec::new(), None),
        }
    }

    fn setup(shards: usize, participants: &[(u8, u8, usize)]) -> (TrackLifecycle, RoomRegistry) {
        let mut registry = RoomRegistry::new();
        for (seed, room_seed, shard) in participants {
            register(&mut registry, *seed, room(*room_seed), *shard);
        }
        (TrackLifecycle::new(shards), registry)
    }

    fn effect_kind(effect: &ParticipantEffect) -> Option<EffectKind> {
        match effect {
            ParticipantEffect::TrackCandidateAdded { .. } => Some(EffectKind::CandidateAdded),
            ParticipantEffect::TrackCandidateRemoved { .. } => Some(EffectKind::CandidateRemoved),
            ParticipantEffect::TrackSubscribed { .. } => Some(EffectKind::Subscribed),
            ParticipantEffect::TrackUnsubscribed { .. } => Some(EffectKind::Unsubscribed),
            ParticipantEffect::TrackPublished { .. } => Some(EffectKind::Published),
            ParticipantEffect::TrackUnpublished { .. } => Some(EffectKind::Unpublished),
            _ => None,
        }
    }

    fn effect_kinds(outcome: &TrackLifecycleOutcome) -> Vec<EffectKind> {
        outcome
            .operations
            .iter()
            .filter_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect { effect, .. } => effect_kind(effect),
                _ => None,
            })
            .collect()
    }

    fn op_kinds(outcome: &TrackLifecycleOutcome) -> Vec<OpKind> {
        let mut kinds = Vec::new();
        for operation in &outcome.operations {
            match operation {
                TrackLifecycleOperation::ParticipantEffect { effect, .. } => {
                    if let Some(kind) = effect_kind(effect) {
                        kinds.push(match kind {
                            EffectKind::CandidateAdded => OpKind::CandidateAdded,
                            EffectKind::CandidateRemoved => OpKind::CandidateRemoved,
                            EffectKind::Subscribed => OpKind::Subscribed,
                            EffectKind::Unsubscribed => OpKind::Unsubscribed,
                            EffectKind::Published => OpKind::Published,
                            EffectKind::Unpublished => OpKind::Unpublished,
                        });
                    }
                }
                TrackLifecycleOperation::Plans { plans, .. } => {
                    for update in plans {
                        kinds.push(if update.plan.is_some() {
                            OpKind::PlanSet
                        } else {
                            OpKind::PlanClear
                        });
                    }
                }
                TrackLifecycleOperation::Update { op, .. } => match op {
                    ShardUpdateOp::InstallRoute { action, .. } => match action {
                        RouteAction::Forward { .. } => kinds.push(OpKind::InstallForward),
                        RouteAction::Reverse { .. } => kinds.push(OpKind::InstallReverse),
                    },
                    ShardUpdateOp::RetireRoute { .. } => kinds.push(OpKind::RetireRoute),
                    ShardUpdateOp::InsertTrackRuntime { .. } => kinds.push(OpKind::InsertRuntime),
                    ShardUpdateOp::RemoveTrackRuntime { .. } => kinds.push(OpKind::RemoveRuntime),
                    ShardUpdateOp::Placeholder => kinds.push(OpKind::Placeholder),
                    _ => {}
                },
            }
        }
        kinds
    }

    fn operations_on(outcome: &TrackLifecycleOutcome, shard: usize) -> Vec<OpKind> {
        let shard = ShardId::new(shard);
        let mut kinds = Vec::new();
        for operation in &outcome.operations {
            match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    shard: held,
                    effect,
                    ..
                } if *held == shard => {
                    if let Some(kind) = effect_kind(effect) {
                        kinds.push(match kind {
                            EffectKind::CandidateAdded => OpKind::CandidateAdded,
                            EffectKind::CandidateRemoved => OpKind::CandidateRemoved,
                            EffectKind::Subscribed => OpKind::Subscribed,
                            EffectKind::Unsubscribed => OpKind::Unsubscribed,
                            EffectKind::Published => OpKind::Published,
                            EffectKind::Unpublished => OpKind::Unpublished,
                        });
                    }
                }
                TrackLifecycleOperation::Plans { shard: held, plans } if *held == shard => {
                    for update in plans {
                        kinds.push(if update.plan.is_some() {
                            OpKind::PlanSet
                        } else {
                            OpKind::PlanClear
                        });
                    }
                }
                TrackLifecycleOperation::Update { shard: held, op } if *held == shard => match op {
                    ShardUpdateOp::InstallRoute { action, .. } => match action {
                        RouteAction::Forward { .. } => kinds.push(OpKind::InstallForward),
                        RouteAction::Reverse { .. } => kinds.push(OpKind::InstallReverse),
                    },
                    ShardUpdateOp::RetireRoute { .. } => kinds.push(OpKind::RetireRoute),
                    ShardUpdateOp::InsertTrackRuntime { .. } => kinds.push(OpKind::InsertRuntime),
                    ShardUpdateOp::RemoveTrackRuntime { .. } => kinds.push(OpKind::RemoveRuntime),
                    ShardUpdateOp::Placeholder => kinds.push(OpKind::Placeholder),
                    _ => {}
                },
                _ => {}
            }
        }
        kinds
    }

    fn plan_updates_on(outcome: &TrackLifecycleOutcome, shard: usize) -> Vec<(TrackKey, bool)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .filter_map(|operation| match operation {
                TrackLifecycleOperation::Plans { shard: held, plans } if *held == shard => Some(
                    plans
                        .iter()
                        .map(|update| (update.key, update.plan.is_some())),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn forward_install_on(
        outcome: &TrackLifecycleOutcome,
        shard: usize,
    ) -> Option<(RouteHandle, TrackKey)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::Update {
                    shard: held,
                    op:
                        ShardUpdateOp::InstallRoute {
                            handle,
                            action: RouteAction::Forward { target },
                        },
                } if *held == shard => Some((*handle, *target)),
                _ => None,
            })
    }

    fn reverse_install_on(
        outcome: &TrackLifecycleOutcome,
        shard: usize,
    ) -> Option<(RouteHandle, TrackKey)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::Update {
                    shard: held,
                    op:
                        ShardUpdateOp::InstallRoute {
                            handle,
                            action: RouteAction::Reverse { target },
                        },
                } if *held == shard => Some((*handle, *target)),
                _ => None,
            })
    }

    fn published_key(outcome: &TrackLifecycleOutcome) -> Option<TrackKey> {
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    effect: ParticipantEffect::TrackPublished { key, .. },
                    ..
                } => Some(*key),
                _ => None,
            })
    }

    fn destination(
        lifecycle: &TrackLifecycle,
        identity: TrackIdentity,
        shard: usize,
    ) -> TrackDestination {
        *lifecycle
            .allocations
            .get(&(identity, ShardId::new(shard)))
            .expect("destination allocation must exist")
    }

    fn assert_internal_invariants(lifecycle: &TrackLifecycle, registry: &RoomRegistry) {
        for (identity, bindings) in &lifecycle.bindings {
            let candidates = lifecycle
                .candidates
                .get(identity)
                .expect("binding state must have candidate state");
            assert!(
                bindings.is_subset(candidates),
                "bindings must be candidates"
            );
        }

        for (identity, candidates) in &lifecycle.candidates {
            for participant in candidates {
                let location =
                    ParticipantLocation::bound_in_room(registry, participant, identity.room_id)
                        .expect("retained candidate must remain bound in the track room");
                assert!(
                    lifecycle
                        .allocations
                        .contains_key(&(*identity, location.shard)),
                    "every retained candidate shard must have an allocation"
                );
            }
        }

        for ((identity, shard), destination) in &lifecycle.allocations {
            assert!(
                lifecycle.topology.contains(*identity),
                "allocations must only be retained for published tracks"
            );
            if destination.route_installed {
                assert!(
                    destination.resident,
                    "installed routes must have a resident runtime"
                );
            }
            let origin =
                ParticipantLocation::bound_in_room(registry, &identity.publisher, identity.room_id)
                    .expect("published track must retain a bound publisher");
            if *shard == origin.shard {
                let track = lifecycle
                    .topology
                    .track(*identity)
                    .expect("allocation identity must still resolve to its track");
                assert_eq!(
                    destination.route_installed,
                    track.requires_reverse_route(),
                    "origin route residency must exactly match reverse-route semantics"
                );
            } else if destination.resident {
                assert!(
                    destination.route_installed,
                    "remote resident runtime must have its forward route installed"
                );
            }
        }

        for identity in lifecycle.topology.identities() {
            let origin =
                ParticipantLocation::bound_in_room(registry, &identity.publisher, identity.room_id)
                    .expect("published track must retain a bound publisher");
            let destination = lifecycle
                .allocations
                .get(&(identity, origin.shard))
                .expect("published track must retain its origin allocation");
            assert!(destination.resident, "origin runtime must remain resident");
        }
    }

    fn publish_audio(
        lifecycle: &mut TrackLifecycle,
        registry: &RoomRegistry,
        publisher: u8,
        room_seed: u8,
    ) -> (TrackIdentity, TrackLifecycleOutcome) {
        let track = track(TrackKind::Audio, publisher, room_seed, "audio");
        let identity = TrackIdentity::from_track(&track);
        let outcome = lifecycle
            .publish(track, registry, Instant::now())
            .expect("publication must be new");
        (identity, outcome)
    }

    #[test]
    fn generations_are_strictly_monotonic() {
        let mut lifecycle = TrackLifecycle::new(1);
        assert_eq!(lifecycle.next_generation(), 1);
        assert_eq!(lifecycle.next_generation(), 2);
        assert_eq!(lifecycle.next_generation(), 3);
    }

    #[test]
    #[should_panic(expected = "track lifecycle generation exhausted")]
    fn generation_exhaustion_is_fatal() {
        let mut lifecycle = TrackLifecycle::new(1);
        lifecycle.generation = u64::MAX;
        let _ = lifecycle.next_generation();
    }

    #[test]
    fn stager_does_not_emit_empty_plan_batches() {
        let mut stager = TrackLifecycleStager::new(7);
        stager.plans(ShardId::new(0), Vec::new());
        assert!(stager.finish().operations.is_empty());
    }

    #[test]
    fn publish_without_subscribers_installs_only_the_origin_view() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0)]);
        let (identity, outcome) = publish_audio(&mut lifecycle, &registry, 1, 1);

        assert_eq!(effect_kinds(&outcome), vec![EffectKind::Published]);
        assert_eq!(lifecycle.allocations.len(), 1);
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());

        let origin = destination(&lifecycle, identity, 0);
        assert!(origin.resident);
        assert_eq!(published_key(&outcome), Some(origin.allocation.key));
        assert!(operations_on(&outcome, 0).contains(&OpKind::InsertRuntime));
        assert!(operations_on(&outcome, 0).contains(&OpKind::PlanSet));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn subscription_declared_before_publication_is_applied_on_publish() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let outcomes = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        assert!(outcomes.is_empty());

        let (identity, outcome) = publish_audio(&mut lifecycle, &registry, 1, 1);
        assert_eq!(
            effect_kinds(&outcome),
            vec![
                EffectKind::CandidateAdded,
                EffectKind::Subscribed,
                EffectKind::Published,
            ]
        );
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(lifecycle.bindings[&identity].contains(&participant(2)));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn automatic_local_subscription_adds_candidate_before_binding() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert_eq!(
            effect_kinds(&outcome),
            vec![EffectKind::CandidateAdded, EffectKind::Subscribed]
        );
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(lifecycle.bindings[&identity].contains(&participant(2)));
        assert!(forward_install_on(&outcome, 0).is_none());
        assert_eq!(lifecycle.allocations.len(), 1);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn allocated_local_subscription_stays_candidate_until_activation() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let subscribe = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::Allocated,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert_eq!(effect_kinds(&subscribe), vec![EffectKind::CandidateAdded]);
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(!lifecycle.bindings[&identity].contains(&participant(2)));

        let activate = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&activate), vec![EffectKind::Subscribed]);
        assert!(lifecycle.bindings[&identity].contains(&participant(2)));

        let deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&deactivate), vec![EffectKind::Unsubscribed]);
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(!lifecycle.bindings[&identity].contains(&participant(2)));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn remote_candidate_reserves_a_key_without_installing_a_route_or_runtime() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::Allocated,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert_eq!(effect_kinds(&outcome), vec![EffectKind::CandidateAdded]);
        let remote = destination(&lifecycle, identity, 1);
        assert!(!remote.resident);
        assert!(!remote.route_installed);
        assert!(forward_install_on(&outcome, 1).is_none());
        assert!(!operations_on(&outcome, 1).contains(&OpKind::InsertRuntime));
        assert_eq!(lifecycle.allocations.len(), 2);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn remote_activation_installs_forward_route_before_publishing_remote_plan() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );

        let outcome = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );

        let remote = destination(&lifecycle, identity, 1);
        assert!(remote.resident);
        assert!(remote.route_installed);
        assert_eq!(
            forward_install_on(&outcome, 1),
            Some((remote.allocation.route, remote.allocation.key))
        );
        assert_eq!(
            operations_on(&outcome, 1),
            vec![
                OpKind::Subscribed,
                OpKind::InstallForward,
                OpKind::InsertRuntime,
                OpKind::Placeholder,
                OpKind::PlanSet,
            ]
        );
        assert_eq!(
            plan_updates_on(&outcome, 1),
            vec![(remote.allocation.key, true)]
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn remote_deactivation_withdraws_view_but_keeps_candidate_allocation() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );
        let _ = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        let held = destination(&lifecycle, identity, 1).allocation;

        let outcome = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );

        let remote = destination(&lifecycle, identity, 1);
        assert_eq!(remote.allocation, held);
        assert!(!remote.resident);
        assert!(!remote.route_installed);
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(!lifecycle.bindings[&identity].contains(&participant(2)));
        assert_eq!(
            operations_on(&outcome, 1),
            vec![
                OpKind::Unsubscribed,
                OpKind::PlanClear,
                OpKind::RetireRoute,
                OpKind::RemoveRuntime,
            ]
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn removing_last_remote_candidate_releases_its_reserved_allocation() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );
        let remote_key = destination(&lifecycle, identity, 1).allocation.key;

        let outcome = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert_eq!(effect_kinds(&outcome), vec![EffectKind::CandidateRemoved]);
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(!plan_updates_on(&outcome, 1).contains(&(remote_key, false)));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn unsubscribing_active_remote_binding_orders_effects_before_teardown() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        let outcome = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert_eq!(
            operations_on(&outcome, 1),
            vec![
                OpKind::Unsubscribed,
                OpKind::CandidateRemoved,
                OpKind::PlanClear,
                OpKind::RetireRoute,
                OpKind::RemoveRuntime,
            ]
        );
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn two_remote_subscribers_on_one_shard_share_one_destination() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1), (3, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let first = destination(&lifecycle, identity, 1).allocation;
        let _ = lifecycle.subscribe(
            room(1),
            participant(3),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let second = destination(&lifecycle, identity, 1).allocation;

        assert_eq!(first, second);
        assert_eq!(lifecycle.allocations.len(), 2);

        let first_remove = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert!(!operations_on(&first_remove, 1).contains(&OpKind::RetireRoute));
        assert!(destination(&lifecycle, identity, 1).resident);

        let second_remove = lifecycle
            .unsubscribe(
                room(1),
                participant(3),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert!(operations_on(&second_remove, 1).contains(&OpKind::RetireRoute));
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn active_remote_shards_are_independent() {
        let (mut lifecycle, registry) = setup(3, &[(1, 1, 0), (2, 1, 1), (3, 1, 2)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let _ = lifecycle.subscribe(
            room(1),
            participant(3),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        assert!(destination(&lifecycle, identity, 1).resident);
        assert!(destination(&lifecycle, identity, 2).resident);

        let outcome = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert!(operations_on(&outcome, 1).contains(&OpKind::RetireRoute));
        assert!(!operations_on(&outcome, 2).contains(&OpKind::RetireRoute));
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(destination(&lifecycle, identity, 2).resident);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn overlapping_subscriptions_do_not_duplicate_candidate_or_binding_state() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let exact = TrackSelector::track(identity.id);

        let first = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert_eq!(
            effect_kinds(&first),
            vec![EffectKind::CandidateAdded, EffectKind::Subscribed]
        );

        let second = lifecycle
            .subscribe(
                room(1),
                participant(2),
                exact.clone(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert!(effect_kinds(&second).is_empty());

        let remove_exact = lifecycle
            .unsubscribe(room(1), participant(2), exact, &registry, Instant::now())
            .pop()
            .unwrap();
        assert!(effect_kinds(&remove_exact).is_empty());
        assert!(lifecycle.candidates[&identity].contains(&participant(2)));
        assert!(lifecycle.bindings[&identity].contains(&participant(2)));

        let remove_last = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        assert_eq!(
            effect_kinds(&remove_last),
            vec![EffectKind::Unsubscribed, EffectKind::CandidateRemoved]
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn publisher_is_never_its_own_candidate() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(1),
                TrackSelector::audio(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert!(effect_kinds(&outcome).is_empty());
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn subscriber_registered_in_another_room_is_not_compiled_into_the_track() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 2, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert!(effect_kinds(&outcome).is_empty());
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn unbound_subscriber_is_not_compiled_into_the_track() {
        let mut registry = RoomRegistry::new();
        register(&mut registry, 1, room(1), 0);
        registry.add_participant(participant(2), room(1), ShardId::new(1), None);
        let mut lifecycle = TrackLifecycle::new(2);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::All,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();

        assert!(effect_kinds(&outcome).is_empty());
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
    }

    #[test]
    fn reverse_route_is_installed_exactly_when_the_track_requires_one_and_only_once() {
        for (kind, label) in [
            (TrackKind::Audio, "audio"),
            (TrackKind::Video, "video"),
            (TrackKind::Data, "chat"),
            (TrackKind::Data, "rel:chat"),
        ] {
            let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
            let publication = track(kind, 1, 1, label);
            let identity = TrackIdentity::from_track(&publication);
            let requires_reverse = publication.requires_reverse_route();
            let publish = lifecycle
                .publish(publication, &registry, Instant::now())
                .unwrap();
            let origin = destination(&lifecycle, identity, 0);

            assert_eq!(
                reverse_install_on(&publish, 0).is_some(),
                requires_reverse,
                "reverse-route installation must match track semantics for {kind:?}"
            );
            assert_eq!(origin.route_installed, requires_reverse);

            let reconcile = lifecycle
                .subscribe(
                    room(1),
                    participant(2),
                    TrackSelector::track(identity.id),
                    SelectionPolicy::All,
                    &registry,
                    Instant::now(),
                )
                .pop()
                .unwrap();
            assert!(
                reverse_install_on(&reconcile, 0).is_none(),
                "reconciliation must not reinstall an already-installed reverse route"
            );
            assert_internal_invariants(&lifecycle, &registry);
        }
    }

    #[test]
    fn unpublish_with_local_binding_removes_effects_before_origin_view() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let origin = destination(&lifecycle, identity, 0);

        let outcome = lifecycle
            .unpublish(participant(1), identity.id, &registry, Instant::now())
            .unwrap();

        let origin_ops = operations_on(&outcome, 0);
        let unsubscribed = origin_ops
            .iter()
            .position(|kind| *kind == OpKind::Unsubscribed)
            .unwrap();
        let candidate_removed = origin_ops
            .iter()
            .position(|kind| *kind == OpKind::CandidateRemoved)
            .unwrap();
        let unpublished = origin_ops
            .iter()
            .position(|kind| *kind == OpKind::Unpublished)
            .unwrap();
        let plan_clear = origin_ops
            .iter()
            .position(|kind| *kind == OpKind::PlanClear)
            .unwrap();
        let runtime_remove = origin_ops
            .iter()
            .position(|kind| *kind == OpKind::RemoveRuntime)
            .unwrap();

        assert!(unsubscribed < candidate_removed);
        assert!(candidate_removed < unpublished);
        assert!(unpublished < plan_clear);
        assert!(plan_clear < runtime_remove);
        if origin.route_installed {
            let retire = origin_ops
                .iter()
                .position(|kind| *kind == OpKind::RetireRoute)
                .unwrap();
            assert!(plan_clear < retire && retire < runtime_remove);
        }
        assert!(!lifecycle.topology.contains(identity));
        assert!(lifecycle.allocations.is_empty());
        assert!(!lifecycle.candidates.contains_key(&identity));
        assert!(!lifecycle.bindings.contains_key(&identity));
    }

    #[test]
    fn unpublish_with_remote_binding_tears_down_every_resident_destination() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        let outcome = lifecycle
            .unpublish(participant(1), identity.id, &registry, Instant::now())
            .unwrap();

        assert_eq!(
            effect_kinds(&outcome),
            vec![
                EffectKind::Unsubscribed,
                EffectKind::CandidateRemoved,
                EffectKind::Unpublished,
            ]
        );
        assert_eq!(
            operations_on(&outcome, 1),
            vec![
                OpKind::Unsubscribed,
                OpKind::CandidateRemoved,
                OpKind::PlanClear,
                OpKind::RetireRoute,
                OpKind::RemoveRuntime,
            ]
        );
        assert!(lifecycle.allocations.is_empty());
        assert!(!lifecycle.topology.contains(identity));
    }

    #[test]
    fn removing_a_subscriber_reconciles_its_tracks_and_releases_remote_destination() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        let outcomes = lifecycle.remove_participant(participant(2), &registry, Instant::now());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            effect_kinds(&outcomes[0]),
            vec![EffectKind::Unsubscribed, EffectKind::CandidateRemoved]
        );
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn removing_a_publisher_retires_all_of_its_tracks_without_empty_generations() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let audio = publish_audio(&mut lifecycle, &registry, 1, 1).0;
        let video = track(TrackKind::Video, 1, 1, "video");
        let video_identity = TrackIdentity::from_track(&video);
        let _ = lifecycle.publish(video, &registry, Instant::now()).unwrap();
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::video(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        let outcomes = lifecycle.remove_participant(participant(1), &registry, Instant::now());

        assert_eq!(
            outcomes.len(),
            1,
            "all retiring tracks share one removal generation"
        );
        assert!(!lifecycle.topology.contains(audio));
        assert!(!lifecycle.topology.contains(video_identity));
        assert!(lifecycle.allocations.is_empty());
        assert!(lifecycle.candidates.is_empty());
        assert!(lifecycle.bindings.is_empty());
    }

    #[test]
    fn removing_unrelated_participant_emits_no_generation() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let _ = publish_audio(&mut lifecycle, &registry, 1, 1);
        let generation = lifecycle.generation;

        let outcomes = lifecycle.remove_participant(participant(2), &registry, Instant::now());
        assert!(outcomes.is_empty());
        assert_eq!(lifecycle.generation, generation);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn subscriber_removed_from_registry_before_lifecycle_cleanup_does_not_leave_state() {
        let (mut lifecycle, mut registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        registry.remove_participant(&participant(2));

        let outcomes = lifecycle.remove_participant(participant(2), &registry, Instant::now());
        assert_eq!(outcomes.len(), 1);
        assert!(effect_kinds(&outcomes[0]).is_empty());
        assert!(
            !lifecycle
                .allocations
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn subscribe_defaults_reconciles_each_matching_track() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let audio = publish_audio(&mut lifecycle, &registry, 1, 1).0;
        let video = track(TrackKind::Video, 1, 1, "video");
        let video_identity = TrackIdentity::from_track(&video);
        let _ = lifecycle.publish(video, &registry, Instant::now()).unwrap();

        let outcomes = lifecycle.subscribe_defaults(
            room(1),
            participant(2),
            [
                (TrackSelector::audio(), SelectionPolicy::All),
                (TrackSelector::video(), SelectionPolicy::Allocated),
            ],
            &registry,
            Instant::now(),
        );

        assert_eq!(outcomes.len(), 2);
        assert!(lifecycle.candidates[&audio].contains(&participant(2)));
        assert!(lifecycle.bindings[&audio].contains(&participant(2)));
        assert!(lifecycle.candidates[&video_identity].contains(&participant(2)));
        assert!(!lifecycle.bindings[&video_identity].contains(&participant(2)));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn generation_advances_once_per_reconciled_track() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let _ = publish_audio(&mut lifecycle, &registry, 1, 1);
        let video = track(TrackKind::Video, 1, 1, "video");
        let _ = lifecycle.publish(video, &registry, Instant::now()).unwrap();
        let before = lifecycle.generation;

        let outcomes = lifecycle.subscribe_defaults(
            room(1),
            participant(2),
            [(TrackSelector::audio(), SelectionPolicy::All)],
            &registry,
            Instant::now(),
        );

        assert_eq!(outcomes.len(), 2);
        assert_eq!(lifecycle.generation, before + 2);
        assert_eq!(outcomes[0].generation + 1, outcomes[1].generation);
    }

    #[test]
    fn repeated_activation_and_deactivation_are_idempotent_at_the_lifecycle_boundary() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );

        let first_activate = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&first_activate), vec![EffectKind::Subscribed]);
        assert!(forward_install_on(&first_activate, 1).is_some());

        let second_activate = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert!(effect_kinds(&second_activate).is_empty());
        assert!(forward_install_on(&second_activate, 1).is_none());

        let first_deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert_eq!(
            effect_kinds(&first_deactivate),
            vec![EffectKind::Unsubscribed]
        );
        assert!(operations_on(&first_deactivate, 1).contains(&OpKind::PlanClear));

        let second_deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        assert!(effect_kinds(&second_deactivate).is_empty());
        assert!(!operations_on(&second_deactivate, 1).contains(&OpKind::PlanClear));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn unsubscribing_a_missing_selector_is_a_true_noop() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let _ = publish_audio(&mut lifecycle, &registry, 1, 1);
        let generation = lifecycle.generation;

        let outcomes = lifecycle.unsubscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            &registry,
            Instant::now(),
        );

        assert!(outcomes.is_empty());
        assert_eq!(lifecycle.generation, generation);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn one_generation_orders_multiple_participant_transitions_by_participant_identity() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0), (3, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        // Add in reverse order so the assertion actually checks compilation order,
        // not insertion order in the topology.
        let _ = lifecycle.topology.subscribe(
            room(1),
            participant(3),
            TrackSelector::audio(),
            SelectionPolicy::All,
        );
        let _ = lifecycle.topology.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
        );

        let outcome = lifecycle.reconcile(identity, &registry, Instant::now());
        let targets = outcome
            .operations
            .iter()
            .filter_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    participant,
                    effect: ParticipantEffect::TrackCandidateAdded { .. },
                    ..
                } => Some(*participant),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(targets, vec![participant_key(2), participant_key(3)]);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn deterministic_multi_shard_state_machine_matches_a_simple_reference_model() {
        let (mut lifecycle, registry) =
            setup(3, &[(1, 1, 0), (2, 1, 1), (3, 1, 1), (4, 1, 2), (5, 1, 2)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        for subscriber in 2_u8..=5 {
            let _ = lifecycle.subscribe(
                room(1),
                participant(subscriber),
                TrackSelector::audio(),
                SelectionPolicy::Allocated,
                &registry,
                Instant::now(),
            );
        }

        let allocations = [
            destination(&lifecycle, identity, 1).allocation,
            destination(&lifecycle, identity, 2).allocation,
        ];
        let mut active = HashSet::<ParticipantId>::new();
        let mut state = 0x9e37_79b9_u32;

        for _ in 0..2_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let seed = 2 + ((state >> 8) % 4) as u8;
            let should_activate = state & 1 == 0;
            let subscriber = participant(seed);

            if should_activate {
                let _ = lifecycle.activate(
                    room(1),
                    participant(1),
                    identity.id,
                    subscriber,
                    &registry,
                    Instant::now(),
                );
                active.insert(subscriber);
            } else {
                let _ = lifecycle.deactivate(
                    room(1),
                    participant(1),
                    identity.id,
                    subscriber,
                    &registry,
                    Instant::now(),
                );
                active.remove(&subscriber);
            }

            assert_eq!(&lifecycle.bindings[&identity], &active);
            assert_eq!(
                destination(&lifecycle, identity, 1).allocation,
                allocations[0]
            );
            assert_eq!(
                destination(&lifecycle, identity, 2).allocation,
                allocations[1]
            );

            for shard in [1_usize, 2] {
                let shard_active = (2_u8..=5).any(|candidate| {
                    let candidate_shard = if candidate <= 3 { 1 } else { 2 };
                    candidate_shard == shard && active.contains(&participant(candidate))
                });
                let destination = destination(&lifecycle, identity, shard);
                assert_eq!(destination.resident, shard_active);
                assert_eq!(destination.route_installed, shard_active);
            }
            assert_internal_invariants(&lifecycle, &registry);
        }
    }

    #[test]
    fn long_activation_churn_preserves_allocation_and_state_invariants() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );
        let allocation = destination(&lifecycle, identity, 1).allocation;

        for _ in 0..1_000 {
            let activate = lifecycle.activate(
                room(1),
                participant(1),
                identity.id,
                participant(2),
                &registry,
                Instant::now(),
            );
            assert_eq!(effect_kinds(&activate), vec![EffectKind::Subscribed]);
            assert_eq!(destination(&lifecycle, identity, 1).allocation, allocation);
            assert_internal_invariants(&lifecycle, &registry);

            let deactivate = lifecycle.deactivate(
                room(1),
                participant(1),
                identity.id,
                participant(2),
                &registry,
                Instant::now(),
            );
            assert_eq!(effect_kinds(&deactivate), vec![EffectKind::Unsubscribed]);
            assert_eq!(destination(&lifecycle, identity, 1).allocation, allocation);
            assert_internal_invariants(&lifecycle, &registry);
        }
    }

    #[test]
    fn active_view_updates_never_reference_an_uninstalled_remote_route() {
        let (mut lifecycle, registry) = setup(3, &[(1, 1, 0), (2, 1, 1), (3, 1, 2)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        for subscriber in [2_u8, 3_u8] {
            let outcome = lifecycle
                .subscribe(
                    room(1),
                    participant(subscriber),
                    TrackSelector::audio(),
                    SelectionPolicy::All,
                    &registry,
                    Instant::now(),
                )
                .pop()
                .unwrap();
            let shard = usize::from(subscriber - 1);
            let remote = destination(&lifecycle, identity, shard);
            assert!(remote.route_installed);
            assert_eq!(
                forward_install_on(&outcome, shard),
                Some((remote.allocation.route, remote.allocation.key))
            );
            assert_internal_invariants(&lifecycle, &registry);
        }
    }

    #[test]
    fn teardown_plan_is_cleared_before_route_and_runtime_are_retired() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (_identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        let outcome = lifecycle
            .unsubscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        let ops = operations_on(&outcome, 1);
        let clear = ops
            .iter()
            .position(|kind| *kind == OpKind::PlanClear)
            .unwrap();
        let retire = ops
            .iter()
            .position(|kind| *kind == OpKind::RetireRoute)
            .unwrap();
        let remove = ops
            .iter()
            .position(|kind| *kind == OpKind::RemoveRuntime)
            .unwrap();
        assert!(clear < retire && retire < remove);
    }

    #[test]
    fn every_candidate_effect_uses_the_destination_shards_track_key() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);

        let outcome = lifecycle
            .subscribe(
                room(1),
                participant(2),
                TrackSelector::audio(),
                SelectionPolicy::Allocated,
                &registry,
                Instant::now(),
            )
            .pop()
            .unwrap();
        let remote = destination(&lifecycle, identity, 1);
        let effect_key = outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    shard,
                    effect: ParticipantEffect::TrackCandidateAdded { key, .. },
                    ..
                } if *shard == ShardId::new(1) => Some(*key),
                _ => None,
            });

        assert_eq!(effect_key, Some(remote.allocation.key));
        assert_ne!(
            remote.allocation.key,
            destination(&lifecycle, identity, 0).allocation.key
        );
    }

    #[test]
    fn operation_stream_for_remote_activation_has_one_forward_install_and_two_views() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );

        let outcome = lifecycle.activate(
            room(1),
            participant(1),
            identity.id,
            participant(2),
            &registry,
            Instant::now(),
        );
        let kinds = op_kinds(&outcome);

        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == OpKind::InstallForward)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == OpKind::InsertRuntime)
                .count(),
            2,
            "origin and destination views are rebuilt in the generation"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == OpKind::PlanSet)
                .count(),
            2
        );
    }
}
