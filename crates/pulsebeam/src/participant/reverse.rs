use crate::media::{KeyframeRequestKind, Rid};
use crate::track::SourceEncoding;
use sha3::{Digest, Sha3_256};
use std::time::Duration;

const KEYFRAME: u8 = 1;
const RELIABLE_CONTROL: u8 = 2;
const SCOPED_RELIABLE_CONTROL: u8 = 3;
const PLI: u8 = 1;
const FIR: u8 = 2;
const RID_ABSENT: u8 = 0;
const RID_PRESENT: u8 = 1;
const SSRC_PRESENT: u8 = 2;

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

#[derive(Debug, Clone)]
pub(crate) struct ReversePacket {
    #[allow(
        clippy::disallowed_types,
        reason = "RFC254 reverse envelopes are immutable snapshots shared by local Link fanout"
    )]
    payload: bytes::Bytes,
    dedup: ReverseDedup,
}

pub(crate) enum ReverseInput {
    Keyframe {
        encoding: SourceEncoding,
        kind: KeyframeRequestKind,
    },
    ReliableControl {
        requester: Option<crate::entity::ConnectionId>,
        bytes: Vec<u8>,
    },
}

impl ReversePacket {
    pub(crate) fn keyframe(encoding: impl Into<SourceEncoding>, kind: KeyframeRequestKind) -> Self {
        let mut payload = Vec::with_capacity(11);
        payload.push(KEYFRAME);
        payload.push(match kind {
            KeyframeRequestKind::Pli => PLI,
            KeyframeRequestKind::Fir => FIR,
        });
        match encoding.into() {
            SourceEncoding::Rid(Some(rid)) => {
                let bytes = rid.as_bytes();
                let Ok(len) = u8::try_from(bytes.len()) else {
                    debug_assert!(false, "a RID must fit in its reverse envelope");
                    return Self::new(payload, KEYFRAME_DEDUP_WINDOW);
                };
                payload.push(RID_PRESENT);
                payload.push(len);
                payload.extend_from_slice(bytes);
            }
            SourceEncoding::Rid(None) => payload.push(RID_ABSENT),
            SourceEncoding::Ssrc(ssrc) => {
                payload.push(SSRC_PRESENT);
                payload.extend_from_slice(&u32::from(ssrc).to_be_bytes());
            }
        }
        Self::new(payload, KEYFRAME_DEDUP_WINDOW)
    }

    pub(crate) fn reliable_control(mut control: Vec<u8>) -> Self {
        let mut payload = Vec::with_capacity(control.len().saturating_add(1));
        payload.push(RELIABLE_CONTROL);
        payload.append(&mut control);
        Self::new(payload, RELIABLE_CONTROL_DEDUP_WINDOW)
    }

    pub(crate) fn with_requester(self, connection: crate::entity::ConnectionId) -> Self {
        let Some((&RELIABLE_CONTROL, control)) = self.payload.split_first() else {
            return self;
        };
        let mut payload = Vec::with_capacity(control.len().saturating_add(17));
        payload.push(SCOPED_RELIABLE_CONTROL);
        payload.extend_from_slice(connection.as_bytes());
        payload.extend_from_slice(control);
        Self::new(payload, RELIABLE_CONTROL_DEDUP_WINDOW)
    }

    fn scoped_control(body: &[u8]) -> Option<(crate::entity::ConnectionId, &[u8])> {
        let (identity, control) = body.split_first_chunk::<16>()?;
        (!control.is_empty()).then(|| (crate::entity::ConnectionId::from_bytes(*identity), control))
    }

    fn new(payload: Vec<u8>, window: Duration) -> Self {
        debug_assert!(!payload.is_empty(), "reverse envelopes carry a type tag");
        debug_assert!(!window.is_zero(), "reverse deduplication must expire");
        let key = Sha3_256::digest(&payload).into();
        Self {
            payload: payload.into(),
            dedup: ReverseDedup { key, window },
        }
    }

    pub(crate) fn wire(&self) -> &[u8] {
        &self.payload
    }

