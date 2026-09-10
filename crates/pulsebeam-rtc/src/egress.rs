#![allow(
    clippy::arithmetic_side_effects,
    reason = "RTP sequence and timestamp spaces intentionally wrap on the wire"
)]

use std::collections::{BTreeSet, VecDeque};

use sha2::{Digest, Sha256};

use crate::{
    CommandError, ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, GlobalMediaTime,
    MediaKind, SenderId, SenderPolicy,
    negotiation::EgressSenderFacts,
    packet::RtpPacket,
    transport::{Transport, TransportError},
};

const MAX_QUEUED_MEDIA_PACKETS: usize = 8_192;

pub(crate) struct MediaEgress {
    senders: Box<[Sender]>,
    queue: VecDeque<QueuedMedia>,
    queued_frames: BTreeSet<(SenderId, FrameId)>,
    queued_payload_bytes: usize,
    max_payload_bytes: usize,
    next_twcc: u64,
}

struct Sender {
    facts: EgressSenderFacts,
    _policy: SenderPolicy,
    ssrc: u32,
    next_sequence: u64,
    base_timestamp: u32,
    base_global: Option<GlobalMediaTime>,
    latest_global: Option<GlobalMediaTime>,
}

struct QueuedMedia {
    sender: usize,
    media: ForwardedMedia,
    payload_bytes: usize,
}

pub(crate) enum PrepareResult {
    Prepared,
    Blocked,
    Fatal,
}

impl MediaEgress {
    pub(crate) fn new(
        facts: Box<[EgressSenderFacts]>,
        randomness: &[u8; 32],
        max_payload_bytes: usize,
        audio_policy: SenderPolicy,
        video_policy: SenderPolicy,
    ) -> Self {
        let senders = facts
            .into_vec()
            .into_iter()
            .map(|facts| {
                let seed = sender_seed(randomness, facts.id.value());
                Sender {
                    _policy: match facts.kind {
                        MediaKind::Audio => audio_policy,
                        MediaKind::Video => video_policy,
                    },
                    ssrc: nonzero_u32([seed[0], seed[1], seed[2], seed[3]]),
                    next_sequence: u64::from(u16::from_be_bytes([seed[4], seed[5]])),
                    base_timestamp: u32::from_be_bytes([seed[6], seed[7], seed[8], seed[9]]),
                    base_global: None,
                    latest_global: None,
                    facts,
                }
            })
            .collect();
        let twcc_seed = sender_seed(randomness, u16::MAX);
        Self {
            senders,
            queue: VecDeque::new(),
            queued_frames: BTreeSet::new(),
            queued_payload_bytes: 0,
            max_payload_bytes,
            next_twcc: u64::from(u16::from_be_bytes([twcc_seed[0], twcc_seed[1]])),
        }
    }

    pub(crate) fn admit(
        &mut self,
        sender: SenderId,
        media: ForwardedMedia,
    ) -> Result<(), CommandError> {
        let sender_index = self
            .senders
            .iter()
            .position(|candidate| candidate.facts.id == sender)
            .ok_or(CommandError::UnknownSender(sender))?;
        if media.frame.boundary != FrameBoundary::Complete
            || matches!(
                &media.frame.dependencies,
                FrameDependencies::Known(dependencies)
                    if dependencies.len() > FrameDependencies::MAX_DIRECT_DEPENDENCIES
                        || dependencies.contains(&media.frame.id)
            )
            || self.queued_frames.contains(&(sender, media.frame.id))
        {
            return Err(CommandError::InvalidFrameMetadata);
        }
        let packet = RtpPacket::parse(media.packet.bytes())
            .map_err(|_| CommandError::InvalidFrameMetadata)?;
        let payload_bytes = packet.payload().len();
        let global = media.packet.global_media_at();
        let Some(sender_state) = self.senders.get(sender_index) else {
            return Err(CommandError::UnknownSender(sender));
        };
        if sender_state
            .latest_global
            .is_some_and(|latest| global < latest)
        {
            return Err(CommandError::InvalidFrameMetadata);
        }
        if self.queue.len() >= MAX_QUEUED_MEDIA_PACKETS
            || payload_bytes
                > self
                    .max_payload_bytes
                    .saturating_sub(self.queued_payload_bytes)
        {
            return Err(CommandError::WouldBlock);
        }
        let Some(sender_state) = self.senders.get_mut(sender_index) else {
            return Err(CommandError::UnknownSender(sender));
        };
        sender_state.latest_global = Some(global);
        self.queued_payload_bytes = self.queued_payload_bytes.saturating_add(payload_bytes);
        self.queued_frames.insert((sender, media.frame.id));
        self.queue.push_back(QueuedMedia {
            sender: sender_index,
            media,
            payload_bytes,
        });
        Ok(())
    }

