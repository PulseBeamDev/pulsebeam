use std::collections::{HashMap, HashSet};

use pulsebeam_runtime::time::Instant;

use crate::{
    control::{
        registry::RoomRegistry,
        topology::{
            Subscriber, TrackAncestry, TrackPlacement, TrackPlacementAllocator, TrackTopology,
        },
    },
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectRoomKey, PublicationKey},
    id::ShardId,
    participant::ParticipantEffect,
    route::NodeRouteAddress,
    shard_update::{
        ShardUpdateOp, TrackDescriptor, TrackPlan, TrackPlanUpdate, TrackRouteAction, TrackRuntime,
    },
    track::{SelectionPolicy, Track, TrackSelector},
};

#[derive(Debug, Clone, Copy)]
struct BoundParticipantLocation {
    shard: ShardId,
    participant: ParticipantId,
    connection: ConnectionId,
}

impl BoundParticipantLocation {
    fn in_room(
        registry: &RoomRegistry,
        participant: &ParticipantId,
        room: ProjectRoomKey,
    ) -> Option<Self> {
        let meta = registry.get_endpoint(EndpointKey {
            room,
            participant: *participant,
        })?;
        Self::new(*participant, meta)
    }

    fn new(participant: ParticipantId, meta: &super::registry::ParticipantMeta) -> Option<Self> {
        meta.materialized.then_some(Self {
            shard: meta.shard_id,
            participant,
            connection: meta.connection_id,
        })
    }

    fn for_subscriber(registry: &RoomRegistry, subscriber: Subscriber) -> Option<Self> {
        match subscriber {
            Subscriber::Joined(endpoint) => {
                Self::in_room(registry, &endpoint.participant, endpoint.room)
            }
            Subscriber::Connection(connection) => {
                let (participant, meta) = registry.get_connection(connection)?;
                Self::new(participant, meta)
            }
        }
    }
}

/// Routable placement held for one track on one shard.
///
/// Candidate shards reserve a placement before they become active. `resident`
/// means the shard currently has the track runtime/plan installed.
/// `route_installed` tracks the route table separately because an origin shard
/// only needs a route for tracks with a reverse path.
#[derive(Debug, Clone, Copy)]
struct TrackDestination {
    placement: TrackPlacement,
    resident: bool,
    route_installed: bool,
}

