//! Per-lane data stream routing state.
//!
//! `Data` and `Reliable` are the same state machine over different runtime
//! keys. Keeping them as one type instantiated twice is what stops the two
//! copies drifting: every lane-dependent decision — which key to mint, which
//! route action to emit, which arena to retire into — is a method here, and
//! nowhere else.

use ahash::{HashMap, HashMapExt};

use crate::{
    control::state::ControlPlaneState,
    entity::{ParticipantId, RoomId},
    id::ShardId,
    route::{RouteAction, RouteHandle},
    shard::{
        participants::ParticipantKey,
        router::{DataStreamId, RuntimeStreamKey},
    },
    track::Topic,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StreamLane {
    Data,
    Reliable,
}

impl StreamLane {
    pub(crate) const ALL: [StreamLane; 2] = [StreamLane::Data, StreamLane::Reliable];

    /// The lane a runtime key belongs to. The key's variant is the only thing
    /// that decides it, so this is the one place that mapping is written.
    pub(crate) fn of(key: RuntimeStreamKey) -> Self {
        match key {
            RuntimeStreamKey::Data(_) => StreamLane::Data,
            RuntimeStreamKey::Reliable(_) => StreamLane::Reliable,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StreamBinding {
    pub publisher_shard: ShardId,
    pub publisher: ParticipantKey,
    pub key: Option<RuntimeStreamKey>,
    pub reverse_route: Option<RouteHandle>,
    pub destination_keys: HashMap<ShardId, RuntimeStreamKey>,
    pub routes: HashMap<ShardId, RouteHandle>,
}

impl StreamBinding {
    fn new(publisher_shard: ShardId, publisher: ParticipantKey) -> Self {
        Self {
            publisher_shard,
            publisher,
            key: None,
            reverse_route: None,
            destination_keys: HashMap::new(),
            routes: HashMap::new(),
        }
    }
}

/// Which publishers are live on each topic.
///
/// A wildcard subscription has to name every stream on its topic, and without
/// this that means walking every stream on the node. Kept beside the bindings
/// rather than derived from them, so it is one lookup instead of one scan.
#[derive(Default)]
struct TopicIndex {
    publishers: HashMap<(RoomId, Topic), Vec<ParticipantId>>,
}

impl TopicIndex {
    fn insert(&mut self, id: &DataStreamId) {
        let publishers = self
            .publishers
            .entry((id.room_id, id.topic.clone()))
            .or_default();
        if !publishers.contains(&id.publisher_id) {
            publishers.push(id.publisher_id);
        }
    }

    fn remove(&mut self, id: &DataStreamId) {
        let key = (id.room_id, id.topic.clone());
        let Some(publishers) = self.publishers.get_mut(&key) else {
            return;
        };
        publishers.retain(|publisher| *publisher != id.publisher_id);
        if publishers.is_empty() {
            self.publishers.remove(&key);
        }
    }

    fn streams(&self, room: &RoomId, topic: &Topic) -> Vec<DataStreamId> {
        self.publishers
            .get(&(*room, topic.clone()))
            .into_iter()
            .flatten()
            .map(|publisher| DataStreamId::new(*room, *publisher, topic.clone()))
            .collect()
    }
}

pub(crate) struct LaneRegistry {
    lane: StreamLane,
    bindings: HashMap<DataStreamId, StreamBinding>,
    by_topic: TopicIndex,
}

impl LaneRegistry {
    pub(crate) fn new(lane: StreamLane) -> Self {
        Self {
            lane,
            bindings: HashMap::new(),
            by_topic: TopicIndex::default(),
        }
    }

    pub(crate) fn get(&self, id: &DataStreamId) -> Option<&StreamBinding> {
        self.bindings.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &DataStreamId) -> Option<&mut StreamBinding> {
        self.bindings.get_mut(id)
    }

    pub(crate) fn remove(&mut self, id: &DataStreamId) -> Option<StreamBinding> {
        self.by_topic.remove(id);
        self.bindings.remove(id)
    }

    /// Every live stream carrying `topic` in `room`. The set a wildcard
    /// subscription resolves to at the moment it is made.
    pub(crate) fn ids_on_topic(&self, room: &RoomId, topic: &Topic) -> Vec<DataStreamId> {
        self.by_topic.streams(room, topic)
    }

    /// Publish `id`, draining anyone who subscribed before it existed.
    pub(crate) fn declare(
        &mut self,
        id: DataStreamId,
        publisher_shard: ShardId,
        publisher: ParticipantKey,
        key: RuntimeStreamKey,
    ) -> &mut StreamBinding {
        debug_assert_eq!(
            self.lane,
            StreamLane::of(key),
            "runtime key does not belong to this lane"
        );
        self.by_topic.insert(&id);
        let binding = self
            .bindings
            .entry(id)
            .or_insert_with(|| StreamBinding::new(publisher_shard, publisher));
        debug_assert_eq!(binding.publisher_shard, publisher_shard);
        binding.key = Some(key);
        binding
    }

    /// The route action that carries this lane's traffic. `None` when the key
    /// belongs to the other lane, which is a caller bug rather than a state.
    pub(crate) fn route_action(&self, key: RuntimeStreamKey) -> Option<RouteAction> {
        match (self.lane, key) {
            (StreamLane::Data, RuntimeStreamKey::Data(stream)) => {
                Some(RouteAction::Data { stream })
            }
            (StreamLane::Reliable, RuntimeStreamKey::Reliable(stream)) => {
                Some(RouteAction::Reliable { stream })
            }
            _ => None,
        }
    }

    pub(crate) fn mint(
        &self,
        state: &mut ControlPlaneState,
        destination: ShardId,
        id: &DataStreamId,
    ) -> Option<RuntimeStreamKey> {
        match self.lane {
            StreamLane::Data => state
                .mint_data(destination, id.clone())
                .map(RuntimeStreamKey::Data),
            StreamLane::Reliable => state
                .mint_reliable(destination, id.clone())
                .map(RuntimeStreamKey::Reliable),
        }
    }

    pub(crate) fn retire_runtime(
        &self,
        state: &mut ControlPlaneState,
        destination: ShardId,
        key: RuntimeStreamKey,
    ) {
        match (self.lane, key) {
            (StreamLane::Data, RuntimeStreamKey::Data(key)) => {
                state.remove_data(destination, key);
            }
            (StreamLane::Reliable, RuntimeStreamKey::Reliable(key)) => {
                state.remove_reliable(destination, key);
            }
            _ => debug_assert!(false, "stream key and lane disagree"),
        }
    }
}

/// Both lanes, so a caller that must touch each does not name them separately.
pub(crate) struct Lanes {
    data: LaneRegistry,
    reliable: LaneRegistry,
}

impl Lanes {
    pub(crate) fn new() -> Self {
        Self {
            data: LaneRegistry::new(StreamLane::Data),
            reliable: LaneRegistry::new(StreamLane::Reliable),
        }
    }

    pub(crate) fn get(&self, lane: StreamLane) -> &LaneRegistry {
        match lane {
            StreamLane::Data => &self.data,
            StreamLane::Reliable => &self.reliable,
        }
    }

    pub(crate) fn get_mut(&mut self, lane: StreamLane) -> &mut LaneRegistry {
        match lane {
            StreamLane::Data => &mut self.data,
            StreamLane::Reliable => &mut self.reliable,
        }
    }
}

#[cfg(test)]
mod tests;
