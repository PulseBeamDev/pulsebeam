use std::{
    collections::{BTreeMap, BTreeSet},
    mem::{size_of, size_of_val},
};

use super::{Encoding, IngressOwner, IngressUpdate, SSRC_GUARD_BYTES, events};
use crate::{
    AcceptError, RtpConfigurationError, negotiation::IngressMediaFacts,
    receive_stream::ReceiveStream,
};

pub(super) struct Reservation {
    primary: u32,
    charge: usize,
    guards: usize,
    events: events::Reservation,
}

impl Reservation {
    pub(super) fn commit(self, owner: &mut IngressOwner) {
        owner.runtime_reservations.insert(self.primary, self.charge);
        owner.guard_bytes = self.guards;
        owner.runtime_bytes = self.events.commit(&mut owner.events);
    }
}

pub(super) fn facts_bytes(facts: &IngressMediaFacts) -> Option<usize> {
    [
        size_of::<(u64, IngressMediaFacts)>(),
        facts.mid.len(),
        facts.rtcp.as_ref().map_or(0, |rtcp| rtcp.cname.capacity()),
        size_of_val(facts.payloads.as_ref()),
        size_of_val(facts.feedback_payloads.as_ref()),
        size_of_val(facts.repairs.as_ref()),
        size_of_val(facts.nack_payloads.as_ref()),
        size_of_val(facts.ssrc_repairs.as_ref()),
        size_of_val(facts.ssrcs.as_ref()),
        size_of_val(facts.rids.as_ref()),
        size_of_val(facts.extensions.as_ref()),
    ]
    .into_iter()
    .chain(facts.rids.iter().map(|rid| rid.len()))
    .chain(facts.extensions.iter().map(|(_, uri)| uri.len()))
    .try_fold(0usize, usize::checked_add)
}

pub(super) fn media_bytes(facts: &IngressMediaFacts) -> Option<usize> {
    let repairs: BTreeSet<_> = facts
        .ssrc_repairs
        .iter()
        .map(|(repair, _)| *repair)
        .collect();
    let primaries: BTreeSet<_> = facts
        .ssrcs
        .iter()
        .copied()
        .filter(|ssrc| !repairs.contains(ssrc))
        .collect();
    let indexes = primaries
        .len()
        .checked_add(repairs.len())?
        .checked_mul(size_of::<(u32, u64)>())?;
    facts_bytes(facts)?
        .checked_add(indexes)?
        .checked_add(size_of::<(u64, BTreeSet<u32>)>())?
        .checked_add(size_of::<(u64, BTreeMap<Box<str>, u32>)>())?
        .checked_add(size_of::<u64>())?
        .checked_add(size_of::<((Box<str>, u64), bool)>())?
        .checked_add(facts.mid.len())?
        .checked_add(
            if facts
                .extensions
                .iter()
                .any(|(_, uri)| uri.as_ref() == crate::MID_EXTENSION)
            {
                size_of::<(u64, u8)>()
            } else {
                0
            },
        )
}

pub(super) fn primaries(facts: &IngressMediaFacts) -> impl Iterator<Item = u32> + '_ {
    facts
        .ssrcs
        .iter()
        .copied()
        .filter(|ssrc| !facts.ssrc_repairs.iter().any(|(repair, _)| repair == ssrc))
}

pub(super) fn bytes(facts: &IngressMediaFacts, strict: bool) -> Option<usize> {
    let rid = if strict {
        facts.rids.iter().map(|rid| rid.len()).max().unwrap_or(0)
    } else if facts
        .extensions
        .iter()
        .any(|(_, uri)| uri.as_ref() == crate::RID_EXTENSION)
    {
        // Bootstrap can learn a RID later, up to the RTP two-byte extension limit.
        usize::from(u8::MAX)
    } else {
        0
    };
    let rid_index = if rid == 0 {
        0
    } else {
        size_of::<(Box<str>, u32)>().checked_add(rid)?
    };
    size_of::<(u32, Encoding)>()
        .checked_add(size_of::<(u32, usize)>())?
        .checked_add(size_of::<u32>())?
        .checked_add(size_of::<(u32, u64)>())?
        .checked_add(if facts.repairs.is_empty() {
            0
        } else {
            size_of::<(u32, u32)>()
        })?
        .checked_add(rid_index)?
        .checked_add(ReceiveStream::missing_bytes(
            !facts.nack_payloads.is_empty(),
        )?)?
        .checked_add(rid)
}

