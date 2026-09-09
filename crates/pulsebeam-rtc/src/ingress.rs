#![allow(
    clippy::disallowed_types,
    reason = "MediaPacket's reviewed immutable Bytes and Arc contract is constructed at ingress"
)]
#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "all indexes derive from bounded negotiated facts and mapper arithmetic deliberately uses RTP wrapping"
)]

use std::{collections::VecDeque, ops::Range, sync::Arc};

use bytes::Bytes;

use crate::{
    EncodingId, EncodingInfo, Event, GlobalMediaTime, MediaKind, MediaPacket, TimePoint,
    negotiation::IngressMediaFacts, packet::RtpPacket,
};

const MAX_RECORDS: usize = 256;
const REORDER_WINDOW: i64 = 2_048;

pub(crate) struct IngressOwner {
    media: Box<[MediaFacts]>,
    encodings: Vec<Encoding>,
    events: VecDeque<Event>,
    rtcp: VecDeque<TimedRecord>,
    max_unsignaled: usize,
    max_bytes: usize,
    queued_media_bytes: usize,
    queued_rtcp_bytes: usize,
    next_encoding: u32,
    dropped: u64,
}

struct MediaFacts {
    kind: MediaKind,
    mid: Box<str>,
    payloads: Box<[(u8, u32)]>,
    ssrcs: Box<[u32]>,
    rids: Box<[Box<str>]>,
    extensions: Box<[(u8, Box<str>)]>,
}

struct TimedRecord {
    _arrival: TimePoint,
    _bytes: Vec<u8>,
}

struct Encoding {
    id: EncodingId,
    media: usize,
    ssrc: Option<u32>,
    rid: Option<Box<str>>,
    unsignaled: bool,
    mapper: ProvisionalMapper,
}

struct ProvisionalMapper {
    first_global: GlobalMediaTime,
    first_timestamp: u32,
    frontier_sequence: i64,
    frontier_timestamp: i64,
    clock_rate: u32,
}

impl IngressOwner {
    pub(crate) fn new(
        media: Box<[IngressMediaFacts]>,
        max_unsignaled: u16,
        max_bytes: usize,
    ) -> Self {
        Self {
            media: media
                .into_vec()
                .into_iter()
                .map(|facts| MediaFacts {
                    kind: facts.kind,
                    mid: facts.mid,
                    payloads: facts.payloads,
                    ssrcs: facts.ssrcs,
                    rids: facts.rids,
                    extensions: facts.extensions,
                })
                .collect(),
            encodings: Vec::new(),
            events: VecDeque::new(),
            rtcp: VecDeque::new(),
            max_unsignaled: usize::from(max_unsignaled),
            max_bytes,
            queued_media_bytes: 0,
            queued_rtcp_bytes: 0,
            next_encoding: 1,
            dropped: 0,
        }
    }

    pub(crate) fn accept_rtp(&mut self, arrival: TimePoint, bytes: Vec<u8>) {
        if self.events.len() >= MAX_RECORDS
            || bytes.len() > self.max_bytes.saturating_sub(self.queued_media_bytes)
        {
            self.drop();
            return;
        }
        let Ok(packet) = RtpPacket::parse(&bytes) else {
            self.drop();
            return;
        };
        let Some((media, clock_rate, rid)) = self.resolve(&packet) else {
            self.drop();
            return;
        };
        let signaled = self.media[media].ssrcs.contains(&packet.ssrc())
            || rid.is_some_and(|value| {
                self.media[media]
                    .rids
                    .iter()
                    .any(|known| known.as_ref() == value)
            });
        let encoding = match self.find_encoding(media, packet.ssrc(), rid) {
            Some(index) => index,
            None => {
                let unsignaled = !signaled;
                if unsignaled
                    && self
                        .encodings
                        .iter()
                        .filter(|encoding| encoding.unsignaled)
                        .count()
                        >= self.max_unsignaled
                {
                    self.drop();
                    return;
                }
                if self.events.len() > MAX_RECORDS.saturating_sub(if unsignaled { 2 } else { 1 }) {
                    self.drop();
                    return;
                }
                let Some(id) = EncodingId::new(self.next_encoding) else {
                    self.drop();
                    return;
                };
                self.next_encoding = self.next_encoding.saturating_add(1);
                self.encodings.push(Encoding {
                    id,
                    media,
                    ssrc: Some(packet.ssrc()),
                    rid: rid.map(Box::<str>::from),
                    unsignaled,
                    mapper: ProvisionalMapper::new(
                        arrival.global,
                        packet.sequence(),
                        packet.timestamp(),
                        clock_rate,
                    ),
                });
                let index = self.encodings.len() - 1;
                if unsignaled {
                    self.events
                        .push_back(Event::EncodingDiscovered(EncodingInfo {
                            id,
                            kind: self.media[media].kind,
                        }));
                }
                index
            }
        };
        let (encoding, global_media_at) = {
            let encoding = &mut self.encodings[encoding];
            if encoding.ssrc.is_none() {
                encoding.ssrc = Some(packet.ssrc());
            }
            if encoding.rid.is_none() {
                encoding.rid = rid.map(Box::<str>::from);
            }
            let Some(global_media_at) = encoding.mapper.map(packet.sequence(), packet.timestamp())
            else {
                self.drop();
                return;
            };
            (encoding.id, global_media_at)
        };
        let extensions = self.extensions_for(media, &packet);
        let packet = MediaPacket::new(Bytes::from(bytes), global_media_at, extensions);
        self.queued_media_bytes = self.queued_media_bytes.saturating_add(packet.bytes().len());
        self.events.push_back(Event::Media { encoding, packet });
    }