    pub(crate) fn from_wire(wire: &[u8]) -> Option<Self> {
        let (&tag, body) = wire.split_first()?;
        let window = match tag {
            KEYFRAME => {
                Self::decode_keyframe(body)?;
                KEYFRAME_DEDUP_WINDOW
            }
            RELIABLE_CONTROL => RELIABLE_CONTROL_DEDUP_WINDOW,
            SCOPED_RELIABLE_CONTROL => {
                Self::scoped_control(body)?;
                RELIABLE_CONTROL_DEDUP_WINDOW
            }
            _ => return None,
        };
        Some(Self::new(wire.to_vec(), window))
    }

    pub(crate) fn dedup(&self) -> ReverseDedup {
        self.dedup
    }

    pub(crate) fn decode(self) -> Option<ReverseInput> {
        let (&tag, body) = self.payload.split_first()?;
        match tag {
            KEYFRAME => Self::decode_keyframe(body),
            RELIABLE_CONTROL => Some(ReverseInput::ReliableControl {
                requester: None,
                bytes: body.to_vec(),
            }),
            SCOPED_RELIABLE_CONTROL => {
                let (requester, control) = Self::scoped_control(body)?;
                Some(ReverseInput::ReliableControl {
                    requester: Some(requester),
                    bytes: control.to_vec(),
                })
            }
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
        let encoding = match rid_state {
            RID_ABSENT if body.is_empty() => SourceEncoding::Rid(None),
            SSRC_PRESENT => SourceEncoding::Ssrc(u32::from_be_bytes(body.try_into().ok()?).into()),
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
                SourceEncoding::Rid(Some(Rid::new(std::str::from_utf8(bytes).ok()?).ok()?))
            }
            _ => return None,
        };
        Some(ReverseInput::Keyframe { encoding, kind })
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
                encoding: SourceEncoding::Rid(Some(rid)),
                kind: KeyframeRequestKind::Fir,
            }) if rid == Rid::from("f")
        ));
        let ridless = ReversePacket::keyframe(None, KeyframeRequestKind::Pli)
            .dedup()
            .key();
        for ssrc in [0, u32::MAX] {
            let encoding = SourceEncoding::Ssrc(ssrc.into());
            let packet = ReversePacket::keyframe(encoding, KeyframeRequestKind::Pli);
            assert_ne!(packet.dedup().key(), ridless);
            assert!(
                matches!(packet.decode(), Some(ReverseInput::Keyframe { encoding: actual, kind: KeyframeRequestKind::Pli }) if actual == encoding)
            );
            let mut packet = ReversePacket::keyframe(encoding, KeyframeRequestKind::Pli);
            let mut extended = packet.payload.to_vec();
            extended.push(0);
            packet.payload = extended.into();
            assert!(packet.decode().is_none());
            let mut packet = ReversePacket::keyframe(encoding, KeyframeRequestKind::Pli);
            packet
                .payload
                .truncate(packet.payload.len().saturating_sub(1));
            assert!(packet.decode().is_none());
        }
    }

    #[test]
    fn reliable_control_dedup_survives_link_without_merging_receivers() {
        let first = crate::entity::ConnectionId::new();
        let second = crate::entity::ConnectionId::new();
        let request = ReversePacket::reliable_control(vec![4, 5]);
        let a = request.clone().with_requester(first);
        let b = request.clone().with_requester(second);
        assert_eq!(a.dedup(), request.with_requester(first).dedup());
        assert_ne!(a.dedup(), b.dedup());
        let round_trip = ReversePacket::from_wire(a.wire()).unwrap();
        assert_eq!(round_trip.dedup(), a.dedup());
        let body = round_trip.wire().split_first().unwrap().1;
        assert_eq!(
            ReversePacket::scoped_control(body),
            Some((first, &[4, 5][..]))
        );
        assert!(
            matches!(round_trip.decode(), Some(ReverseInput::ReliableControl { requester: Some(requester), bytes }) if requester == first && bytes == vec![4, 5])
        );
        assert!(ReversePacket::from_wire(&[SCOPED_RELIABLE_CONTROL, 0]).is_none());
    }

    #[test]
    fn reliable_control_is_opaque_until_the_endpoint_decodes_it() {
        let packet = ReversePacket::reliable_control(vec![4, 5]);
        assert!(matches!(
            packet.decode(),
            Some(ReverseInput::ReliableControl { requester: None, bytes }) if bytes == vec![4, 5]
        ));
    }
}
