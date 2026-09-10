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
    ConnectionWarning, EncodingId, EncodingInfo, EncodingRetireReason, Event, MediaKind,
    MediaPacket, PacketFeedbackKind, TimePoint,
    clock::{ClockMapper, ClockWarning, ntp_micros},
    negotiation::IngressMediaFacts,
    packet::RtpPacket,
    rtcp::{self, FeedbackBatch, LifecycleFact},
    transport::PathEpoch,
};

const MAX_RECORDS: usize = 256;

pub(crate) struct IngressOwner {
    media: Box<[MediaFacts]>,
    encodings: Vec<Encoding>,
    events: VecDeque<Event>,
    feedback: VecDeque<FeedbackBatch>,
    cname_by_ssrc: Vec<(u32, Box<str>)>,
    groups: Vec<(Box<str>, i128)>,
    max_unsignaled: usize,
    max_bytes: usize,
    queued_media_bytes: usize,
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

struct Encoding {
    id: EncodingId,
    media: usize,
    ssrc: Option<u32>,
    rid: Option<Box<str>>,
    unsignaled: bool,
    retired: bool,
    mapper: ClockMapper,
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
            feedback: VecDeque::new(),
            cname_by_ssrc: Vec::new(),
            groups: Vec::new(),
            max_unsignaled: usize::from(max_unsignaled),
            max_bytes,
            queued_media_bytes: 0,
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
                    retired: false,
                    mapper: ClockMapper::new(
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
        let (encoding, global_media_at, warning) = {
            let encoding = &mut self.encodings[encoding];
            if encoding.ssrc.is_none() {
                encoding.ssrc = Some(packet.ssrc());
            }
            if encoding.rid.is_none() {
                encoding.rid = rid.map(Box::<str>::from);
            }
            if encoding.retired {
                self.drop();
                return;
            }
            let Some((global_media_at, warning)) =
                encoding
                    .mapper
                    .map(packet.sequence(), packet.timestamp(), arrival.monotonic)
            else {
                self.drop();
                return;
            };
            (encoding.id, global_media_at, warning)
        };
        if let Some(warning) = warning {
            self.push_clock_warning(warning);
        }
        let extensions = self.extensions_for(media, &packet);
        let packet = MediaPacket::new(Bytes::from(bytes), global_media_at, extensions);
        self.queued_media_bytes = self.queued_media_bytes.saturating_add(packet.bytes().len());
        self.events.push_back(Event::Media { encoding, packet });
    }

    pub(crate) fn accept_rtcp(
        &mut self,
        arrival: TimePoint,
        bytes: Vec<u8>,
        path_epoch: PathEpoch,
        mode: PacketFeedbackKind,
        smoothed_rtt: Option<std::time::Duration>,
    ) {
        if bytes.len() > self.max_bytes {
            self.drop();
            return;
        }
        let Ok(parsed) = rtcp::parse(&bytes, arrival, path_epoch, mode) else {
            self.drop();
            return;
        };
        if parsed.lifecycle.len() > MAX_RECORDS.saturating_sub(self.events.len())
            || parsed.feedback.len() > MAX_RECORDS.saturating_sub(self.feedback.len())
        {
            self.drop();
            return;
        }
        for fact in parsed.lifecycle {
            self.apply_rtcp_fact(arrival, smoothed_rtt, fact);
        }
        self.feedback.extend(parsed.feedback);
    }

    #[allow(
        dead_code,
        reason = "Plan 08 consumes authenticated, mode-filtered feedback through this owner seam"
    )]
    pub(crate) fn poll_feedback(&mut self) -> Option<FeedbackBatch> {
        self.feedback.pop_front()
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

    fn apply_rtcp_fact(
        &mut self,
        arrival: TimePoint,
        smoothed_rtt: Option<std::time::Duration>,
        fact: LifecycleFact,
    ) {
        match fact {
            LifecycleFact::Cname { ssrc, cname } => {
                if let Some((_, known)) = self
                    .cname_by_ssrc
                    .iter_mut()
                    .find(|(known, _)| *known == ssrc)
                {
                    *known = cname;
                } else if self.cname_by_ssrc.len() < MAX_RECORDS {
                    self.cname_by_ssrc.push((ssrc, cname));
                }
            }
            LifecycleFact::Bye { ssrc } => {
                if let Some(encoding) = self
                    .encodings
                    .iter_mut()
                    .find(|encoding| encoding.ssrc == Some(ssrc))
                    && !encoding.retired
                {
                    encoding.retired = true;
                    self.events.push_back(Event::EncodingRetired {
                        encoding: encoding.id,
                        reason: EncodingRetireReason::RemoteBye,
                    });
                }
            }
            LifecycleFact::SenderReport {
                ssrc,
                ntp_seconds,
                ntp_fraction,
                rtp_timestamp,
            } => {
                let Some(ntp) = ntp_micros(ntp_seconds, ntp_fraction) else {
                    self.drop();
                    return;
                };
                let Some(index) = self
                    .encodings
                    .iter()
                    .position(|encoding| encoding.ssrc == Some(ssrc) && !encoding.retired)
                else {
                    return;
                };
                let cname = self
                    .cname_by_ssrc
                    .iter()
                    .find(|(known, _)| *known == ssrc)
                    .map(|(_, cname)| cname.clone());
                let relation = self.encodings[index].mapper.relation_at(rtp_timestamp, ntp);
                let group_offset = cname
                    .as_ref()
                    .and_then(|cname| {
                        self.groups
                            .iter()
                            .find(|(known, _)| known == cname)
                            .map(|(_, offset)| *offset)
                    })
                    .or(relation);
                let Some(group_offset) = group_offset else {
                    return;
                };
                if let Some(cname) = cname
                    && !self.groups.iter().any(|(known, _)| known == &cname)
                    && self.groups.len() < MAX_RECORDS
                {
                    self.groups.push((cname, group_offset));
                }
                if let Some(warning) = self.encodings[index].mapper.observe_sender_report(
                    rtp_timestamp,
                    ntp,
                    arrival.monotonic,
                    smoothed_rtt,
                    group_offset,
                ) {
                    self.push_clock_warning(warning);
                }
            }
        }
    }

    fn push_clock_warning(&mut self, warning: ClockWarning) {
        let warning = match warning {
            ClockWarning::Synchronized => ConnectionWarning::MediaClockSynchronized,
            ClockWarning::Stale => ConnectionWarning::MediaClockStale,
            ClockWarning::Discontinuous => ConnectionWarning::MediaClockDiscontinuous,
        };
        if !self
            .events
            .iter()
            .any(|event| matches!(event, Event::Warning(known) if *known == warning))
        {
            self.events.push_back(Event::Warning(warning));
        }
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::GlobalMediaTime;

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
        let mut mapper = ClockMapper::new(GlobalMediaTime::from_micros(1_000), 10, 90_000, 90_000);
        assert_eq!(
            mapper
                .map(11, 180_000, Instant::now())
                .map(|(time, _)| time),
            Some(GlobalMediaTime::from_micros(1_001_000))
        );
        assert_eq!(
            mapper.map(10, 90_000, Instant::now()).map(|(time, _)| time),
            Some(GlobalMediaTime::from_micros(1_000))
        );
        assert!(
            mapper
                .map(10_u16.wrapping_sub(2_049), 0, Instant::now())
                .is_none()
        );
    }
}
