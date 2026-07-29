use std::collections::{BTreeMap, HashMap, VecDeque};

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, RelDelivery, RelMsg, RelNack, rel_control};
use str0m::Rtc;
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};

use super::driver::DataTrackDirection;
use super::handles::{
    OrderedTopicDelivery, OrderedTopicMessage, OrderedTopicPublisher, OrderedTopicSubscriber,
    OutgoingCommand,
};
use super::mailbox;

const RETRANSMIT_CAPACITY: usize = 256;
const DELIVERY_CAPACITY: usize = 256;
const REORDER_CAPACITY: usize = 256;

struct Publisher {
    channel_id: ChannelId,
    opened: bool,
    stream_id: u64,
    next_seq: u64,
    retransmits: VecDeque<RelMsg>,
    handle: OrderedTopicPublisher,
}

struct Subscriber {
    channel_id: ChannelId,
    target: mailbox::Sender<OrderedTopicDelivery>,
    publishers: HashMap<String, PublisherDelivery>,
}

struct PublisherDelivery {
    stream_id: u64,
    next_seq: u64,
    pending: BTreeMap<u64, RelMsg>,
}

impl PublisherDelivery {
    fn new(message: &RelMsg) -> Self {
        Self {
            stream_id: message.stream_id,
            next_seq: message.seq,
            pending: BTreeMap::new(),
        }
    }

    fn accept(
        &mut self,
        publisher_id: &str,
        message: RelMsg,
    ) -> (Vec<OrderedTopicDelivery>, Option<u64>) {
        let stream_id = message.stream_id;
        let mut events = Vec::new();
        if self.stream_id != stream_id {
            self.stream_id = stream_id;
            self.next_seq = message.seq;
            self.pending.clear();
            events.push(OrderedTopicDelivery::ResyncRequired {
                publisher_id: publisher_id.to_string(),
                new_stream_id: stream_id,
            });
        }
        if message.seq >= self.next_seq {
            if message.seq - self.next_seq >= REORDER_CAPACITY as u64 {
                self.next_seq = message.seq;
                self.pending.clear();
                events.push(OrderedTopicDelivery::ResyncRequired {
                    publisher_id: publisher_id.to_string(),
                    new_stream_id: stream_id,
                });
            }
            self.pending.entry(message.seq).or_insert(message);
        }
        while let Some(current) = self.pending.remove(&self.next_seq) {
            debug_assert_eq!(current.stream_id, self.stream_id);
            debug_assert_eq!(current.seq, self.next_seq);
            events.push(OrderedTopicDelivery::Message(OrderedTopicMessage {
                publisher_id: publisher_id.to_string(),
                stream_id: current.stream_id,
                seq: current.seq,
                payload: current.payload,
            }));
            self.next_seq = self.next_seq.wrapping_add(1);
            debug_assert_ne!(self.next_seq, 0, "reliable sequence exhausted");
        }
        let nack_from = (!self.pending.is_empty()).then_some(self.next_seq);
        (events, nack_from)
    }
}

pub(super) struct OrderedTopics {
    publishers: HashMap<String, Publisher>,
    publisher_channels: HashMap<ChannelId, String>,
    subscribers: HashMap<String, Subscriber>,
    subscriber_channels: HashMap<ChannelId, String>,
}

impl OrderedTopics {
    pub(super) fn new() -> Self {
        Self {
            publishers: HashMap::new(),
            publisher_channels: HashMap::new(),
            subscribers: HashMap::new(),
            subscriber_channels: HashMap::new(),
        }
    }

    pub(super) fn declare_publisher(
        &mut self,
        rtc: &mut Rtc,
        topic: &str,
        tx: mailbox::Sender<OutgoingCommand>,
    ) -> Result<OrderedTopicPublisher, ()> {
        debug_assert!(!topic.is_empty());
        if let Some(publisher) = self.publishers.get(topic) {
            return Ok(publisher.handle.clone());
        }
        let channel_id = add_channel(rtc, DataTrackDirection::Publish, topic, None)?;
        let handle = OrderedTopicPublisher {
            topic: topic.to_string(),
            channel_id,
            tx,
        };
        self.publishers.insert(
            topic.to_string(),
            Publisher {
                channel_id,
                opened: false,
                stream_id: 1,
                next_seq: 0,
                retransmits: VecDeque::with_capacity(RETRANSMIT_CAPACITY),
                handle: handle.clone(),
            },
        );
        self.publisher_channels
            .insert(channel_id, topic.to_string());
        Ok(handle)
    }