pub(super) fn repair_owner_bytes(facts: &IngressMediaFacts, repair: u32) -> usize {
    if facts.ssrc_repairs.iter().any(|(known, _)| *known == repair) {
        0
    } else {
        size_of::<(u32, u64)>()
    }
}

pub(super) fn bootstrap(
    media: &BTreeMap<u64, IngressMediaFacts>,
    limit: usize,
) -> Result<(BTreeMap<u32, usize>, usize, usize), AcceptError> {
    let mut reservations = BTreeMap::new();
    let mut identities = BTreeSet::new();
    let mut total = 0usize;
    for facts in media.values() {
        total = total
            .checked_add(media_bytes(facts).ok_or(AcceptError::SessionLimitExceeded)?)
            .filter(|bytes| *bytes <= limit)
            .ok_or(AcceptError::SessionLimitExceeded)?;
        for ssrc in facts
            .ssrcs
            .iter()
            .copied()
            .chain(facts.ssrc_repairs.iter().map(|(repair, _)| *repair))
        {
            if identities.insert(ssrc) {
                total = total
                    .checked_add(SSRC_GUARD_BYTES)
                    .filter(|bytes| *bytes <= limit)
                    .ok_or(AcceptError::SessionLimitExceeded)?;
            }
        }
        let charge = bytes(facts, false).ok_or(AcceptError::SessionLimitExceeded)?;
        for ssrc in primaries(facts) {
            if reservations.insert(ssrc, charge).is_some() {
                return Err(AcceptError::CapabilityConflict);
            }
            total = total
                .checked_add(charge)
                .filter(|bytes| *bytes <= limit)
                .ok_or(AcceptError::SessionLimitExceeded)?;
        }
    }
    let guards = identities
        .len()
        .checked_mul(SSRC_GUARD_BYTES)
        .ok_or(AcceptError::SessionLimitExceeded)?;
    Ok((reservations, total, guards))
}

impl IngressOwner {
    pub(super) fn stage_runtime(
        &self,
        update: &mut IngressUpdate,
    ) -> Result<(), RtpConfigurationError> {
        use RtpConfigurationError as Error;
        for key in &update.removed {
            let facts = self.media.get(key).ok_or(Error::ConnectionState)?;
            let observed = self
                .encodings_by_media
                .get(key)
                .into_iter()
                .flatten()
                .copied();
            for ssrc in primaries(facts).chain(observed) {
                update.runtime.insert(ssrc, None);
            }
        }
        for facts in update.new_media.values() {
            let charge = bytes(facts, true).ok_or(Error::Capacity)?;
            for ssrc in primaries(facts) {
                // An adopted bootstrap receiver keeps its original NACK mode and
                // may already own a larger RID reservation than the typed facts.
                // Transfer repair ownership separately when new facts declare it.
                let charge = if let Some(encoding) = self.encodings.get(&ssrc) {
                    let old = self
                        .media
                        .get(&encoding.media)
                        .ok_or(Error::ConnectionState)?;
                    let old_owner = encoding
                        .repair_ssrc
                        .map_or(0, |repair| repair_owner_bytes(old, repair));
                    let new_owner = encoding
                        .repair_ssrc
                        .map_or(0, |repair| repair_owner_bytes(facts, repair));
                    let reserved = self
                        .runtime_reservations
                        .get(&ssrc)
                        .copied()
                        .and_then(|bytes| bytes.checked_sub(old_owner))
                        .ok_or(Error::ConnectionState)?;
                    charge
                        .max(reserved)
                        .checked_add(new_owner)
                        .ok_or(Error::Capacity)?
                } else {
                    charge
                };
                update.runtime.insert(ssrc, Some(charge));
            }
        }
        let removed_facts = update.removed.iter().try_fold(0usize, |total, key| {
            let facts = self.media.get(key).ok_or(Error::ConnectionState)?;
            total
                .checked_add(media_bytes(facts).ok_or(Error::Capacity)?)
                .ok_or(Error::Capacity)
        })?;
        let added_facts = update
            .new_media
            .values()
            .try_fold(0usize, |total, facts| {
                total.checked_add(media_bytes(facts)?)
            })
            .ok_or(Error::Capacity)?;
        let removed = update
            .runtime
            .keys()
            .try_fold(0usize, |total, ssrc| {
                total.checked_add(self.runtime_reservations.get(ssrc).copied().unwrap_or(0))
            })
            .and_then(|bytes| bytes.checked_add(removed_facts))
            .ok_or(Error::Capacity)?;
        let added = update
            .runtime
            .values()
            .flatten()
            .try_fold(0usize, |total, charge| total.checked_add(*charge))
            .and_then(|bytes| bytes.checked_add(added_facts))
            .ok_or(Error::Capacity)?;
        let guards = update
            .identities
            .iter()
            .filter(|ssrc| !self.has_issued_ssrc(**ssrc))
            .count()
            .checked_mul(SSRC_GUARD_BYTES)
            .ok_or(Error::Capacity)?;
        update.guard_bytes = self
            .guard_bytes
            .checked_add(guards)
            .ok_or(Error::Capacity)?;
        update.runtime_bytes = self
            .runtime_bytes
            .checked_sub(removed)
            .ok_or(Error::ConnectionState)?
            .checked_add(added)
            .and_then(|bytes| bytes.checked_add(guards))
            .ok_or(Error::Capacity)?;
        let removed = update
            .runtime
            .keys()
            .filter(|ssrc| self.runtime_reservations.contains_key(*ssrc))
            .count();
        let added = update.runtime.values().flatten().count();
        let future = self
            .runtime_reservations
            .len()
            .checked_sub(removed)
            .and_then(|count| count.checked_add(added))
            .ok_or(Error::ConnectionState)?;
        let retiring = update
            .runtime
            .iter()
            .filter(|(ssrc, charge)| charge.is_none() && self.encodings.contains_key(*ssrc))
            .count();
        let events = self
            .prepare_events(update.runtime_bytes, future, retiring)
            .ok_or(Error::Capacity)?;
        update.runtime_bytes = events.runtime_bytes;
        update.event_reservation = Some(events);
        Ok(())
    }

