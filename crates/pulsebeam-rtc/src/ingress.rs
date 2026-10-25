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
    media: Vec<IngressMediaFacts>,
    active: Vec<bool>,
    strict: bool,
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

pub(crate) struct IngressUpdate {
    new_media: Vec<IngressMediaFacts>,
    active: Vec<bool>,
    remap: Vec<(usize, usize)>,
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
            active: vec![true; media.len()],
            strict: false,
            media: media.into_vec(),
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

    pub(crate) fn set_feedback_profile(&mut self, profile: PacketFeedbackKind) {
        self.receive_feedback.set_profile(profile);
    }

    pub(crate) fn stage_configuration(
        &self,
        desired: &[(crate::rtp_config::PhysicalBinding, bool)],
        limit: usize,
    ) -> Result<IngressUpdate, crate::RtpConfigurationError> {
        use crate::RtpConfigurationError as Error;
        let mut update = IngressUpdate {
            new_media: Vec::new(),
            active: vec![false; self.media.len()],
            remap: Vec::new(),
        };
        for (physical, active) in desired.iter().filter(|(physical, _)| !physical.outbound) {
            let encoding = physical.encoding();
            if let Some((index, _)) = self
                .media
                .iter()
                .enumerate()
                .find(|(_, media)| **media == physical.ingress_facts(media.sender_index))
            {
                update.active[index] = *active;
                continue;
            }
            let index = self.media.len().saturating_add(update.new_media.len());
            if index >= limit {
                return Err(Error::Capacity);
            }
            let facts = physical.ingress_facts(u32::try_from(index).map_err(|_| Error::Capacity)?);
            // Bootstrap and typed mappings use the same receive streams. Moving
            // a known SSRC to a narrower selected envelope retains its receiver.
            for (old_index, old) in self.media.iter().enumerate() {
                let shares = old.ssrcs.iter().any(|ssrc| {
                    *ssrc == encoding.primary.get()
                        || encoding.repair.is_some_and(|r| r.get() == *ssrc)
                }) || old.ssrc_repairs.iter().any(|(repair, primary)| {
                    *repair == encoding.primary.get()
                        || *primary == encoding.primary.get()
                        || encoding
                            .repair
                            .is_some_and(|r| r.get() == *repair || r.get() == *primary)
                });
                if shares
                    && (self.strict
                        || old.mid != facts.mid
                        || old.kind != facts.kind
                        || !facts.payloads.iter().all(|p| old.payloads.contains(p))
                        || !facts.repairs.iter().all(|p| old.repairs.contains(p))
                        || !facts.extensions.iter().all(|p| old.extensions.contains(p)))
                {
                    return Err(Error::Collision);
                }
                if shares {
                    for (encoding_index, _) in self.encodings.iter().enumerate().filter(|(_, e)| {
                        e.media == old_index && e.ssrc == Some(encoding.primary.get())
                    }) {
                        update.remap.push((encoding_index, index));
                    }
                }
            }
            update.new_media.push(facts);
            update.active.push(*active);
        }
        Ok(update)
    }

    pub(crate) fn commit_configuration(&mut self, update: IngressUpdate) {
        for (index, media) in update.remap {
            self.encodings[index].media = media;
        }
        for encoding in &mut self.encodings {
            let active = update.active.get(encoding.media) == Some(&true);
            if !active {
                encoding.retired = true;
            } else if self.active.get(encoding.media) != Some(&true) {
                encoding.retired = false;
            }
        }
        self.media.extend(update.new_media);
        self.active = update.active;
        self.strict = true;
    }

    pub(crate) const fn control_ssrc(&self) -> u32 {
        self.receiver_ssrc
    }

