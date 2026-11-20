use std::collections::{BTreeMap, HashMap};
use std::ops::Bound::{Excluded, Unbounded};

use crate::{
    control::controller::SignalingProtocol,
    control::room::RoomShard,
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectRoomKey, RoomId},
    id::ShardId,
    route::NodeTransportAddress,
};

/// Everything the control plane knows about one participant.
///
/// The single owner of `participant -> (shard, room)`. It used to be three
/// indexes — this registry, the lifecycle state, and a copy on every shard —
/// which is three chances for them to disagree about where somebody is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantMeta {
    pub shard_id: ShardId,
    pub room: ProjectRoomKey,
    /// The client's ICE association, kept so teardown can retire it. The
    /// route outlives the negotiation that produced it, so something has to
    /// remember it, and this is the record that already knows who it belongs
    /// to.
    pub transport: Option<NodeTransportAddress>,
    pub connection_id: ConnectionId,
    pub profile: SignalingProtocol,
    pub authorization: Option<super::controller::AuthorizationLease>,
    pub materialized: bool,
}

impl ParticipantMeta {
    pub fn room_id(&self) -> RoomId {
        self.room.room
    }
}

#[derive(Default)]
struct RoomParticipants {
    version: u64,
    ordered: BTreeMap<String, ParticipantId>,
}