    pub(super) fn declare_subscriber(
        &mut self,
        rtc: &mut Rtc,
        topic: &str,
    ) -> Result<OrderedTopicSubscriber, ()> {
        debug_assert!(!topic.is_empty());
        if let Some(subscriber) = self.subscribers.get(topic) {
            debug_assert!(false, "ordered topic subscriber already declared");
            let _ = subscriber;
            return Err(());
        }
        let channel_id = add_channel(rtc, DataTrackDirection::Subscribe, topic, None)?;
        let (target, rx) = mailbox::bounded(DELIVERY_CAPACITY);
        let handle = OrderedTopicSubscriber {
            topic: topic.to_string(),
            rx,
        };
        self.subscribers.insert(
            topic.to_string(),
            Subscriber {
                channel_id,
                target,
                publishers: HashMap::new(),
            },
        );
        self.subscriber_channels
            .insert(channel_id, topic.to_string());
        Ok(handle)
    }

    pub(super) fn open_channel(&mut self, channel_id: ChannelId, label: &str) -> bool {
        let Some((direction, topic, publisher_id)) = parse_label(label) else {
            return false;
        };
        match direction {
            DataTrackDirection::Publish => {
                debug_assert!(publisher_id.is_none());
                let Some(publisher) = self.publishers.get_mut(&topic) else {
                    return true;
                };
                publisher.channel_id = channel_id;
                publisher.handle.channel_id = channel_id;
                if publisher.opened {
                    publisher.stream_id = publisher.stream_id.wrapping_add(1);
                } else {
                    publisher.opened = true;
                }
                debug_assert_ne!(publisher.stream_id, 0);
                publisher.next_seq = 0;
                publisher.retransmits.clear();
                self.publisher_channels
                    .retain(|_, existing_topic| *existing_topic != topic);
                self.publisher_channels.insert(channel_id, topic);
            }
            DataTrackDirection::Subscribe => {
                debug_assert!(publisher_id.is_none());
                let Some(subscriber) = self.subscribers.get_mut(&topic) else {
                    return true;
                };
                subscriber.channel_id = channel_id;
                subscriber.publishers.clear();
                self.subscriber_channels
                    .retain(|_, existing_topic| existing_topic != &topic);
                self.subscriber_channels.insert(channel_id, topic);
            }
        }
        true
    }

    pub(super) fn send(
        &mut self,
        rtc: &mut Rtc,
        channel_id: ChannelId,
        payload: Vec<u8>,
    ) -> Result<(), Vec<u8>> {
        let Some(topic) = self.publisher_channels.get(&channel_id) else {
            return Err(payload);
        };
        let Some(publisher) = self.publishers.get_mut(topic) else {
            debug_assert!(false, "reliable publisher channel has no publisher");
            return Ok(());
        };
        debug_assert_eq!(publisher.channel_id, channel_id);
        debug_assert_ne!(publisher.stream_id, 0);
        let message = RelMsg {
            stream_id: publisher.stream_id,
            seq: publisher.next_seq,
            payload,
            resync_required: false,
        };
        publisher.next_seq = publisher.next_seq.wrapping_add(1);
        debug_assert_ne!(publisher.next_seq, 0, "reliable sequence exhausted");
        let encoded = message.encode_to_vec();
        if publisher.retransmits.len() == RETRANSMIT_CAPACITY {
            publisher.retransmits.pop_front();
        }
        debug_assert!(publisher.retransmits.len() < RETRANSMIT_CAPACITY);
        publisher.retransmits.push_back(message);
        if let Some(mut channel) = rtc.channel(channel_id) {
            let _ = channel.write(true, &encoded);
        }
        Ok(())
    }