    pub(super) fn prepare_runtime(
        &self,
        primary: u32,
        media: u64,
        discovery: bool,
        repair: Option<u32>,
        owned_bytes: usize,
    ) -> Option<Reservation> {
        let facts = self.media.get(&media)?;
        let previous = self.runtime_reservations.get(&primary).copied();
        let mut charge = match previous {
            Some(charge) => charge,
            None if discovery => bytes(facts, self.strict)?,
            None => return None,
        };
        let mut guards = if self.has_issued_ssrc(primary) {
            0
        } else {
            SSRC_GUARD_BYTES
        };
        if let Some(repair) = repair {
            if self
                .encodings
                .get(&primary)
                .and_then(|encoding| encoding.repair_ssrc)
                .is_none()
            {
                charge = charge.checked_add(repair_owner_bytes(facts, repair))?;
            }
            if !self.has_issued_ssrc(repair) {
                guards = guards.checked_add(SSRC_GUARD_BYTES)?;
            }
        }
        let total = self
            .runtime_bytes
            .checked_add(charge.checked_sub(previous.unwrap_or(0))?)?
            .checked_add(guards)?
            .checked_add(owned_bytes)?;
        let future = self
            .runtime_reservations
            .len()
            .checked_add(usize::from(previous.is_none()))?;
        Some(Reservation {
            primary,
            charge,
            guards: self.guard_bytes.checked_add(guards)?,
            events: self.prepare_events(total, future, if discovery { 2 } else { 1 })?,
        })
    }

