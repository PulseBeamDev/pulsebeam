use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    mem::size_of,
};

use super::{
    EgressUpdate, MediaEgress, QueuedMedia, RetainedKey, RetainedPacket, RetiringSender, Sender,
};
use crate::{
    AcceptError, MediaKind, RtpConfigurationError, RtpExtension, RtpWrite,
    scheduler::SenderScheduler,
};

mod retirement;
pub(super) use retirement::{RetirementReservation, retirement_bytes};

pub(super) const RETIREMENT_BYTES: usize = size_of::<RetiringSender>();
pub(super) const FIR_BYTES: usize = size_of::<(u32, u8)>();
pub(super) const REPAIR_OWNER_BYTES: usize = size_of::<(u64, u64)>();
pub(super) const REPAIR_INDEX_BYTES: usize = size_of::<(u64, RetainedKey)>();
pub(super) const ISSUED_SSRC_BYTES: usize = size_of::<u32>();
pub(super) const MID_GROUP_BYTES: usize = size_of::<(Box<str>, BTreeSet<u64>)>();
pub(super) const RETAINED_PACKET_BYTES: usize = size_of::<(u16, RetainedPacket)>();
pub(super) const RETAINED_EXPIRY_BYTES: usize = size_of::<(std::time::Instant, RetainedKey)>();
pub(super) const RETAINED_GROUP_BYTES: usize = size_of::<(u64, BTreeMap<u16, RetainedPacket>)>();

pub(super) fn retained_entry_bytes(capacity: usize) -> Option<usize> {
    capacity
        .checked_add(RETAINED_PACKET_BYTES)?
        .checked_add(RETAINED_EXPIRY_BYTES)
}

pub(super) fn queue_bytes(queue: &VecDeque<QueuedMedia>) -> usize {
    queue.capacity().saturating_mul(size_of::<QueuedMedia>())
}

pub(super) fn extension_bytes(media: &RtpWrite) -> usize {
    media
        .extensions
        .capacity()
        .saturating_mul(size_of::<RtpExtension>())
}

pub(super) fn mid_bytes(mid: &str) -> Option<usize> {
    MID_GROUP_BYTES.checked_add(mid.len())
}

// Fund possible entries before acceptance, rather than failing lazy readiness.
fn sender_index_bytes(sender: &Sender) -> Option<usize> {
    let by_ssrc = size_of::<(u32, u64)>();
    let mid_member = size_of::<u64>();
    let rtcp_sender = size_of::<u64>();
    let report_ready = size_of::<u64>();
    let keyframe_ready = if sender.facts.kind == MediaKind::Video {
        size_of::<u64>()
    } else {
        0
    };
    let padding_sender = if sender.facts.retransmission_payload_type.is_some() {
        size_of::<u64>()
    } else {
        0
    };
    let eligible = size_of::<(u64, usize)>();
    let deficit = SenderScheduler::sender_payload_bytes();
    [
        by_ssrc,
        mid_member,
        rtcp_sender,
        report_ready,
        keyframe_ready,
        padding_sender,
        eligible,
        deficit,
    ]
    .into_iter()
    .try_fold(0usize, usize::checked_add)
}

fn sender_bytes(sender: &Sender) -> Option<usize> {
    let facts = &sender.facts;
    let mut bytes = size_of::<(u64, Sender)>()
        .checked_add(sender_index_bytes(sender)?)?
        .checked_add(facts.mid.len())?
        .checked_add(facts.rtcp.as_ref().map_or(0, |rtcp| rtcp.cname.capacity()))?
        .checked_add(
            facts
                .forwarded_extensions
                .len()
                .checked_mul(size_of::<(u8, Box<str>)>())?,
        )?
        .checked_add(sender.rid.as_ref().map_or(0, String::capacity))?;
    for (_, uri) in &facts.forwarded_extensions {
        bytes = bytes.checked_add(uri.len())?;
    }
    Some(bytes)
}

