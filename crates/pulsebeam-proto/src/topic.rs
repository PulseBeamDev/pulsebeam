use prost::Message;

include!(concat!(env!("OUT_DIR"), "/topic.rs"));

pub const MAX_PAYLOAD_BYTES: usize = 65_536;
pub const MAX_RECIPIENT_CONNECTIONS: usize = 32;
const RECIPIENT_CONNECTION_BYTES: usize = 16;
pub const MAX_FRAME_BYTES: usize = MAX_PAYLOAD_BYTES
    .saturating_add(MAX_RECIPIENT_CONNECTIONS.saturating_mul(RECIPIENT_CONNECTION_BYTES))
    .saturating_add(256);

pub fn data_recipient_id(connection: &[u8; RECIPIENT_CONNECTION_BYTES]) -> alloc::string::String {
    use core::fmt::Write;
    let mut id = alloc::string::String::from("pbr_");
    for octet in connection {
        let _ = write!(id, "{octet:02x}");
    }
    id
}

pub fn parse_data_recipient_id(id: &str) -> Option<[u8; RECIPIENT_CONNECTION_BYTES]> {
    let hex = id.strip_prefix("pbr_")?;
    if hex.len() != RECIPIENT_CONNECTION_BYTES.saturating_mul(2)
        || !hex
            .bytes()
            .all(|octet| matches!(octet, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let mut bytes = [0; RECIPIENT_CONNECTION_BYTES];
    for (out, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(core::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    Malformed,
    InvalidIdentity,
    InvalidSequence,
}

impl From<topic_frame::Body> for TopicFrame {
    fn from(body: topic_frame::Body) -> Self {
        Self {
            body: Some(body),
            recipient_connections: None,
            repair_target: Default::default(),
        }
    }
}

impl TopicFrame {
    pub fn decode_bounded(input: &[u8]) -> Result<Self, FrameError> {
        if input.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let frame = Self::decode(input).map_err(|_| FrameError::Malformed)?;
        let id = frame.publication_id().ok_or(FrameError::Malformed)?;
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
        if matches!(&frame.body, Some(topic_frame::Body::Omission(omission)) if omission.sequence == u64::MAX)
        {
            return Err(FrameError::InvalidSequence);
        }
        if !frame.repair_target.is_empty()
            && (frame.repair_target.len() != 16 || !frame.is_publisher_output())
        {
            return Err(FrameError::InvalidIdentity);
        }
        if let Some(recipients) = &frame.recipient_connections {
            if !matches!(frame.body, Some(topic_frame::Body::Message(_))) {
                return Err(FrameError::InvalidIdentity);
            }
            let (ids, remainder) = recipients.as_chunks::<RECIPIENT_CONNECTION_BYTES>();
            if !remainder.is_empty()
                || ids.is_empty()
                || ids.len() > MAX_RECIPIENT_CONNECTIONS
                || ids.windows(2).any(|pair| matches!(pair, [a, b] if a >= b))
            {
                return Err(FrameError::InvalidIdentity);
            }
        }
        if frame
            .requester()
            .is_some_and(|id| !id.is_empty() && id.len() != 16)
        {
            return Err(FrameError::InvalidIdentity);
        }
        Ok(frame)
    }

    pub fn requester(&self) -> Option<&[u8]> {
        match self.body.as_ref()? {
            topic_frame::Body::Probe(probe) => Some(&probe.requester),
            topic_frame::Body::Nack(nack) => Some(&nack.requester),
            _ => None,
        }
    }

    pub fn publication_id(&self) -> Option<&str> {
        Some(match self.body.as_ref()? {
            topic_frame::Body::Message(value) => &value.publication_id,
            topic_frame::Body::Nack(value) => &value.publication_id,
            topic_frame::Body::Probe(value) => &value.publication_id,
            topic_frame::Body::Frontier(value) => &value.publication_id,
            topic_frame::Body::Floor(value) => &value.publication_id,
            topic_frame::Body::Omission(value) => &value.publication_id,
        })
    }

    pub fn is_publisher_output(&self) -> bool {
        matches!(
            self.body,
            Some(
                topic_frame::Body::Message(_)
                    | topic_frame::Body::Frontier(_)
                    | topic_frame::Body::Floor(_)
                    | topic_frame::Body::Omission(_)
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
            recipient_connections: None,
            repair_target: Default::default(),
            body: Some(topic_frame::Body::Message(TopicMessage {
                captured_at: Some(TopicCaptureTimestamp {
                    nanoseconds: i64::MIN.saturating_add(17),
                    uncertainty_nanoseconds: Some(u32::MAX),
                }),
                publication_id: "opaque-server-data-track".to_string(),
                sequence: u64::MAX.saturating_sub(1),
                payload: vec![42; MAX_PAYLOAD_BYTES],
            })),
        };
        let wire = frame.encode_to_vec();
        assert!(wire.len() > MAX_PAYLOAD_BYTES);
        assert_eq!(TopicFrame::decode_bounded(&wire), Ok(frame.clone()));
        assert_eq!(frame.publication_id(), Some("opaque-server-data-track"));
        assert!(frame.is_publisher_output());
        assert!(!frame.is_subscriber_control());
    }

    #[test]
    fn recipient_references_require_the_issued_canonical_format() {
        let id = data_recipient_id(&[0xaf; RECIPIENT_CONNECTION_BYTES]);
        assert_eq!(
            parse_data_recipient_id(&id),
            Some([0xaf; RECIPIENT_CONNECTION_BYTES])
        );
        assert_eq!(parse_data_recipient_id("alice"), None);
        assert_eq!(parse_data_recipient_id("pbr_a"), None);
        assert_eq!(parse_data_recipient_id(&id.replacen("af", "AF", 1)), None);
    }

    #[test]
    fn recipient_connections_are_bounded_and_unambiguous() {
        let mut frame = TopicFrame::from(topic_frame::Body::Message(TopicMessage {
            publication_id: "opaque".to_string(),
            ..Default::default()
        }));
        for invalid in [
            vec![],
            vec![1; 15],
            vec![1; 32],
            [vec![2; 16], vec![1; 16]].concat(),
        ] {
            frame.recipient_connections = Some(invalid);
            assert_eq!(
                TopicFrame::decode_bounded(&frame.encode_to_vec()),
                Err(FrameError::InvalidIdentity)
            );
        }
        let ids = (0..=MAX_RECIPIENT_CONNECTIONS)
            .flat_map(|i| [u8::try_from(i).unwrap(); RECIPIENT_CONNECTION_BYTES])
            .collect::<alloc::vec::Vec<_>>();
        frame.recipient_connections = Some(ids.clone());
        assert_eq!(
            TopicFrame::decode_bounded(&frame.encode_to_vec()),
            Err(FrameError::InvalidIdentity)
        );
        frame.recipient_connections = Some(ids[RECIPIENT_CONNECTION_BYTES..].to_vec());
        assert_eq!(
            TopicFrame::decode_bounded(&frame.encode_to_vec()),
            Ok(frame)
        );
    }

    #[test]
    fn capture_timestamp_preserves_present_zero_uncertainty() {
        let stamp = TopicCaptureTimestamp {
            nanoseconds: 0,
            uncertainty_nanoseconds: Some(0),
        };
        assert_eq!(
            TopicCaptureTimestamp::decode(stamp.encode_to_vec().as_slice()),
            Ok(stamp)
        );
    }

    #[test]
    fn controls_are_distinct_from_messages_and_input_is_bounded() {
        let probe = TopicFrame {
            recipient_connections: None,
            repair_target: Default::default(),
            body: Some(topic_frame::Body::Probe(TopicProbe {
                requester: Default::default(),
                publication_id: "opaque".to_string(),
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
                recipient_connections: None,
                repair_target: Default::default(),
                body: Some(topic_frame::Body::Message(TopicMessage {
                    captured_at: None,
                    publication_id: id.to_string(),
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