    pub(crate) fn physical_identities(&self) -> Vec<u32> {
        self.media
            .iter()
            .flat_map(|m| {
                m.ssrcs
                    .iter()
                    .copied()
                    .chain(m.ssrc_repairs.iter().map(|(repair, _)| *repair))
            })
            .chain(
                self.encodings
                    .iter()
                    .flat_map(|e| [e.ssrc, e.repair_ssrc].into_iter().flatten()),
            )
            .collect()
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
        // MID/RID may stop repeating after SSRC acknowledgment. Preserve the
        // authenticated association, but never hide an explicit RID conflict.
        if state
            .rid
            .as_deref()
            .zip(rid)
            .is_some_and(|(known, signaled)| known != signaled)
        {
            self.drop();
            return;
        }
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
        let rid = state.rid.as_deref().map(Arc::from);
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
        let permitted = match self
            .encodings
            .iter()
            .find(|encoding| encoding.ssrc == Some(ssrc))
        {
            Some(encoding) => !encoding.retired,
            None => self.media.iter().any(|media| {
                media.kind == crate::MediaKind::Video
                    && media.ssrcs.contains(&ssrc)
                    && !media.ssrc_repairs.iter().any(|(repair, _)| *repair == ssrc)
            }),
        };
        if !permitted {
            return Err(crate::WriteError::UnknownEncoding);
        }
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
        let mid_id = self
            .media
            .iter()
            .enumerate()
            .filter(|(i, _)| self.active[*i])
            .find_map(|(_, media)| {
                media.extensions.iter().find_map(|(id, uri)| {
                    (uri.as_ref() == "urn:ietf:params:rtp-hdrext:sdes:mid").then_some(*id)
                })
            });
        let mid = packet
            .extensions()
            .ok()?
            .find(|value| Some(value.id()) == mid_id);
        let by_mid = mid
            .map(|value| std::str::from_utf8(value.value()))
            .transpose()
            .ok()?;
        let by_ssrc = self
            .encodings
            .iter()
            .find(|encoding| {
                self.active[encoding.media]
                    && (encoding.ssrc == Some(packet.ssrc())
                        || encoding.repair_ssrc == Some(packet.ssrc()))
            })
            .map(|encoding| encoding.media)
            .or_else(|| {
                self.media
                    .iter()
                    .enumerate()
                    .find(|(i, media)| {
                        self.active[*i]
                            && (media.ssrcs.contains(&packet.ssrc())
                                || media
                                    .ssrc_repairs
                                    .iter()
                                    .any(|(repair, _)| *repair == packet.ssrc()))
                    })
                    .map(|(i, _)| i)
            });
        if self.strict && by_ssrc.is_none() {
            return None;
        }
        let selected = by_ssrc;
        let mut candidates = self.media.iter().enumerate().filter_map(|(index, media)| {
            if !self.active[index]
                || selected.is_some_and(|selected| selected != index)
                || by_mid.is_some_and(|mid| media.mid.as_ref() != mid)
            {
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
        if self.strict
            && rid.is_some_and(|rid| {
                !self.media[index]
                    .rids
                    .iter()
                    .any(|known| known.as_ref() == rid)
            })
        {
            return None;
        }
        Some((index, clock_rate, rid))
    }

    fn find_encoding(&self, media: usize, ssrc: u32, rid: Option<&str>) -> Option<usize> {
        self.encodings
            .iter()
            .position(|encoding| encoding.media == media && encoding.ssrc == Some(ssrc))
            .or_else(|| {
                self.encodings.iter().position(|encoding| {
                    encoding.media == media
                        && rid.is_some_and(|rid| encoding.rid.as_deref() == Some(rid))
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rid_ingress(now: Instant) -> IngressOwner {
        IngressOwner::new(
            Box::new([IngressMediaFacts {
                sender_index: 0,
                kind: crate::MediaKind::Video,
                mid: "v".into(),
                payloads: Box::new([(96, 90_000)]),
                feedback_payloads: Box::new([96]),
                repairs: Box::new([]),
                nack_payloads: Box::new([96]),
                ssrc_repairs: Box::new([]),
                ssrcs: Box::new([]),
                rids: Box::new(["q".into(), "h".into()]),
                extensions: Box::new([
                    (1, "urn:ietf:params:rtp-hdrext:sdes:mid".into()),
                    (2, "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".into()),
                ]),
            }]),
            2,
            4096,
            99,
            ReceiveFeedback::new(None, now),
        )
    }

    fn rid_packet(ssrc: u32, sequence: u16, rid: Option<u8>) -> Vec<u8> {
        let mut bytes = vec![if rid.is_some() { 0x90 } else { 0x80 }, 96];
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&u32::from(sequence).to_be_bytes());
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        if let Some(rid) = rid {
            bytes.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, b'v', 0x20, rid]);
        }
        bytes.push(0x65);
        bytes
    }

    fn received_rids(ingress: &mut IngressOwner) -> Vec<(u32, Option<String>)> {
        std::iter::from_fn(|| ingress.poll_event())
            .filter_map(|event| match event {
                Event::Rtp { rid, packet, .. } => {
                    Some((packet.header.ssrc, rid.as_deref().map(str::to_owned)))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn established_ssrc_retains_rid_when_sdes_headers_are_omitted() {
        let now = Instant::now();
        let mut ingress = rid_ingress(now);
        ingress.accept_rtp(now, rid_packet(11, 1, Some(b'q')), None);
        ingress.accept_rtp(now, rid_packet(22, 100, Some(b'h')), None);
        assert_eq!(
            received_rids(&mut ingress),
            [(11, Some("q".into())), (22, Some("h".into()))]
        );
        // Acknowledged libwebrtc senders may stop repeating both MID and RID.
        ingress.accept_rtp(now, rid_packet(11, 2, None), None);
        ingress.accept_rtp(now, rid_packet(22, 101, None), None);
        assert_eq!(
            received_rids(&mut ingress),
            [(11, Some("q".into())), (22, Some("h".into()))]
        );
    }

    #[test]
    fn established_ssrc_rejects_conflicting_rid_without_rebinding() {
        let now = Instant::now();
        let mut ingress = rid_ingress(now);
        ingress.accept_rtp(now, rid_packet(11, 1, Some(b'q')), None);
        ingress.accept_rtp(now, rid_packet(22, 100, Some(b'h')), None);
        assert_eq!(
            received_rids(&mut ingress),
            [(11, Some("q".into())), (22, Some("h".into()))]
        );
        // Neither insertion order may let a RID override an established SSRC.
        ingress.accept_rtp(now, rid_packet(11, 2, Some(b'h')), None);
        ingress.accept_rtp(now, rid_packet(22, 101, Some(b'q')), None);
        assert!(received_rids(&mut ingress).is_empty());
        ingress.accept_rtp(now, rid_packet(11, 3, None), None);
        ingress.accept_rtp(now, rid_packet(22, 102, None), None);
        assert_eq!(
            received_rids(&mut ingress),
            [(11, Some("q".into())), (22, Some("h".into()))]
        );
    }

    #[test]
    fn established_ssrc_retirement_rejects_headerless_late_packets() {
        let now = Instant::now();
        let mut ingress = rid_ingress(now);
        ingress.accept_rtp(now, rid_packet(11, 1, Some(b'q')), None);
        assert_eq!(received_rids(&mut ingress), [(11, Some("q".into()))]);
        ingress.retire(11).unwrap();
        assert!(received_rids(&mut ingress).is_empty());
        ingress.accept_rtp(now, rid_packet(11, 2, None), None);
        assert!(received_rids(&mut ingress).is_empty());
    }

    #[test]
    fn offered_primary_can_bootstrap_a_keyframe_without_discovering_an_encoding() {
        let facts = IngressMediaFacts {
            sender_index: 0,
            kind: crate::MediaKind::Video,
            mid: "video".into(),
            payloads: Box::new([(96, 90_000)]),
            feedback_payloads: Box::new([96]),
            repairs: Box::new([(97, 96)]),
            nack_payloads: Box::new([96]),
            ssrc_repairs: Box::new([(12, 11)]),
            ssrcs: Box::new([11, 12]),
            rids: Box::new([]),
            extensions: Box::new([]),
        };
        let now = Instant::now();
        let mut ingress = IngressOwner::new(
            Box::new([facts]),
            1,
            4096,
            99,
            ReceiveFeedback::new(None, now),
        );
        let request = ingress.keyframe_request(11).unwrap();
        assert_eq!(&request[..4], &[0x81, 206, 0, 2]);
        assert_eq!(&request[4..8], &99_u32.to_be_bytes());
        assert_eq!(&request[8..], &11_u32.to_be_bytes());
        for ssrc in [0, 12, 13] {
            assert_eq!(
                ingress.keyframe_request(ssrc).unwrap_err(),
                crate::WriteError::UnknownEncoding
            );
        }
        assert!(ingress.poll_event().is_none());
        assert!(
            ingress.stats().is_empty(),
            "signaling is not authenticated encoding discovery"
        );
        ingress.accept_rtp(
            now,
            vec![0x80, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 11, 0x65],
            None,
        );
        assert!(matches!(
            ingress.poll_event(),
            Some(Event::EncodingDiscovered { .. })
        ));
        ingress.retire(11).unwrap();
        assert_eq!(
            ingress.keyframe_request(11).unwrap_err(),
            crate::WriteError::UnknownEncoding
        );
    }
}
