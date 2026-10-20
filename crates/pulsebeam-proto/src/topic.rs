use prost::Message;

include!(concat!(env!("OUT_DIR"), "/topic.rs"));

pub const MAX_PAYLOAD_BYTES: usize = 65_536;
pub const MAX_FRAME_BYTES: usize = MAX_PAYLOAD_BYTES.saturating_add(256);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    Malformed,
    InvalidIdentity,
    InvalidSequence,
}

impl TopicFrame {
    pub fn decode_bounded(input: &[u8]) -> Result<Self, FrameError> {
        if input.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let frame = Self::decode(input).map_err(|_| FrameError::Malformed)?;
        let id = frame.data_track_id().ok_or(FrameError::Malformed)?;
        if id.is_empty() || id.len() > 128 {
            return Err(FrameError::InvalidIdentity);
        }
        if let Some(topic_frame::Body::Message(message)) = &frame.body {
            if message.payload.len() > MAX_PAYLOAD_BYTES {
                return Err(FrameError::TooLarge);
            }
            if message.sequence == u64::MAX {
                return Err(FrameError::InvalidSequence);
            }
        }
        Ok(frame)
    }

    pub fn data_track_id(&self) -> Option<&str> {
        Some(match self.body.as_ref()? {
            topic_frame::Body::Message(value) => &value.data_track_id,
            topic_frame::Body::Nack(value) => &value.data_track_id,
            topic_frame::Body::Probe(value) => &value.data_track_id,
            topic_frame::Body::Frontier(value) => &value.data_track_id,
            topic_frame::Body::Floor(value) => &value.data_track_id,
        })
    }

    pub fn is_publisher_output(&self) -> bool {
        matches!(
            self.body,
            Some(
                topic_frame::Body::Message(_)
                    | topic_frame::Body::Frontier(_)
                    | topic_frame::Body::Floor(_)
            )
        )
    }

    pub fn is_subscriber_control(&self) -> bool {
        matches!(
            self.body,
            Some(topic_frame::Body::Nack(_) | topic_frame::Body::Probe(_))
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};

    #[test]
    fn common_envelope_preserves_full_payload_and_lossless_sequence() {
        let frame = TopicFrame {
            body: Some(topic_frame::Body::Message(TopicMessage {
                data_track_id: "opaque-server-data-track".to_string(),
                sequence: u64::MAX.saturating_sub(1),
                payload: vec![42; MAX_PAYLOAD_BYTES],
            })),
        };
        let wire = frame.encode_to_vec();
        assert!(wire.len() > MAX_PAYLOAD_BYTES);
        assert_eq!(TopicFrame::decode_bounded(&wire), Ok(frame.clone()));
        assert_eq!(frame.data_track_id(), Some("opaque-server-data-track"));
        assert!(frame.is_publisher_output());
        assert!(!frame.is_subscriber_control());
    }

    #[test]
    fn controls_are_distinct_from_messages_and_input_is_bounded() {
        let probe = TopicFrame {
            body: Some(topic_frame::Body::Probe(TopicProbe {
                data_track_id: "opaque".to_string(),
                from_sequence: u64::MAX,
                live: false,
            })),
        };
        assert!(probe.is_subscriber_control());
        assert!(!probe.is_publisher_output());
        assert_eq!(
            TopicFrame::decode_bounded(&probe.encode_to_vec()),
            Ok(probe)
        );
        assert_eq!(TopicFrame::decode_bounded(&[]), Err(FrameError::Malformed));
        assert_eq!(
            TopicFrame::decode_bounded(&vec![0; MAX_FRAME_BYTES.saturating_add(1)]),
            Err(FrameError::TooLarge)
        );
        for (id, sequence, error) in [
            ("", 0, FrameError::InvalidIdentity),
            ("opaque", u64::MAX, FrameError::InvalidSequence),
        ] {
            let frame = TopicFrame {
                body: Some(topic_frame::Body::Message(TopicMessage {
                    data_track_id: id.to_string(),
                    sequence,
                    payload: vec![],
                })),
            };
            assert_eq!(
                TopicFrame::decode_bounded(&frame.encode_to_vec()),
                Err(error)
            );
        }
    }
}