pub(super) fn bootstrap(
    senders: &mut BTreeMap<u64, Sender>,
    by_mid: &BTreeMap<Box<str>, BTreeSet<u64>>,
    issued_ssrcs: &BTreeSet<u32>,
    retiring: &mut VecDeque<RetiringSender>,
    limit: usize,
) -> Result<usize, AcceptError> {
    let mut bytes = by_mid
        .keys()
        .try_fold(0usize, |bytes, mid| bytes.checked_add(mid_bytes(mid)?))
        .and_then(|bytes| bytes.checked_add(issued_ssrcs.len().checked_mul(ISSUED_SSRC_BYTES)?))
        .ok_or(AcceptError::SessionLimitExceeded)?;
    if bytes > limit {
        return Err(AcceptError::SessionLimitExceeded);
    }
    for sender in senders.values_mut() {
        sender.runtime_bytes = sender_bytes(sender).ok_or(AcceptError::SessionLimitExceeded)?;
        bytes = bytes
            .checked_add(sender.runtime_bytes)
            .filter(|bytes| *bytes <= limit)
            .ok_or(AcceptError::SessionLimitExceeded)?;
    }
    bytes
        .checked_add(
            senders
                .len()
                .checked_mul(RETIREMENT_BYTES)
                .ok_or(AcceptError::SessionLimitExceeded)?,
        )
        .filter(|bytes| *bytes <= limit)
        .ok_or(AcceptError::SessionLimitExceeded)?;
    retiring
        .try_reserve_exact(senders.len())
        .map_err(|_| AcceptError::SessionLimitExceeded)?;
    bytes
        .checked_add(retirement_bytes(retiring))
        .filter(|bytes| *bytes <= limit)
        .ok_or(AcceptError::SessionLimitExceeded)
}

impl MediaEgress {
    pub(super) fn stage_runtime(
        &self,
        update: &mut EgressUpdate,
    ) -> Result<(), RtpConfigurationError> {
        use RtpConfigurationError as Error;
        let mut mids = BTreeMap::<&str, (usize, usize)>::new();
        for key in &update.removed {
            let sender = self.senders.get(key).ok_or(Error::ConnectionState)?;
            let count = &mut mids.entry(&sender.facts.mid).or_default().0;
            *count = count.checked_add(1).ok_or(Error::ConnectionState)?;
        }
        for sender in update.new_senders.values() {
            let count = &mut mids.entry(&sender.facts.mid).or_default().1;
            *count = count.checked_add(1).ok_or(Error::Capacity)?;
        }
        let mut removed_mids = 0usize;
        let mut added_mids = 0usize;
        for (mid, (removed, added)) in mids {
            let current = self.by_mid.get(mid).map_or(0, BTreeSet::len);
            let next = current
                .checked_sub(removed)
                .and_then(|count| count.checked_add(added))
                .ok_or(Error::ConnectionState)?;
            if current == 0 && next > 0 {
                added_mids = added_mids
                    .checked_add(mid_bytes(mid).ok_or(Error::Capacity)?)
                    .ok_or(Error::Capacity)?;
            } else if current > 0 && next == 0 {
                removed_mids = removed_mids
                    .checked_add(mid_bytes(mid).ok_or(Error::ConnectionState)?)
                    .ok_or(Error::ConnectionState)?;
            }
        }
        let mut released_slots = 0usize;
        let removed = update
            .removed
            .iter()
            .try_fold(removed_mids, |bytes, key| {
                let sender = self.senders.get(key)?;
                let cleanup = sender.retirement_reserved
                    && (!sender.queue.is_empty()
                        || !sender.requested_repairs.is_empty()
                        || self.retained.contains_key(key));
                released_slots = released_slots
                    .checked_add(usize::from(sender.retirement_reserved && !cleanup))?;
                let queue = if cleanup {
                    0
                } else {
                    queue_bytes(&sender.queue)
                };
                bytes
                    .checked_add(sender.runtime_bytes)?
                    .checked_add(sender.fir_sequences.len().checked_mul(FIR_BYTES)?)?
                    .checked_add(queue)
            })
            .ok_or(Error::ConnectionState)?;
        let added = update
            .new_senders
            .values_mut()
            .try_fold(added_mids, |bytes, sender| {
                sender.runtime_bytes = sender_bytes(sender)?;
                let guards = ISSUED_SSRC_BYTES.checked_add(
                    if sender.facts.retransmission_payload_type.is_some() {
                        ISSUED_SSRC_BYTES
                    } else {
                        0
                    },
                )?;
                bytes.checked_add(sender.runtime_bytes)?.checked_add(guards)
            })
            .ok_or(Error::Capacity)?;
        let slots = self
            .retirement_slots
            .checked_sub(released_slots)
            .and_then(|slots| slots.checked_add(update.new_senders.len()))
            .ok_or(Error::Capacity)?;
        let total = self
            .runtime_bytes
            .checked_sub(removed)
            .ok_or(Error::ConnectionState)?
            .checked_add(added)
            .ok_or(Error::Capacity)?;
        update.retirement = self
            .prepare_retirement(slots, total)
            .ok_or(Error::Capacity)?;
        Ok(())
    }

