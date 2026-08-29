use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    vec,
    vec::Vec,
};

use proto::{
    prelude::Message,
    reliable::{RelControl, RelDelivery, RelMsg, RelNack, rel_control},
};

use crate::{
    DataChannelId, Generation, TopicDirection, TopicKind, TopicRegistration,
    context::{AgentContext, AgentEffect, DataChannelConfig, DataChannelEffect},
};

pub const ORDERED_REPLAY_CAPACITY: usize = 256;
pub const MAX_TOPIC_LABEL_LEN: usize = 96;

#[derive(thiserror::Error, Clone, PartialEq, Eq, Debug)]
pub enum TopicError {
    #[error("topic is not registered")]
    Unregistered,
    #[error("topic is not a publisher")]
    NotPublisher,
    #[error("topic channel is not open")]
    NotOpen,
    #[error("topic label exceeds {MAX_TOPIC_LABEL_LEN} bytes")]
    LabelTooLong,
    #[error("topic contains unsupported characters")]
    IllegalTopic,
    #[error("topic scope is empty")]
    EmptyScope,
    #[error("topic scope is supported only by latest subscribers")]
    InvalidScope,
}

pub fn data_channel_config(
    registration: &TopicRegistration,
) -> Result<DataChannelConfig, TopicError> {
    validate_registration(registration)?;
    Ok(DataChannelConfig {
        label: topic_label(registration),
        protocol: String::from("pulsebeam/v1"),
        ordered: registration.kind == TopicKind::Ordered,
        negotiated: None,
        reliability: match registration.kind {
            TopicKind::Latest => crate::DataChannelReliability::MaxRetransmits(0),
            TopicKind::Ordered => crate::DataChannelReliability::Reliable,
        },
    })
}

pub fn validate_registration(registration: &TopicRegistration) -> Result<(), TopicError> {
    if !registration
        .topic
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(TopicError::IllegalTopic);
    }
    if registration
        .publisher_id
        .as_ref()
        .is_some_and(String::is_empty)
    {
        return Err(TopicError::EmptyScope);
    }
    if registration.publisher_id.is_some()
        && (registration.kind != TopicKind::Latest
            || registration.direction != TopicDirection::Subscribe)
    {
        return Err(TopicError::InvalidScope);
    }
    if topic_label(registration).len() > MAX_TOPIC_LABEL_LEN {
        return Err(TopicError::LabelTooLong);
    }
    Ok(())
}