impl TrackDestination {
    fn reserved(placement: TrackPlacement) -> Self {
        Self {
            placement,
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
        connection: ConnectionId,
        #[cfg(test)]
        participant: ParticipantId,
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

    fn plan(&mut self, shard: ShardId, publication_id: PublicationKey, plan: Option<TrackPlan>) {
        self.plans(
            shard,
            vec![TrackPlanUpdate {
                publication_id,
                plan,
            }],
        );
    }

    fn participant(&mut self, location: BoundParticipantLocation, effect: ParticipantEffect) {
        self.operations
            .push(TrackLifecycleOperation::ParticipantEffect {
                shard: location.shard,
                connection: location.connection,
                #[cfg(test)]
                participant: location.participant,
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
    candidates: HashSet<Subscriber>,
    bindings: HashSet<Subscriber>,
    locations: HashMap<Subscriber, BoundParticipantLocation>,
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

    fn local_bindings(&self, shard: ShardId) -> Vec<ConnectionId> {
        let mut connections = self
            .bindings
            .iter()
            .filter_map(|participant| self.locations.get(participant))
            .filter(|location| location.shard == shard)
            .map(|location| location.connection)
            .collect::<Vec<_>>();
        connections.sort();
        connections
    }
}

struct RuntimePlan<'a> {
    shard: ShardId,
    origin: ConnectionId,
    track: &'a Track,
    publication_id: PublicationKey,
    local: Vec<ConnectionId>,
    remote: Vec<NodeRouteAddress>,
    reverse: Option<NodeRouteAddress>,
}

pub(crate) struct TrackLifecycle {
    topology: TrackTopology,
    allocator: TrackPlacementAllocator,
    placements: HashMap<(TrackAncestry, ShardId), TrackDestination>,
    candidates: HashMap<TrackAncestry, HashSet<Subscriber>>,
    bindings: HashMap<TrackAncestry, HashSet<Subscriber>>,
    // Removal effects still need the applied endpoint after its subscription is gone.
    locations: HashMap<TrackAncestry, HashMap<Subscriber, BoundParticipantLocation>>,
    generation: u64,
}

impl TrackLifecycle {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            topology: TrackTopology::default(),
            allocator: TrackPlacementAllocator::new(shard_count),
            placements: HashMap::new(),
            candidates: HashMap::new(),
            bindings: HashMap::new(),
            locations: HashMap::new(),
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
        endpoint: EndpointKey,
        publication_id: PublicationKey,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Option<TrackLifecycleOutcome> {
        let identity = TrackAncestry {
            room_id: endpoint.room,
            publisher: endpoint.participant,
            publication_id,
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

        self.release_placements(identity, now);
        Some(stager.finish())
    }

    pub(crate) fn activate(
        &mut self,
        room_id: ProjectRoomKey,
        publisher: ParticipantId,
        publication_id: PublicationKey,
        subscriber: impl Into<Subscriber>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let identity = TrackAncestry {
            room_id,
            publisher,
            publication_id,
        };
        let _ = self.topology.activate(identity, subscriber);
        self.reconcile(identity, registry, now)
    }

    pub(crate) fn deactivate(
        &mut self,
        room_id: ProjectRoomKey,
        publisher: ParticipantId,
        publication_id: PublicationKey,
        subscriber: impl Into<Subscriber>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let identity = TrackAncestry {
            room_id,
            publisher,
            publication_id,
        };
        let _ = self.topology.deactivate(identity, subscriber);
        self.reconcile(identity, registry, now)
    }

    pub(crate) fn subscribe(
        &mut self,
        room_id: ProjectRoomKey,
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
        room_id: ProjectRoomKey,
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

    pub(super) fn catalog_version(&self) -> u64 {
        self.topology.catalog_version()
    }

    pub(super) fn room_catalog_version(&self, room: ProjectRoomKey) -> u64 {
        self.topology.room_catalog_version(room)
    }

    pub(super) fn publisher_sources(
        &self,
        room: ProjectRoomKey,
        publisher: ParticipantId,
        connection: ConnectionId,
    ) -> impl Iterator<Item = &Track> {
        self.topology.publisher_sources(room, publisher, connection)
    }

    pub(super) fn sources_in_room(&self, room_id: ProjectRoomKey) -> impl Iterator<Item = &Track> {
        self.topology.sources_in_room(room_id)
    }

    pub(super) fn source_exact(
        &self,
        room: ProjectRoomKey,
        publication: PublicationKey,
    ) -> Option<&Track> {
        self.topology.source(room, publication)
    }

    pub(crate) fn network_source(
        &self,
        room_id: ProjectRoomKey,
        track: PublicationKey,
        registry: &RoomRegistry,
    ) -> Option<ShardId> {
        let source = self.topology.source(room_id, track)?;
        let meta = source.meta();
        let publisher = registry.get_incarnation(meta.origin, meta.connection_id)?;
        (publisher.materialized && publisher.scope.joined_room() == Some(room_id))
            .then_some(publisher.shard_id)
    }

    pub(crate) fn allows_network_reverse(
        &self,
        room: ProjectRoomKey,
        track: PublicationKey,
        shard: ShardId,
        registry: &RoomRegistry,
    ) -> bool {
        self.network_source(room, track, registry)
            .is_some_and(|origin| origin != shard)
            && self
                .topology
                .source(room, track)
                .is_some_and(crate::track::Track::requires_reverse_route)
    }

    pub(crate) fn source_by_address<'a>(
        &'a self,
        source_room: ProjectRoomKey,
        subscriber: ConnectionId,
        address: &pulsebeam_proto::state::LogicalSourceAddress,
        registry: &'a RoomRegistry,
    ) -> Option<&'a Track> {
        let kind = crate::control::native::track_kind(address.kind)?;
        let publisher = ParticipantId::new(&address.participant_id).ok()?;
        let track_id = crate::entity::TrackId::new(&address.track_id).ok()?;
        let track = self.current_named_source(
            EndpointKey {
                room: source_room,
                participant: publisher,
            },
            kind,
            track_id,
            registry,
        )?;
        self.source_for_connection(source_room, subscriber, track.id(), registry)
    }

    pub(super) fn current_named_source(
        &self,
        endpoint: EndpointKey,
        kind: crate::entity::TrackKind,
        track_id: crate::entity::TrackId,
        registry: &RoomRegistry,
    ) -> Option<&Track> {
        let current = registry.get_endpoint(endpoint)?;
        if !current.materialized {
            return None;
        }
        let mut sources = self
            .topology
            .exact_named_sources(endpoint.room, endpoint.participant, kind, track_id)?
            .filter(|track| track.meta().connection_id == current.connection_id);
        let source = sources.next()?;
        sources.next().is_none().then_some(source)
    }

    pub(crate) fn source_for_connection(
        &self,
        source_room: ProjectRoomKey,
        subscriber: ConnectionId,
        track: PublicationKey,
        registry: &RoomRegistry,
    ) -> Option<&Track> {
        let (_, receiver) = registry.get_connection(subscriber)?;
        if !receiver.materialized || source_room.project != receiver.scope.project {
            return None;
        }
        let source = self.topology.source(source_room, track)?;
        let meta = source.meta();
        if meta.connection_id == subscriber {
            return None;
        }
        let publisher = registry.get_incarnation(meta.origin, meta.connection_id)?;
        (publisher.materialized && publisher.scope.joined_room() == Some(source_room))
            .then_some(source)
    }

    #[cfg(test)]
    fn source(
        &self,
        subscriber_room: ProjectRoomKey,
        source_room: ProjectRoomKey,
        subscriber: ParticipantId,
        track: PublicationKey,
        registry: &RoomRegistry,
    ) -> Option<&Track> {
        let receiver = registry.get_endpoint(EndpointKey {
            room: subscriber_room,
            participant: subscriber,
        })?;
        self.source_for_connection(source_room, receiver.connection_id, track, registry)
    }

    pub(crate) fn set_owned_subscription(
        &mut self,
        connection: ConnectionId,
        request: super::topology::OwnedSubscription,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        if registry
            .get_incarnation(request.subscriber, connection)
            .is_none()
            || self
                .source_for_connection(request.source_room, connection, request.track, registry)
                .is_none()
        {
            return Vec::new();
        }
        let identities = self
            .topology
            .matching_tracks(request.source_room, &TrackSelector::track(request.track));
        if !self.topology.set_owned_scoped(
            request.source_room,
            Subscriber::Connection(connection),
            request.reference,
            request.track,
            request.enabled,
        ) {
            return Vec::new();
        }
        self.reconcile_all(identities, registry, now)
    }

    pub(crate) fn unsubscribe_owned(
        &mut self,
        room_id: ProjectRoomKey,
        subscriber: impl Into<Subscriber>,
        reference: u64,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let Some(subscription) = self.topology.remove_owned(room_id, subscriber, reference) else {
            return Vec::new();
        };
        let identities = self
            .topology
            .matching_tracks(room_id, &subscription.selector);
        self.reconcile_all(identities, registry, now)
    }

    #[cfg(test)]
    fn remove_participant(
        &mut self,
        endpoint: EndpointKey,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let connection = registry
            .get_endpoint(endpoint)
            .map(|meta| meta.connection_id);
        self.remove_owner(Some(endpoint), connection, registry, now)
    }

    pub(crate) fn remove_connection(
        &mut self,
        endpoint: Option<EndpointKey>,
        connection: ConnectionId,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        self.remove_owner(endpoint, Some(connection), registry, now)
    }

    fn remove_owner(
        &mut self,
        endpoint: Option<EndpointKey>,
        connection: Option<ConnectionId>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        let retires = |identity: TrackAncestry| {
            endpoint.is_some_and(|endpoint| {
                identity.publisher == endpoint.participant && identity.room_id == endpoint.room
            })
        };
        let subscribers = endpoint
            .map(Subscriber::Joined)
            .into_iter()
            .chain(connection.map(Subscriber::Connection));
        let mut affected = self
            .topology
            .identities()
            .filter(|identity| {
                retires(*identity)
                    || subscribers
                        .clone()
                        .any(|subscriber| self.topology.has_candidate(*identity, subscriber))
            })
            .collect::<Vec<_>>();

        let retiring = affected
            .iter()
            .copied()
            .filter(|identity| retires(*identity))
            .collect::<Vec<_>>();

        let mut outcomes = Vec::new();
        if !retiring.is_empty() {
            let mut stager = TrackLifecycleStager::new(self.next_generation());
            for identity in retiring.iter().copied() {
                self.stage_track_removal(identity, registry, &mut stager);
            }
            outcomes.push(stager.finish());
        }

        if let Some(connection) = connection {
            self.topology
                .remove_subscriber(Subscriber::Connection(connection));
        }
        if let Some(endpoint) = endpoint {
            self.topology.remove_participant(endpoint);
        }

        for identity in retiring {
            debug_assert!(
                !self.topology.contains(identity),
                "participant removal must remove every published track"
            );
            self.release_placements(identity, now);
        }

        affected.retain(|identity| self.topology.contains(*identity));
        outcomes.extend(self.reconcile_all(affected, registry, now));
        outcomes
    }

    fn reconcile_all(
        &mut self,
        identities: impl IntoIterator<Item = TrackAncestry>,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Vec<TrackLifecycleOutcome> {
        identities
            .into_iter()
            .map(|identity| self.reconcile(identity, registry, now))
            .collect()
    }

    pub(super) fn reconcile(
        &mut self,
        identity: TrackAncestry,
        registry: &RoomRegistry,
        now: Instant,
    ) -> TrackLifecycleOutcome {
        let mut stager = TrackLifecycleStager::new(self.next_generation());
        let mut desired = self.desired_state(identity, registry);

        let origin = self.ensure_destination(identity, desired.origin.shard, now);
        self.ensure_reverse_route(identity, &mut desired.track, origin.placement.route);
        self.reserve_candidate_destinations(identity, &desired, now);

        self.stage_participant_transitions(identity, &desired, registry, &mut stager);
        self.reconcile_remote_residency(identity, &desired, &mut stager);
        self.stage_active_views(identity, &desired, &mut stager);
        self.release_unused_destinations(identity, &desired, now);

        self.candidates.insert(identity, desired.candidates);
        self.bindings.insert(identity, desired.bindings);
        self.locations.insert(identity, desired.locations);
        #[cfg(debug_assertions)]
        self.assert_committed_track(identity, registry);
        stager.finish()
    }

    #[cfg(debug_assertions)]
    #[allow(
        clippy::expect_used,
        reason = "a completed reconciliation retains the published track and its bound participants"
    )]
    fn assert_committed_track(&self, identity: TrackAncestry, registry: &RoomRegistry) {
        let track = self.topology.track(identity).expect("committed track");
        let candidates = self
            .candidates
            .get(&identity)
            .expect("committed candidates");
        let bindings = self.bindings.get(&identity).expect("committed bindings");
        debug_assert!(
            bindings.is_subset(candidates),
            "bindings must be candidates"
        );
        debug_assert!(!candidates.contains(&Subscriber::Joined(EndpointKey {
            room: identity.room_id,
            participant: identity.publisher,
        })));
        let origin =
            BoundParticipantLocation::in_room(registry, &identity.publisher, identity.room_id)
                .expect("published track must retain a bound publisher");
        let mut candidate_shards = HashSet::from([origin.shard]);
        let mut active_shards = HashSet::new();
        for participant in candidates {
            let location = BoundParticipantLocation::for_subscriber(registry, *participant)
                .expect("retained candidate must remain bound in its admitted room");
            candidate_shards.insert(location.shard);
            if bindings.contains(participant) {
                active_shards.insert(location.shard);
            }
        }
        for shard in candidate_shards {
            let destination = self
                .placements
                .get(&(identity, shard))
                .expect("committed candidate shard must retain its placement");
            debug_assert_eq!(
                destination.placement.publication_id,
                identity.publication_id
            );
            debug_assert_eq!(destination.placement.route.shard(), shard);
            debug_assert!(
                destination.resident,
                "candidate runtime must remain resident"
            );
            debug_assert_eq!(
                destination.route_installed,
                if shard == origin.shard {
                    track.requires_reverse_route()
                } else {
                    active_shards.contains(&shard)
                },
                "route residency must match origin semantics or active bindings"
            );
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "reconciliation only runs for a published track whose publisher remains bound"
    )]
    fn desired_state(&self, identity: TrackAncestry, registry: &RoomRegistry) -> DesiredTrackState {
        let track = self
            .topology
            .track(identity)
            .expect("reconcile must target a published track")
            .clone();
        let origin =
            BoundParticipantLocation::in_room(registry, &identity.publisher, identity.room_id)
                .expect("a published track must have a bound live publisher in its room");

        let publisher_meta = registry.get_endpoint(EndpointKey {
            room: identity.room_id,
            participant: identity.publisher,
        });
        let publisher = Some(identity.publisher.as_str());
        let mut locations = HashMap::new();
        let candidates = self
            .topology
            .candidate_subscribers(identity)
            .filter_map(|subscriber| {
                let location = BoundParticipantLocation::for_subscriber(registry, subscriber)?;
                if location.connection == origin.connection {
                    return None;
                }
                let meta = registry.get_incarnation(location.participant, location.connection)?;
                let allowed = meta.authorization.as_ref().map_or(
                    meta.profile == super::controller::SignalingProtocol::State,
                    |lease| {
                        if meta.scope.joined_room() == Some(identity.room_id) {
                            lease.permissions().allows_source(
                                pulsebeam_core::auth::grants::Action::Subscribe,
                                publisher,
                                &track,
                            )
                        } else {
                            publisher_meta.is_some_and(|source| source.authorization.is_some())
                                && lease.permissions().allows_source_at_room(
                                    pulsebeam_core::auth::grants::Action::Subscribe,
                                    identity.room_id.room.as_str(),
                                    publisher,
                                    &track,
                                )
                        }
                    },
                );
                if !allowed {
                    return None;
                }
                locations.insert(subscriber, location);
                Some(subscriber)
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
        identity: TrackAncestry,
        shard: ShardId,
        now: Instant,
    ) -> TrackDestination {
        if let Some(destination) = self.placements.get(&(identity, shard)).copied() {
            return destination;
        }

        // Track placement is an internal capacity invariant. Supported shard
        // capacity must be lower than the route space, so exhaustion is not
        // a recoverable lifecycle state and is asserted inside TrackPlacementAllocator.
        let placement = self.allocator.allocate(shard, identity, now);
        let destination = TrackDestination::reserved(placement);
        self.placements.insert((identity, shard), destination);
        destination
    }

    #[allow(
        clippy::expect_used,
        reason = "the desired track is cloned from the topology entry updated here"
    )]
    fn ensure_reverse_route(
        &mut self,
        identity: TrackAncestry,
        desired_track: &mut Track,
        route: NodeRouteAddress,
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
        identity: TrackAncestry,
        desired: &DesiredTrackState,
        now: Instant,
    ) {
        for shard in desired.candidate_remote_shards() {
            let _ = self.ensure_destination(identity, shard, now);
        }
    }

    fn stage_participant_transitions(
        &self,
        identity: TrackAncestry,
        desired: &DesiredTrackState,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let previous_candidates = self.candidates.get(&identity).cloned().unwrap_or_default();
        let previous_bindings = self.bindings.get(&identity).cloned().unwrap_or_default();

        // Remove active state before removing candidate state. Sort participant
        // transitions so one semantic generation always compiles to one operation order.
        let previous = self.locations.get(&identity);
        let mut removed_bindings = previous_bindings
            .difference(&desired.bindings)
            .copied()
            .collect::<Vec<_>>();
        removed_bindings.sort();
        for participant in removed_bindings {
            self.stage_track_effect(
                identity,
                previous.and_then(|locations| locations.get(&participant)),
                registry,
                stager,
                ParticipantEffect::TrackUnsubscribed {
                    publication_id: identity.publication_id,
                },
            );
        }

        let mut removed_candidates = previous_candidates
            .difference(&desired.candidates)
            .copied()
            .collect::<Vec<_>>();
        removed_candidates.sort();
        for participant in removed_candidates {
            self.stage_track_effect(
                identity,
                previous.and_then(|locations| locations.get(&participant)),
                registry,
                stager,
                ParticipantEffect::TrackCandidateRemoved {
                    publication_id: identity.publication_id,
                    reason: crate::participant::effect::TrackRemoval::Withdrawn,
                },
            );
        }

        // Establish candidate state before activating it.
        let mut added_candidates = desired
            .candidates
            .difference(&previous_candidates)
            .copied()
            .collect::<Vec<_>>();
        added_candidates.sort();
        for participant in added_candidates {
            self.stage_track_effect(
                identity,
                desired.locations.get(&participant),
                registry,
                stager,
                ParticipantEffect::TrackCandidateAdded {
                    track: desired.track.clone(),
                },
            );
        }

        let mut added_bindings = desired
            .bindings
            .difference(&previous_bindings)
            .copied()
            .collect::<Vec<_>>();
        added_bindings.sort();
        for participant in added_bindings {
            debug_assert!(desired.candidates.contains(&participant));
            self.stage_track_effect(
                identity,
                desired.locations.get(&participant),
                registry,
                stager,
                ParticipantEffect::TrackSubscribed {
                    publication_id: identity.publication_id,
                },
            );
        }
    }

    fn reconcile_remote_residency(
        &mut self,
        identity: TrackAncestry,
        desired: &DesiredTrackState,
        stager: &mut TrackLifecycleStager,
    ) {
        let candidate_shards = desired.candidate_remote_shards();
        let active_shards = desired.active_remote_shards();
        let destinations = self.remote_destinations(identity, desired.origin.shard);

        for (shard, destination) in destinations {
            let should_be_resident = candidate_shards.contains(&shard);
            let should_have_route = active_shards.contains(&shard);
            if destination.resident && !should_be_resident {
                self.stage_shard_withdrawal(shard, destination, stager);
                self.set_destination_state(identity, shard, false, false);
                continue;
            }
            if !destination.route_installed && should_have_route {
                stager.update(
                    shard,
                    ShardUpdateOp::InstallRoute {
                        address: destination.placement.route,
                        action: TrackRouteAction::Forward {
                            publication_id: destination.placement.publication_id,
                        },
                    },
                );
                self.set_route_installed(identity, shard, true);
            } else if destination.route_installed && !should_have_route {
                stager.update(
                    shard,
                    ShardUpdateOp::RetireRoute {
                        address: destination.placement.route,
                    },
                );
                self.set_route_installed(identity, shard, false);
            }
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "active views are staged only after their origin and remote placements are reserved"
    )]
    fn stage_active_views(
        &mut self,
        identity: TrackAncestry,
        desired: &DesiredTrackState,
        stager: &mut TrackLifecycleStager,
    ) {
        let origin = self
            .placements
            .get(&(identity, desired.origin.shard))
            .copied()
            .expect("origin placement must exist before staging its track view");
        let origin_was_resident = origin.resident;

        if desired.track.requires_reverse_route() {
            if !origin.route_installed {
                stager.update(
                    desired.origin.shard,
                    ShardUpdateOp::InstallRoute {
                        address: origin.placement.route,
                        action: TrackRouteAction::Reverse {
                            publication_id: origin.placement.publication_id,
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
                self.placements
                    .get(&(identity, shard))
                    .expect("active remote shard must have a reserved placement")
                    .placement
                    .route
            })
            .collect();

        self.stage_runtime_and_plan(
            stager,
            RuntimePlan {
                shard: desired.origin.shard,
                origin: desired.origin.connection,
                track: &desired.track,
                publication_id: origin.placement.publication_id,
                local: desired.local_bindings(desired.origin.shard),
                remote,
                reverse: desired.track.reverse(),
            },
        );
        self.set_resident(identity, desired.origin.shard, true);

        if !origin_was_resident {
            stager.participant(
                desired.origin,
                ParticipantEffect::TrackPublished {
                    publication_id: identity.publication_id,
                },
            );
        }

        for shard in desired.candidate_remote_shards() {
            let destination = self
                .placements
                .get(&(identity, shard))
                .copied()
                .expect("candidate remote shard must have a reserved placement");
            debug_assert_eq!(
                destination.route_installed,
                desired.active_remote_shards().contains(&shard)
            );

            self.stage_runtime_and_plan(
                stager,
                RuntimePlan {
                    shard,
                    origin: desired.origin.connection,
                    track: &desired.track,
                    publication_id: destination.placement.publication_id,
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
                publication_id: runtime.publication_id,
                runtime: TrackRuntime {
                    descriptor: Some(TrackDescriptor {
                        origin: runtime.origin,
                        kind: runtime.track.kind(),
                        encodings: runtime
                            .track
                            .layers()
                            .iter()
                            .map(|layer| layer.encoding)
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
            runtime.publication_id,
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
        stager.plan(shard, destination.placement.publication_id, None);
        if destination.route_installed {
            stager.update(
                shard,
                ShardUpdateOp::RetireRoute {
                    address: destination.placement.route,
                },
            );
        }
        stager.update(
            shard,
            ShardUpdateOp::RemoveTrackRuntime {
                publication_id: destination.placement.publication_id,
            },
        );
    }

    fn stage_track_effect(
        &self,
        identity: TrackAncestry,
        location: Option<&BoundParticipantLocation>,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
        effect: ParticipantEffect,
    ) {
        let Some(location) = location.and_then(|location| {
            BoundParticipantLocation::for_subscriber(
                registry,
                Subscriber::Connection(location.connection),
            )
            .filter(|current| current.connection == location.connection)
        }) else {
            return;
        };
        let participant = location.participant;
        let requires_placement = matches!(
            effect,
            ParticipantEffect::TrackCandidateAdded { .. }
                | ParticipantEffect::TrackSubscribed { .. }
        );
        if !self.placements.contains_key(&(identity, location.shard)) {
            if requires_placement {
                debug_assert!(
                    false,
                    "a live lifecycle participant must have a track placement: identity={identity:?} participant={participant:?} shard={:?} effect={effect:?}",
                    location.shard,
                );
            }
            return;
        }

        stager.participant(location, effect);
    }

    fn stage_track_removal(
        &mut self,
        identity: TrackAncestry,
        registry: &RoomRegistry,
        stager: &mut TrackLifecycleStager,
    ) {
        let locations = self.locations.remove(&identity).unwrap_or_default();
        let bindings = self.bindings.remove(&identity).unwrap_or_default();
        let candidates = self.candidates.remove(&identity).unwrap_or_default();

        let mut bindings = bindings.into_iter().collect::<Vec<_>>();
        bindings.sort();
        for participant in bindings {
            self.stage_track_effect(
                identity,
                locations.get(&participant),
                registry,
                stager,
                ParticipantEffect::TrackUnsubscribed {
                    publication_id: identity.publication_id,
                },
            );
        }

        let mut candidates = candidates.into_iter().collect::<Vec<_>>();
        candidates.sort();
        for participant in candidates {
            self.stage_track_effect(
                identity,
                locations.get(&participant),
                registry,
                stager,
                ParticipantEffect::TrackCandidateRemoved {
                    publication_id: identity.publication_id,
                    reason: crate::participant::effect::TrackRemoval::SourceEnded,
                },
            );
        }

        if let Some(origin) =
            BoundParticipantLocation::in_room(registry, &identity.publisher, identity.room_id)
            && let Some(destination) = self.placements.get(&(identity, origin.shard))
            && destination.resident
        {
            stager.participant(
                origin,
                ParticipantEffect::TrackUnpublished {
                    publication_id: identity.publication_id,
                },
            );
        }

        let mut placements = self
            .placements
            .iter()
            .filter_map(|((held, shard), destination)| {
                (*held == identity).then_some((*shard, *destination))
            })
            .collect::<Vec<_>>();
        placements.sort_by_key(|(shard, _)| shard.index());

        for (shard, destination) in placements {
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
        identity: TrackAncestry,
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
            let removed = self.placements.remove(&(identity, shard));
            debug_assert!(removed.is_some());
            self.allocator.release(destination.placement, now);
        }
    }

    fn release_placements(&mut self, identity: TrackAncestry, now: Instant) {
        debug_assert!(
            !self.topology.contains(identity),
            "placement release follows committed publication removal"
        );
        let placements = self
            .placements
            .extract_if(|(held, _), _| *held == identity)
            .map(|(_, destination)| destination.placement)
            .collect::<Vec<_>>();

        for placement in placements {
            self.allocator.release(placement, now);
        }

        self.candidates.remove(&identity);
        self.bindings.remove(&identity);
    }

    fn remote_destinations(
        &self,
        identity: TrackAncestry,
        origin_shard: ShardId,
    ) -> Vec<(ShardId, TrackDestination)> {
        let mut destinations = self
            .placements
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
        reason = "route state is changed only for a placement reserved by this lifecycle"
    )]
    fn set_route_installed(&mut self, identity: TrackAncestry, shard: ShardId, installed: bool) {
        let destination = self
            .placements
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.route_installed = installed;
    }

    #[allow(
        clippy::expect_used,
        reason = "residency is changed only for a placement reserved by this lifecycle"
    )]
    fn set_resident(&mut self, identity: TrackAncestry, shard: ShardId, resident: bool) {
        let destination = self
            .placements
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.resident = resident;
    }

    #[allow(
        clippy::expect_used,
        reason = "destination state is changed only for a placement reserved by this lifecycle"
    )]
    fn set_destination_state(
        &mut self,
        identity: TrackAncestry,
        shard: ShardId,
        resident: bool,
        route_installed: bool,
    ) {
        let destination = self
            .placements
            .get_mut(&(identity, shard))
            .expect("track destination disappeared during reconciliation");
        destination.resident = resident;
        destination.route_installed = route_installed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        entity::{RoomId, TrackKind},
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

    fn room(seed: u8) -> ProjectRoomKey {
        let name = match seed {
            1 => "lifecycle-1",
            2 => "lifecycle-2",
            _ => "lifecycle-other",
        };
        ProjectRoomKey::fixture(RoomId::new(name).unwrap())
    }

    fn endpoint(seed: u8, room_seed: u8) -> EndpointKey {
        EndpointKey {
            room: room(room_seed),
            participant: participant(seed),
        }
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::new(&format!("participant-{seed}")).unwrap()
    }

    fn register(registry: &mut RoomRegistry, seed: u8, room_id: ProjectRoomKey, shard: usize) {
        let participant = participant(seed);
        registry.add_participant(participant, room_id.room, ShardId::new(shard), None);
        registry.mark_materialized(&participant);
    }

    fn track(kind: TrackKind, publisher: u8, room_seed: u8, label: &str) -> Track {
        let connection = crate::entity::ConnectionId::new();
        let publisher = participant(publisher);
        let meta = TrackMeta {
            room: room(room_seed),
            shard_id: ShardId::new(0),
            id: connection.fixture_publication(kind, label),
            origin: publisher,
            connection_id: connection,
            track_id: Some(
                crate::entity::TrackId::new(label.strip_prefix("rel:").unwrap_or(label)).unwrap(),
            ),
        };
        match kind {
            TrackKind::Data => {
                let (lane, topic) = label
                    .strip_prefix("rel:")
                    .map_or((DataLane::Latest, label), |topic| {
                        (DataLane::Ordered, topic)
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
                        TrackRouteAction::Forward { .. } => kinds.push(OpKind::InstallForward),
                        TrackRouteAction::Reverse { .. } => kinds.push(OpKind::InstallReverse),
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
                        TrackRouteAction::Forward { .. } => kinds.push(OpKind::InstallForward),
                        TrackRouteAction::Reverse { .. } => kinds.push(OpKind::InstallReverse),
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

    fn plan_updates_on(
        outcome: &TrackLifecycleOutcome,
        shard: usize,
    ) -> Vec<(PublicationKey, bool)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .filter_map(|operation| match operation {
                TrackLifecycleOperation::Plans { shard: held, plans } if *held == shard => Some(
                    plans
                        .iter()
                        .map(|update| (update.publication_id, update.plan.is_some())),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn forward_install_on(
        outcome: &TrackLifecycleOutcome,
        shard: usize,
    ) -> Option<(NodeRouteAddress, PublicationKey)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::Update {
                    shard: held,
                    op:
                        ShardUpdateOp::InstallRoute {
                            address,
                            action: TrackRouteAction::Forward { publication_id },
                        },
                } if *held == shard => Some((*address, *publication_id)),
                _ => None,
            })
    }

    fn reverse_install_on(
        outcome: &TrackLifecycleOutcome,
        shard: usize,
    ) -> Option<(NodeRouteAddress, PublicationKey)> {
        let shard = ShardId::new(shard);
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::Update {
                    shard: held,
                    op:
                        ShardUpdateOp::InstallRoute {
                            address,
                            action: TrackRouteAction::Reverse { publication_id },
                        },
                } if *held == shard => Some((*address, *publication_id)),
                _ => None,
            })
    }

    fn published_track_id(outcome: &TrackLifecycleOutcome) -> Option<PublicationKey> {
        outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    effect: ParticipantEffect::TrackPublished { publication_id },
                    ..
                } => Some(*publication_id),
                _ => None,
            })
    }

    fn destination(
        lifecycle: &TrackLifecycle,
        identity: TrackAncestry,
        shard: usize,
    ) -> TrackDestination {
        *lifecycle
            .placements
            .get(&(identity, ShardId::new(shard)))
            .expect("destination placement must exist")
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "bindings must be candidates")]
    fn committed_track_rejects_a_binding_without_a_candidate() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        lifecycle
            .bindings
            .get_mut(&identity)
            .unwrap()
            .insert(Subscriber::Joined(endpoint(2, 1)));
        lifecycle.assert_committed_track(identity, &registry);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "committed candidate shard must retain its placement")]
    fn committed_track_rejects_a_missing_origin_placement() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        lifecycle.placements.remove(&(identity, ShardId::new(0)));
        lifecycle.assert_committed_track(identity, &registry);
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
                let location = BoundParticipantLocation::for_subscriber(registry, *participant)
                    .expect("retained candidate must remain bound in its admitted room");
                assert!(
                    lifecycle
                        .placements
                        .contains_key(&(*identity, location.shard)),
                    "every retained candidate shard must have a placement"
                );
            }
        }

        for ((identity, shard), destination) in &lifecycle.placements {
            assert!(
                lifecycle.topology.contains(*identity),
                "placements must only be retained for published tracks"
            );
            if destination.route_installed {
                assert!(
                    destination.resident,
                    "installed routes must have a resident runtime"
                );
            }
            let origin =
                BoundParticipantLocation::in_room(registry, &identity.publisher, identity.room_id)
                    .expect("published track must retain a bound publisher");
            if *shard == origin.shard {
                let track = lifecycle
                    .topology
                    .track(*identity)
                    .expect("placement identity must still resolve to its track");
                assert_eq!(
                    destination.route_installed,
                    track.requires_reverse_route(),
                    "origin route residency must exactly match reverse-route semantics"
                );
            } else {
                let has_candidate = lifecycle.candidates[identity].iter().any(|participant| {
                    BoundParticipantLocation::for_subscriber(registry, *participant)
                        .is_some_and(|location| location.shard == *shard)
                });
                let has_binding = lifecycle.bindings[identity].iter().any(|participant| {
                    BoundParticipantLocation::for_subscriber(registry, *participant)
                        .is_some_and(|location| location.shard == *shard)
                });
                assert_eq!(
                    destination.resident, has_candidate,
                    "remote runtime residency must exactly match candidate state"
                );
                assert_eq!(
                    destination.route_installed, has_binding,
                    "remote forward-route residency must exactly match active binding state"
                );
            }
        }

        for identity in lifecycle.topology.identities() {
            let origin =
                BoundParticipantLocation::in_room(registry, &identity.publisher, identity.room_id)
                    .expect("published track must retain a bound publisher");
            let destination = lifecycle
                .placements
                .get(&(identity, origin.shard))
                .expect("published track must retain its origin placement");
            assert!(destination.resident, "origin runtime must remain resident");
        }
    }

    fn publish_audio(
        lifecycle: &mut TrackLifecycle,
        registry: &RoomRegistry,
        publisher: u8,
        room_seed: u8,
    ) -> (TrackAncestry, TrackLifecycleOutcome) {
        let track = track(TrackKind::Audio, publisher, room_seed, "audio");
        let identity = TrackAncestry::from_track(&track);
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
        assert_eq!(lifecycle.placements.len(), 1);
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());

        let origin = destination(&lifecycle, identity, 0);
        assert!(origin.resident);
        assert_eq!(
            published_track_id(&outcome),
            Some(origin.placement.publication_id)
        );
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
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));
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
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));
        assert!(forward_install_on(&outcome, 0).is_none());
        assert_eq!(lifecycle.placements.len(), 1);
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
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(!lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));

        let activate = lifecycle.activate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&activate), vec![EffectKind::Subscribed]);
        assert!(lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));

        let deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&deactivate), vec![EffectKind::Unsubscribed]);
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(!lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn remote_candidate_installs_a_catalog_runtime_without_a_forward_route() {
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
        assert!(remote.resident);
        assert!(!remote.route_installed);
        assert!(forward_install_on(&outcome, 1).is_none());
        assert!(operations_on(&outcome, 1).contains(&OpKind::InsertRuntime));
        assert!(plan_updates_on(&outcome, 1).contains(&(remote.placement.publication_id, true)));
        assert_eq!(lifecycle.placements.len(), 2);
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
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );

        let remote = destination(&lifecycle, identity, 1);
        assert!(remote.resident);
        assert!(remote.route_installed);
        assert_eq!(
            forward_install_on(&outcome, 1),
            Some((remote.placement.route, remote.placement.publication_id))
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
            vec![(remote.placement.publication_id, true)]
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn remote_deactivation_withdraws_view_but_keeps_candidate_placement() {
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
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        let held = destination(&lifecycle, identity, 1).placement;

        let outcome = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );

        let remote = destination(&lifecycle, identity, 1);
        assert_eq!(remote.placement, held);
        assert!(remote.resident);
        assert!(!remote.route_installed);
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(!lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));
        assert_eq!(
            operations_on(&outcome, 1),
            vec![
                OpKind::Unsubscribed,
                OpKind::RetireRoute,
                OpKind::InsertRuntime,
                OpKind::Placeholder,
                OpKind::PlanSet,
            ]
        );
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn network_branches_require_a_current_publication_and_reverse_capability() {
        let (mut lifecycle, registry) = setup(2, &[(1, 1, 0), (2, 1, 1)]);
        let connection = registry
            .get_participant(&participant(1))
            .unwrap()
            .connection_id;
        for kind in [TrackKind::Audio, TrackKind::Video] {
            let mut publication = track(kind, 1, 1, "network");
            publication.meta_mut().connection_id = connection;
            publication.meta_mut().id = connection.fixture_publication(kind, "network");
            let id = publication.id();
            assert!(
                lifecycle
                    .publish(publication, &registry, Instant::now())
                    .is_some()
            );
            assert_eq!(
                lifecycle.network_source(room(1), id, &registry),
                Some(ShardId::new(0))
            );
            assert_eq!(lifecycle.network_source(room(2), id, &registry), None);
            assert!(!lifecycle.allows_network_reverse(room(1), id, ShardId::new(0), &registry));
            assert_eq!(
                lifecycle.allows_network_reverse(room(1), id, ShardId::new(1), &registry),
                kind == TrackKind::Video
            );
            assert!(
                lifecycle
                    .unpublish(
                        EndpointKey {
                            room: room(1),
                            participant: participant(1)
                        },
                        id,
                        &registry,
                        Instant::now()
                    )
                    .is_some()
            );
            assert_eq!(lifecycle.network_source(room(1), id, &registry), None);
        }
    }

    #[test]
    fn removing_last_remote_candidate_releases_its_reserved_placement() {
        let (mut lifecycle, mut registry) = setup(2, &[(1, 1, 0), (2, 1, 1), (3, 2, 1)]);
        let mut publication = track(TrackKind::Audio, 1, 1, "audio");
        let connection = registry
            .get_participant(&participant(1))
            .unwrap()
            .connection_id;
        publication.meta_mut().connection_id = connection;
        publication.meta_mut().id = connection.fixture_publication(TrackKind::Audio, "audio");
        let identity = TrackAncestry::from_track(&publication);
        let _ = lifecycle.publish(publication, &registry, Instant::now());
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(2),
                    identity.publication_id,
                    &registry
                )
                .is_some()
        );
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_some()
        );
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(2),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(1),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(9),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        let denied = super::super::topology::OwnedSubscription {
            source_room: room(2),
            subscriber: participant(3),
            reference: 1,
            track: identity.publication_id,
            enabled: true,
        };
        let denied_connection = registry.get_endpoint(endpoint(3, 2)).unwrap().connection_id;
        assert!(
            lifecycle
                .set_owned_subscription(denied_connection, denied, &registry, Instant::now())
                .is_empty()
        );
        assert!(
            lifecycle
                .topology
                .remove_owned(room(2), Subscriber::Connection(denied_connection), 1)
                .is_none()
        );
        let _ = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
            &registry,
            Instant::now(),
        );
        let remote_track_id = destination(&lifecycle, identity, 1)
            .placement
            .publication_id;

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
                .placements
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(plan_updates_on(&outcome, 1).contains(&(remote_track_id, false)));
        assert_internal_invariants(&lifecycle, &registry);
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(2),
                    identity.publication_id,
                    &registry
                )
                .is_some()
        );
        registry.add_participant(participant(2), room(1).room, ShardId::new(1), None);
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(2),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_some()
        );
        registry.mark_materialized(&participant(2));
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(2),
                    identity.publication_id,
                    &registry
                )
                .is_some()
        );
        registry.add_participant(participant(1), room(1).room, ShardId::new(0), None);
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        registry.mark_materialized(&participant(1));
        assert!(
            lifecycle
                .source(
                    room(1),
                    room(1),
                    participant(2),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
        let _ = lifecycle.unpublish(
            EndpointKey {
                room: identity.room_id,
                participant: identity.publisher,
            },
            identity.publication_id,
            &registry,
            Instant::now(),
        );
        assert!(
            lifecycle
                .topology
                .source(room(1), identity.publication_id)
                .is_none()
        );
        assert!(
            lifecycle
                .source(
                    room(2),
                    room(1),
                    participant(3),
                    identity.publication_id,
                    &registry
                )
                .is_none()
        );
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
                .placements
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
        let first = destination(&lifecycle, identity, 1).placement;
        let _ = lifecycle.subscribe(
            room(1),
            participant(3),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );
        let second = destination(&lifecycle, identity, 1).placement;

        assert_eq!(first, second);
        assert_eq!(lifecycle.placements.len(), 2);

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
                .placements
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
                .placements
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(destination(&lifecycle, identity, 2).resident);
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn overlapping_subscriptions_do_not_duplicate_candidate_or_binding_state() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let (identity, _) = publish_audio(&mut lifecycle, &registry, 1, 1);
        let exact = TrackSelector::track(identity.publication_id);

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
        assert!(lifecycle.candidates[&identity].contains(&endpoint(2, 1).into()));
        assert!(lifecycle.bindings[&identity].contains(&endpoint(2, 1).into()));

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
    fn indexed_exact_source_lookup_fences_self_stale_and_ambiguous_publications() {
        use crate::{
            control::controller::SignalingProtocol,
            entity::{ConnectionId, ParticipantId},
            route::{NodeTransportAddress, TransportRoute},
        };
        use std::num::NonZeroU64;

        let room_id = room(1);
        let subscriber = participant(1);
        let publisher_external = ParticipantId::new("publisher").unwrap();
        let publisher = publisher_external;
        let mut registry = RoomRegistry::new();
        register(&mut registry, 1, room_id, 0);
        let connection = ConnectionId::new();
        registry
            .commit_candidate(
                publisher,
                room_id,
                ShardId::new(0),
                1,
                NodeTransportAddress::new(TransportRoute::new(ShardId::new(0), 7), 1),
                connection,
                SignalingProtocol::State,
            )
            .unwrap();
        let publication = |incarnation| {
            Track::data_publication(
                TrackMeta {
                    room: room_id,
                    shard_id: ShardId::new(0),
                    id: connection.fixture_publication_at(
                        TrackKind::Data,
                        "label",
                        NonZeroU64::new(incarnation).unwrap(),
                    ),
                    origin: publisher,
                    connection_id: connection,
                    track_id: Some(crate::entity::TrackId::new("label").unwrap()),
                },
                DataLane::Latest,
            )
        };
        let first = publication(1);
        let second = publication(2);
        let mut lifecycle = TrackLifecycle::new(1);
        lifecycle.publish(first, &registry, Instant::now()).unwrap();
        for index in 0..65_u32 {
            let external = ParticipantId::new(&format!("other-{index}")).unwrap();
            let other = external;
            let connection = ConnectionId::new();
            registry
                .commit_candidate(
                    other,
                    room_id,
                    ShardId::new(0),
                    index as usize + 2,
                    NodeTransportAddress::new(TransportRoute::new(ShardId::new(0), index + 9), 1),
                    connection,
                    SignalingProtocol::State,
                )
                .unwrap();
            let track = Track::data_publication(
                TrackMeta {
                    room: room_id,
                    shard_id: ShardId::new(0),
                    id: connection.fixture_publication_at(
                        TrackKind::Data,
                        "label",
                        NonZeroU64::new(1).unwrap(),
                    ),
                    origin: other,
                    connection_id: connection,
                    track_id: Some(crate::entity::TrackId::new("label").unwrap()),
                },
                DataLane::Latest,
            );
            lifecycle.publish(track, &registry, Instant::now()).unwrap();
        }
        let connection_for = |participant| {
            registry
                .get_endpoint(EndpointKey {
                    room: room_id,
                    participant,
                })
                .unwrap()
                .connection_id
        };
        let subscriber_connection = connection_for(subscriber);
        let publisher_connection = connection_for(publisher);
        let address = pulsebeam_proto::state::LogicalSourceAddress {
            room: "lifecycle-1".into(),
            participant_id: publisher_external.as_str().into(),
            kind: pulsebeam_proto::state::TrackKind::Data as i32,
            track_id: "label".into(),
        };
        assert!(
            lifecycle
                .source_by_address(room_id, subscriber_connection, &address, &registry)
                .is_some()
        );
        assert!(
            lifecycle
                .source_by_address(room_id, publisher_connection, &address, &registry)
                .is_none()
        );

        lifecycle
            .publish(second, &registry, Instant::now())
            .unwrap();
        assert!(
            lifecycle
                .source_by_address(room_id, subscriber_connection, &address, &registry)
                .is_none()
        );

        let replacement = ConnectionId::new();
        registry
            .commit_candidate(
                publisher,
                room_id,
                ShardId::new(0),
                1,
                NodeTransportAddress::new(TransportRoute::new(ShardId::new(0), 8), 1),
                replacement,
                SignalingProtocol::State,
            )
            .unwrap();
        assert!(
            lifecycle
                .source_by_address(room_id, subscriber_connection, &address, &registry)
                .is_none()
        );
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
        let connection = registry
            .get_participant(&participant(1))
            .unwrap()
            .connection_id;
        let mut publication = track(TrackKind::Audio, 1, 1, "audio");
        publication.meta_mut().connection_id = connection;
        publication.meta_mut().id = connection.fixture_publication(TrackKind::Audio, "audio");
        let identity = TrackAncestry::from_track(&publication);
        lifecycle
            .publish(publication, &registry, Instant::now())
            .unwrap();

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
                .placements
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert_internal_invariants(&lifecycle, &registry);

        let connection = registry.get_endpoint(endpoint(2, 2)).unwrap().connection_id;
        let owner = Subscriber::Connection(connection);
        let owned = || super::super::topology::OwnedSubscription {
            source_room: room(1),
            subscriber: participant(2),
            reference: 1,
            track: identity.publication_id,
            enabled: true,
        };
        let admitted =
            lifecycle.set_owned_subscription(connection, owned(), &registry, Instant::now());
        assert!(!admitted.is_empty());
        assert_eq!(lifecycle.candidates[&identity], HashSet::from([owner]));
        assert_eq!(lifecycle.bindings[&identity], HashSet::from([owner]));
        assert_internal_invariants(&lifecycle, &registry);

        lifecycle.set_owned_subscription(
            connection,
            super::super::topology::OwnedSubscription {
                enabled: false,
                ..owned()
            },
            &registry,
            Instant::now(),
        );
        assert_eq!(lifecycle.candidates[&identity], HashSet::from([owner]));
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);

        lifecycle.unsubscribe_owned(room(1), owner, 1, &registry, Instant::now());
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);

        lifecycle.set_owned_subscription(connection, owned(), &registry, Instant::now());
        assert!(
            lifecycle
                .remove_participant(
                    EndpointKey {
                        room: room(1),
                        participant: participant(2),
                    },
                    &registry,
                    Instant::now(),
                )
                .iter()
                .all(|outcome| effect_kinds(outcome).is_empty())
        );
        assert_eq!(lifecycle.bindings[&identity], HashSet::from([owner]));
        lifecycle.remove_participant(
            EndpointKey {
                room: room(2),
                participant: participant(2),
            },
            &registry,
            Instant::now(),
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn unbound_subscriber_is_not_compiled_into_the_track() {
        let mut registry = RoomRegistry::new();
        register(&mut registry, 1, room(1), 0);
        registry.add_participant(participant(2), room(1).room, ShardId::new(1), None);
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
                .placements
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
            let identity = TrackAncestry::from_track(&publication);
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
                    TrackSelector::track(identity.publication_id),
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
            .unpublish(
                EndpointKey {
                    room: room(1),
                    participant: participant(1),
                },
                identity.publication_id,
                &registry,
                Instant::now(),
            )
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
        assert!(lifecycle.placements.is_empty());
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
            .unpublish(
                EndpointKey {
                    room: room(1),
                    participant: participant(1),
                },
                identity.publication_id,
                &registry,
                Instant::now(),
            )
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
        assert!(lifecycle.placements.is_empty());
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

        let outcomes = lifecycle.remove_participant(
            EndpointKey {
                room: room(1),
                participant: participant(2),
            },
            &registry,
            Instant::now(),
        );
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            effect_kinds(&outcomes[0]),
            vec![EffectKind::Unsubscribed, EffectKind::CandidateRemoved]
        );
        assert!(
            !lifecycle
                .placements
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
        let video_identity = TrackAncestry::from_track(&video);
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

        let outcomes = lifecycle.remove_participant(
            EndpointKey {
                room: room(1),
                participant: participant(1),
            },
            &registry,
            Instant::now(),
        );

        assert_eq!(
            outcomes.len(),
            1,
            "all retiring tracks share one removal generation"
        );
        assert!(!lifecycle.topology.contains(audio));
        assert!(!lifecycle.topology.contains(video_identity));
        assert!(lifecycle.placements.is_empty());
        assert!(lifecycle.candidates.is_empty());
        assert!(lifecycle.bindings.is_empty());
    }

    #[test]
    fn removing_unrelated_participant_emits_no_generation() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let _ = publish_audio(&mut lifecycle, &registry, 1, 1);
        let generation = lifecycle.generation;

        let outcomes = lifecycle.remove_participant(
            EndpointKey {
                room: room(1),
                participant: participant(2),
            },
            &registry,
            Instant::now(),
        );
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

        let outcomes = lifecycle.remove_participant(
            EndpointKey {
                room: room(1),
                participant: participant(2),
            },
            &registry,
            Instant::now(),
        );
        assert_eq!(outcomes.len(), 1);
        assert!(effect_kinds(&outcomes[0]).is_empty());
        assert!(
            !lifecycle
                .placements
                .contains_key(&(identity, ShardId::new(1)))
        );
        assert!(lifecycle.candidates[&identity].is_empty());
        assert!(lifecycle.bindings[&identity].is_empty());
        assert_internal_invariants(&lifecycle, &registry);
    }

    #[test]
    fn generation_advances_once_per_reconciled_track() {
        let (mut lifecycle, registry) = setup(1, &[(1, 1, 0), (2, 1, 0)]);
        let _ = publish_audio(&mut lifecycle, &registry, 1, 1);
        let video = track(TrackKind::Video, 1, 1, "video");
        let _ = lifecycle.publish(video, &registry, Instant::now()).unwrap();
        let before = lifecycle.generation;

        let outcomes = lifecycle.subscribe(
            room(1),
            participant(2),
            TrackSelector::audio(),
            SelectionPolicy::All,
            &registry,
            Instant::now(),
        );

        assert_eq!(outcomes.len(), 1);
        assert_eq!(lifecycle.generation, before + 1);
        assert_eq!(outcomes[0].generation, before + 1);
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
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert_eq!(effect_kinds(&first_activate), vec![EffectKind::Subscribed]);
        assert!(forward_install_on(&first_activate, 1).is_some());

        let second_activate = lifecycle.activate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert!(effect_kinds(&second_activate).is_empty());
        assert!(forward_install_on(&second_activate, 1).is_none());

        let first_deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert_eq!(
            effect_kinds(&first_deactivate),
            vec![EffectKind::Unsubscribed]
        );
        assert!(operations_on(&first_deactivate, 1).contains(&OpKind::RetireRoute));

        let second_deactivate = lifecycle.deactivate(
            room(1),
            participant(1),
            identity.publication_id,
            endpoint(2, 1),
            &registry,
            Instant::now(),
        );
        assert!(effect_kinds(&second_deactivate).is_empty());
        assert!(!operations_on(&second_deactivate, 1).contains(&OpKind::RetireRoute));
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

        assert_eq!(targets, vec![participant(2), participant(3)]);
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

        let placements = [
            destination(&lifecycle, identity, 1).placement,
            destination(&lifecycle, identity, 2).placement,
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
                    identity.publication_id,
                    EndpointKey {
                        room: room(1),
                        participant: subscriber,
                    },
                    &registry,
                    Instant::now(),
                );
                active.insert(subscriber);
            } else {
                let _ = lifecycle.deactivate(
                    room(1),
                    participant(1),
                    identity.publication_id,
                    EndpointKey {
                        room: room(1),
                        participant: subscriber,
                    },
                    &registry,
                    Instant::now(),
                );
                active.remove(&subscriber);
            }

            assert_eq!(
                lifecycle.bindings[&identity],
                active
                    .iter()
                    .map(|participant| Subscriber::Joined(EndpointKey {
                        room: room(1),
                        participant: *participant,
                    }))
                    .collect::<HashSet<_>>()
            );
            assert_eq!(
                destination(&lifecycle, identity, 1).placement,
                placements[0]
            );
            assert_eq!(
                destination(&lifecycle, identity, 2).placement,
                placements[1]
            );

            for shard in [1_usize, 2] {
                let shard_active = (2_u8..=5).any(|candidate| {
                    let candidate_shard = if candidate <= 3 { 1 } else { 2 };
                    candidate_shard == shard && active.contains(&participant(candidate))
                });
                let destination = destination(&lifecycle, identity, shard);
                assert!(destination.resident);
                assert_eq!(destination.route_installed, shard_active);
            }
            assert_internal_invariants(&lifecycle, &registry);
        }
    }

    #[test]
    fn long_activation_churn_preserves_placement_and_state_invariants() {
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
        let placement = destination(&lifecycle, identity, 1).placement;

        for _ in 0..1_000 {
            let activate = lifecycle.activate(
                room(1),
                participant(1),
                identity.publication_id,
                endpoint(2, 1),
                &registry,
                Instant::now(),
            );
            assert_eq!(effect_kinds(&activate), vec![EffectKind::Subscribed]);
            assert_eq!(destination(&lifecycle, identity, 1).placement, placement);
            assert_internal_invariants(&lifecycle, &registry);

            let deactivate = lifecycle.deactivate(
                room(1),
                participant(1),
                identity.publication_id,
                endpoint(2, 1),
                &registry,
                Instant::now(),
            );
            assert_eq!(effect_kinds(&deactivate), vec![EffectKind::Unsubscribed]);
            assert_eq!(destination(&lifecycle, identity, 1).placement, placement);
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
                Some((remote.placement.route, remote.placement.publication_id))
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
    fn every_candidate_effect_carries_the_stable_track_id() {
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
        let effect_track_id = outcome
            .operations
            .iter()
            .find_map(|operation| match operation {
                TrackLifecycleOperation::ParticipantEffect {
                    shard,
                    effect: ParticipantEffect::TrackCandidateAdded { track },
                    ..
                } if *shard == ShardId::new(1) => Some(track.id()),
                _ => None,
            });

        assert_eq!(effect_track_id, Some(remote.placement.publication_id));
        assert_eq!(
            remote.placement.publication_id,
            destination(&lifecycle, identity, 0)
                .placement
                .publication_id
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
            identity.publication_id,
            endpoint(2, 1),
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
