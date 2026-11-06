#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "packet construction uses bounded offsets and validated wire lengths"
)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    Event, MediaKind, MediaPayloadBitrate, RtpWrite, SenderStats, WriteError,
    congestion::{
        Config, ControllerInput, EcnValidation, FeedbackSample, Readiness, SafeRtpEnvelope,
        ScreamController, UpdateError,
    },
    negotiation::EgressSenderFacts,
    packet::RtpPacket,
    scheduler::SenderScheduler,
    time::ProtocolClock,
    transport::{
        DatagramKind, PreparedRtpIdentity, PreparedTransmit, RtpAdmission, RtpService, Transport,
        TransportError,
    },
};

mod layout;
mod runtime;

const MAX_PACED_TRANSPORT_BYTES: usize = 8 * 1024 * 1024;
const RTP_TRANSPORT_ALLOWANCE: usize = 96;
const REPAIR_MAINTENANCE_BUDGET: usize = 256;
pub(crate) struct MediaEgress {
    senders: BTreeMap<u64, Sender>,
    next_sender: Option<u64>,
    by_ssrc: BTreeMap<u32, u64>,
    by_mid: BTreeMap<Box<str>, BTreeSet<u64>>,
    issued_ssrcs: BTreeSet<u32>,
    retiring: VecDeque<RetiringSender>,
    retirement_slots: usize,
    rtcp_senders: BTreeSet<u64>,
    padding_senders: BTreeSet<u64>,
    keyframe_ready: BTreeSet<u64>,
    report_ready: BTreeSet<u64>,
    queued_packets: usize,
    eligible: BTreeMap<u64, usize>,
    queued_payload_bytes: usize,
    queued_transport_bytes: usize,
    queue_time_updated_at: Instant,
    queue_active_time: Duration,
    queue_paused: bool,
    queue_enqueued_time_sum: u128,
    max_payload_bytes: usize,
    max_retransmission_bytes: usize,
    max_runtime_bytes: usize,
    runtime_bytes: usize,
    original_workspace: Vec<u8>,
    retained: BTreeMap<u64, BTreeMap<u16, RetainedPacket>>,
    retained_expiry: BTreeSet<(Instant, RetainedKey)>,
    retained_bytes: usize,
    retained_runtime_bytes: usize,
    repair_requests: BTreeMap<u64, RetainedKey>,
    next_repair_request: Option<u64>,
    next_twcc: u64,
    pending: Option<PendingTransmit>,
    last_send_attempt: Option<Instant>,
    scheduler: SenderScheduler,
    controller: ScreamController,
    controller_origin: Instant,
    envelope: Option<SafeRtpEnvelope>,
    active_path: Option<u64>,
    path_available: bool,
    bandwidth_estimate: u64,
    desired_bitrate: u64,
    clock: ProtocolClock,
    estimate_changed: Option<MediaPayloadBitrate>,
    next_rtcp: Instant,
    rtcp_cursor: u64,
    offer_window_started: Instant,
    offered_bytes: u64,
    offered_rate: u64,
    rtp_bytes_in_flight: u64,
}

pub(crate) struct EgressUpdate {
    new_senders: BTreeMap<u64, Sender>,
    adopted: BTreeSet<u64>,
    removed: BTreeSet<u64>,
    active: BTreeMap<u64, bool>,
    next_sender: Option<u64>,
    retirement: runtime::RetirementReservation,
}

struct RetiringSender {
    sender: u64,
    queue: VecDeque<QueuedMedia>,
    repairs: BTreeMap<u64, u64>,
}

struct Sender {
    facts: EgressSenderFacts,
    bootstrap: bool,
    runtime_bytes: usize,
    retirement_reserved: bool,
    active: bool,
    rid: Option<String>,
    rid_extension_id: Option<u8>,
    repaired_rid_extension_id: Option<u8>,
    ssrc: u32,
    rtx_ssrc: u32,
    next_rtx_sequence: u64,
    latest_rtp: Option<(u32, Instant)>,
    highest_sequence: Option<u64>,
    queue: VecDeque<QueuedMedia>,
    queued_payload_bytes: usize,
    requested_repairs: BTreeMap<u64, u64>,
    keyframe_requested: bool,
    reported_sequence: Option<u32>,
    pending_report: Option<(Instant, u32)>,
    fir_sequences: BTreeMap<u32, u8>,
    committed_packets: u64,
    committed_payload_bytes: u64,
}

struct QueuedMedia {
    enqueued_at: Duration,
    media: RtpWrite,
    payload_bytes: usize,
    transport_estimate: usize,
}

// A queued NACK names the retained original, not the next packet with the same
// wire sequence after rollover. Expiry uses the same exact identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RetainedKey {
    sender: u64,
    index: u64,
}

struct RetainedPacket {
    index: u64,
    timestamp: u32,
    packet: Vec<u8>,
    retained_at: Instant,
    last_repair_at: Option<Instant>,
    transport_bytes: usize,
}

enum PendingTransmit {
    Original {
        sender: u64,
        sequence: u16,
        timestamp: u32,
        twcc: Option<u16>,
        packet: Vec<u8>,
    },
    Repair {
        sender: u64,
        request: u64,
        original_index: u64,
        sequence: u16,
        twcc: Option<u16>,
    },
    Control,
    Padding {
        sender: u64,
        ssrc: u32,
        sequence: u16,
        twcc: Option<u16>,
    },
}

pub(crate) enum PrepareResult {
    Prepared,
    Blocked,
    Fatal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EgressStats {
    pub(crate) queued_transport_bytes: usize,
    pub(crate) bandwidth_estimate: u64,
    pub(crate) pacing_bitrate: u64,
}

impl MediaEgress {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        facts: Box<[EgressSenderFacts]>,
        randomness: &[u8; 32],
        max_payload_bytes: usize,
        max_retransmission_bytes: usize,
        max_runtime_bytes: usize,
        accepted_at: Instant,
        clock: ProtocolClock,
    ) -> Result<Self, crate::AcceptError> {
        let mut senders: BTreeMap<_, _> = facts
            .into_vec()
            .into_iter()
            .map(|facts| {
                let (ssrc, rtx_ssrc, rtx_sequence) = sender_rtp_identity(randomness, facts.id);
                (
                    facts.id.value(),
                    Sender {
                        bootstrap: true,
                        active: true,
                        runtime_bytes: 0,
                        retirement_reserved: true,
                        rid: None,
                        rid_extension_id: None,
                        repaired_rid_extension_id: None,
                        ssrc,
                        rtx_ssrc,
                        next_rtx_sequence: u64::from(rtx_sequence),
                        latest_rtp: None,
                        highest_sequence: None,
                        queue: VecDeque::new(),
                        queued_payload_bytes: 0,
                        requested_repairs: BTreeMap::new(),
                        keyframe_requested: false,
                        reported_sequence: None,
                        pending_report: None,
                        fir_sequences: BTreeMap::new(),
                        committed_packets: 0,
                        committed_payload_bytes: 0,
                        facts,
                    },
                )
            })
            .collect();
        let twcc_seed = sender_seed(randomness, u64::MAX);
        let next_sender = senders
            .last_key_value()
            .map_or(Some(1), |(&id, _)| id.checked_add(1));
        let by_ssrc = senders.iter().map(|(&id, s)| (s.ssrc, id)).collect();
        let mut by_mid: BTreeMap<Box<str>, BTreeSet<u64>> = BTreeMap::new();
        let mut issued_ssrcs = BTreeSet::new();
        let mut padding_senders = BTreeSet::new();
        for (&id, s) in &senders {
            by_mid.entry(s.facts.mid.clone()).or_default().insert(id);
            issued_ssrcs.insert(s.ssrc);
            if s.facts.retransmission_payload_type.is_some() {
                issued_ssrcs.insert(s.rtx_ssrc);
                padding_senders.insert(id);
            }
        }
        let retirement_slots = senders.len();
        let mut retiring = VecDeque::new();
        let runtime_bytes = runtime::bootstrap(
            &mut senders,
            &by_mid,
            &issued_ssrcs,
            &mut retiring,
            max_runtime_bytes,
        )?;
        let mut owner = Self {
            senders,
            next_sender,
            by_ssrc,
            by_mid,
            issued_ssrcs,
            retiring,
            retirement_slots,
            rtcp_senders: BTreeSet::new(),
            padding_senders,
            keyframe_ready: BTreeSet::new(),
            report_ready: BTreeSet::new(),
            queued_packets: 0,
            eligible: BTreeMap::new(),
            queued_payload_bytes: 0,
            queued_transport_bytes: 0,
            queue_time_updated_at: accepted_at,
            queue_active_time: Duration::ZERO,
            queue_paused: true,
            queue_enqueued_time_sum: 0,
            max_payload_bytes,
            max_retransmission_bytes,
            max_runtime_bytes,
            runtime_bytes,
            retained: BTreeMap::new(),
            retained_expiry: BTreeSet::new(),
            retained_bytes: 0,
            retained_runtime_bytes: 0,
            original_workspace: Vec::new(),
            repair_requests: BTreeMap::new(),
            next_repair_request: Some(0),
            next_twcc: u64::from(u16::from_be_bytes([twcc_seed[0], twcc_seed[1]])),
            pending: None,
            last_send_attempt: None,
            scheduler: SenderScheduler::default(),
            controller: ScreamController::new(Config::default())
                .map_err(|_| crate::AcceptError::InvalidConfiguration)?,
            controller_origin: accepted_at,
            envelope: None,
            active_path: None,
            path_available: false,
            bandwidth_estimate: 0,
            desired_bitrate: 0,
            clock,
            estimate_changed: None,
            next_rtcp: accepted_at
                .checked_add(Duration::from_secs(1))
                .unwrap_or(accepted_at),
            rtcp_cursor: 0,
            offer_window_started: accepted_at,
            offered_bytes: 0,
            offered_rate: 0,
            rtp_bytes_in_flight: 0,
        };
        owner.update_estimate(owner.controller.snapshot().target_media_payload_rate);
        owner.estimate_changed = None;
        Ok(owner)
    }