pub fn topic_label(registration: &TopicRegistration) -> String {
    let lane = match registration.kind {
        TopicKind::Latest => "rt",
        TopicKind::Ordered => "rel",
    };
    let direction = match registration.direction {
        TopicDirection::Publish => "pub",
        TopicDirection::Subscribe => "sub",
    };
    let mut label = alloc::format!("v1/{lane}/{direction}/{}", registration.topic);
    if let Some(publisher_id) = &registration.publisher_id {
        label.push('/');
        label.push_str(publisher_id);
    }
    label
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OrderedDelivery {
    Message {
        publisher_id: String,
        stream_id: u64,
        sequence: u64,
        payload: Vec<u8>,
    },
    Resync {
        publisher_id: String,
        stream_id: u64,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TopicDelivery {
    Latest {
        registration: TopicRegistration,
        payload: Vec<u8>,
    },
    Ordered {
        registration: TopicRegistration,
        delivery: OrderedDelivery,
    },
}

pub struct OrderedPublisher {
    stream_id: u64,
    next_sequence: u64,
    replay: VecDeque<RelMsg>,
}

impl OrderedPublisher {
    pub fn new(stream_id: u64) -> Self {
        Self {
            stream_id,
            next_sequence: 0,
            replay: VecDeque::with_capacity(ORDERED_REPLAY_CAPACITY),
        }
    }

    pub fn stream_id(&self) -> u64 {
        self.stream_id
    }

    pub fn accept(&mut self, payload: Vec<u8>) -> Vec<u8> {
        let message = RelMsg {
            stream_id: self.stream_id,
            seq: self.next_sequence,
            payload,
            resync_required: false,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        debug_assert_ne!(self.next_sequence, 0, "ordered sequence space exhausted");
        if self.replay.len() == ORDERED_REPLAY_CAPACITY {
            let removed = self.replay.pop_front();
            debug_assert!(removed.is_some());
        }
        self.replay.push_back(message.clone());
        message.encode_to_vec()
    }

    pub fn recover(&self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let Ok(control) = RelControl::decode(bytes) else {
            return Vec::new();
        };
        let Some(rel_control::Msg::Nack(nack)) = control.msg else {
            return Vec::new();
        };
        if nack.stream_id != self.stream_id {
            return Vec::new();
        }
        let Some(oldest) = self.replay.front() else {
            return Vec::new();
        };
        if nack.from_seq < oldest.seq {
            return vec![
                RelMsg {
                    stream_id: self.stream_id,
                    seq: self.next_sequence,
                    payload: Vec::new(),
                    resync_required: true,
                }
                .encode_to_vec(),
            ];
        }
        self.replay
            .iter()
            .filter(|message| message.seq >= nack.from_seq)
            .map(Message::encode_to_vec)
            .collect()
    }
}

pub struct OrderedSubscriber {
    publishers: BTreeMap<String, SubscriberState>,
}

struct SubscriberState {
    stream_id: u64,
    expected: u64,
    pending: BTreeMap<u64, RelMsg>,
}

impl OrderedSubscriber {
    pub fn new() -> Self {
        Self {
            publishers: BTreeMap::new(),
        }
    }

    pub fn receive(&mut self, bytes: &[u8]) -> (Vec<OrderedDelivery>, Option<Vec<u8>>) {
        let Ok(delivery) = RelDelivery::decode(bytes) else {
            return (Vec::new(), None);
        };
        let Ok(message) = RelMsg::decode(delivery.frame.as_slice()) else {
            return (Vec::new(), None);
        };
        let state = self
            .publishers
            .entry(delivery.publisher_id.clone())
            .or_insert_with(|| SubscriberState {
                stream_id: message.stream_id,
                expected: message.seq,
                pending: BTreeMap::new(),
            });
        if message.resync_required || state.stream_id != message.stream_id {
            state.stream_id = message.stream_id;
            state.expected = message.seq.wrapping_add(1);
            debug_assert_ne!(state.expected, 0, "ordered sequence space exhausted");
            state.pending.clear();
            return (
                vec![OrderedDelivery::Resync {
                    publisher_id: delivery.publisher_id,
                    stream_id: message.stream_id,
                }],
                None,
            );
        }
        if message.seq < state.expected {
            return (Vec::new(), None);
        }
        if message.seq.saturating_sub(state.expected) >= ORDERED_REPLAY_CAPACITY as u64 {
            state.expected = message.seq.wrapping_add(1);
            debug_assert_ne!(state.expected, 0, "ordered sequence space exhausted");
            state.pending.clear();
            return (
                vec![OrderedDelivery::Resync {
                    publisher_id: delivery.publisher_id,
                    stream_id: message.stream_id,
                }],
                None,
            );
        }
        state.pending.insert(message.seq, message);
        let mut output = Vec::new();
        while let Some(message) = state.pending.remove(&state.expected) {
            output.push(OrderedDelivery::Message {
                publisher_id: delivery.publisher_id.clone(),
                stream_id: message.stream_id,
                sequence: message.seq,
                payload: message.payload,
            });
            state.expected = state.expected.wrapping_add(1);
            debug_assert_ne!(state.expected, 0, "ordered sequence space exhausted");
        }
        let nack = (!state.pending.is_empty()).then(|| {
            RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id: state.stream_id,
                    from_seq: state.expected,
                    publisher_id: delivery.publisher_id,
                })),
            }
            .encode_to_vec()
        });
        (output, nack)
    }
}

impl Default for OrderedSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TopicRegistry {
    entries: BTreeMap<TopicRegistration, TopicEntry>,
    generation: Option<Generation>,
}

struct TopicEntry {
    channel: Option<DataChannelId>,
    open: bool,
    ordered_publisher: Option<OrderedPublisher>,
    ordered_subscriber: Option<OrderedSubscriber>,
}

