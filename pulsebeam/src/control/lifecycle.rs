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
}

#[derive(Debug, Clone, Copy)]
struct TrackDestination {
    allocation: TrackAllocation,
    installed: bool,
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

struct RuntimePlan<'a> {
    shard: ShardId,
    identity: TrackIdentity,
    origin_key: ParticipantKey,
    track: &'a Track,
    key: TrackKey,
    local: Vec<ParticipantKey>,
    remote: Vec<RouteHandle>,
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

pub(crate) struct TrackLifecycle {
    topology: TrackTopology,
    allocator: TrackAllocator,
    keys: HashMap<TrackIdentity, TrackKey>,
    allocations: HashMap<(TrackIdentity, ShardId), TrackDestination>,
    candidates: HashMap<TrackIdentity, HashSet<ParticipantId>>,
    bindings: HashMap<TrackIdentity, HashSet<ParticipantId>>,
    retries: HashSet<TrackIdentity>,
    generation: u64,
}

impl TrackLifecycle {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            topology: TrackTopology::default(),
            allocator: TrackAllocator::new(shard_count),
            keys: HashMap::new(),
            allocations: HashMap::new(),
            candidates: HashMap::new(),
            bindings: HashMap::new(),
            retries: HashSet::new(),
            generation: 0,
        }
    }

    pub(crate) fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        debug_assert_ne!(self.generation, 0);
        self.generation
    }

    pub(crate) fn publish(
        &mut self,
        track: Track,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Option<TrackLifecycleOutcome> {
        let identity = self.topology.publish(track)?;
        Some(self.reconcile(identity, false, registry, now))
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
        self.topology.unpublish(identity)?;
        let mut stager = TrackLifecycleStager::new(self.next_generation());
        self.stage_track_removals(identity, registry, &mut stager);
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
        self.reconcile(identity, false, registry, now)
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
        self.reconcile(identity, false, registry, now)
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
        identities
            .into_iter()
            .map(|identity| self.reconcile(identity, false, registry, now))
            .collect()
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
        identities
            .into_iter()
            .map(|identity| self.reconcile(identity, false, registry, now))
            .collect()
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
        self.topology
            .identities()
            .filter(|identity| identity.room_id == room_id)
            .collect::<Vec<_>>()
            .into_iter()
            .map(|identity| self.reconcile(identity, false, registry, now))
            .collect()
    }

    pub(crate) fn retry(
        &mut self,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        std::mem::take(&mut self.retries)
            .into_iter()
            .map(|identity| self.reconcile(identity, false, registry, now))
            .collect()
    }

    pub(crate) fn remove_participant(
        &mut self,
        participant: ParticipantId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let mut affected: Vec<_> = self
            .topology
            .identities()
            .filter(|identity| {
                identity.publisher == participant
                    || self
                        .topology
                        .candidate_subscribers(*identity)
                        .any(|subscriber| subscriber == participant)
            })
            .collect();
        let retiring: Vec<_> = affected
            .iter()
            .copied()
            .filter(|identity| identity.publisher == participant)
            .collect();
        let mut stager = TrackLifecycleStager::new(self.next_generation());
        for identity in retiring.iter().copied() {
            self.stage_track_removals(identity, registry, &mut stager);
            let removed = self.topology.unpublish(identity);
            debug_assert!(removed.is_some(), "retiring track must remain published");
        }
        let mut outcomes = vec![stager.finish()];
        self.topology.remove_participant(participant);
        for identity in retiring {
            self.release_allocations(identity, now);
        }
        affected.retain(|identity| self.topology.contains(*identity));
        outcomes.extend(
            affected
                .into_iter()
                .map(|identity| self.reconcile(identity, false, registry, now)),
        );
        outcomes
    }

    fn reconcile(
        &mut self,
        identity: TrackIdentity,
        retiring: bool,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let generation = self.next_generation();
        let mut stager = TrackLifecycleStager::new(generation);
        let Some(track) = self.topology.track(identity).cloned() else {
            if retiring {
                self.release_allocations(identity, now);
            }
            return stager.finish();
        };
        let Some(origin) = ParticipantLocation::lookup(registry, &identity.publisher) else {
            debug_assert!(false, "a published track must have a live publisher");
            return stager.finish();
        };
        let Some(origin_key) = origin.binding else {
            debug_assert!(false, "a published track must have a publisher key");
            return stager.finish();
        };
        let origin_shard = origin.shard;
        let mut candidates: HashSet<_> = self
            .topology
            .candidate_subscribers(identity)
            .filter(|participant| *participant != identity.publisher)
            .filter(|participant| {
                ParticipantLocation::lookup(registry, participant)
                    .is_some_and(|meta| meta.room_id == identity.room_id && meta.binding.is_some())
            })
            .collect();
        let mut bindings: HashSet<_> = self
            .topology
            .active_subscribers(identity)
            .filter(|participant| candidates.contains(participant))
            .collect();

        let origin_allocation = match self.allocations.get(&(identity, origin_shard)).copied() {
            Some(destination) => {
                debug_assert!(destination.installed);
                destination.allocation
            }
            None => {
                let Ok(allocation) = self.allocator.allocate(origin_shard, identity, now) else {
                    self.retries.insert(identity);
                    return stager.finish();
                };
                self.allocations.insert(
                    (identity, origin_shard),
                    TrackDestination {
                        allocation,
                        installed: true,
                    },
                );
                self.keys.insert(identity, allocation.key);
                allocation
            }
        };
        if track.requires_reverse_route()
            && track.reverse().is_none()
            && let Some(track) = self.topology.track_mut(identity)
        {
            track.set_reverse(Some(origin_allocation.route));
        }

        let candidate_shards: HashSet<_> = candidates
            .iter()
            .filter_map(|participant| {
                ParticipantLocation::lookup(registry, participant).map(|meta| meta.shard)
            })
            .filter(|shard| *shard != origin_shard)
            .collect();
        let mut unavailable = HashSet::new();
        for destination in candidate_shards {
            if self.allocations.contains_key(&(identity, destination)) {
                continue;
            }
            let Ok(allocation) = self.allocator.allocate(destination, identity, now) else {
                unavailable.insert(destination);
                continue;
            };
            self.allocations.insert(
                (identity, destination),
                TrackDestination {
                    allocation,
                    installed: false,
                },
            );
        }
        if unavailable.is_empty() {
            self.retries.remove(&identity);
        } else {
            self.retries.insert(identity);
            candidates.retain(|participant| {
                ParticipantLocation::lookup(registry, participant)
                    .is_none_or(|meta| !unavailable.contains(&meta.shard))
            });
            bindings.retain(|participant| candidates.contains(participant));
        }

        let previous_candidates = self.candidates.get(&identity).cloned().unwrap_or_default();
        let previous_bindings = self.bindings.get(&identity).cloned().unwrap_or_default();
        for participant in previous_bindings.difference(&bindings) {
            self.stage_binding_effect(
                identity,
                *participant,
                ParticipantEffect::TrackUnsubscribed {
                    key: self.track_key(
                        identity,
                        *participant,
                        registry,
                        "an active binding must have a destination allocation",
                    ),
                    track_id: identity.id,
                },
                registry,
                &mut stager,
            );
        }
        for participant in previous_candidates.difference(&candidates) {
            self.stage_binding_effect(
                identity,
                *participant,
                ParticipantEffect::TrackCandidateRemoved {
                    key: self.track_key(
                        identity,
                        *participant,
                        registry,
                        "a candidate must have a destination allocation",
                    ),
                    track_id: identity.id,
                },
                registry,
                &mut stager,
            );
        }
        for participant in candidates.difference(&previous_candidates) {
            self.stage_binding_effect(
                identity,
                *participant,
                ParticipantEffect::TrackCandidateAdded {
                    key: self.track_key(
                        identity,
                        *participant,
                        registry,
                        "a candidate must have a destination allocation",
                    ),
                    track: track.clone(),
                },
                registry,
                &mut stager,
            );
        }
        for participant in bindings.difference(&previous_bindings) {
            debug_assert!(candidates.contains(participant));
            self.stage_binding_effect(
                identity,
                *participant,
                ParticipantEffect::TrackSubscribed {
                    key: self.track_key(
                        identity,
                        *participant,
                        registry,
                        "an active binding must have a destination allocation",
                    ),
                    track_id: identity.id,
                },
                registry,
                &mut stager,
            );
        }

        let active_shards: HashSet<_> = bindings
            .iter()
            .filter_map(|participant| {
                ParticipantLocation::lookup(registry, participant).map(|meta| meta.shard)
            })
            .filter(|shard| *shard != origin_shard)
            .collect();
        let destinations: Vec<_> = self
            .allocations
            .iter()
            .filter_map(|((held, shard), destination)| {
                (*held == identity && *shard != origin_shard).then_some((*shard, *destination))
            })
            .collect();
        for (shard, destination) in &destinations {
            let should_install = active_shards.contains(shard);
            if should_install && !destination.installed {
                stager.update(
                    *shard,
                    ShardUpdateOp::InstallRoute {
                        handle: destination.allocation.route,
                        action: RouteAction::Forward {
                            target: destination.allocation.key,
                        },
                    },
                );
                if let Some(state) = self.allocations.get_mut(&(identity, *shard)) {
                    state.installed = true;
                }
            } else if !should_install && destination.installed {
                stager.plans(
                    *shard,
                    vec![TrackPlanUpdate {
                        key: destination.allocation.key,
                        plan: None,
                    }],
                );
                stager.update(
                    *shard,
                    ShardUpdateOp::RetireRoute {
                        handle: destination.allocation.route,
                    },
                );
                stager.update(
                    *shard,
                    ShardUpdateOp::RemoveTrackRuntime {
                        key: destination.allocation.key,
                    },
                );
                if let Some(state) = self.allocations.get_mut(&(identity, *shard)) {
                    state.installed = false;
                }
            }
        }

        let remote_routes: Vec<_> = active_shards
            .iter()
            .filter_map(|shard| {
                self.allocations
                    .get(&(identity, *shard))
                    .map(|destination| destination.allocation.route)
            })
            .collect();
        let origin_local = bindings
            .iter()
            .filter_map(|participant| ParticipantLocation::lookup(registry, participant))
            .filter_map(|meta| {
                (meta.shard == origin_shard)
                    .then_some(meta.binding)
                    .flatten()
            })
            .collect();
        if track.requires_reverse_route() {
            stager.update(
                origin_shard,
                ShardUpdateOp::InstallRoute {
                    handle: origin_allocation.route,
                    action: RouteAction::Reverse {
                        target: origin_allocation.key,
                    },
                },
            );
        }
        self.stage_runtime_and_plan(
            &mut stager,
            RuntimePlan {
                shard: origin_shard,
                identity,
                origin_key,
                track: &track,
                key: origin_allocation.key,
                local: origin_local,
                remote: remote_routes,
            },
        );
        stager.participant(
            origin_shard,
            origin_key,
            ParticipantEffect::TrackPublished {
                key: origin_allocation.key,
                track_id: identity.id,
            },
        );
        for shard in active_shards {
            let Some(destination) = self.allocations.get(&(identity, shard)).copied() else {
                debug_assert!(false, "an active shard must have an allocation");
                continue;
            };
            let local = bindings
                .iter()
                .filter_map(|participant| ParticipantLocation::lookup(registry, participant))
                .filter_map(|meta| (meta.shard == shard).then_some(meta.binding).flatten())
                .collect();
            self.stage_runtime_and_plan(
                &mut stager,
                RuntimePlan {
                    shard,
                    identity,
                    origin_key,
                    track: &track,
                    key: destination.allocation.key,
                    local,
                    remote: Vec::new(),
                },
            );
        }
        let candidate_shards: HashSet<_> = candidates
            .iter()
            .filter_map(|participant| {
                ParticipantLocation::lookup(registry, participant).map(|meta| meta.shard)
            })
            .collect();
        for (shard, destination) in destinations {
            if candidate_shards.contains(&shard) {
                continue;
            }
            debug_assert!(
                !self
                    .allocations
                    .get(&(identity, shard))
                    .is_some_and(|state| state.installed)
            );
            let removed = self.allocations.remove(&(identity, shard));
            debug_assert!(removed.is_some());
            self.allocator.release(destination.allocation, now);
        }
        self.candidates.insert(identity, candidates);
        self.bindings.insert(identity, bindings);
        stager.finish()
    }

    fn track_key(
        &self,
        identity: TrackIdentity,
        participant: ParticipantId,
        registry: &RoomRegistry,
        message: &str,
    ) -> TrackKey {
        let Some(meta) = ParticipantLocation::lookup(registry, &participant) else {
            debug_assert!(false, "a lifecycle participant must remain registered");
            return self.keys.get(&identity).copied().unwrap_or_default();
        };
        let Some(destination) = self.allocations.get(&(identity, meta.shard)) else {
            debug_assert!(false, "{message}");
            return self.keys.get(&identity).copied().unwrap_or_default();
        };
        destination.allocation.key
    }

    fn stage_binding_effect(
        &self,
        _identity: TrackIdentity,
        participant: ParticipantId,
        effect: ParticipantEffect,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let Some(meta) = ParticipantLocation::lookup(registry, &participant) else {
            return;
        };
        let Some(key) = meta.binding else {
            return;
        };
        stager.participant(meta.shard, key, effect);
    }

    fn stage_runtime_and_plan(&self, stager: &mut TrackLifecycleStager, runtime: RuntimePlan<'_>) {
        let reverse = self
            .topology
            .track(runtime.identity)
            .and_then(Track::reverse);
        stager.update(
            runtime.shard,
            ShardUpdateOp::InsertTrackRuntime {
                key: runtime.key,
                runtime: TrackRuntime {
                    descriptor: Some(TrackDescriptor {
                        origin_key: runtime.origin_key,
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
        stager.update(runtime.shard, ShardUpdateOp::Placeholder);
        stager.plans(
            runtime.shard,
            vec![TrackPlanUpdate {
                key: runtime.key,
                plan: Some(TrackPlan::new(runtime.local, runtime.remote, reverse)),
            }],
        );
    }

    fn stage_track_removals(
        &mut self,
        identity: TrackIdentity,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let bindings = self.bindings.remove(&identity).unwrap_or_default();
        let candidates = self.candidates.remove(&identity).unwrap_or_default();
        for participant in &bindings {
            self.stage_binding_effect(
                identity,
                *participant,
                ParticipantEffect::TrackUnsubscribed {
                    key: self.track_key(
                        identity,
                        *participant,
                        registry,
                        "an active binding must have an allocation",
                    ),
                    track_id: identity.id,
                },
                registry,
                stager,
            );
        }
        for participant in candidates {
            self.stage_binding_effect(
                identity,
                participant,
                ParticipantEffect::TrackCandidateRemoved {
                    key: self.track_key(
                        identity,
                        participant,
                        registry,
                        "a candidate must have an allocation",
                    ),
                    track_id: identity.id,
                },
                registry,
                stager,
            );
        }
        if let Some(meta) = ParticipantLocation::lookup(registry, &identity.publisher)
            && let Some(participant) = meta.binding
            && let Some(source_key) = self
                .allocations
                .get(&(identity, meta.shard))
                .map(|destination| destination.allocation.key)
                .or_else(|| self.keys.get(&identity).copied())
        {
            stager.participant(
                meta.shard,
                participant,
                ParticipantEffect::TrackUnpublished {
                    key: source_key,
                    track_id: identity.id,
                },
            );
        }
        let allocations: Vec<_> = self
            .allocations
            .iter()
            .filter_map(|((held, shard), destination)| {
                (*held == identity).then_some((*shard, *destination))
            })
            .collect();
        for (shard, destination) in allocations {
            if destination.installed {
                stager.update(
                    shard,
                    ShardUpdateOp::RetireRoute {
                        handle: destination.allocation.route,
                    },
                );
                stager.update(
                    shard,
                    ShardUpdateOp::RemoveTrackRuntime {
                        key: destination.allocation.key,
                    },
                );
                stager.plans(
                    shard,
                    vec![TrackPlanUpdate {
                        key: destination.allocation.key,
                        plan: None,
                    }],
                );
            }
        }
    }

    fn release_allocations(&mut self, identity: TrackIdentity, now: Instant) {
        let allocations: Vec<_> = self
            .allocations
            .extract_if(|(held, _), _| *held == identity)
            .map(|(_, destination)| destination.allocation)
            .collect();
        for allocation in allocations {
            self.allocator.release(allocation, now);
        }
        self.keys.remove(&identity);
        self.candidates.remove(&identity);
        self.bindings.remove(&identity);
    }
}
