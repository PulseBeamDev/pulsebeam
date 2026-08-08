use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use indexmap::IndexSet;

use super::router::RoutingContext;
use crate::track::Topic;
use crate::{entity::ParticipantId, shard::participants::ParticipantHandle};

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StreamId {
    publisher: ParticipantId,
    topic: Topic,
}

pub(super) struct ReliableRoutes {
    published: HashSet<StreamId>,
    local_subscribers: HashMap<Topic, FastIndexSet<ParticipantHandle>>,
}

impl ReliableRoutes {
    pub(super) fn new() -> Self {
        Self {
            published: HashSet::new(),
            local_subscribers: HashMap::new(),
        }
    }

    pub(super) fn remove_participant(&mut self, participant: ParticipantHandle) {
        for subscribers in self.local_subscribers.values_mut() {
            subscribers.swap_remove(&participant);
        }
        self.local_subscribers
            .retain(|_, subscribers| !subscribers.is_empty());
        self.published
            .retain(|stream| stream.publisher != participant.participant_id());
    }

    pub(super) fn publish(&mut self, publisher: ParticipantId, topic: Topic) {
        let inserted = self.published.insert(StreamId { publisher, topic });
        debug_assert!(inserted);
    }

    pub(super) fn unpublish(&mut self, publisher: ParticipantId, topic: &Topic) {
        let removed = self.published.remove(&StreamId {
            publisher,
            topic: topic.clone(),
        });
        debug_assert!(removed);
    }

    pub(super) fn subscribe_local(&mut self, subscriber: ParticipantHandle, topic: Topic) -> bool {
        let subscribers = self.local_subscribers.entry(topic).or_insert_with(|| {
            IndexSet::with_capacity_and_hasher(256, ahash::RandomState::default())
        });
        let was_empty = subscribers.is_empty();
        let inserted = subscribers.insert(subscriber);
        debug_assert!(inserted);
        was_empty
    }

    pub(super) fn unsubscribe_local(
        &mut self,
        subscriber: ParticipantHandle,
        topic: &Topic,
    ) -> bool {
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