impl TopicRegistry {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            generation: None,
        }
    }

    pub fn reconcile(&mut self, registrations: &[TopicRegistration]) -> Vec<DataChannelId> {
        let wanted: BTreeMap<_, _> = registrations
            .iter()
            .cloned()
            .map(|registration| (registration, ()))
            .collect();
        let removed = self
            .entries
            .iter()
            .filter(|(registration, _)| !wanted.contains_key(*registration))
            .filter_map(|(_, entry)| entry.channel)
            .collect();
        self.entries
            .retain(|registration, _| wanted.contains_key(registration));
        for registration in registrations {
            self.entries
                .entry(registration.clone())
                .or_insert_with(TopicEntry::new);
        }
        removed
    }

    pub(crate) fn activate(
        &mut self,
        generation: Generation,
        cx: &mut AgentContext,
    ) -> Result<(), TopicError> {
        self.generation = Some(generation);
        for (registration, entry) in &mut self.entries {
            if entry.channel.is_some() {
                continue;
            }
            let id = cx.data_channel_id().ok_or(TopicError::NotOpen)?;
            entry.channel = Some(id);
            entry.open = false;
            entry.ordered_publisher = (registration.kind == TopicKind::Ordered
                && registration.direction == TopicDirection::Publish)
                .then(|| OrderedPublisher::new(generation.get()));
            entry.ordered_subscriber = (registration.kind == TopicKind::Ordered
                && registration.direction == TopicDirection::Subscribe)
                .then(OrderedSubscriber::new);
            cx.dc_open(generation, id, data_channel_config(registration)?);
        }
        Ok(())
    }

    pub fn invalidate_generation(&mut self) -> Vec<DataChannelId> {
        self.generation = None;
        self.entries
            .values_mut()
            .filter_map(|entry| {
                entry.open = false;
                entry.ordered_publisher = None;
                entry.ordered_subscriber = None;
                entry.channel.take()
            })
            .collect()
    }

    pub fn opened(&mut self, generation: Generation, id: DataChannelId) -> bool {
        if self.generation != Some(generation) {
            return false;
        }
        let Some(entry) = self
            .entries
            .values_mut()
            .find(|entry| entry.channel == Some(id))
        else {
            return false;
        };
        entry.open = true;
        true
    }

    pub fn send(
        &mut self,
        registration: &TopicRegistration,
        payload: Vec<u8>,
    ) -> Result<AgentEffect, TopicError> {
        let entry = self
            .entries
            .get_mut(registration)
            .ok_or(TopicError::Unregistered)?;
        if registration.direction != TopicDirection::Publish {
            return Err(TopicError::NotPublisher);
        }
        let (Some(generation), Some(id)) = (self.generation, entry.channel) else {
            return Err(TopicError::NotOpen);
        };
        if !entry.open {
            return Err(TopicError::NotOpen);
        }
        let payload = match &mut entry.ordered_publisher {
            Some(publisher) => publisher.accept(payload),
            None => payload,
        };
        Ok(AgentEffect::DataChannel(DataChannelEffect::Send {
            generation,
            id,
            payload,
        }))
    }

    pub fn receive(&mut self, id: DataChannelId, payload: &[u8]) -> TopicReceive {
        let Some((registration, entry)) = self
            .entries
            .iter_mut()
            .find(|(_, entry)| entry.channel == Some(id) && entry.open)
        else {
            return TopicReceive::Ignored;
        };
        match registration.kind {
            TopicKind::Latest => TopicReceive::Delivery(TopicDelivery::Latest {
                registration: registration.clone(),
                payload: payload.to_vec(),
            }),
            TopicKind::Ordered if registration.direction == TopicDirection::Publish => {
                let Some(publisher) = &entry.ordered_publisher else {
                    return TopicReceive::Ignored;
                };
                TopicReceive::Replay(publisher.recover(payload))
            }
            TopicKind::Ordered => {
                let Some(subscriber) = &mut entry.ordered_subscriber else {
                    return TopicReceive::Ignored;
                };
                let (deliveries, nack) = subscriber.receive(payload);
                TopicReceive::Ordered {
                    registration: registration.clone(),
                    deliveries,
                    nack,
                }
            }
        }
    }
}

impl Default for TopicRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TopicEntry {
    fn new() -> Self {
        Self {
            channel: None,
            open: false,
            ordered_publisher: None,
            ordered_subscriber: None,
        }
    }
}

