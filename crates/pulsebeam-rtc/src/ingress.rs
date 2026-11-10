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

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Bound,
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;

use crate::{
    EcnCodepoint, EncodingInfo, EncodingRetireReason, EncodingStats, Event, PacketFeedbackKind,
    RtpHeader, RtpPacket as ReceivedRtp, SenderReport,
    negotiation::IngressMediaFacts,
    packet::RtpPacket,
    receive_feedback::ReceiveFeedback,
    receive_stream::ReceiveStream,
    rtcp::{self, FeedbackBatch, LifecycleFact, RepairRequest},
    transport::PathEpoch,
};

mod control;
mod events;
mod runtime;

const MAX_RECORDS: usize = 256;
// Both ingress terminal identities and the transport's pre-unprotect fence retain the SSRC.
pub(crate) const SSRC_GUARD_BYTES: usize = std::mem::size_of::<[u32; 2]>();

pub(crate) struct IngressOwner {
    media: BTreeMap<u64, IngressMediaFacts>,
    media_by_mid: BTreeMap<(Box<str>, u64), bool>,
    active: BTreeSet<u64>,
    next_media: Option<u64>,
    encodings_by_media: BTreeMap<u64, BTreeSet<u32>>,
    strict: bool,
    receiver_ssrc: u32,
    receiver_cname: [u8; 32],
    receive_feedback: ReceiveFeedback,
    control_cursor: Option<u32>,
    encodings: BTreeMap<u32, Encoding>,
    terminal_ssrcs: BTreeSet<u32>,
    media_by_ssrc: BTreeMap<u32, u64>,
    declared_primary: BTreeMap<u32, u64>,
    encoding_by_rid: BTreeMap<u64, BTreeMap<Box<str>, u32>>,
    repair_by_ssrc: BTreeMap<u32, u32>,
    mid_extension_id: Option<u8>,
    mid_candidates: BTreeMap<u64, u8>,
    events: VecDeque<QueuedEvent>,
    feedback: VecDeque<control::Feedback>,
    repair: VecDeque<RepairRequest>,
    keyframes: VecDeque<rtcp::KeyframeRequest>,
    max_bytes: usize,
    max_runtime_bytes: usize,
    runtime_bytes: usize,
    guard_bytes: usize,
    runtime_reservations: BTreeMap<u32, usize>,
    queued_media_bytes: usize,
    dropped: u64,
}

pub(crate) struct IngressUpdate {
    new_media: BTreeMap<u64, IngressMediaFacts>,
    removed: BTreeSet<u64>,
    active: BTreeMap<u64, bool>,
    remap: BTreeMap<u32, u64>,
    identities: BTreeSet<u32>,
    next_media: Option<u64>,
    runtime: BTreeMap<u32, Option<usize>>,
    runtime_bytes: usize,
    guard_bytes: usize,
    event_reservation: Option<events::Reservation>,
}

struct QueuedEvent {
    event: Event,
    retained_bytes: usize,
    owned_bytes: usize,
}

struct RepairIdentity {
    ssrc: u32,
    media: u64,
}

struct Encoding {
    media: u64,
    ssrc: Option<u32>,
    repair_ssrc: Option<u32>,
    rid: Option<Box<str>>,
    retired: bool,
    receiver: ReceiveStream,
}

impl IngressOwner {
    pub(crate) fn new(
        media: Box<[IngressMediaFacts]>,
        max_bytes: usize,
        max_runtime_bytes: usize,
        receiver_ssrc: u32,
        receiver_cname: [u8; 32],
        mut receive_feedback: ReceiveFeedback,
    ) -> Result<Self, crate::AcceptError> {
        let media: BTreeMap<_, _> = media
            .into_vec()
            .into_iter()
            .enumerate()
            .map(|(key, facts)| (key as u64, facts))
            .collect();
        let (runtime_reservations, runtime_bytes, guard_bytes) =
            runtime::bootstrap(&media, max_runtime_bytes)?;
        let feedback_bytes = receive_feedback
            .reserve(max_runtime_bytes.saturating_sub(runtime_bytes))
            .ok_or(crate::AcceptError::SessionLimitExceeded)?;
        let runtime_bytes = runtime_bytes
            .checked_add(feedback_bytes)
            .ok_or(crate::AcceptError::SessionLimitExceeded)?;
        let active = media.keys().copied().collect();
        let next_media = media
            .last_key_value()
            .map_or(Some(0), |(&key, _)| key.checked_add(1));
        let media_by_mid = media
            .iter()
            .map(|(&key, facts)| ((facts.mid.clone(), key), true))
            .collect();
        let mut owner = Self {
            media,
            active,
            next_media,
            encodings_by_media: BTreeMap::new(),
            media_by_mid,
            strict: false,
            receiver_ssrc,
            receiver_cname,
            receive_feedback,
            control_cursor: None,
            encodings: BTreeMap::new(),
            terminal_ssrcs: BTreeSet::new(),
            media_by_ssrc: BTreeMap::new(),
            declared_primary: BTreeMap::new(),
            encoding_by_rid: BTreeMap::new(),
            repair_by_ssrc: BTreeMap::new(),
            mid_extension_id: None,
            mid_candidates: BTreeMap::new(),
            events: VecDeque::new(),
            feedback: VecDeque::new(),
            repair: VecDeque::new(),
            keyframes: VecDeque::new(),
            max_bytes,
            max_runtime_bytes,
            runtime_bytes,
            guard_bytes,
            runtime_reservations,
            queued_media_bytes: 0,
            dropped: 0,
        };
        let events = owner
            .prepare_events(owner.runtime_bytes, owner.runtime_reservations.len(), 0)
            .ok_or(crate::AcceptError::SessionLimitExceeded)?;
        owner.runtime_bytes = events.commit(&mut owner.events);
        for key in 0..owner.media.len() {
            owner.index_media(key as u64);
        }
        owner.mid_extension_id = owner.mid_candidates.first_key_value().map(|(_, id)| *id);
        Ok(owner)
    }

    pub(crate) fn set_feedback_profile(
        &mut self,
        profile: PacketFeedbackKind,
    ) -> Result<(), crate::RtpConfigurationError> {
        let base = self
            .runtime_bytes
            .checked_sub(self.receive_feedback.storage_bytes())
            .ok_or(crate::RtpConfigurationError::ConnectionState)?;
        let available = self
            .max_runtime_bytes
            .checked_sub(base)
            .ok_or(crate::RtpConfigurationError::Capacity)?;
        let bytes = self
            .receive_feedback
            .set_profile(profile, available)
            .ok_or(crate::RtpConfigurationError::Capacity)?;
        self.runtime_bytes = base + bytes;
        Ok(())
    }

    pub(crate) fn stage_configuration(
        &self,
        desired: &[(crate::rtp_config::PhysicalBinding, bool)],
        removed: &[u32],
        scope: impl Into<crate::rtp_config::ConfigurationScope>,
    ) -> Result<IngressUpdate, crate::RtpConfigurationError> {
        use crate::{RtpConfigurationError as Error, rtp_config::ConfigurationScope};
        let scope = scope.into();
        let mut update = IngressUpdate {
            new_media: BTreeMap::new(),
            removed: BTreeSet::new(),
            active: if scope == ConfigurationScope::Bootstrap {
                self.media.keys().map(|&key| (key, false)).collect()
            } else {
                BTreeMap::new()
            },
            remap: BTreeMap::new(),
            identities: BTreeSet::new(),
            next_media: self.next_media,
            runtime: BTreeMap::new(),
            runtime_bytes: self.runtime_bytes,
            guard_bytes: self.guard_bytes,
            event_reservation: None,
        };
        for (physical, active) in desired.iter().filter(|(physical, _)| !physical.outbound) {
            let encoding = physical.encoding();
            let identities: Vec<_> = std::iter::once(encoding.primary)
                .chain(encoding.repair)
                .collect();
            if identities
                .iter()
                .any(|ssrc| self.terminal_ssrcs.contains(ssrc))
            {
                return Err(Error::Collision);
            }
            update.identities.extend(identities.iter().copied());
            let owners: BTreeSet<_> = identities
                .iter()
                .filter_map(|ssrc| self.media_by_ssrc.get(ssrc).copied())
                .collect();
            if owners.len() > 1 {
                return Err(Error::Collision);
            }
            let facts = physical.ingress_facts();
            let bootstrap: BTreeSet<_> = self
                .media_by_mid
                .range((facts.mid.clone(), 0)..=(facts.mid.clone(), u64::MAX))
                .filter_map(|((_, key), bootstrap)| bootstrap.then_some(*key))
                .collect();
            if let Some(&owner) = owners.first() {
                let old = self.media.get(&owner).ok_or(Error::ConnectionState)?;
                let known_repair = self
                    .encodings
                    .get(&encoding.primary)
                    .and_then(|source| source.repair_ssrc)
                    .or_else(|| {
                        old.ssrc_repairs
                            .iter()
                            .find(|(_, primary)| *primary == encoding.primary)
                            .map(|(repair, _)| *repair)
                    });
                if self.repair_by_ssrc.contains_key(&encoding.primary)
                    || old
                        .ssrc_repairs
                        .iter()
                        .any(|(repair, _)| *repair == encoding.primary)
                    || known_repair.is_some() && known_repair != encoding.repair
                    || self
                        .encodings
                        .get(&encoding.primary)
                        .and_then(|source| source.rid.as_deref())
                        .is_some_and(|rid| Some(rid) != encoding.rid.as_deref())
                    || encoding.repair.is_some_and(|repair| {
                        self.encodings.contains_key(&repair)
                            || old.ssrcs.contains(&repair)
                                && !old.ssrc_repairs.iter().any(|(known, _)| *known == repair)
                            || self
                                .repair_by_ssrc
                                .get(&repair)
                                .is_some_and(|primary| *primary != encoding.primary)
                            || old.ssrc_repairs.iter().any(|(known, primary)| {
                                *known == repair && *primary != encoding.primary
                            })
                    })
                {
                    return Err(Error::Collision);
                }
                if *old == facts && !bootstrap.contains(&owner) {
                    update.active.insert(owner, *active);
                    continue;
                }
                if self.strict && !bootstrap.contains(&owner)
                    || old.mid != facts.mid
                    || old.kind != facts.kind
                    || old
                        .rtcp
                        .as_ref()
                        .is_some_and(|rtcp| Some(rtcp) != facts.rtcp.as_ref())
                    || !facts.payloads.iter().all(|p| old.payloads.contains(p))
                    || !facts.repairs.iter().all(|p| old.repairs.contains(p))
                    || !facts.extensions.iter().all(|p| old.extensions.contains(p))
                {
                    return Err(Error::Collision);
                }
            }
            for owner in bootstrap {
                update.active.remove(&owner);
                update.removed.insert(owner);
            }
            let key = update.next_media.ok_or(Error::Capacity)?;
            update.next_media = key.checked_add(1);
            if let Some(observed) = self.encodings.get(&encoding.primary) {
                if !owners.contains(&observed.media) {
                    return Err(Error::Collision);
                }
                update.remap.insert(encoding.primary, key);
            }
            update.new_media.insert(key, facts);
            update.active.insert(key, *active);
        }
        for primary in removed {
            let key = self.declared_primary.get(primary).ok_or(Error::Invalid)?;
            if update.active.contains_key(key) {
                return Err(Error::Invalid);
            }
            update.removed.insert(*key);
        }
        if scope == ConfigurationScope::Complete
            || !self.strict && scope != ConfigurationScope::Bootstrap
        {
            update.removed.extend(
                self.media
                    .keys()
                    .filter(|key| !update.active.contains_key(key))
                    .copied(),
            );
        }
        self.stage_runtime(&mut update)?;
        Ok(update)
    }

