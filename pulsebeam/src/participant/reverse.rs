use crate::rtp::{EncodingId as Rid, KeyframeRequestKind};
use sha3::{Digest, Sha3_256};
use std::time::Duration;

const KEYFRAME: u8 = 1;
const RELIABLE_CONTROL: u8 = 2;
const PLI: u8 = 1;
const FIR: u8 = 2;
const RID_ABSENT: u8 = 0;
const RID_PRESENT: u8 = 1;

const KEYFRAME_DEDUP_WINDOW: Duration = crate::track::KEYFRAME_DEBOUNCE;
const RELIABLE_CONTROL_DEDUP_WINDOW: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReverseDedup {
    key: [u8; 32],
    window: Duration,
}

impl ReverseDedup {
    pub(crate) fn key(self) -> [u8; 32] {
        self.key
    }

    pub(crate) fn window(self) -> Duration {
        self.window
    }
}

#[derive(Debug)]
pub(crate) struct ReversePacket {
    payload: Vec<u8>,
    dedup: ReverseDedup,
}

pub(crate) enum ReverseInput {
    Keyframe {
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    },
    ReliableControl(Vec<u8>),
}

impl ReversePacket {
    pub(crate) fn keyframe(rid: Option<Rid>, kind: KeyframeRequestKind) -> Self {
        let mut payload = Vec::with_capacity(11);
        payload.push(KEYFRAME);
        payload.push(match kind {
            KeyframeRequestKind::Pli => PLI,
            KeyframeRequestKind::Fir => FIR,
        });
        match rid {
            Some(rid) => {
                let bytes = rid.as_bytes();
                let Ok(len) = u8::try_from(bytes.len()) else {
                    debug_assert!(false, "a RID must fit in its reverse envelope");
                    return Self::new(payload, KEYFRAME_DEDUP_WINDOW);
                };
                payload.push(RID_PRESENT);
                payload.push(len);
                payload.extend_from_slice(bytes);
            }
            None => payload.push(RID_ABSENT),
        }
        Self::new(payload, KEYFRAME_DEDUP_WINDOW)
    }

    pub(crate) fn reliable_control(mut control: Vec<u8>) -> Self {
        let mut payload = Vec::with_capacity(control.len().saturating_add(1));
        payload.push(RELIABLE_CONTROL);
        payload.append(&mut control);
        Self::new(payload, RELIABLE_CONTROL_DEDUP_WINDOW)
    }

    fn new(payload: Vec<u8>, window: Duration) -> Self {
        debug_assert!(!payload.is_empty(), "reverse envelopes carry a type tag");
        debug_assert!(!window.is_zero(), "reverse deduplication must expire");
        let key = Sha3_256::digest(&payload).into();
        Self {
            payload,
            dedup: ReverseDedup { key, window },
        }
    }

    pub(crate) fn dedup(&self) -> ReverseDedup {
        self.dedup
    }

    pub(crate) fn decode(self) -> Option<ReverseInput> {
        let (&tag, body) = self.payload.split_first()?;
        match tag {
            KEYFRAME => Self::decode_keyframe(body),
            RELIABLE_CONTROL => Some(ReverseInput::ReliableControl(body.to_vec())),
            _ => None,
        }
    }

    fn decode_keyframe(body: &[u8]) -> Option<ReverseInput> {
        let (&kind, body) = body.split_first()?;
        let kind = match kind {
            PLI => KeyframeRequestKind::Pli,
            FIR => KeyframeRequestKind::Fir,
            _ => return None,
        };
        let (&rid_state, body) = body.split_first()?;
        let rid = match rid_state {
            RID_ABSENT if body.is_empty() => None,
            RID_PRESENT => {
                let (&len, body) = body.split_first()?;
                let len = usize::from(len);
                let bytes = body.get(..len)?;
                if body
                    .get(len..)
                    .is_none_or(|remaining| !remaining.is_empty())
                {
                    return None;
                }
                Some(Rid::from(std::str::from_utf8(bytes).ok()?))
            }
            _ => return None,
        };
        Some(ReverseInput::Keyframe { rid, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_envelope_round_trips_its_endpoint_metadata() {
        let packet = ReversePacket::keyframe(Some(Rid::from("f")), KeyframeRequestKind::Fir);
        assert!(matches!(
            packet.decode(),
            Some(ReverseInput::Keyframe {
                rid: Some(rid),
                kind: KeyframeRequestKind::Fir,
            }) if rid == Rid::from("f")
        ));
    }

    #[test]
    fn reliable_control_is_opaque_until_the_endpoint_decodes_it() {
        let packet = ReversePacket::reliable_control(vec![4, 5]);
        assert!(matches!(
            packet.decode(),
            Some(ReverseInput::ReliableControl(bytes)) if bytes == vec![4, 5]
        ));
    }
}