pub enum TopicReceive {
    Ignored,
    Delivery(TopicDelivery),
    Replay(Vec<Vec<u8>>),
    Ordered {
        registration: TopicRegistration,
        deliveries: Vec<OrderedDelivery>,
        nack: Option<Vec<u8>>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests use direct assertions")]
    use super::*;

    fn wrap(publisher_id: &str, frame: Vec<u8>) -> Vec<u8> {
        RelDelivery {
            publisher_id: String::from(publisher_id),
            frame,
        }
        .encode_to_vec()
    }

    #[test]
    fn labels_match_server_declarations() {
        let latest = TopicRegistration {
            topic: String::from("game-sync"),
            kind: TopicKind::Latest,
            direction: TopicDirection::Publish,
            publisher_id: None,
        };
        let config = data_channel_config(&latest).unwrap();
        assert_eq!(config.label, "v1/rt/pub/game-sync");
        assert!(!config.ordered);
        assert_eq!(
            config.reliability,
            crate::DataChannelReliability::MaxRetransmits(0)
        );
        let ordered = TopicRegistration {
            topic: String::from("chat"),
            kind: TopicKind::Ordered,
            direction: TopicDirection::Subscribe,
            publisher_id: None,
        };
        let config = data_channel_config(&ordered).unwrap();
        assert_eq!(config.label, "v1/rel/sub/chat");
        assert!(config.ordered);
    }

    #[test]
    fn ordered_gap_is_replayed_and_duplicates_are_suppressed() {
        let mut publisher = OrderedPublisher::new(7);
        let first = publisher.accept(vec![1]);
        let _second = publisher.accept(vec![2]);
        let third = publisher.accept(vec![3]);
        let mut subscriber = OrderedSubscriber::new();
        assert_eq!(subscriber.receive(&wrap("publisher", first)).0.len(), 1);
        let (_, nack) = subscriber.receive(&wrap("publisher", third));
        let replay = publisher.recover(&nack.unwrap());
        let (delivered, _) = subscriber.receive(&wrap("publisher", replay[0].clone()));
        assert_eq!(delivered.len(), 2);
        assert_eq!(
            delivered[0],
            OrderedDelivery::Message {
                publisher_id: String::from("publisher"),
                stream_id: 7,
                sequence: 1,
                payload: vec![2]
            }
        );
        assert_eq!(
            delivered[1],
            OrderedDelivery::Message {
                publisher_id: String::from("publisher"),
                stream_id: 7,
                sequence: 2,
                payload: vec![3]
            }
        );
        assert!(
            subscriber
                .receive(&wrap("publisher", replay[1].clone()))
                .0
                .is_empty()
        );
    }

    #[test]
    fn ordered_recovery_window_exhaustion_resyncs() {
        let mut publisher = OrderedPublisher::new(9);
        for sequence in 0..=ORDERED_REPLAY_CAPACITY {
            let payload = u8::try_from(sequence & usize::from(u8::MAX)).unwrap();
            let _ = publisher.accept(vec![payload]);
        }
        let control = RelControl {
            msg: Some(rel_control::Msg::Nack(RelNack {
                stream_id: 9,
                from_seq: 0,
                publisher_id: String::from("publisher"),
            })),
        }
        .encode_to_vec();
        let recovery = publisher.recover(&control);
        let message = RelMsg::decode(recovery[0].as_slice()).unwrap();
        assert!(message.resync_required);
        assert_eq!(message.stream_id, 9);
    }

    #[test]
    fn a_new_stream_notifies_the_subscriber() {
        let mut subscriber = OrderedSubscriber::new();
        let _ = subscriber.receive(&wrap(
            "publisher",
            RelMsg {
                stream_id: 1,
                seq: 0,
                payload: vec![1],
                resync_required: false,
            }
            .encode_to_vec(),
        ));
        let (deliveries, _) = subscriber.receive(&wrap(
            "publisher",
            RelMsg {
                stream_id: 2,
                seq: 0,
                payload: Vec::new(),
                resync_required: false,
            }
            .encode_to_vec(),
        ));
        assert_eq!(
            deliveries,
            vec![OrderedDelivery::Resync {
                publisher_id: String::from("publisher"),
                stream_id: 2
            }]
        );
    }
}
