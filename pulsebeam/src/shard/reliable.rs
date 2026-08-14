use ahash::HashMap;

use crate::shard::participants::ParticipantKey;
use crate::shard::keyset::KeySet;
use crate::track::Topic;

/// Local topic subscriptions for the reliable lane. Everything else about a
/// reliable stream — publish/import flags, remote destination handles — lives
/// in `DataPlane::reliable_streams`, keyed by `ReliableStreamKey`; a
/// subscription names a topic, not a stream, so it has no key to live under
/// there.
///
/// `ParticipantKey` is already a dense slotmap key, so a subscriber set
/// indexes by it directly ([`KeySet`]) rather than scanning — these sets run
/// to hundreds, and both the forwarding and teardown paths touch them.
pub(super) struct ReliableRoutes {
    local_subscribers: HashMap<Topic, KeySet<ParticipantKey>>,
}

impl ReliableRoutes {
    pub(super) fn new() -> Self {
        Self {
            local_subscribers: HashMap::default(),
        }
    }

    pub(super) fn remove_participant(&mut self, participant: ParticipantKey) {
        for subscribers in self.local_subscribers.values_mut() {
            subscribers.remove_value(&participant);
        }
        self.local_subscribers
            .retain(|_, subscribers| !subscribers.is_empty());
    }

    pub(super) fn has_local_subscribers(&self, topic: &Topic) -> bool {
        self.local_subscribers
            .get(topic)
            .is_some_and(|s| !s.is_empty())
    }

    pub(super) fn local_subscribers(&self, topic: &Topic) -> Option<&KeySet<ParticipantKey>> {
        self.local_subscribers.get(topic)
    }

    pub(super) fn subscribe_local(&mut self, subscriber: ParticipantKey, topic: Topic) -> bool {
        let subscribers = self
            .local_subscribers
            .entry(topic)
            .or_insert_with(|| KeySet::with_capacity(256));
        let was_empty = subscribers.is_empty();
        let inserted = subscribers.insert_unique(subscriber);
        debug_assert!(inserted);
        was_empty
    }

    pub(super) fn unsubscribe_local(&mut self, subscriber: ParticipantKey, topic: &Topic) -> bool {
        let Some(subscribers) = self.local_subscribers.get_mut(topic) else {
            debug_assert!(false, "unsubscribing from an unknown reliable topic");
            return false;
        };
        let was_last = subscribers.len() == 1;
        let removed = subscribers.remove_value(&subscriber);
        debug_assert!(removed);
        if subscribers.is_empty() {
            self.local_subscribers.remove(topic);
        }
        was_last
    }
}
