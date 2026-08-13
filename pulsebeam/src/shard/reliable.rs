use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;

use super::router::RoutingContext;
use crate::id::ShardId;
use crate::route::RemoteRoute;
use crate::track::Topic;
use crate::{entity::ParticipantId, shard::participants::ParticipantKey};

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct StreamId {
    pub publisher: ParticipantId,
    pub topic: Topic,
}

pub(super) struct ReliableRoutes {
    published: HashSet<StreamId>,
    local_subscribers: HashMap<Topic, FastIndexSet<ParticipantKey>>,
    /// Acknowledged destination handles per stream. A reliable subscription
    /// names only a topic, so these are filled in as publishers are announced.
    remote_routes: HashMap<StreamId, HashMap<ShardId, RemoteRoute>>,
    /// Streams this shard installed a *destination* route for. Distinct from
    /// `published`, which is what this shard sends.
    imported: HashSet<StreamId>,
}

impl ReliableRoutes {
    pub(super) fn new() -> Self {
        Self {
            published: HashSet::new(),
            local_subscribers: HashMap::new(),
            remote_routes: HashMap::new(),
            imported: HashSet::new(),
        }
    }

    pub(super) fn remove_participant(
        &mut self,
        participant: ParticipantKey,
        participant_id: ParticipantId,
    ) {
        for subscribers in self.local_subscribers.values_mut() {
            subscribers.swap_remove(&participant);
        }
        self.local_subscribers
            .retain(|_, subscribers| !subscribers.is_empty());
        self.published
            .retain(|stream| stream.publisher != participant_id);
        self.remote_routes
            .retain(|stream, _| stream.publisher != participant_id);
    }

    pub(super) fn publish(&mut self, publisher: ParticipantId, topic: Topic) {
        let inserted = self.published.insert(StreamId { publisher, topic });
        debug_assert!(inserted);
    }

    pub(super) fn mark_imported(&mut self, publisher: ParticipantId, topic: Topic) {
        self.imported.insert(StreamId { publisher, topic });
    }

    /// Remote publishers this shard holds a destination route for on `topic`.
    pub(super) fn imported_on(&self, topic: &Topic) -> Vec<ParticipantId> {
        self.imported
            .iter()
            .filter(|s| &s.topic == topic)
            .map(|s| s.publisher)
            .collect()
    }

    pub(super) fn clear_imported(&mut self, publisher: ParticipantId, topic: &Topic) {
        self.imported.remove(&StreamId {
            publisher,
            topic: topic.clone(),
        });
    }

    pub(super) fn has_local_subscribers(&self, topic: &Topic) -> bool {
        self.local_subscribers
            .get(topic)
            .is_some_and(|s| !s.is_empty())
    }

    /// Publishers this shard already serves on `topic`, so a destination that
    /// subscribes after they appeared still gets routes for them.
    pub(super) fn published_on(&self, topic: &Topic) -> Vec<ParticipantId> {
        self.published
            .iter()
            .filter(|s| &s.topic == topic)
            .map(|s| s.publisher)
            .collect()
    }

    pub(super) fn attach_remote(
        &mut self,
        publisher: ParticipantId,
        topic: Topic,
        remote: RemoteRoute,
    ) {
        self.remote_routes
            .entry(StreamId { publisher, topic })
            .or_default()
            .insert(remote.shard_id, remote);
    }

    pub(super) fn detach_remote(
        &mut self,
        publisher: ParticipantId,
        topic: &Topic,
        shard_id: ShardId,
    ) {
        let key = StreamId {
            publisher,
            topic: topic.clone(),
        };
        if let Some(dests) = self.remote_routes.get_mut(&key) {
            dests.remove(&shard_id);
            if dests.is_empty() {
                self.remote_routes.remove(&key);
            }
        }
    }

    pub(super) fn remote_routes_mut(
        &mut self,
        publisher: ParticipantId,
        topic: &Topic,
    ) -> Option<impl Iterator<Item = &mut RemoteRoute>> {
        self.remote_routes
            .get_mut(&StreamId {
                publisher,
                topic: topic.clone(),
            })
            .map(|dests| dests.values_mut())
    }

    pub(super) fn unpublish(&mut self, publisher: ParticipantId, topic: &Topic) {
        let removed = self.published.remove(&StreamId {
            publisher,
            topic: topic.clone(),
        });
        debug_assert!(removed);
    }

    pub(super) fn subscribe_local(&mut self, subscriber: ParticipantKey, topic: Topic) -> bool {
        let subscribers = self.local_subscribers.entry(topic).or_insert_with(|| {
            IndexSet::with_capacity_and_hasher(256, ahash::RandomState::default())
        });
        let was_empty = subscribers.is_empty();
        let inserted = subscribers.insert(subscriber);
        debug_assert!(inserted);
        was_empty
    }

    pub(super) fn unsubscribe_local(&mut self, subscriber: ParticipantKey, topic: &Topic) -> bool {
        let Some(subscribers) = self.local_subscribers.get_mut(topic) else {
            debug_assert!(false, "unsubscribing from an unknown reliable topic");
            return false;
        };
        let was_last = subscribers.len() == 1;
        let removed = subscribers.swap_remove(&subscriber);
        debug_assert!(removed);
        if subscribers.is_empty() {
            self.local_subscribers.remove(topic);
        }
        was_last
    }

    pub(super) fn route(
        &self,
        origin: ParticipantId,
        topic: &Topic,
        frame: &[u8],
        local_origin: bool,
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(!frame.is_empty());
        let stream = StreamId {
            publisher: origin,
            topic: topic.clone(),
        };
        if local_origin && !self.published.contains(&stream) {
            return;
        }
        debug_assert!(!local_origin || self.published.contains(&stream));
        if let Some(subscribers) = self.local_subscribers.get(topic) {
            for &subscriber in subscribers {
                ctx.forward_reliable_sctp(subscriber, origin, topic, frame);
            }
        }
    }
}