    pub(super) fn handle_data(&mut self, rtc: &mut Rtc, data: &ChannelData) -> bool {
        if let Some(topic) = self.publisher_channels.get(&data.id).cloned() {
            self.handle_control(rtc, data.id, &topic, data.data.as_ref());
            return true;
        }
        let Some(topic) = self.subscriber_channels.get(&data.id).cloned() else {
            return false;
        };
        self.handle_message(rtc, data.id, &topic, data.data.as_ref());
        true
    }

    fn handle_control(&self, rtc: &mut Rtc, channel_id: ChannelId, topic: &str, bytes: &[u8]) {
        let Ok(control) = RelControl::decode(bytes) else {
            return;
        };
        let Some(rel_control::Msg::Nack(nack)) = control.msg else {
            return;
        };
        let Some(publisher) = self.publishers.get(topic) else {
            debug_assert!(false, "reliable control targets an unknown publisher");
            return;
        };
        if nack.stream_id != publisher.stream_id {
            return;
        }
        if let Some(mut channel) = rtc.channel(channel_id) {
            if let Some(earliest) = publisher.retransmits.front()
                && earliest.seq > nack.from_seq
            {
                let reset = RelMsg {
                    stream_id: publisher.stream_id,
                    seq: earliest.seq,
                    payload: Vec::new(),
                    resync_required: true,
                };
                let _ = channel.write(true, &reset.encode_to_vec());
            }
            for message in publisher
                .retransmits
                .iter()
                .filter(|message| message.seq >= nack.from_seq)
            {
                let _ = channel.write(true, &message.encode_to_vec());
            }
        }
    }

    fn handle_message(&mut self, rtc: &mut Rtc, channel_id: ChannelId, topic: &str, bytes: &[u8]) {
        let Ok(delivery) = RelDelivery::decode(bytes) else {
            return;
        };
        if delivery.publisher_id.is_empty() {
            return;
        }
        let Ok(message) = RelMsg::decode(delivery.frame.as_slice()) else {
            return;
        };
        debug_assert_ne!(message.stream_id, 0);
        let Some(subscriber) = self.subscribers.get_mut(topic) else {
            debug_assert!(false, "reliable channel targets an unknown subscriber");
            return;
        };
        let publisher_id = delivery.publisher_id;
        let stream_id = message.stream_id;
        let state = subscriber
            .publishers
            .entry(publisher_id.clone())
            .or_insert_with(|| PublisherDelivery::new(&message));
        if message.resync_required {
            state.stream_id = message.stream_id;
            state.next_seq = message.seq;
            state.pending.clear();
            let result = subscriber
                .target
                .try_send(OrderedTopicDelivery::ResyncRequired {
                    publisher_id,
                    new_stream_id: message.stream_id,
                });
            debug_assert!(result.is_ok());
            return;
        }
        let (events, nack_from) = state.accept(&publisher_id, message);
        for event in events {
            let result = subscriber.target.try_send(event);
            debug_assert!(result.is_ok());
        }
        if let Some(from_seq) = nack_from {
            let nack = RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id,
                    from_seq,
                    publisher_id,
                })),
            };
            if let Some(mut channel) = rtc.channel(channel_id) {
                let _ = channel.write(true, &nack.encode_to_vec());
            }
        }
    }
}

fn add_channel(
    rtc: &mut Rtc,
    direction: DataTrackDirection,
    topic: &str,
    publisher_id: Option<&str>,
) -> Result<ChannelId, ()> {
    let config = ChannelConfig {
        label: label(direction, topic, publisher_id),
        ordered: true,
        reliability: Reliability::Reliable,
        negotiated: None,
        protocol: String::new(),
    };
    let mut sdp_api = rtc.sdp_api();
    let channel_id = sdp_api.add_channel_with_config(config);
    if sdp_api.apply().is_some() {
        return Err(());
    }
    Ok(channel_id)
}

fn label(direction: DataTrackDirection, topic: &str, publisher_id: Option<&str>) -> String {
    debug_assert!(!topic.is_empty());
    debug_assert!(publisher_id.is_none());
    let direction = match direction {
        DataTrackDirection::Publish => "pub",
        DataTrackDirection::Subscribe => "sub",
    };
    match publisher_id {
        Some(publisher_id) => format!("v1/rel/{direction}/{topic}/{publisher_id}"),
        None => format!("v1/rel/{direction}/{topic}"),
    }
}

