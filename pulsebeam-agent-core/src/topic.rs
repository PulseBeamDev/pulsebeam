use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, RelDelivery, RelMsg, RelNack, rel_control};

const RETRANSMIT_CAPACITY: usize = 256;
const REORDER_CAPACITY: u64 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestMessage {
    pub version: u64,
    pub payload: Vec<u8>,
}

pub struct LatestTopic {
    version: u64,
    value: Option<LatestMessage>,
}

impl LatestTopic {
    pub fn new() -> Self {
        Self {
            version: 0,
            value: None,
        }
    }

    pub fn publish(&mut self, payload: Vec<u8>) -> Result<LatestMessage, TopicError> {
        let version = self
            .version
            .checked_add(1)
            .ok_or(TopicError::SequenceExhausted)?;
        self.version = version;
        let message = LatestMessage { version, payload };
        self.value = Some(message.clone());
        Ok(message)
    }

    pub fn accept(&mut self, message: LatestMessage) -> bool {
        if message.version <= self.version {
            return false;
        }
        self.version = message.version;
        self.value = Some(message);
        true
    }

    pub fn current(&self) -> Option<&LatestMessage> {
        self.value.as_ref()
    }
}

impl Default for LatestTopic {
    fn default() -> Self {
        Self::new()
    }
}

pub struct TopicPublisher {
    stream_id: u64,
    next_seq: u64,
    retransmits: VecDeque<RelMsg>,
}

impl TopicPublisher {
    pub fn new(stream_id: u64) -> Result<Self, TopicError> {
        if stream_id == 0 {
            return Err(TopicError::InvalidStreamId);
        }
        Ok(Self {
            stream_id,
            next_seq: 0,
            retransmits: VecDeque::with_capacity(RETRANSMIT_CAPACITY),
        })
    }

    pub const fn stream_id(&self) -> u64 {
        self.stream_id
    }

    pub fn start_stream(&mut self, stream_id: u64) -> Result<(), TopicError> {
        if stream_id == 0 {
            return Err(TopicError::InvalidStreamId);
        }
        self.stream_id = stream_id;
        self.next_seq = 0;
        self.retransmits.clear();
        Ok(())
    }

    pub fn publish(&mut self, payload: Vec<u8>) -> Result<RelMsg, TopicError> {
        let seq = self.next_seq;
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or(TopicError::SequenceExhausted)?;
        let message = RelMsg {
            stream_id: self.stream_id,
            seq,
            payload,
            resync_required: false,
        };
        if self.retransmits.len() == RETRANSMIT_CAPACITY {
            self.retransmits.pop_front();
        }
        self.retransmits.push_back(message.clone());
        Ok(message)
    }

    pub fn retransmit(&self, nack: &RelNack) -> Vec<RelMsg> {
        if nack.stream_id != self.stream_id {
            return Vec::new();
        }
        let mut output = Vec::new();
        if self
            .retransmits
            .front()
            .is_some_and(|message| message.seq > nack.from_seq)
        {
            output.push(RelMsg {
                stream_id: self.stream_id,
                seq: self.retransmits.front().map_or(0, |message| message.seq),
                payload: Vec::new(),
                resync_required: true,
            });
        }
        output.extend(
            self.retransmits
                .iter()
                .filter(|message| message.seq >= nack.from_seq)
                .cloned(),
        );
        output
    }

    pub fn encode_delivery(publisher_id: impl Into<String>, message: &RelMsg) -> Vec<u8> {
        RelDelivery {
            publisher_id: publisher_id.into(),
            frame: message.encode_to_vec(),
        }
        .encode_to_vec()
    }

