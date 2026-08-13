use ahash::HashMap;
use indexmap::IndexSet;

use crate::shard::participants::ParticipantKey;
use crate::track::Topic;

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

/// Local topic subscriptions for the reliable lane. Everything else about a
/// reliable stream — publish/import flags, remote destination handles — lives
/// in `DataPlane::reliable_streams`, keyed by `ReliableStreamKey`; a
/// subscription names a topic, not a stream, so it has no key to live under
/// there.
pub(super) struct ReliableRoutes {
    local_subscribers: HashMap<Topic, FastIndexSet<ParticipantKey>>,
}

impl ReliableRoutes {
    pub(super) fn new() -> Self {
        Self {
            local_subscribers: HashMap::default(),
        }
    }

    pub(super) fn remove_participant(&mut self, participant: ParticipantKey) {
        for subscribers in self.local_subscribers.values_mut() {
            subscribers.swap_remove(&participant);
        }
        self.local_subscribers
            .retain(|_, subscribers| !subscribers.is_empty());
    }

    pub(super) fn has_local_subscribers(&self, topic: &Topic) -> bool {
        self.local_subscribers
            .get(topic)
            .is_some_and(|s| !s.is_empty())
    }

    pub(super) fn local_subscribers(&self, topic: &Topic) -> Option<&FastIndexSet<ParticipantKey>> {
        self.local_subscribers.get(topic)
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
}
