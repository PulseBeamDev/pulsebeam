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

use std::{collections::VecDeque, ops::Range, sync::Arc, time::Instant};

use bytes::Bytes;

use crate::{
    ConnectionWarning, EcnCodepoint, EncodingId, EncodingInfo, EncodingRetireReason, EncodingStats,
    Event, MediaPacket, PacketFeedbackKind, TimePoint,
    clock::{ClockMapper, ClockWarning, ntp_micros},
    negotiation::IngressMediaFacts,
    packet::RtpPacket,
    receive_feedback::ReceiveFeedback,
    receive_stream::ReceiveStream,
    rtcp::{self, FeedbackBatch, LifecycleFact, RepairRequest},
    transport::PathEpoch,
};

const MAX_RECORDS: usize = 256;

pub(crate) struct IngressOwner {
    media: Box<[IngressMediaFacts]>,
    receiver_ssrc: u32,
    receive_feedback: ReceiveFeedback,
    control_cursor: usize,
    encodings: Vec<Encoding>,
    events: VecDeque<Event>,
    feedback: VecDeque<FeedbackBatch>,
    repair: VecDeque<RepairRequest>,
    keyframes: VecDeque<rtcp::KeyframeRequest>,
    cname_by_ssrc: Vec<(u32, Box<str>)>,
    groups: Vec<(Box<str>, i128)>,
    max_unsignaled: usize,
    max_bytes: usize,
    queued_media_bytes: usize,
    next_encoding: u32,
    dropped: u64,
}

struct Encoding {
    id: EncodingId,
    media: usize,
    ssrc: Option<u32>,
    repair_ssrc: Option<u32>,
    rid: Option<Box<str>>,
    unsignaled: bool,
    retired: bool,
    receiver: ReceiveStream,
    mapper: ClockMapper,
}

impl IngressOwner {
    pub(crate) fn new(
        media: Box<[IngressMediaFacts]>,
        max_unsignaled: u16,
        max_bytes: usize,
        receiver_ssrc: u32,
        receive_feedback: ReceiveFeedback,
    ) -> Self {
        Self {
            media,
            receiver_ssrc,
            receive_feedback,
            control_cursor: 0,
            encodings: Vec::new(),
            events: VecDeque::new(),
            feedback: VecDeque::new(),
            repair: VecDeque::new(),
            keyframes: VecDeque::new(),
            cname_by_ssrc: Vec::new(),
            groups: Vec::new(),
            max_unsignaled: usize::from(max_unsignaled),
            max_bytes,
            queued_media_bytes: 0,
            next_encoding: 1,
            dropped: 0,
        }
    }

    pub(crate) fn accept_rtp(
        &mut self,
        arrival: TimePoint,
        bytes: Vec<u8>,
        ecn: Option<EcnCodepoint>,
    ) {
        self.accept_packet(arrival, bytes, false, ecn);
    }