    pub(crate) fn retiring_ssrcs(&self, update: &IngressUpdate) -> BTreeSet<u32> {
        let mut retired = BTreeSet::new();
        for key in &update.removed {
            let Some(facts) = self.media.get(key) else {
                continue;
            };
            if let Some(owned) = self.encodings_by_media.get(key) {
                for primary in owned {
                    if update.remap.contains_key(primary) {
                        continue;
                    }
                    if let Some(encoding) = self
                        .encodings
                        .get(primary)
                        .filter(|encoding| encoding.media == *key)
                    {
                        retired.insert(*primary);
                        retired.extend(encoding.repair_ssrc.or_else(|| {
                            facts
                                .ssrc_repairs
                                .iter()
                                .find(|(_, source)| source == primary)
                                .map(|(repair, _)| *repair)
                        }));
                    }
                }
            }
            retired.extend(
                facts
                    .ssrcs
                    .iter()
                    .copied()
                    .chain(facts.ssrc_repairs.iter().map(|(repair, _)| *repair))
                    .filter(|ssrc| !update.identities.contains(ssrc)),
            );
        }
        retired
    }

    pub(crate) fn commit_configuration(&mut self, update: IngressUpdate) -> Vec<u32> {
        if let Some(events) = update.event_reservation {
            let total = events.commit(&mut self.events);
            debug_assert_eq!(total, update.runtime_bytes);
        }
        for &key in update.removed.iter().chain(update.active.keys()) {
            self.unindex_media(key);
        }
        self.media_by_mid.extend(
            update
                .new_media
                .iter()
                .map(|(&key, facts)| ((facts.mid.clone(), key), false)),
        );
        self.media.extend(update.new_media);
        for (ssrc, media) in update.remap {
            if let Some(encoding) = self.encodings.get_mut(&ssrc) {
                let old = encoding.media;
                encoding.media = media;
                if let Some(owned) = self.encodings_by_media.get_mut(&old) {
                    owned.remove(&ssrc);
                }
                self.encodings_by_media
                    .entry(media)
                    .or_default()
                    .insert(ssrc);
            }
        }
        let mut retired = BTreeSet::new();
        for key in update.removed {
            self.active.remove(&key);
            self.encoding_by_rid.remove(&key);
            if let Some(encodings) = self.encodings_by_media.remove(&key) {
                for ssrc in encodings {
                    if self
                        .encodings
                        .get(&ssrc)
                        .is_some_and(|encoding| encoding.media == key)
                        && let Ok(identities) = self.retire(ssrc)
                    {
                        retired.extend(identities.into_iter().flatten());
                    }
                }
            }
            if let Some(facts) = self.media.remove(&key) {
                self.media_by_mid.remove(&(facts.mid.clone(), key));
                for ssrc in facts
                    .ssrcs
                    .iter()
                    .copied()
                    .chain(facts.ssrc_repairs.iter().map(|(repair, _)| *repair))
                {
                    if !update.identities.contains(&ssrc) {
                        self.terminal_ssrcs.insert(ssrc);
                        retired.insert(ssrc);
                    }
                }
            }
        }
        for (key, active) in update.active {
            self.set_media_active(key, active);
        }
        for (ssrc, charge) in update.runtime {
            if let Some(charge) = charge {
                self.runtime_reservations.insert(ssrc, charge);
            } else {
                self.runtime_reservations.remove(&ssrc);
            }
        }
        self.runtime_bytes = update.runtime_bytes;
        self.guard_bytes = update.guard_bytes;
        self.next_media = update.next_media;
        self.strict = true;
        self.mid_extension_id = self.mid_candidates.first_key_value().map(|(_, id)| *id);
        retired.into_iter().collect()
    }

    pub(crate) fn set_encoding_active(
        &mut self,
        primary: u32,
        active: bool,
    ) -> Result<(), crate::RtpConfigurationError> {
        let key = *self
            .declared_primary
            .get(&primary)
            .ok_or(crate::RtpConfigurationError::ConnectionState)?;
        self.unindex_media(key);
        self.set_media_active(key, active);
        Ok(())
    }

    fn set_media_active(&mut self, key: u64, active: bool) {
        if active {
            self.active.insert(key);
        } else {
            self.active.remove(&key);
        }
        if let Some(owned) = self.encodings_by_media.get(&key) {
            for ssrc in owned {
                if let Some(encoding) = self.encodings.get_mut(ssrc) {
                    encoding.retired = !active;
                }
            }
        }
        self.index_media(key);
    }

    fn remove_media_owner(index: &mut BTreeMap<u32, u64>, ssrc: u32, media: u64) {
        if index.get(&ssrc) == Some(&media) {
            index.remove(&ssrc);
        }
    }

    fn unindex_media(&mut self, key: u64) {
        self.mid_candidates.remove(&key);
        if let Some(rids) = self.encoding_by_rid.get_mut(&key) {
            rids.clear();
        }
        if let Some(facts) = self.media.get(&key) {
            for ssrc in facts
                .ssrcs
                .iter()
                .copied()
                .chain(facts.ssrc_repairs.iter().map(|(repair, _)| *repair))
            {
                Self::remove_media_owner(&mut self.media_by_ssrc, ssrc, key);
                Self::remove_media_owner(&mut self.declared_primary, ssrc, key);
            }
        }
        if let Some(owned) = self.encodings_by_media.get(&key) {
            for ssrc in owned {
                if let Some(encoding) = self.encodings.get(ssrc) {
                    Self::remove_media_owner(&mut self.media_by_ssrc, *ssrc, key);
                    if let Some(repair) = encoding.repair_ssrc {
                        Self::remove_media_owner(&mut self.media_by_ssrc, repair, key);
                        if self.repair_by_ssrc.get(&repair) == Some(ssrc) {
                            self.repair_by_ssrc.remove(&repair);
                        }
                    }
                }
            }
        }
    }

    fn index_media(&mut self, key: u64) {
        let Some(facts) = self.media.get(&key) else {
            return;
        };
        self.encodings_by_media.entry(key).or_default();
        self.encoding_by_rid.entry(key).or_default();
        for ssrc in facts
            .ssrcs
            .iter()
            .copied()
            .chain(facts.ssrc_repairs.iter().map(|(repair, _)| *repair))
        {
            if !self.terminal_ssrcs.contains(&ssrc) {
                self.media_by_ssrc.insert(ssrc, key);
            }
        }
        for &primary in &facts.ssrcs {
            if !facts
                .ssrc_repairs
                .iter()
                .any(|(repair, _)| *repair == primary)
            {
                self.declared_primary.insert(primary, key);
            }
        }
        if self.active.contains(&key)
            && let Some(id) = facts
                .extensions
                .iter()
                .find_map(|(id, uri)| (uri.as_ref() == crate::MID_EXTENSION).then_some(*id))
        {
            self.mid_candidates.insert(key, id);
        }
        if let Some(owned) = self.encodings_by_media.get(&key) {
            for &ssrc in owned {
                let Some(encoding) = self.encodings.get(&ssrc) else {
                    continue;
                };
                if let Some(rid) = &encoding.rid {
                    self.encoding_by_rid
                        .entry(key)
                        .or_default()
                        .insert(rid.clone(), ssrc);
                }
                self.media_by_ssrc.insert(ssrc, key);
                if let Some(repair) = encoding.repair_ssrc {
                    self.media_by_ssrc.insert(repair, key);
                    self.repair_by_ssrc.insert(repair, ssrc);
                }
            }
        }
    }

    pub(crate) const fn control_ssrc(&self) -> u32 {
        self.receiver_ssrc
    }

    // Commit can temporarily unindex identities. Stable owners retain every
    // issued SSRC in the live index or the permanent terminal set.
    pub(crate) fn has_issued_ssrc(&self, ssrc: u32) -> bool {
        self.media_by_ssrc.contains_key(&ssrc) || self.terminal_ssrcs.contains(&ssrc)
    }

    pub(crate) fn accept_rtp(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        ecn: Option<EcnCodepoint>,
    ) {
        self.accept_packet(arrival, bytes, None, ecn);
    }

