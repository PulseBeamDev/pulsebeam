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

/// The channel a subscriber is served on.
///
/// Generic because this module routes the handle without ever reading it, and
/// because `str0m::channel::ChannelId` is opaque — no constructor, no accessor —
/// so a concrete field here would make every invariant below untestable.
pub(crate) type DefaultChannel = str0m::channel::ChannelId;

/// One subscriber of a specific publisher's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Subscriber<C = DefaultChannel> {
    pub key: ParticipantKey,
    pub channel: C,
}

/// A subscriber to every publisher on a topic. Carries its own shard because,
/// unlike [`Subscriber`], it is not already filed under one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WildcardSubscriber<C = DefaultChannel> {
    pub shard: ShardId,
    pub key: ParticipantKey,
    pub channel: C,
}

impl<C> WildcardSubscriber<C> {
    fn split(self) -> (ShardId, Subscriber<C>) {
        (
            self.shard,
            Subscriber {
                key: self.key,
                channel: self.channel,
            },
        )
    }
}

#[derive(Debug)]
pub(crate) struct StreamBinding<C = DefaultChannel> {
    pub publisher_shard: ShardId,
    pub publisher: ParticipantKey,
    pub key: Option<RuntimeStreamKey>,
    pub reverse_route: Option<RouteHandle>,
    pub subscribers: HashMap<ShardId, HashMap<ParticipantId, Subscriber<C>>>,
    pub destination_keys: HashMap<ShardId, RuntimeStreamKey>,
    pub routes: HashMap<ShardId, RouteHandle>,
}

impl<C> StreamBinding<C> {
    fn new(publisher_shard: ShardId, publisher: ParticipantKey) -> Self {
        Self {
            publisher_shard,
            publisher,
            key: None,
            reverse_route: None,
            subscribers: HashMap::new(),
            destination_keys: HashMap::new(),
            routes: HashMap::new(),
        }
    }

    pub(crate) fn add_subscriber(
        &mut self,
        shard: ShardId,
        participant: ParticipantId,
        subscriber: Subscriber<C>,
    ) {
        self.subscribers
            .entry(shard)
            .or_default()
            .insert(participant, subscriber);
    }

    pub(crate) fn remove_subscriber(&mut self, participant: &ParticipantId) -> bool {
        let mut removed = false;
        self.subscribers.retain(|_, subscribers| {
            removed |= subscribers.remove(participant).is_some();
            !subscribers.is_empty()
        });
        removed
    }
}

/// Subscribers that named a publisher whose stream is not yet ready, keyed by
/// stream, then by the subscriber's shard.
type PendingSubscribers<C> = HashMap<ShardId, HashMap<ParticipantId, Subscriber<C>>>;

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

pub(crate) struct LaneRegistry<C = DefaultChannel> {
    lane: StreamLane,
    bindings: HashMap<DataStreamId, StreamBinding<C>>,
    pending: HashMap<DataStreamId, PendingSubscribers<C>>,
    wildcards: HashMap<(RoomId, Topic), HashMap<ParticipantId, WildcardSubscriber<C>>>,
    by_topic: TopicIndex,
}

impl<C: Copy> LaneRegistry<C> {
    pub(crate) fn new(lane: StreamLane) -> Self {
        Self {
            lane,
            bindings: HashMap::new(),
            pending: HashMap::new(),
            wildcards: HashMap::new(),
            by_topic: TopicIndex::default(),
        }
    }

    pub(crate) fn get(&self, id: &DataStreamId) -> Option<&StreamBinding<C>> {
        self.bindings.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &DataStreamId) -> Option<&mut StreamBinding<C>> {
        self.bindings.get_mut(id)
    }

    pub(crate) fn remove(&mut self, id: &DataStreamId) -> Option<StreamBinding<C>> {
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
    ) -> &mut StreamBinding<C> {
        debug_assert_eq!(
            self.lane,
            StreamLane::of(key),
            "runtime key does not belong to this lane"
        );
        let pending = self.pending.remove(&id);
        self.by_topic.insert(&id);
        let binding = self
            .bindings
            .entry(id)
            .or_insert_with(|| StreamBinding::new(publisher_shard, publisher));
        debug_assert_eq!(binding.publisher_shard, publisher_shard);
        binding.key = Some(key);
        for (shard, subscribers) in pending.into_iter().flatten() {
            for (participant, subscriber) in subscribers {
                binding.add_subscriber(shard, participant, subscriber);
            }
        }
        binding
    }