    fn accept_packet(
        &mut self,
        arrival: TimePoint,
        bytes: Vec<u8>,
        repair: bool,
        ecn: Option<EcnCodepoint>,
    ) {
        let Ok(packet) = RtpPacket::parse(&bytes) else {
            self.drop();
            return;
        };
        let Some((media, clock_rate, rid)) = self.resolve(&packet) else {
            self.drop();
            return;
        };
        let facts = &self.media[media];
        let primary_type = facts
            .repairs
            .iter()
            .find(|(pt, _)| *pt == packet.payload_type())
            .map_or_else(|| packet.payload_type(), |(_, primary)| *primary);
        if !repair && facts.feedback_payloads.contains(&primary_type) {
            let twcc = facts
                .extensions
                .iter()
                .find(|(_, uri)| crate::negotiation::TWCC_URIS.contains(&uri.as_ref()))
                .and_then(|(id, _)| packet.extensions().ok()?.find(|value| value.id() == *id))
                .and_then(|value| <[u8; 2]>::try_from(value.value()).ok())
                .map(u16::from_be_bytes);
            self.receive_feedback.observe(
                arrival.monotonic,
                packet.ssrc(),
                packet.sequence(),
                twcc,
                ecn,
            );
        }
        if self.events.len() >= MAX_RECORDS
            || bytes.len() > self.max_bytes.saturating_sub(self.queued_media_bytes)
        {
            self.drop();
            return;
        }
        if let Some((_, primary_type)) = self.media[media]
            .repairs
            .iter()
            .find(|(pt, _)| *pt == packet.payload_type())
        {
            let primary_ssrc = self.media[media]
                .ssrc_repairs
                .iter()
                .find(|(rtx, _)| *rtx == packet.ssrc())
                .map(|(_, primary)| *primary)
                .or_else(|| {
                    rid.and_then(|rid| {
                        self.encodings
                            .iter()
                            .find(|encoding| {
                                encoding.media == media && encoding.rid.as_deref() == Some(rid)
                            })
                            .and_then(|encoding| encoding.ssrc)
                    })
                });
            let primary_ssrc = primary_ssrc.or_else(|| {
                self.encodings
                    .iter()
                    .find(|encoding| {
                        encoding.media == media && encoding.repair_ssrc == Some(packet.ssrc())
                    })
                    .and_then(|encoding| encoding.ssrc)
            });
            let Some(primary_ssrc) = primary_ssrc else {
                self.drop();
                return;
            };
            if self.encodings.iter().any(|encoding| {
                encoding.media == media
                    && (encoding.repair_ssrc == Some(packet.ssrc())
                        || rid.is_some_and(|rid| encoding.rid.as_deref() == Some(rid)))
                    && encoding.ssrc != Some(primary_ssrc)
            }) {
                self.drop();
                return;
            }
            let Some(sequence) = packet.payload().get(..2) else {
                self.drop();
                return;
            };
            if repair {
                self.drop();
                return;
            }
            let mut normalized = bytes[..packet.payload_range().start].to_vec();
            normalized[0] &= !0x20;
            normalized[1] = (normalized[1] & 0x80) | primary_type;
            normalized[2..4].copy_from_slice(sequence);
            normalized[8..12].copy_from_slice(&primary_ssrc.to_be_bytes());
            normalized.extend_from_slice(&packet.payload()[2..]);
            if let Some(encoding) = self
                .encodings
                .iter_mut()
                .find(|encoding| encoding.media == media && encoding.ssrc == Some(primary_ssrc))
            {
                if encoding.retired
                    || encoding
                        .repair_ssrc
                        .is_some_and(|known| known != packet.ssrc())
                {
                    self.drop();
                    return;
                }
                encoding.repair_ssrc = Some(packet.ssrc());
            }
            self.accept_packet(arrival, normalized, true, None);
            return;
        }
        // A negotiated repair SSRC cannot create an unrelated primary encoding.
        if self.media[media]
            .ssrc_repairs
            .iter()
            .any(|(rtx, _)| *rtx == packet.ssrc())
            || self
                .encodings
                .iter()
                .any(|encoding| encoding.repair_ssrc == Some(packet.ssrc()))
        {
            self.drop();
            return;
        }
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
                if self.events.len() > MAX_RECORDS.saturating_sub(2) {
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
                    repair_ssrc: None,
                    rid: rid.map(Box::<str>::from),
                    unsignaled,
                    retired: false,
                    receiver: ReceiveStream::new(
                        clock_rate,
                        self.media[media]
                            .nack_payloads
                            .contains(&packet.payload_type()),
                    ),
                    mapper: ClockMapper::new(
                        arrival.global,
                        packet.sequence(),
                        packet.timestamp(),
                        clock_rate,
                    ),
                });
                let index = self.encodings.len() - 1;
                self.events
                    .push_back(Event::EncodingDiscovered(EncodingInfo {
                        id,
                        sender_index: self.media[media].sender_index,
                        kind: self.media[media].kind,
                        rid: rid.map(Arc::from),
                    }));
                index
            }
        };
        let (encoding, global_media_at, warning, clock_quality) = {
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
            if !encoding.receiver.accept(
                packet.sequence(),
                packet.timestamp(),
                arrival.monotonic,
                repair,
            ) {
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
            (
                encoding.id,
                global_media_at,
                warning,
                encoding.mapper.quality(),
            )
        };
        if let Some(warning) = warning {
            self.push_clock_warning(warning);
        }
        let extensions = self.extensions_for(media, &packet);
        let packet = MediaPacket::with_clock_quality(
            Bytes::from(bytes),
            global_media_at,
            extensions,
            clock_quality,
        );
        if packet.queued_bytes() > self.max_bytes.saturating_sub(self.queued_media_bytes) {
            self.drop();
            return;
        }
        self.queued_media_bytes = self
            .queued_media_bytes
            .saturating_add(packet.queued_bytes());
        self.events.push_back(Event::Media { encoding, packet });
    }

    pub(crate) fn accept_rtcp(
        &mut self,
        arrival: TimePoint,
        bytes: Vec<u8>,
        path_epoch: PathEpoch,
        mode: Option<PacketFeedbackKind>,
        smoothed_rtt: Option<std::time::Duration>,
    ) -> Vec<rtcp::ReportReceipt> {
        if bytes.len() > self.max_bytes {
            self.drop();
            return Vec::new();
        }
        let Ok(parsed) = rtcp::parse(&bytes, arrival, path_epoch, mode) else {
            self.drop();
            return Vec::new();
        };
        if parsed.lifecycle.len() > MAX_RECORDS.saturating_sub(self.events.len())
            || parsed.feedback.len() > MAX_RECORDS.saturating_sub(self.feedback.len())
            || parsed.repair.len() > MAX_RECORDS.saturating_sub(self.repair.len())
            || parsed.keyframes.len() > MAX_RECORDS.saturating_sub(self.keyframes.len())
        {
            self.drop();
            return Vec::new();
        }
        for fact in parsed.lifecycle {
            self.apply_rtcp_fact(arrival, smoothed_rtt, fact);
        }
        self.feedback.extend(parsed.feedback);
        self.repair.extend(parsed.repair);
        self.keyframes.extend(parsed.keyframes);
        parsed.receipts
    }

    #[allow(
        dead_code,
        reason = "Plan 08 consumes authenticated, mode-filtered feedback through this owner seam"
    )]
    pub(crate) fn poll_feedback(&mut self) -> Option<FeedbackBatch> {
        self.feedback.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn test_queue_feedback(&mut self, feedback: FeedbackBatch) {
        self.feedback.push_back(feedback);
    }

    pub(crate) fn poll_keyframe(&mut self) -> Option<rtcp::KeyframeRequest> {
        self.keyframes.pop_front()
    }

    pub(crate) fn poll_repair(&mut self) -> Option<RepairRequest> {
        self.repair.pop_front()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if let Event::Media { packet, .. } = &event {
            self.queued_media_bytes = self
                .queued_media_bytes
                .saturating_sub(packet.queued_bytes());
        }
        Some(event)
    }

    pub(crate) fn retire(&mut self, id: EncodingId) -> Result<(), crate::CommandError> {
        let encoding = self
            .encodings
            .iter_mut()
            .find(|encoding| encoding.id == id)
            .ok_or(crate::CommandError::UnknownEncoding(id))?;
        if !encoding.retired {
            encoding.retired = true;
            self.events.push_back(Event::EncodingRetired {
                encoding: id,
                reason: EncodingRetireReason::Retired,
            });
        }
        Ok(())
    }

    pub(crate) fn keyframe_request(&self, id: EncodingId) -> Result<[u8; 12], crate::CommandError> {
        let ssrc = self
            .encodings
            .iter()
            .find(|encoding| encoding.id == id && !encoding.retired)
            .and_then(|encoding| encoding.ssrc)
            .ok_or(crate::CommandError::UnknownEncoding(id))?;
        let mut packet = [0_u8; 12];
        packet[..4].copy_from_slice(&[0x81, 206, 0, 2]);
        packet[4..8].copy_from_slice(&self.receiver_ssrc.to_be_bytes());
        packet[8..].copy_from_slice(&ssrc.to_be_bytes());
        Ok(packet)
    }

    pub(crate) fn abort(&mut self) {
        self.events.clear();
        self.feedback.clear();
        self.receive_feedback.clear();
        self.repair.clear();
        self.keyframes.clear();
        self.queued_media_bytes = 0;
    }

    pub(crate) fn stats(&self) -> Vec<EncodingStats> {
        self.encodings
            .iter()
            .map(|encoding| EncodingStats {
                encoding: encoding.id,
                kind: self.media[encoding.media].kind,
                received_packets: encoding.receiver.received(),
                retired: encoding.retired,
            })
            .collect()
    }

    pub(crate) fn poll_control(&mut self, now: Instant) -> Option<Vec<u8>> {
        if let Some(packet) = self.receive_feedback.poll(now, self.receiver_ssrc) {
            return Some(packet);
        }
        for _ in 0..self.encodings.len() {
            let index = self.control_cursor % self.encodings.len();
            self.control_cursor = index.saturating_add(1);
            let encoding = &mut self.encodings[index];
            if !encoding.retired
                && let Some(ssrc) = encoding.ssrc
                && let Some(packet) = encoding
                    .receiver
                    .poll_control(now, self.receiver_ssrc, ssrc)
            {
                return Some(packet);
            }
        }
        None
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.encodings
            .iter()
            .filter(|encoding| !encoding.retired)
            .filter_map(|encoding| encoding.receiver.next_deadline())
            .chain(self.receive_feedback.deadline())
            .min()
    }

    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }

    fn resolve<'a>(&self, packet: &'a RtpPacket<'a>) -> Option<(usize, u32, Option<&'a str>)> {
        // BUNDLE negotiation guarantees one shared MID extension ID. MID and
        // authenticated SSRC association take precedence over payload type.
        let mid_id = self.media.iter().find_map(|media| {
            media.extensions.iter().find_map(|(id, uri)| {
                (uri.as_ref() == "urn:ietf:params:rtp-hdrext:sdes:mid").then_some(*id)
            })
        });
        let mid = packet
            .extensions()
            .ok()?
            .find(|value| Some(value.id()) == mid_id);
        let by_mid = match mid {
            Some(value) => {
                let mid = std::str::from_utf8(value.value()).ok()?;
                Some(
                    self.media
                        .iter()
                        .position(|media| media.mid.as_ref() == mid)?,
                )
            }
            None => None,
        };
        let by_ssrc = self
            .encodings
            .iter()
            .find(|encoding| {
                encoding.ssrc == Some(packet.ssrc()) || encoding.repair_ssrc == Some(packet.ssrc())
            })
            .map(|encoding| encoding.media)
            .or_else(|| {
                self.media
                    .iter()
                    .position(|media| media.ssrcs.contains(&packet.ssrc()))
            });
        if by_mid.zip(by_ssrc).is_some_and(|(mid, ssrc)| mid != ssrc) {
            return None;
        }
        let selected = by_mid.or(by_ssrc);
        let mut candidates = self.media.iter().enumerate().filter_map(|(index, media)| {
            if selected.is_some_and(|selected| selected != index) {
                return None;
            }
            media
                .payloads
                .iter()
                .find(|(payload_type, _)| {
                    *payload_type == packet.payload_type()
                        || media.repairs.iter().any(|(repair, primary)| {
                            *repair == packet.payload_type() && primary == payload_type
                        })
                })
                .map(|(_, clock_rate)| (index, *clock_rate))
        });
        let (index, clock_rate) = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        let rid = extension_value(
            packet,
            &self.media[index].extensions,
            if self.media[index]
                .repairs
                .iter()
                .any(|(repair, _)| *repair == packet.payload_type())
            {
                "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
            } else {
                "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
            },
        );
        Some((index, clock_rate, rid))
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
                self.encodings[index].receiver.sender_report(
                    ntp_seconds,
                    ntp_fraction,
                    arrival.monotonic,
                );
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
    use crate::{GlobalMediaTime, MediaKind};

    #[test]
    fn keyframe_requests_are_bounded_and_cleared_on_abort() {
        let mut ingress = owner();
        let pli = [0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7];
        for _ in 0..MAX_RECORDS {
            ingress.accept_rtcp(at(1), pli.to_vec(), PathEpoch::from_value(1), None, None);
        }
        assert_eq!(ingress.keyframes.len(), MAX_RECORDS);
        let dropped = ingress.dropped;
        ingress.accept_rtcp(at(1), pli.to_vec(), PathEpoch::from_value(1), None, None);
        assert_eq!(ingress.dropped, dropped + 1);
        ingress.abort();
        assert!(ingress.poll_keyframe().is_none());
    }

    fn owner() -> IngressOwner {
        IngressOwner::new(
            vec![IngressMediaFacts {
                sender_index: 3,
                kind: MediaKind::Video,
                mid: "video".into(),
                payloads: vec![(96, 90_000)].into(),
                feedback_payloads: Box::new([]),
                repairs: Box::new([]),
                nack_payloads: Box::new([]),
                ssrc_repairs: Box::new([]),
                ssrcs: Box::new([]),
                rids: Box::new([]),
                extensions: vec![(1, "urn:test:extension".into())].into(),
            }]
            .into(),
            1,
            1024,
            9,
            ReceiveFeedback::new(None, Instant::now()),
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
        owner.accept_rtp(at(1_000), rtp(u16::MAX, u32::MAX - 1), None);
        let Event::EncodingDiscovered(info) = owner.poll_event().expect("discovery") else {
            panic!("unsignaled encoding must be discovered first");
        };
        let Event::Media { encoding, packet } = owner.poll_event().expect("media") else {
            panic!("media follows discovery");
        };
        assert_eq!(encoding, info.id);
        assert_eq!(info.sender_index, 3);
        assert_eq!(info.rid, None);
        assert_eq!(
            packet.global_media_at(),
            GlobalMediaTime::from_micros(1_000)
        );
        assert_eq!(packet.extension("urn:test:extension"), Some(&[0xaa][..]));
        assert_eq!(packet.to_transit().bytes(), packet.bytes());

        owner.accept_rtp(at(9_000_000), rtp(0, 1), None);
        let Event::Media { packet, .. } = owner.poll_event().expect("wrapped media") else {
            panic!("wrapped packet is media");
        };
        assert_eq!(
            packet.global_media_at(),
            GlobalMediaTime::from_micros(1_033)
        );
    }

    fn bundled_owner() -> IngressOwner {
        IngressOwner::new(
            [(2, "v0", 7), (5, "v1", 8)]
                .into_iter()
                .map(|(sender_index, mid, ssrc)| IngressMediaFacts {
                    sender_index,
                    kind: MediaKind::Video,
                    mid: mid.into(),
                    payloads: vec![(96, 90_000)].into(),
                    feedback_payloads: Box::new([]),
                    repairs: Box::new([]),
                    nack_payloads: Box::new([]),
                    ssrc_repairs: Box::new([]),
                    ssrcs: vec![ssrc].into(),
                    rids: vec!["hi".into()].into(),
                    extensions: vec![
                        (1, "urn:ietf:params:rtp-hdrext:sdes:mid".into()),
                        (2, "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".into()),
                    ]
                    .into(),
                })
                .collect(),
            1,
            4096,
            9,
            ReceiveFeedback::new(None, Instant::now()),
        )
    }

    fn bundled_rtp(ssrc: u32, mid: Option<&[u8]>, rid: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = vec![0x80, 96, 0, 1, 0, 0, 0, 1];
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        if mid.is_some() || rid.is_some() {
            bytes[0] |= 0x10;
            let mut extensions = Vec::new();
            for (id, value) in [(1_u8, mid), (2_u8, rid)] {
                if let Some(value) = value {
                    let length = u8::try_from(value.len() - 1).expect("short extension");
                    extensions.push((id << 4) | length);
                    extensions.extend_from_slice(value);
                }
            }
            while !extensions.len().is_multiple_of(4) {
                extensions.push(0);
            }
            bytes.extend_from_slice(&[0xbe, 0xde]);
            bytes.extend_from_slice(
                &u16::try_from(extensions.len() / 4)
                    .expect("length")
                    .to_be_bytes(),
            );
            bytes.extend_from_slice(&extensions);
        }
        bytes.extend_from_slice(&[1, 2]);
        bytes
    }

    fn discovered(owner: &mut IngressOwner, sender_index: u32, rid: Option<&str>) -> EncodingId {
        let Event::EncodingDiscovered(info) = owner.poll_event().expect("discovery") else {
            panic!("all encodings must be discovered before media");
        };
        assert_eq!(info.sender_index, sender_index);
        assert_eq!(info.kind, MediaKind::Video);
        assert_eq!(info.rid.as_deref(), rid);
        let Event::Media { encoding, packet } = owner.poll_event().expect("media") else {
            panic!("media follows discovery");
        };
        assert_eq!(encoding, info.id);
        assert_eq!(
            packet.global_media_at(),
            GlobalMediaTime::from_micros(1_000)
        );
        info.id
    }

    #[test]
    fn bundled_same_kind_encodings_resolve_mid_ssrc_and_retirement() {
        let mut owner = bundled_owner();
        owner.accept_rtp(at(1_000), bundled_rtp(7, None, Some(b"hi")), None);
        let first = discovered(&mut owner, 2, Some("hi"));
        owner.accept_rtp(at(1_000), bundled_rtp(8, Some(b"v1"), Some(b"hi")), None);
        let second = discovered(&mut owner, 5, Some("hi"));
        assert_ne!(first, second);

        let mut next = bundled_rtp(8, None, None);
        next[3] = 2;
        owner.accept_rtp(at(1_000), next, None);
        assert!(
            matches!(owner.poll_event(), Some(Event::Media { encoding, .. }) if encoding == second)
        );
        owner.retire(first).expect("retire");
        assert!(
            matches!(owner.poll_event(), Some(Event::EncodingRetired { encoding, .. }) if encoding == first)
        );
        owner.accept_rtp(at(1_000), bundled_rtp(7, Some(b"v0"), Some(b"hi")), None);
        assert!(owner.poll_event().is_none());
    }

    #[test]
    fn unsignaled_mid_association_does_not_leak_between_bundled_sections() {
        let mut owner = bundled_owner();
        owner.accept_rtp(at(1_000), bundled_rtp(99, Some(b"v1"), None), None);
        let encoding = discovered(&mut owner, 5, None);
        let mut next = bundled_rtp(99, None, None);
        next[3] = 2;
        owner.accept_rtp(at(1_000), next, None);
        assert!(
            matches!(owner.poll_event(), Some(Event::Media { encoding: id, .. }) if id == encoding)
        );
        owner.accept_rtp(at(1_000), bundled_rtp(100, Some(b"v0"), None), None);
        assert!(owner.poll_event().is_none());
        assert_eq!(owner.stats().len(), 1);
    }

    #[test]
    fn ambiguous_or_conflicting_bundle_packets_create_no_encoding() {
        let mut owner = bundled_owner();
        for packet in [
            bundled_rtp(99, None, None),
            bundled_rtp(99, Some(b"xx"), None),
            bundled_rtp(99, Some(&[0xff]), None),
            bundled_rtp(7, Some(b"v1"), None),
        ] {
            owner.accept_rtp(at(1_000), packet, None);
            assert!(owner.poll_event().is_none());
            assert!(owner.stats().is_empty());
        }
        assert_eq!(owner.dropped(), 4);
    }

    #[test]
    fn incoming_rtx_recovers_one_primary_packet_without_duplicate_source_delivery() {
        let mut owner = owner();
        owner.media[0].repairs = vec![(97, 96)].into();
        owner.media[0].ssrc_repairs = vec![(8, 7)].into();
        for sequence in [1, 3] {
            owner.accept_rtp(at(1_000), rtp(sequence, u32::from(sequence) * 3_000), None);
        }
        let original = rtp(2, 6_000);
        let header_len = RtpPacket::parse(&original).unwrap().payload_range().start;
        let mut repair = original[..header_len].to_vec();
        repair[1] = 97;
        repair[2..4].copy_from_slice(&100_u16.to_be_bytes());
        repair[8..12].copy_from_slice(&8_u32.to_be_bytes());
        repair.extend_from_slice(&2_u16.to_be_bytes());
        repair.extend_from_slice(&original[header_len..]);
        owner.accept_rtp(at(1_000), repair.clone(), None);
        owner.accept_rtp(at(1_000), original.clone(), None);
        owner.accept_rtp(at(1_000), repair, None);
        let packets = std::iter::from_fn(|| owner.poll_event())
            .filter_map(|event| match event {
                Event::Media { packet, .. } => Some(packet),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[2].bytes().as_ref(), original);
        assert_eq!(owner.stats().len(), 1);
        assert_eq!(owner.stats()[0].received_packets, 3);
    }

    #[test]
    fn repaired_rid_learns_only_a_bounded_primary_transport_association() {
        let mut owner = owner();
        owner.media[0].repairs = vec![(97, 96)].into();
        owner.media[0].rids = vec!["hi".into()].into();
        owner.media[0].extensions = vec![
            (2, "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".into()),
            (
                3,
                "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id".into(),
            ),
        ]
        .into();
        owner.accept_rtp(at(1_000), bundled_rtp(7, None, Some(b"hi")), None);
        let encoding = discovered(&mut owner, 3, Some("hi"));
        for sequence in [2_u16, 3] {
            let rid = (sequence == 2).then_some(b"hi".as_slice());
            let mut repair = bundled_rtp(8, None, rid);
            if rid.is_some() {
                repair[16] = 0x31;
            }
            repair[1] = 97;
            repair[2..4].copy_from_slice(&sequence.wrapping_add(100).to_be_bytes());
            let offset = RtpPacket::parse(&repair).unwrap().payload_range().start;
            repair.splice(offset..offset, sequence.to_be_bytes());
            owner.accept_rtp(at(1_000), repair, None);
            let Event::Media {
                encoding: actual,
                packet,
            } = owner.poll_event().expect("repaired primary")
            else {
                panic!("repair must not discover an encoding");
            };
            assert_eq!(actual, encoding);
            assert_eq!(packet.view().unwrap().ssrc, 7);
            assert_eq!(packet.view().unwrap().sequence_number, sequence);
        }
        owner.accept_rtp(at(1_000), bundled_rtp(8, None, None), None);
        assert!(owner.poll_event().is_none());
        assert_eq!(owner.stats().len(), 1);
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