fn parse_label(label: &str) -> Option<(DataTrackDirection, String, Option<String>)> {
    let rest = label.strip_prefix("v1/rel/")?;
    let (direction, rest) = rest.split_once('/')?;
    let direction = match direction {
        "pub" => DataTrackDirection::Publish,
        "sub" => DataTrackDirection::Subscribe,
        _ => return None,
    };
    let (topic, publisher_id) = match rest.split_once('/') {
        Some((topic, publisher_id)) => (topic, Some(publisher_id.to_string())),
        None => (rest, None),
    };
    if topic.is_empty() || publisher_id.is_some() {
        return None;
    }
    Some((direction, topic.to_string(), publisher_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_as_topic_wide_channels() {
        let publisher = label(DataTrackDirection::Publish, "chat", None);
        assert_eq!(
            parse_label(&publisher),
            Some((DataTrackDirection::Publish, "chat".to_string(), None))
        );

        let subscriber = label(DataTrackDirection::Subscribe, "chat", None);
        assert_eq!(
            parse_label(&subscriber),
            Some((DataTrackDirection::Subscribe, "chat".to_string(), None))
        );
    }

    #[test]
    fn malformed_or_cross_lane_labels_are_rejected() {
        for label in [
            "v1/rt/pub/chat",
            "v1/rel/pub/chat/publisher",
            "v1/rel/pub/",
            "v1/rel/sub/chat/",
        ] {
            assert!(parse_label(label).is_none(), "{label}");
        }
    }

    #[test]
    fn publisher_delivery_reorders_gaps_and_deduplicates_retransmits() {
        let first = RelMsg {
            stream_id: 7,
            seq: 0,
            payload: vec![0],
            resync_required: false,
        };
        let mut delivery = PublisherDelivery::new(&first);
        let (events, nack) = delivery.accept("alice", first);
        assert_eq!(message_sequences(&events), vec![0]);
        assert_eq!(nack, None);

        let (events, nack) = delivery.accept(
            "alice",
            RelMsg {
                stream_id: 7,
                seq: 2,
                payload: vec![2],
                resync_required: false,
            },
        );
        assert!(events.is_empty());
        assert_eq!(nack, Some(1));

        let (events, nack) = delivery.accept(
            "alice",
            RelMsg {
                stream_id: 7,
                seq: 1,
                payload: vec![1],
                resync_required: false,
            },
        );
        assert_eq!(message_sequences(&events), vec![1, 2]);
        assert_eq!(nack, None);

        let (events, nack) = delivery.accept(
            "alice",
            RelMsg {
                stream_id: 7,
                seq: 2,
                payload: vec![2],
                resync_required: false,
            },
        );
        assert!(events.is_empty());
        assert_eq!(nack, None);
    }

    #[test]
    fn publisher_delivery_requires_resync_when_stream_changes() {
        let first = RelMsg {
            stream_id: 3,
            seq: 9,
            payload: vec![9],
            resync_required: false,
        };
        let mut delivery = PublisherDelivery::new(&first);
        let _ = delivery.accept("bob", first);

        let (events, nack) = delivery.accept(
            "bob",
            RelMsg {
                stream_id: 4,
                seq: 0,
                payload: vec![0],
                resync_required: false,
            },
        );
        assert_eq!(
            events,
            vec![
                OrderedTopicDelivery::ResyncRequired {
                    publisher_id: "bob".to_string(),
                    new_stream_id: 4,
                },
                OrderedTopicDelivery::Message(OrderedTopicMessage {
                    publisher_id: "bob".to_string(),
                    stream_id: 4,
                    seq: 0,
                    payload: vec![0],
                }),
            ]
        );
        assert_eq!(nack, None);
    }

    fn message_sequences(events: &[OrderedTopicDelivery]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                OrderedTopicDelivery::Message(message) => Some(message.seq),
                OrderedTopicDelivery::ResyncRequired { .. } => None,
            })
            .collect()
    }
}