    pub(crate) fn prepare_one(&mut self, transport: &mut Transport) -> PrepareResult {
        let Some(queued) = self.queue.front() else {
            return PrepareResult::Blocked;
        };
        let Some(sender) = self.senders.get_mut(queued.sender) else {
            return PrepareResult::Fatal;
        };
        let Ok(source) = RtpPacket::parse(queued.media.packet.bytes()) else {
            return PrepareResult::Fatal;
        };
        let global = queued.media.packet.global_media_at();
        let base_global = *sender.base_global.get_or_insert(global);
        let Some(delta) = global.as_micros().checked_sub(base_global.as_micros()) else {
            return PrepareResult::Fatal;
        };
        let ticks = u128::from(delta)
            .saturating_mul(u128::from(sender.facts.clock_rate))
            .saturating_add(500_000)
            / 1_000_000;
        let timestamp_offset =
            u32::try_from(ticks % (u128::from(u32::MAX) + 1)).unwrap_or_default();
        let timestamp = sender.base_timestamp.wrapping_add(timestamp_offset);
        let sequence = u16::try_from(sender.next_sequence % 65_536).unwrap_or_default();
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| u16::try_from(self.next_twcc % 65_536).unwrap_or_default());
        let Some(packet) = build_rtp(
            sender,
            source.marker(),
            sequence,
            timestamp,
            twcc,
            source.payload(),
        ) else {
            return PrepareResult::Fatal;
        };

        sender.next_sequence = match sender.next_sequence.checked_add(1) {
            Some(value) => value,
            None => return PrepareResult::Fatal,
        };
        if twcc.is_some() {
            self.next_twcc = match self.next_twcc.checked_add(1) {
                Some(value) => value,
                None => return PrepareResult::Fatal,
            };
        }
        match transport.send_rtp(&packet) {
            Ok(()) => {
                let Some(queued) = self.queue.pop_front() else {
                    return PrepareResult::Fatal;
                };
                let Some(sender) = self.senders.get(queued.sender) else {
                    return PrepareResult::Fatal;
                };
                self.queued_frames
                    .remove(&(sender.facts.id, queued.media.frame.id));
                self.queued_payload_bytes = self
                    .queued_payload_bytes
                    .saturating_sub(queued.payload_bytes);
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => PrepareResult::Blocked,
            Err(
                TransportError::Closed
                | TransportError::InvalidInput
                | TransportError::Configuration
                | TransportError::Crypto
                | TransportError::Timeout
                | TransportError::NotDue,
            ) => PrepareResult::Fatal,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::items_after_test_module,
    reason = "crate-private tests construct the reviewed immutable public packet values"
)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::*;
    use crate::{FrameMetadata, MediaPacket};

    #[test]
    fn admission_enforces_exact_packet_and_payload_byte_bounds() {
        let mut bytes = owner(2);
        assert_eq!(bytes.admit(sender(), media(1, b"a")), Ok(()));
        assert_eq!(bytes.admit(sender(), media(2, b"b")), Ok(()));
        assert_eq!(
            bytes.admit(sender(), media(3, b"c")),
            Err(CommandError::WouldBlock)
        );

        let mut packets = owner(usize::MAX);
        for id in 1..=MAX_QUEUED_MEDIA_PACKETS {
            assert_eq!(packets.admit(sender(), media(id as u64, b"")), Ok(()));
        }
        assert_eq!(
            packets.admit(sender(), media(9_000, b"")),
            Err(CommandError::WouldBlock)
        );
    }

    #[test]
    fn admission_rejects_unknown_senders_and_invalid_complete_frame_metadata() {
        let mut owner = owner(100);
        assert_eq!(
            owner.admit(SenderId::new(2).expect("nonzero"), media(1, b"a")),
            Err(CommandError::UnknownSender(
                SenderId::new(2).expect("nonzero")
            ))
        );
        let mut incomplete = media(1, b"a");
        incomplete.frame.boundary = FrameBoundary::Start;
        assert_eq!(
            owner.admit(sender(), incomplete),
            Err(CommandError::InvalidFrameMetadata)
        );
        let mut cyclic = media(2, b"a");
        cyclic.frame.dependencies = FrameDependencies::Known(Arc::from([FrameId::from_value(2)]));
        assert_eq!(
            owner.admit(sender(), cyclic),
            Err(CommandError::InvalidFrameMetadata)
        );
    }

    fn owner(max_payload_bytes: usize) -> MediaEgress {
        MediaEgress::new(
            vec![EgressSenderFacts {
                id: sender(),
                kind: MediaKind::Audio,
                mid: "audio".into(),
                payload_type: 111,
                clock_rate: 48_000,
                mid_extension_id: Some(9),
                twcc_extension_id: Some(3),
            }]
            .into_boxed_slice(),
            &[7; 32],
            max_payload_bytes,
            crate::ConnectionConfig::default().default_audio_policy,
            crate::ConnectionConfig::default().default_video_policy,
        )
    }

    fn sender() -> SenderId {
        SenderId::new(1).expect("nonzero")
    }

    fn media(id: u64, payload: &'static [u8]) -> ForwardedMedia {
        let mut bytes = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7];
        bytes.extend_from_slice(payload);
        ForwardedMedia {
            packet: MediaPacket::new(
                Bytes::from(bytes),
                GlobalMediaTime::from_micros(id),
                Arc::from([]),
            ),
            frame: FrameMetadata {
                id: FrameId::from_value(id),
                boundary: FrameBoundary::Complete,
                random_access: true,
                discardable: false,
                dependencies: FrameDependencies::Known(Arc::from([])),
            },
        }
    }
}