pub struct RoomRegistry {
    room_shards: HashMap<ProjectRoomKey, RoomShard>,
    participants: HashMap<EndpointKey, ParticipantMeta>,
    participants_by_room: HashMap<ProjectRoomKey, RoomParticipants>,
    catalog_version: u64,
    connections: HashMap<ConnectionId, EndpointKey>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            room_shards: HashMap::new(),
            participants: HashMap::new(),
            participants_by_room: HashMap::new(),
            catalog_version: 0,
            connections: HashMap::new(),
        }
    }

    pub fn get_room_shard(&self, room_id: &ProjectRoomKey) -> Option<&RoomShard> {
        self.room_shards.get(room_id)
    }

    pub fn reserve_partition(
        &mut self,
        room: ProjectRoomKey,
        participant: ParticipantId,
        connection: ConnectionId,
        shard: ShardId,
        index: usize,
    ) {
        self.room_shards
            .entry(room)
            .or_insert_with(RoomShard::new)
            .reserve(participant, connection, shard, index);
    }

    pub fn partition_reserved(
        &self,
        room: ProjectRoomKey,
        participant: ParticipantId,
        connection: ConnectionId,
        shard: ShardId,
        index: usize,
    ) -> bool {
        self.room_shards
            .get(&room)
            .is_some_and(|room| room.reserved(participant, connection, shard, index))
    }

    pub fn release_partition(
        &mut self,
        room_id: ProjectRoomKey,
        participant: ParticipantId,
        connection: ConnectionId,
    ) {
        let empty = if let Some(room) = self.room_shards.get_mut(&room_id) {
            room.release(participant, connection);
            room.is_empty()
        } else {
            false
        };
        if empty {
            self.room_shards.remove(&room_id);
        }
    }

    #[cfg(test)]
    pub fn add_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        transport: Option<NodeTransportAddress>,
    ) {
        let room_id = ProjectRoomKey::fixture(room_id);
        let endpoint = EndpointKey {
            room: room_id,
            participant: participant_id,
        };
        let connection_id = ConnectionId::new();
        if let Some(previous) = self.participants.insert(
            endpoint,
            ParticipantMeta {
                shard_id,
                room: room_id,
                transport,
                connection_id,
                profile: SignalingProtocol::State,
                authorization: None,
                materialized: false,
            },
        ) {
            self.connections.remove(&previous.connection_id);
            self.unindex_participant(previous.room, participant_id);
            self.remove_from_room_shard(&previous.room, &participant_id, previous.shard_id);
        }
        self.index_participant(room_id, participant_id);
        self.connections.insert(connection_id, endpoint);
        let room = self
            .room_shards
            .entry(room_id)
            .or_insert_with(RoomShard::new);
        let partition_index = room.next_partition_index(
            &participant_id,
            1, // Test helper accepts arbitrary execution-shard placement.
        );
        room.add_participant(&participant_id, shard_id, partition_index);
    }

    pub(super) fn participants(&self) -> impl Iterator<Item = &ParticipantMeta> {
        self.participants.values()
    }

    #[cfg(test)]
    fn fixture_endpoint(&self, participant_id: &ParticipantId) -> Option<EndpointKey> {
        let mut endpoints = self
            .participants
            .keys()
            .filter(|key| &key.participant == participant_id);
        let endpoint = endpoints.next().copied();
        assert!(
            endpoints.next().is_none(),
            "ambiguous fixture participant; use a scoped key"
        );
        endpoint
    }

    #[cfg(test)]
    pub fn get_participant(&self, participant_id: &ParticipantId) -> Option<&ParticipantMeta> {
        self.get_endpoint(self.fixture_endpoint(participant_id)?)
    }

    pub(super) fn get_endpoint(&self, endpoint: EndpointKey) -> Option<&ParticipantMeta> {
        self.participants.get(&endpoint)
    }

    pub(super) fn get_connection(
        &self,
        connection: ConnectionId,
    ) -> Option<(ParticipantId, &ParticipantMeta)> {
        let endpoint = *self.connections.get(&connection)?;
        let meta = self.get_endpoint(endpoint)?;
        (meta.connection_id == connection).then_some((endpoint.participant, meta))
    }

    pub(super) fn get_incarnation(
        &self,
        participant: ParticipantId,
        connection: ConnectionId,
    ) -> Option<&ParticipantMeta> {
        let (owner, meta) = self.get_connection(connection)?;
        (owner == participant).then_some(meta)
    }

    #[cfg(test)]
    pub fn local_participant_ids_in_room(&self, room_id: &ProjectRoomKey) -> Vec<ParticipantId> {
        self.ordered_participants_in_room(room_id, None)
            .map(|(_, participant, _)| participant)
            .collect()
    }

    pub fn ordered_participants_in_room<'a>(
        &'a self,
        room_id: &ProjectRoomKey,
        after: Option<&'a str>,
    ) -> impl Iterator<Item = (&'a str, ParticipantId, &'a ParticipantMeta)> + 'a {
        let room = *room_id;
        self.participants_by_room
            .get(room_id)
            .into_iter()
            .flat_map(move |participants| {
                participants
                    .ordered
                    .range::<str, _>((after.map_or(Unbounded, Excluded), Unbounded))
            })
            .filter_map(move |(canonical, participant)| {
                self.get_endpoint(EndpointKey {
                    room,
                    participant: *participant,
                })
                .map(|meta| (canonical.as_str(), *participant, meta))
            })
    }

    pub(super) fn catalog_version(&self) -> u64 {
        self.catalog_version
    }

    pub(super) fn room_catalog_version(&self, room: ProjectRoomKey) -> u64 {
        self.participants_by_room
            .get(&room)
            .map_or(0, |room| room.version)
    }

    fn touch_catalog(&mut self, room: ProjectRoomKey) {
        self.catalog_version = self
            .catalog_version
            .checked_add(1)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("registry catalog version exhausted"));
        if let Some(participants) = self.participants_by_room.get_mut(&room) {
            participants.version = self.catalog_version;
        }
    }

    fn index_participant(&mut self, room: ProjectRoomKey, participant: ParticipantId) {
        self.participants_by_room
            .entry(room)
            .or_default()
            .ordered
            .insert(participant.to_string(), participant);
        self.touch_catalog(room);
    }

    fn unindex_participant(&mut self, room: ProjectRoomKey, participant: ParticipantId) {
        if let Some(participants) = self.participants_by_room.get_mut(&room) {
            participants.ordered.remove(&participant.to_string());
            if participants.ordered.is_empty() {
                self.participants_by_room.remove(&room);
            }
            self.touch_catalog(room);
        }
    }

    /// Atomically installs a prepared local incarnation when it wins UUIDv7
    /// ordering. The ordering is only a local convergence discriminator: this
    /// registry is neither replicated nor durable across process restarts.
    #[expect(
        clippy::too_many_arguments,
        reason = "candidate identity, placement, and transport commit atomically"
    )]
    pub fn commit_candidate(
        &mut self,
        participant_id: ParticipantId,
        room_id: ProjectRoomKey,
        shard_id: ShardId,
        partition_index: usize,
        transport: NodeTransportAddress,
        connection_id: ConnectionId,
        profile: SignalingProtocol,
    ) -> Result<Option<ParticipantMeta>, CommitCandidateError> {
        let endpoint = EndpointKey {
            room: room_id,
            participant: participant_id,
        };
        if self
            .connections
            .get(&connection_id)
            .is_some_and(|owner| *owner != endpoint)
            || self
                .participants
                .get(&endpoint)
                .is_some_and(|current| current.connection_id >= connection_id)
        {
            return Err(CommitCandidateError::Superseded);
        }

        let previous = self.participants.insert(
            endpoint,
            ParticipantMeta {
                shard_id,
                room: room_id,
                transport: Some(transport),
                connection_id,
                profile,
                authorization: None,
                materialized: true,
            },
        );
        self.connections.insert(connection_id, endpoint);
        if let Some(previous) = &previous {
            self.connections.remove(&previous.connection_id);
            self.unindex_participant(previous.room, participant_id);
            // Keep the partition table alive across replacement of its last participant.
            if let Some(room) = self.room_shards.get_mut(&room_id) {
                room.remove_participant(&participant_id, previous.shard_id);
            }
        }
        self.index_participant(room_id, participant_id);
        self.room_shards
            .entry(room_id)
            .or_insert_with(RoomShard::new)
            .add_participant(&participant_id, shard_id, partition_index);
        Ok(previous)
    }

    #[cfg(test)]
    pub fn mark_materialized(&mut self, participant_id: &ParticipantId) {
        if let Some(endpoint) = self.fixture_endpoint(participant_id)
            && let Some(meta) = self.participants.get_mut(&endpoint)
        {
            meta.materialized = true;
        }
    }

    /// The transport route to retire when this participant goes away.
    /// Returns the shard_id that was hosting the participant, if found.
    #[cfg(test)]
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ShardId> {
        let endpoint = self.fixture_endpoint(participant_id)?;
        let connection = self.get_endpoint(endpoint)?.connection_id;
        self.remove_incarnation(participant_id, connection)
            .map(|meta| meta.shard_id)
    }

    pub fn remove_incarnation(
        &mut self,
        participant_id: &ParticipantId,
        connection_id: ConnectionId,
    ) -> Option<ParticipantMeta> {
        let endpoint = *self.connections.get(&connection_id)?;
        if endpoint.participant != *participant_id
            || self.get_endpoint(endpoint)?.connection_id != connection_id
        {
            return None;
        }
        let meta = self.participants.remove(&endpoint)?;
        self.connections.remove(&connection_id);
        self.unindex_participant(endpoint.room, endpoint.participant);
        self.remove_from_room_shard(&endpoint.room, participant_id, meta.shard_id);
        Some(meta)
    }

    pub fn update_authorization(
        &mut self,
        participant_id: &ParticipantId,
        connection_id: ConnectionId,
        authorization: super::controller::AuthorizationLease,
    ) {
        if let Some(endpoint) = self.connections.get(&connection_id).copied()
            && endpoint.participant == *participant_id
            && let Some(meta) = self.participants.get_mut(&endpoint)
            && meta.connection_id == connection_id
        {
            meta.authorization = Some(authorization);
            self.touch_catalog(endpoint.room);
        }
    }

    fn remove_from_room_shard(
        &mut self,
        room_id: &ProjectRoomKey,
        participant_id: &ParticipantId,
        shard_id: ShardId,
    ) {
        let empty = if let Some(room) = self.room_shards.get_mut(room_id) {
            room.remove_participant(participant_id, shard_id);
            room.is_empty()
        } else {
            false
        };
        if empty {
            self.room_shards.remove(room_id);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitCandidateError {
    Superseded,
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::{entity::RoomId, route::TransportRoute};

    fn room_id(s: &str) -> RoomId {
        RoomId::new(s).unwrap()
    }

    fn participant_id() -> ParticipantId {
        crate::entity::fixture_participant_id()
    }

    fn connection_id(sequence: u8) -> ConnectionId {
        ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, sequence])
    }

    fn transport(shard: usize, slot: u32) -> NodeTransportAddress {
        NodeTransportAddress::new(TransportRoute::new(ShardId::new(shard), slot), 1)
    }

    #[test]
    fn identical_room_ids_in_different_projects_do_not_share_placement_or_presence() {
        let mut registry = RoomRegistry::new();
        let first = ProjectRoomKey::fixture(room_id("stage"));
        let second = ProjectRoomKey {
            project: crate::entity::ProjectId::new(),
            ..first
        };
        let name = ParticipantId::new("speaker").unwrap();
        let participant = name;
        let connection = connection_id(1);
        let shard = ShardId::new(0);
        registry.reserve_partition(first, participant, connection, shard, 0);
        assert!(!registry.partition_reserved(second, participant, connection, shard, 0));
        registry
            .commit_candidate(
                participant,
                first,
                shard,
                0,
                transport(0, 1),
                connection,
                SignalingProtocol::State,
            )
            .unwrap();
        assert_eq!(
            registry.local_participant_ids_in_room(&first),
            vec![participant]
        );
        assert!(registry.local_participant_ids_in_room(&second).is_empty());
        assert_eq!(registry.room_catalog_version(second), 0);
        assert!(registry.get_room_shard(&second).is_none());
        assert!(
            registry
                .get_endpoint(EndpointKey {
                    room: first,
                    participant
                })
                .is_some()
        );
        assert!(
            registry
                .get_endpoint(EndpointKey {
                    room: second,
                    participant
                })
                .is_none()
        );

        let join_second = |registry: &mut RoomRegistry, connection| {
            registry.commit_candidate(
                participant,
                second,
                ShardId::new(1),
                0,
                transport(1, 1),
                connection,
                SignalingProtocol::State,
            )
        };
        assert!(matches!(
            join_second(&mut registry, connection),
            Err(CommitCandidateError::Superseded)
        ));
        let other_connection = connection_id(2);
        assert!(
            join_second(&mut registry, other_connection)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            registry.local_participant_ids_in_room(&second),
            vec![participant]
        );
        assert_eq!(registry.get_connection(connection).unwrap().1.room, first);
        assert_eq!(
            registry.get_connection(other_connection).unwrap().1.room,
            second
        );

        assert_eq!(
            registry
                .remove_incarnation(&participant, connection)
                .unwrap()
                .room,
            first
        );
        assert!(registry.local_participant_ids_in_room(&first).is_empty());
        assert_eq!(
            registry.local_participant_ids_in_room(&second),
            vec![participant]
        );
        assert_eq!(
            registry.get_connection(other_connection).unwrap().1.room,
            second
        );
    }

    #[test]
    fn ordered_presence_seeks_are_scoped_and_follow_registry_lifetimes() {
        let mut registry = RoomRegistry::new();
        let room = room_id("catalog");
        let other = room_id("other");
        let mut expected = Vec::new();
        for name in ["zulu", "alpha", "bravo", "echo"] {
            let external = ParticipantId::new(name).unwrap();
            let participant = external;
            registry.add_participant(participant, room, ShardId::new(0), None);
            expected.push(participant.to_string());
        }
        expected.sort();
        let first: Vec<_> = registry
            .ordered_participants_in_room(&ProjectRoomKey::fixture(room), None)
            .take(2)
            .map(|(canonical, _, meta)| {
                assert_eq!(meta.room_id(), room);
                assert!(meta.transport.is_none());
                canonical.to_owned()
            })
            .collect();
        assert_eq!(first, expected[..2]);
        let rest: Vec<_> = registry
            .ordered_participants_in_room(
                &ProjectRoomKey::fixture(room),
                first.last().map(String::as_str),
            )
            .map(|(canonical, _, _)| canonical.to_owned())
            .collect();
        assert_eq!(rest, expected[2..]);
        assert_eq!(
            registry
                .ordered_participants_in_room(&ProjectRoomKey::fixture(other), None)
                .count(),
            0
        );
        let moving = registry
            .ordered_participants_in_room(&ProjectRoomKey::fixture(room), None)
            .next()
            .unwrap()
            .1;
        registry.remove_participant(&moving).unwrap();
        registry.add_participant(moving, other, ShardId::new(1), None);
        assert_eq!(
            registry
                .ordered_participants_in_room(&ProjectRoomKey::fixture(room), None)
                .count(),
            3
        );
        assert_eq!(
            registry.local_participant_ids_in_room(&ProjectRoomKey::fixture(other)),
            vec![moving]
        );
        let connection = registry.get_participant(&moving).unwrap().connection_id;
        assert!(registry.remove_incarnation(&moving, connection).is_some());
        assert_eq!(
            registry
                .ordered_participants_in_room(&ProjectRoomKey::fixture(other), None)
                .count(),
            0
        );
        assert!(
            !registry
                .participants_by_room
                .contains_key(&ProjectRoomKey::fixture(other))
        );
    }

    #[test]
    fn add_participant_creates_room_and_entry() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-a");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(0), None);

        reg.get_room_shard(&ProjectRoomKey::fixture(rid)).unwrap();
        let meta = reg.get_participant(&pid).expect("participant should exist");
        assert_eq!(meta.room_id(), rid);
        assert_eq!(meta.shard_id, ShardId::new(0));
    }

    #[test]
    fn add_participant_increments_room_count() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-b");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, ShardId::new(0), None);
        reg.add_participant(pid2, rid, ShardId::new(1), None);

        let room = reg.get_room_shard(&ProjectRoomKey::fixture(rid)).unwrap();
        assert_eq!(room.participant_count(), 2);
    }

    #[test]
    fn removing_and_adding_participant_updates_both_rooms() {
        let mut reg = RoomRegistry::new();
        let old_room = room_id("room-b-old");
        let new_room = room_id("room-b-new");
        let pid = participant_id();

        reg.add_participant(pid, old_room, ShardId::new(0), None);
        reg.remove_participant(&pid).unwrap();
        reg.add_participant(pid, new_room, ShardId::new(1), None);

        assert!(
            reg.get_room_shard(&ProjectRoomKey::fixture(old_room))
                .is_none()
        );
        assert_eq!(
            reg.get_room_shard(&ProjectRoomKey::fixture(new_room))
                .unwrap()
                .participant_count(),
            1
        );
        let meta = reg.get_participant(&pid).unwrap();
        assert_eq!(meta.room_id(), new_room);
        assert_eq!(meta.shard_id, ShardId::new(1));
    }

    #[test]
    fn remove_participant_returns_shard_id() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-c");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(3), None);
        let shard = reg.remove_participant(&pid);

        assert_eq!(shard, Some(ShardId::new(3)));
    }

    #[test]
    fn remove_unknown_participant_returns_none() {
        let mut reg = RoomRegistry::new();
        let pid = participant_id();
        assert!(reg.remove_participant(&pid).is_none());
    }

    #[test]
    fn room_is_immediately_deleted_after_last_participant_leaves() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-d");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(0), None);
        reg.remove_participant(&pid);

        assert!(reg.get_room_shard(&ProjectRoomKey::fixture(rid)).is_none());
    }

    #[test]
    fn participant_can_rejoin_a_deleted_room() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-f");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, ShardId::new(0), None);
        reg.remove_participant(&pid1);

        reg.add_participant(pid2, rid, ShardId::new(1), None);

        reg.get_room_shard(&ProjectRoomKey::fixture(rid)).unwrap();
        assert_eq!(
            reg.get_room_shard(&ProjectRoomKey::fixture(rid))
                .unwrap()
                .participant_count(),
            1
        );
    }

    #[test]
    fn participant_removed_from_registry_after_remove() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-h");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(0), None);
        reg.remove_participant(&pid);

        assert!(reg.get_participant(&pid).is_none());
    }

    #[test]
    fn multiple_rooms_are_independent() {
        let mut reg = RoomRegistry::new();
        let rid1 = room_id("room-x");
        let rid2 = room_id("room-y");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid1, ShardId::new(0), None);
        reg.add_participant(pid2, rid2, ShardId::new(1), None);
        reg.remove_participant(&pid1);
        assert!(reg.get_room_shard(&ProjectRoomKey::fixture(rid1)).is_none());
        assert!(reg.get_room_shard(&ProjectRoomKey::fixture(rid2)).is_some());
    }

    #[test]
    fn prepared_candidate_does_not_replace_current_until_commit() {
        let mut reg = RoomRegistry::new();
        let room = room_id("prepare");
        let participant = participant_id();
        let current = connection_id(1);
        reg.commit_candidate(
            participant,
            ProjectRoomKey::fixture(room),
            ShardId::new(0),
            0,
            transport(0, 1),
            current,
            SignalingProtocol::State,
        )
        .unwrap();

        let _prepared = (connection_id(2), transport(0, 2));

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
        assert_eq!(
            reg.get_connection(current).map(|(owner, _)| owner),
            Some(participant)
        );
        assert!(reg.get_connection(connection_id(2)).is_none());
    }

    #[test]
    fn failed_candidate_leaves_current_installed() {
        let mut reg = RoomRegistry::new();
        let room = room_id("failed-prepare");
        let participant = participant_id();
        let current = connection_id(2);
        reg.commit_candidate(
            participant,
            ProjectRoomKey::fixture(room),
            ShardId::new(0),
            0,
            transport(0, 1),
            current,
            SignalingProtocol::State,
        )
        .unwrap();

        let failed = connection_id(1);
        assert_eq!(
            reg.commit_candidate(
                participant,
                ProjectRoomKey::fixture(room),
                ShardId::new(0),
                0,
                transport(0, 2),
                failed,
                SignalingProtocol::State,
            ),
            Err(CommitCandidateError::Superseded)
        );

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
        assert_eq!(
            reg.get_connection(current).map(|(owner, _)| owner),
            Some(participant)
        );
        assert!(reg.get_connection(failed).is_none());
    }

    #[test]
    fn candidates_committing_out_of_order_keep_largest_connection_id() {
        let mut reg = RoomRegistry::new();
        let room = room_id("ordering");
        let participant = participant_id();
        let older = connection_id(1);
        let newer = connection_id(2);

        reg.commit_candidate(
            participant,
            ProjectRoomKey::fixture(room),
            ShardId::new(0),
            0,
            transport(0, 2),
            newer,
            SignalingProtocol::State,
        )
        .unwrap();
        assert_eq!(
            reg.commit_candidate(
                participant,
                ProjectRoomKey::fixture(room),
                ShardId::new(0),
                0,
                transport(0, 1),
                older,
                SignalingProtocol::State,
            ),
            Err(CommitCandidateError::Superseded)
        );

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            newer
        );
        assert_eq!(
            reg.get_connection(newer).map(|(owner, _)| owner),
            Some(participant)
        );
        assert!(reg.get_connection(older).is_none());
    }

    #[test]
    fn stale_lifecycle_work_cannot_remove_replacement() {
        let mut reg = RoomRegistry::new();
        let room = room_id("fencing");
        let participant = participant_id();
        let old = connection_id(1);
        let current = connection_id(2);
        reg.commit_candidate(
            participant,
            ProjectRoomKey::fixture(room),
            ShardId::new(0),
            0,
            transport(0, 1),
            old,
            SignalingProtocol::State,
        )
        .unwrap();
        reg.commit_candidate(
            participant,
            ProjectRoomKey::fixture(room),
            ShardId::new(0),
            0,
            transport(0, 2),
            current,
            SignalingProtocol::State,
        )
        .unwrap();

        // DELETE, close, transport failure, and expiry all converge on this
        // exact-incarnation operation in the controller.
        for _ in 0..4 {
            assert!(reg.remove_incarnation(&participant, old).is_none());
        }
        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
        assert!(reg.get_connection(old).is_none());
        assert_eq!(
            reg.get_connection(current).map(|(owner, _)| owner),
            Some(participant)
        );
        assert!(reg.remove_incarnation(&participant, current).is_some());
        assert!(reg.get_connection(current).is_none());
    }

    #[test]
    fn local_registry_restart_makes_no_durability_promise() {
        let mut before_restart = RoomRegistry::new();
        let room = room_id("restart");
        let participant = participant_id();
        before_restart
            .commit_candidate(
                participant,
                ProjectRoomKey::fixture(room),
                ShardId::new(0),
                0,
                transport(0, 1),
                connection_id(1),
                SignalingProtocol::State,
            )
            .unwrap();

        let after_restart = RoomRegistry::new();
        assert!(after_restart.get_participant(&participant).is_none());
    }
}