    pub(crate) fn retain_rtcp(&mut self, arrival: TimePoint, bytes: Vec<u8>) {
        if self.rtcp.len() >= MAX_RECORDS
            || bytes.len() > self.max_bytes.saturating_sub(self.queued_rtcp_bytes)
        {
            self.drop();
            return;
        }
        self.queued_rtcp_bytes = self.queued_rtcp_bytes.saturating_add(bytes.len());
        self.rtcp.push_back(TimedRecord {
            _arrival: arrival,
            _bytes: bytes,
        });
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if let Event::Media { packet, .. } = &event {
            self.queued_media_bytes = self.queued_media_bytes.saturating_sub(packet.bytes().len());
        }
        Some(event)
    }

    fn resolve<'a>(&self, packet: &'a RtpPacket<'a>) -> Option<(usize, u32, Option<&'a str>)> {
        let mut candidates = self.media.iter().enumerate().filter_map(|(index, media)| {
            media
                .payloads
                .iter()
                .find(|(payload_type, _)| *payload_type == packet.payload_type())
                .map(|(_, clock_rate)| (index, *clock_rate))
        });
        let (first, clock_rate) = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        let media = &self.media[first];
        let mid = extension_value(
            packet,
            &media.extensions,
            "urn:ietf:params:rtp-hdrext:sdes:mid",
        );
        if mid.is_some_and(|value| value != media.mid.as_ref()) {
            return None;
        }
        let rid = extension_value(
            packet,
            &media.extensions,
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
        );
        Some((first, clock_rate, rid))
    }

    fn find_encoding(&self, media: usize, ssrc: u32, rid: Option<&str>) -> Option<usize> {
        self.encodings.iter().position(|encoding| {
            encoding.media == media
                && ((encoding.ssrc == Some(ssrc))
                    || rid.is_some_and(|rid| encoding.rid.as_deref() == Some(rid)))
                && !(encoding.ssrc.is_some_and(|known| known != ssrc)
                    && rid.is_some_and(|rid| encoding.rid.as_deref() != Some(rid)))
        })
    }

    fn extensions_for(
        &self,
        media: usize,
        packet: &RtpPacket<'_>,
    ) -> Arc<[(Arc<str>, Range<usize>)]> {
        let Ok(values) = packet.extensions() else {
            return Arc::from([]);
        };
        values
            .filter_map(|value| {
                self.media[media]
                    .extensions
                    .iter()
                    .find(|(id, _)| *id == value.id())
                    .map(|(_, uri)| (Arc::from(uri.as_ref()), value.range()))
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

fn extension_value<'a>(
    packet: &'a RtpPacket<'a>,
    extensions: &[(u8, Box<str>)],
    uri: &str,
) -> Option<&'a str> {
    let id = extensions
        .iter()
        .find(|(_, candidate)| candidate.as_ref() == uri)?
        .0;
    let value = packet
        .extensions()
        .ok()?
        .find(|value| value.id() == id)?
        .value();
    std::str::from_utf8(value)
        .ok()
        .filter(|value| !value.is_empty())
}

impl ProvisionalMapper {
    fn new(first_global: GlobalMediaTime, sequence: u16, timestamp: u32, clock_rate: u32) -> Self {
        Self {
            first_global,
            first_timestamp: timestamp,
            frontier_sequence: i64::from(sequence),
            frontier_timestamp: 0,
            clock_rate,
        }
    }

    fn map(&mut self, sequence: u16, timestamp: u32) -> Option<GlobalMediaTime> {
        let sequence = self.frontier_sequence
            + i64::from(i16::from_be_bytes(
                sequence
                    .wrapping_sub(self.frontier_sequence as u16)
                    .to_be_bytes(),
            ));
        if sequence < self.frontier_sequence - REORDER_WINDOW {
            return None;
        }
        let timestamp = self.frontier_timestamp
            + i64::from(i32::from_be_bytes(
                timestamp
                    .wrapping_sub(self.wrapped_frontier_timestamp())
                    .to_be_bytes(),
            ));
        if sequence > self.frontier_sequence {
            self.frontier_sequence = sequence;
            self.frontier_timestamp = timestamp;
        }
        let delta = i128::from(timestamp) * 1_000_000 / i128::from(self.clock_rate);
        let mapped = i128::from(self.first_global.as_micros()) + delta;
        u64::try_from(mapped).ok().map(GlobalMediaTime::from_micros)
    }

    fn wrapped_frontier_timestamp(&self) -> u32 {
        self.first_timestamp
            .wrapping_add(self.frontier_timestamp as u32)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn owner() -> IngressOwner {
        IngressOwner::new(
            vec![IngressMediaFacts {
                kind: MediaKind::Video,
                mid: "video".into(),
                payloads: vec![(96, 90_000)].into(),
                ssrcs: Box::new([]),
                rids: Box::new([]),
                extensions: vec![(1, "urn:test:extension".into())].into(),
            }]
            .into(),
            1,
            1024,
        )
    }

    fn rtp(sequence: u16, timestamp: u32) -> Vec<u8> {
        let mut bytes = vec![0x90, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
        bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
        bytes[4..8].copy_from_slice(&timestamp.to_be_bytes());
        bytes.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, 0xaa, 0, 0]);
        bytes.extend_from_slice(&[1, 2]);
        bytes
    }

    fn at(global: u64) -> TimePoint {
        TimePoint {
            monotonic: Instant::now(),
            global: GlobalMediaTime::from_micros(global),
        }
    }

    #[test]
    fn unsignaled_media_is_discovered_before_media_and_keeps_receive_anchor() {
        let mut owner = owner();
        owner.accept_rtp(at(1_000), rtp(u16::MAX, u32::MAX - 1));
        let Event::EncodingDiscovered(info) = owner.poll_event().expect("discovery") else {
            panic!("unsignaled encoding must be discovered first");
        };
        let Event::Media { encoding, packet } = owner.poll_event().expect("media") else {
            panic!("media follows discovery");
        };
        assert_eq!(encoding, info.id);
        assert_eq!(
            packet.global_media_at(),
            GlobalMediaTime::from_micros(1_000)
        );
        assert_eq!(packet.extension("urn:test:extension"), Some(&[0xaa][..]));
        assert_eq!(packet.to_transit().bytes(), packet.bytes());

        owner.accept_rtp(at(9_000_000), rtp(0, 1));
        let Event::Media { packet, .. } = owner.poll_event().expect("wrapped media") else {
            panic!("wrapped packet is media");
        };
        assert_eq!(
            packet.global_media_at(),
            GlobalMediaTime::from_micros(1_033)
        );
    }

    #[test]
    fn old_reordered_packets_map_earlier_without_moving_the_frontier() {
        let mut mapper =
            ProvisionalMapper::new(GlobalMediaTime::from_micros(1_000), 10, 90_000, 90_000);
        assert_eq!(
            mapper.map(11, 180_000),
            Some(GlobalMediaTime::from_micros(1_001_000))
        );
        assert_eq!(
            mapper.map(10, 90_000),
            Some(GlobalMediaTime::from_micros(1_000))
        );
        assert!(mapper.map(10_u16.wrapping_sub(2_049), 0).is_none());
    }
}