    pub(super) fn release_runtime(&mut self, ssrc: u32) {
        if let Some(charge) = self.runtime_reservations.remove(&ssrc) {
            self.runtime_bytes = self.runtime_bytes.saturating_sub(charge);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_registry_and_each_owned_payload_are_charged() {
        const MID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
        let facts = IngressMediaFacts {
            kind: crate::MediaKind::Video,
            rtcp: None,
            mid: "v".into(),
            payloads: Box::new([(96, 90_000)]),
            feedback_payloads: Box::new([96]),
            repairs: Box::new([]),
            nack_payloads: Box::new([96]),
            ssrc_repairs: Box::new([]),
            ssrcs: Box::new([11]),
            rids: Box::new([]),
            extensions: Box::new([]),
        };
        let baseline = facts_bytes(&facts).unwrap();
        assert_eq!(
            baseline,
            size_of::<(u64, IngressMediaFacts)>()
                + 1
                + size_of::<(u8, u32)>()
                + 2
                + size_of::<u32>()
        );
        let owner = size_of::<(u32, u64)>();
        let groups = size_of::<(u64, BTreeSet<u32>)>()
            + size_of::<(u64, BTreeMap<Box<str>, u32>)>()
            + size_of::<u64>()
            + size_of::<((Box<str>, u64), bool)>()
            + facts.mid.len()
            + owner;
        assert_eq!(media_bytes(&facts), Some(baseline + groups));
        let mut named = facts.clone();
        let mut cname = String::with_capacity(4096);
        cname.push('a');
        let capacity = cname.capacity();
        named.rtcp = Some(crate::RtcpParameters {
            cname,
            reduced_size: false,
        });
        assert_eq!(facts_bytes(&named), Some(baseline + capacity));
        assert_eq!(media_bytes(&named), Some(baseline + groups + capacity));
        named.rtcp.as_mut().unwrap().reduced_size = true;
        assert_eq!(facts_bytes(&named), Some(baseline + capacity));
        named.rtcp = None;
        assert_eq!(facts_bytes(&named), Some(baseline));
        assert_eq!(
            bytes(&facts, true),
            Some(
                size_of::<(u32, Encoding)>()
                    + size_of::<(u32, usize)>()
                    + size_of::<u32>()
                    + owner
                    + ReceiveStream::missing_bytes(true).unwrap()
            )
        );
        let mut rid_facts = facts.clone();
        rid_facts.extensions = Box::new([(2, crate::RID_EXTENSION.into())]);
        rid_facts.rids = Box::new(["h".into()]);
        let index = size_of::<(Box<str>, u32)>();
        assert_eq!(
            bytes(&rid_facts, true),
            bytes(&facts, true).map(|base| base + index + 2)
        );
        assert_eq!(
            bytes(&rid_facts, false),
            bytes(&facts, false).map(|base| base + index + 2 * usize::from(u8::MAX))
        );
        rid_facts.extensions = Box::new([(2, crate::REPAIRED_RID_EXTENSION.into())]);
        assert_eq!(bytes(&rid_facts, false), bytes(&facts, false));
        let mut repair_facts = facts.clone();
        repair_facts.repairs = Box::new([(97, 96)]);
        assert_eq!(
            bytes(&repair_facts, true),
            bytes(&facts, true).map(|base| base + size_of::<(u32, u32)>())
        );
        repair_facts.ssrc_repairs = Box::new([(12, 11), (12, 11)]);
        assert_eq!(repair_owner_bytes(&repair_facts, 12), 0);
        assert_eq!(repair_owner_bytes(&repair_facts, 13), owner);
        assert_eq!(
            media_bytes(&repair_facts),
            Some(baseline + groups + size_of::<(u8, u8)>() + 2 * size_of::<(u32, u32)>() + owner)
        );
        type Change = fn(&mut IngressMediaFacts);
        let changes: [(Change, usize, usize); 9] = [
            (|f| f.mid = "video".into(), 4, 0),
            (
                |f| f.payloads = Box::new([(96, 90_000), (98, 90_000)]),
                size_of::<(u8, u32)>(),
                0,
            ),
            (|f| f.feedback_payloads = Box::new([96, 98]), 1, 0),
            (
                |f| f.repairs = Box::new([(97, 96)]),
                size_of::<(u8, u8)>(),
                0,
            ),
            (|f| f.nack_payloads = Box::new([96, 98]), 1, 0),
            (
                |f| f.ssrc_repairs = Box::new([(12, 11)]),
                size_of::<(u32, u32)>(),
                owner,
            ),
            (|f| f.ssrcs = Box::new([11, 12]), size_of::<u32>(), owner),
            (
                |f| f.rids = Box::new(["h".into()]),
                size_of::<Box<str>>() + 1,
                0,
            ),
            (
                |f| f.extensions = Box::new([(1, MID_URI.into())]),
                size_of::<(u8, Box<str>)>() + MID_URI.len(),
                0,
            ),
        ];
        for (change, additional, indexes) in changes {
            let mut grown = facts.clone();
            change(&mut grown);
            assert_eq!(facts_bytes(&grown), Some(baseline + additional));
            let mid = if grown
                .extensions
                .iter()
                .any(|(_, uri)| uri.as_ref() == MID_URI)
            {
                size_of::<(u64, u8)>()
            } else {
                0
            };
            assert_eq!(
                media_bytes(&grown),
                Some(
                    baseline + additional + groups + mid + indexes + grown.mid.len()
                        - facts.mid.len()
                )
            );
            assert_eq!(facts_bytes(&facts), Some(baseline));
        }
    }
}