    pub fn accept_control(&self, bytes: &[u8]) -> Result<Vec<RelMsg>, TopicError> {
        debug_assert!(!bytes.is_empty());
        let control =
            RelControl::decode(bytes).map_err(|error| TopicError::Decode(error.to_string()))?;
        let Some(rel_control::Msg::Nack(nack)) = control.msg else {
            return Ok(Vec::new());
        };
        Ok(self.retransmit(&nack))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrderedEvent {
    Message {
        publisher_id: String,
        stream_id: u64,
        seq: u64,
        payload: Vec<u8>,
    },
    Nack(RelNack),
    ResyncRequired {
        publisher_id: String,
        stream_id: u64,
    },
}

struct PublisherDelivery {
    stream_id: u64,
    next_seq: u64,
    pending: BTreeMap<u64, RelMsg>,
}

pub struct OrderedReceiver {
    publishers: BTreeMap<String, PublisherDelivery>,
}

impl OrderedReceiver {
    pub fn new() -> Self {
        Self {
            publishers: BTreeMap::new(),
        }
    }

    pub fn accept(
        &mut self,
        publisher_id: impl Into<String>,
        message: RelMsg,
    ) -> Result<Vec<OrderedEvent>, TopicError> {
        let publisher_id = publisher_id.into();
        if publisher_id.is_empty() {
            return Err(TopicError::EmptyPublisherId);
        }
        if message.stream_id == 0 {
            return Err(TopicError::InvalidStreamId);
        }
        let state = self
            .publishers
            .entry(publisher_id.clone())
            .or_insert_with(|| PublisherDelivery {
                stream_id: message.stream_id,
                next_seq: message.seq,
                pending: BTreeMap::new(),
            });
        let mut events = Vec::new();
        if message.resync_required {
            state.stream_id = message.stream_id;
            state.next_seq = message.seq;
            state.pending.clear();
            events.push(OrderedEvent::ResyncRequired {
                publisher_id,
                stream_id: message.stream_id,
            });
            return Ok(events);
        }
        if state.stream_id != message.stream_id {
            state.stream_id = message.stream_id;
            state.next_seq = message.seq;
            state.pending.clear();
            events.push(OrderedEvent::ResyncRequired {
                publisher_id: publisher_id.clone(),
                stream_id: message.stream_id,
            });
        }
        if message.seq >= state.next_seq {
            if message.seq.saturating_sub(state.next_seq) >= REORDER_CAPACITY {
                state.next_seq = message.seq;
                state.pending.clear();
                events.push(OrderedEvent::ResyncRequired {
                    publisher_id: publisher_id.clone(),
                    stream_id: message.stream_id,
                });
            }
            state.pending.entry(message.seq).or_insert(message);
        }
        while let Some(current) = state.pending.remove(&state.next_seq) {
            debug_assert_eq!(current.stream_id, state.stream_id);
            debug_assert_eq!(current.seq, state.next_seq);
            events.push(OrderedEvent::Message {
                publisher_id: publisher_id.clone(),
                stream_id: current.stream_id,
                seq: current.seq,
                payload: current.payload,
            });
            state.next_seq = state
                .next_seq
                .checked_add(1)
                .ok_or(TopicError::SequenceExhausted)?;
        }
        if !state.pending.is_empty() {
            events.push(OrderedEvent::Nack(RelNack {
                stream_id: state.stream_id,
                from_seq: state.next_seq,
                publisher_id,
            }));
        }
        Ok(events)
    }

    pub fn accept_delivery(&mut self, bytes: &[u8]) -> Result<Vec<OrderedEvent>, TopicError> {
        debug_assert!(!bytes.is_empty());
        let delivery =
            RelDelivery::decode(bytes).map_err(|error| TopicError::Decode(error.to_string()))?;
        let message = RelMsg::decode(delivery.frame.as_slice())
            .map_err(|error| TopicError::Decode(error.to_string()))?;
        self.accept(delivery.publisher_id, message)
    }

    pub fn encode_control(event: &OrderedEvent) -> Option<Vec<u8>> {
        let OrderedEvent::Nack(nack) = event else {
            return None;
        };
        Some(
            RelControl {
                msg: Some(rel_control::Msg::Nack(nack.clone())),
            }
            .encode_to_vec(),
        )
    }

    pub fn decode_control(bytes: &[u8]) -> Result<Option<RelNack>, TopicError> {
        debug_assert!(!bytes.is_empty());
        let control =
            RelControl::decode(bytes).map_err(|error| TopicError::Decode(error.to_string()))?;
        Ok(control.msg.map(|rel_control::Msg::Nack(nack)| nack))
    }
}

impl Default for OrderedReceiver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopicError {
    InvalidStreamId,
    EmptyPublisherId,
    SequenceExhausted,
    Decode(String),
}

impl fmt::Display for TopicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStreamId => formatter.write_str("stream id must be non-zero"),
            Self::EmptyPublisherId => formatter.write_str("publisher id must not be empty"),
            Self::SequenceExhausted => formatter.write_str("topic sequence exhausted"),
            Self::Decode(error) => write!(formatter, "invalid topic message: {error}"),
        }
    }
}

impl std::error::Error for TopicError {}

pub type TopicStream = OrderedReceiver;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn latest_topic_drops_old_state() {
        let mut topic = LatestTopic::new();
        let first = topic.publish(vec![1]).unwrap();
        let second = topic.publish(vec![2]).unwrap();
        assert!(!topic.accept(first));
        assert!(topic.accept(LatestMessage {
            version: 3,
            payload: vec![3]
        }));
        assert_eq!(
            topic.current().map(|message| message.payload.clone()),
            Some(vec![3])
        );
        assert!(second.version < topic.current().unwrap().version);
    }

    #[test]
    fn ordered_topic_nacks_gap_and_delivers_after_fill() {
        let mut receiver = OrderedReceiver::new();
        let events = receiver
            .accept(
                "alice",
                RelMsg {
                    stream_id: 1,
                    seq: 0,
                    payload: vec![0],
                    resync_required: false,
                },
            )
            .unwrap();
        assert!(matches!(
            events.first(),
            Some(OrderedEvent::Message { seq: 0, .. })
        ));
        let events = receiver
            .accept(
                "alice",
                RelMsg {
                    stream_id: 1,
                    seq: 2,
                    payload: vec![2],
                    resync_required: false,
                },
            )
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, OrderedEvent::Nack(RelNack { from_seq: 1, .. })))
        );
        let events = receiver
            .accept(
                "alice",
                RelMsg {
                    stream_id: 1,
                    seq: 1,
                    payload: vec![1],
                    resync_required: false,
                },
            )
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    OrderedEvent::Message { seq, .. } => Some(*seq),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn publisher_resyncs_when_nack_is_older_than_retention() {
        let mut publisher = TopicPublisher::new(2).unwrap();
        for _ in 0..257 {
            publisher.publish(vec![0]).unwrap();
        }
        let retransmits = publisher.retransmit(&RelNack {
            stream_id: 2,
            from_seq: 0,
            publisher_id: "alice".to_owned(),
        });
        assert!(
            retransmits
                .first()
                .is_some_and(|message| message.resync_required)
        );
    }
}