    #[allow(
        clippy::result_large_err,
        reason = "keep caller-owned input inline instead of allocating an error wrapper"
    )]
    pub(crate) fn admit(
        &mut self,
        at: Instant,
        mid: &str,
        media: RtpWrite,
    ) -> Result<(), (WriteError, RtpWrite)> {
        self.admit_encoding(at, mid, None, media)
    }

    #[allow(
        clippy::result_large_err,
        reason = "preserve caller-owned packet on rejection"
    )]
    pub(crate) fn admit_encoding(
        &mut self,
        at: Instant,
        mid: &str,
        ssrc: Option<u32>,
        media: RtpWrite,
    ) -> Result<(), (WriteError, RtpWrite)> {
        let result = (|| {
            let mut matches = self.by_mid.get(mid).into_iter().flatten().filter(|id| {
                self.senders
                    .get(id)
                    .is_some_and(|s| s.active && ssrc.is_none_or(|ssrc| s.ssrc == ssrc))
            });
            let index = *matches.next().ok_or(WriteError::UnknownStream)?;
            if matches.next().is_some() {
                return Err(WriteError::UnknownStream);
            }
            if media.sequence >= (1_u64 << 48) {
                return Err(WriteError::IndexExhausted);
            }
            let state = self.senders.get(&index).ok_or(WriteError::UnknownStream)?;
            if media.extensions.iter().enumerate().any(|(i, value)| {
                value.value.len() > usize::from(u8::MAX)
                    || media
                        .extensions
                        .iter()
                        .take(i)
                        .any(|other| other.uri == value.uri)
                    || !state
                        .facts
                        .forwarded_extensions
                        .iter()
                        .any(|(_, uri)| uri.as_ref() == value.uri.as_ref())
                        && !(state.facts.dependency_extension_id.is_some()
                            && crate::negotiation::DEPENDENCY_URIS.contains(&value.uri.as_ref()))
                        && !(state.facts.playout_extension_id.is_some()
                            && value.uri.as_ref()
                                == "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
                            && value.value.len() == 3)
            }) {
                return Err(WriteError::InvalidRtp);
            }
            let layout =
                layout::PacketLayout::original(state, &media).ok_or(WriteError::InvalidRtp)?;
            if layout.bytes > usize::from(u16::MAX) {
                return Err(WriteError::InvalidRtp);
            }
            let payload_bytes = media.payload.len();
            let transport_estimate = payload_bytes
                .saturating_add(RTP_TRANSPORT_ALLOWANCE)
                .saturating_add(
                    media
                        .extensions
                        .iter()
                        .map(|value| value.value.len().saturating_add(2))
                        .sum::<usize>(),
                );
            if transport_estimate > usize::from(u16::MAX) {
                return Err(WriteError::InvalidRtp);
            }
            self.offered_bytes = self.offered_bytes.saturating_add(payload_bytes as u64);
            if payload_bytes
                > self
                    .max_payload_bytes
                    .saturating_sub(self.queued_payload_bytes)
                || transport_estimate
                    > MAX_PACED_TRANSPORT_BYTES.saturating_sub(self.queued_transport_bytes)
            {
                return Err(WriteError::WouldBlock);
            }
            if !self.reserve_media(index, &media, layout.bytes) {
                return Err(WriteError::WouldBlock);
            }
            self.queued_payload_bytes = self.queued_payload_bytes.saturating_add(payload_bytes);
            self.queued_transport_bytes = self
                .queued_transport_bytes
                .saturating_add(transport_estimate);
            self.queued_packets = self.queued_packets.saturating_add(1);
            let state = self
                .senders
                .get_mut(&index)
                .ok_or(WriteError::UnknownStream)?;
            state.queued_payload_bytes = state.queued_payload_bytes.saturating_add(payload_bytes);
            Ok((index, payload_bytes, transport_estimate))
        })();
        match result {
            Ok((index, payload_bytes, transport_estimate)) => {
                let enqueued_at = self.queue_time_at(at);
                self.queue_enqueued_time_sum = self
                    .queue_enqueued_time_sum
                    .saturating_add(enqueued_at.as_nanos());
                let Some(state) = self.senders.get_mut(&index) else {
                    return Err((WriteError::UnknownStream, media));
                };
                self.eligible.entry(index).or_insert(transport_estimate);
                state.queue.push_back(QueuedMedia {
                    enqueued_at,
                    media,
                    payload_bytes,
                    transport_estimate,
                });
                self.refresh_pacer_queue(at);
                Ok(())
            }
            Err(error) => Err((error, media)),
        }
    }

    pub(crate) fn set_desired_bitrate(&mut self, bitrate: Option<MediaPayloadBitrate>) {
        self.desired_bitrate = bitrate.map_or(0, MediaPayloadBitrate::as_bps);
    }

    pub(crate) fn discard_pending(&mut self, mid: &str) -> Result<(), WriteError> {
        if self.pending.is_some() {
            return Err(WriteError::InvalidState);
        }
        let indexes: Vec<_> = self
            .by_mid
            .get(mid)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        if indexes.is_empty() {
            return Err(WriteError::UnknownStream);
        }
        for index in indexes {
            while self.pop_head(index).is_some() {}
        }
        self.refresh_pacer_queue(self.queue_time_updated_at);
        Ok(())
    }

    pub(crate) fn stage_configuration(
        &self,
        desired: &[(crate::rtp_config::PhysicalBinding, bool)],
        randomness: &[u8; 32],
        removed: &[u32],
        scope: impl Into<crate::rtp_config::ConfigurationScope>,
    ) -> Result<EgressUpdate, crate::RtpConfigurationError> {
        use crate::{RtpConfigurationError as Error, rtp_config::ConfigurationScope};
        let scope = scope.into();
        if self.pending.is_some() {
            return Err(Error::ConnectionState);
        }
        let mut update = EgressUpdate {
            new_senders: BTreeMap::new(),
            adopted: BTreeSet::new(),
            removed: BTreeSet::new(),
            active: if scope == ConfigurationScope::Bootstrap {
                self.senders.keys().map(|&key| (key, false)).collect()
            } else {
                BTreeMap::new()
            },
            next_sender: self.next_sender,
            retirement: runtime::RetirementReservation::current(self),
        };
        let mut issued = BTreeSet::new();
        for (physical, active) in desired.iter().filter(|(physical, _)| physical.outbound) {
            let encoding = physical.encoding();
            if let Some(keys) = self.by_mid.get(physical.binding.mid.as_str()) {
                update.removed.extend(
                    keys.iter()
                        .copied()
                        .filter(|key| self.senders.get(key).is_some_and(|sender| sender.bootstrap)),
                );
            }
            if let Some(&key) = self.by_ssrc.get(&encoding.primary) {
                let sender = self.senders.get(&key).ok_or(Error::ConnectionState)?;
                let facts = physical.egress_facts(sender.facts.id);
                let mut previous = sender.facts.clone();
                previous.forwarded_extensions.sort_by(|a, b| a.0.cmp(&b.0));
                if previous != facts
                    || encoding.repair.is_some_and(|ssrc| ssrc != sender.rtx_ssrc)
                    || sender.rid != encoding.rid
                {
                    return Err(Error::Collision);
                }
                update.adopted.insert(key);
                update.active.insert(key, *active);
                continue;
            }
            for ssrc in std::iter::once(encoding.primary).chain(encoding.repair) {
                if self.issued_ssrcs.contains(&ssrc) || !issued.insert(ssrc) {
                    return Err(Error::Collision);
                }
            }
            let key = update.next_sender.ok_or(Error::Capacity)?;
            let id = crate::id::SenderId::new(key).ok_or(Error::Capacity)?;
            update.next_sender = key.checked_add(1);
            let facts = physical.egress_facts(id);
            let (_, fallback_repair, rtx_sequence) = sender_rtp_identity(randomness, id);
            update.new_senders.insert(
                key,
                Sender {
                    facts,
                    bootstrap: false,
                    active: *active,
                    runtime_bytes: 0,
                    retirement_reserved: true,
                    rid: encoding.rid.clone(),
                    rid_extension_id: physical.extension_id(crate::RID_EXTENSION),
                    repaired_rid_extension_id: physical.extension_id(crate::REPAIRED_RID_EXTENSION),
                    ssrc: encoding.primary,
                    rtx_ssrc: encoding.repair.unwrap_or(fallback_repair),
                    next_rtx_sequence: u64::from(rtx_sequence),
                    latest_rtp: None,
                    highest_sequence: None,
                    queue: VecDeque::new(),
                    queued_payload_bytes: 0,
                    requested_repairs: BTreeMap::new(),
                    keyframe_requested: false,
                    reported_sequence: None,
                    pending_report: None,
                    fir_sequences: BTreeMap::new(),
                    committed_packets: 0,
                    committed_payload_bytes: 0,
                },
            );
            update.active.insert(key, *active);
        }
        for primary in removed {
            let key = self.by_ssrc.get(primary).ok_or(Error::Invalid)?;
            if update.active.contains_key(key) {
                return Err(Error::Invalid);
            }
            update.removed.insert(*key);
        }
        if scope == ConfigurationScope::Complete {
            update.removed.extend(
                self.senders
                    .keys()
                    .filter(|key| !update.active.contains_key(key))
                    .copied(),
            );
        }
        update.removed.retain(|key| !update.adopted.contains(key));
        for key in &update.removed {
            update.active.remove(key);
        }
        self.stage_runtime(&mut update)?;
        Ok(update)
    }

    pub(crate) fn retiring_ssrcs<'a>(
        &'a self,
        update: &'a EgressUpdate,
    ) -> impl Iterator<Item = u32> + 'a {
        update
            .removed
            .iter()
            .filter_map(|key| self.senders.get(key))
            .flat_map(|sender| {
                std::iter::once(sender.ssrc).chain(
                    sender
                        .facts
                        .retransmission_payload_type
                        .map(|_| sender.rtx_ssrc),
                )
            })
    }

    pub(crate) fn set_encoding_active(
        &mut self,
        primary: u32,
        active: bool,
    ) -> Result<(), crate::RtpConfigurationError> {
        let key = *self
            .by_ssrc
            .get(&primary)
            .ok_or(crate::RtpConfigurationError::ConnectionState)?;
        self.set_sender_active(key, active);
        Ok(())
    }

    fn set_sender_active(&mut self, key: u64, active: bool) {
        let Some(sender) = self.senders.get_mut(&key) else {
            return;
        };
        sender.active = active;
        if !active {
            sender.keyframe_requested = false;
            self.keyframe_ready.remove(&key);
            self.rtcp_senders.remove(&key);
            self.padding_senders.remove(&key);
            self.cancel_sender_work(key);
        } else {
            if sender.committed_packets > 0 {
                self.rtcp_senders.insert(key);
            }
            if sender.facts.retransmission_payload_type.is_some() {
                self.padding_senders.insert(key);
            }
        }
    }

    pub(crate) fn commit_configuration(&mut self, update: EgressUpdate) {
        for key in update.removed {
            self.retire_sender(key);
        }
        for (&key, &active) in &update.active {
            if let Some(sender) = self.senders.get_mut(&key)
                && update.adopted.contains(&key)
            {
                sender.bootstrap = false;
            }
            self.set_sender_active(key, active);
        }
        for (key, sender) in update.new_senders {
            self.by_ssrc.insert(sender.ssrc, key);
            self.by_mid
                .entry(sender.facts.mid.clone())
                .or_default()
                .insert(key);
            self.issued_ssrcs.insert(sender.ssrc);
            if sender.facts.retransmission_payload_type.is_some() {
                self.issued_ssrcs.insert(sender.rtx_ssrc);
                if sender.active {
                    self.padding_senders.insert(key);
                }
            }
            self.senders.insert(key, sender);
        }
        self.commit_retirement(update.retirement);
        if self.senders.is_empty() {
            self.runtime_bytes = self
                .runtime_bytes
                .saturating_sub(self.original_workspace.capacity());
            self.original_workspace = Vec::new();
        }
        self.next_sender = update.next_sender;
        self.refresh_pacer_queue(self.queue_time_updated_at);
    }

    fn retire_sender(&mut self, key: u64) {
        let Some(sender) = self.senders.remove(&key) else {
            return;
        };
        self.by_ssrc.remove(&sender.ssrc);
        if let Some(keys) = self.by_mid.get_mut(sender.facts.mid.as_ref()) {
            keys.remove(&key);
            if keys.is_empty() {
                self.by_mid.remove(sender.facts.mid.as_ref());
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::MID_GROUP_BYTES)
                    .saturating_sub(sender.facts.mid.len());
            }
        }
        self.eligible.remove(&key);
        self.scheduler.retire(key);
        self.rtcp_senders.remove(&key);
        self.padding_senders.remove(&key);
        self.keyframe_ready.remove(&key);
        self.report_ready.remove(&key);
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(sender.runtime_bytes)
            .saturating_sub(
                sender
                    .fir_sequences
                    .len()
                    .saturating_mul(runtime::FIR_BYTES),
            );
        let queue_bytes = runtime::queue_bytes(&sender.queue);
        if sender.retirement_reserved {
            if !sender.queue.is_empty()
                || !sender.requested_repairs.is_empty()
                || self.retained.contains_key(&key)
            {
                self.enqueue_retirement(RetiringSender {
                    sender: key,
                    queue: sender.queue,
                    repairs: sender.requested_repairs,
                });
            } else {
                self.retirement_slots = self.retirement_slots.saturating_sub(1);
                self.runtime_bytes = self.runtime_bytes.saturating_sub(queue_bytes);
            }
        } else {
            self.runtime_bytes = self.runtime_bytes.saturating_sub(queue_bytes);
            // A paused job already owns the cleanup reservation and will also
            // reclaim this terminal sender's cache without a second allocation.
            debug_assert!(sender.queue.is_empty() && sender.requested_repairs.is_empty());
        }
    }

    fn cancel_sender_work(&mut self, key: u64) {
        let Some(sender) = self.senders.get_mut(&key) else {
            return;
        };
        let queue = std::mem::take(&mut sender.queue);
        let repairs = std::mem::take(&mut sender.requested_repairs);
        sender.queued_payload_bytes = 0;
        self.eligible.remove(&key);
        if !queue.is_empty() || !repairs.is_empty() {
            debug_assert!(sender.retirement_reserved);
            sender.retirement_reserved = false;
            self.enqueue_retirement(RetiringSender {
                sender: key,
                queue,
                repairs,
            });
        } else {
            self.runtime_bytes = self
                .runtime_bytes
                .saturating_sub(runtime::queue_bytes(&queue));
        }
    }

    fn clean_retired(&mut self, at: Instant) {
        if self.retiring.is_empty() {
            self.release_idle_retirement();
            return;
        }
        for _ in 0..REPAIR_MAINTENANCE_BUDGET {
            let Some(job) = self.retiring.front_mut() else {
                break;
            };
            if let Some(queued) = job.queue.pop_front() {
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::extension_bytes(&queued.media));
                self.queue_enqueued_time_sum = self
                    .queue_enqueued_time_sum
                    .saturating_sub(queued.enqueued_at.as_nanos());
                self.queued_packets = self.queued_packets.saturating_sub(1);
                self.queued_payload_bytes = self
                    .queued_payload_bytes
                    .saturating_sub(queued.payload_bytes);
                self.queued_transport_bytes = self
                    .queued_transport_bytes
                    .saturating_sub(queued.transport_estimate);
            } else if let Some((_, request)) = job.repairs.pop_first() {
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::REPAIR_OWNER_BYTES);
                self.release_repair_index(request);
            } else if let Some(packet) = (!self.senders.contains_key(&job.sender))
                .then(|| self.retained.get(&job.sender))
                .flatten()
                .and_then(|packets| packets.first_key_value())
                .map(|(_, packet)| packet)
            {
                let key = RetainedKey {
                    sender: job.sender,
                    index: packet.index,
                };
                self.remove_retained(key);
            } else {
                if let Some(job) = self.retiring.pop_front() {
                    self.runtime_bytes = self
                        .runtime_bytes
                        .saturating_sub(runtime::queue_bytes(&job.queue));
                    self.release_retirement(job.sender);
                }
            }
        }
        self.release_idle_retirement();
        self.refresh_pacer_queue(at);
    }

    pub(crate) fn has_issued_ssrc(&self, ssrc: u32) -> bool {
        self.issued_ssrcs.contains(&ssrc)
    }

    #[cfg(test)]
    pub(crate) const fn test_runtime_bytes(&self) -> usize {
        self.runtime_bytes
    }

    #[cfg(test)]
    pub(crate) fn test_flight_peak(&self) -> u64 {
        self.controller.diagnostics().flight_peak
    }

    pub(crate) fn observe_bytes_in_flight(&mut self, bytes: u64) {
        self.controller.observe_bytes_in_flight(bytes);
    }

    pub(crate) fn update_controller(
        &mut self,
        at: Instant,
        path_change: Option<(u64, bool)>,
        feedback: &[FeedbackSample],
        feedback_hold: Duration,
        fresh_network_feedback: bool,
        bytes_in_flight: u64,
    ) -> Result<bool, UpdateError> {
        if let Some((epoch, available)) = path_change {
            self.active_path = Some(epoch);
            self.path_available = available;
            self.offer_window_started = at;
            self.offered_bytes = 0;
            self.offered_rate = 0;
        }
        let interval = at.saturating_duration_since(self.offer_window_started);
        if interval >= Duration::from_millis(200) {
            let micros = interval.as_micros().max(1);
            self.offered_rate =
                u64::try_from(u128::from(self.offered_bytes).saturating_mul(8_000_000) / micros)
                    .unwrap_or(u64::MAX);
            self.offered_bytes = 0;
            self.offer_window_started = at;
        }
        let desired = self.desired_bitrate();
        let now = at.saturating_duration_since(self.controller_origin);
        let output = self.controller.update(
            now,
            ControllerInput {
                path_epoch: self.active_path,
                path_available: self.path_available,
                feedback,
                feedback_hold,
                fresh_network_feedback,
                bytes_in_flight,
                paced_queue_bytes: self.queued_transport_bytes as u64,
                desired_media_rate: desired,
                ecn: EcnValidation::default(),
            },
        )?;
        self.advance_queue_time(at);
        self.queue_paused =
            !self.path_available || bytes_in_flight >= output.max_rtp_bytes_in_flight;
        self.rtp_bytes_in_flight = bytes_in_flight;
        self.envelope = Some(output);
        self.refresh_pacer_queue(at);
        self.update_estimate(output.target_media_payload_rate);
        Ok(output.application_limited)
    }

    pub(crate) fn request_keyframe(&mut self, request: crate::rtcp::KeyframeRequest) {
        let Some(&key) = self.by_ssrc.get(&request.media_ssrc) else {
            return;
        };
        let Some(sender) = self
            .senders
            .get_mut(&key)
            .filter(|s| s.active && s.facts.kind == MediaKind::Video)
        else {
            return;
        };
        if let Some((requester, sequence)) = request.fir {
            if let Some(previous) = sender.fir_sequences.get_mut(&requester) {
                let distance = sequence.wrapping_sub(*previous);
                if distance == 0 || distance >= 128 {
                    return;
                }
                *previous = sequence;
            } else {
                // Preserve duplicate suppression for known requesters rather than evicting
                // their state when an authenticated peer supplies unbounded identities.
                let Some(total) = self
                    .runtime_bytes
                    .checked_add(runtime::FIR_BYTES)
                    .filter(|bytes| *bytes <= self.max_runtime_bytes)
                else {
                    return;
                };
                sender.fir_sequences.insert(requester, sequence);
                self.runtime_bytes = total;
            }
        }
        sender.keyframe_requested = true;
        self.keyframe_ready.insert(key);
    }

    pub(crate) fn request_repair(&mut self, media_ssrc: u32, sequence: u16) {
        let Some(&sender) = self.by_ssrc.get(&media_ssrc) else {
            return;
        };
        let Some(state) = self.senders.get(&sender).filter(|state| state.active) else {
            return;
        };
        let Some(packet) = self
            .retained
            .get(&sender)
            .and_then(|packets| packets.get(&sequence))
        else {
            return;
        };
        let key = RetainedKey {
            sender,
            index: packet.index,
        };
        if state.requested_repairs.contains_key(&key.index) {
            return;
        }
        let Some(request) = self.next_repair_request else {
            return;
        };
        if !self.reserve_repair_request(sender) {
            return;
        }
        self.next_repair_request = request.checked_add(1);
        if let Some(state) = self.senders.get_mut(&sender) {
            state.requested_repairs.insert(key.index, request);
        }
        self.repair_requests.insert(request, key);
    }

    pub(crate) fn observe_receipt(
        &mut self,
        arrival: Instant,
        receipt: crate::rtcp::ReportReceipt,
    ) {
        let Some(&key) = self.by_ssrc.get(&receipt.ssrc) else {
            return;
        };
        let Some(sender) = self.senders.get_mut(&key) else {
            return;
        };
        let sequence = receipt.highest_sequence;
        if sender.reported_sequence.is_some_and(|previous| {
            let advance = sequence.wrapping_sub(previous);
            advance == 0 || advance >= (1_u32 << 31)
        }) {
            return;
        }
        sender.reported_sequence = Some(sequence);
        sender.pending_report = Some((arrival, sequence));
        self.report_ready.insert(key);
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        if let Some(estimate) = self.estimate_changed.take() {
            return Some(Event::BandwidthEstimate(estimate));
        }
        if let Some(key) = self.keyframe_ready.pop_first() {
            let sender = self.senders.get_mut(&key)?;
            sender.keyframe_requested = false;
            return Some(Event::KeyframeRequested { ssrc: sender.ssrc });
        }
        let key = self.report_ready.pop_first()?;
        let sender = self.senders.get_mut(&key)?;
        let (arrival, sequence) = sender.pending_report.take()?;
        Some(Event::ReceiverReport(crate::ReceiverReport {
            mid: sender.facts.mid.as_ref().into(),
            ssrc: sender.ssrc,
            arrival,
            extended_highest_sequence: sequence,
        }))
    }

    pub(crate) fn abort(&mut self) {
        self.begin_shutdown();
        self.estimate_changed = None;
    }

    pub(crate) fn begin_shutdown(&mut self) {
        for sender in self.senders.values_mut() {
            for queued in sender.queue.drain(..) {
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::extension_bytes(&queued.media));
            }
            sender.queued_payload_bytes = 0;
            self.runtime_bytes = self.runtime_bytes.saturating_sub(
                sender
                    .requested_repairs
                    .len()
                    .saturating_mul(runtime::REPAIR_OWNER_BYTES),
            );
            sender.requested_repairs.clear();
        }
        self.queued_packets = 0;
        self.queued_payload_bytes = 0;
        self.queued_transport_bytes = 0;
        self.queue_enqueued_time_sum = 0;
        self.queue_paused = true;
        while let Some(job) = self.retiring.pop_front() {
            self.runtime_bytes = self
                .runtime_bytes
                .saturating_sub(runtime::queue_bytes(&job.queue));
            for queued in job.queue {
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::extension_bytes(&queued.media));
            }
            self.runtime_bytes = self.runtime_bytes.saturating_sub(
                job.repairs
                    .len()
                    .saturating_mul(runtime::REPAIR_OWNER_BYTES),
            );
            self.release_retirement(job.sender);
        }
        self.release_idle_retirement();
        self.eligible.clear();
        self.retained.clear();
        self.retained_expiry.clear();
        self.retained_bytes = 0;
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(self.retained_runtime_bytes);
        self.retained_runtime_bytes = 0;
        self.runtime_bytes = self.runtime_bytes.saturating_sub(
            self.repair_requests
                .len()
                .saturating_mul(runtime::REPAIR_INDEX_BYTES),
        );
        self.repair_requests.clear();
        if let Some(PendingTransmit::Original { packet, .. }) = self.pending.take() {
            self.runtime_bytes = self.runtime_bytes.saturating_sub(packet.capacity());
        }
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(self.original_workspace.capacity());
        self.original_workspace = Vec::new();
    }

    pub(crate) fn stats(&self) -> (EgressStats, Vec<SenderStats>) {
        let senders = self
            .senders
            .values()
            .map(|sender| SenderStats {
                mid: sender.facts.mid.clone().into(),
                ssrc: sender.ssrc,
                queued_packets: sender.queue.len(),
                queued_payload_bytes: sender.queued_payload_bytes,
                transmitted_packets: sender.committed_packets,
                transmitted_payload_bytes: sender.committed_payload_bytes,
            })
            .collect();
        (
            EgressStats {
                queued_transport_bytes: self.queued_transport_bytes,
                bandwidth_estimate: self.bandwidth_estimate,
                pacing_bitrate: self
                    .envelope
                    .map_or(0, |envelope| envelope.pacing_transport_rate),
            },
            senders,
        )
    }

    pub(crate) fn prepare_one(
        &mut self,
        at: Instant,
        bytes_in_flight: u64,
        transport: &mut Transport,
        allow_padding: bool,
        admission: &mut RtpAdmission<'_>,
    ) -> PrepareResult {
        if self.pending.is_some() {
            return PrepareResult::Fatal;
        }
        self.clean_retired(at);
        self.expire_repair(at);
        self.clean_repair_requests();
        if !transport.can_send_user_data() {
            return PrepareResult::Blocked;
        }
        if at >= self.next_rtcp
            && let Some(result) = self.prepare_rtcp(at, transport)
        {
            return result;
        }
        self.last_send_attempt = Some(at);
        let now = at.saturating_duration_since(self.controller_origin);
        if self.controller.ready(now, bytes_in_flight, 1) != Readiness::Ready {
            self.controller.process_idle(now);
            return PrepareResult::Blocked;
        }
        if let Some(envelope) = self.envelope
            && bytes_in_flight >= envelope.max_rtp_bytes_in_flight
        {
            return PrepareResult::Blocked;
        }
        if let Some(result) = self.prepare_repair(at, bytes_in_flight, transport, admission) {
            return result;
        }

        let Some(sender_index) = self.scheduler.select(&self.eligible) else {
            return if allow_padding {
                self.prepare_padding(at, bytes_in_flight, transport, admission)
            } else {
                PrepareResult::Blocked
            };
        };
        let Some(sender) = self.senders.get_mut(&sender_index) else {
            return PrepareResult::Fatal;
        };
        let Some(queued) = sender.queue.front() else {
            return PrepareResult::Fatal;
        };
        let media = &queued.media;
        let sequence = wire_u16(media.sequence);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let extensions = write_media_extensions(sender, media);
        let mut packet = std::mem::take(&mut self.original_workspace);
        if !encode_rtp(
            sender,
            media.marker,
            sequence,
            media.timestamp,
            twcc,
            &media.payload,
            sender.facts.payload_type,
            sender.ssrc,
            false,
            &extensions,
            &mut packet,
        ) {
            self.recycle_original(packet);
            return PrepareResult::Fatal;
        }
        let timestamp = media.timestamp;
        let index = media.sequence;
        let classification = crate::TransmitKind::Original { ssrc: sender.ssrc };
        let twcc_extension_id = sender.facts.twcc_extension_id;
        if !self.fits_send_window(bytes_in_flight, packet.len()) {
            self.recycle_original(packet);
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_info(
            &packet,
            index,
            classification,
            twcc_extension_id,
            admission,
        ) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Original {
                    sender: sender_index,
                    sequence,
                    timestamp,
                    twcc,
                    packet,
                });
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => {
                self.recycle_original(packet);
                PrepareResult::Blocked
            }
            Err(_) => {
                self.recycle_original(packet);
                PrepareResult::Fatal
            }
        }
    }

    pub(crate) fn preflight_commit(&self, prepared: &PreparedTransmit) -> bool {
        if prepared.kind == DatagramKind::Rtp
            && self.controller.ready(
                self.last_send_attempt
                    .unwrap_or(self.controller_origin)
                    .saturating_duration_since(self.controller_origin),
                self.rtp_bytes_in_flight,
                prepared.wire_len,
            ) != Readiness::Ready
        {
            return false;
        }
        let sender = match &self.pending {
            Some(
                PendingTransmit::Original { sender, .. }
                | PendingTransmit::Repair { sender, .. }
                | PendingTransmit::Padding { sender, .. },
            ) => sender,
            Some(PendingTransmit::Control) => {
                return prepared.kind == DatagramKind::Rtcp && prepared.rtp.is_none();
            }
            None => return prepared.rtp.is_none(),
        };
        let Some(state) = self.senders.get(sender) else {
            return false;
        };
        let expected = match &self.pending {
            Some(PendingTransmit::Original { sequence, twcc, .. }) => PreparedRtpIdentity {
                ssrc: state.ssrc,
                sequence: *sequence,
                index: state
                    .queue
                    .front()
                    .map_or(0, |queued| queued.media.sequence),
                twcc_sequence: *twcc,
                service: RtpService::Original,
            },
            Some(PendingTransmit::Repair { sequence, twcc, .. }) => PreparedRtpIdentity {
                ssrc: state.rtx_ssrc,
                sequence: *sequence,
                index: state.next_rtx_sequence,
                twcc_sequence: *twcc,
                service: RtpService::Repair,
            },
            Some(PendingTransmit::Padding {
                ssrc,
                sequence,
                twcc,
                ..
            }) => PreparedRtpIdentity {
                ssrc: *ssrc,
                sequence: *sequence,
                index: state.next_rtx_sequence,
                twcc_sequence: *twcc,
                service: RtpService::Padding,
            },
            _ => return false,
        };
        prepared.rtp == Some(expected)
    }

    pub(crate) fn commit(
        &mut self,
        at: Instant,
        prepared: &PreparedTransmit,
        post_send_flight: u64,
    ) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if !matches!(pending, PendingTransmit::Control) {
            self.controller.commit_send(
                at.saturating_duration_since(self.controller_origin),
                prepared.wire_len,
                post_send_flight,
                matches!(pending, PendingTransmit::Padding { .. }),
            );
            self.rtp_bytes_in_flight = post_send_flight;
        }
        match pending {
            PendingTransmit::Original {
                sender,
                timestamp,
                twcc,
                packet,
                ..
            } => {
                let Some(queued) = self.pop_head(sender) else {
                    self.recycle_original(packet);
                    return;
                };
                let Some(state) = self.senders.get_mut(&sender) else {
                    self.recycle_original(packet);
                    return;
                };
                self.rtcp_senders.insert(sender);
                state.latest_rtp = Some((queued.media.timestamp, queued.media.wallclock));
                state.highest_sequence = Some(
                    state
                        .highest_sequence
                        .map_or(queued.media.sequence, |previous| {
                            previous.max(queued.media.sequence)
                        }),
                );
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                state.committed_packets = state.committed_packets.saturating_add(1);
                state.committed_payload_bytes = state
                    .committed_payload_bytes
                    .saturating_add(queued.payload_bytes as u64);
                self.finish_original(
                    sender,
                    queued.media.sequence,
                    timestamp,
                    packet,
                    at,
                    prepared.wire_len,
                );
            }
            PendingTransmit::Repair {
                sender,
                request,
                original_index,
                twcc,
                ..
            } => {
                let Some(state) = self.senders.get_mut(&sender) else {
                    return;
                };
                state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
                if let Some(retained) = self
                    .retained
                    .get_mut(&sender)
                    .and_then(|packets| packets.get_mut(&wire_u16(original_index)))
                    .filter(|packet| packet.index == original_index)
                {
                    retained.last_repair_at = Some(at);
                }
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                self.finish_repair_request(
                    request,
                    RetainedKey {
                        sender,
                        index: original_index,
                    },
                );
            }
            PendingTransmit::Control => {
                self.next_rtcp = at.checked_add(Duration::from_secs(1)).unwrap_or(at);
            }
            PendingTransmit::Padding {
                sender, ssrc, twcc, ..
            } => {
                let Some(state) = self.senders.get_mut(&sender) else {
                    return;
                };
                debug_assert_eq!(ssrc, state.rtx_ssrc);
                state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
            }
        }
        self.refresh_pacer_queue(at);
    }

    pub(crate) fn maintenance_deadline(&self, at: Instant) -> Option<Instant> {
        [
            (!self.retiring.is_empty()).then_some(at),
            self.retained_expiry
                .first()
                .and_then(|(retained_at, _)| retained_at.checked_add(Duration::from_secs(3))),
            self.repair_requests
                .first_key_value()
                .filter(|(request, key)| self.requested_repair(**key) != Some(**request))
                .map(|_| at),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn next_deadline(&self, at: Instant) -> Option<Instant> {
        let queue_idle = self.queued_packets == 0 && self.repair_requests.is_empty();
        let candidate = self
            .next_packet_transport_bytes()
            .map(|bytes| (self.rtp_bytes_in_flight, bytes));
        let padding = queue_idle
            .then(|| self.padding_transport_bytes())
            .flatten()
            .map(|bytes| (self.rtp_bytes_in_flight, bytes));
        [
            self.maintenance_deadline(at),
            (self.scheduler.deferred()
                && !self.eligible.is_empty()
                && candidate.is_some_and(|(flight, bytes)| {
                    self.controller.ready(
                        at.saturating_duration_since(self.controller_origin),
                        flight,
                        bytes,
                    ) == Readiness::Ready
                })
                && self
                    .envelope
                    .is_some_and(|e| self.rtp_bytes_in_flight < e.max_rtp_bytes_in_flight))
            .then_some(at),
            self.controller
                .next_deadline(
                    at.saturating_duration_since(self.controller_origin),
                    candidate,
                    padding,
                )
                .and_then(|deadline| self.controller_origin.checked_add(deadline))
                .filter(|deadline| self.send_deadline_unattempted(*deadline)),
            (!self.rtcp_senders.is_empty()).then_some(self.next_rtcp),
            self.controller
                .next_deadline(
                    at.saturating_duration_since(self.controller_origin),
                    None,
                    None,
                )
                .and_then(|deadline| self.controller_origin.checked_add(deadline)),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn next_packet_transport_bytes(&self) -> Option<usize> {
        if let Some((_, key)) = self.repair_requests.first_key_value() {
            let retained = self.retained_packet(*key)?;
            let sender = self.senders.get(&key.sender)?;
            let original = RtpPacket::parse(&retained.packet).ok()?;
            let forwarded = original
                .extensions()
                .into_iter()
                .flatten()
                .filter(|extension| {
                    sender.facts.dependency_extension_id == Some(extension.id())
                        || sender
                            .facts
                            .forwarded_extensions
                            .iter()
                            .any(|(id, _)| *id == extension.id())
                })
                .map(|extension| (extension.id(), extension.value().len()));
            let layout = layout::PacketLayout::new(
                original.payload().len().checked_add(2)?,
                forwarded.chain(repair_header_extensions(sender)),
                sender_uses_one_byte_extensions(sender),
            )?;
            return layout.bytes.checked_add(RTP_TRANSPORT_ALLOWANCE);
        }
        let sender = self
            .senders
            .get(&self.scheduler.candidate(&self.eligible)?)?;
        layout::PacketLayout::original(sender, &sender.queue.front()?.media)?
            .bytes
            .checked_add(RTP_TRANSPORT_ALLOWANCE)
    }

    fn padding_transport_bytes(&self) -> Option<usize> {
        let sender = self.senders.get(&self.padding_candidate()?)?;
        let header = layout::PacketLayout::new(
            0,
            repair_header_extensions(sender),
            sender_uses_one_byte_extensions(sender),
        )?
        .bytes;
        let padding = self
            .controller
            .padding_request_bytes()
            .saturating_sub(header)
            .clamp(1, 255);
        header
            .checked_add(padding)?
            .checked_add(RTP_TRANSPORT_ALLOWANCE)
    }

    fn send_deadline_unattempted(&self, deadline: Instant) -> bool {
        // Once attempted, sending needs new input or a future pacing deadline, not a spin.
        self.last_send_attempt
            .is_none_or(|attempt| deadline > attempt)
    }

    #[cfg(test)]
    pub(crate) fn observed_offer(&self) -> (u64, bool) {
        (
            self.offered_rate,
            self.envelope
                .is_some_and(|envelope| envelope.application_limited),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_has_repair_senders(&self) -> bool {
        self.senders
            .values()
            .any(|sender| sender.facts.retransmission_payload_type.is_some())
    }

    #[cfg(test)]
    pub(crate) fn test_envelope(&self) -> Option<SafeRtpEnvelope> {
        self.envelope
    }

    #[cfg(test)]
    pub(crate) fn test_pacing_profile(&self) -> (u64, u64) {
        (
            self.envelope
                .map_or(0, |envelope| envelope.pacing_transport_rate),
            self.active_path.unwrap_or(0),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_startup_probe_rate(&self) -> Option<u64> {
        self.envelope
            .filter(|envelope| envelope.padding_transport_rate > 0)
            .map(|envelope| envelope.pacing_transport_rate)
    }

    #[cfg(test)]
    pub(crate) fn test_initial_probe_started_at(&self) -> Option<Instant> {
        self.controller
            .diagnostics()
            .initial_probe_started
            .and_then(|started| self.controller_origin.checked_add(started))
    }

    pub(crate) const fn controller_origin(&self) -> Instant {
        self.controller_origin
    }

    fn pop_head(&mut self, sender: u64) -> Option<QueuedMedia> {
        let state = self.senders.get_mut(&sender)?;
        let queued = state.queue.pop_front()?;
        self.runtime_bytes = self
            .runtime_bytes
            .saturating_sub(runtime::extension_bytes(&queued.media));
        if let Some(next) = state.queue.front() {
            self.eligible.insert(sender, next.transport_estimate);
        } else {
            self.eligible.remove(&sender);
        }
        self.queue_enqueued_time_sum = self
            .queue_enqueued_time_sum
            .saturating_sub(queued.enqueued_at.as_nanos());
        state.queued_payload_bytes = state
            .queued_payload_bytes
            .saturating_sub(queued.payload_bytes);
        self.queued_packets = self.queued_packets.saturating_sub(1);
        self.queued_payload_bytes = self
            .queued_payload_bytes
            .saturating_sub(queued.payload_bytes);
        self.queued_transport_bytes = self
            .queued_transport_bytes
            .saturating_sub(queued.transport_estimate);
        Some(queued)
    }

    fn desired_bitrate(&self) -> u64 {
        self.desired_bitrate
    }

    fn queue_time_at(&self, at: Instant) -> Duration {
        self.queue_active_time.saturating_add(if self.queue_paused {
            Duration::ZERO
        } else {
            at.saturating_duration_since(self.queue_time_updated_at)
        })
    }

    fn advance_queue_time(&mut self, at: Instant) {
        self.queue_active_time = self.queue_time_at(at);
        self.queue_time_updated_at = self.queue_time_updated_at.max(at);
    }

    fn refresh_pacer_queue(&mut self, at: Instant) {
        self.advance_queue_time(at);
        let age_sum = self
            .queue_active_time
            .as_nanos()
            .saturating_mul(self.queued_packets as u128)
            .saturating_sub(self.queue_enqueued_time_sum);
        let average = age_sum
            .checked_div(self.queued_packets as u128)
            .unwrap_or_default();
        self.controller.update_queue(
            self.queued_transport_bytes as u64,
            Duration::from_nanos(u64::try_from(average).unwrap_or(u64::MAX)),
        );
        if let Some(envelope) = &mut self.envelope {
            envelope.pacing_transport_rate = self.controller.snapshot().pacing_transport_rate;
        }
    }

    fn update_estimate(&mut self, budget: u64) {
        if budget != self.bandwidth_estimate {
            self.bandwidth_estimate = budget;
            self.estimate_changed = Some(MediaPayloadBitrate::from_bps(budget));
        }
    }

    fn retained_packet(&self, key: RetainedKey) -> Option<&RetainedPacket> {
        self.retained
            .get(&key.sender)
            .and_then(|packets| packets.get(&wire_u16(key.index)))
            .filter(|packet| packet.index == key.index)
    }

    fn remove_retained(&mut self, key: RetainedKey) {
        if self.retained_packet(key).is_none() {
            return;
        }
        let Some(packets) = self.retained.get_mut(&key.sender) else {
            return;
        };
        let Some(packet) = packets.remove(&wire_u16(key.index)) else {
            return;
        };
        let mut released = packet
            .packet
            .capacity()
            .saturating_add(runtime::RETAINED_PACKET_BYTES)
            .saturating_add(runtime::RETAINED_EXPIRY_BYTES);
        if packets.is_empty() {
            self.retained.remove(&key.sender);
            released = released.saturating_add(runtime::RETAINED_GROUP_BYTES);
        }
        self.runtime_bytes = self.runtime_bytes.saturating_sub(released);
        self.retained_runtime_bytes = self.retained_runtime_bytes.saturating_sub(released);
        self.retained_expiry.remove(&(packet.retained_at, key));
        self.retained_bytes = self.retained_bytes.saturating_sub(packet.transport_bytes);
        if let Some(request) = self.requested_repair(key) {
            self.finish_repair_request(request, key);
        }
    }

    fn requested_repair(&self, key: RetainedKey) -> Option<u64> {
        self.senders
            .get(&key.sender)?
            .requested_repairs
            .get(&key.index)
            .copied()
    }

    fn finish_repair_request(&mut self, request: u64, key: RetainedKey) {
        self.release_repair_index(request);
        if self.requested_repair(key) == Some(request)
            && let Some(state) = self.senders.get_mut(&key.sender)
        {
            if state.requested_repairs.remove(&key.index).is_some() {
                self.runtime_bytes = self
                    .runtime_bytes
                    .saturating_sub(runtime::REPAIR_OWNER_BYTES);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_original(
        &mut self,
        sender: u64,
        index: u64,
        timestamp: u32,
        mut packet: Vec<u8>,
        at: Instant,
        wire_len: usize,
    ) {
        if !self.try_retain_original(sender, index, timestamp, &mut packet, at, wire_len, true) {
            self.recycle_original(packet);
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn retain_original(
        &mut self,
        sender: u64,
        index: u64,
        timestamp: u32,
        mut packet: Vec<u8>,
        at: Instant,
        wire_len: usize,
    ) {
        self.try_retain_original(sender, index, timestamp, &mut packet, at, wire_len, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn try_retain_original(
        &mut self,
        sender: u64,
        index: u64,
        timestamp: u32,
        packet: &mut Vec<u8>,
        at: Instant,
        wire_len: usize,
        working: bool,
    ) -> bool {
        if self
            .senders
            .get(&sender)
            .is_none_or(|s| s.facts.retransmission_payload_type.is_none())
            || wire_len > self.max_retransmission_bytes
            || RtpPacket::parse(packet).is_err()
        {
            return false;
        }
        let Some(entry_bytes) = runtime::retained_entry_bytes(packet.capacity()) else {
            return false;
        };
        if entry_bytes
            .checked_add(runtime::RETAINED_GROUP_BYTES)
            .is_none_or(|bytes| bytes > self.max_runtime_bytes)
        {
            return false;
        }
        let credit = if working { packet.capacity() } else { 0 };
        let replacement_min = if working && self.original_workspace.capacity() == 0 {
            packet.capacity()
        } else {
            0
        };
        let sequence = wire_u16(index);
        if let Some(previous) = self
            .retained
            .get(&sender)
            .and_then(|packets| packets.get(&sequence))
        {
            if previous.index >= index {
                return false;
            }
            let previous = RetainedKey {
                sender,
                index: previous.index,
            };
            self.remove_retained(previous);
        }
        let mut replacement = None;
        let mut reservation = None;
        for evictions in 0..=REPAIR_MAINTENANCE_BUDGET {
            let Some(charge) = entry_bytes.checked_add(if self.retained.contains_key(&sender) {
                0
            } else {
                runtime::RETAINED_GROUP_BYTES
            }) else {
                return false;
            };
            let replacement_bytes = replacement.as_ref().map_or(replacement_min, Vec::capacity);
            let projected = self
                .runtime_bytes
                .checked_sub(credit)
                .and_then(|bytes| bytes.checked_add(charge))
                .and_then(|bytes| bytes.checked_add(replacement_bytes));
            if projected.is_some_and(|bytes| bytes <= self.max_runtime_bytes)
                && wire_len
                    <= self
                        .max_retransmission_bytes
                        .saturating_sub(self.retained_bytes)
            {
                if replacement_min > 0 && replacement.is_none() {
                    let mut buffer = Vec::new();
                    if buffer.try_reserve_exact(replacement_min).is_err() {
                        return false;
                    }
                    replacement = Some(buffer);
                }
                let replacement_bytes = replacement.as_ref().map_or(0, Vec::capacity);
                if let Some(total) = self
                    .runtime_bytes
                    .checked_sub(credit)
                    .and_then(|bytes| bytes.checked_add(charge))
                    .and_then(|bytes| bytes.checked_add(replacement_bytes))
                    .filter(|bytes| *bytes <= self.max_runtime_bytes)
                {
                    reservation = Some((total, charge));
                    break;
                }
            }
            if evictions == REPAIR_MAINTENANCE_BUDGET {
                return false;
            }
            let Some((_, oldest)) = self.retained_expiry.first().copied() else {
                return false;
            };
            self.remove_retained(oldest);
        }
        let Some((total, charge)) = reservation else {
            return false;
        };
        if let Some(buffer) = replacement {
            self.original_workspace = buffer;
        }
        let key = RetainedKey { sender, index };
        self.retained.entry(sender).or_default().insert(
            sequence,
            RetainedPacket {
                index,
                timestamp,
                packet: std::mem::take(packet),
                retained_at: at,
                last_repair_at: None,
                transport_bytes: wire_len,
            },
        );
        self.retained_expiry.insert((at, key));
        self.retained_bytes = self.retained_bytes.saturating_add(wire_len);
        self.runtime_bytes = total;
        self.retained_runtime_bytes = self.retained_runtime_bytes.saturating_add(charge);
        true
    }

    fn expire_repair(&mut self, now: Instant) {
        // The remaining due expiry is a ready deadline, not an unbounded pump walk.
        for _ in 0..REPAIR_MAINTENANCE_BUDGET {
            let Some((retained_at, key)) = self.retained_expiry.first().copied() else {
                break;
            };
            if now.saturating_duration_since(retained_at) < Duration::from_secs(3) {
                break;
            }
            self.remove_retained(key);
        }
    }

    fn clean_repair_requests(&mut self) {
        for _ in 0..REPAIR_MAINTENANCE_BUDGET {
            let Some((&request, &key)) = self.repair_requests.first_key_value() else {
                break;
            };
            if self.requested_repair(key) == Some(request) {
                break;
            }
            self.finish_repair_request(request, key);
        }
    }

    fn repair_due(&self, retained: &RetainedPacket, now: Instant) -> bool {
        let rtt = self
            .envelope
            .map_or(Duration::from_millis(50), |envelope| envelope.smoothed_rtt);
        retained
            .last_repair_at
            .is_none_or(|last| now.saturating_duration_since(last) >= rtt)
    }

    fn prepare_repair(
        &mut self,
        at: Instant,
        bytes_in_flight: u64,
        transport: &mut Transport,
        admission: &mut RtpAdmission<'_>,
    ) -> Option<PrepareResult> {
        let (&request, &key) = self.repair_requests.first_key_value()?;
        if self.requested_repair(key) != Some(request) {
            return Some(PrepareResult::Blocked);
        }
        let Some(retained) = self.retained_packet(key) else {
            self.finish_repair_request(request, key);
            return None;
        };
        if at.saturating_duration_since(retained.retained_at) >= Duration::from_secs(3)
            || !self.repair_due(retained, at)
        {
            self.finish_repair_request(request, key);
            return Some(PrepareResult::Blocked);
        }
        let sender_index = key.sender;
        let original_sequence = wire_u16(key.index);
        let Some(sender) = self.senders.get(&sender_index) else {
            self.finish_repair_request(request, key);
            return None;
        };
        if !sender.active {
            self.finish_repair_request(request, key);
            return None;
        }
        let payload_type = sender.facts.retransmission_payload_type?;
        let sequence = wire_u16(sender.next_rtx_sequence);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let original = RtpPacket::parse(&retained.packet).ok()?;
        let extensions = repair_extensions(sender, &original);
        let mut payload = original_sequence.to_be_bytes().to_vec();
        payload.extend_from_slice(original.payload());
        let packet = build_rtp(
            sender,
            original.marker(),
            sequence,
            retained.timestamp,
            twcc,
            &payload,
            payload_type,
            sender.rtx_ssrc,
            false,
            &extensions,
        )?;
        if !self.fits_send_window(bytes_in_flight, packet.len()) {
            return Some(PrepareResult::Blocked);
        }
        let result = match transport.send_rtp_with_info(
            &packet,
            sender.next_rtx_sequence,
            crate::TransmitKind::Repair {
                ssrc: sender.rtx_ssrc,
            },
            sender.facts.twcc_extension_id,
            admission,
        ) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Repair {
                    sender: sender_index,
                    request,
                    original_index: key.index,
                    sequence,
                    twcc,
                });
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => PrepareResult::Blocked,
            Err(_) => PrepareResult::Fatal,
        };
        Some(result)
    }

    fn prepare_rtcp(&mut self, at: Instant, transport: &mut Transport) -> Option<PrepareResult> {
        let &sender_index = self
            .rtcp_senders
            .range((
                std::ops::Bound::Excluded(self.rtcp_cursor),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .or_else(|| self.rtcp_senders.first())?;
        let sender = self.senders.get(&sender_index)?;
        let packet = sender_report(sender, at, &self.clock)?;
        match transport.send_rtcp(&packet) {
            Ok(()) => {
                self.rtcp_cursor = sender_index;
                self.pending = Some(PendingTransmit::Control);
                Some(PrepareResult::Prepared)
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => {
                Some(PrepareResult::Blocked)
            }
            Err(_) => Some(PrepareResult::Fatal),
        }
    }

    fn fits_send_window(&self, bytes_in_flight: u64, packet_len: usize) -> bool {
        self.envelope.is_some_and(|envelope| {
            u128::from(bytes_in_flight) + packet_len as u128 + RTP_TRANSPORT_ALLOWANCE as u128
                <= u128::from(envelope.max_rtp_bytes_in_flight)
        })
    }

    fn padding_candidate(&self) -> Option<u64> {
        (self.desired_bitrate > 0)
            .then(|| self.padding_senders.first().copied())
            .flatten()
    }

    fn prepare_padding(
        &mut self,
        at: Instant,
        bytes_in_flight: u64,
        transport: &mut Transport,
        admission: &mut RtpAdmission<'_>,
    ) -> PrepareResult {
        if !self.path_available || self.queued_packets != 0 || !self.repair_requests.is_empty() {
            return PrepareResult::Blocked;
        }
        let Some(sender_index) = self.padding_candidate() else {
            self.controller
                .process_idle(at.saturating_duration_since(self.controller_origin));
            return PrepareResult::Blocked;
        };
        let Some(requested) = self
            .controller
            .prepare_padding(at.saturating_duration_since(self.controller_origin))
        else {
            return PrepareResult::Blocked;
        };
        let Some(sender) = self.senders.get(&sender_index) else {
            return PrepareResult::Blocked;
        };
        let (sequence, payload_type, ssrc) = padding_source(sender);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let timestamp = sender.latest_rtp.map_or(0, |(rtp, _)| rtp);
        let Some(mut packet) = build_rtp(
            sender,
            false,
            sequence,
            timestamp,
            twcc,
            &[1],
            payload_type,
            ssrc,
            true,
            &[],
        ) else {
            self.controller.padding_unavailable();
            return PrepareResult::Fatal;
        };
        packet.pop();
        packet.extend_from_slice(&padding_bytes(requested, packet.len()));
        if !self.fits_send_window(bytes_in_flight, packet.len()) {
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_info(
            &packet,
            sender.next_rtx_sequence,
            crate::TransmitKind::Padding {
                ssrc: sender.rtx_ssrc,
            },
            sender.facts.twcc_extension_id,
            admission,
        ) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Padding {
                    sender: sender_index,
                    ssrc,
                    sequence,
                    twcc,
                });
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => PrepareResult::Blocked,
            Err(_) => PrepareResult::Fatal,
        }
    }
}

fn repair_header_extensions(sender: &Sender) -> impl Iterator<Item = (u8, usize)> + Clone {
    let mid = sender
        .facts
        .mid_extension_id
        .map(|id| (id, sender.facts.mid.len()));
    let rid = sender
        .repaired_rid_extension_id
        .zip(sender.rid.as_ref())
        .map(|(id, rid)| (id, rid.len()));
    let twcc = sender.facts.twcc_extension_id.map(|id| (id, 2));
    mid.into_iter().chain(rid).chain(twcc)
}

fn padding_source(sender: &Sender) -> (u16, u8, u32) {
    (
        wire_u16(sender.next_rtx_sequence),
        sender.facts.retransmission_payload_type.unwrap_or_default(),
        sender.rtx_ssrc,
    )
}

fn padding_bytes(requested: usize, header_bytes: usize) -> Vec<u8> {
    // A generation quantum may overshoot by one complete packet. Commit charges
    // its actual protected wire length, including the negotiated SRTP trailer.
    let count =
        u8::try_from(requested.saturating_sub(header_bytes).clamp(1, 255)).unwrap_or(u8::MAX);
    let mut padding = vec![0; usize::from(count)];
    padding[usize::from(count) - 1] = count;
    padding
}

fn wire_u16(value: u64) -> u16 {
    u16::try_from(value % 65_536).unwrap_or_default()
}

pub(crate) fn sender_rtp_identity(
    randomness: &[u8; 32],
    sender: crate::id::SenderId,
) -> (u32, u32, u16) {
    let seed = sender_seed(randomness, sender.value());
    (
        nonzero_u32([seed[0], seed[1], seed[2], seed[3]]),
        nonzero_u32([seed[10], seed[11], seed[12], seed[13]]),
        u16::from_be_bytes([seed[14], seed[15]]),
    )
}

fn sender_seed(randomness: &[u8; 32], sender: u64) -> [u8; 32] {
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

fn media_extension_id(sender: &Sender, uri: &str) -> Option<u8> {
    sender
        .facts
        .forwarded_extensions
        .iter()
        .find(|(_, supported)| supported.as_ref() == uri)
        .map(|(id, _)| *id)
        .or_else(|| match uri {
            uri if crate::negotiation::DEPENDENCY_URIS.contains(&uri) => {
                sender.facts.dependency_extension_id
            }
            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => {
                sender.facts.playout_extension_id
            }
            _ => None,
        })
}

fn write_media_extensions(sender: &Sender, media: &RtpWrite) -> Vec<(u8, Vec<u8>)> {
    media
        .extensions
        .iter()
        .filter_map(|value| {
            media_extension_id(sender, &value.uri).map(|id| (id, value.value.to_vec()))
        })
        .collect()
}

fn repair_extensions(sender: &Sender, original: &RtpPacket<'_>) -> Vec<(u8, Vec<u8>)> {
    original
        .extensions()
        .into_iter()
        .flatten()
        .filter(|extension| {
            sender.facts.dependency_extension_id == Some(extension.id())
                || sender
                    .facts
                    .forwarded_extensions
                    .iter()
                    .any(|(id, _)| *id == extension.id())
        })
        .map(|extension| (extension.id(), extension.value().to_vec()))
        .collect()
}

fn rtp_extensions<'a>(
    sender: &'a Sender,
    ssrc: u32,
    twcc: Option<&'a [u8; 2]>,
    forwarded_extensions: &'a [(u8, Vec<u8>)],
) -> impl Iterator<Item = (u8, &'a [u8])> + Clone {
    let mid = sender
        .facts
        .mid_extension_id
        .map(|id| (id, sender.facts.mid.as_bytes()));
    let rid_id = if ssrc == sender.rtx_ssrc {
        sender.repaired_rid_extension_id
    } else {
        sender.rid_extension_id
    };
    let rid = rid_id
        .zip(sender.rid.as_ref())
        .map(|(id, rid)| (id, rid.as_bytes()));
    let twcc = sender
        .facts
        .twcc_extension_id
        .zip(twcc)
        .map(|(id, value)| (id, value.as_slice()));
    forwarded_extensions
        .iter()
        .map(|(id, value)| (*id, value.as_slice()))
        .chain(mid)
        .chain(rid)
        .chain(twcc)
}

#[allow(clippy::too_many_arguments)]
fn build_rtp(
    sender: &Sender,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    twcc: Option<u16>,
    payload: &[u8],
    payload_type: u8,
    ssrc: u32,
    padding: bool,
    forwarded_extensions: &[(u8, Vec<u8>)],
) -> Option<Vec<u8>> {
    let twcc_bytes = twcc.map(u16::to_be_bytes);
    let extensions = rtp_extensions(sender, ssrc, twcc_bytes.as_ref(), forwarded_extensions);
    let layout = layout::PacketLayout::new(
        payload.len(),
        extensions.map(|(id, value)| (id, value.len())),
        sender_uses_one_byte_extensions(sender),
    )?;
    let mut packet = Vec::with_capacity(layout.bytes);
    encode_rtp(
        sender,
        marker,
        sequence,
        timestamp,
        twcc,
        payload,
        payload_type,
        ssrc,
        padding,
        forwarded_extensions,
        &mut packet,
    )
    .then_some(packet)
}

#[allow(clippy::too_many_arguments)]
fn encode_rtp(
    sender: &Sender,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    twcc: Option<u16>,
    payload: &[u8],
    payload_type: u8,
    ssrc: u32,
    padding: bool,
    forwarded_extensions: &[(u8, Vec<u8>)],
    packet: &mut Vec<u8>,
) -> bool {
    let twcc_bytes = twcc.map(u16::to_be_bytes);
    let extensions = rtp_extensions(sender, ssrc, twcc_bytes.as_ref(), forwarded_extensions);
    let Some(layout) = layout::PacketLayout::new(
        payload.len(),
        extensions.clone().map(|(id, value)| (id, value.len())),
        sender_uses_one_byte_extensions(sender),
    ) else {
        return false;
    };
    if packet.capacity() < layout.bytes {
        return false;
    }
    packet.clear();
    packet.push(
        0x80 | if layout.extensions.is_none() { 0 } else { 0x10 } | if padding { 0x20 } else { 0 },
    );
    packet.push(payload_type | if marker { 0x80 } else { 0 });
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    if let Some(layout) = layout.extensions
        && !write_extensions(packet, extensions, layout)
    {
        return false;
    }
    packet.extend_from_slice(payload);
    debug_assert_eq!(packet.len(), layout.bytes);
    true
}

fn sender_uses_one_byte_extensions(sender: &Sender) -> bool {
    sender.facts.dependency_extension_id.is_none()
        && sender.rid.as_ref().is_none_or(|rid| rid.len() <= 16)
        && [sender.rid_extension_id, sender.repaired_rid_extension_id]
            .into_iter()
            .flatten()
            .all(|id| (1..=14).contains(&id))
        && sender.facts.mid.len() <= 16
        && [
            sender.facts.mid_extension_id,
            sender.facts.twcc_extension_id,
            sender.facts.playout_extension_id,
        ]
        .into_iter()
        .flatten()
        .all(|id| (1..=14).contains(&id))
        && sender.facts.forwarded_extensions.iter().all(|(id, uri)| {
            (1..=14).contains(id)
                && matches!(
                    &**uri,
                    "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
                        | "urn:3gpp:video-orientation"
                        | "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"
                )
        })
}

fn write_extensions<'a>(
    packet: &mut Vec<u8>,
    extensions: impl Iterator<Item = (u8, &'a [u8])>,
    layout: layout::ExtensionLayout,
) -> bool {
    packet.extend_from_slice(if layout.one_byte {
        &[0xbe, 0xde]
    } else {
        &[0x10, 0x00]
    });
    packet.extend_from_slice(&layout.words.to_be_bytes());
    let start = packet.len();
    for (id, value) in extensions {
        if layout.one_byte {
            packet.push((id << 4) | u8::try_from(value.len() - 1).unwrap_or_default());
        } else {
            let Ok(length) = u8::try_from(value.len()) else {
                return false;
            };
            packet.extend_from_slice(&[id, length]);
        }
        packet.extend_from_slice(value);
    }
    while !(packet.len() - start).is_multiple_of(4) {
        packet.push(0);
    }
    true
}

fn sender_report(sender: &Sender, at: Instant, clock: &ProtocolClock) -> Option<Vec<u8>> {
    let (rtp, wallclock) = sender.latest_rtp?;
    let ticks = at
        .saturating_duration_since(wallclock)
        .as_nanos()
        .saturating_mul(u128::from(sender.facts.clock_rate))
        / 1_000_000_000;
    let timestamp = rtp.wrapping_add(u32::try_from(ticks % (1_u128 << 32)).unwrap_or_default());
    let (seconds, fraction) = clock.ntp(at);
    let mut packet = vec![0x80, 200, 0, 6];
    packet.extend_from_slice(&sender.ssrc.to_be_bytes());
    packet.extend_from_slice(&seconds.to_be_bytes());
    packet.extend_from_slice(&fraction.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(
        &u32::try_from(sender.committed_packets)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    packet.extend_from_slice(
        &u32::try_from(sender.committed_payload_bytes)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );

    crate::rtcp::append_cname(
        &mut packet,
        sender.ssrc,
        sender.facts.rtcp.as_ref()?.cname.as_bytes(),
    )?;
    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod cc;
    use std::time::UNIX_EPOCH;

    fn audio_egress(start: Instant) -> MediaEgress {
        let facts = EgressSenderFacts {
            id: crate::id::SenderId::new(1).unwrap(),
            kind: MediaKind::Audio,
            rtcp: Some(crate::RtcpParameters {
                cname: "test-egress".into(),
                reduced_size: false,
            }),
            mid: "audio".into(),
            payload_type: 111,
            retransmission_payload_type: None,
            clock_rate: 48_000,
            mid_extension_id: Some(1),
            twcc_extension_id: Some(2),
            playout_extension_id: None,
            dependency_extension_id: None,
            forwarded_extensions: Box::new([]),
        };
        MediaEgress::new(
            Box::new([facts]),
            &[7; 32],
            1_024,
            1_024,
            crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
            start,
            ProtocolClock::new(start, UNIX_EPOCH + Duration::from_secs(17)),
        )
        .unwrap()
    }

    fn repair_egress(start: Instant, retransmission_bytes: usize) -> MediaEgress {
        let mut first = audio_egress(start).senders.remove(&1).unwrap().facts;
        first.retransmission_payload_type = Some(112);
        let mut second = first.clone();
        second.id = crate::id::SenderId::new(2).unwrap();
        second.mid = "other".into();
        MediaEgress::new(
            Box::new([first, second]),
            &[7; 32],
            1_024,
            retransmission_bytes,
            crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
            start,
            ProtocolClock::new(start, UNIX_EPOCH),
        )
        .unwrap()
    }

    fn queue_owned_bytes(queue: &VecDeque<QueuedMedia>) -> usize {
        runtime::queue_bytes(queue)
            .checked_add(
                queue
                    .iter()
                    .map(|queued| runtime::extension_bytes(&queued.media))
                    .sum::<usize>(),
            )
            .unwrap()
    }

    fn runtime_owned_bytes(owner: &MediaEgress) -> usize {
        assert_eq!(
            owner.retirement_slots,
            owner
                .retiring
                .len()
                .checked_add(
                    owner
                        .senders
                        .values()
                        .filter(|sender| sender.retirement_reserved)
                        .count()
                )
                .unwrap()
        );
        assert!(owner.retirement_slots <= owner.retiring.capacity());
        let senders = owner
            .senders
            .values()
            .map(|sender| {
                sender
                    .runtime_bytes
                    .checked_add(queue_owned_bytes(&sender.queue))
                    .unwrap()
                    .checked_add(
                        sender
                            .fir_sequences
                            .len()
                            .checked_mul(runtime::FIR_BYTES)
                            .unwrap(),
                    )
                    .unwrap()
                    .checked_add(
                        sender
                            .requested_repairs
                            .len()
                            .checked_mul(runtime::REPAIR_OWNER_BYTES)
                            .unwrap(),
                    )
                    .unwrap()
            })
            .sum::<usize>();
        let jobs = owner
            .retiring
            .iter()
            .map(|job| {
                queue_owned_bytes(&job.queue)
                    .checked_add(
                        job.repairs
                            .len()
                            .checked_mul(runtime::REPAIR_OWNER_BYTES)
                            .unwrap(),
                    )
                    .unwrap()
            })
            .sum::<usize>();
        let groups = owner
            .by_mid
            .keys()
            .map(|mid| runtime::mid_bytes(mid).unwrap())
            .sum::<usize>();
        let retained = owner
            .retained
            .len()
            .checked_mul(runtime::RETAINED_GROUP_BYTES)
            .unwrap()
            .checked_add(
                owner
                    .retained_expiry
                    .len()
                    .checked_mul(runtime::RETAINED_EXPIRY_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .checked_add(
                owner
                    .retained
                    .values()
                    .flat_map(|packets| packets.values())
                    .map(|packet| {
                        packet
                            .packet
                            .capacity()
                            .checked_add(runtime::RETAINED_PACKET_BYTES)
                            .unwrap()
                    })
                    .sum::<usize>(),
            )
            .unwrap();
        assert_eq!(owner.retained_runtime_bytes, retained);
        let guards = owner
            .issued_ssrcs
            .len()
            .checked_mul(runtime::ISSUED_SSRC_BYTES)
            .unwrap();
        let pending = match &owner.pending {
            Some(PendingTransmit::Original { packet, .. }) => packet.capacity(),
            _ => 0,
        };
        senders
            .checked_add(owner.original_workspace.capacity())
            .unwrap()
            .checked_add(pending)
            .unwrap()
            .checked_add(retained)
            .unwrap()
            .checked_add(runtime::retirement_bytes(&owner.retiring))
            .unwrap()
            .checked_add(groups)
            .unwrap()
            .checked_add(guards)
            .unwrap()
            .checked_add(jobs)
            .unwrap()
            .checked_add(
                owner
                    .repair_requests
                    .len()
                    .checked_mul(runtime::REPAIR_INDEX_BYTES)
                    .unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn original_wire_layout_matches_generated_extensions_and_encoder_profiles() {
        let start = Instant::now();
        let mut sender = audio_egress(start).senders.remove(&1).unwrap();
        let uri = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";
        sender.facts.forwarded_extensions = Box::new([(14, uri.into())]);
        sender.rid_extension_id = Some(3);
        for (mid, rid, value, one_byte) in [
            (1, 0, 16, true),
            (16, 16, 1, true),
            (17, 17, 17, false),
            (64, 32, 255, false),
        ] {
            sender.facts.mid = "m".repeat(mid).into();
            sender.rid = (rid > 0).then(|| "r".repeat(rid));
            let media = RtpWrite {
                sequence: 0,
                timestamp: 0,
                wallclock: start,
                marker: false,
                payload: vec![7; 3].into(),
                extensions: vec![crate::RtpExtension {
                    uri: uri.into(),
                    value: vec![9; value].into(),
                }],
            };
            let layout = layout::PacketLayout::original(&sender, &media).unwrap();
            assert_eq!(layout.extensions.unwrap().one_byte, one_byte);
            let forwarded = write_media_extensions(&sender, &media);
            let packet = build_rtp(
                &sender,
                false,
                0,
                0,
                Some(0x3456),
                &media.payload,
                sender.facts.payload_type,
                sender.ssrc,
                false,
                &forwarded,
            )
            .unwrap();
            assert_eq!(packet.len(), layout.bytes);
            let parsed = RtpPacket::parse(&packet).unwrap();
            let extensions: Vec<_> = parsed.extensions().unwrap().collect();
            assert!(
                extensions
                    .iter()
                    .any(|value| value.id() == 1 && value.value() == sender.facts.mid.as_bytes())
            );
            assert!(
                extensions
                    .iter()
                    .any(|value| value.id() == 2 && value.value() == [0x34, 0x56])
            );
            assert_eq!(
                extensions
                    .iter()
                    .find(|value| value.id() == 14)
                    .unwrap()
                    .value()
                    .len(),
                value
            );
        }
        assert!(layout::PacketLayout::new(0, [(1, 256)].into_iter(), true).is_none());
        assert!(layout::PacketLayout::new(0, [(0, 1)].into_iter(), false).is_none());
        assert!(layout::PacketLayout::new(usize::MAX, std::iter::empty(), false).is_none());
        let zero = layout::PacketLayout::new(0, [(15, 0)].into_iter(), true).unwrap();
        assert!(!zero.extensions.unwrap().one_byte);
        assert_eq!(zero.bytes, 20);
    }

    #[test]
    fn original_workspace_follows_blocked_pending_retained_and_shutdown_ownership() {
        let now = Instant::now();
        let mut owner = repair_egress(now, 4096);
        let limit = owner.max_runtime_bytes;
        let write = |sequence, bytes| RtpWrite {
            sequence,
            timestamp: 0,
            wallclock: now,
            marker: false,
            payload: vec![7; bytes].into(),
            extensions: vec![],
        };
        let encode = |owner: &mut MediaEgress, id| {
            let mut packet = std::mem::take(&mut owner.original_workspace);
            let pointer = packet.as_ptr();
            let capacity = packet.capacity();
            let sender = &owner.senders[&id];
            let media = &sender.queue.front().unwrap().media;
            assert!(encode_rtp(
                sender,
                media.marker,
                wire_u16(media.sequence),
                media.timestamp,
                Some(0),
                &media.payload,
                sender.facts.payload_type,
                sender.ssrc,
                false,
                &[],
                &mut packet
            ));
            assert_eq!(packet.as_ptr(), pointer);
            assert_eq!(packet.capacity(), capacity);
            packet
        };
        owner.admit(now, "audio", write(0, 8)).unwrap();
        let capacity = owner.original_workspace.capacity();
        let packet = encode(&mut owner, 1);
        let pointer = packet.as_ptr();
        owner.recycle_original(packet);
        assert_eq!(owner.original_workspace.as_ptr(), pointer);
        assert_eq!(owner.original_workspace.len(), 0);
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));

        let packet = encode(&mut owner, 1);
        owner.pending = Some(PendingTransmit::Original {
            sender: 1,
            sequence: 0,
            timestamp: 0,
            twcc: Some(0),
            packet,
        });
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
        let before = owner.runtime_bytes;
        owner.max_runtime_bytes = before;
        let input = write(0, 200);
        let payload = input.payload.as_ptr();
        let (error, returned) = owner.admit(now, "other", input).unwrap_err();
        assert_eq!(error, WriteError::WouldBlock);
        assert_eq!(returned.payload.as_ptr(), payload);
        assert_eq!(owner.original_workspace.capacity(), 0);
        assert_eq!(owner.runtime_bytes, before);
        assert!(owner.senders[&2].queue.is_empty());
        owner.max_runtime_bytes = limit;
        owner.admit(now, "other", returned).unwrap();
        assert!(owner.original_workspace.capacity() >= capacity);
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
        let Some(PendingTransmit::Original { packet, .. }) = owner.pending.take() else {
            panic!()
        };
        owner.pop_head(1).unwrap();
        owner.finish_original(1, 0, 0, packet, now, 64);
        assert_eq!(owner.retained[&1][&0].packet.as_ptr(), pointer);
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
        owner.expire_repair(now.checked_add(Duration::from_secs(3)).unwrap());

        let packet = encode(&mut owner, 2);
        let pointer = packet.as_ptr();
        owner.pop_head(2).unwrap();
        owner.max_runtime_bytes = owner.runtime_bytes;
        owner.finish_original(2, 0, 0, packet, now, 256);
        assert!(owner.retained.is_empty());
        assert_eq!(owner.original_workspace.as_ptr(), pointer);
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
        owner.admit(now, "other", write(1, 200)).unwrap();
        let packet = encode(&mut owner, 2);
        let pointer = packet.as_ptr();
        let capacity = packet.capacity();
        owner.pop_head(2).unwrap();
        owner.max_runtime_bytes = limit;
        owner.finish_original(2, 1, 0, packet, now, 256);
        assert_eq!(owner.retained[&2][&1].packet.as_ptr(), pointer);
        assert!(owner.original_workspace.capacity() >= capacity);
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));

        owner.admit(now, "other", write(2, 200)).unwrap();
        let packet = encode(&mut owner, 2);
        owner.pending = Some(PendingTransmit::Original {
            sender: 2,
            sequence: 2,
            timestamp: 0,
            twcc: Some(0),
            packet,
        });
        owner.admit(now, "audio", write(1, 8)).unwrap();
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
        owner.begin_shutdown();
        assert_eq!(owner.original_workspace.capacity(), 0);
        assert!(owner.pending.is_none());
        assert_eq!(owner.runtime_bytes, runtime_owned_bytes(&owner));
    }

    #[test]
    fn queue_and_extension_backing_admission_is_atomic_and_follows_actual_capacity() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        let unnamed = egress
            .runtime_bytes
            .checked_sub(
                egress.senders[&1]
                    .facts
                    .rtcp
                    .as_ref()
                    .unwrap()
                    .cname
                    .capacity(),
            )
            .unwrap();
        let mut facts = egress.senders.remove(&1).unwrap().facts;
        let mut cname = String::with_capacity(4096);
        cname.push('a');
        let capacity = cname.capacity();
        facts.rtcp = Some(crate::RtcpParameters {
            cname,
            reduced_size: false,
        });
        egress = MediaEgress::new(
            Box::new([facts]),
            &[7; 32],
            1024,
            1024,
            crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
            start,
            ProtocolClock::new(start, UNIX_EPOCH),
        )
        .unwrap();
        assert_eq!(egress.runtime_bytes, unnamed.checked_add(capacity).unwrap());
        let initial = egress.runtime_bytes;
        let write = |sequence, extensions| RtpWrite {
            sequence,
            timestamp: 0,
            wallclock: start,
            marker: false,
            payload: vec![7].into(),
            extensions: Vec::with_capacity(extensions),
        };
        let input = write(0, 3);
        let extension_bytes = runtime::extension_bytes(&input);
        let pointer = input.extensions.as_ptr();
        let capacity = input.extensions.capacity();
        let queue_bytes = runtime::queue_bytes(&VecDeque::with_capacity(1));
        let workspace = Vec::<u8>::with_capacity(
            layout::PacketLayout::original(&egress.senders[&1], &input)
                .unwrap()
                .bytes,
        )
        .capacity();
        let exact = initial
            .checked_add(workspace)
            .unwrap()
            .checked_add(queue_bytes)
            .unwrap()
            .checked_add(extension_bytes)
            .unwrap();
        egress.max_runtime_bytes = exact.checked_sub(1).unwrap();
        let (error, returned) = egress.admit(start, "audio", input).unwrap_err();
        assert_eq!(error, WriteError::WouldBlock);
        assert_eq!(returned.extensions.capacity(), capacity);
        assert_eq!(returned.extensions.as_ptr(), pointer);
        assert_eq!(egress.senders[&1].queue.capacity(), 0);
        assert_eq!(egress.runtime_bytes, initial);
        egress.max_runtime_bytes = exact;
        egress.admit(start, "audio", returned).unwrap();
        assert_eq!(egress.runtime_bytes, exact);
        let old_capacity = egress.senders[&1].queue.capacity();
        for sequence in 1..u64::try_from(old_capacity).unwrap() {
            egress.admit(start, "audio", write(sequence, 0)).unwrap();
        }
        let next = u64::try_from(old_capacity).unwrap();
        let (error, returned) = egress.admit(start, "audio", write(next, 0)).unwrap_err();
        assert_eq!(error, WriteError::WouldBlock);
        assert_eq!(egress.senders[&1].queue.capacity(), old_capacity);
        assert_eq!(
            egress.senders[&1]
                .queue
                .front()
                .unwrap()
                .media
                .extensions
                .as_ptr(),
            pointer
        );
        assert_eq!(egress.runtime_bytes, exact);
        let expanded_bytes = runtime::queue_bytes(&VecDeque::with_capacity(
            old_capacity.checked_mul(2).unwrap(),
        ));
        egress.max_runtime_bytes = initial
            .checked_add(workspace)
            .unwrap()
            .checked_add(extension_bytes)
            .unwrap()
            .checked_add(expanded_bytes)
            .unwrap();
        egress.admit(start, "audio", returned).unwrap();
        assert_eq!(egress.runtime_bytes, egress.max_runtime_bytes);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        let queued = egress.pop_head(1).unwrap();
        assert_eq!(queued.media.extensions.as_ptr(), pointer);
        drop(queued);
        assert_eq!(
            egress.runtime_bytes,
            initial
                .checked_add(workspace)
                .unwrap()
                .checked_add(expanded_bytes)
                .unwrap()
        );
        egress.discard_pending("audio").unwrap();
        assert_eq!(
            egress.runtime_bytes,
            initial
                .checked_add(workspace)
                .unwrap()
                .checked_add(expanded_bytes)
                .unwrap()
        );
        egress.cancel_sender_work(1);
        assert_eq!(
            egress.runtime_bytes,
            initial.checked_add(workspace).unwrap()
        );
        assert!(egress.retiring.is_empty());
        egress
            .admit(start, "audio", write(next.checked_add(1).unwrap(), 0))
            .unwrap();
        egress.discard_pending("audio").unwrap();
        let ssrc = egress.senders[&1].ssrc;
        let before = egress.runtime_bytes;
        let update = egress
            .stage_configuration(&[], &[7; 32], &[ssrc], false)
            .unwrap();
        assert_eq!(egress.runtime_bytes, before);
        egress.commit_configuration(update);
        assert_eq!(egress.runtime_bytes, runtime::ISSUED_SSRC_BYTES);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
    }

    #[test]
    fn fir_and_shared_mid_reservations_survive_pause_and_retirement() {
        let start = Instant::now();
        let mut facts = audio_egress(start).senders.remove(&1).unwrap().facts;
        facts.kind = MediaKind::Video;
        facts.mid = "video".into();
        facts.payload_type = 96;
        facts.clock_rate = 90_000;
        let mut sibling = facts.clone();
        sibling.id = crate::id::SenderId::new(2).unwrap();
        let mut egress = MediaEgress::new(
            Box::new([facts, sibling]),
            &[7; 32],
            1024,
            1024,
            crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
            start,
            ProtocolClock::new(start, UNIX_EPOCH),
        )
        .unwrap();
        let sibling_ssrc = egress.senders[&2].ssrc;
        egress
            .admit_encoding(
                start,
                "video",
                Some(sibling_ssrc),
                RtpWrite {
                    sequence: 0,
                    timestamp: 0,
                    wallclock: start,
                    marker: false,
                    payload: vec![7].into(),
                    extensions: Vec::new(),
                },
            )
            .unwrap();
        drop(egress.pop_head(2).unwrap());
        let initial = egress.runtime_bytes;
        egress.max_runtime_bytes = initial
            .checked_add(257usize.checked_mul(runtime::FIR_BYTES).unwrap())
            .unwrap();
        let ssrc = egress.senders[&1].ssrc;
        let request = |requester, sequence| crate::rtcp::KeyframeRequest {
            media_ssrc: ssrc,
            fir: Some((requester, sequence)),
        };
        for requester in 0..257 {
            egress.request_keyframe(request(requester, 250));
            assert!(
                matches!(egress.poll_event(), Some(Event::KeyframeRequested { ssrc: known }) if known == ssrc)
            );
        }
        assert_eq!(egress.senders[&1].fir_sequences.len(), 257);
        assert_eq!(egress.runtime_bytes, egress.max_runtime_bytes);
        egress.request_keyframe(request(257, 0));
        assert!(egress.poll_event().is_none());
        egress
            .admit_encoding(
                start,
                "video",
                Some(sibling_ssrc),
                RtpWrite {
                    sequence: 0,
                    timestamp: 0,
                    wallclock: start,
                    marker: false,
                    payload: vec![7].into(),
                    extensions: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(egress.scheduler.select(&egress.eligible), Some(2));
        assert_eq!(egress.runtime_bytes, egress.max_runtime_bytes);
        for sequence in [250, 249, 122] {
            egress.request_keyframe(request(0, sequence));
            assert!(egress.poll_event().is_none());
        }
        egress.request_keyframe(request(0, 1));
        assert!(matches!(
            egress.poll_event(),
            Some(Event::KeyframeRequested { .. })
        ));
        egress.senders.get_mut(&1).unwrap().active = false;
        egress.cancel_sender_work(1);
        egress.request_keyframe(request(0, 2));
        assert!(egress.poll_event().is_none());
        egress.senders.get_mut(&1).unwrap().active = true;
        egress.request_keyframe(request(0, 1));
        assert!(egress.poll_event().is_none());
        egress.request_keyframe(request(0, 2));
        assert!(matches!(
            egress.poll_event(),
            Some(Event::KeyframeRequested { .. })
        ));
        egress.request_keyframe(crate::rtcp::KeyframeRequest {
            media_ssrc: ssrc,
            fir: None,
        });
        assert!(matches!(
            egress.poll_event(),
            Some(Event::KeyframeRequested { .. })
        ));
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        let first = egress.senders[&1].runtime_bytes;
        let update = egress
            .stage_configuration(&[], &[7; 32], &[ssrc], false)
            .unwrap();
        egress.commit_configuration(update);
        assert_eq!(egress.by_mid.len(), 1);
        assert_eq!(egress.by_mid["video"], BTreeSet::from([2]));
        assert_eq!(egress.runtime_bytes, initial.checked_sub(first).unwrap());
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        assert!(egress.has_issued_ssrc(ssrc));
        let update = egress
            .stage_configuration(&[], &[7; 32], &[], true)
            .unwrap();
        egress.commit_configuration(update);
        egress.clean_retired(start);
        assert!(egress.by_mid.is_empty());
        assert!(egress.by_ssrc.is_empty());
        assert_eq!(
            egress.runtime_bytes,
            runtime::ISSUED_SSRC_BYTES.checked_mul(2).unwrap()
        );
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        assert!(egress.has_issued_ssrc(ssrc));
        assert!(egress.has_issued_ssrc(sibling_ssrc));
    }

    #[test]
    fn retirement_reservations_survive_repeated_pause_and_terminal_cache_cleanup() {
        let start = Instant::now();
        let mut egress = repair_egress(start, 128);
        let initial = egress.runtime_bytes;
        let initial_retirement = runtime::retirement_bytes(&egress.retiring);
        assert!(matches!(
            MediaEgress::new(
                egress
                    .senders
                    .values()
                    .map(|sender| sender.facts.clone())
                    .collect(),
                &[7; 32],
                1024,
                128,
                initial.checked_sub(1).unwrap(),
                start,
                ProtocolClock::new(start, UNIX_EPOCH),
            ),
            Err(crate::AcceptError::SessionLimitExceeded)
        ));
        let first_base = egress.senders[&1]
            .runtime_bytes
            .checked_add(runtime::mid_bytes("audio").unwrap())
            .unwrap();
        let queue_bytes = runtime::queue_bytes(&VecDeque::with_capacity(1));
        let expected = runtime_owned_bytes;
        let write = |sequence| RtpWrite {
            sequence,
            timestamp: 0,
            wallclock: start,
            marker: false,
            payload: vec![7].into(),
            extensions: Vec::with_capacity(2),
        };
        let extension_bytes = runtime::extension_bytes(&write(0));
        let workspace = Vec::<u8>::with_capacity(
            layout::PacketLayout::original(&egress.senders[&1], &write(0))
                .unwrap()
                .bytes,
        )
        .capacity();
        egress.max_runtime_bytes = initial
            .checked_add(workspace)
            .unwrap()
            .checked_add(initial_retirement)
            .unwrap()
            .checked_add(
                queue_bytes
                    .checked_add(extension_bytes)
                    .unwrap()
                    .checked_mul(2)
                    .unwrap(),
            )
            .unwrap();
        let ssrc = egress.senders[&1].ssrc;
        let mut original = vec![0x80, 111, 0, 7, 0, 0, 0, 0];
        original.extend_from_slice(&ssrc.to_be_bytes());
        original.push(7);
        let pointer = original.as_ptr();
        let cache = runtime::retained_entry_bytes(original.capacity())
            .unwrap()
            .checked_add(runtime::RETAINED_GROUP_BYTES)
            .unwrap();
        egress.max_runtime_bytes = egress.max_runtime_bytes.checked_add(cache).unwrap();
        egress.retain_original(1, 7, 0, original, start, 64);
        assert_eq!(egress.retained[&1][&7].packet.as_ptr(), pointer);
        for sequence in [8, 9] {
            egress.admit(start, "audio", write(sequence)).unwrap();
            egress.senders.get_mut(&1).unwrap().active = false;
            egress.cancel_sender_work(1);
            egress.senders.get_mut(&1).unwrap().active = true;
            assert!(!egress.senders[&1].retirement_reserved);
            assert_eq!(egress.runtime_bytes, expected(&egress));
            if sequence == 8 {
                let requests = egress.next_repair_request;
                let limit = egress.max_runtime_bytes;
                let used = egress.runtime_bytes;
                egress.max_runtime_bytes = used.checked_add(initial_retirement).unwrap();
                egress.request_repair(ssrc, 7);
                assert_eq!(egress.next_repair_request, requests);
                assert_eq!(egress.runtime_bytes, used);
                assert!(!egress.senders[&1].retirement_reserved);
                let input = write(9);
                let pointer = input.extensions.as_ptr();
                let ring_capacity = egress.retiring.capacity();
                let slots = egress.retirement_slots;
                egress.max_runtime_bytes = used
                    .checked_add(queue_bytes)
                    .unwrap()
                    .checked_add(extension_bytes)
                    .unwrap();
                let (error, returned) = egress.admit(start, "audio", input).unwrap_err();
                assert_eq!(error, WriteError::WouldBlock);
                assert_eq!(returned.extensions.as_ptr(), pointer);
                assert_eq!(returned.sequence, 9);
                assert_eq!(egress.senders[&1].queue.capacity(), 0);
                assert!(!egress.senders[&1].retirement_reserved);
                assert_eq!(egress.retiring.capacity(), ring_capacity);
                assert_eq!(egress.retirement_slots, slots);
                assert_eq!(egress.runtime_bytes, expected(&egress));
                assert_eq!(egress.runtime_bytes, used);
                egress.max_runtime_bytes = limit;
            }
        }
        assert_eq!(egress.retiring.len(), 2);
        assert_eq!(egress.runtime_bytes, egress.max_runtime_bytes);
        let requests = egress.next_repair_request;
        egress.request_repair(ssrc, 7);
        assert_eq!(egress.next_repair_request, requests);
        assert!(egress.repair_requests.is_empty());
        let (error, returned) = egress.admit(start, "audio", write(10)).unwrap_err();
        assert_eq!(error, WriteError::WouldBlock);
        assert_eq!(returned.sequence, 10);
        assert_eq!(egress.runtime_bytes, expected(&egress));
        let queued = egress.queued_payload_bytes;
        egress.retire_sender(1);
        assert_eq!(
            egress.retiring.len(),
            2,
            "existing jobs own terminal cache cleanup"
        );
        assert_eq!(
            egress.runtime_bytes,
            egress.max_runtime_bytes.saturating_sub(first_base)
        );
        assert_eq!(egress.queued_payload_bytes, queued);
        assert_eq!(egress.retained_bytes, 64);
        assert!(egress.has_issued_ssrc(ssrc));
        egress.clean_retired(start);
        assert!(egress.retiring.is_empty());
        assert_eq!(egress.retained_bytes, 0);
        assert_eq!(egress.queued_payload_bytes, 0);
        assert_eq!(egress.runtime_bytes, expected(&egress));
        assert_eq!(
            egress.runtime_bytes,
            initial
                .saturating_add(workspace)
                .saturating_sub(first_base)
                .saturating_sub(initial_retirement)
                .saturating_add(runtime::retirement_bytes(&egress.retiring))
        );
        egress.admit(start, "other", write(1)).unwrap();
        egress.cancel_sender_work(2);
        let used = egress.runtime_bytes;
        egress.clean_retired(start);
        assert!(egress.senders[&2].retirement_reserved);
        assert_eq!(
            egress.runtime_bytes,
            used.checked_sub(queue_bytes)
                .unwrap()
                .checked_sub(extension_bytes)
                .unwrap()
        );
        egress.admit(start, "other", write(2)).unwrap();
        egress.cancel_sender_work(2);
        let mut live = write(3);
        live.extensions = Vec::with_capacity(3);
        egress.admit(start, "other", live).unwrap();
        egress.begin_shutdown();
        assert!(egress.senders[&2].queue.capacity() > 0);
        assert!(egress.senders[&2].queue.is_empty());
        assert!(egress.retiring.is_empty());
        assert!(egress.senders[&2].retirement_reserved);
        assert_eq!(egress.runtime_bytes, expected(&egress));
    }

    #[test]
    fn retained_backing_moves_without_copy_and_pressure_eviction_is_bounded() {
        let start = Instant::now();
        let mut egress = repair_egress(start, 128);
        let initial = egress.runtime_bytes;
        let ssrc = egress.senders[&1].ssrc;
        let sibling = egress.senders[&2].ssrc;
        let packet = |ssrc: u32, index, capacity| {
            let mut bytes = Vec::with_capacity(capacity);
            bytes.extend_from_slice(&[0x80, 111]);
            bytes.extend_from_slice(&wire_u16(index).to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(&ssrc.to_be_bytes());
            bytes.push(7);
            bytes
        };
        let entry = runtime::retained_entry_bytes(64).unwrap();
        let exact = initial
            .checked_add(entry)
            .unwrap()
            .checked_add(runtime::RETAINED_GROUP_BYTES)
            .unwrap();
        egress.max_runtime_bytes = exact.checked_sub(1).unwrap();
        egress.retain_original(1, 7, 0, packet(ssrc, 7, 64), start, 64);
        assert!(egress.retained.is_empty());
        assert_eq!(egress.runtime_bytes, initial);
        let original = packet(ssrc, 7, 64);
        let pointer = original.as_ptr();
        egress.max_runtime_bytes = exact;
        egress.retain_original(1, 7, 0, original, start, 64);
        assert_eq!(egress.retained[&1][&7].packet.as_ptr(), pointer);
        assert_eq!(egress.runtime_bytes, exact);
        assert_eq!(egress.retained_bytes, 64);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        egress.retain_original(2, 8, 0, packet(sibling, 8, 64), start, 64);
        assert!(!egress.retained.contains_key(&1));
        assert_eq!(egress.retained[&2].len(), 1);
        assert_eq!(egress.runtime_bytes, exact);
        let expired = start + Duration::from_secs(3);
        egress.expire_repair(expired);
        assert_eq!(egress.runtime_bytes, initial);
        egress.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES;
        egress.max_retransmission_bytes = (REPAIR_MAINTENANCE_BUDGET + 1).checked_mul(64).unwrap();
        for offset in 0..=REPAIR_MAINTENANCE_BUDGET {
            let index = 1000u64.checked_add(u64::try_from(offset).unwrap()).unwrap();
            egress.retain_original(1, index, 0, packet(ssrc, index, 64), expired, 64);
        }
        assert_eq!(egress.retained[&1].len(), REPAIR_MAINTENANCE_BUDGET + 1);
        egress.max_runtime_bytes = egress.runtime_bytes;
        let large = packet(
            ssrc,
            2000,
            entry.checked_mul(REPAIR_MAINTENANCE_BUDGET).unwrap(),
        );
        egress.retain_original(1, 2000, 0, large, expired, 64);
        assert_eq!(egress.retained[&1].len(), 1);
        assert!(!egress.retained[&1].contains_key(&2000));
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        egress.begin_shutdown();
        assert_eq!(egress.retained_runtime_bytes, 0);
        assert_eq!(egress.runtime_bytes, initial);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
    }

    #[test]
    fn retained_repairs_fence_rollover_and_share_byte_and_cleanup_budgets() {
        let start = Instant::now();
        let mut egress = repair_egress(start, 128);
        let packet = |ssrc: u32, index: u64, value: u8| {
            let mut packet = vec![0x80, 111];
            packet.extend_from_slice(&wire_u16(index).to_be_bytes());
            packet.extend_from_slice(&0_u32.to_be_bytes());
            packet.extend_from_slice(&ssrc.to_be_bytes());
            packet.push(value);
            packet
        };
        let first_ssrc = egress.senders[&1].ssrc;
        let second_ssrc = egress.senders[&2].ssrc;
        let old = RetainedKey {
            sender: 1,
            index: 7,
        };
        let current = RetainedKey {
            sender: 1,
            index: 65_543,
        };
        let initial = egress.runtime_bytes;
        let request_bytes = runtime::REPAIR_OWNER_BYTES
            .checked_add(runtime::REPAIR_INDEX_BYTES)
            .unwrap();
        egress.retain_original(1, 7, 0, packet(first_ssrc, 7, 1), start, 64);
        let cached = egress.runtime_bytes;
        egress.max_runtime_bytes = cached
            .checked_add(request_bytes)
            .unwrap()
            .checked_sub(1)
            .unwrap();
        let next = egress.next_repair_request;
        egress.request_repair(first_ssrc, 7);
        assert_eq!(egress.next_repair_request, next);
        assert_eq!(egress.runtime_bytes, cached);
        egress.max_runtime_bytes = cached.checked_add(request_bytes).unwrap();
        egress.request_repair(first_ssrc, 7);
        assert_eq!(egress.runtime_bytes, egress.max_runtime_bytes);
        let admitted = egress.requested_repair(old).unwrap();
        egress.finish_repair_request(admitted, old);
        assert_eq!(egress.runtime_bytes, cached);
        egress.max_runtime_bytes = crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES;
        for (sender, ssrc) in [(1, first_ssrc), (2, second_ssrc)] {
            egress.retain_original(sender, 7, 0, packet(ssrc, 7, 1), start, 64);
            egress.request_repair(ssrc, 7);
        }
        let replaced_at = start + Duration::from_secs(1);
        egress.retain_original(
            1,
            current.index,
            0,
            packet(first_ssrc, current.index, 2),
            replaced_at,
            64,
        );
        egress.remove_retained(old);
        egress.request_repair(first_ssrc, 7);
        assert!(egress.retained_packet(old).is_none());
        assert!(egress.requested_repair(old).is_none());
        assert_eq!(
            egress.retained_packet(current).unwrap().packet.last(),
            Some(&2)
        );
        assert_eq!(egress.retained_bytes, 128);
        assert_eq!(egress.retained_expiry.len(), 2);
        let expired_at = start + Duration::from_secs(3);
        egress.expire_repair(expired_at);
        egress.clean_repair_requests();
        assert_eq!(egress.retained_bytes, 64);
        assert_eq!(
            egress.repair_requests.first_key_value().map(|(_, key)| key),
            Some(&current)
        );
        assert_eq!(egress.senders[&1].requested_repairs.len(), 1);
        egress.retain_original(2, 8, 0, packet(second_ssrc, 8, 3), expired_at, 128);
        assert!(egress.retained_packet(current).is_none());
        assert!(
            egress
                .senders
                .values()
                .all(|sender| sender.requested_repairs.is_empty())
        );
        assert_eq!(egress.retained_bytes, 128);
        egress.expire_repair(expired_at + Duration::from_secs(3));
        egress.clean_repair_requests();
        assert!(egress.retained.is_empty());
        assert!(egress.retained_expiry.is_empty());
        assert!(egress.repair_requests.is_empty());
        assert_eq!(egress.retained_bytes, 0);

        egress.max_retransmission_bytes = (REPAIR_MAINTENANCE_BUDGET + 1) * 64;
        let refill_at = expired_at + Duration::from_secs(3);
        let cleanup_at = refill_at + Duration::from_secs(3);
        for offset in 0..=u64::try_from(REPAIR_MAINTENANCE_BUDGET).unwrap() {
            let index = 65_544 + offset;
            egress.retain_original(1, index, 0, packet(first_ssrc, index, 4), refill_at, 64);
            egress.request_repair(first_ssrc, wire_u16(index));
        }
        egress.expire_repair(cleanup_at);
        egress.clean_repair_requests();
        assert_eq!(egress.retained_bytes, 64);
        assert!(
            egress
                .next_deadline(cleanup_at)
                .is_some_and(|at| at <= cleanup_at)
        );
        egress.expire_repair(cleanup_at);
        egress.clean_repair_requests();
        assert_eq!(egress.retained_bytes, 0);
        assert!(egress.retained.is_empty());
        assert!(egress.retained_expiry.is_empty());
        assert!(
            egress
                .senders
                .values()
                .all(|sender| sender.requested_repairs.is_empty())
        );
        assert!(egress.repair_requests.is_empty());
        assert_eq!(egress.runtime_bytes, initial);
        for (sender, ssrc) in [(1, first_ssrc), (2, second_ssrc)] {
            egress.retain_original(sender, 131_079, 0, packet(ssrc, 131_079, 5), cleanup_at, 64);
            egress.request_repair(ssrc, 7);
        }
        egress.cancel_sender_work(1);
        let charged = egress.runtime_bytes;
        egress.clean_repair_requests();
        assert_eq!(
            egress.runtime_bytes,
            charged.saturating_sub(runtime::REPAIR_INDEX_BYTES)
        );
        assert_eq!(egress.retiring[0].repairs.len(), 1);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        egress.begin_shutdown();
        assert!(
            egress
                .senders
                .values()
                .all(|sender| sender.requested_repairs.is_empty())
        );
        assert_eq!(egress.runtime_bytes, initial);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
    }

    #[test]
    fn terminal_sender_reclaims_charged_work_without_resetting_continuing_state() {
        let start = Instant::now();
        let mut egress = repair_egress(start, (REPAIR_MAINTENANCE_BUDGET + 2) * 64);
        let backlog = 8_193;
        egress.max_payload_bytes = backlog;
        let retired_ssrc = egress.senders[&1].ssrc;
        let continuing_ssrc = egress.senders[&2].ssrc;
        for index in 0..=u64::try_from(REPAIR_MAINTENANCE_BUDGET).unwrap() {
            let mut packet = vec![0x80, 111];
            packet.extend_from_slice(&wire_u16(index).to_be_bytes());
            packet.extend_from_slice(&0_u32.to_be_bytes());
            packet.extend_from_slice(&retired_ssrc.to_be_bytes());
            packet.push(7);
            egress.retain_original(1, index, 0, packet, start, 64);
        }
        let write = |sequence, payload| RtpWrite {
            sequence,
            timestamp: 0,
            wallclock: start,
            marker: false,
            payload: vec![payload].into(),
            extensions: Vec::new(),
        };
        for sequence in 0..u64::try_from(backlog).unwrap() {
            egress
                .admit(start, "audio", write(sequence.saturating_add(257), 7))
                .unwrap();
        }
        assert_eq!(egress.queued_packets, backlog);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        let before = (
            egress.runtime_bytes,
            egress.queued_packets,
            egress.queued_payload_bytes,
            egress.queued_transport_bytes,
        );
        let input = write(0, 8);
        let pointer = input.payload.as_ptr();
        let (error, returned) = egress.admit(start, "other", input).unwrap_err();
        assert_eq!(error, WriteError::WouldBlock);
        assert_eq!(returned.payload.as_ptr(), pointer);
        assert_eq!(
            (
                egress.runtime_bytes,
                egress.queued_packets,
                egress.queued_payload_bytes,
                egress.queued_transport_bytes
            ),
            before
        );
        assert_eq!(egress.senders[&2].queue.capacity(), 0);
        let mut continuing_packet = vec![0x80, 111, 0, 9, 0, 0, 0, 0];
        continuing_packet.extend_from_slice(&continuing_ssrc.to_be_bytes());
        continuing_packet.push(8);
        egress.retain_original(2, 9, 0, continuing_packet, start, 64);
        egress.request_repair(retired_ssrc, 7);
        egress.request_repair(continuing_ssrc, 9);
        egress.observe_receipt(
            start,
            crate::rtcp::ReportReceipt {
                ssrc: retired_ssrc,
                highest_sequence: 256,
            },
        );
        let continuing = egress.senders.get_mut(&2).unwrap();
        continuing.next_rtx_sequence = 77;
        continuing.highest_sequence = Some(100);
        continuing.committed_packets = 8;
        let twcc = egress.next_twcc;
        let queued_bytes = egress.queued_payload_bytes;
        egress.retire_sender(1);
        assert_eq!(egress.stats().1.len(), 1);
        assert_eq!(egress.stats().1[0].ssrc, continuing_ssrc);
        assert!(egress.has_issued_ssrc(retired_ssrc));
        assert!(egress.has_issued_ssrc(continuing_ssrc));
        assert!(egress.poll_event().is_none());
        assert_eq!(
            egress
                .admit(
                    start,
                    "audio",
                    RtpWrite {
                        sequence: 900,
                        timestamp: 0,
                        wallclock: start,
                        marker: false,
                        payload: vec![7].into(),
                        extensions: Vec::new(),
                    }
                )
                .unwrap_err()
                .0,
            WriteError::UnknownStream
        );
        egress.request_repair(retired_ssrc, 8);
        assert_eq!(egress.repair_requests.len(), 2);
        assert_eq!(
            egress.queued_payload_bytes, queued_bytes,
            "unfinished buffers stay charged"
        );
        egress.clean_retired(start);
        assert_eq!(
            egress.queued_packets,
            backlog.checked_sub(REPAIR_MAINTENANCE_BUDGET).unwrap(),
            "retirement yields after one work quantum"
        );
        assert!(egress.next_deadline(start).is_some_and(|at| at <= start));
        let cleanup_work = backlog
            .checked_add(REPAIR_MAINTENANCE_BUDGET)
            .unwrap()
            .checked_add(3)
            .unwrap();
        for _ in 1..cleanup_work.div_ceil(REPAIR_MAINTENANCE_BUDGET) {
            egress.clean_retired(start);
        }
        egress.clean_repair_requests();
        assert!(egress.retiring.is_empty());
        assert_eq!(egress.queued_payload_bytes, 0);
        assert_eq!(egress.queued_transport_bytes, 0);
        assert_eq!(egress.queued_packets, 0);
        assert_eq!(egress.retained_bytes, 64);
        assert_eq!(egress.retained_expiry.len(), 1);
        assert_eq!(
            egress.repair_requests.first_key_value().map(|(_, key)| key),
            Some(&RetainedKey {
                sender: 2,
                index: 9
            })
        );
        assert_eq!(egress.senders[&2].requested_repairs.len(), 1);
        let continuing = &egress.senders[&2];
        assert_eq!(continuing.next_rtx_sequence, 77);
        assert_eq!(continuing.highest_sequence, Some(100));
        assert_eq!(continuing.committed_packets, 8);
        assert_eq!(egress.next_twcc, twcc);

        for sequence in 101..=357 {
            egress
                .admit(
                    start,
                    "other",
                    RtpWrite {
                        sequence,
                        timestamp: 0,
                        wallclock: start,
                        marker: false,
                        payload: vec![8].into(),
                        extensions: Vec::new(),
                    },
                )
                .unwrap();
        }
        let charged = egress.queued_transport_bytes;
        egress.senders.get_mut(&2).unwrap().active = false;
        egress.cancel_sender_work(2);
        assert!(egress.senders[&2].queue.is_empty());
        assert_eq!(egress.queued_transport_bytes, charged);
        let original = RetainedKey {
            sender: 2,
            index: 9,
        };
        assert!(egress.requested_repair(original).is_none());
        egress.request_repair(continuing_ssrc, 9);
        assert!(egress.requested_repair(original).is_none());
        egress.senders.get_mut(&2).unwrap().active = true;
        egress
            .admit(
                start,
                "other",
                RtpWrite {
                    sequence: 358,
                    timestamp: 0,
                    wallclock: start,
                    marker: false,
                    payload: vec![9].into(),
                    extensions: Vec::new(),
                },
            )
            .unwrap();
        egress.request_repair(continuing_ssrc, 9);
        let requested = egress.requested_repair(original).unwrap();
        assert!(
            egress
                .repair_requests
                .first_key_value()
                .is_some_and(|(&id, _)| id < requested)
        );
        egress.clean_repair_requests();
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));
        assert_eq!(egress.requested_repair(original), Some(requested));
        assert_eq!(
            egress.repair_requests.first_key_value(),
            Some((&requested, &original))
        );
        egress.clean_retired(start);
        assert_eq!(egress.queued_packets, 2);
        assert_eq!(egress.senders[&2].queue.len(), 1);
        assert!(!egress.retiring.is_empty());
        for _ in 0..3 {
            egress.clean_retired(start);
        }
        assert!(egress.retiring.is_empty());
        assert_eq!(egress.queued_packets, 1);
        assert_eq!(egress.queued_payload_bytes, 1);
        assert_eq!(
            egress.senders[&2]
                .queue
                .front()
                .unwrap()
                .media
                .payload
                .as_ref(),
            &[9]
        );
        assert_eq!(egress.retained_bytes, 64);
        assert_eq!(egress.requested_repair(original), Some(requested));
        assert_eq!(egress.next_twcc, twcc);
        assert_eq!(egress.senders[&2].next_rtx_sequence, 77);
        assert_eq!(egress.runtime_bytes, runtime_owned_bytes(&egress));

        egress.finish_repair_request(requested, original);
        egress.next_repair_request = Some(u64::MAX);
        egress.request_repair(continuing_ssrc, 9);
        assert_eq!(egress.requested_repair(original), Some(u64::MAX));
        egress.finish_repair_request(u64::MAX, original);
        egress.request_repair(continuing_ssrc, 9);
        assert!(egress.repair_requests.is_empty());
    }

    #[test]
    fn admitted_queue_age_drains_native_media_without_changing_estimate() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        egress
            .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
            .unwrap();
        let configured_rate = egress.controller.snapshot().pacing_transport_rate;
        let nominal = egress.bandwidth_estimate;
        for sequence in 0..4 {
            egress
                .admit(
                    start,
                    "audio",
                    RtpWrite {
                        sequence,
                        timestamp: 0,
                        wallclock: start,
                        marker: false,
                        payload: vec![0; 128].into(),
                        extensions: Vec::new(),
                    },
                )
                .unwrap();
        }
        egress
            .update_controller(
                start + Duration::from_millis(1_995),
                None,
                &[],
                Duration::ZERO,
                false,
                0,
            )
            .unwrap();
        let required = u64::try_from(egress.queued_transport_bytes).unwrap() * 1_600;
        assert_eq!(egress.controller.snapshot().pacing_transport_rate, required);
        assert_eq!(egress.envelope.unwrap().pacing_transport_rate, required);
        assert_eq!(egress.bandwidth_estimate, nominal);
        assert_eq!(egress.controller.diagnostics().charged_bytes, 0);
        egress.discard_pending("audio").unwrap();
        assert_eq!(
            egress.controller.snapshot().pacing_transport_rate,
            configured_rate
        );
        assert_eq!(egress.queue_enqueued_time_sum, 0);
    }

    #[test]
    fn queue_age_excludes_congestion_pause_and_averages_admission_times() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        egress
            .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
            .unwrap();
        let configured_rate = egress.controller.snapshot().pacing_transport_rate;
        let write = |sequence, at| RtpWrite {
            sequence,
            timestamp: 0,
            wallclock: at,
            marker: false,
            payload: vec![0; 128].into(),
            extensions: Vec::new(),
        };
        egress.admit(start, "audio", write(0, start)).unwrap();
        let window = egress.envelope.unwrap().max_rtp_bytes_in_flight;
        egress
            .update_controller(
                start + Duration::from_millis(10),
                None,
                &[],
                Duration::ZERO,
                false,
                window,
            )
            .unwrap();
        let resumed = start + Duration::from_secs(10);
        egress
            .update_controller(resumed, None, &[], Duration::ZERO, false, 0)
            .unwrap();
        assert_eq!(
            egress.controller.snapshot().pacing_transport_rate,
            configured_rate
        );
        egress.admit(resumed, "audio", write(1, resumed)).unwrap();
        egress
            .update_controller(
                resumed + Duration::from_millis(1_989),
                None,
                &[],
                Duration::ZERO,
                false,
                0,
            )
            .unwrap();
        let required =
            u64::try_from(u128::try_from(egress.queued_transport_bytes).unwrap() * 8_000 / 6)
                .unwrap();
        assert_eq!(egress.controller.snapshot().pacing_transport_rate, required);
    }

    #[test]
    fn same_route_hint_and_queue_changes_preserve_spent_pacer_service() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000)));
        egress
            .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
            .unwrap();
        assert_eq!(egress.controller.prepare_padding(Duration::ZERO), Some(187));
        egress
            .controller
            .commit_send(Duration::ZERO, 200, 200, true);
        egress.set_desired_bitrate(None);
        egress
            .update_controller(
                start + Duration::from_millis(1),
                Some((1, false)),
                &[],
                Duration::ZERO,
                false,
                0,
            )
            .unwrap();
        egress.discard_pending("audio").unwrap();
        egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(3_000_000)));
        let resumed = start + Duration::from_millis(2);
        egress
            .update_controller(resumed, Some((1, true)), &[], Duration::ZERO, false, 0)
            .unwrap();
        assert_eq!(egress.controller.diagnostics().charged_bytes, 200);
        assert!(
            egress
                .controller
                .next_deadline(resumed.duration_since(start), None, Some((0, 200)))
                .unwrap()
                > resumed.duration_since(start)
        );
        assert_eq!(egress.test_initial_probe_started_at(), Some(start));
    }

    #[test]
    fn receiver_progress_is_sender_associated_bounded_and_wrap_safe() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        let ssrc = egress.senders[&1].ssrc;
        // Local writes and commits are not receiver confirmation.
        egress.senders.get_mut(&1).unwrap().committed_packets = 100;
        assert!(egress.poll_event().is_none());
        egress.observe_receipt(
            start,
            crate::rtcp::ReportReceipt {
                ssrc: ssrc ^ 1,
                highest_sequence: 99,
            },
        );
        assert!(egress.poll_event().is_none());
        for sequence in [u32::MAX - 1, u32::MAX, 0, 2, 1, 2] {
            egress.observe_receipt(
                start,
                crate::rtcp::ReportReceipt {
                    ssrc,
                    highest_sequence: sequence,
                },
            );
        }
        assert!(
            matches!(egress.poll_event(), Some(Event::ReceiverReport(report))
            if report.mid.as_ref() == "audio" && report.ssrc == ssrc
                && report.arrival == start && report.extended_highest_sequence == 2)
        );
        assert!(
            egress.poll_event().is_none(),
            "reports coalesce into one fact per sender"
        );
        egress.observe_receipt(
            start,
            crate::rtcp::ReportReceipt {
                ssrc,
                highest_sequence: u32::MAX,
            },
        );
        assert!(
            egress.poll_event().is_none(),
            "a delayed old report cannot reverse progress"
        );
    }

    #[test]
    fn sender_report_correspondence_and_incoming_wire_facts_use_the_fixed_anchor() {
        let start = Instant::now();
        let mut egress = audio_egress(start);
        let sender = egress.senders.get_mut(&1).unwrap();
        sender.latest_rtp = Some((u32::MAX - 11_999, start + Duration::from_millis(250)));
        sender.committed_packets = 2;
        sender.committed_payload_bytes = 777;
        let packet =
            sender_report(sender, start + Duration::from_millis(750), &egress.clock).unwrap();
        assert_eq!(u32::from_be_bytes(packet[20..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(packet[24..28].try_into().unwrap()), 777);
        let parsed = crate::rtcp::parse(
            &packet,
            start,
            crate::transport::PathEpoch::from_value(1),
            None,
        )
        .unwrap();
        assert!(
            matches!(&parsed.lifecycle[1], crate::rtcp::LifecycleFact::Cname { ssrc, cname }
            if *ssrc == sender.ssrc && cname.as_ref() == sender.facts.rtcp.as_ref().unwrap().cname)
        );
        let arrival = start + Duration::from_secs(2);
        let mut ingress = crate::ingress::IngressOwner::new(
            Box::new([]),
            4_096,
            crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            99,
            [b'r'; 32],
            crate::receive_feedback::ReceiveFeedback::new(None, start),
        )
        .unwrap();
        ingress.accept_rtcp(
            arrival,
            packet,
            crate::transport::PathEpoch::from_value(1),
            None,
            None,
        );
        assert!(
            matches!(ingress.poll_event(), Some(Event::SenderReport(report))
            if report.ssrc == sender.ssrc && report.arrival == arrival
                && report.ntp_seconds == 2_208_988_817 && report.ntp_fraction == 3 << 30
                && report.rtp_timestamp == 12_000)
        );
    }
}