    /// Attach every wildcard subscriber of `id`'s topic. Called when a stream
    /// becomes ready, so a topic-wide subscription made earlier still reaches it.
    pub(crate) fn apply_wildcards(&mut self, id: &DataStreamId) {
        let Some(wildcard) = self.wildcards.get(&(id.room_id, id.topic.clone())) else {
            return;
        };
        let subscribers: Vec<_> = wildcard
            .iter()
            .map(|(participant, entry)| (*participant, entry.split()))
            .collect();
        let Some(binding) = self.bindings.get_mut(id) else {
            debug_assert!(false, "stream binding must exist to take its wildcards");
            return;
        };
        for (participant, (shard, subscriber)) in subscribers {
            binding.add_subscriber(shard, participant, subscriber);
        }
    }

    /// Subscribe to one named publisher. Returns the stream when it was live,
    /// so the caller knows whether to reconcile or wait; parks the subscriber
    /// otherwise.
    pub(crate) fn subscribe(
        &mut self,
        id: DataStreamId,
        shard: ShardId,
        participant: ParticipantId,
        subscriber: Subscriber<C>,
    ) -> Option<DataStreamId> {
        if let Some(binding) = self.bindings.get_mut(&id) {
            binding.add_subscriber(shard, participant, subscriber);
            return Some(id);
        }
        self.pending
            .entry(id)
            .or_default()
            .entry(shard)
            .or_default()
            .insert(participant, subscriber);
        None
    }

    /// Subscribe to every publisher on a topic, present and future. Returns the
    /// streams that already exist.
    pub(crate) fn subscribe_wildcard(
        &mut self,
        room: RoomId,
        topic: Topic,
        participant: ParticipantId,
        subscriber: WildcardSubscriber<C>,
    ) -> Vec<DataStreamId> {
        self.wildcards
            .entry((room, topic.clone()))
            .or_default()
            .insert(participant, subscriber);
        let ids = self.ids_on_topic(&room, &topic);
        let (shard, entry) = subscriber.split();
        for id in &ids {
            if let Some(binding) = self.bindings.get_mut(id) {
                binding.add_subscriber(shard, participant, entry);
            }
        }
        ids
    }

    pub(crate) fn unsubscribe_wildcard(
        &mut self,
        room: RoomId,
        topic: Topic,
        participant: &ParticipantId,
    ) {
        if let Some(subscribers) = self.wildcards.get_mut(&(room, topic)) {
            subscribers.remove(participant);
        }
    }

    /// Drop `participant` from `id`, live or parked. Returns whether the live
    /// binding changed, which is what decides if a reconcile is owed.
    pub(crate) fn unsubscribe(
        &mut self,
        id: &DataStreamId,
        participant: &ParticipantId,
        drop_pending: bool,
    ) -> bool {
        let changed = self
            .bindings
            .get_mut(id)
            .is_some_and(|binding| binding.remove_subscriber(participant));
        if drop_pending && let Some(pending) = self.pending.get_mut(id) {
            Self::purge(pending, participant);
        }
        changed
    }

    /// Forget `participant` everywhere. Returns the streams whose live
    /// membership changed.
    pub(crate) fn retire_participant(&mut self, participant: &ParticipantId) -> Vec<DataStreamId> {
        let changed = self
            .bindings
            .iter_mut()
            .filter_map(|(id, binding)| binding.remove_subscriber(participant).then(|| id.clone()))
            .collect();
        self.pending.retain(|_, pending| {
            Self::purge(pending, participant);
            !pending.is_empty()
        });
        self.wildcards.retain(|_, subscribers| {
            subscribers.remove(participant);
            !subscribers.is_empty()
        });
        changed
    }

    pub(crate) fn forget_pending(&mut self, id: &DataStreamId) {
        self.pending.remove(id);
    }

    fn purge(pending: &mut PendingSubscribers<C>, participant: &ParticipantId) {
        pending.retain(|_, subscribers| {
            subscribers.remove(participant);
            !subscribers.is_empty()
        });
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
