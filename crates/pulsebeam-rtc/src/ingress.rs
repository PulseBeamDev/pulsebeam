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

use std::{collections::VecDeque, sync::Arc, time::Instant};

use bytes::Bytes;

use crate::{
    EcnCodepoint, EncodingInfo, EncodingRetireReason, EncodingStats, Event, PacketFeedbackKind,
    RtpExtension, RtpHeader, RtpPacket as ReceivedRtp, SenderReport,
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
    max_unsignaled: usize,
    max_bytes: usize,
    queued_media_bytes: usize,
    dropped: u64,
}

struct Encoding {
    media: usize,
    ssrc: Option<u32>,
    repair_ssrc: Option<u32>,
    rid: Option<Box<str>>,
    unsignaled: bool,
    retired: bool,
    receiver: ReceiveStream,
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
            max_unsignaled: usize::from(max_unsignaled),
            max_bytes,
            queued_media_bytes: 0,
            dropped: 0,
        }
    }

    pub(crate) fn accept_rtp(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        ecn: Option<EcnCodepoint>,
    ) {
        self.accept_packet(arrival, bytes, false, ecn);
    }

    fn accept_packet(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        repair: bool,
        ecn: Option<EcnCodepoint>,
    ) {
        let bytes = Bytes::from(bytes);
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
            self.receive_feedback
                .observe(arrival, packet.ssrc(), packet.sequence(), twcc, ecn);
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
                self.encodings.push(Encoding {
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
                });
                let index = self.encodings.len() - 1;
                self.events
                    .push_back(Event::EncodingDiscovered(EncodingInfo {
                        mid: self.media[media].mid.clone().into(),
                        ssrc: packet.ssrc(),
                        kind: self.media[media].kind,
                        rid: rid.map(Arc::from),
                    }));
                index
            }
        };
        let state = &mut self.encodings[encoding];
        if state.ssrc.is_none() {
            state.ssrc = Some(packet.ssrc());
        }
        if state.rid.is_none() {
            state.rid = rid.map(Box::<str>::from);
        }
        if state.retired
            || !state
                .receiver
                .accept(packet.sequence(), packet.timestamp(), arrival, repair)
        {
            self.drop();
            return;
        }
        let header = RtpHeader {
            sequence: packet.sequence(),
            timestamp: packet.timestamp(),
            ssrc: packet.ssrc(),
            payload_type: packet.payload_type(),
            marker: packet.marker(),
        };
        let payload_range = packet.payload_range();
        let extension_ranges = packet
            .extensions()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|value| {
                self.media[media]
                    .extensions
                    .iter()
                    .find(|(id, _)| *id == value.id())
                    .map(|(_, uri)| (Arc::<str>::from(uri.as_ref()), value.range()))
            });
        let rid = rid.map(Arc::from);
        let packet = ReceivedRtp {
            arrival,
            header,
            payload: bytes.slice(payload_range),
            extensions: extension_ranges
                .map(|(uri, range)| RtpExtension {
                    uri,
                    value: bytes.slice(range),
                })
                .collect(),
        };
        self.queued_media_bytes = self
            .queued_media_bytes
            .saturating_add(packet.queued_bytes());
        self.events.push_back(Event::Rtp {
            mid: self.media[media].mid.clone().into(),
            rid,
            packet,
        });
    }

    pub(crate) fn accept_rtcp(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        path_epoch: PathEpoch,
        mode: Option<PacketFeedbackKind>,
        _smoothed_rtt: Option<std::time::Duration>,
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
            self.apply_rtcp_fact(arrival, fact);
        }
        self.feedback.extend(parsed.feedback);
        self.repair.extend(parsed.repair);
        self.keyframes.extend(parsed.keyframes);
        parsed.receipts
    }

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

    pub(crate) fn has_pending_output(&self) -> bool {
        !self.events.is_empty()
            || !self.feedback.is_empty()
            || !self.keyframes.is_empty()
            || !self.repair.is_empty()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if let Event::Rtp { packet, .. } = &event {
            self.queued_media_bytes = self
                .queued_media_bytes
                .saturating_sub(packet.queued_bytes());
        }
        Some(event)
    }

    pub(crate) fn retire(&mut self, ssrc: u32) -> Result<(), crate::WriteError> {
        let encoding = self
            .encodings
            .iter_mut()
            .find(|encoding| encoding.ssrc == Some(ssrc))
            .ok_or(crate::WriteError::UnknownEncoding)?;
        if !encoding.retired {
            encoding.retired = true;
            self.events.push_back(Event::EncodingRetired {
                ssrc,
                reason: EncodingRetireReason::Retired,
            });
        }
        Ok(())
    }

    pub(crate) fn keyframe_request(&self, ssrc: u32) -> Result<[u8; 12], crate::WriteError> {
        let ssrc = self
            .encodings
            .iter()
            .find(|encoding| encoding.ssrc == Some(ssrc) && !encoding.retired)
            .and_then(|encoding| encoding.ssrc)
            .ok_or(crate::WriteError::UnknownEncoding)?;
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
                mid: self.media[encoding.media].mid.clone().into(),
                ssrc: encoding.ssrc,
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

    fn drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    fn apply_rtcp_fact(&mut self, arrival: Instant, fact: LifecycleFact) {
        match fact {
            LifecycleFact::Cname { .. } => {}
            LifecycleFact::Bye { ssrc } => {
                if let Some(encoding) = self
                    .encodings
                    .iter_mut()
                    .find(|encoding| encoding.ssrc == Some(ssrc))
                    && !encoding.retired
                {
                    encoding.retired = true;
                    self.events.push_back(Event::EncodingRetired {
                        ssrc,
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
                if let Some(encoding) = self
                    .encodings
                    .iter_mut()
                    .find(|encoding| encoding.ssrc == Some(ssrc) && !encoding.retired)
                {
                    encoding
                        .receiver
                        .sender_report(ntp_seconds, ntp_fraction, arrival);
                }
                self.events.push_back(Event::SenderReport(SenderReport {
                    arrival,
                    ssrc,
                    ntp_seconds,
                    ntp_fraction,
                    rtp_timestamp,
                }));
            }
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