    fn accept_packet(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        repair: Option<RepairIdentity>,
        ecn: Option<EcnCodepoint>,
    ) {
        let backing_capacity = bytes.capacity();
        let bytes = Bytes::from(bytes);
        let Ok(packet) = RtpPacket::parse(&bytes) else {
            self.drop();
            return;
        };
        let Some((media, clock_rate, rid)) = self.resolve(&packet) else {
            self.drop();
            return;
        };
        if repair.as_ref().is_some_and(|repair| repair.media != media) {
            self.drop();
            return;
        }
        let facts = &self.media[&media];
        // Typed facts bind one primary SSRC to its RID. SDP RID lists do not.
        let rid = rid.or_else(
            || match (self.strict, facts.ssrcs.as_ref(), facts.rids.as_ref()) {
                (true, [ssrc], [rid]) if *ssrc == packet.ssrc() => Some(rid.as_ref()),
                _ => None,
            },
        );
        let primary_type = facts
            .repairs
            .iter()
            .find(|(pt, _)| *pt == packet.payload_type())
            .map_or_else(|| packet.payload_type(), |(_, primary)| *primary);
        if repair.is_none() && facts.feedback_payloads.contains(&primary_type) {
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
        if bytes.len() > self.max_bytes {
            self.drop();
            return;
        }
        if let Some((_, primary_type)) = self.media[&media]
            .repairs
            .iter()
            .find(|(pt, _)| *pt == packet.payload_type())
        {
            let primary_ssrc = self.media[&media]
                .ssrc_repairs
                .iter()
                .find(|(rtx, _)| *rtx == packet.ssrc())
                .map(|(_, primary)| *primary)
                .or_else(|| {
                    rid.and_then(|rid| self.encoding_by_rid.get(&media)?.get(rid).copied())
                });
            let primary_ssrc =
                primary_ssrc.or_else(|| self.repair_by_ssrc.get(&packet.ssrc()).copied());
            let Some(primary_ssrc) = primary_ssrc else {
                self.drop();
                return;
            };
            if self.encodings.contains_key(&packet.ssrc())
                || self.declared_primary.contains_key(&packet.ssrc())
                || self
                    .repair_by_ssrc
                    .get(&packet.ssrc())
                    .is_some_and(|primary| *primary != primary_ssrc)
                || rid
                    .and_then(|rid| self.encoding_by_rid.get(&media)?.get(rid))
                    .is_some_and(|primary| *primary != primary_ssrc)
            {
                self.drop();
                return;
            }
            let Some(sequence) = packet.payload().get(..2) else {
                self.drop();
                return;
            };
            if repair.is_some() {
                self.drop();
                return;
            }
            let mut normalized = bytes[..packet.payload_range().start].to_vec();
            normalized[0] &= !0x20;
            normalized[1] = (normalized[1] & 0x80) | primary_type;
            normalized[2..4].copy_from_slice(sequence);
            normalized[8..12].copy_from_slice(&primary_ssrc.to_be_bytes());
            normalized.extend_from_slice(&packet.payload()[2..]);
            let retained = queued_backing_bytes(
                &packet,
                &self.media[&media],
                normalized.capacity(),
                packet.payload().len() - 2,
            );
            if retained > self.max_bytes.saturating_sub(self.queued_media_bytes) {
                self.drop();
                return;
            }
            if let Some(encoding) = self.encodings.get(&primary_ssrc)
                && (encoding.retired
                    || encoding
                        .repair_ssrc
                        .is_some_and(|known| known != packet.ssrc()))
            {
                self.drop();
                return;
            }
            self.accept_packet(
                arrival,
                normalized,
                Some(RepairIdentity {
                    ssrc: packet.ssrc(),
                    media,
                }),
                None,
            );
            return;
        }
        // A negotiated repair SSRC cannot create an unrelated primary encoding.
        if self.media[&media]
            .ssrc_repairs
            .iter()
            .any(|(rtx, _)| *rtx == packet.ssrc())
            || self.repair_by_ssrc.contains_key(&packet.ssrc())
        {
            self.drop();
            return;
        }
        let retained_bytes = queued_backing_bytes(
            &packet,
            &self.media[&media],
            backing_capacity,
            packet.payload().len(),
        );
        let Some(queued_total) = self
            .queued_media_bytes
            .checked_add(retained_bytes)
            .filter(|bytes| *bytes <= self.max_bytes)
        else {
            self.drop();
            return;
        };
        let encoding = self.find_encoding(media, packet.ssrc(), rid);
        let discovery = encoding.is_none();
        let index = encoding.unwrap_or_else(|| packet.ssrc());
        let state = self.encodings.get(&index);
        // MID/RID may stop repeating after SSRC acknowledgment. Preserve the
        // authenticated association, but never hide an explicit RID conflict.
        if state.is_some_and(|state| {
            state.retired
                || state.ssrc.is_some_and(|known| known != packet.ssrc())
                || state
                    .rid
                    .as_deref()
                    .zip(rid)
                    .is_some_and(|(known, signaled)| known != signaled)
                || state
                    .repair_ssrc
                    .zip(repair.as_ref())
                    .is_some_and(|(known, repair)| known != repair.ssrc)
        }) {
            self.drop();
            return;
        }
        let event_rid = state.and_then(|state| state.rid.as_deref()).or(rid);
        let strings = self.media[&media]
            .mid
            .len()
            .checked_add(event_rid.map_or(0, str::len))
            .and_then(|bytes| {
                if discovery {
                    bytes
                        .checked_add(self.media[&media].mid.len())?
                        .checked_add(rid.map_or(0, str::len))
                } else {
                    Some(bytes)
                }
            });
        let Some(extensions) =
            strings.and_then(|strings| self.prepare_extensions(media, &packet, &bytes, strings))
        else {
            self.drop();
            return;
        };
        let Some(rtp) = QueuedEvent::new(
            Event::Rtp {
                mid: Arc::from(self.media[&media].mid.as_ref()),
                rid: event_rid.map(Arc::from),
                packet: ReceivedRtp {
                    arrival,
                    header: RtpHeader {
                        sequence: packet.sequence(),
                        timestamp: packet.timestamp(),
                        ssrc: packet.ssrc(),
                        payload_type: packet.payload_type(),
                        marker: packet.marker(),
                    },
                    payload: bytes.slice(packet.payload_range()),
                    extensions,
                },
            },
            retained_bytes,
        ) else {
            self.drop();
            return;
        };
        let notice = if discovery {
            let Some(notice) = QueuedEvent::new(
                Event::EncodingDiscovered(EncodingInfo {
                    mid: Arc::from(self.media[&media].mid.as_ref()),
                    ssrc: packet.ssrc(),
                    kind: self.media[&media].kind,
                    rid: rid.map(Arc::from),
                    payloads: self.media[&media].payloads.clone(),
                }),
                0,
            ) else {
                self.drop();
                return;
            };
            Some(notice)
        } else {
            None
        };
        let Some(owned_bytes) = rtp
            .owned_bytes
            .checked_add(notice.as_ref().map_or(0, |notice| notice.owned_bytes))
        else {
            self.drop();
            return;
        };
        let Some(reservation) = self.prepare_runtime(
            index,
            media,
            discovery,
            repair.as_ref().map(|repair| repair.ssrc),
            owned_bytes,
        ) else {
            self.drop();
            return;
        };
        let mut new_encoding = discovery.then(|| Encoding {
            media,
            ssrc: Some(packet.ssrc()),
            repair_ssrc: None,
            rid: None,
            retired: false,
            receiver: ReceiveStream::new(
                clock_rate,
                self.media[&media]
                    .nack_payloads
                    .contains(&packet.payload_type()),
            ),
        });
        let Some(state) = new_encoding
            .as_mut()
            .or_else(|| self.encodings.get_mut(&index))
        else {
            self.drop();
            return;
        };
        if !state.receiver.accept(
            packet.sequence(),
            packet.timestamp(),
            arrival,
            repair.is_some(),
        ) {
            self.drop();
            return;
        }
        let learned_rid = state
            .rid
            .is_none()
            .then(|| rid.map(Box::<str>::from))
            .flatten();
        state.ssrc = Some(packet.ssrc());
        if let Some(rid) = &learned_rid {
            state.rid = Some(rid.clone());
        }
        if let Some(repair) = &repair {
            state.repair_ssrc = Some(repair.ssrc);
        }
        reservation.commit(self);
        if let Some(state) = new_encoding {
            self.encodings.insert(index, state);
            self.encodings_by_media
                .entry(media)
                .or_default()
                .insert(index);
            self.media_by_ssrc.insert(packet.ssrc(), media);
        }
        if let Some(rid) = learned_rid {
            self.encoding_by_rid
                .entry(media)
                .or_default()
                .insert(rid, index);
        }
        if let Some(repair) = repair {
            self.repair_by_ssrc.insert(repair.ssrc, index);
            self.media_by_ssrc.insert(repair.ssrc, media);
        }
        if let Some(notice) = notice {
            self.push_event(notice);
        }
        self.queued_media_bytes = queued_total;
        self.push_event(rtp);
    }

    pub(crate) fn accept_rtcp(
        &mut self,
        arrival: Instant,
        bytes: Vec<u8>,
        path_epoch: PathEpoch,
        mode: Option<PacketFeedbackKind>,
        _smoothed_rtt: Option<std::time::Duration>,
    ) -> (Vec<rtcp::ReportReceipt>, Vec<u32>) {
        if bytes.len() > self.max_bytes {
            self.drop();
            return (Vec::new(), Vec::new());
        }
        let Ok(parsed) = rtcp::parse(&bytes, arrival, path_epoch, mode) else {
            self.drop();
            return (Vec::new(), Vec::new());
        };
        let mut retired = Vec::new();
        for fact in &parsed.lifecycle {
            if let LifecycleFact::Bye { ssrc } = *fact
                && let Ok(identities) =
                    self.retire_with_reason(ssrc, EncodingRetireReason::RemoteBye)
            {
                retired.extend(identities.into_iter().flatten());
            }
        }
        let notices = parsed
            .lifecycle
            .iter()
            .filter(|fact| matches!(fact, LifecycleFact::SenderReport { .. }))
            .count();
        let Some(control) =
            self.prepare_control(parsed.feedback, parsed.repair, parsed.keyframes, notices)
        else {
            self.drop();
            return (Vec::new(), retired);
        };
        control.commit(self);
        for fact in parsed.lifecycle {
            if !matches!(fact, LifecycleFact::Bye { .. }) {
                self.apply_rtcp_fact(arrival, fact);
            }
        }
        (parsed.receipts, retired)
    }

    pub(crate) fn poll_feedback(&mut self) -> Option<FeedbackBatch> {
        let feedback = self.feedback.pop_front()?;
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(feedback.owned_bytes)
            .saturating_sub(control::release_empty(&mut self.feedback));
        Some(feedback.batch)
    }

    #[cfg(test)]
    pub(crate) fn test_runtime_bytes(&self) -> usize {
        assert_eq!(
            self.queued_media_bytes,
            self.events
                .iter()
                .map(|queued| queued.retained_bytes)
                .sum::<usize>()
        );
        let issued: BTreeSet<_> = self
            .media_by_ssrc
            .keys()
            .copied()
            .chain(self.terminal_ssrcs.iter().copied())
            .collect();
        assert_eq!(self.guard_bytes, issued.len() * SSRC_GUARD_BYTES);
        assert!(self.events.len() + self.runtime_reservations.len() <= self.events.capacity());
        for (ssrc, encoding) in &self.encodings {
            let facts = &self.media[&encoding.media];
            let repair = encoding
                .repair_ssrc
                .map_or(0, |repair| runtime::repair_owner_bytes(facts, repair));
            assert!(
                self.runtime_reservations[ssrc]
                    >= runtime::bytes(facts, self.strict).unwrap() + repair
            );
        }
        for (repair, primary) in &self.repair_by_ssrc {
            assert_eq!(self.encodings[primary].repair_ssrc, Some(*repair));
        }
        assert_eq!(
            self.runtime_bytes,
            self.test_media_bytes()
                + self.runtime_reservations.values().sum::<usize>()
                + self
                    .events
                    .iter()
                    .map(|queued| {
                        assert_eq!(
                            queued.owned_bytes,
                            events::owned_bytes(&queued.event).unwrap()
                        );
                        queued.owned_bytes
                    })
                    .sum::<usize>()
                + self.guard_bytes
                + self.event_backing_bytes()
                + self.control_backing_bytes()
                + self.receive_feedback.storage_bytes()
                + self
                    .feedback
                    .iter()
                    .map(|feedback| {
                        assert_eq!(
                            feedback.owned_bytes,
                            control::feedback_bytes(&feedback.batch).unwrap()
                        );
                        feedback.owned_bytes
                    })
                    .sum::<usize>()
        );
        self.runtime_bytes
    }

    #[cfg(test)]
    pub(crate) fn test_terminal_guard_bytes(&self) -> usize {
        self.terminal_ssrcs.len() * std::mem::size_of::<u32>()
    }

    #[cfg(test)]
    pub(crate) fn test_limit_runtime_to_current_usage(&mut self) -> usize {
        let bytes = self.test_runtime_bytes();
        self.max_runtime_bytes = bytes;
        bytes
    }

    #[cfg(test)]
    pub(crate) fn test_media_bytes(&self) -> usize {
        assert_eq!(self.encodings_by_media.len(), self.media.len());
        assert_eq!(self.encoding_by_rid.len(), self.media.len());
        for key in self.media.keys() {
            assert!(self.encodings_by_media.contains_key(key));
            assert!(self.encoding_by_rid.contains_key(key));
        }
        self.media
            .values()
            .try_fold(0usize, |total, facts| {
                total.checked_add(runtime::media_bytes(facts)?)
            })
            .unwrap()
    }

    #[cfg(test)]
    pub(crate) fn test_queue_feedback(&mut self, feedback: FeedbackBatch) {
        self.prepare_control(vec![feedback], Vec::new(), Vec::new(), 0)
            .unwrap()
            .commit(self);
    }

    pub(crate) fn poll_keyframes(&mut self) -> impl Iterator<Item = rtcp::KeyframeRequest> + '_ {
        std::iter::from_fn(move || self.poll_keyframe()).take(MAX_RECORDS)
    }