fn sender_seed(randomness: &[u8; 32], sender: u16) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"pulsebeam rtc outbound sender v1");
    hash.update(randomness);
    hash.update(sender.to_be_bytes());
    hash.finalize().into()
}

fn nonzero_u32(bytes: [u8; 4]) -> u32 {
    let value = u32::from_be_bytes(bytes);
    if value == 0 { 1 } else { value }
}

fn build_rtp(
    sender: &Sender,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    twcc: Option<u16>,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let mut extensions = Vec::with_capacity(2);
    if let Some(id) = sender.facts.mid_extension_id {
        extensions.push((id, sender.facts.mid.as_bytes().to_vec()));
    }
    if let (Some(id), Some(sequence)) = (sender.facts.twcc_extension_id, twcc) {
        extensions.push((id, sequence.to_be_bytes().to_vec()));
    }
    let mut packet = Vec::with_capacity(20usize.saturating_add(payload.len()));
    packet.push(0x80 | if extensions.is_empty() { 0 } else { 0x10 });
    packet.push(sender.facts.payload_type | if marker { 0x80 } else { 0 });
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&sender.ssrc.to_be_bytes());
    if !extensions.is_empty() && !write_extensions(&mut packet, &extensions) {
        return None;
    }
    packet.extend_from_slice(payload);
    Some(packet)
}

fn write_extensions(packet: &mut Vec<u8>, extensions: &[(u8, Vec<u8>)]) -> bool {
    let one_byte = extensions
        .iter()
        .all(|(id, value)| (1..=14).contains(id) && (1..=16).contains(&value.len()));
    packet.extend_from_slice(if one_byte {
        &[0xbe, 0xde]
    } else {
        &[0x10, 0x00]
    });
    let length_offset = packet.len();
    packet.extend_from_slice(&[0, 0]);
    let start = packet.len();
    for (id, value) in extensions {
        if one_byte {
            packet.push((*id << 4) | u8::try_from(value.len() - 1).unwrap_or_default());
        } else {
            let Ok(length) = u8::try_from(value.len()) else {
                return false;
            };
            packet.extend_from_slice(&[*id, length]);
        }
        packet.extend_from_slice(value);
    }
    while !(packet.len() - start).is_multiple_of(4) {
        packet.push(0);
    }
    let words = u16::try_from((packet.len() - start) / 4).unwrap_or(u16::MAX);
    if let Some(length) = packet.get_mut(length_offset..length_offset.saturating_add(2)) {
        length.copy_from_slice(&words.to_be_bytes());
    }
    true
}
