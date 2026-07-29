use ahash::{HashMap, HashMapExt};
use str0m::channel::ChannelId;

use crate::entity::ParticipantId;
use crate::participant::event::ParticipantSink;
use crate::track::{DataTopicChannel, DataTrackDirection, Topic};

pub(super) struct ReliableChannels {
    publishers: HashMap<Topic, ChannelId>,
    subscribers: HashMap<(Topic, ParticipantId), ChannelId>,
}

impl ReliableChannels {
    pub(super) fn new() -> Self {
        Self {
            publishers: HashMap::new(),
            subscribers: HashMap::new(),
        }
    }

    pub(super) fn publisher_channel(&self, topic: &Topic) -> Option<ChannelId> {
        self.publishers.get(topic).copied()
    }

    pub(super) fn subscriber_channel(
        &self,
        topic: &Topic,
        publisher: ParticipantId,
    ) -> Option<ChannelId> {
        self.subscribers.get(&(topic.clone(), publisher)).copied()
    }

    pub(super) fn open(
        &mut self,
        channel_id: ChannelId,
        channel: &DataTopicChannel,
        events: &mut impl ParticipantSink,
    ) -> Result<(), ()> {
        debug_assert_eq!(channel.lane, crate::track::DataLane::Reliable);
        match channel.direction {
            DataTrackDirection::Publish => {
                debug_assert!(channel.scope.is_none());
                if self.publishers.contains_key(&channel.topic) {
                    return Err(());
                }
                self.publishers.insert(channel.topic.clone(), channel_id);
                events.publish_reliable_data_topic(channel.topic.clone());
            }
            DataTrackDirection::Subscribe => {
                let Some(publisher) = channel.scope else {
                    debug_assert!(false, "reliable subscriber must identify its publisher");
                    return Err(());
                };
                let key = (channel.topic.clone(), publisher);
                if self.subscribers.contains_key(&key) {
                    return Err(());
                }
                self.subscribers.insert(key, channel_id);
                events.subscribe_reliable_data_topic(channel.topic.clone(), publisher);
            }
        }
        Ok(())
    }

    pub(super) fn close(&mut self, channel: DataTopicChannel, events: &mut impl ParticipantSink) {
        debug_assert_eq!(channel.lane, crate::track::DataLane::Reliable);
        match channel.direction {
            DataTrackDirection::Publish => {
                debug_assert!(channel.scope.is_none());
                let removed = self.publishers.remove(&channel.topic);
                debug_assert!(removed.is_some());
                events.unpublish_reliable_data_topic(channel.topic);
            }
            DataTrackDirection::Subscribe => {
                let publisher = channel
                    .scope
                    .expect("reliable subscriber must identify its publisher");
                let removed = self.subscribers.remove(&(channel.topic.clone(), publisher));
                debug_assert!(removed.is_some());
                events.unsubscribe_reliable_data_topic(channel.topic, publisher);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.publishers.clear();
        self.subscribers.clear();
    }
}