    fn poll_keyframe(&mut self) -> Option<rtcp::KeyframeRequest> {
        let request = self.keyframes.pop_front()?;
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(control::release_empty(&mut self.keyframes));
        Some(request)
    }

    pub(crate) fn poll_repair(&mut self) -> Option<RepairRequest> {
        let request = self.repair.pop_front()?;
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(control::release_empty(&mut self.repair));
        Some(request)
    }

    pub(crate) fn has_pending_output(&self) -> bool {
        !self.events.is_empty()
            || !self.feedback.is_empty()
            || !self.keyframes.is_empty()
            || !self.repair.is_empty()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        for _ in 0..MAX_RECORDS {
            let QueuedEvent {
                event,
                retained_bytes,
                owned_bytes,
            } = self.events.pop_front()?;
            self.queued_media_bytes = self.queued_media_bytes.saturating_sub(retained_bytes);
            self.runtime_bytes = self.runtime_bytes.saturating_sub(owned_bytes);
            self.release_idle_events();
            let source = match &event {
                Event::Rtp { packet, .. } => Some(packet.header.ssrc),
                Event::EncodingDiscovered(info) => Some(info.ssrc),
                Event::SenderReport(report) => Some(report.ssrc),
                _ => None,
            };
            if source.is_some_and(|ssrc| self.terminal_ssrcs.contains(&ssrc)) {
                continue;
            }
            if let Event::Rtp { packet, .. } = &event
                && !self
                    .encodings
                    .get(&packet.header.ssrc)
                    .and_then(|encoding| self.media.get(&encoding.media))
                    .is_some_and(|facts| {
                        facts
                            .payloads
                            .iter()
                            .any(|payload| payload.payload_type == packet.header.payload_type)
                    })
            {
                continue;
            }
            return Some(event);
        }
        None
    }

    pub(crate) fn retire(&mut self, ssrc: u32) -> Result<[Option<u32>; 2], crate::WriteError> {
        self.retire_with_reason(ssrc, EncodingRetireReason::Retired)
    }

    fn retire_with_reason(
        &mut self,
        ssrc: u32,
        reason: EncodingRetireReason,
    ) -> Result<[Option<u32>; 2], crate::WriteError> {
        let Some(encoding) = self.encodings.remove(&ssrc) else {
            return if self.terminal_ssrcs.contains(&ssrc) {
                Ok([Some(ssrc), None])
            } else {
                Err(crate::WriteError::UnknownEncoding)
            };
        };
        if let Some(encodings) = self.encodings_by_media.get_mut(&encoding.media) {
            encodings.remove(&ssrc);
        }
        let repair = encoding.repair_ssrc.or_else(|| {
            self.media[&encoding.media]
                .ssrc_repairs
                .iter()
                .find(|(_, primary)| *primary == ssrc)
                .map(|(repair, _)| *repair)
        });
        for identity in [Some(ssrc), repair].into_iter().flatten() {
            self.terminal_ssrcs.insert(identity);
            self.media_by_ssrc.remove(&identity);
            self.repair_by_ssrc.remove(&identity);
        }
        if let Some(rid) = encoding.rid
            && let Some(rids) = self.encoding_by_rid.get_mut(&encoding.media)
        {
            rids.remove(&rid);
        }
        self.release_runtime(ssrc);
        // Queued old packets remain charged until bounded polling drops them.
        // Admission and delivery are already fenced by terminal_ssrcs.
        self.push_event(QueuedEvent {
            event: Event::EncodingRetired { ssrc, reason },
            retained_bytes: 0,
            owned_bytes: 0,
        });
        Ok([Some(ssrc), repair])
    }

    pub(crate) fn compound_control(&self, packet: Vec<u8>) -> Option<Vec<u8>> {
        // Compound RTCP is valid even when reduced-size feedback is permitted.
        rtcp::compound(packet, self.receiver_ssrc, &self.receiver_cname)
    }

