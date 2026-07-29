use std::collections::{HashMap, VecDeque};

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, RelMsg, RelNack, rel_control};
use str0m::Rtc;
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};

use super::driver::DataTrackDirection;
use super::handles::{
    OutgoingCommand, ReliableDataEvent, ReliableDataMessage, ReliableDataPublisher,
    ReliableDataSubscriber,
};
use super::mailbox;

const RETRANSMIT_CAPACITY: usize = 256;
const SUBSCRIBER_CAPACITY: usize = 256;

struct Publisher {
    channel_id: ChannelId,
    stream_id: u64,
    next_seq: u64,
    retransmits: VecDeque<RelMsg>,
    handle: ReliableDataPublisher,
}

struct Subscriber {
    channel_id: ChannelId,
    target: mailbox::Sender<ReliableDataEvent>,
    pending_handle: Option<ReliableDataSubscriber>,
    last_received: Option<(u64, u64)>,
}

pub(super) enum ChannelReady {
    Publisher(ReliableDataPublisher),
    Subscriber(ReliableDataSubscriber),
}

pub(super) struct E2eReliable {
    publishers: HashMap<String, Publisher>,
    publisher_channels: HashMap<ChannelId, String>,
    subscribers: HashMap<(String, String), Subscriber>,
    subscriber_channels: HashMap<ChannelId, (String, String)>,
}

