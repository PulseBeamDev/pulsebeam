use ahash::{HashMap, HashMapExt};
use indexmap::IndexSet;

use super::router::RoutingContext;
use super::worker::CrossShardEvent;
use crate::entity::{ParticipantId, RoomId};
use crate::id::ShardId;
use crate::track::Topic;

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StreamId {
    publisher: ParticipantId,
    topic: Topic,
}

struct Route {
    published: bool,
    local_subscribers: FastIndexSet<ParticipantId>,
    remote_subscriber_shards: HashMap<ShardId, usize>,
}

impl Route {
    fn new() -> Self {
        Self {
            published: false,
            local_subscribers: IndexSet::with_capacity_and_hasher(
                256,
                ahash::RandomState::default(),
            ),
            remote_subscriber_shards: HashMap::new(),
        }
    }

    fn is_unused(&self) -> bool {
        !self.published
            && self.local_subscribers.is_empty()
            && self.remote_subscriber_shards.is_empty()
    }
}

pub(super) struct ReliableRoutes {
    streams: HashMap<StreamId, Route>,
}

impl ReliableRoutes {
    pub(super) fn new() -> Self {
        Self {
            streams: HashMap::new(),
        }
    }

    pub(super) fn remove_participant(&mut self, participant: &ParticipantId) {
        for route in self.streams.values_mut() {
            route.local_subscribers.swap_remove(participant);
        }
        self.streams.retain(|_, route| !route.is_unused());
    }

    pub(super) fn publish(&mut self, publisher: ParticipantId, topic: Topic) {
        let route = self
            .streams
            .entry(StreamId { publisher, topic })
            .or_insert_with(Route::new);
        debug_assert!(!route.published);
        route.published = true;
    }

    pub(super) fn unpublish(&mut self, publisher: ParticipantId, topic: &Topic) {
        let key = StreamId {
            publisher,
            topic: topic.clone(),
        };
        let Some(route) = self.streams.get_mut(&key) else {
            debug_assert!(false, "unpublishing an unknown reliable stream");
            return;
        };
        debug_assert!(route.published);
        route.published = false;
        if route.is_unused() {
            self.streams.remove(&key);
        }
    }

    pub(super) fn subscribe_local(
        &mut self,
        subscriber: ParticipantId,
        publisher: ParticipantId,
        topic: Topic,
    ) -> bool {
        let route = self
            .streams
            .entry(StreamId { publisher, topic })
            .or_insert_with(Route::new);
        let was_empty = route.local_subscribers.is_empty();
        let inserted = route.local_subscribers.insert(subscriber);
        debug_assert!(inserted);
        was_empty
    }

    pub(super) fn unsubscribe_local(
        &mut self,
        subscriber: ParticipantId,
        publisher: ParticipantId,
        topic: &Topic,
    ) -> bool {
        let key = StreamId {
            publisher,
            topic: topic.clone(),
        };
        let Some(route) = self.streams.get_mut(&key) else {
            debug_assert!(false, "unsubscribing from an unknown reliable stream");
            return false;
        };
        let was_last = route.local_subscribers.len() == 1;
        let removed = route.local_subscribers.swap_remove(&subscriber);
        debug_assert!(removed);
        if route.is_unused() {
            self.streams.remove(&key);
        }
        was_last
    }

    pub(super) fn subscribe_remote(
        &mut self,
        shard: ShardId,
        publisher: ParticipantId,
        topic: Topic,
    ) {
        let route = self
            .streams
            .entry(StreamId { publisher, topic })
            .or_insert_with(Route::new);
        let count = route.remote_subscriber_shards.entry(shard).or_insert(0);
        *count += 1;
        debug_assert!(*count <= 2);
    }

    pub(super) fn unsubscribe_remote(
        &mut self,
        shard: ShardId,
        publisher: ParticipantId,
        topic: &Topic,
    ) {
        let key = StreamId {
            publisher,
            topic: topic.clone(),
        };
        let Some(route) = self.streams.get_mut(&key) else {
            debug_assert!(false, "unsubscribing an unknown remote reliable stream");
            return;
        };
        let Some(count) = route.remote_subscriber_shards.get_mut(&shard) else {
            debug_assert!(false, "unsubscribing an unknown remote reliable shard");
            return;
        };
        debug_assert!(*count > 0);
        *count -= 1;
        if *count == 0 {
            route.remote_subscriber_shards.remove(&shard);
        }
        if route.is_unused() {
            self.streams.remove(&key);
        }
    }

    pub(super) fn route(
        &self,
        room_id: RoomId,
        origin: ParticipantId,
        topic: &Topic,
        frame: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(!frame.is_empty());
        let key = StreamId {
            publisher: origin,
            topic: topic.clone(),
        };
        let Some(route) = self.streams.get(&key) else {
            return;
        };
        debug_assert!(route.published);
        for &subscriber in &route.local_subscribers {
            ctx.forward_reliable_sctp(subscriber, origin, topic, frame);
        }
        if ctx.is_local(&origin) {
            for &shard in route.remote_subscriber_shards.keys() {
                ctx.send(
                    shard,
                    CrossShardEvent::ReliableDataSctpPublished {
                        room_id,
                        origin,
                        topic: topic.clone(),
                        frame: frame.to_vec(),
                    },
                );
            }
        }
    }
}