    pub(super) fn prepare_workspace(
        &self,
        required: usize,
        total: usize,
    ) -> Option<(Option<Vec<u8>>, usize)> {
        let pending = match &self.pending {
            Some(super::PendingTransmit::Original { packet, .. }) => packet.capacity(),
            _ => 0,
        };
        let required = required.max(pending);
        let old = self.original_workspace.capacity();
        if old >= required {
            return Some((None, total));
        }
        if total.checked_sub(old)?.checked_add(required)? > self.max_runtime_bytes {
            return None;
        }
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(required).ok()?;
        let projected = total.checked_sub(old)?.checked_add(buffer.capacity())?;
        (projected <= self.max_runtime_bytes).then_some((Some(buffer), projected))
    }

    pub(super) fn recycle_original(&mut self, mut packet: Vec<u8>) {
        if self.original_workspace.capacity() == 0 {
            packet.clear();
            self.original_workspace = packet;
        } else {
            debug_assert!(self.original_workspace.capacity() >= packet.capacity());
            self.runtime_bytes = self.runtime_bytes.saturating_sub(packet.capacity());
        }
    }

    pub(super) fn reserve_media(
        &mut self,
        key: u64,
        media: &RtpWrite,
        packet_bytes: usize,
    ) -> bool {
        let Some(sender) = self.senders.get(&key) else {
            return false;
        };
        let Some(slots) = self
            .retirement_slots
            .checked_add(usize::from(!sender.retirement_reserved))
        else {
            return false;
        };
        let Some(mut total) = self
            .runtime_bytes
            .checked_add(extension_bytes(media))
            .filter(|bytes| *bytes <= self.max_runtime_bytes)
        else {
            return false;
        };
        let mut queue = None;
        if sender.queue.len() == sender.queue.capacity() {
            let Some(capacity) = sender
                .queue
                .capacity()
                .checked_mul(2)
                .map(|capacity| capacity.max(1))
            else {
                return false;
            };
            let old = queue_bytes(&sender.queue);
            let Some(minimum) = capacity
                .checked_mul(size_of::<QueuedMedia>())
                .and_then(|bytes| bytes.checked_sub(old))
            else {
                return false;
            };
            if total
                .checked_add(minimum)
                .is_none_or(|bytes| bytes > self.max_runtime_bytes)
            {
                return false;
            }
            let mut expanded = VecDeque::new();
            if expanded.try_reserve_exact(capacity).is_err() {
                return false;
            }
            let Some(projected) = queue_bytes(&expanded)
                .checked_sub(old)
                .and_then(|growth| total.checked_add(growth))
                .filter(|bytes| *bytes <= self.max_runtime_bytes)
            else {
                return false;
            };
            queue = Some(expanded);
            total = projected;
        }
        let Some((workspace, total)) = self.prepare_workspace(packet_bytes, total) else {
            return false;
        };
        let Some(retirement) = self.prepare_retirement(slots, total) else {
            return false;
        };
        let Some(sender) = self.senders.get_mut(&key) else {
            return false;
        };
        if let Some(buffer) = workspace {
            self.original_workspace = buffer;
        }
        if let Some(mut expanded) = queue {
            expanded.append(&mut sender.queue);
            sender.queue = expanded;
        }
        sender.retirement_reserved = true;
        self.commit_retirement(retirement);
        true
    }

    pub(super) fn reserve_repair_request(&mut self, key: u64) -> bool {
        let Some(sender) = self.senders.get(&key) else {
            return false;
        };
        let Some(slots) = self
            .retirement_slots
            .checked_add(usize::from(!sender.retirement_reserved))
        else {
            return false;
        };
        let Some(total) = self
            .runtime_bytes
            .checked_add(REPAIR_OWNER_BYTES)
            .and_then(|bytes| bytes.checked_add(REPAIR_INDEX_BYTES))
        else {
            return false;
        };
        let Some(retirement) = self.prepare_retirement(slots, total) else {
            return false;
        };
        let Some(sender) = self.senders.get_mut(&key) else {
            return false;
        };
        sender.retirement_reserved = true;
        self.commit_retirement(retirement);
        true
    }

    pub(super) fn release_repair_index(&mut self, request: u64) {
        if self.repair_requests.remove(&request).is_some() {
            self.runtime_bytes = self.runtime_bytes.saturating_sub(REPAIR_INDEX_BYTES);
        }
    }
}
