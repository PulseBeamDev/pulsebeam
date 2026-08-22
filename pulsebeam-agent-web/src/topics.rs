use std::collections::BTreeMap;

use pulsebeam_agent_core::topic::TopicError;
use pulsebeam_agent_core::{
    LatestMessage, LatestTopic, OrderedEvent, OrderedReceiver, TopicPublisher,
};

use crate::interop::{DataChannelConfig, topic_label};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopicEvent {
    Latest { topic: String, payload: Vec<u8> },
    Ordered(OrderedEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicAction {
    pub channel: String,
    pub payload: Vec<u8>,
}

struct OrderedPublisher {
    publisher_id: String,
    publisher: TopicPublisher,
}

pub struct TopicRegistry {
    latest: BTreeMap<String, LatestTopic>,
    ordered_publishers: BTreeMap<String, OrderedPublisher>,
    ordered_receivers: BTreeMap<String, OrderedReceiver>,
    channels: BTreeMap<String, DataChannelConfig>,
}

impl TopicRegistry {
    pub fn new() -> Self {
        Self {
            latest: BTreeMap::new(),
            ordered_publishers: BTreeMap::new(),
            ordered_receivers: BTreeMap::new(),
            channels: BTreeMap::new(),
        }
    }

    pub fn register_latest_publisher(&mut self, topic: impl Into<String>) -> DataChannelConfig {
        let topic = topic.into();
        let channel = topic_label(false, true, &topic, None);
        let config = DataChannelConfig::latest(channel.clone());
        self.latest.entry(topic).or_default();
        self.channels.insert(channel, config.clone());
        config
    }

    pub fn register_latest_subscriber(
        &mut self,
        topic: impl Into<String>,
        publisher_id: Option<&str>,
    ) -> DataChannelConfig {
        let topic = topic.into();
        let channel = topic_label(false, false, &topic, publisher_id);
        let config = DataChannelConfig::latest(channel.clone());
        self.latest.entry(topic).or_default();
        self.channels.insert(channel, config.clone());
        config
    }

    pub fn register_ordered_publisher(
        &mut self,
        topic: impl Into<String>,
        publisher_id: impl Into<String>,
        stream_id: u64,
    ) -> Result<DataChannelConfig, TopicError> {
        let topic = topic.into();
        let publisher_id = publisher_id.into();
        let channel = topic_label(true, true, &topic, None);
        let config = DataChannelConfig::reliable(channel.clone());
        self.ordered_publishers.insert(
            topic,
            OrderedPublisher {
                publisher_id,
                publisher: TopicPublisher::new(stream_id)?,
            },
        );
        self.channels.insert(channel, config.clone());
        Ok(config)
    }

    pub fn register_ordered_subscriber(&mut self, topic: impl Into<String>) -> DataChannelConfig {
        let topic = topic.into();
        let channel = topic_label(true, false, &topic, None);
        let config = DataChannelConfig::reliable(channel.clone());
        self.ordered_receivers.insert(topic, OrderedReceiver::new());
        self.channels.insert(channel, config.clone());
        config
    }

    pub fn channels(&self) -> impl Iterator<Item = &DataChannelConfig> {
        self.channels.values()
    }

    pub fn publish_latest(
        &mut self,
        topic: &str,
        payload: Vec<u8>,
    ) -> Result<TopicAction, TopicError> {
        let state = self
            .latest
            .get_mut(topic)
            .ok_or_else(|| TopicError::Decode("latest topic is not registered".to_owned()))?;
        let message = state.publish(payload)?;
        Ok(TopicAction {
            channel: topic_label(false, true, topic, None),
            payload: message.payload,
        })
    }

    pub fn publish_ordered(
        &mut self,
        topic: &str,
        payload: Vec<u8>,
    ) -> Result<TopicAction, TopicError> {
        let publisher = self
            .ordered_publishers
            .get_mut(topic)
            .ok_or_else(|| TopicError::Decode("ordered topic is not registered".to_owned()))?;
        let message = publisher.publisher.publish(payload)?;
        Ok(TopicAction {
            channel: topic_label(true, true, topic, None),
            payload: publisher
                .publisher
                .encode_delivery(&publisher.publisher_id, &message),
        })
    }

    pub fn receive(
        &mut self,
        channel: &str,
        payload: &[u8],
    ) -> Result<(Vec<TopicEvent>, Vec<TopicAction>), TopicError> {
        debug_assert!(!channel.is_empty());
        if let Some(topic) = channel.strip_prefix("v1/rt/sub/") {
            let topic = topic.split('/').next().unwrap_or(topic);
            let state = self.latest.entry(topic.to_owned()).or_default();
            let version = state
                .current()
                .map_or(1, |message| message.version.saturating_add(1));
            if state.accept(LatestMessage {
                version,
                payload: payload.to_vec(),
            }) {
                return Ok((
                    vec![TopicEvent::Latest {
                        topic: topic.to_owned(),
                        payload: payload.to_vec(),
                    }],
                    Vec::new(),
                ));
            }
            return Ok((Vec::new(), Vec::new()));
        }
        if let Some(topic) = channel.strip_prefix("v1/rel/sub/") {
            let topic = topic.split('/').next().unwrap_or(topic);
            let receiver = self.ordered_receivers.entry(topic.to_owned()).or_default();
            let events = receiver.accept_delivery(payload)?;
            let actions = events
                .iter()
                .filter_map(OrderedReceiver::encode_control)
                .map(|payload| TopicAction {
                    channel: channel.to_owned(),
                    payload,
                })
                .collect();
            return Ok((
                events.into_iter().map(TopicEvent::Ordered).collect(),
                actions,
            ));
        }
        Err(TopicError::Decode("unknown data channel label".to_owned()))
    }
}

impl Default for TopicRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pulsebeam_proto::prelude::Message;

    #[test]
    fn topic_registry_uses_core_ordering_and_reference_labels() {
        let mut topics = TopicRegistry::new();
        topics.register_latest_publisher("presence");
        let latest = topics.publish_latest("presence", vec![1]).unwrap();
        assert_eq!(latest.channel, "v1/rt/pub/presence");

        topics.register_ordered_subscriber("chat");
        let first = pulsebeam_proto::reliable::RelDelivery {
            publisher_id: "alice".to_owned(),
            frame: pulsebeam_proto::reliable::RelMsg {
                stream_id: 1,
                seq: 0,
                payload: vec![2],
                resync_required: false,
            }
            .encode_to_vec(),
        }
        .encode_to_vec();
        let (events, actions) = topics.receive("v1/rel/sub/chat", &first).unwrap();
        assert!(matches!(
            events.first(),
            Some(TopicEvent::Ordered(OrderedEvent::Message { seq: 0, .. }))
        ));
        assert!(actions.is_empty());
    }
}
