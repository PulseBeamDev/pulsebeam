//! Per-lane data stream routing state.
//!
//! The two lanes are the same data channel. Reliable delivery is unreliable
//! delivery plus a retransmit cache and the feedback to drive it, layered
//! end-to-end over a hop-to-hop transport that guarantees neither — so the
//! routing state machine is identical and only the runtime key differs.
//! Keeping them as one type instantiated twice is what stops the two copies
//! drifting: every lane-dependent decision — which key to mint, which route
//! action to emit, which arena to retire into — is a method here, and nowhere
//! else.

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

/// How a data stream is delivered.
///
/// Named for the guarantee rather than the medium: both lanes carry the same
/// data channel, and `Unreliable` is the base the other adds to. The lane is
/// part of a stream's identity, not a flag on it — `Topic::publisher()`
/// resolves to `.ordered()` or `.latest()`, and one topic name can carry both
/// at once without either claiming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StreamLane {
    Unreliable,
    Reliable,
}

impl StreamLane {
    /// The lane a runtime key belongs to. The key's variant is the only thing
    /// that decides it, so this is the one place that mapping is written.
    pub(crate) fn of(key: RuntimeStreamKey) -> Self {
        match key {
            RuntimeStreamKey::Unreliable(_) => StreamLane::Unreliable,
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
            (StreamLane::Unreliable, RuntimeStreamKey::Unreliable(stream)) => {
                Some(RouteAction::Unreliable { stream })
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
            StreamLane::Unreliable => state
                .mint_data(destination, id.clone())
                .map(RuntimeStreamKey::Unreliable),
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
            (StreamLane::Unreliable, RuntimeStreamKey::Unreliable(key)) => {
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
            data: LaneRegistry::new(StreamLane::Unreliable),
            reliable: LaneRegistry::new(StreamLane::Reliable),
        }
    }

    pub(crate) fn get(&self, lane: StreamLane) -> &LaneRegistry {
        match lane {
            StreamLane::Unreliable => &self.data,
            StreamLane::Reliable => &self.reliable,
        }
    }

    pub(crate) fn get_mut(&mut self, lane: StreamLane) -> &mut LaneRegistry {
        match lane {
            StreamLane::Unreliable => &mut self.data,
            StreamLane::Reliable => &mut self.reliable,
        }
    }
}

#[cfg(test)]
mod tests;