    pub(crate) fn keyframe_request(&self, ssrc: u32) -> Result<[u8; 12], crate::WriteError> {
        if self.terminal_ssrcs.contains(&ssrc) {
            return Err(crate::WriteError::UnknownEncoding);
        }
        let permitted = match self.encodings.get(&ssrc) {
            Some(encoding) => !encoding.retired,
            None => self
                .media_by_ssrc
                .get(&ssrc)
                .filter(|key| self.active.contains(key))
                .and_then(|key| self.media.get(key))
                .is_some_and(|media| {
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
        self.events = VecDeque::new();
        self.feedback = VecDeque::new();
        self.receive_feedback.abort();
        self.repair = VecDeque::new();
        self.keyframes = VecDeque::new();
        self.queued_media_bytes = 0;
        self.encodings.clear();
        self.media.clear();
        self.media_by_mid.clear();
        self.active.clear();
        self.encodings_by_media.clear();
        self.media_by_ssrc.clear();
        self.declared_primary.clear();
        self.mid_candidates.clear();
        self.encoding_by_rid.clear();
        self.repair_by_ssrc.clear();
        self.terminal_ssrcs.clear();
        self.runtime_reservations.clear();
        self.runtime_bytes = 0;
        self.guard_bytes = 0;
        self.mid_extension_id = None;
    }

    pub(crate) fn stats(&self) -> Vec<EncodingStats> {
        self.encodings
            .values()
            .map(|encoding| EncodingStats {
                mid: self.media[&encoding.media].mid.clone().into(),
                ssrc: encoding.ssrc,
                kind: self.media[&encoding.media].kind,
                received_packets: encoding.receiver.received(),
                retired: encoding.retired,
            })
            .collect()
    }

    pub(crate) fn poll_control(&mut self, now: Instant) -> Option<Vec<u8>> {
        if let Some(packet) = self.receive_feedback.poll(now, self.receiver_ssrc) {
            return self.compound_control(packet);
        }
        for _ in 0..self.encodings.len().min(MAX_RECORDS) {
            let lower = self
                .control_cursor
                .map_or(Bound::Unbounded, Bound::Excluded);
            let key = self
                .encodings
                .range((lower, Bound::Unbounded))
                .next()
                .or_else(|| self.encodings.first_key_value())
                .map(|(&key, _)| key)?;
            self.control_cursor = Some(key);
            let encoding = self.encodings.get_mut(&key)?;
            if !encoding.retired
                && let Some(ssrc) = encoding.ssrc
                && let Some(packet) = encoding
                    .receiver
                    .poll_control(now, self.receiver_ssrc, ssrc)
            {
                return self.compound_control(packet);
            }
        }
        None
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.encodings
            .values()
            .filter(|encoding| !encoding.retired)
            .filter_map(|encoding| encoding.receiver.next_deadline())
            .chain(self.receive_feedback.deadline())
            .min()
    }

    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }

    fn resolve<'a>(&self, packet: &'a RtpPacket<'a>) -> Option<(u64, u32, Option<&'a str>)> {
        if self.terminal_ssrcs.contains(&packet.ssrc()) {
            return None;
        }
        let mid = packet
            .extensions()
            .ok()?
            .find(|value| Some(value.id()) == self.mid_extension_id);
        let by_mid = mid
            .map(|value| std::str::from_utf8(value.value()))
            .transpose()
            .ok()?;
        let by_ssrc = self
            .media_by_ssrc
            .get(&packet.ssrc())
            .copied()
            .filter(|index| self.active.contains(index));
        if self.strict && by_ssrc.is_none() {
            return None;
        }
        let resolve = |index: u64| {
            let media = &self.media[&index];
            if !self.active.contains(&index) || by_mid.is_some_and(|mid| media.mid.as_ref() != mid)
            {
                return None;
            }
            media
                .payloads
                .iter()
                .find(|payload| {
                    payload.payload_type == packet.payload_type()
                        || media.repairs.iter().any(|(repair, primary)| {
                            *repair == packet.payload_type() && *primary == payload.payload_type
                        })
                })
                .map(|payload| (index, payload.clock_rate))
        };
        let (index, clock_rate) = if let Some(index) = by_ssrc {
            resolve(index)?
        } else {
            // Bootstrap alone permits discovery without an explicit SSRC.
            let mut candidates = self.media.keys().copied().filter_map(resolve);
            let selected = candidates.next()?;
            if candidates.next().is_some() {
                return None;
            }
            selected
        };
        let facts = &self.media[&index];
        let repair = facts
            .repairs
            .iter()
            .any(|(pt, _)| *pt == packet.payload_type());
        let rid = extension_value(
            packet,
            &facts.extensions,
            if repair {
                crate::REPAIRED_RID_EXTENSION
            } else {
                crate::RID_EXTENSION
            },
        );
        if self.strict
            && rid.is_some_and(|rid| !facts.rids.iter().any(|known| known.as_ref() == rid))
        {
            return None;
        }
        Some((index, clock_rate, rid))
    }

    fn find_encoding(&self, media: u64, ssrc: u32, rid: Option<&str>) -> Option<u32> {
        self.encodings
            .get(&ssrc)
            .filter(|encoding| encoding.media == media)
            .map(|_| ssrc)
            .or_else(|| self.encoding_by_rid.get(&media)?.get(rid?).copied())
    }

    fn drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    fn apply_rtcp_fact(&mut self, arrival: Instant, fact: LifecycleFact) {
        match fact {
            LifecycleFact::Cname { .. } | LifecycleFact::Bye { .. } => {}
            LifecycleFact::SenderReport {
                ssrc,
                ntp_seconds,
                ntp_fraction,
                rtp_timestamp,
            } => {
                if let Some(encoding) = self
                    .encodings
                    .get_mut(&ssrc)
                    .filter(|encoding| !encoding.retired)
                {
                    encoding
                        .receiver
                        .sender_report(ntp_seconds, ntp_fraction, arrival);
                }
                self.push_event(QueuedEvent {
                    event: Event::SenderReport(SenderReport {
                        arrival,
                        ssrc,
                        ntp_seconds,
                        ntp_fraction,
                        rtp_timestamp,
                    }),
                    retained_bytes: 0,
                    owned_bytes: 0,
                });
            }
        }
    }
}

fn queued_backing_bytes(
    packet: &RtpPacket<'_>,
    facts: &IngressMediaFacts,
    capacity: usize,
    payload_len: usize,
) -> usize {
    let retained = payload_len != 0
        || packet.extensions().ok().into_iter().flatten().any(|value| {
            !value.value().is_empty() && facts.extensions.iter().any(|(id, _)| *id == value.id())
        });
    if retained { capacity } else { 0 }
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
    use crate::rtcp::RtcpPacketExt;

    fn rid_ingress(now: Instant) -> IngressOwner {
        IngressOwner::new(
            Box::new([IngressMediaFacts {
                kind: crate::MediaKind::Video,
                rtcp: None,
                mid: "v".into(),
                payloads: Box::new([crate::RtpPayloadInfo {
                    payload_type: 96,
                    name: "h264".into(),
                    clock_rate: 90_000,
                }]),
                feedback_payloads: Box::new([96]),
                repairs: Box::new([(97, 96)]),
                nack_payloads: Box::new([96]),
                ssrc_repairs: Box::new([]),
                ssrcs: Box::new([]),
                rids: Box::new(["q".into(), "h".into()]),
                extensions: Box::new([
                    (1, "urn:ietf:params:rtp-hdrext:sdes:mid".into()),
                    (2, "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".into()),
                    (3, crate::REPAIRED_RID_EXTENSION.into()),
                ]),
            }]),
            4096,
            crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            99,
            [b'r'; 32],
            ReceiveFeedback::new(None, now),
        )
        .unwrap()
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

    fn repair_packet(ssrc: u32, sequence: u16, original: u16, rid: u8) -> Vec<u8> {
        let mut bytes = rid_packet(ssrc, sequence, Some(rid));
        bytes[1] = 97;
        bytes[18] = 0x30;
        bytes.splice(20..20, original.to_be_bytes());
        bytes
    }

    fn metadata_bytes(ingress: &IngressOwner, wire: &[u8], rid: Option<&str>) -> usize {
        let packet = RtpPacket::parse(wire).unwrap();
        let (media, _, _) = ingress.resolve(&packet).unwrap();
        let uris: Vec<_> = packet
            .extensions()
            .unwrap()
            .filter_map(|value| {
                ingress.media[&media]
                    .extensions
                    .iter()
                    .find(|(id, _)| *id == value.id())
                    .map(|(_, uri)| uri.as_ref())
            })
            .collect();
        let mut native = Vec::<crate::RtpExtension>::new();
        native.try_reserve_exact(uris.len()).unwrap();
        ingress.media[&media].mid.len()
            + rid.map_or(0, str::len)
            + native.capacity() * std::mem::size_of::<crate::RtpExtension>()
            + uris.iter().map(|uri| uri.len()).sum::<usize>()
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
        ingress.accept_rtp(now, rid_packet(11, 0, None), None);
        assert_eq!(received_rids(&mut ingress), [(11, None)]);
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
        for ssrc in [11, 22] {
            ingress.accept_rtp(now, repair_packet(ssrc, 50, 3, b'q'), None);
            assert!(received_rids(&mut ingress).is_empty());
        }
        assert!(ingress.repair_by_ssrc.is_empty());
        assert!(ingress.encodings[&11].repair_ssrc.is_none());
        ingress.accept_rtp(now, rid_packet(11, 3, None), None);
        ingress.accept_rtp(now, rid_packet(22, 102, None), None);
        assert_eq!(
            received_rids(&mut ingress),
            [(11, Some("q".into())), (22, Some("h".into()))]
        );
        let full = ingress.test_runtime_bytes();
        ingress.max_runtime_bytes = full;
        ingress.accept_rtp(now, repair_packet(33, 50, 4, b'q'), None);
        assert!(received_rids(&mut ingress).is_empty());
        assert_eq!(ingress.test_runtime_bytes(), full);
        assert!(!ingress.media_by_ssrc.contains_key(&33));
        assert!(ingress.encodings[&11].repair_ssrc.is_none());
        let guard = SSRC_GUARD_BYTES;
        let repair_credit = guard + std::mem::size_of::<(u32, u64)>();
        let repair_metadata = metadata_bytes(&ingress, &repair_packet(33, 50, 4, b'q'), Some("q"));
        ingress.max_runtime_bytes = full + repair_credit + repair_metadata - 1;
        ingress.accept_rtp(now, repair_packet(33, 50, 4, b'q'), None);
        assert_eq!(ingress.test_runtime_bytes(), full);
        assert_eq!(ingress.stats()[0].received_packets, 2);
        assert!(ingress.encodings[&11].repair_ssrc.is_none());
        assert!(!ingress.media_by_ssrc.contains_key(&33));
        ingress.max_runtime_bytes += 1;
        ingress.accept_rtp(now, repair_packet(33, 49, 3, b'q'), None);
        assert_eq!(ingress.test_runtime_bytes(), full);
        assert!(ingress.encodings[&11].repair_ssrc.is_none());
        assert!(!ingress.media_by_ssrc.contains_key(&33));
        for (sequence, original) in [(50, 4), (51, 5)] {
            let mut repair = repair_packet(33, sequence, original, b'q');
            repair.reserve_exact(ingress.max_bytes);
            assert!(repair.capacity() > ingress.max_bytes);
            ingress.accept_rtp(now, repair, None);
            assert!(ingress.queued_media_bytes > 0);
            assert!(ingress.queued_media_bytes <= ingress.max_bytes);
            assert_eq!(received_rids(&mut ingress), [(11, Some("q".into()))]);
            assert_eq!(ingress.test_runtime_bytes(), full + repair_credit);
            assert_eq!(ingress.runtime_reservations.len(), 2);
        }
        ingress.accept_rtp(now, repair_packet(34, 52, 6, b'q'), None);
        assert!(received_rids(&mut ingress).is_empty());
        assert_eq!(ingress.test_runtime_bytes(), full + repair_credit);
        assert_eq!(ingress.retire(11).unwrap(), [Some(11), Some(33)]);
        assert_eq!(ingress.guard_bytes, 3 * guard);
        ingress.test_runtime_bytes();
        ingress.accept_rtp(now, rid_packet(22, 103, None), None);
        assert_eq!(received_rids(&mut ingress), [(22, Some("h".into()))]);
    }

    #[test]
    fn retirement_fences_queued_delivery_and_reclaims_receiver_without_rid_reassociation() {
        for remote_bye in [false, true] {
            let now = Instant::now();
            let mut ingress = rid_ingress(now);
            ingress.max_bytes = 8192;
            ingress.accept_rtp(now, rid_packet(11, 1, Some(b'q')), None);
            let mut report = vec![0x80, 200, 0, 6, 0, 0, 0, 11];
            report.resize(28, 0);
            ingress.accept_rtcp(now, report.clone(), PathEpoch::from_value(1), None, None);
            assert!(
                ingress
                    .events
                    .iter()
                    .any(|queued| matches!(queued.event, Event::SenderReport(_)))
            );
            for sequence in 1..u16::try_from(MAX_RECORDS).unwrap() {
                ingress.accept_rtp(now, rid_packet(22, sequence, Some(b'h')), None);
            }
            assert_eq!(ingress.events.len(), MAX_RECORDS + 3);
            let queued = ingress.events.len();
            let promises = ingress.runtime_reservations.len();
            let required = ingress.events.capacity() + 1;
            let notices = required - queued - promises;
            report[4..8].copy_from_slice(&22u32.to_be_bytes());
            report[8..12].copy_from_slice(&14u32.to_be_bytes());
            report[12..16].copy_from_slice(&0x3456_7890u32.to_be_bytes());
            report[16..20].copy_from_slice(&1234u32.to_be_bytes());
            let mut batch = report.repeat(notices);
            let mut receipt = vec![0x81, 201, 0, 7, 0, 0, 0, 9, 0, 0, 0, 22];
            receipt.resize(32, 0);
            receipt[16..20].copy_from_slice(&17u32.to_be_bytes());
            batch.extend_from_slice(&receipt);
            assert!(batch.len() <= ingress.max_bytes);
            let before = ingress.test_runtime_bytes();
            let backing = crate::event_buffer::Reservation::prepare(
                &ingress.events,
                required,
                required * std::mem::size_of::<QueuedEvent>(),
            )
            .unwrap();
            let exact = before + backing.bytes() - ingress.event_backing_bytes();
            drop(backing);
            for admitted in [false, true] {
                ingress.max_runtime_bytes = exact - usize::from(!admitted);
                let (receipts, retired) =
                    ingress.accept_rtcp(now, batch.clone(), PathEpoch::from_value(1), None, None);
                assert!(retired.is_empty());
                if admitted {
                    assert_eq!(
                        receipts,
                        [rtcp::ReportReceipt {
                            ssrc: 22,
                            highest_sequence: 17
                        }]
                    );
                } else {
                    assert!(receipts.is_empty());
                }
                assert_eq!(
                    ingress.events.len(),
                    queued + if admitted { notices } else { 0 }
                );
                assert_eq!(ingress.runtime_reservations.len(), promises);
                assert_eq!(
                    ingress.test_runtime_bytes(),
                    if admitted { exact } else { before }
                );
                let control = ingress
                    .encodings
                    .get_mut(&22)
                    .unwrap()
                    .receiver
                    .poll_control(
                        now + std::time::Duration::from_secs(1 + u64::from(admitted)),
                        99,
                        22,
                    )
                    .unwrap();
                let control = crate::packet::RtcpCompound::parse(&control)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .receiver_report()
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    control.reports().next().unwrap().last_sender_report(),
                    if admitted { (14 << 16) | 0x3456 } else { 0 }
                );
            }
            let charged = ingress.queued_media_bytes;
            let capacity = ingress.events.capacity();
            if remote_bye {
                let bye = vec![0x81, 203, 0, 1, 0, 0, 0, 11];
                let (_, retired) =
                    ingress.accept_rtcp(now, bye, PathEpoch::from_value(1), None, None);
                assert_eq!(retired, [11]);
            } else {
                assert_eq!(ingress.retire(11).unwrap(), [Some(11), None]);
            }
            assert_eq!(ingress.stats().len(), 1);
            assert_eq!(ingress.stats()[0].ssrc, Some(22));
            assert_eq!(
                ingress.queued_media_bytes, charged,
                "unfinished queued packets remain charged"
            );
            assert_eq!(ingress.events.capacity(), capacity);
            ingress.test_runtime_bytes();
            let mut passage = Vec::new();
            let mut reports = 0;
            while let Some(event) = ingress.poll_event() {
                match event {
                    Event::Rtp { packet, .. } => passage.push(packet.header.ssrc),
                    Event::EncodingDiscovered(info) => assert_ne!(info.ssrc, 11),
                    Event::SenderReport(report) => {
                        assert_eq!(report.ssrc, 22);
                        assert_eq!(report.arrival, now);
                        assert_eq!(report.ntp_seconds, 14);
                        assert_eq!(report.ntp_fraction, 0x3456_7890);
                        assert_eq!(report.rtp_timestamp, 1234);
                        reports += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(reports, notices);
            assert!(!passage.is_empty() && passage.iter().all(|ssrc| *ssrc == 22));
            assert_eq!(ingress.queued_media_bytes, 0);
            ingress.accept_rtp(now, rid_packet(33, 1, Some(b'q')), None);
            assert_eq!(received_rids(&mut ingress), [(33, Some("q".into()))]);
            for rid in [None, Some(b'q')] {
                ingress.accept_rtp(now, rid_packet(11, 2, rid), None);
                assert!(received_rids(&mut ingress).is_empty());
            }
            // A different SSRC cannot steal the still-owned RID/receiver.
            ingress.accept_rtp(now, rid_packet(44, 2, Some(b'q')), None);
            assert!(received_rids(&mut ingress).is_empty());
            ingress.accept_rtp(now, rid_packet(22, 300, None), None);
            ingress.accept_rtp(now, rid_packet(33, 2, None), None);
            assert_eq!(
                received_rids(&mut ingress),
                [(22, Some("h".into())), (33, Some("q".into()))]
            );
            assert_eq!(ingress.stats().len(), 2);
            ingress.abort();
            assert_eq!(ingress.test_runtime_bytes(), 0);
            assert_eq!(ingress.events.capacity(), 0);
            assert_eq!(ingress.queued_media_bytes, 0);
        }
    }

    #[test]
    fn receive_runtime_bytes_cover_lazy_history_and_rid_until_terminal_retirement() {
        let now = Instant::now();
        let mut ingress = rid_ingress(now);
        let charge = runtime::bytes(&ingress.media[&0], false).unwrap();
        let facts = ingress.test_media_bytes();
        assert_eq!(ingress.runtime_bytes, facts);
        let guard = SSRC_GUARD_BYTES;
        let total = facts
            .checked_add(charge)
            .unwrap()
            .checked_add(guard)
            .unwrap()
            .checked_add(3 * std::mem::size_of::<QueuedEvent>())
            .unwrap();
        let payload_metadata = std::mem::size_of::<crate::RtpPayloadInfo>() + "h264".len();
        let initial_metadata = 2 * "v".len() + payload_metadata;
        ingress.max_runtime_bytes = total + initial_metadata;
        let mut oversized = rid_packet(11, 1, None);
        oversized.reserve_exact(ingress.max_bytes);
        assert!(oversized.len() < ingress.max_bytes);
        assert!(oversized.capacity() > ingress.max_bytes);
        ingress.accept_rtp(now, oversized, None);
        assert_eq!(ingress.test_runtime_bytes(), facts);
        assert!(ingress.poll_event().is_none());
        assert_eq!(ingress.queued_media_bytes, 0);
        ingress.max_runtime_bytes -= 1;
        ingress.accept_rtp(now, rid_packet(11, 1, None), None);
        assert_eq!(ingress.test_runtime_bytes(), facts);
        assert!(ingress.encodings.is_empty());
        ingress.max_runtime_bytes += 1;
        let mut first = rid_packet(11, 1, None);
        first.reserve_exact(100);
        let capacity = first.capacity();
        let pointer = first.as_ptr();
        ingress.accept_rtp(now, first, None);
        assert_eq!(ingress.test_runtime_bytes(), total + initial_metadata);
        assert_eq!(ingress.queued_media_bytes, capacity);
        assert!(matches!(
            ingress.poll_event(),
            Some(Event::EncodingDiscovered(_))
        ));
        assert_eq!(ingress.test_runtime_bytes(), total + "v".len());
        let Some(Event::Rtp { packet: held, .. }) = ingress.poll_event() else {
            panic!("expected admitted RTP");
        };
        assert_eq!(held.header.ssrc, 11);
        assert_eq!(held.payload.as_ptr(), pointer.wrapping_add(12));
        assert_eq!(ingress.test_runtime_bytes(), total);
        assert_eq!(ingress.queued_media_bytes, 0);
        for (sequence, with_mid) in [(2, false), (3, true)] {
            let mut empty = rid_packet(11, sequence, None);
            empty.truncate(12);
            if with_mid {
                empty[0] = 0x90;
                empty.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, b'v', 0, 0]);
                empty.reserve_exact(100);
            } else {
                empty.reserve_exact(ingress.max_bytes);
            }
            let retained = if with_mid { empty.capacity() } else { 0 };
            let metadata = metadata_bytes(&ingress, &empty, None);
            ingress.max_runtime_bytes = total + metadata;
            ingress.accept_rtp(now, empty, None);
            assert_eq!(ingress.test_runtime_bytes(), total + metadata);
            assert_eq!(ingress.queued_media_bytes, retained);
            let Some(Event::Rtp { packet, .. }) = ingress.poll_event() else {
                panic!("expected empty RTP");
            };
            assert!(packet.payload.is_empty());
            assert_eq!(packet.extensions.len(), usize::from(with_mid));
            assert_eq!(ingress.test_runtime_bytes(), total);
            assert_eq!(ingress.queued_media_bytes, 0);
        }
        assert_eq!(ingress.encodings_by_media[&0].len(), 1);
        assert!(ingress.encoding_by_rid[&0].is_empty());
        // Receiver RID/loss backing is prepaid; queued copies have their own lifetime.
        let learned = "q".repeat(usize::from(u8::MAX));
        let mut late = rid_packet(11, 20_000, None);
        late.truncate(12);
        late[0] = 0x90;
        late.extend_from_slice(&[0x10, 0, 0, 65, 1, 1, b'v', 2, u8::MAX]);
        late.extend_from_slice(learned.as_bytes());
        late.push(0x65);
        let late_metadata = metadata_bytes(&ingress, &late, Some(&learned));
        ingress.max_runtime_bytes = total + late_metadata - 1;
        ingress.accept_rtp(now, late.clone(), None);
        assert_eq!(ingress.test_runtime_bytes(), total);
        assert_eq!(ingress.stats()[0].received_packets, 3);
        assert!(ingress.encodings[&11].rid.is_none());
        assert!(ingress.encoding_by_rid[&0].is_empty());
        ingress.max_runtime_bytes += 1;
        let mut duplicate = late.clone();
        duplicate[2..4].copy_from_slice(&3u16.to_be_bytes());
        ingress.accept_rtp(now, duplicate, None);
        assert_eq!(ingress.test_runtime_bytes(), total);
        assert!(ingress.encodings[&11].rid.is_none());
        ingress.accept_rtp(now, late, None);
        assert_eq!(ingress.test_runtime_bytes(), total + late_metadata);
        let receiver_rid = ingress.encodings[&11].rid.as_ref().unwrap();
        let (indexed_rid, indexed_ssrc) = ingress.encoding_by_rid[&0].first_key_value().unwrap();
        assert_eq!(receiver_rid.as_ref(), learned);
        assert_eq!(indexed_rid.as_ref(), learned);
        assert_eq!(*indexed_ssrc, 11);
        assert_ne!(receiver_rid.as_ptr(), indexed_rid.as_ptr());
        ingress.accept_rtp(now, rid_packet(22, 1, Some(b'h')), None);
        assert_eq!(ingress.runtime_reservations.len(), 1);
        assert_eq!(ingress.stats()[0].received_packets, 4);
        let queued = ingress.queued_media_bytes;
        ingress.retire(11).unwrap();
        let retired = facts + guard + ingress.event_backing_bytes() + late_metadata;
        assert_eq!(ingress.test_runtime_bytes(), retired);
        assert!(ingress.encodings_by_media[&0].is_empty());
        assert!(ingress.encoding_by_rid[&0].is_empty());
        assert_eq!(ingress.test_media_bytes(), facts);
        assert_eq!(ingress.queued_media_bytes, queued);
        ingress.accept_rtp(now, rid_packet(22, 1, Some(b'h')), None);
        assert_eq!(ingress.test_runtime_bytes(), retired);
        assert!(received_rids(&mut ingress).is_empty());
        assert_eq!(ingress.events.capacity(), 0);
        assert_eq!(ingress.test_runtime_bytes(), facts + guard);
        let next = rid_packet(22, 1, Some(b'h'));
        let next_metadata =
            metadata_bytes(&ingress, &next, Some("h")) + "v".len() + "h".len() + payload_metadata;
        let next_total = total + guard + next_metadata;
        ingress.max_runtime_bytes = next_total - 1;
        ingress.accept_rtp(now, next.clone(), None);
        assert_eq!(ingress.test_runtime_bytes(), facts + guard);
        assert!(ingress.encodings.is_empty());
        ingress.max_runtime_bytes += 1;
        ingress.accept_rtp(now, next, None);
        assert_eq!(ingress.test_runtime_bytes(), next_total);
        assert_eq!(received_rids(&mut ingress), [(22, Some("h".into()))]);
        ingress.accept_rtp(now, rid_packet(11, 20_001, None), None);
        assert!(received_rids(&mut ingress).is_empty());
        assert_eq!(ingress.test_runtime_bytes(), total + guard);
        ingress.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES;
        for ssrc in 1000..1033 {
            ingress.accept_rtp(now, rid_packet(ssrc, 1, None), None);
            assert!(
                matches!(ingress.poll_event(), Some(Event::EncodingDiscovered(info)) if info.ssrc == ssrc)
            );
            assert!(
                matches!(ingress.poll_event(), Some(Event::Rtp { packet, .. }) if packet.header.ssrc == ssrc && packet.payload.as_ref() == [0x65])
            );
        }
        assert_eq!(ingress.encodings.len(), 34);
        assert_eq!(ingress.runtime_reservations.len(), 34);
        assert_eq!(ingress.encodings_by_media[&0].len(), 34);
        let before = ingress.test_runtime_bytes();
        let guards = ingress.guard_bytes;
        let continuing = rid_packet(22, 2, None);
        ingress.max_runtime_bytes = before + metadata_bytes(&ingress, &continuing, Some("h"));
        ingress.accept_rtp(now, rid_packet(1033, 1, None), None);
        assert!(ingress.poll_event().is_none());
        assert_eq!(ingress.test_runtime_bytes(), before);
        assert_eq!(ingress.guard_bytes, guards);
        assert!(!ingress.has_issued_ssrc(1033));
        assert_eq!(ingress.encodings.len(), 34);
        ingress.accept_rtp(now, continuing, None);
        assert_eq!(received_rids(&mut ingress), [(22, Some("h".into()))]);
        assert_eq!(ingress.stats()[0].received_packets, 2);
        assert_eq!(ingress.test_runtime_bytes(), before);
        ingress.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES;
        ingress.accept_rtp(now, rid_packet(1033, 1, None), None);
        assert!(
            matches!(ingress.poll_event(), Some(Event::EncodingDiscovered(info)) if info.ssrc == 1033)
        );
        assert_eq!(received_rids(&mut ingress), [(1033, None)]);
        assert_eq!(ingress.encodings.len(), 35);
        let guards = ingress.guard_bytes;
        ingress.retire(1033).unwrap();
        assert!(matches!(
            ingress.poll_event(),
            Some(Event::EncodingRetired { ssrc: 1033, .. })
        ));
        assert_eq!(ingress.guard_bytes, guards);
        assert!(ingress.has_issued_ssrc(1033));
        assert_eq!(ingress.encodings.len(), 34);
        assert_eq!(ingress.runtime_reservations.len(), 34);
        ingress.accept_rtp(now, rid_packet(1033, 2, None), None);
        assert!(ingress.poll_event().is_none());
        ingress.test_runtime_bytes();
        ingress.accept_rtp(now, rid_packet(22, 3, None), None);
        assert!(ingress.queued_media_bytes > 0);
        ingress.abort();
        assert_eq!(ingress.queued_media_bytes, 0);
        assert_eq!(held.payload.as_ref(), &[0x65]);
        assert_eq!(ingress.test_runtime_bytes(), 0);
        assert_eq!(ingress.test_media_bytes(), 0);
        assert!(ingress.runtime_reservations.is_empty());

        let mut plain_facts = rid_ingress(now).media[&0].clone();
        plain_facts.nack_payloads = Box::new([]);
        let smaller = runtime::bytes(&plain_facts, false).unwrap();
        let plain_storage = runtime::media_bytes(&plain_facts).unwrap();
        assert!(smaller.checked_mul(2).unwrap() < charge);
        let mut plain = IngressOwner::new(
            Box::new([plain_facts]),
            4096,
            plain_storage.checked_add(charge).unwrap(),
            99,
            [b'r'; 32],
            ReceiveFeedback::new(None, now),
        )
        .unwrap();
        plain.accept_rtp(now, rid_packet(11, 1, Some(b'q')), None);
        plain.accept_rtp(now, rid_packet(22, 1, Some(b'h')), None);
        assert_eq!(
            plain.runtime_bytes,
            plain_storage
                .checked_add(smaller.checked_mul(2).unwrap())
                .unwrap()
                .checked_add(2 * guard)
                .unwrap()
                .checked_add(plain.event_backing_bytes())
                .unwrap()
                .checked_add(
                    plain
                        .events
                        .iter()
                        .map(|queued| queued.owned_bytes)
                        .sum::<usize>()
                )
                .unwrap()
        );
        assert_eq!(received_rids(&mut plain).len(), 2);
        let twcc = vec![
            0x8f, 205, 0, 5, 0, 0, 0, 9, 0, 0, 0, 11, 0, 1, 0, 2, 0, 0, 0, 1, 0, 1, 0, 1,
        ];
        let rfc8888 = vec![
            0x8b, 205, 0, 5, 0, 0, 0, 9, 0, 0, 0, 11, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        let nack = [0x81, 205, 0, 3, 0, 0, 0, 9, 0, 0, 0, 11, 0, 7, 0, 0];
        let pli = [0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 11];
        let mut compound = Vec::new();
        for (index, (mode, feedback)) in [
            (PacketFeedbackKind::TransportWide, &twcc),
            (PacketFeedbackKind::Rfc8888, &rfc8888),
        ]
        .into_iter()
        .enumerate()
        {
            let at = now + std::time::Duration::from_secs(u64::try_from(index).unwrap() * 2);
            let seconds = 12 + u32::try_from(index).unwrap();
            compound = vec![0x80, 200, 0, 6];
            for value in [22, seconds, 0x3456_7890, 1234, 1, 1] {
                compound.extend_from_slice(&value.to_be_bytes());
            }
            let mut rr = vec![0x81, 201, 0, 7, 0, 0, 0, 9, 0, 0, 0, 11];
            rr.resize(32, 0);
            rr[16..20].copy_from_slice(&17u32.to_be_bytes());
            compound.extend_from_slice(&rr);
            compound.extend_from_slice(feedback);
            compound.extend_from_slice(&nack);
            compound.extend_from_slice(&pli);
            compound.extend_from_slice(feedback);
            let parsed = rtcp::parse(&compound, at, PathEpoch::from_value(1), mode).unwrap();
            let mut expected = parsed.feedback.clone();
            let mut owned = expected
                .iter()
                .map(|batch| control::feedback_bytes(batch).unwrap())
                .sum::<usize>();
            let mut slots = VecDeque::<control::Feedback>::new();
            slots.try_reserve_exact(expected.len()).unwrap();
            let mut feedback_slots = slots.capacity() * std::mem::size_of::<control::Feedback>();
            let repair_slots = VecDeque::<RepairRequest>::with_capacity(1).capacity()
                * std::mem::size_of::<RepairRequest>();
            let keyframe_slots = VecDeque::<rtcp::KeyframeRequest>::with_capacity(1).capacity()
                * std::mem::size_of::<rtcp::KeyframeRequest>();
            let base = plain.test_runtime_bytes();
            let exact = base + owned + feedback_slots + repair_slots + keyframe_slots;
            plain.max_runtime_bytes = exact;
            let staged = plain
                .prepare_control(parsed.feedback, parsed.repair, parsed.keyframes, 1)
                .unwrap();
            drop(staged);
            assert_eq!(plain.test_runtime_bytes(), base);
            assert_eq!(plain.control_backing_bytes(), 0);
            for admitted in [false, true] {
                plain.max_runtime_bytes = exact - usize::from(!admitted);
                let (receipts, retired) = plain.accept_rtcp(
                    at,
                    compound.clone(),
                    PathEpoch::from_value(1),
                    Some(mode),
                    None,
                );
                assert!(retired.is_empty());
                let tick = if admitted { 2 } else { 1 };
                let report = plain
                    .encodings
                    .get_mut(&22)
                    .unwrap()
                    .receiver
                    .poll_control(at + std::time::Duration::from_secs(tick), 99, 22)
                    .unwrap();
                let report = crate::packet::RtcpCompound::parse(&report)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .receiver_report()
                    .unwrap()
                    .unwrap();
                let previous = if index == 0 { 0 } else { 0x000c_3456 };
                assert_eq!(
                    report.reports().next().unwrap().last_sender_report(),
                    if admitted {
                        (seconds << 16) | 0x3456
                    } else {
                        previous
                    }
                );
                if admitted {
                    assert_eq!(
                        receipts,
                        [rtcp::ReportReceipt {
                            ssrc: 11,
                            highest_sequence: 17
                        }]
                    );
                    assert_eq!(plain.test_runtime_bytes(), exact);
                } else {
                    assert!(receipts.is_empty());
                    assert!(
                        plain.events.is_empty()
                            && plain.feedback.is_empty()
                            && plain.repair.is_empty()
                            && plain.keyframes.is_empty()
                    );
                    assert_eq!(plain.test_runtime_bytes(), base);
                }
            }
            if index == 0 {
                let feedback_at = at + std::time::Duration::from_secs(2);
                let dropped = plain.dropped();
                plain.accept_rtcp(
                    feedback_at,
                    feedback.clone(),
                    PathEpoch::from_value(1),
                    Some(mode),
                    None,
                );
                assert_eq!(plain.dropped(), dropped + 1);
                assert_eq!(plain.test_runtime_bytes(), exact);
                let extra = control::feedback_bytes(&expected[0]).unwrap();
                plain.max_runtime_bytes = exact + extra + feedback_slots;
                plain.accept_rtcp(
                    feedback_at,
                    feedback.clone(),
                    PathEpoch::from_value(1),
                    Some(mode),
                    None,
                );
                owned += extra;
                let mut batch = expected[0].clone();
                batch.received_at = feedback_at;
                expected.push(batch);
                feedback_slots *= 2;
                assert_eq!(plain.test_runtime_bytes(), plain.max_runtime_bytes);
                assert_eq!(plain.feedback.capacity(), 4);
            } else {
                plain.retire(11).unwrap();
            }
            let before = plain.test_runtime_bytes();
            let keyframe = plain.poll_keyframe().unwrap();
            assert_eq!(keyframe.media_ssrc, 11);
            assert_eq!(keyframe.fir, None);
            assert_eq!(plain.test_runtime_bytes(), before - keyframe_slots);
            let before = plain.test_runtime_bytes();
            assert_eq!(
                plain.poll_repair(),
                Some(RepairRequest {
                    media_ssrc: 11,
                    sequence: 7
                })
            );
            assert_eq!(plain.test_runtime_bytes(), before - repair_slots);
            let before = plain.test_runtime_bytes();
            let count = expected.len();
            for (index, batch) in expected.into_iter().enumerate() {
                let owned = control::feedback_bytes(&batch).unwrap();
                let previous = plain.test_runtime_bytes();
                assert_eq!(plain.poll_feedback(), Some(batch));
                assert_eq!(
                    plain.test_runtime_bytes(),
                    previous
                        - owned
                        - if index + 1 == count {
                            feedback_slots
                        } else {
                            0
                        }
                );
            }
            assert_eq!(plain.test_runtime_bytes(), before - owned - feedback_slots);
            assert_eq!(plain.control_backing_bytes(), 0);
            while plain.poll_event().is_some() {}
        }
        let control_at = now + std::time::Duration::from_secs(4);
        let base = plain.test_runtime_bytes();
        plain.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES;
        let mut backlog = twcc.clone();
        backlog.extend_from_slice(&nack);
        backlog.extend_from_slice(&pli);
        for _ in 0..257 {
            let dropped = plain.dropped();
            let (receipts, retired) = plain.accept_rtcp(
                control_at,
                backlog.clone(),
                PathEpoch::from_value(1),
                Some(PacketFeedbackKind::TransportWide),
                None,
            );
            assert!(receipts.is_empty() && retired.is_empty());
            assert_eq!(plain.dropped(), dropped);
            plain.test_runtime_bytes();
        }
        assert_eq!(plain.feedback.len(), 257);
        assert_eq!(plain.repair.len(), 257);
        assert_eq!(plain.keyframes.len(), 257);
        let queued = plain.test_runtime_bytes();
        compound[8..12].copy_from_slice(&14u32.to_be_bytes());
        let parsed = rtcp::parse(
            &compound,
            control_at,
            PathEpoch::from_value(1),
            PacketFeedbackKind::Rfc8888,
        )
        .unwrap();
        assert!(plain.feedback.capacity() >= plain.feedback.len() + parsed.feedback.len());
        assert!(plain.repair.capacity() >= plain.repair.len() + parsed.repair.len());
        assert!(plain.keyframes.capacity() >= plain.keyframes.len() + parsed.keyframes.len());
        let extra = parsed
            .feedback
            .iter()
            .map(|batch| control::feedback_bytes(batch).unwrap())
            .sum::<usize>();
        let exact = queued + extra;
        for admitted in [false, true] {
            plain.max_runtime_bytes = exact - usize::from(!admitted);
            let (receipts, retired) = plain.accept_rtcp(
                control_at,
                compound.clone(),
                PathEpoch::from_value(1),
                Some(PacketFeedbackKind::Rfc8888),
                None,
            );
            assert!(retired.is_empty());
            if admitted {
                assert_eq!(
                    receipts,
                    [rtcp::ReportReceipt {
                        ssrc: 11,
                        highest_sequence: 17
                    }]
                );
            } else {
                assert!(receipts.is_empty());
            }
            let added = usize::from(admitted);
            assert_eq!(plain.feedback.len(), 257 + added * 2);
            assert_eq!(plain.repair.len(), 257 + added);
            assert_eq!(plain.keyframes.len(), 257 + added);
            assert_eq!(plain.events.len(), added);
            assert_eq!(
                plain.test_runtime_bytes(),
                if admitted { exact } else { queued }
            );
            let report = plain
                .encodings
                .get_mut(&22)
                .unwrap()
                .receiver
                .poll_control(
                    control_at + std::time::Duration::from_secs(1 + u64::from(admitted)),
                    99,
                    22,
                )
                .unwrap();
            let report = crate::packet::RtcpCompound::parse(&report)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .receiver_report()
                .unwrap()
                .unwrap();
            assert_eq!(
                report.reports().next().unwrap().last_sender_report(),
                ((if admitted { 14 } else { 13 }) << 16) | 0x3456
            );
        }
        while plain.poll_repair().is_some() {}
        while plain.poll_feedback().is_some() {}
        while plain.poll_event().is_some() {}
        let retained = plain.test_runtime_bytes();
        let requests: Vec<_> = plain.poll_keyframes().collect();
        assert_eq!(requests.len(), 256);
        assert!(
            requests
                .iter()
                .all(|request| request.media_ssrc == 11 && request.fir.is_none())
        );
        assert_eq!(plain.keyframes.len(), 2);
        assert!(plain.has_pending_output());
        assert_eq!(plain.test_runtime_bytes(), retained);
        assert_eq!(plain.poll_keyframes().count(), 2);
        assert!(!plain.has_pending_output());
        assert_eq!(plain.control_backing_bytes(), 0);
        assert_eq!(plain.test_runtime_bytes(), base);
        let full = plain.test_limit_runtime_to_current_usage();
        let mut compressed = twcc;
        compressed[14..16].copy_from_slice(&8192u16.to_be_bytes());
        compressed[20..24].copy_from_slice(&[0x1f, 0xff, 0, 1]);
        compressed.extend_from_slice(&[0x81, 203, 0, 1, 0, 0, 0, 22]);
        let (receipts, retired) = plain.accept_rtcp(
            control_at,
            compressed,
            PathEpoch::from_value(1),
            Some(PacketFeedbackKind::TransportWide),
            None,
        );
        assert!(receipts.is_empty());
        assert_eq!(retired, [22]);
        assert!(plain.feedback.is_empty());
        assert!(plain.test_runtime_bytes() < full);
        plain.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES;
        assert_eq!(plain.control_backing_bytes(), 0);
        while plain.poll_event().is_some() {}
        assert!(!plain.has_pending_output());
        plain.max_bytes = crate::ConnectionLimits::DEFAULT_MAX_QUEUED_MEDIA_BYTES;
        plain.accept_rtp(control_at, rid_packet(33, 1, Some(b'q')), None);
        assert_eq!(received_rids(&mut plain), [(33, Some("q".into()))]);
        for sequence in 2..=258 {
            plain.accept_rtp(control_at, rid_packet(33, sequence, Some(b'q')), None);
        }
        assert_eq!(plain.events.len(), 257);
        plain.accept_rtp(control_at, rid_packet(44, 1, Some(b'h')), None);
        assert_eq!(plain.events.len(), 259);
        assert!(plain.encodings.contains_key(&44));
        let full = plain.test_limit_runtime_to_current_usage();
        let next = rid_packet(33, 259, Some(b'q'));
        let metadata = metadata_bytes(&plain, &next, Some("q"));
        let received = plain.encodings[&33].receiver.received();
        plain.accept_rtp(control_at, next.clone(), None);
        assert_eq!(plain.events.len(), 259);
        assert_eq!(plain.test_runtime_bytes(), full);
        assert_eq!(plain.encodings[&33].receiver.received(), received);
        plain.max_runtime_bytes += metadata;
        plain.accept_rtp(control_at, next, None);
        assert_eq!(plain.events.len(), 260);
        assert_eq!(plain.test_runtime_bytes(), full + metadata);
        assert_eq!(plain.encodings[&33].receiver.received(), received + 1);
        plain.retire(33).unwrap();
        assert_eq!(plain.events.len(), 261);
        assert!(
            plain.poll_event().is_none(),
            "stale discard work stays bounded"
        );
        assert_eq!(plain.events.len(), 5);
        assert!(
            plain.has_pending_output(),
            "remaining events need immediate service"
        );
        assert_eq!(received_rids(&mut plain), [(44, Some("h".into()))]);
        assert_eq!(plain.queued_media_bytes, 0);
        plain.retire(44).unwrap();
        assert!(matches!(
            plain.poll_event(),
            Some(Event::EncodingRetired { ssrc: 44, .. })
        ));
        assert_eq!(plain.events.capacity(), 0);
        plain.accept_rtcp(
            control_at,
            compound,
            PathEpoch::from_value(1),
            Some(PacketFeedbackKind::Rfc8888),
            None,
        );
        assert!(plain.control_backing_bytes() > 0);
        plain.abort();
        assert_eq!(plain.control_backing_bytes(), 0);
        assert_eq!(plain.events.capacity(), 0);
        assert_eq!(plain.test_runtime_bytes(), 0);
    }

    #[test]
    fn offered_primary_can_bootstrap_a_keyframe_without_discovering_an_encoding() {
        let facts = IngressMediaFacts {
            kind: crate::MediaKind::Video,
            rtcp: None,
            mid: "video".into(),
            payloads: Box::new([crate::RtpPayloadInfo {
                payload_type: 96,
                name: "h264".into(),
                clock_rate: 90_000,
            }]),
            feedback_payloads: Box::new([96]),
            repairs: Box::new([(97, 96)]),
            nack_payloads: Box::new([96]),
            ssrc_repairs: Box::new([(12, 11), (12, 11)]),
            ssrcs: Box::new([11, 12]),
            rids: Box::new([]),
            extensions: Box::new([]),
        };
        let now = Instant::now();
        let mut feedback = ReceiveFeedback::new(Some(PacketFeedbackKind::Rfc8888), now);
        let feedback_bytes = feedback.reserve(usize::MAX).unwrap();
        let required = runtime::media_bytes(&facts)
            .unwrap()
            .checked_add(runtime::bytes(&facts, false).unwrap())
            .unwrap()
            .checked_add(2 * SSRC_GUARD_BYTES)
            .unwrap()
            .checked_add(std::mem::size_of::<QueuedEvent>())
            .unwrap()
            .checked_add(feedback_bytes)
            .unwrap();
        assert!(matches!(
            IngressOwner::new(
                Box::new([facts.clone()]),
                4096,
                required.checked_sub(1).unwrap(),
                99,
                [b'r'; 32],
                ReceiveFeedback::new(Some(PacketFeedbackKind::Rfc8888), now),
            ),
            Err(crate::AcceptError::SessionLimitExceeded)
        ));
        let mut ingress =
            IngressOwner::new(Box::new([facts]), 4096, required, 99, [b'r'; 32], feedback).unwrap();
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
        let packet = vec![0x80, 97, 0, 50, 0, 0, 0, 0, 0, 0, 0, 12, 0, 1, 0x65];
        ingress.accept_rtp(now, packet.clone(), None);
        assert!(ingress.stats().is_empty());
        assert_eq!(ingress.test_runtime_bytes(), required);
        let reported_at = now + std::time::Duration::from_millis(50);
        let report = ingress.poll_control(reported_at).unwrap();
        assert_eq!(&report[..4], &[0x80, 201, 0, 1]);
        let parsed = rtcp::parse(
            &report,
            reported_at,
            PathEpoch::from_value(1),
            PacketFeedbackKind::Rfc8888,
        )
        .unwrap();
        let rtcp::FeedbackReport::Rfc8888 { reports, .. } = &parsed.feedback[0].report else {
            panic!("physical receipt without accepting an encoding");
        };
        assert!(
            matches!(&parsed.lifecycle[0], LifecycleFact::Cname { ssrc: 99, cname }
            if cname.as_bytes() == ingress.receiver_cname)
        );
        assert_eq!(reports[0].ssrc, 12);
        assert_eq!(reports[0].begin_sequence, 50);
        assert!(matches!(
            reports[0].statuses[0],
            rtcp::Rfc8888Status::Received { .. }
        ));
        assert_eq!(ingress.test_runtime_bytes(), required);
        let metadata =
            2 * "video".len() + std::mem::size_of::<crate::RtpPayloadInfo>() + "h264".len();
        ingress.max_runtime_bytes = required + 2 * std::mem::size_of::<QueuedEvent>() + metadata;
        let capacity = ingress.events.capacity();
        let staged = ingress.prepare_events(ingress.runtime_bytes, 1, 2).unwrap();
        drop(staged);
        assert_eq!(ingress.events.capacity(), capacity);
        assert_eq!(ingress.test_runtime_bytes(), required);
        ingress.accept_rtp(reported_at, packet, None);
        assert!(matches!(
            ingress.poll_event(),
            Some(Event::EncodingDiscovered { .. })
        ));
        assert_eq!(ingress.encodings[&11].repair_ssrc, Some(12));
        assert_eq!(ingress.repair_by_ssrc.get(&12), Some(&11));
        ingress.test_runtime_bytes();
        ingress.retire(11).unwrap();
        assert_eq!(
            ingress.keyframe_request(11).unwrap_err(),
            crate::WriteError::UnknownEncoding
        );
        assert_eq!(ingress.receive_feedback.storage_bytes(), feedback_bytes);
        ingress.abort();
        assert_eq!(ingress.receive_feedback.storage_bytes(), 0);
        assert_eq!(ingress.test_runtime_bytes(), 0);
    }
}