impl E2eReliable {
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
    ) -> Result<ChannelId, ()> {
        debug_assert!(!topic.is_empty());
        if let Some(publisher) = self.publishers.get(topic) {
            return Ok(publisher.channel_id);
        }
        let channel_id = add_channel(rtc, DataTrackDirection::Publish, topic, None)?;
        let handle = ReliableDataPublisher {
            topic: topic.to_string(),
            channel_id,
            tx,
        };
        self.publishers.insert(
            topic.to_string(),
            Publisher {
                channel_id,
                stream_id: 0,
                next_seq: 0,
                retransmits: VecDeque::with_capacity(RETRANSMIT_CAPACITY),
                handle,
            },
        );
        Ok(channel_id)
    }

    pub(super) fn declare_subscriber(
        &mut self,
        rtc: &mut Rtc,
        topic: &str,
        publisher_id: &str,
    ) -> Result<ChannelId, ()> {
        debug_assert!(!topic.is_empty());
        debug_assert!(!publisher_id.is_empty());
        let key = (topic.to_string(), publisher_id.to_string());
        if let Some(subscriber) = self.subscribers.get(&key) {
            return Ok(subscriber.channel_id);
        }
        let channel_id = add_channel(
            rtc,
            DataTrackDirection::Subscribe,
            topic,
            Some(publisher_id),
        )?;
        let (target, rx) = mailbox::bounded(SUBSCRIBER_CAPACITY);
        self.subscribers.insert(
            key,
            Subscriber {
                channel_id,
                target,
                pending_handle: Some(ReliableDataSubscriber {
                    topic: topic.to_string(),
                    publisher_id: publisher_id.to_string(),
                    rx,
                }),
                last_received: None,
            },
        );
        Ok(channel_id)
    }

    pub(super) fn open_channel(
        &mut self,
        channel_id: ChannelId,
        label: &str,
    ) -> Option<ChannelReady> {
        let (direction, topic, publisher_id) = parse_label(label)?;
        match direction {
            DataTrackDirection::Publish => {
                debug_assert!(publisher_id.is_none());
                let publisher = self.publishers.get_mut(&topic)?;
                publisher.channel_id = channel_id;
                publisher.handle.channel_id = channel_id;
                publisher.stream_id = publisher.stream_id.wrapping_add(1);
                debug_assert_ne!(publisher.stream_id, 0);
                publisher.next_seq = 0;
                publisher.retransmits.clear();
                self.publisher_channels
                    .retain(|_, existing_topic| *existing_topic != topic);
                self.publisher_channels.insert(channel_id, topic);
                Some(ChannelReady::Publisher(publisher.handle.clone()))
            }
            DataTrackDirection::Subscribe => {
                let publisher_id = publisher_id?;
                let key = (topic, publisher_id);
                let subscriber = self.subscribers.get_mut(&key)?;
                subscriber.channel_id = channel_id;
                subscriber.last_received = None;
                self.subscriber_channels
                    .retain(|_, existing_key| existing_key != &key);
                self.subscriber_channels.insert(channel_id, key);
                subscriber
                    .pending_handle
                    .take()
                    .map(ChannelReady::Subscriber)
            }
        }
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
        let Some(key) = self.subscriber_channels.get(&data.id).cloned() else {
            return false;
        };
        self.handle_message(rtc, data.id, &key, data.data.as_ref());
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
            for message in publisher
                .retransmits
                .iter()
                .filter(|message| message.seq >= nack.from_seq)
            {
                let _ = channel.write(true, &message.encode_to_vec());
            }
        }
    }

    fn handle_message(
        &mut self,
        rtc: &mut Rtc,
        channel_id: ChannelId,
        key: &(String, String),
        bytes: &[u8],
    ) {
        let Ok(message) = RelMsg::decode(bytes) else {
            return;
        };
        debug_assert_ne!(message.stream_id, 0);
        let Some(subscriber) = self.subscribers.get_mut(key) else {
            debug_assert!(false, "reliable channel targets an unknown subscriber");
            return;
        };
        let mut nack_from = None;
        if let Some((stream_id, last_seq)) = subscriber.last_received {
            if stream_id != message.stream_id {
                let _ = subscriber.target.try_send(ReliableDataEvent::StreamReset {
                    new_stream_id: message.stream_id,
                });
            } else {
                let next_expected = last_seq.wrapping_add(1);
                if message.seq > next_expected {
                    nack_from = Some(next_expected);
                }
            }
        }
        if subscriber
            .last_received
            .is_none_or(|(stream_id, last_seq)| {
                stream_id != message.stream_id || message.seq > last_seq
            })
        {
            subscriber.last_received = Some((message.stream_id, message.seq));
        }
        if let Some(from_seq) = nack_from {
            let nack = RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id: message.stream_id,
                    from_seq,
                })),
            };
            if let Some(mut channel) = rtc.channel(channel_id) {
                let _ = channel.write(true, &nack.encode_to_vec());
            }
        }
        let _ = subscriber
            .target
            .try_send(ReliableDataEvent::Message(ReliableDataMessage {
                stream_id: message.stream_id,
                seq: message.seq,
                payload: message.payload,
            }));
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
    debug_assert_eq!(
        publisher_id.is_some(),
        direction == DataTrackDirection::Subscribe
    );
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
    if topic.is_empty()
        || publisher_id.as_ref().is_some_and(String::is_empty)
        || publisher_id.is_some() != (direction == DataTrackDirection::Subscribe)
    {
        return None;
    }
    Some((direction, topic.to_string(), publisher_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_with_required_publisher_scope() {
        let publisher = label(DataTrackDirection::Publish, "chat", None);
        assert_eq!(
            parse_label(&publisher),
            Some((DataTrackDirection::Publish, "chat".to_string(), None))
        );

        let subscriber = label(DataTrackDirection::Subscribe, "chat", Some("publisher"));
        assert_eq!(
            parse_label(&subscriber),
            Some((
                DataTrackDirection::Subscribe,
                "chat".to_string(),
                Some("publisher".to_string())
            ))
        );
    }

    #[test]
    fn malformed_or_cross_lane_labels_are_rejected() {
        for label in [
            "v1/rt/pub/chat",
            "v1/rel/pub/chat/publisher",
            "v1/rel/sub/chat",
            "v1/rel/pub/",
            "v1/rel/sub/chat/",
        ] {
            assert!(parse_label(label).is_none(), "{label}");
        }
    }
}
