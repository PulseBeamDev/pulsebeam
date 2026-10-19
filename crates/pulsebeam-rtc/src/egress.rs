#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "sender indexes are resolved from the same immutable negotiated sender array"
)]

use std::{
    collections::{BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    CommandError, Event, ForwardedMedia, FrameBoundary, FrameDependencies, FrameId,
    GlobalMediaTime, KeyframeRequestReason, MediaKind, MediaPacket, MediaPayloadBitrate,
    PlayoutDelay, PlayoutDelayStatus, SenderAssignment, SenderId, SenderStats, TimePoint,
    congestion::{
        ControllerInput, EcnValidation, FeedbackSample, SafeRtpEnvelope, ScreamController,
    },
    dependency_descriptor::{DescriptorContinuity, PreparedDescriptor},
    negotiation::EgressSenderFacts,
    pacer::Pacer,
    packet::RtpPacket,
    scheduler::SenderScheduler,
    sent_history::PacketFeedback,
    transport::{
        DatagramKind, PreparedRtpIdentity, PreparedTransmit, RtpService, Transport, TransportError,
    },
};

const MAX_QUEUED_MEDIA_PACKETS: usize = 8_192;
const MAX_PACED_TRANSPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXPIRATIONS_PER_POLL: usize = 256;
const RTP_TRANSPORT_ALLOWANCE: usize = 96;
const PROBE_WINDOW: Duration = Duration::from_secs(5);
const PROBE_MIN_TARGET: u64 = 3_000;
const PROBE_ALLOWANCE: u64 = 48 * 1024;
const INITIAL_PROBE_DURATION: Duration = Duration::from_millis(100);
const INITIAL_PROBE_CLUSTERS: u64 = 2;
const MAX_PROBE_PACKETS: u8 = 192;
const MAX_PROBE_ACCOUNTING: usize = 32_768;

pub(crate) struct MediaEgress {
    senders: Box<[Sender]>,
    queued_packets: usize,
    expiry_cursor: usize,
    eligible: Vec<Option<usize>>,
    queued_payload_bytes: usize,
    queued_descriptor_bytes: usize,
    queued_transport_bytes: usize,
    max_payload_bytes: usize,
    max_retransmission_bytes: usize,
    retained: VecDeque<RetainedPacket>,
    retained_bytes: usize,
    repair_requests: VecDeque<(usize, u16)>,
    next_twcc: u64,
    pending: Option<PendingTransmit>,
    pacer: Pacer,
    media_pacer: Pacer,
    probe_pacer: Pacer,
    probe_pacing_rate: u64,
    last_send_attempt: Option<Instant>,
    scheduler: SenderScheduler,
    controller: ScreamController,
    controller_origin: Instant,
    envelope: Option<SafeRtpEnvelope>,
    active_path: Option<u64>,
    path_available: bool,
    media_budget: u64,
    budget_changed: Option<MediaPayloadBitrate>,
    next_rtcp: Instant,
    rtcp_cursor: usize,
    probe_remaining: u8,
    probe_sender: Option<usize>,
    feedback_recovery: FeedbackRecovery,
    probe_target_bytes: u64,
    probe_packet_count: u8,
    probe_started_at: Option<Instant>,
    probe_start_queue_delay: Duration,
    last_probe: Option<Instant>,
    initial_probe_started_at: Option<Instant>,
    initial_probe_allowance: u64,
    probe_followup: bool,
    probe_history: VecDeque<ProbeCluster>,
    probe_sent_bytes: u64,
    probe_last_sent_at: Option<Instant>,
    probe_received_bytes: u64,
    probe_lost_packets: u32,
    probe_reported_packets: u32,
    emitted_rtp: VecDeque<(Instant, u64, bool)>,
    probe_disabled_until: Option<Instant>,
    offer_window_started: Instant,
    offered_bytes: u64,
    admitted_bytes: u64,
    offered_rate: u64,
    admitted_rate: u64,
    rtp_bytes_in_flight: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct PayloadAdmission {
    debt: u128,
    updated_at: Option<Instant>,
}

impl PayloadAdmission {
    fn eligible(&mut self, now: Instant, rate: u64, horizon: Duration) -> bool {
        if let Some(previous) = self.updated_at {
            self.debt = self.debt.saturating_sub(
                now.saturating_duration_since(previous)
                    .as_nanos()
                    .saturating_mul(u128::from(rate)),
            );
        }
        self.updated_at = Some(now);
        self.debt <= horizon.as_nanos().saturating_mul(u128::from(rate))
    }

    fn charge(&mut self, bytes: usize) {
        self.debt = self
            .debt
            .saturating_add((bytes as u128).saturating_mul(8_000_000_000));
    }
}

struct Sender {
    facts: EgressSenderFacts,
    admission: PayloadAdmission,
    assignment: Option<SenderAssignment>,
    ssrc: u32,
    rtx_ssrc: u32,
    primary_feedback_observed: bool,
    next_sequence: u64,
    next_rtx_sequence: u64,
    base_timestamp: u32,
    base_global: Option<GlobalMediaTime>,
    latest_admitted_global: Option<GlobalMediaTime>,
    latest_committed_global: Option<GlobalMediaTime>,
    open_frame: Option<OpenFrame>,
    queue: VecDeque<QueuedMedia>,
    queued_frames: BTreeSet<FrameId>,
    queued_payload_bytes: usize,
    expiring_frame: Option<FrameId>,
    dependency_chain_valid: bool,
    descriptor: Option<DescriptorContinuity>,
    keyframe_requested: Option<KeyframeRequestReason>,
    fir_sequences: Vec<(u32, u8)>,
    audio_talkspurt_pending: bool,
    committed_packets: u64,
    committed_payload_bytes: u64,
    playout_first_sequence: Option<u64>,
    playout_confirmed: bool,
    playout_ever_signaled: bool,
}

impl Sender {
    fn selected_bitrate(&self) -> u64 {
        self.assignment
            .map_or(0, |assignment| assignment.selected_bitrate.as_bps())
    }

    fn potential_bitrate(&self) -> u64 {
        self.assignment
            .map_or(0, |assignment| assignment.potential_bitrate.as_bps())
    }

    fn playout_to_signal(&self) -> Option<PlayoutDelay> {
        if self.facts.playout_extension_id.is_some() && !self.playout_confirmed {
            self.assignment
                .and_then(|assignment| assignment.playout_delay)
        } else {
            None
        }
    }

    fn note_playout_commit(&mut self) {
        if self.playout_to_signal().is_some() {
            self.playout_first_sequence
                .get_or_insert(self.next_sequence);
            self.playout_ever_signaled = true;
        }
    }

    fn playout_status(&self) -> PlayoutDelayStatus {
        match (
            self.assignment
                .and_then(|assignment| assignment.playout_delay),
            self.facts.playout_extension_id,
            self.playout_confirmed,
        ) {
            (None, _, _) => PlayoutDelayStatus::Unspecified,
            (Some(_), None, _) => PlayoutDelayStatus::NotSignaled,
            (_, _, true) => PlayoutDelayStatus::Confirmed,
            _ => PlayoutDelayStatus::Pending,
        }
    }
}

#[derive(Clone)]
struct OpenFrame {
    id: FrameId,
    global: GlobalMediaTime,
    random_access: bool,
    discardable: bool,
    dependencies: FrameDependencies,
    source_ssrc: u32,
    source_timestamp: u32,
    source_sequence: u16,
}

struct QueuedMedia {
    queued_at: Instant,
    media: ForwardedMedia,
    descriptor_bytes: usize,
    payload_bytes: usize,
    transport_estimate: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FeedbackRecovery {
    Idle,
    Soliciting,
    AwaitingReport,
    Followup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeResult {
    Pending,
    Successful,
    Failed,
}

#[derive(Clone, Copy, Debug)]
struct ProbeCluster {
    started_at: Instant,
    last_sent_at: Instant,
    finished_at: Instant,
    deadline: Instant,
    sent_bytes: u64,
    target_bytes: u64,
    packet_count: u32,
    received_bytes: u64,
    lost_packets: u32,
    reported_packets: u32,
    result: ProbeResult,
}

struct RetainedPacket {
    sender: usize,
    sequence: u16,
    timestamp: u32,
    packet: Vec<u8>,
    retained_at: Instant,
    last_repair_at: Option<Instant>,
    global: GlobalMediaTime,
    transport_bytes: usize,
}

enum PendingTransmit {
    Original {
        sender: usize,
        sequence: u16,
        timestamp: u32,
        twcc: Option<u16>,
        packet: Vec<u8>,
        descriptor: Option<PreparedDescriptor>,
    },
    Repair {
        sender: usize,
        original_sequence: u16,
        sequence: u16,
        twcc: Option<u16>,
    },
    Control,
    Padding {
        sender: usize,
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
    pub(crate) media_budget: u64,
    pub(crate) pacing_bitrate: u64,
}

impl MediaEgress {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        facts: Box<[EgressSenderFacts]>,
        randomness: &[u8; 32],
        max_payload_bytes: usize,
        max_retransmission_bytes: usize,
        accepted_at: Instant,
    ) -> Self {
        let senders: Box<[_]> = facts
            .into_vec()
            .into_iter()
            .map(|facts| {
                let seed = sender_seed(randomness, facts.id.value());
                Sender {
                    assignment: None,
                    ssrc: nonzero_u32([seed[0], seed[1], seed[2], seed[3]]),
                    rtx_ssrc: nonzero_u32([seed[10], seed[11], seed[12], seed[13]]),
                    next_sequence: u64::from(u16::from_be_bytes([seed[4], seed[5]])),
                    next_rtx_sequence: u64::from(u16::from_be_bytes([seed[14], seed[15]])),
                    base_timestamp: u32::from_be_bytes([seed[6], seed[7], seed[8], seed[9]]),
                    base_global: None,
                    latest_admitted_global: None,
                    latest_committed_global: None,
                    admission: PayloadAdmission::default(),
                    open_frame: None,
                    queue: VecDeque::new(),
                    queued_frames: BTreeSet::new(),
                    queued_payload_bytes: 0,
                    expiring_frame: None,
                    dependency_chain_valid: true,
                    descriptor: facts
                        .dependency_extension_id
                        .map(|_| DescriptorContinuity::default()),
                    keyframe_requested: None,
                    fir_sequences: Vec::new(),
                    audio_talkspurt_pending: facts.kind == MediaKind::Audio,
                    primary_feedback_observed: false,
                    committed_packets: 0,
                    committed_payload_bytes: 0,
                    playout_first_sequence: None,
                    playout_confirmed: false,
                    playout_ever_signaled: false,
                    facts,
                }
            })
            .collect();
        let desired = senders.iter().fold(0_u64, |total, sender| {
            total.saturating_add(sender.potential_bitrate())
        });
        let twcc_seed = sender_seed(randomness, u16::MAX);
        let sender_count = senders.len();
        let mut owner = Self {
            senders,
            queued_packets: 0,
            expiry_cursor: 0,
            eligible: vec![None; sender_count],
            queued_payload_bytes: 0,
            queued_descriptor_bytes: 0,
            queued_transport_bytes: 0,
            max_payload_bytes,
            max_retransmission_bytes,
            retained: VecDeque::new(),
            retained_bytes: 0,
            repair_requests: VecDeque::new(),
            next_twcc: u64::from(u16::from_be_bytes([twcc_seed[0], twcc_seed[1]])),
            pending: None,
            pacer: Pacer::default(),
            media_pacer: Pacer::default(),
            probe_pacer: Pacer::default(),
            probe_pacing_rate: 0,
            last_send_attempt: None,
            scheduler: SenderScheduler::new(sender_count),
            controller: ScreamController::new(desired, None),
            controller_origin: accepted_at,
            envelope: None,
            active_path: None,
            path_available: false,
            media_budget: 0,
            budget_changed: None,
            next_rtcp: accepted_at
                .checked_add(Duration::from_secs(1))
                .unwrap_or(accepted_at),
            rtcp_cursor: 0,
            probe_remaining: 0,
            probe_sender: None,
            feedback_recovery: FeedbackRecovery::Idle,
            probe_target_bytes: 0,
            probe_packet_count: 0,
            probe_started_at: None,
            probe_start_queue_delay: Duration::ZERO,
            last_probe: None,
            initial_probe_started_at: None,
            initial_probe_allowance: PROBE_ALLOWANCE,
            probe_followup: false,
            probe_history: VecDeque::with_capacity(64),
            probe_sent_bytes: 0,
            probe_last_sent_at: None,
            probe_received_bytes: 0,
            probe_lost_packets: 0,
            probe_reported_packets: 0,
            emitted_rtp: VecDeque::new(),
            probe_disabled_until: None,
            offer_window_started: accepted_at,
            offered_bytes: 0,
            admitted_bytes: 0,
            offered_rate: 0,
            admitted_rate: 0,
            rtp_bytes_in_flight: 0,
        };
        owner.update_media_budget(
            owner
                .controller
                .snapshot(desired, 0, 0)
                .target_media_payload_rate,
        );
        owner.budget_changed = None;
        owner
    }

    fn clear_sender_media(
        &mut self,
        sender: SenderId,
        retain_repairs: bool,
    ) -> Result<(), CommandError> {
        let index = self.sender_index(sender)?;
        // Protection consumes the SRTP index. Public poll normally commits it before returning.
        if self.pending.is_some() {
            return Err(CommandError::InvalidState);
        }
        while self.pop_head(index).is_some() {}
        if !retain_repairs {
            let mut retained_bytes = 0usize;
            self.retained.retain(|packet| {
                if packet.sender != index {
                    return true;
                }
                retained_bytes = retained_bytes.saturating_add(packet.transport_bytes);
                false
            });
            self.retained_bytes = self.retained_bytes.saturating_sub(retained_bytes);
            self.repair_requests.retain(|(owner, _)| *owner != index);
        }
        let state = &mut self.senders[index];
        state.open_frame = None;
        state.expiring_frame = None;
        state.dependency_chain_valid = false;
        state.latest_admitted_global = state.latest_committed_global;
        state.keyframe_requested = None;
        state.audio_talkspurt_pending = state.facts.kind == MediaKind::Audio;
        if let Some(continuity) = &mut state.descriptor {
            continuity.reset_source();
        }
        Ok(())
    }

    pub(crate) fn set_assignment(
        &mut self,
        sender: SenderId,
        assignment: Option<SenderAssignment>,
        at: TimePoint,
        recover: bool,
    ) -> Result<(), CommandError> {
        let index = self.sender_index(sender)?;
        let previous = self.senders[index].assignment;
        if self.pending.is_some() {
            return Err(CommandError::InvalidState);
        }
        if let Some(next) = assignment {
            if next.selected_bitrate > next.potential_bitrate
                || next.decode_target_mask == Some(0)
                || [
                    next.delivery.max_media_age,
                    next.delivery.max_queue_residence,
                    next.delivery.repair_window,
                ]
                .into_iter()
                .any(|limit| {
                    limit.as_micros() > u128::from(u64::MAX)
                        || at.monotonic.checked_add(limit).is_none()
                })
            {
                return Err(CommandError::InvalidSenderAssignment);
            }
            if next.playout_delay.is_none() && self.senders[index].playout_ever_signaled {
                return Err(CommandError::PlayoutResetRequiresNewSender(sender));
            }
        }
        let same_source = previous
            .zip(assignment)
            .is_some_and(|(old, next)| old.source == next.source);
        if recover && !same_source {
            return Err(CommandError::SourceIncarnationMismatch);
        }
        let paused = previous.zip(assignment).is_some_and(|(old, next)| {
            old.selected_bitrate.as_bps() > 0 && next.selected_bitrate.as_bps() == 0
        });
        let selection_changed = previous
            .zip(assignment)
            .is_some_and(|(old, next)| old.decode_target_mask != next.decode_target_mask);
        if !same_source || paused || recover || selection_changed {
            self.clear_sender_media(sender, same_source && !paused && assignment.is_some())?;
        }
        let state = &mut self.senders[index];
        if recover {
            state.dependency_chain_valid = true;
        }
        let old_rate = state.selected_bitrate();
        state
            .admission
            .eligible(at.monotonic, old_rate, Duration::ZERO);
        if previous.and_then(|old| old.playout_delay)
            != assignment.and_then(|next| next.playout_delay)
        {
            state.playout_first_sequence = None;
            state.playout_confirmed = false;
        }
        state.assignment = assignment;
        self.expire_repair(at.monotonic);
        self.policy_updated(at);
        Ok(())
    }

    fn policy_updated(&mut self, at: TimePoint) {
        self.expire(at, MAX_EXPIRATIONS_PER_POLL);
        let capacity = self
            .controller
            .snapshot(
                self.potential_bitrate(),
                self.rtp_bytes_in_flight,
                self.queued_transport_bytes as u64,
            )
            .target_media_payload_rate;
        self.media_pacer.reprice(at.monotonic, capacity);
        self.update_media_budget(capacity);
    }

    pub(crate) fn admit(
        &mut self,
        at: TimePoint,
        sender: SenderId,
        media: ForwardedMedia,
    ) -> Result<(), (CommandError, ForwardedMedia)> {
        let admission = (|| {
            let sender_index = self.sender_index(sender)?;
            let assignment = self.senders[sender_index]
                .assignment
                .ok_or(CommandError::SenderUnbound(sender))?;
            if media.source != assignment.source {
                return Err(CommandError::SourceIncarnationMismatch);
            }
            let packet = RtpPacket::parse(media.packet.bytes())
                .map_err(|_| CommandError::InvalidFrameMetadata)?;
            let payload_bytes = packet.payload().len();
            let transport_estimate = media
                .packet
                .bytes()
                .len()
                .saturating_add(RTP_TRANSPORT_ALLOWANCE)
                .saturating_add(
                    if self.senders[sender_index].descriptor.is_some()
                        && media.frame.descriptor.is_some()
                    {
                        pulsebeam_core::dd::MAX_DD_LEN
                    } else {
                        0
                    },
                );
            self.validate_frame(at, sender_index, &media, &packet)?;
            crate::dependency_descriptor::validate(
                &media.frame,
                self.senders[sender_index]
                    .descriptor
                    .as_ref()
                    .and(assignment.decode_target_mask),
            )
            .map_err(|_| CommandError::InvalidFrameMetadata)?;
            self.offered_bytes = self.offered_bytes.saturating_add(payload_bytes as u64);
            if self
                .latest_useful_at(sender_index, media.packet.global_media_at())
                .is_none_or(|deadline| at.global > deadline)
            {
                return Err(CommandError::MediaExpired);
            }
            let pacer_wait = self.predicted_pacer_wait(at.monotonic);
            if self.senders[sender_index].selected_bitrate() == 0
                || !self.useful_at(sender_index, media.packet.global_media_at(), at, pacer_wait)
            {
                return Err(CommandError::WouldBlock);
            }
            if self.queued_packets >= MAX_QUEUED_MEDIA_PACKETS
                || payload_bytes
                    > self
                        .max_payload_bytes
                        .saturating_sub(self.queued_payload_bytes)
                || transport_estimate
                    > MAX_PACED_TRANSPORT_BYTES.saturating_sub(self.queued_transport_bytes)
            {
                return Err(CommandError::WouldBlock);
            }

            let horizon = assignment.delivery.max_queue_residence;
            let mut admission = self.senders[sender_index].admission;
            let payload_eligible = admission.eligible(
                at.monotonic,
                self.senders[sender_index].selected_bitrate(),
                horizon,
            );
            // Charge the entire frame, but gate its start rather than cutting its
            // tail whenever a keyframe exceeds the nominal sender burst budget.
            // Continuations still obey the queue, usefulness, and pacer bounds above.
            if !payload_eligible
                && matches!(
                    media.frame.boundary,
                    FrameBoundary::Start | FrameBoundary::Complete
                )
            {
                return Err(CommandError::WouldBlock);
            }

            let descriptor_bytes = media
                .frame
                .descriptor
                .as_ref()
                .map_or(0, |_| crate::SourceDescriptor::MEMORY_CHARGE);
            if descriptor_bytes
                > MAX_PACED_TRANSPORT_BYTES.saturating_sub(self.queued_descriptor_bytes)
            {
                return Err(CommandError::WouldBlock);
            }
            self.apply_frame_admission(sender_index, &media, &packet);
            admission.charge(payload_bytes);
            self.senders[sender_index].admission = admission;
            self.admitted_bytes = self.admitted_bytes.saturating_add(payload_bytes as u64);
            self.senders[sender_index].latest_admitted_global =
                Some(media.packet.global_media_at());
            self.queued_payload_bytes = self.queued_payload_bytes.saturating_add(payload_bytes);
            self.queued_descriptor_bytes = self
                .queued_descriptor_bytes
                .saturating_add(descriptor_bytes);
            self.queued_transport_bytes = self
                .queued_transport_bytes
                .saturating_add(transport_estimate);
            self.queued_packets = self.queued_packets.saturating_add(1);
            self.senders[sender_index].queued_payload_bytes = self.senders[sender_index]
                .queued_payload_bytes
                .saturating_add(payload_bytes);
            self.senders[sender_index]
                .queued_frames
                .insert(media.frame.id);
            Ok((
                sender_index,
                descriptor_bytes,
                payload_bytes,
                transport_estimate,
            ))
        })();
        let (sender_index, descriptor_bytes, payload_bytes, transport_estimate) = match admission {
            Ok(admission) => admission,
            Err(reason) => return Err((reason, media)),
        };
        self.senders[sender_index].queue.push_back(QueuedMedia {
            queued_at: at.monotonic,
            media,
            descriptor_bytes,
            payload_bytes,
            transport_estimate,
        });
        if self.probe_remaining > 0
            && self.probe_started_at.is_some_and(|started| {
                let remaining = self
                    .probe_duration()
                    .saturating_sub(at.monotonic.saturating_duration_since(started));
                self.predicted_pacer_wait(at.monotonic) >= remaining
                    || self.envelope.is_some_and(|envelope| {
                        self.rtp_bytes_in_flight
                            .saturating_add(self.queued_transport_bytes as u64)
                            >= envelope.max_rtp_bytes_in_flight
                    })
            })
        {
            self.finish_probe(at.monotonic, true);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_flight_peak(&self) -> u64 {
        self.controller.test_flight_peak()
    }

    pub(crate) fn observe_bytes_in_flight(&mut self, bytes: u64) {
        self.controller.observe_bytes_in_flight(bytes);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_controller(
        &mut self,
        at: TimePoint,
        path_change: Option<(u64, bool)>,
        feedback: &[FeedbackSample],
        probe_feedback: &[PacketFeedback],
        feedback_hold: Duration,
        fresh_network_feedback: bool,
        bytes_in_flight: u64,
    ) -> bool {
        if let Some((epoch, available)) = path_change {
            self.active_path = available.then_some(epoch);
            self.feedback_recovery = FeedbackRecovery::Idle;
            self.path_available = available;
            self.pacer.reset();
            self.media_pacer.reset();
            self.probe_pacer.reset();
            self.probe_pacing_rate = 0;
            self.probe_remaining = 0;
            self.probe_sender = None;
            self.probe_target_bytes = 0;
            self.probe_packet_count = 0;
            self.probe_started_at = None;
            self.probe_sent_bytes = 0;
            self.probe_last_sent_at = None;
            self.probe_received_bytes = 0;
            self.probe_lost_packets = 0;
            self.probe_reported_packets = 0;
            self.probe_history.clear();
            self.probe_followup = false;
            self.last_probe = None;
            self.offer_window_started = at.monotonic;
            self.offered_bytes = 0;
            self.admitted_bytes = 0;
            self.offered_rate = 0;
            self.admitted_rate = 0;
        }
        let interval = at
            .monotonic
            .saturating_duration_since(self.offer_window_started);
        if interval >= Duration::from_millis(200) {
            let micros = interval.as_micros().max(1);
            self.offered_rate =
                u64::try_from(u128::from(self.offered_bytes).saturating_mul(8_000_000) / micros)
                    .unwrap_or(u64::MAX);
            self.admitted_rate =
                u64::try_from(u128::from(self.admitted_bytes).saturating_mul(8_000_000) / micros)
                    .unwrap_or(u64::MAX);
            self.offered_bytes = 0;
            self.admitted_bytes = 0;
            self.offer_window_started = at.monotonic;
        }
        let desired = self.potential_bitrate();
        let now = at
            .monotonic
            .saturating_duration_since(self.controller_origin);
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
                offered_media_rate: self.offered_rate,
                admitted_media_rate: self.admitted_rate,
                selected_media_rate: self.senders.iter().fold(0_u64, |total, sender| {
                    total.saturating_add(sender.selected_bitrate())
                }),
                has_unserved_source_interest: self
                    .senders
                    .iter()
                    .any(|sender| sender.potential_bitrate() > sender.selected_bitrate()),
                desired_media_rate: desired,
                window_or_pacer_blocked: (self.queued_transport_bytes > 0
                    || !self.repair_requests.is_empty())
                    && (!self.pacer.eligible(at.monotonic)
                        || (self.queued_packets != 0 && !self.media_pacer.eligible(at.monotonic))
                        || self.envelope.is_some_and(|envelope| {
                            bytes_in_flight >= envelope.max_rtp_bytes_in_flight
                        })),
                ecn: EcnValidation::default(),
            },
        );
        self.pacer
            .reprice(at.monotonic, output.pacing_transport_rate);
        if self.probe_pacing_rate > 0 && self.has_established_media_service() {
            self.probe_pacing_rate = self.probe_pacing_rate.min(output.pacing_transport_rate);
            self.probe_pacer
                .reprice(at.monotonic, self.probe_pacing_rate);
        }
        if fresh_network_feedback
            && !output.feedback_stale
            && self.feedback_recovery == FeedbackRecovery::AwaitingReport
        {
            self.feedback_recovery = FeedbackRecovery::Followup;
        }
        self.rtp_bytes_in_flight = bytes_in_flight;
        if self.probe_started_at.is_some_and(|started| {
            at.monotonic.saturating_duration_since(started) >= self.probe_duration()
        }) {
            // libwebrtc accepts feedback covering 80% of the requested probe.
            // Timer quantization must not discard an otherwise useful flight sample.
            let incomplete = self.probe_packet_count < 5
                || self.probe_sent_bytes.saturating_mul(100)
                    < self.probe_target_bytes.saturating_mul(80);
            self.finish_probe(at.monotonic, incomplete);
        }
        self.observe_probe_feedback(at.monotonic, probe_feedback, output.reason);
        if !self.discovery_followup_permitted(at.monotonic) {
            self.probe_followup = false;
        }
        if self.probe_remaining > 0
            && self.probe_started_at.is_some()
            && (output.feedback_stale
                || output.queue_delay > output.native_queue_delay_target
                || output.queue_delay
                    > self
                        .probe_start_queue_delay
                        .saturating_add(self.probe_queue_impact_limit())
                || matches!(output.reason, crate::congestion::ControllerReason::Loss))
        {
            self.finish_probe(at.monotonic, true);
        }
        self.envelope = Some(output);
        self.media_pacer
            .reprice(at.monotonic, output.target_media_payload_rate);
        self.update_media_budget(output.target_media_payload_rate);
        output.application_limited
    }

    pub(crate) fn request_keyframe(&mut self, request: crate::rtcp::KeyframeRequest) {
        let Some(sender) = self.senders.iter_mut().find(|sender| {
            sender.ssrc == request.media_ssrc && sender.facts.kind == MediaKind::Video
        }) else {
            return;
        };
        if let Some((requester, sequence)) = request.fir {
            if let Some((_, previous)) = sender
                .fir_sequences
                .iter_mut()
                .find(|(ssrc, _)| *ssrc == requester)
            {
                let distance = sequence.wrapping_sub(*previous);
                if distance == 0 || distance >= 128 {
                    return;
                }
                *previous = sequence;
            } else {
                // Preserve duplicate suppression for known requesters rather than evicting
                // their state when an authenticated peer supplies unbounded identities.
                if sender.fir_sequences.len() >= 256 {
                    return;
                }
                sender.fir_sequences.push((requester, sequence));
            }
        }
        sender
            .keyframe_requested
            .get_or_insert(KeyframeRequestReason::Peer);
    }

    pub(crate) fn request_repair(&mut self, media_ssrc: u32, sequence: u16) {
        let Some(sender) = self
            .senders
            .iter()
            .position(|sender| sender.ssrc == media_ssrc)
        else {
            return;
        };
        if self.repair_requests.len() < MAX_QUEUED_MEDIA_PACKETS
            && !self.repair_requests.contains(&(sender, sequence))
            && self
                .retained
                .iter()
                .any(|packet| packet.sender == sender && packet.sequence == sequence)
        {
            self.repair_requests.push_back((sender, sequence));
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        if let Some(allocation) = self.budget_changed.take() {
            return Some(Event::MediaBudgetChanged(allocation));
        }
        let sender = self
            .senders
            .iter_mut()
            .find(|sender| sender.keyframe_requested.is_some())?;
        let reason = sender.keyframe_requested.take()?;
        Some(Event::KeyframeRequested {
            sender: sender.facts.id,
            reason,
        })
    }

    pub(crate) fn abort(&mut self) {
        self.begin_shutdown();
        self.budget_changed = None;
    }

    pub(crate) fn begin_shutdown(&mut self) {
        for sender in &mut self.senders {
            sender.queue.clear();
            sender.queued_frames.clear();
            sender.queued_payload_bytes = 0;
            sender.expiring_frame = None;
        }
        self.queued_packets = 0;
        self.queued_payload_bytes = 0;
        self.queued_descriptor_bytes = 0;
        self.queued_transport_bytes = 0;
        self.retained.clear();
        self.retained_bytes = 0;
        self.repair_requests.clear();
        self.pending = None;
        self.probe_followup = false;
        self.probe_pacer.reset();
        self.probe_pacing_rate = 0;
        self.probe_remaining = 0;
        self.probe_sender = None;
        self.probe_target_bytes = 0;
        self.probe_packet_count = 0;
        self.probe_started_at = None;
        self.probe_sent_bytes = 0;
        self.probe_last_sent_at = None;
        self.probe_received_bytes = 0;
        self.probe_lost_packets = 0;
        self.probe_reported_packets = 0;
    }

    pub(crate) fn confirm_playout(&mut self, ssrc: u32, highest_sequence: u32) {
        let Some(sender) = self.senders.iter_mut().find(|sender| sender.ssrc == ssrc) else {
            return;
        };
        let Some(first) = sender.playout_first_sequence else {
            return;
        };
        let last = sender.next_sequence.saturating_sub(1);
        let mut report = (last & !u64::from(u32::MAX)) | u64::from(highest_sequence);
        if report > last {
            let Some(previous_cycle) = report.checked_sub(1_u64 << 32) else {
                return;
            };
            report = previous_cycle;
        }
        if report >= first && report <= last {
            sender.playout_confirmed = true;
        }
    }

    pub(crate) fn stats(&self) -> (EgressStats, Vec<SenderStats>) {
        let senders = self
            .senders
            .iter()
            .map(|sender| SenderStats {
                sender: sender.facts.id,
                assignment: sender.assignment,
                playout_delay: sender.playout_status(),
                queued_packets: sender.queue.len(),
                queued_payload_bytes: sender.queued_payload_bytes,
                transmitted_packets: sender.committed_packets,
                transmitted_payload_bytes: sender.committed_payload_bytes,
            })
            .collect();
        (
            EgressStats {
                queued_transport_bytes: self.queued_transport_bytes,
                media_budget: self.media_budget,
                pacing_bitrate: self
                    .envelope
                    .map_or(0, |envelope| envelope.pacing_transport_rate),
            },
            senders,
        )
    }

    pub(crate) fn prepare_one(
        &mut self,
        at: TimePoint,
        bytes_in_flight: u64,
        transport: &mut Transport,
        allow_padding: bool,
    ) -> PrepareResult {
        if self.pending.is_some() {
            return PrepareResult::Fatal;
        }
        self.expire(at, MAX_EXPIRATIONS_PER_POLL);
        self.expire_repair(at.monotonic);
        if !transport.can_send_user_data() {
            return PrepareResult::Blocked;
        }
        if at.monotonic >= self.next_rtcp
            && let Some(result) = self.prepare_rtcp(at, transport)
        {
            return result;
        }
        self.last_send_attempt = Some(at.monotonic);
        if !self.pacer.eligible(at.monotonic) {
            return if allow_padding
                && self.probe_remaining > 0
                && self.probe_pacing_rate > 0
                && self.queued_packets == 0
                && self.repair_requests.is_empty()
            {
                self.prepare_padding(at, bytes_in_flight, transport)
            } else {
                PrepareResult::Blocked
            };
        }
        if let Some(envelope) = self.envelope
            && bytes_in_flight >= envelope.max_rtp_bytes_in_flight
        {
            return PrepareResult::Blocked;
        }
        if let Some(result) = self.prepare_repair(at, bytes_in_flight, transport) {
            return result;
        }
        if self.queued_packets != 0
            && (self.media_budget == 0 || !self.media_pacer.eligible(at.monotonic))
        {
            return PrepareResult::Blocked;
        }

        self.fill_eligible(at);
        let Some(sender_index) = self.scheduler.select(&self.eligible) else {
            return if allow_padding {
                self.prepare_padding(at, bytes_in_flight, transport)
            } else {
                PrepareResult::Blocked
            };
        };
        let Some(sender) = self.senders.get_mut(sender_index) else {
            return PrepareResult::Fatal;
        };
        let Some(queued) = sender.queue.front() else {
            return PrepareResult::Fatal;
        };
        let descriptor = if let Some(continuity) = &mut sender.descriptor {
            match continuity.prepare(
                &queued.media.frame,
                sender
                    .assignment
                    .and_then(|assignment| assignment.decode_target_mask),
            ) {
                Ok(prepared) => Some(prepared),
                Err(_) => {
                    let frame = queued.media.frame.id;
                    self.abandon_frame(sender_index, frame);
                    self.pop_head(sender_index);
                    return PrepareResult::Blocked;
                }
            }
        } else {
            None
        };
        let Ok(source) = RtpPacket::parse(queued.media.packet.bytes()) else {
            return PrepareResult::Fatal;
        };
        let global = queued.media.packet.global_media_at();
        let Some(timestamp) = mapped_timestamp(sender, global) else {
            return PrepareResult::Fatal;
        };
        let sequence = wire_u16(sender.next_sequence);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let mut extensions = forwarded_extensions(sender, &queued.media.packet);
        if let Some(id) = sender.facts.dependency_extension_id
            && let Some(bytes) = descriptor
                .as_ref()
                .and_then(|prepared| prepared.bytes.as_ref())
        {
            extensions.push((id, bytes.clone()));
        }
        let Some(packet) = build_rtp(
            sender,
            source.marker() || sender.audio_talkspurt_pending,
            sequence,
            timestamp,
            twcc,
            source.payload(),
            sender.facts.payload_type,
            sender.ssrc,
            false,
            &extensions,
        ) else {
            return PrepareResult::Fatal;
        };
        if !self.fits_send_window(bytes_in_flight, packet.len()) {
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_service(&packet, RtpService::Original) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Original {
                    sender: sender_index,
                    sequence,
                    timestamp,
                    twcc,
                    packet,
                    descriptor,
                });
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => PrepareResult::Blocked,
            Err(_) => PrepareResult::Fatal,
        }
    }

    pub(crate) fn preflight_commit(&self, prepared: &PreparedTransmit) -> bool {
        if prepared.kind == DatagramKind::Rtp
            && self.envelope.is_some_and(|envelope| {
                self.rtp_bytes_in_flight
                    .saturating_add(prepared.wire_len as u64)
                    > envelope.max_rtp_bytes_in_flight
            })
        {
            return false;
        }
        match &self.pending {
            Some(PendingTransmit::Original {
                sender,
                sequence,
                twcc,
                ..
            }) => {
                let state = &self.senders[*sender];
                prepared.rtp.is_some_and(|identity| {
                    identity
                        == PreparedRtpIdentity {
                            ssrc: state.ssrc,
                            sequence: *sequence,
                            twcc_sequence: *twcc,
                            service: RtpService::Original,
                        }
                })
            }
            Some(PendingTransmit::Repair {
                sender,
                sequence,
                twcc,
                ..
            }) => {
                let state = &self.senders[*sender];
                prepared.rtp.is_some_and(|identity| {
                    identity
                        == PreparedRtpIdentity {
                            ssrc: state.rtx_ssrc,
                            sequence: *sequence,
                            twcc_sequence: *twcc,
                            service: RtpService::Repair,
                        }
                })
            }
            Some(PendingTransmit::Control) => {
                prepared.kind == DatagramKind::Rtcp && prepared.rtp.is_none()
            }
            Some(PendingTransmit::Padding {
                ssrc,
                sequence,
                twcc,
                ..
            }) => prepared.rtp.is_some_and(|identity| {
                identity
                    == PreparedRtpIdentity {
                        ssrc: *ssrc,
                        sequence: *sequence,
                        twcc_sequence: *twcc,
                        service: RtpService::Padding,
                    }
            }),
            None => prepared.rtp.is_none(),
        }
    }

    pub(crate) fn commit(&mut self, at: TimePoint, prepared: &PreparedTransmit) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if !matches!(pending, PendingTransmit::Control) {
            self.pacer.commit(
                at.monotonic,
                prepared.wire_len,
                self.envelope
                    .map_or(50_000, |value| value.pacing_transport_rate),
            );
            self.controller.note_send(
                at.monotonic
                    .saturating_duration_since(self.controller_origin),
            );
        }
        if !matches!(pending, PendingTransmit::Control) {
            self.record_rtp(
                at.monotonic,
                prepared.wire_len as u64,
                matches!(pending, PendingTransmit::Padding { .. }),
            );
        }
        match pending {
            PendingTransmit::Original {
                sender,
                sequence,
                timestamp,
                twcc,
                packet,
                descriptor,
            } => {
                let Some(queued) = self.pop_head(sender) else {
                    return;
                };
                self.media_pacer
                    .commit(at.monotonic, queued.payload_bytes, self.media_budget);
                let state = &mut self.senders[sender];
                if let Some(prepared) = descriptor
                    && let Some(continuity) = &mut state.descriptor
                {
                    continuity.commit(prepared);
                }
                state
                    .base_global
                    .get_or_insert_with(|| queued.media.packet.global_media_at());
                state.latest_committed_global = Some(queued.media.packet.global_media_at());
                if queued.media.frame.random_access
                    || matches!(&queued.media.frame.dependencies, FrameDependencies::Known(value) if value.is_empty())
                {
                    state.dependency_chain_valid = true;
                }
                state.audio_talkspurt_pending = false;
                state.note_playout_commit();
                state.next_sequence = state.next_sequence.saturating_add(1);
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                state.committed_packets = state.committed_packets.saturating_add(1);
                state.committed_payload_bytes = state
                    .committed_payload_bytes
                    .saturating_add(queued.payload_bytes as u64);
                self.retain_original(
                    sender,
                    sequence,
                    timestamp,
                    &packet,
                    queued.media.packet.global_media_at(),
                    at.monotonic,
                    prepared.wire_len,
                );
            }
            PendingTransmit::Repair {
                sender,
                original_sequence,
                twcc,
                ..
            } => {
                self.senders[sender].next_rtx_sequence =
                    self.senders[sender].next_rtx_sequence.saturating_add(1);
                if let Some(retained) = self
                    .retained
                    .iter_mut()
                    .find(|packet| packet.sender == sender && packet.sequence == original_sequence)
                {
                    retained.last_repair_at = Some(at.monotonic);
                }
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                if let Some(index) = self
                    .repair_requests
                    .iter()
                    .position(|request| *request == (sender, original_sequence))
                {
                    self.repair_requests.remove(index);
                }
            }
            PendingTransmit::Control => {
                self.next_rtcp = at
                    .monotonic
                    .checked_add(Duration::from_secs(1))
                    .unwrap_or(at.monotonic);
            }
            PendingTransmit::Padding {
                sender, ssrc, twcc, ..
            } => {
                if self.probe_pacing_rate > 0 {
                    self.probe_pacer.commit_probe_cluster(
                        at.monotonic,
                        prepared.wire_len,
                        self.probe_pacing_rate,
                    );
                }
                let state = &mut self.senders[sender];
                if ssrc == state.rtx_ssrc {
                    state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
                } else {
                    state.note_playout_commit();
                    state.next_sequence = state.next_sequence.saturating_add(1);
                }
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                if self.probe_started_at.is_none() {
                    self.probe_started_at = Some(at.monotonic);
                    self.probe_start_queue_delay = self
                        .envelope
                        .map_or(Duration::ZERO, |envelope| envelope.queue_delay);
                }
                self.probe_sent_bytes = self
                    .probe_sent_bytes
                    .saturating_add(prepared.wire_len as u64);
                self.probe_last_sent_at = Some(at.monotonic);
                self.probe_packet_count = self.probe_packet_count.saturating_add(1);
                self.feedback_recovery = match self.feedback_recovery {
                    FeedbackRecovery::Soliciting => FeedbackRecovery::AwaitingReport,
                    FeedbackRecovery::Followup => FeedbackRecovery::Idle,
                    state => state,
                };
                self.probe_remaining = self.probe_remaining.saturating_sub(1);
                if self.probe_remaining == 0
                    || (self.probe_packet_count >= 5
                        && self.probe_sent_bytes >= self.probe_target_bytes)
                {
                    self.finish_probe(at.monotonic, false);
                }
            }
        }
    }

    pub(crate) fn next_deadline(&self, at: TimePoint) -> Option<Instant> {
        let queue_idle = self.queued_packets == 0 && self.repair_requests.is_empty();
        let probe_deadline = self.envelope.and_then(|envelope| {
            (queue_idle
                && self.probe_remaining == 0
                && (envelope.probe_permitted || self.recovery_permitted(envelope))
                && self.probe_candidate().is_some())
            .then(|| {
                if (self.feedback_recovery == FeedbackRecovery::Followup
                    && !envelope.feedback_stale
                    && self.recovery_permitted(envelope))
                    || (envelope.probe_permitted
                        && self.discovery_followup_permitted(
                            self.last_probe.unwrap_or(self.controller_origin),
                        ))
                {
                    return self.controller_origin;
                }
                self.last_probe.map_or(self.controller_origin, |last| {
                    let retry = last.checked_add(Duration::from_secs(1)).unwrap_or(last);
                    last.checked_add(self.probe_interval(retry, envelope))
                        .unwrap_or(last)
                })
            })
            .map(|deadline| {
                self.pacer
                    .next_deadline()
                    .map_or(deadline, |paced| deadline.max(paced))
            })
        });
        [
            self.queue_deadline(at),
            (self.queued_packets != 0 || !self.repair_requests.is_empty())
                .then(|| {
                    let transport = self.pacer.next_deadline();
                    if !self.repair_requests.is_empty() {
                        return transport;
                    }
                    transport
                        .into_iter()
                        .chain(self.media_pacer.next_deadline())
                        .max()
                })
                .flatten()
                .filter(|deadline| self.send_deadline_unattempted(*deadline)),
            (queue_idle && self.probe_remaining > 0 && self.active_probe_candidate().is_some())
                .then(|| {
                    if self.probe_pacing_rate > 0 {
                        self.probe_pacer.next_deadline()
                    } else {
                        self.pacer.next_deadline()
                    }
                })
                .flatten()
                .filter(|deadline| self.send_deadline_unattempted(*deadline)),
            self.senders
                .iter()
                .any(|sender| sender.committed_packets > 0)
                .then_some(self.next_rtcp),
            self.controller
                .next_deadline()
                .and_then(|deadline| self.controller_origin.checked_add(deadline)),
            probe_deadline.filter(|deadline| self.send_deadline_unattempted(*deadline)),
            self.probe_started_at
                .and_then(|started| started.checked_add(self.probe_duration())),
            self.probe_history
                .iter()
                .filter(|cluster| cluster.result == ProbeResult::Pending)
                .map(|cluster| cluster.deadline)
                .min(),
        ]
        .into_iter()
        .flatten()
        .min()
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
    pub(crate) fn native_queue_delay_target(&self) -> Duration {
        self.envelope.map_or(Duration::ZERO, |envelope| {
            envelope.native_queue_delay_target
        })
    }

    #[cfg(test)]
    pub(crate) fn test_envelope(&self) -> Option<SafeRtpEnvelope> {
        self.envelope
    }

    #[cfg(test)]
    pub(crate) fn test_initial_probe_started_at(&self) -> Option<Instant> {
        self.initial_probe_started_at
    }

    #[cfg(test)]
    pub(crate) fn test_prepare_padding(
        &mut self,
        at: TimePoint,
        bytes_in_flight: u64,
        transport: &mut Transport,
    ) -> PrepareResult {
        self.prepare_padding(at, bytes_in_flight, transport)
    }

    pub(crate) const fn controller_origin(&self) -> Instant {
        self.controller_origin
    }

    fn sender_index(&self, sender: SenderId) -> Result<usize, CommandError> {
        self.senders
            .iter()
            .position(|candidate| candidate.facts.id == sender)
            .ok_or(CommandError::UnknownSender(sender))
    }

    fn predicted_pacer_wait(&self, now: Instant) -> Duration {
        let current = self
            .pacer
            .next_deadline()
            .map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(now)
            });
        let rate = self
            .envelope
            .map_or(50_000, |envelope| envelope.pacing_transport_rate)
            .max(1);
        let micros = (self.queued_transport_bytes as u128)
            .saturating_mul(8_000_000)
            .div_ceil(u128::from(rate));
        current.saturating_add(Duration::from_micros(
            u64::try_from(micros).unwrap_or(u64::MAX),
        ))
    }

    fn validate_frame(
        &self,
        at: TimePoint,
        sender_index: usize,
        media: &ForwardedMedia,
        packet: &RtpPacket<'_>,
    ) -> Result<(), CommandError> {
        let sender = &self.senders[sender_index];
        if matches!(
            &media.frame.dependencies,
            FrameDependencies::Known(dependencies)
                if dependencies.len() > FrameDependencies::MAX_DIRECT_DEPENDENCIES
                    || dependencies.contains(&media.frame.id)
        ) || sender
            .latest_admitted_global
            .is_some_and(|latest| media.packet.global_media_at() < latest)
        {
            return Err(CommandError::InvalidFrameMetadata);
        }
        if matches!(
            media.frame.boundary,
            FrameBoundary::Complete | FrameBoundary::Start
        ) && sender.queued_frames.contains(&media.frame.id)
        {
            return Err(CommandError::InvalidFrameMetadata);
        }
        let metadata_matches = |open: &OpenFrame| {
            open.id == media.frame.id
                && open.global == media.packet.global_media_at()
                && open.random_access == media.frame.random_access
                && open.discardable == media.frame.discardable
                && open.dependencies == media.frame.dependencies
        };
        match (media.frame.boundary, sender.open_frame.as_ref()) {
            (FrameBoundary::Complete | FrameBoundary::Start, None) => {}
            (FrameBoundary::Middle | FrameBoundary::End, None) => {
                return Err(CommandError::FrameInterrupted);
            }
            (FrameBoundary::Complete | FrameBoundary::Start, Some(open))
                if open.id != media.frame.id =>
            {
                return Err(CommandError::FrameInterrupted);
            }
            (FrameBoundary::Middle | FrameBoundary::End, Some(open)) if metadata_matches(open) => {
                if packet.ssrc() != open.source_ssrc || packet.timestamp() != open.source_timestamp
                {
                    return Err(CommandError::InvalidFrameMetadata);
                }
                if packet.sequence() != open.source_sequence.wrapping_add(1) {
                    return Err(CommandError::FrameInterrupted);
                }
            }
            _ => return Err(CommandError::InvalidFrameMetadata),
        }
        if !sender.dependency_chain_valid
            && !media.frame.random_access
            && !matches!(&media.frame.dependencies, FrameDependencies::Known(value) if value.is_empty())
        {
            let recovery_pending = sender.queue.iter().any(|queued| {
                (queued.media.frame.random_access
                        || matches!(&queued.media.frame.dependencies, FrameDependencies::Known(ids) if ids.is_empty()))
                    && !self.queued_expired(sender_index, queued, at)
            });
            return Err(if recovery_pending {
                CommandError::WouldBlock
            } else {
                CommandError::DependencyChainInterrupted
            });
        }
        Ok(())
    }

    fn apply_frame_admission(
        &mut self,
        sender: usize,
        media: &ForwardedMedia,
        packet: &RtpPacket<'_>,
    ) {
        let state = &mut self.senders[sender];
        match media.frame.boundary {
            FrameBoundary::Start => {
                state.open_frame = Some(OpenFrame {
                    id: media.frame.id,
                    global: media.packet.global_media_at(),
                    random_access: media.frame.random_access,
                    discardable: media.frame.discardable,
                    dependencies: media.frame.dependencies.clone(),
                    source_ssrc: packet.ssrc(),
                    source_timestamp: packet.timestamp(),
                    source_sequence: packet.sequence(),
                });
            }
            FrameBoundary::End => state.open_frame = None,
            FrameBoundary::Middle => {
                if let Some(open) = &mut state.open_frame {
                    open.source_sequence = packet.sequence();
                }
            }
            FrameBoundary::Complete => {}
        }
    }

    fn useful_at(
        &self,
        sender: usize,
        global: GlobalMediaTime,
        at: TimePoint,
        pacer_wait: Duration,
    ) -> bool {
        self.senders[sender].assignment.is_some_and(|assignment| {
            let network = self.network_service_time();
            pacer_wait <= assignment.delivery.max_queue_residence
                && self
                    .latest_useful_at(sender, global)
                    .is_some_and(|deadline| {
                        at.global
                            .checked_add(pacer_wait.saturating_add(network))
                            .is_some_and(|arrival| arrival <= deadline)
                    })
        })
    }

    fn latest_useful_at(&self, sender: usize, global: GlobalMediaTime) -> Option<GlobalMediaTime> {
        let assignment = self.senders[sender].assignment?;
        Some(
            global
                .checked_add(assignment.delivery.max_media_age)
                .unwrap_or(GlobalMediaTime::from_micros(u64::MAX)),
        )
    }

    fn network_service_time(&self) -> Duration {
        self.envelope.map_or(Duration::ZERO, |envelope| {
            envelope
                .queue_delay
                .saturating_add(envelope.smoothed_rtt / 2)
        })
    }

    fn queue_deadline(&self, at: TimePoint) -> Option<Instant> {
        self.senders
            .iter()
            .enumerate()
            .filter_map(|(index, sender)| {
                let queued = sender.queue.front()?;
                if self.queued_expired(index, queued, at) {
                    return Some(at.monotonic);
                }
                let assignment = sender.assignment?;
                let residence = queued
                    .queued_at
                    .checked_add(assignment.delivery.max_queue_residence)
                    .and_then(|deadline| deadline.checked_add(Duration::from_nanos(1)));
                let latest = self.latest_useful_at(index, queued.media.packet.global_media_at())?;
                let arrival = at.global.checked_add(self.network_service_time())?;
                let remaining = latest
                    .as_micros()
                    .saturating_sub(arrival.as_micros())
                    .saturating_add(1);
                let media = at.monotonic.checked_add(Duration::from_micros(remaining));
                residence.into_iter().chain(media).min()
            })
            .min()
    }

    fn queued_expired(&self, sender: usize, queued: &QueuedMedia, at: TimePoint) -> bool {
        let state = &self.senders[sender];
        state.expiring_frame == Some(queued.media.frame.id)
            || state.assignment.is_none_or(|assignment| {
                at.monotonic.saturating_duration_since(queued.queued_at)
                    > assignment.delivery.max_queue_residence
            })
            || !self.useful_at(
                sender,
                queued.media.packet.global_media_at(),
                at,
                Duration::ZERO,
            )
            || (!state.dependency_chain_valid
                && !queued.media.frame.random_access
                && !matches!(&queued.media.frame.dependencies, FrameDependencies::Known(ids) if ids.is_empty()))
    }

    fn fill_eligible(&mut self, at: TimePoint) {
        for index in 0..self.senders.len() {
            self.eligible[index] = self.senders[index]
                .queue
                .front()
                .filter(|queued| !self.queued_expired(index, queued, at))
                .map(|queued| queued.payload_bytes);
        }
    }

    fn abandon_frame(&mut self, sender: usize, frame: FrameId) {
        let state = &mut self.senders[sender];
        if state.expiring_frame == Some(frame) {
            return;
        }
        state.expiring_frame = Some(frame);
        state.dependency_chain_valid = false;
        if state.facts.kind == MediaKind::Video {
            state.keyframe_requested = Some(KeyframeRequestReason::LocalRecovery);
        }
        if state
            .open_frame
            .as_ref()
            .is_some_and(|open| open.id == frame)
        {
            state.open_frame = None;
        }
    }

    fn expire(&mut self, at: TimePoint, limit: usize) {
        let mut work = 0;
        let mut senders = 0;
        while work < limit && senders < self.senders.len() {
            let index = self.expiry_cursor;
            self.expiry_cursor = (index + 1) % self.senders.len();
            senders += 1;
            while work < limit {
                let Some(frame) = self.senders[index]
                    .queue
                    .front()
                    .filter(|queued| self.queued_expired(index, queued, at))
                    .map(|queued| queued.media.frame.id)
                else {
                    break;
                };
                self.abandon_frame(index, frame);
                self.pop_head(index);
                work += 1;
            }
        }
    }

    fn pop_head(&mut self, sender: usize) -> Option<QueuedMedia> {
        let state = &mut self.senders[sender];
        let queued = state.queue.pop_front()?;
        state.queued_payload_bytes = state
            .queued_payload_bytes
            .saturating_sub(queued.payload_bytes);
        if state
            .queue
            .front()
            .is_none_or(|next| next.media.frame.id != queued.media.frame.id)
        {
            state.queued_frames.remove(&queued.media.frame.id);
            state.expiring_frame = None;
        }
        self.queued_packets = self.queued_packets.saturating_sub(1);
        self.queued_descriptor_bytes = self
            .queued_descriptor_bytes
            .saturating_sub(queued.descriptor_bytes);
        self.queued_payload_bytes = self
            .queued_payload_bytes
            .saturating_sub(queued.payload_bytes);
        self.queued_transport_bytes = self
            .queued_transport_bytes
            .saturating_sub(queued.transport_estimate);
        Some(queued)
    }

    fn potential_bitrate(&self) -> u64 {
        self.senders.iter().fold(0_u64, |total, sender| {
            total.saturating_add(sender.potential_bitrate())
        })
    }

    fn update_media_budget(&mut self, budget: u64) {
        if budget != self.media_budget {
            self.media_budget = budget;
            self.budget_changed = Some(MediaPayloadBitrate::from_bps(budget));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn retain_original(
        &mut self,
        sender: usize,
        sequence: u16,
        timestamp: u32,
        packet: &[u8],
        global: GlobalMediaTime,
        at: Instant,
        wire_len: usize,
    ) {
        if self.senders[sender]
            .facts
            .retransmission_payload_type
            .is_none()
            || wire_len > self.max_retransmission_bytes
        {
            return;
        }
        while wire_len
            > self
                .max_retransmission_bytes
                .saturating_sub(self.retained_bytes)
        {
            let Some(oldest) = self.retained.pop_front() else {
                return;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(oldest.transport_bytes);
            self.repair_requests
                .retain(|request| *request != (oldest.sender, oldest.sequence));
        }
        if RtpPacket::parse(packet).is_err() {
            return;
        }
        self.retained.push_back(RetainedPacket {
            sender,
            sequence,
            timestamp,
            packet: packet.to_vec(),
            retained_at: at,
            last_repair_at: None,
            global,
            transport_bytes: wire_len,
        });
        self.retained_bytes = self.retained_bytes.saturating_add(wire_len);
    }

    fn expire_repair(&mut self, now: Instant) {
        let senders = &self.senders;
        let requests = &mut self.repair_requests;
        let mut removed_bytes = 0usize;
        self.retained.retain(|packet| {
            let keep = senders[packet.sender].assignment.is_some_and(|assignment| {
                now.saturating_duration_since(packet.retained_at)
                    < assignment.delivery.repair_window
            });
            if !keep {
                removed_bytes = removed_bytes.saturating_add(packet.transport_bytes);
                requests.retain(|request| *request != (packet.sender, packet.sequence));
            }
            keep
        });
        self.retained_bytes = self.retained_bytes.saturating_sub(removed_bytes);
    }

    fn repair_due(&self, retained: &RetainedPacket, now: Instant) -> bool {
        let rtt = self
            .envelope
            .map_or(Duration::from_millis(50), |envelope| envelope.smoothed_rtt);
        retained
            .last_repair_at
            .is_none_or(|last| now.saturating_duration_since(last) >= rtt)
    }

    fn repair_useful_at(&self, retained: &RetainedPacket, at: TimePoint) -> bool {
        self.senders[retained.sender]
            .assignment
            .is_some_and(|assignment| {
                at.monotonic.saturating_duration_since(retained.retained_at)
                    < assignment.delivery.repair_window
                    && self.useful_at(retained.sender, retained.global, at, Duration::ZERO)
            })
    }

    fn prepare_repair(
        &mut self,
        at: TimePoint,
        bytes_in_flight: u64,
        transport: &mut Transport,
    ) -> Option<PrepareResult> {
        let (sender_index, original_sequence) = *self.repair_requests.front()?;
        let Some(retained) = self
            .retained
            .iter()
            .find(|packet| packet.sender == sender_index && packet.sequence == original_sequence)
        else {
            self.repair_requests.pop_front();
            return None;
        };
        if !self.repair_due(retained, at.monotonic) || !self.repair_useful_at(retained, at) {
            self.repair_requests.pop_front();
            return Some(PrepareResult::Blocked);
        }
        let sender = &self.senders[sender_index];
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
        let result = match transport.send_rtp_with_service(&packet, RtpService::Repair) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Repair {
                    sender: sender_index,
                    original_sequence,
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

    fn prepare_rtcp(&mut self, at: TimePoint, transport: &mut Transport) -> Option<PrepareResult> {
        let sender_index = (0..self.senders.len())
            .map(|offset| (self.rtcp_cursor + offset) % self.senders.len())
            .find(|index| self.senders[*index].committed_packets > 0)?;
        let sender = &self.senders[sender_index];
        let timestamp = sender
            .latest_committed_global
            .and_then(|global| mapped_timestamp(sender, global))
            .unwrap_or(sender.base_timestamp);
        let packet = sender_report(sender, timestamp, at.global);
        match transport.send_rtcp(&packet) {
            Ok(()) => {
                self.rtcp_cursor = (sender_index + 1) % self.senders.len();
                self.pending = Some(PendingTransmit::Control);
                Some(PrepareResult::Prepared)
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => {
                Some(PrepareResult::Blocked)
            }
            Err(_) => Some(PrepareResult::Fatal),
        }
    }

    fn finish_probe(&mut self, at: Instant, aborted: bool) {
        if aborted {
            self.feedback_recovery = FeedbackRecovery::Idle;
            self.probe_followup = false;
        }
        self.probe_remaining = 0;
        self.probe_pacer.reset();
        self.probe_pacing_rate = 0;
        self.probe_sender = None;
        let target_bytes = std::mem::take(&mut self.probe_target_bytes);
        let packet_count = std::mem::take(&mut self.probe_packet_count);
        self.last_probe = Some(at);
        if let (Some(started_at), Some(last_sent_at)) =
            (self.probe_started_at.take(), self.probe_last_sent_at.take())
        {
            let timeout = self
                .envelope
                .map_or(Duration::from_millis(200), |envelope| {
                    envelope
                        .smoothed_rtt
                        .saturating_mul(2)
                        .clamp(Duration::from_millis(200), Duration::from_secs(2))
                });
            if self.probe_history.len() == 64 {
                self.probe_history.pop_front();
            }
            self.probe_history.push_back(ProbeCluster {
                started_at,
                last_sent_at,
                finished_at: at,
                deadline: at.checked_add(timeout).unwrap_or(at),
                sent_bytes: self.probe_sent_bytes,
                target_bytes,
                packet_count: u32::from(packet_count),
                received_bytes: self.probe_received_bytes,
                lost_packets: self.probe_lost_packets,
                reported_packets: self.probe_reported_packets,
                result: if aborted {
                    ProbeResult::Failed
                } else {
                    ProbeResult::Pending
                },
            });
        }
        self.probe_sent_bytes = 0;
        self.probe_received_bytes = 0;
        self.probe_lost_packets = 0;
        self.probe_reported_packets = 0;
    }

    fn observe_probe_feedback(
        &mut self,
        at: Instant,
        feedback: &[PacketFeedback],
        reason: crate::congestion::ControllerReason,
    ) {
        let congested = matches!(
            reason,
            crate::congestion::ControllerReason::Loss
                | crate::congestion::ControllerReason::Delay
                | crate::congestion::ControllerReason::Policer
                | crate::congestion::ControllerReason::ClassicEcn
                | crate::congestion::ControllerReason::L4s
        );
        for sample in feedback {
            if sample.received
                && !sample.lost
                && let Some(sender) = self
                    .senders
                    .iter_mut()
                    .find(|sender| sender.ssrc == sample.ssrc)
            {
                sender.primary_feedback_observed = true;
            }
            // Sent history emits each receipt transition once, including a
            // reordered receipt whose ACK-edge credit was already consumed.
            if sample.service != RtpService::Padding || (!sample.lost && !sample.received) {
                continue;
            }
            if let Some(cluster) = self.probe_history.iter_mut().rev().find(|cluster| {
                cluster.result == ProbeResult::Pending
                    && sample.committed_at >= cluster.started_at
                    && sample.committed_at <= cluster.last_sent_at
            }) {
                cluster.reported_packets = cluster.reported_packets.saturating_add(1);
                if sample.received {
                    cluster.received_bytes = cluster
                        .received_bytes
                        .saturating_add(u64::from(sample.transport_bytes));
                } else {
                    cluster.lost_packets = cluster.lost_packets.saturating_add(1);
                }
            } else if self.probe_started_at.is_some_and(|start| {
                sample.committed_at >= start
                    && self
                        .probe_last_sent_at
                        .is_some_and(|last| sample.committed_at <= last)
            }) {
                self.probe_reported_packets = self.probe_reported_packets.saturating_add(1);
                if sample.received {
                    self.probe_received_bytes = self
                        .probe_received_bytes
                        .saturating_add(u64::from(sample.transport_bytes));
                } else {
                    self.probe_lost_packets = self.probe_lost_packets.saturating_add(1);
                }
            }
        }
        for cluster in &mut self.probe_history {
            if cluster.result != ProbeResult::Pending {
                continue;
            }
            if congested || at >= cluster.deadline {
                cluster.result = ProbeResult::Failed;
                self.probe_followup = false;
            } else if cluster.sent_bytes > 0
                && cluster.received_bytes.saturating_mul(100)
                    >= cluster
                        .sent_bytes
                        .max(cluster.target_bytes)
                        .saturating_mul(80)
                && cluster
                    .reported_packets
                    .saturating_sub(cluster.lost_packets)
                    .saturating_mul(100)
                    >= cluster.packet_count.min(5).saturating_mul(80)
            {
                cluster.result = ProbeResult::Successful;
                if cluster.sent_bytes.saturating_mul(100) >= PROBE_MIN_TARGET.saturating_mul(80)
                    && self
                        .last_probe
                        .is_some_and(|last| last == cluster.finished_at)
                {
                    self.probe_followup = true;
                }
            }
        }
        if congested {
            self.probe_followup = false;
        }
    }

    fn initial_probing(&self, at: Instant) -> bool {
        self.initial_probe_started_at
            .is_some_and(|started| at.saturating_duration_since(started) < PROBE_WINDOW)
            && self.potential_bitrate() > 0
            && self
                .senders
                .iter()
                .all(|sender| sender.selected_bitrate() == 0)
    }

    fn discovery_followup_permitted(&self, at: Instant) -> bool {
        self.probe_followup
            && self.probe_candidate().is_some_and(|sender| {
                self.initial_probing(at)
                    || (self.senders[sender].selected_bitrate() == 0
                        && self.senders[sender].latest_committed_global.is_none())
            })
    }

    fn probe_allowance(&self, at: Instant) -> u64 {
        if self.initial_probing(at) {
            self.initial_probe_allowance
        } else {
            PROBE_ALLOWANCE
        }
    }

    fn record_rtp(&mut self, at: Instant, bytes: u64, probe: bool) {
        for _ in 0..MAX_EXPIRATIONS_PER_POLL {
            if self
                .emitted_rtp
                .front()
                .is_none_or(|(sent, _, _)| at.saturating_duration_since(*sent) < PROBE_WINDOW)
            {
                break;
            }
            self.emitted_rtp.pop_front();
        }
        if self.emitted_rtp.len() >= MAX_PROBE_ACCOUNTING {
            let evicted_recent = self
                .emitted_rtp
                .pop_front()
                .is_some_and(|(sent, _, _)| at.saturating_duration_since(sent) < PROBE_WINDOW);
            if evicted_recent {
                self.probe_disabled_until = at.checked_add(PROBE_WINDOW);
            }
        }
        self.emitted_rtp.push_back((at, bytes, probe));
    }

    fn fits_send_window(&self, bytes_in_flight: u64, packet_len: usize) -> bool {
        self.envelope.is_some_and(|envelope| {
            bytes_in_flight.saturating_add(
                u64::try_from(packet_len.saturating_add(RTP_TRANSPORT_ALLOWANCE))
                    .unwrap_or(u64::MAX),
            ) <= envelope.max_rtp_bytes_in_flight
        })
    }

    fn probe_budget_available(&self, at: Instant, bytes: u64) -> bool {
        if self.probe_disabled_until.is_some_and(|until| at < until) {
            return false;
        }
        let mut nonprobe = 0_u64;
        let mut probes = bytes;
        for (sent, size, probe) in &self.emitted_rtp {
            if at.saturating_duration_since(*sent) < PROBE_WINDOW {
                if *probe {
                    probes = probes.saturating_add(*size);
                } else {
                    nonprobe = nonprobe.saturating_add(*size);
                }
            }
        }
        let allowance = self.probe_allowance(at);
        if probes > nonprobe / 19 + allowance {
            return false;
        }
        // A probe must remain affordable as older non-probe bytes age out,
        // until the probe itself leaves the five-second window.
        for (sent, size, probe) in &self.emitted_rtp {
            if at.saturating_duration_since(*sent) >= PROBE_WINDOW {
                continue;
            }
            if *probe {
                probes = probes.saturating_sub(*size);
            } else {
                nonprobe = nonprobe.saturating_sub(*size);
            }
            if probes > nonprobe / 19 + allowance {
                return false;
            }
        }
        true
    }

    fn probe_queue_impact_limit(&self) -> Duration {
        Duration::from_millis(10)
    }

    fn padding_allowed(&self, sender: usize) -> bool {
        let sender = &self.senders[sender];
        // Keep padding out of unfinished frames, including the primary-SSRC
        // fallback where interleaving would look like source-media loss.
        sender.open_frame.is_none() && sender.potential_bitrate() > 0
    }

    fn active_probe_candidate(&self) -> Option<usize> {
        self.probe_sender
            .filter(|sender| self.padding_allowed(*sender))
            .or_else(|| {
                self.probe_started_at
                    .is_none()
                    .then(|| self.probe_candidate())
                    .flatten()
            })
    }

    fn probe_candidate(&self) -> Option<usize> {
        self.senders
            .iter()
            .enumerate()
            .filter(|(index, _)| self.padding_allowed(*index))
            .max_by_key(|(_, sender)| {
                sender
                    .potential_bitrate()
                    .saturating_sub(sender.selected_bitrate())
            })
            .map(|(index, _)| index)
    }

    fn probe_budget_bitrate(&self) -> u64 {
        self.potential_bitrate()
    }

    fn probe_target_rate(&self, credible_media_rate: u64) -> u64 {
        self.probe_budget_bitrate()
            .min(credible_media_rate.saturating_mul(2).max(300_000))
    }

    fn cluster_target_bytes(&self, credible_media_rate: u64) -> u64 {
        (self.probe_target_rate(credible_media_rate) / 400).clamp(1, 48 * 1024)
    }

    fn has_established_media_service(&self) -> bool {
        self.senders
            .iter()
            .any(|sender| sender.latest_committed_global.is_some() && sender.selected_bitrate() > 0)
    }

    fn probe_duration(&self) -> Duration {
        if self.probe_pacing_rate == 0 {
            return Duration::from_millis(20);
        }
        let micros = u64::try_from(
            u128::from(self.probe_target_bytes)
                .saturating_mul(8_000_000)
                .div_ceil(u128::from(self.probe_pacing_rate)),
        )
        .unwrap_or(u64::MAX);
        Duration::from_micros(micros).clamp(Duration::from_millis(20), INITIAL_PROBE_DURATION)
    }

    fn probe_fits_selected_horizon(
        &self,
        now: Instant,
        transport_bytes: usize,
        pacing_rate: u64,
    ) -> bool {
        let service = Duration::from_micros(
            u64::try_from(
                (transport_bytes as u128)
                    .saturating_mul(8_000_000)
                    .div_ceil(u128::from(pacing_rate.max(1))),
            )
            .unwrap_or(u64::MAX),
        );
        let wait = self.predicted_pacer_wait(now).saturating_add(service);
        self.senders.iter().all(|sender| {
            sender.assignment.is_none_or(|assignment| {
                assignment.selected_bitrate.as_bps() == 0
                    || wait <= assignment.delivery.max_queue_residence
            })
        })
    }

    fn recovery_permitted(&self, envelope: SafeRtpEnvelope) -> bool {
        self.path_available
            && (envelope.feedback_stale
                || self.feedback_recovery == FeedbackRecovery::Followup
                || self.senders.iter().enumerate().any(|(index, sender)| {
                    self.padding_allowed(index)
                        && sender.assignment.is_some_and(|assignment| {
                            assignment.selected_bitrate.as_bps() > 0
                                && envelope
                                    .queue_delay
                                    .saturating_add(envelope.smoothed_rtt / 2)
                                    > assignment.delivery.max_media_age
                        })
                }))
            && self.potential_bitrate() > 0
            && self.queued_transport_bytes == 0
            && self.rtp_bytes_in_flight == 0
            && envelope.max_rtp_bytes_in_flight > 0
    }

    fn probe_interval(&self, at: Instant, envelope: SafeRtpEnvelope) -> Duration {
        let seconds = if self.initial_probe_started_at.is_none()
            || self.initial_probing(at)
            || self.recovery_permitted(envelope)
        {
            1
        } else {
            // libwebrtc separates repeated initial discovery from periodic ALR probes.
            5
        };
        Duration::from_secs(seconds).max(envelope.smoothed_rtt.saturating_mul(4))
    }

    fn start_probe(&mut self, at: TimePoint, envelope: SafeRtpEnvelope) -> bool {
        let interval = self.probe_interval(at.monotonic, envelope);
        let due = self
            .last_probe
            .is_none_or(|last| at.monotonic.saturating_duration_since(last) >= interval);
        let recovery = self.recovery_permitted(envelope);
        let followup = recovery
            && !envelope.feedback_stale
            && self.feedback_recovery == FeedbackRecovery::Followup;
        let discovery_followup =
            envelope.probe_permitted && self.discovery_followup_permitted(at.monotonic);
        if (!envelope.probe_permitted && !recovery) || (!due && !followup && !discovery_followup) {
            return false;
        }
        let Some(sender_index) = self.probe_candidate() else {
            return false;
        };
        self.probe_followup = false;
        let promote = followup && !envelope.feedback_stale && envelope.probe_permitted;
        if envelope.probe_permitted && (!recovery || promote) {
            self.initial_probe_started_at.get_or_insert(at.monotonic);
            if self.initial_probing(at.monotonic) {
                // libwebrtc budgets two initial clusters, capped at twice application demand.
                // Account initial service separately from the rate-bounded sample duration.
                let allowance = u64::try_from(
                    u128::from(self.potential_bitrate())
                        .saturating_mul(2)
                        .saturating_mul(INITIAL_PROBE_DURATION.as_micros())
                        .saturating_mul(u128::from(INITIAL_PROBE_CLUSTERS))
                        .div_ceil(8_000_000),
                )
                .unwrap_or(u64::MAX)
                .clamp(PROBE_ALLOWANCE, PROBE_ALLOWANCE * INITIAL_PROBE_CLUSTERS);
                self.initial_probe_allowance = self.initial_probe_allowance.max(allowance);
            }
        }
        self.probe_sender = Some(sender_index);
        self.probe_target_bytes = self.cluster_target_bytes(envelope.target_media_payload_rate);
        self.probe_remaining = if recovery && !promote {
            1
        } else {
            MAX_PROBE_PACKETS
        };
        self.probe_pacer.reset();
        self.probe_pacing_rate = if self.probe_remaining == 1 {
            0
        } else {
            let bootstrap_sample = if self.senders[sender_index].selected_bitrate() == 0
                && (self.senders[sender_index].latest_committed_global.is_none()
                    || self.initial_probing(at.monotonic))
            {
                u64::try_from(
                    u128::from(self.probe_budget_bitrate())
                        .saturating_mul(envelope.smoothed_rtt.as_micros())
                        .div_ceil(8_000_000),
                )
                .unwrap_or(u64::MAX)
                .min(envelope.max_rtp_bytes_in_flight)
            } else {
                0
            };
            let idle_sample = if !self.initial_probing(at.monotonic)
                && (envelope.application_limited
                    || self.senders[sender_index].selected_bitrate() == 0)
            {
                // A successful small cluster can still leave the native
                // observed-flight growth bound below its reference window.
                let headroom = if self.senders[sender_index].selected_bitrate() == 0
                    && self.senders[sender_index].latest_committed_global.is_none()
                    && !self.has_established_media_service()
                {
                    PROBE_MIN_TARGET
                } else {
                    0
                };
                envelope.reference_window.saturating_add(headroom)
            } else {
                0
            };
            let rate = self.probe_budget_bitrate().saturating_mul(2).min(
                envelope
                    .target_media_payload_rate
                    .saturating_mul(2)
                    .max(300_000),
            );
            let rate = if self.has_established_media_service() {
                rate.min(envelope.pacing_transport_rate)
            } else {
                rate
            };
            let service_bound = u64::try_from(
                u128::from(rate).saturating_mul(INITIAL_PROBE_DURATION.as_micros()) / 8_000_000,
            )
            .unwrap_or(u64::MAX);
            self.probe_target_bytes = self
                .probe_target_bytes
                .max(PROBE_MIN_TARGET)
                .max(bootstrap_sample)
                .max(idle_sample)
                .min(service_bound.max(PROBE_MIN_TARGET))
                .min(envelope.max_rtp_bytes_in_flight)
                .min(48 * 1024);
            rate
        };
        if promote {
            self.feedback_recovery = FeedbackRecovery::Idle;
        } else if recovery && !followup {
            self.feedback_recovery = FeedbackRecovery::Soliciting;
        }
        true
    }

    fn prepare_padding(
        &mut self,
        at: TimePoint,
        bytes_in_flight: u64,
        transport: &mut Transport,
    ) -> PrepareResult {
        if !self.path_available || self.queued_packets != 0 || !self.repair_requests.is_empty() {
            return PrepareResult::Blocked;
        }
        let Some(envelope) = self.envelope else {
            return PrepareResult::Blocked;
        };
        if self.probe_remaining == 0 && !self.start_probe(at, envelope) {
            return PrepareResult::Blocked;
        }
        if self.probe_pacing_rate > 0 && !self.probe_pacer.eligible(at.monotonic) {
            return PrepareResult::Blocked;
        }
        let Some(sender_index) = self.active_probe_candidate() else {
            return PrepareResult::Blocked;
        };
        self.probe_sender = Some(sender_index);
        let sender = &self.senders[sender_index];
        if sender.potential_bitrate() == 0 {
            self.finish_probe(at.monotonic, true);
            return PrepareResult::Blocked;
        }
        if !self.padding_allowed(sender_index) {
            return PrepareResult::Blocked;
        }
        let (sequence, payload_type, ssrc) = padding_source(sender);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let timestamp = sender
            .latest_committed_global
            .and_then(|global| mapped_timestamp(sender, global))
            .unwrap_or(sender.base_timestamp);
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
            return PrepareResult::Fatal;
        };
        packet.pop();
        packet.extend_from_slice(&probe_padding(
            matches!(
                self.feedback_recovery,
                FeedbackRecovery::Soliciting | FeedbackRecovery::Followup
            ),
            envelope.pacing_transport_rate.max(self.probe_pacing_rate),
            packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE),
        ));
        if !self.probe_fits_selected_horizon(
            at.monotonic,
            packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE),
            envelope.pacing_transport_rate,
        ) {
            self.finish_probe(at.monotonic, true);
            return PrepareResult::Blocked;
        }
        if packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE) as u64
            > (48 * 1024_u64).saturating_sub(self.probe_sent_bytes)
            || !self.fits_send_window(bytes_in_flight, packet.len())
        {
            self.finish_probe(at.monotonic, true);
            return PrepareResult::Blocked;
        }
        if !self.probe_budget_available(
            at.monotonic,
            packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE) as u64,
        ) {
            self.finish_probe(at.monotonic, true);
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_service(&packet, RtpService::Padding) {
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

fn padding_source(sender: &Sender) -> (u16, u8, u32) {
    match (
        sender.primary_feedback_observed,
        sender.facts.retransmission_payload_type,
    ) {
        (true, Some(payload_type)) => (
            wire_u16(sender.next_rtx_sequence),
            payload_type,
            sender.rtx_ssrc,
        ),
        _ => (
            wire_u16(sender.next_sequence),
            sender.facts.payload_type,
            sender.ssrc,
        ),
    }
}

fn probe_padding(recovery: bool, pacing_rate: u64, overhead: usize) -> Vec<u8> {
    // Limit probe serialization to 5 ms so padding does not monopolize the pacer.
    // The final RTP padding octet counts the entire padding region.
    let count = if recovery {
        1
    } else {
        u8::try_from(
            (pacing_rate / 1_600)
                .saturating_sub(overhead as u64)
                .clamp(1, 255),
        )
        .unwrap_or(u8::MAX)
    };
    let mut padding = vec![0; usize::from(count)];
    padding[usize::from(count) - 1] = count;
    padding
}

fn mapped_timestamp(sender: &Sender, global: GlobalMediaTime) -> Option<u32> {
    let base_global = sender.base_global.unwrap_or(global);
    if sender
        .latest_committed_global
        .is_some_and(|latest| global < latest)
    {
        return None;
    }
    let delta = global.as_micros().checked_sub(base_global.as_micros())?;
    let ticks = u128::from(delta)
        .saturating_mul(u128::from(sender.facts.clock_rate))
        .saturating_add(500_000)
        / 1_000_000;
    let offset = u32::try_from(ticks % (u128::from(u32::MAX) + 1)).ok()?;
    Some(sender.base_timestamp.wrapping_add(offset))
}

fn wire_u16(value: u64) -> u16 {
    u16::try_from(value % 65_536).unwrap_or_default()
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

fn forwarded_extensions(sender: &Sender, packet: &MediaPacket) -> Vec<(u8, Vec<u8>)> {
    sender
        .facts
        .forwarded_extensions
        .iter()
        .filter_map(|(id, uri)| {
            let value = packet.extension(uri)?;
            let valid = match &**uri {
                "urn:ietf:params:rtp-hdrext:ssrc-audio-level" | "urn:3gpp:video-orientation" => {
                    value.len() == 1
                }
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time" => {
                    matches!(value.len(), 8 | 16)
                }
                _ => value.len() <= usize::from(u8::MAX),
            };
            valid.then(|| (*id, value.to_vec()))
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
    let mut extensions = forwarded_extensions.to_vec();
    if let Some(id) = sender.facts.mid_extension_id {
        extensions.push((id, sender.facts.mid.as_bytes().to_vec()));
    }
    if let (Some(id), Some(sequence)) = (sender.facts.twcc_extension_id, twcc) {
        extensions.push((id, sequence.to_be_bytes().to_vec()));
    }
    if ssrc == sender.ssrc
        && let (Some(id), Some(delay)) = (
            sender.facts.playout_extension_id,
            sender.playout_to_signal(),
        )
    {
        extensions.push((id, delay.wire_value().to_vec()));
    }
    let mut packet = Vec::with_capacity(24usize.saturating_add(payload.len()));
    packet
        .push(0x80 | if extensions.is_empty() { 0 } else { 0x10 } | if padding { 0x20 } else { 0 });
    packet.push(payload_type | if marker { 0x80 } else { 0 });
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    if !extensions.is_empty()
        && !write_extensions(
            &mut packet,
            &extensions,
            sender_uses_one_byte_extensions(sender),
        )
    {
        return None;
    }
    packet.extend_from_slice(payload);
    Some(packet)
}

fn sender_uses_one_byte_extensions(sender: &Sender) -> bool {
    sender.facts.dependency_extension_id.is_none()
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

fn write_extensions(
    packet: &mut Vec<u8>,
    extensions: &[(u8, Vec<u8>)],
    allow_one_byte: bool,
) -> bool {
    let one_byte = allow_one_byte
        && extensions
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
    let Ok(words) = u16::try_from((packet.len() - start) / 4) else {
        return false;
    };
    let [high, low] = words.to_be_bytes();
    packet[length_offset] = high;
    packet[length_offset + 1] = low;
    true
}

fn sender_report(sender: &Sender, timestamp: u32, global: GlobalMediaTime) -> Vec<u8> {
    let micros = global.as_micros();
    let seconds = micros / 1_000_000;
    let fraction = (u128::from(micros % 1_000_000) << 32) / 1_000_000;
    let mut packet = vec![0x80, 200, 0, 6];
    packet.extend_from_slice(&sender.ssrc.to_be_bytes());
    packet.extend_from_slice(&u32::try_from(seconds).unwrap_or(u32::MAX).to_be_bytes());
    packet.extend_from_slice(&u32::try_from(fraction).unwrap_or(u32::MAX).to_be_bytes());
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

    let cname = format!("pulsebeam-{:08x}", sender.ssrc);
    let start = packet.len();
    packet.extend_from_slice(&[0x81, 202, 0, 0]);
    packet.extend_from_slice(&sender.ssrc.to_be_bytes());
    packet.push(1);
    packet.push(u8::try_from(cname.len()).unwrap_or(u8::MAX));
    packet.extend_from_slice(cname.as_bytes());
    packet.push(0);
    while !(packet.len() - start).is_multiple_of(4) {
        packet.push(0);
    }
    let words = u16::try_from((packet.len() - start) / 4 - 1).unwrap_or(u16::MAX);
    packet[start + 2..start + 4].copy_from_slice(&words.to_be_bytes());
    packet
}

#[cfg(test)]
#[allow(
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "crate-private tests construct reviewed immutable public packet values"
)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::*;
    use crate::{FrameMetadata, MediaPacket, PlayoutDelay};

    fn current_assignment(owner: &MediaEgress, sender: SenderId) -> SenderAssignment {
        owner
            .senders
            .iter()
            .find(|state| state.facts.id == sender)
            .expect("sender")
            .assignment
            .expect("explicit assignment")
    }

    fn bound_egress(
        facts: Box<[EgressSenderFacts]>,
        randomness: &[u8; 32],
        max_payload_bytes: usize,
        max_retransmission_bytes: usize,
        audio: SenderAssignment,
        video: SenderAssignment,
        now: Instant,
    ) -> MediaEgress {
        let assignments = facts
            .iter()
            .map(|facts| {
                (
                    facts.id,
                    match facts.kind {
                        MediaKind::Audio => audio,
                        MediaKind::Video => video,
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut owner = MediaEgress::new(
            facts,
            randomness,
            max_payload_bytes,
            max_retransmission_bytes,
            now,
        );
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for (sender, assignment) in assignments {
            owner
                .set_assignment(sender, Some(assignment), at, false)
                .expect("explicit test assignment");
        }
        owner.budget_changed = None;
        owner
    }

    fn playout_owner(now: Instant, extension: Option<u8>) -> MediaEgress {
        playout_owner_with_senders(now, extension, 2)
    }

    fn playout_owner_with_senders(now: Instant, extension: Option<u8>, count: u16) -> MediaEgress {
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::from_millis(149, 204).expect("explicit bounds")),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(256_000))
        };
        bound_egress(
            (1..=count)
                .map(|id| EgressSenderFacts {
                    id: SenderId::new(id).expect("sender"),
                    kind: MediaKind::Audio,
                    mid: format!("audio{id}").into(),
                    payload_type: 111,
                    retransmission_payload_type: Some(112),
                    clock_rate: 48_000,
                    mid_extension_id: Some(4),
                    twcc_extension_id: Some(3),
                    playout_extension_id: extension,
                    dependency_extension_id: None,
                    forwarded_extensions: Box::new([]),
                })
                .collect(),
            &[7; 32],
            1024,
            1024,
            policy,
            policy,
            now,
        )
    }

    #[test]
    fn payload_admission_bounds_sustained_rate_without_using_pacing_rate() {
        let now = Instant::now();
        let horizon = Duration::from_millis(80);
        let rate = 1_745_000;
        let mut admission = PayloadAdmission::default();
        let mut admitted = 0_u64;
        for tick in 0..6_000 {
            if admission.eligible(now + Duration::from_millis(tick * 2), rate, horizon) {
                admission.charge(1_000);
                admitted += 1_000;
            }
        }
        let ceiling =
            u128::from(rate) * (Duration::from_secs(12) + horizon).as_micros() / 8_000_000;
        assert!(u128::from(admitted) <= ceiling + 1_000);
        assert!(u128::from(admitted) >= ceiling.saturating_sub(2_000));
        assert!(!admission.eligible(now + Duration::from_secs(12), 0, horizon));
    }

    #[test]
    fn payload_admission_at_zero_rate_allows_only_one_initial_packet() {
        let now = Instant::now();
        let horizon = Duration::from_millis(80);
        let mut admission = PayloadAdmission::default();
        assert!(admission.eligible(now, 0, horizon));
        admission.charge(1_000);
        assert!(!admission.eligible(now + Duration::from_secs(1), 0, horizon));
        assert!(!admission.eligible(now + Duration::from_secs(10), 0, horizon));
        assert!(admission.eligible(now + Duration::from_secs(11), 80_000, horizon));
    }

    #[test]
    fn payload_admission_retries_do_not_charge_and_idle_has_no_burst_credit() {
        let now = Instant::now();
        let mut admission = PayloadAdmission::default();
        let horizon = Duration::from_millis(15);
        assert!(admission.eligible(now, 80_000, horizon));
        admission.charge(1_000);
        for _ in 0..10 {
            assert!(!admission.eligible(now, 80_000, horizon));
        }
        assert!(admission.eligible(now + Duration::from_millis(85), 80_000, horizon));
        assert!(admission.eligible(now + Duration::from_secs(10), 80_000, horizon));
        admission.charge(1_000);
        assert!(!admission.eligible(now + Duration::from_secs(10), 80_000, horizon));
    }

    #[test]
    fn rtcp_keyframe_requests_map_video_and_coalesce_with_fir_replay_protection() {
        let mut owner = playout_owner(Instant::now(), None);
        owner.senders[0].facts.kind = MediaKind::Video;
        let video = owner.senders[0].ssrc;
        let audio = owner.senders[1].ssrc;
        let id = owner.senders[0].facts.id;
        owner.budget_changed = None;
        for ssrc in [audio, 0, owner.senders[0].rtx_ssrc] {
            owner.request_keyframe(crate::rtcp::KeyframeRequest {
                media_ssrc: ssrc,
                fir: None,
            });
            assert!(owner.poll_event().is_none());
        }
        owner.senders[0].keyframe_requested = Some(KeyframeRequestReason::Peer);
        for _ in 0..2 {
            owner.request_keyframe(crate::rtcp::KeyframeRequest {
                media_ssrc: video,
                fir: None,
            });
        }
        assert!(
            matches!(owner.poll_event(), Some(Event::KeyframeRequested { sender, reason: KeyframeRequestReason::Peer }) if sender == id)
        );
        assert!(owner.poll_event().is_none());
        for (sequence, expected) in [
            (254, true),
            (254, false),
            (253, false),
            (255, true),
            (0, true),
            (0, false),
        ] {
            owner.request_keyframe(crate::rtcp::KeyframeRequest {
                media_ssrc: video,
                fir: Some((42, sequence)),
            });
            assert_eq!(
                matches!(owner.poll_event(), Some(Event::KeyframeRequested { sender, reason: KeyframeRequestReason::Peer }) if sender == id),
                expected
            );
        }
    }

    #[test]
    fn keyframe_requests_bound_fir_requesters_without_evicting_replay_state() {
        let mut owner = playout_owner(Instant::now(), None);
        owner.senders[0].facts.kind = MediaKind::Video;
        owner.budget_changed = None;
        let media_ssrc = owner.senders[0].ssrc;
        for requester in 0..256 {
            owner.request_keyframe(crate::rtcp::KeyframeRequest {
                media_ssrc,
                fir: Some((requester, 1)),
            });
            assert!(matches!(
                owner.poll_event(),
                Some(Event::KeyframeRequested { .. })
            ));
        }
        for requester in [256, 0] {
            owner.request_keyframe(crate::rtcp::KeyframeRequest {
                media_ssrc,
                fir: Some((requester, 1)),
            });
            assert!(owner.poll_event().is_none());
        }
        owner.request_keyframe(crate::rtcp::KeyframeRequest {
            media_ssrc,
            fir: Some((0, 2)),
        });
        assert!(matches!(
            owner.poll_event(),
            Some(Event::KeyframeRequested { .. })
        ));
        assert_eq!(owner.senders[0].fir_sequences.len(), 256);
    }

    fn queued_media(at: TimePoint, id: u64, boundary: FrameBoundary) -> ForwardedMedia {
        let sequence: u16 = match boundary {
            FrameBoundary::Start | FrameBoundary::Complete => 1,
            FrameBoundary::Middle => 2,
            FrameBoundary::End => 3,
        };
        let mut bytes = vec![0x80, 111, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 1, 2, 3];
        bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
        ForwardedMedia {
            source: pulsebeam_rtc::SourceIncarnation::from_value(1),
            packet: MediaPacket::new(bytes.into(), at.global, Arc::from([])),
            frame: FrameMetadata {
                descriptor: None,
                id: FrameId::from_value(id),
                boundary,
                random_access: true,
                discardable: false,
                dependencies: FrameDependencies::Unknown,
            },
        }
    }

    #[test]
    fn descriptor_budget_rejection_preserves_admission_and_source_facts() {
        use pulsebeam_core::dd::temporal::TemporalDdGenerator;
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner.senders[0].facts.kind = MediaKind::Video;
        owner.senders[0].descriptor = Some(DescriptorContinuity::default());
        let sender = owner.senders[0].facts.id;
        let mut generator = TemporalDdGenerator::new(3);
        let structure = Arc::new(generator.structure().clone());
        let mut media = |id| {
            let dd = generator.next(true);
            let mut media = queued_media(at, id, FrameBoundary::Complete);
            media.frame.dependencies = FrameDependencies::known(Vec::new()).unwrap();
            media.frame.descriptor = Some(Arc::new(crate::SourceDescriptor {
                template_id: dd.template_id,
                decode_target_indications: dd.frame_dependencies.dtis.into_iter().collect(),
                chain_dependencies: dd
                    .frame_dependencies
                    .chain_diffs
                    .into_iter()
                    .map(|_| None)
                    .collect(),
                structure: structure.clone(),
                active_decode_targets: 7,
                attached_structure: false,
            }));
            media
        };
        let rejected = media(1);
        let later = media(2);
        let original = rejected.frame.clone();
        let admission_before = (
            owner.senders[0].admission.debt,
            owner.senders[0].admission.updated_at,
        );
        owner.queued_descriptor_bytes = MAX_PACED_TRANSPORT_BYTES;
        assert_eq!(
            owner
                .admit(at, sender, rejected.clone())
                .map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock)
        );
        assert!(owner.queued_packets == 0);
        assert_eq!(owner.admitted_bytes, 0);
        assert_eq!(rejected.frame, original);
        assert_eq!(
            (
                owner.senders[0].admission.debt,
                owner.senders[0].admission.updated_at,
            ),
            admission_before
        );
        owner.queued_descriptor_bytes = 0;
        // A resolved independent frame does not need a seed packet accepted by RTC.
        owner.admit(at, sender, later).unwrap();
        assert_eq!(owner.queued_packets, 1);
        assert_eq!(
            owner.queued_descriptor_bytes,
            crate::SourceDescriptor::MEMORY_CHARGE
        );
    }

    #[test]
    fn probe_padding_uses_full_legal_packets_except_for_feedback_recovery() {
        let egress = playout_owner(Instant::now(), Some(5));
        let sender = &egress.senders[0];
        for (recovery, count) in [(false, 255), (true, 1)] {
            let padding = probe_padding(recovery, 1_000_000, 128);
            assert_eq!(padding.len(), count);
            assert_eq!(usize::from(*padding.last().expect("count")), count);
            assert!(padding[..count - 1].iter().all(|byte| *byte == 0));
            let packet = build_rtp(
                sender,
                false,
                1,
                0,
                Some(1),
                &padding,
                sender.facts.payload_type,
                sender.ssrc,
                true,
                &[],
            )
            .expect("padding RTP");
            assert_ne!(packet[0] & 0x20, 0);
            assert!(
                RtpPacket::parse(&packet)
                    .expect("valid padding")
                    .payload()
                    .is_empty()
            );
        }
    }

    #[test]
    fn rtx_padding_requires_receipt_of_its_own_primary_stream() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        let primary = owner.senders[0].ssrc;
        let rtx = owner.senders[0].rtx_ssrc;
        let sample = PacketFeedback {
            sent_id: crate::sent_history::SentPacketId::for_test(1),
            ssrc: primary,
            committed_at: now,
            transport_bytes: 300,
            service: RtpService::Padding,
            received: true,
            newly_acked: true,
            lost: false,
            loss_origin: None,
            receiver_arrival: None,
            ecn: None,
        };
        for feedback in [
            PacketFeedback {
                received: false,
                lost: true,
                loss_origin: Some(crate::congestion::LossOrigin::Reordering),
                ..sample
            },
            PacketFeedback {
                received: false,
                newly_acked: false,
                ..sample
            },
            PacketFeedback {
                ssrc: owner.senders[1].ssrc,
                ..sample
            },
            PacketFeedback {
                ssrc: rtx,
                ..sample
            },
        ] {
            owner.observe_probe_feedback(
                now,
                &[feedback],
                crate::congestion::ControllerReason::Feedback,
            );
            assert!(!owner.senders[0].primary_feedback_observed);
            assert_eq!(padding_source(&owner.senders[0]).2, primary);
        }
        owner.observe_probe_feedback(
            now,
            &[sample],
            crate::congestion::ControllerReason::Feedback,
        );
        assert!(owner.senders[0].primary_feedback_observed);
        assert_eq!(padding_source(&owner.senders[0]).2, rtx);
        assert_eq!(
            owner.senders[0].committed_packets, 0,
            "mapping does not invent media commits"
        );
    }

    #[test]
    fn padding_commits_only_its_negotiated_sequence_space() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for rtx in [true, false] {
            let mut owner = playout_owner(now, Some(12));
            if !rtx {
                owner.senders[0].facts.retransmission_payload_type = None;
            }
            owner.senders[0].primary_feedback_observed = true;
            owner.senders[0]
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = Some(PlayoutDelay::from_millis(10, 100).unwrap());
            let sender = &owner.senders[0];
            let primary_before = sender.next_sequence;
            let rtx_before = sender.next_rtx_sequence;
            let twcc_before = owner.next_twcc;
            let (sequence, payload_type, ssrc) = padding_source(sender);
            assert_eq!(ssrc, if rtx { sender.rtx_ssrc } else { sender.ssrc });
            assert_eq!(payload_type, if rtx { 112 } else { 111 });
            let twcc = Some(wire_u16(twcc_before));
            let packet = build_rtp(
                sender,
                false,
                sequence,
                sender.base_timestamp,
                twcc,
                &probe_padding(false, 1_000_000, 128),
                payload_type,
                ssrc,
                true,
                &[],
            )
            .unwrap();
            let parsed = RtpPacket::parse(&packet).unwrap();
            assert_eq!(parsed.ssrc(), ssrc);
            assert_eq!(parsed.sequence(), sequence);
            assert!(
                parsed.payload().is_empty(),
                "pure padding has no RTX OSN or media"
            );
            let extensions: Vec<_> = parsed.extensions().unwrap().collect();
            assert!(
                extensions
                    .iter()
                    .any(|extension| Some(extension.id()) == sender.facts.mid_extension_id)
            );
            assert!(
                extensions
                    .iter()
                    .any(|extension| Some(extension.id()) == sender.facts.twcc_extension_id)
            );
            assert_eq!(
                extensions
                    .iter()
                    .any(|extension| Some(extension.id()) == sender.facts.playout_extension_id),
                !rtx
            );
            let identity = PreparedRtpIdentity {
                ssrc,
                sequence,
                twcc_sequence: twcc,
                service: RtpService::Padding,
            };
            owner.pending = Some(PendingTransmit::Padding {
                sender: 0,
                ssrc,
                sequence,
                twcc,
            });
            let mut prepared = PreparedTransmit {
                target: crate::TransmitTarget::Udp {
                    local: "127.0.0.1:10000".parse().unwrap(),
                    remote: "127.0.0.1:10001".parse().unwrap(),
                    ecn: None,
                },
                wire_len: packet.len(),
                bytes: packet,
                kind: DatagramKind::Rtp,
                path_epoch: None,
                rtp: Some(identity),
            };
            assert!(owner.preflight_commit(&prepared));
            prepared.rtp = Some(PreparedRtpIdentity {
                ssrc: ssrc.wrapping_add(1),
                ..identity
            });
            assert!(!owner.preflight_commit(&prepared));
            assert_eq!(owner.senders[0].next_sequence, primary_before);
            assert_eq!(owner.senders[0].next_rtx_sequence, rtx_before);
            prepared.rtp = Some(identity);
            owner.commit(at, &prepared);
            let sender = &owner.senders[0];
            assert_eq!(sender.next_sequence, primary_before + u64::from(!rtx));
            assert_eq!(sender.next_rtx_sequence, rtx_before + u64::from(rtx));
            assert_eq!(sender.playout_first_sequence.is_some(), !rtx);
            assert_eq!(owner.next_twcc, twcc_before + 1);
            assert_eq!(owner.pacer.charged_bytes(), prepared.wire_len as u64);
        }
    }

    #[test]
    fn probes_reserve_the_explicit_queue_horizon_without_clearing_debt() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = None;
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        }
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .unwrap()
                .delivery
                .max_queue_residence = Duration::from_millis(15);
        }
        owner.pacer.commit(now, 1_000, 400_000);
        assert!(!owner.probe_fits_selected_horizon(now, 300, 400_000));
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .unwrap()
                .delivery
                .max_queue_residence = Duration::from_millis(80);
        }
        assert!(owner.probe_fits_selected_horizon(now, 300, 400_000));
        assert_eq!(owner.pacer.charged_bytes(), 1_000);
    }

    #[test]
    fn probe_padding_respects_low_rate_pacing_time() {
        for (rate, overhead, expected) in [
            (0, 128, 1),
            (110_000, 128, 1),
            (300_000, 128, 59),
            (1_000_000, 128, 255),
        ] {
            assert_eq!(probe_padding(false, rate, overhead).len(), expected);
        }
    }

    #[test]
    fn media_admission_aborts_probe_only_when_remaining_service_is_consumed() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for (remaining, elapsed_ms, window, abort) in [
            (0, 0, 9_000, false),
            (1, 0, 9_000, false),
            (1, 19, 9_000, true),
            (1, 0, 100, true),
        ] {
            let mut owner = playout_owner(now, None);
            owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
            let previous = now
                .checked_sub(Duration::from_secs(2))
                .expect("previous probe");
            owner.last_probe = Some(previous);
            owner.probe_remaining = remaining;
            owner.probe_started_at = Some(now - Duration::from_millis(elapsed_ms));
            let envelope = owner.envelope.as_mut().expect("controller envelope");
            envelope.pacing_transport_rate = 100_000;
            envelope.max_rtp_bytes_in_flight = window;
            let sender = owner.senders[0].facts.id;
            owner
                .admit(at, sender, queued_media(at, 1, FrameBoundary::Complete))
                .expect("admit media");
            assert_eq!(owner.probe_remaining, if abort { 0 } else { remaining });
            assert_eq!(owner.last_probe, Some(if abort { now } else { previous }));
        }
    }

    #[test]
    fn forwarded_extensions_remap_by_uri_and_survive_repair_without_source_transport_state() {
        let now = Instant::now();
        let mut owner = playout_owner(now, Some(12));
        owner.senders[0].facts.forwarded_extensions = vec![
            (7, "urn:ietf:params:rtp-hdrext:ssrc-audio-level".into()),
            (200, "urn:example:opaque".into()),
        ]
        .into();
        let mut source = vec![0x90, 111, 0, 1, 0, 0, 0, 7, 0, 0, 0, 9];
        assert!(write_extensions(
            &mut source,
            &[
                (2, vec![0x88]),
                (5, vec![0x55; 255]),
                (4, b"source-mid".to_vec())
            ],
            true,
        ));
        source.extend_from_slice(b"opaque media");
        let ranges = RtpPacket::parse(&source)
            .expect("source RTP")
            .extensions()
            .expect("source extensions")
            .filter_map(|extension| match extension.id() {
                2 => Some((
                    Arc::from("urn:ietf:params:rtp-hdrext:ssrc-audio-level"),
                    extension.range(),
                )),
                5 => Some((Arc::from("urn:example:opaque"), extension.range())),
                4 => Some((
                    Arc::from("urn:ietf:params:rtp-hdrext:sdes:mid"),
                    extension.range(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into();
        let global = GlobalMediaTime::from_micros(1_000_000);
        let media = MediaPacket::new(Bytes::from(source), global, ranges);
        let sender = &owner.senders[0];
        let original = build_rtp(
            sender,
            false,
            17,
            19,
            Some(23),
            b"opaque media",
            111,
            sender.ssrc,
            false,
            &forwarded_extensions(sender, &media),
        )
        .expect("outbound RTP");
        let parsed = RtpPacket::parse(&original).expect("original RTP");
        assert_eq!(parsed.extension_profile(), Some(0x1000));
        let extensions = repair_extensions(sender, &parsed);
        assert_eq!(extensions, vec![(7, vec![0x88]), (200, vec![0x55; 255])]);
        assert_eq!(parsed.payload(), b"opaque media");
        assert_eq!(media.global_media_at(), global);
        assert_eq!(
            media.extension("urn:ietf:params:rtp-hdrext:sdes:mid"),
            Some(&b"source-mid"[..])
        );
        owner.retain_original(0, 17, 19, &original, global, now, original.len() + 16);
        let retained = owner.retained.front().expect("retained original");
        assert_eq!(retained.packet, original);
        assert_eq!(owner.retained_bytes, original.len() + 16);
        let sender = &owner.senders[0];
        let repaired_source = RtpPacket::parse(&retained.packet).expect("retained RTP");
        let repaired = build_rtp(
            sender,
            repaired_source.marker(),
            31,
            retained.timestamp,
            Some(29),
            b"\0\x11opaque media",
            112,
            sender.rtx_ssrc,
            false,
            &repair_extensions(sender, &repaired_source),
        )
        .expect("RTX");
        let repaired = RtpPacket::parse(&repaired).expect("RTX RTP");
        assert!(!repaired.marker());
        assert_eq!(repair_extensions(sender, &repaired), extensions);
        assert!(
            repaired
                .extensions()
                .expect("RTX extensions")
                .any(|extension| extension.id() == 3 && extension.value() == 29_u16.to_be_bytes())
        );
    }

    #[test]
    fn opaque_extension_senders_keep_one_wire_format_even_when_values_are_absent_or_short() {
        let mut owner = playout_owner(Instant::now(), None);
        let sender = &mut owner.senders[0];
        assert!(sender_uses_one_byte_extensions(sender));
        sender.facts.forwarded_extensions = vec![(7, "urn:example:opaque".into())].into();
        for extensions in [vec![], vec![(7, vec![1])], vec![(7, vec![1; 255])]] {
            let packet = build_rtp(
                sender,
                false,
                1,
                1,
                Some(1),
                b"payload",
                111,
                sender.ssrc,
                false,
                &extensions,
            )
            .expect("RTP");
            assert_eq!(
                RtpPacket::parse(&packet)
                    .expect("parse")
                    .extension_profile(),
                Some(0x1000)
            );
        }
        sender.facts.forwarded_extensions =
            vec![(15, "urn:ietf:params:rtp-hdrext:ssrc-audio-level".into())].into();
        assert!(!sender_uses_one_byte_extensions(sender));
    }

    #[test]
    fn media_age_and_repair_window_are_independent_of_wire_playout() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for playout in [
            None,
            Some(PlayoutDelay::ZERO),
            Some(PlayoutDelay::from_millis(0, 500).unwrap()),
        ] {
            let mut owner = playout_owner(now, None);
            let assignment = owner.senders[0].assignment.as_mut().unwrap();
            assignment.playout_delay = playout;
            assignment.delivery.max_media_age = Duration::from_secs(1);
            assignment.delivery.repair_window = Duration::from_millis(500);
            let packet = queued_media(at, 1, FrameBoundary::Complete).packet;
            owner.retain_original(0, 7, 19, packet.bytes(), at.global, now, 40);
            let late = TimePoint {
                monotonic: now + Duration::from_millis(250),
                global: at.global.checked_add(Duration::from_millis(250)).unwrap(),
            };
            assert!(owner.useful_at(0, at.global, late, Duration::ZERO));
            assert!(owner.repair_useful_at(&owner.retained[0], late));
            assert!(owner.repair_due(&owner.retained[0], late.monotonic));
            owner.retained[0].last_repair_at = Some(late.monotonic);
            assert!(!owner.repair_due(
                &owner.retained[0],
                late.monotonic + Duration::from_millis(49)
            ));
            assert!(owner.repair_due(
                &owner.retained[0],
                late.monotonic + Duration::from_millis(50)
            ));
            let deadline = TimePoint {
                monotonic: now + Duration::from_millis(500),
                global: at.global.checked_add(Duration::from_millis(500)).unwrap(),
            };
            assert!(owner.useful_at(0, at.global, deadline, Duration::ZERO));
            assert!(!owner.repair_useful_at(&owner.retained[0], deadline));
            owner.senders[0]
                .assignment
                .as_mut()
                .unwrap()
                .delivery
                .max_media_age = Duration::from_millis(150);
            assert!(!owner.useful_at(0, at.global, late, Duration::ZERO));
            assert!(!owner.repair_useful_at(&owner.retained[0], late));
            owner.expire_repair(deadline.monotonic);
            assert!(owner.retained.is_empty());
        }
    }

    #[test]
    fn caller_repair_window_is_not_extended_by_long_rtt() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let assignment = owner.senders[0].assignment.as_mut().unwrap();
        assignment.delivery.max_media_age = Duration::from_secs(3);
        assignment.delivery.repair_window = Duration::from_secs(2);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        owner.envelope.as_mut().unwrap().smoothed_rtt = Duration::from_millis(900);
        let packet = queued_media(at, 1, FrameBoundary::Complete).packet;
        owner.retain_original(0, 7, 19, packet.bytes(), at.global, now, 40);
        for (age, expected) in [(1500, true), (1999, true), (2000, false)] {
            let time = TimePoint {
                monotonic: now + Duration::from_millis(age),
                global: at.global.checked_add(Duration::from_millis(age)).unwrap(),
            };
            assert_eq!(owner.repair_useful_at(&owner.retained[0], time), expected);
            assert!(owner.useful_at(0, at.global, time, Duration::ZERO));
        }
        owner.expire_repair(now + Duration::from_secs(2));
        assert!(owner.retained.is_empty());
    }

    #[test]
    fn repair_requests_cannot_outlive_evicted_originals_or_block_later_repairs() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        let packet = queued_media(
            TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(1_000_000),
            },
            1,
            FrameBoundary::Complete,
        )
        .packet;
        let wire_len = packet.bytes().len() + 16;
        owner.max_retransmission_bytes = wire_len;
        for sequence in [17, 18, 19] {
            owner.retain_original(
                0,
                sequence,
                7,
                packet.bytes(),
                packet.global_media_at(),
                now,
                wire_len,
            );
            assert!(
                owner.repair_requests.is_empty(),
                "eviction retires queued repair"
            );
            assert_eq!(owner.retained.len(), 1);
            assert_eq!(owner.retained_bytes, wire_len);
            owner.request_repair(owner.senders[0].ssrc, sequence - 1);
            assert!(
                owner.repair_requests.is_empty(),
                "unretained NACK cannot create state"
            );
            owner.request_repair(owner.senders[0].ssrc, sequence);
            owner.request_repair(owner.senders[0].ssrc, sequence);
            assert_eq!(owner.repair_requests, VecDeque::from([(0, sequence)]));
        }
    }

    #[test]
    fn forwarded_extension_values_obey_semantic_and_wire_size_bounds() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        owner.senders[0].facts.forwarded_extensions = vec![
            (7, "urn:ietf:params:rtp-hdrext:ssrc-audio-level".into()),
            (8, "urn:3gpp:video-orientation".into()),
            (
                9,
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time".into(),
            ),
            (10, "urn:example:opaque".into()),
        ]
        .into();
        for length in [0, 1, 8, 16, 255, 256] {
            let media = MediaPacket::new(
                Bytes::from(vec![0; length]),
                GlobalMediaTime::from_micros(1),
                owner.senders[0]
                    .facts
                    .forwarded_extensions
                    .iter()
                    .map(|(_, uri)| (Arc::from(&**uri), 0..length))
                    .collect::<Vec<_>>()
                    .into(),
            );
            let extensions = forwarded_extensions(&owner.senders[0], &media);
            assert_eq!(extensions.iter().any(|(id, _)| *id == 7), length == 1);
            assert_eq!(extensions.iter().any(|(id, _)| *id == 8), length == 1);
            assert_eq!(
                extensions.iter().any(|(id, _)| *id == 9),
                matches!(length, 8 | 16)
            );
            assert_eq!(extensions.iter().any(|(id, _)| *id == 10), length <= 255);
        }
    }

    #[test]
    fn transport_budget_does_not_redistribute_caller_targets() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        let targets = [
            {
                let assignment = current_assignment(&owner, owner.senders[0].facts.id);
                (assignment.selected_bitrate, assignment.potential_bitrate)
            },
            {
                let assignment = current_assignment(&owner, owner.senders[1].facts.id);
                (assignment.selected_bitrate, assignment.potential_bitrate)
            },
        ];
        for budget in [512_000, 1_000_000, 50_000] {
            owner.update_media_budget(budget);
            assert_eq!(owner.media_budget, budget);
            assert_eq!(
                owner
                    .stats()
                    .1
                    .iter()
                    .map(|sender| {
                        let assignment = sender.assignment.expect("bound sender");
                        (assignment.selected_bitrate, assignment.potential_bitrate)
                    })
                    .collect::<Vec<_>>(),
                targets
            );
        }
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = Some(PlayoutDelay::ZERO);
        assert_eq!(
            {
                let assignment = current_assignment(&owner, owner.senders[0].facts.id);
                (assignment.selected_bitrate, assignment.potential_bitrate)
            },
            targets[0]
        );
        assert_eq!(
            {
                let assignment = current_assignment(&owner, owner.senders[1].facts.id);
                (assignment.selected_bitrate, assignment.potential_bitrate)
            },
            targets[1]
        );
    }

    #[test]
    fn probe_candidate_uses_actual_unmet_source_interest_not_probe_size_scale() {
        let mut owner = playout_owner(Instant::now(), None);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = Some(PlayoutDelay::ZERO);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = Some(PlayoutDelay::from_millis(0, 500).unwrap());
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(300_000);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(290_000);
        assert_eq!(owner.probe_candidate(), Some(0));
    }

    #[test]
    fn transport_budget_updates_coalesce_without_sender_allocations() {
        let mut owner = playout_owner(Instant::now(), None);
        for budget in 1..=1_000 {
            owner.update_media_budget(budget);
        }
        assert!(
            matches!(owner.poll_event(), Some(Event::MediaBudgetChanged(budget)) if budget.as_bps() == 1_000)
        );
        assert!(owner.poll_event().is_none());
        owner.update_media_budget(1_000);
        assert!(owner.poll_event().is_none());
    }

    #[test]
    fn sender_recovery_preserves_committed_repairs_until_destructive_discard() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, Some(12));
        let a = owner.senders[0].facts.id;
        let b = owner.senders[1].facts.id;
        owner
            .admit(at, a, queued_media(at, 1, FrameBoundary::Start))
            .expect("queue interrupted frame");
        owner
            .admit(at, b, queued_media(at, 2, FrameBoundary::Complete))
            .expect("queue sibling");
        let bytes = queued_media(at, 3, FrameBoundary::Complete).packet;
        owner.retain_original(0, 17, 19, bytes.bytes(), at.global, now, 40);
        owner.request_repair(owner.senders[0].ssrc, 17);
        owner.senders[0].latest_committed_global = Some(at.global);
        let payload_debt = owner.senders[0].admission.debt;
        owner
            .clear_sender_media(a, true)
            .expect("recover same publication");
        assert_eq!(owner.queued_packets, 1);
        assert!(owner.senders[0].queue.is_empty());
        assert_eq!(owner.senders[1].queue.len(), 1);
        assert_eq!(
            owner.senders[1].queued_frames,
            BTreeSet::from([FrameId::from_value(2)])
        );
        assert!(owner.senders[0].open_frame.is_none());
        assert!(!owner.senders[0].dependency_chain_valid);
        assert_eq!(owner.senders[0].latest_admitted_global, Some(at.global));
        assert_eq!(owner.senders[0].admission.debt, payload_debt);
        assert_eq!(owner.retained.len(), 1);
        assert_eq!(owner.retained[0].sequence, 17);
        assert_eq!(owner.retained_bytes, 40);
        assert_eq!(owner.repair_requests, VecDeque::from([(0, 17)]));
        owner
            .clear_sender_media(a, false)
            .expect("replace publication");
        assert!(owner.retained.is_empty());
        assert_eq!(owner.retained_bytes, 0);
        assert!(owner.repair_requests.is_empty());
        owner.request_repair(owner.senders[0].ssrc, 17);
        assert!(owner.repair_requests.is_empty());
        assert_eq!(owner.queued_packets, 1);
    }

    #[test]
    fn sender_discard_clears_only_its_queue_dependencies_and_repairability() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, Some(12));
        let a = owner.senders[0].facts.id;
        let b = owner.senders[1].facts.id;
        owner
            .admit(at, a, queued_media(at, 1, FrameBoundary::Start))
            .expect("queue start");
        owner
            .admit(at, b, queued_media(at, 2, FrameBoundary::Complete))
            .expect("queue sibling");
        let bytes = queued_media(at, 3, FrameBoundary::Complete).packet;
        for index in 0..2 {
            owner.retain_original(index, 17, 19, bytes.bytes(), at.global, now, 40);
            owner.request_repair(owner.senders[index].ssrc, 17);
        }
        owner.senders[0].latest_committed_global = Some(at.global);
        owner.senders[0].playout_first_sequence = Some(owner.senders[0].next_sequence);
        owner.senders[0].playout_confirmed = true;
        owner.senders[0].playout_ever_signaled = true;
        let state = &owner.senders[0];
        let continuity = (
            (
                state.ssrc,
                state.rtx_ssrc,
                state.next_sequence,
                state.next_rtx_sequence,
            ),
            (
                state.base_timestamp,
                state.base_global,
                state.latest_committed_global,
            ),
            (state.committed_packets, state.committed_payload_bytes),
            (
                state.playout_first_sequence,
                state.playout_confirmed,
                state.playout_ever_signaled,
            ),
            owner.next_twcc,
        );
        let sibling_bytes = owner.senders[1].queue[0].transport_estimate;
        owner.clear_sender_media(a, false).expect("discard");
        assert_eq!(owner.queued_packets, 1);
        assert!(owner.senders[0].queue.is_empty());
        assert_eq!(owner.senders[1].queue.len(), 1);
        assert_eq!(owner.queued_payload_bytes, 3);
        assert_eq!(owner.queued_transport_bytes, sibling_bytes);
        assert_eq!(
            owner.senders[1].queued_frames,
            BTreeSet::from([FrameId::from_value(2)])
        );
        assert_eq!(owner.retained.len(), 1);
        assert_eq!(owner.retained[0].sender, 1);
        assert_eq!(owner.retained_bytes, 40);
        assert_eq!(owner.repair_requests, VecDeque::from([(1, 17)]));
        let state = &owner.senders[0];
        assert!(state.open_frame.is_none());
        assert!(!state.dependency_chain_valid);
        assert_eq!(state.latest_admitted_global, state.latest_committed_global);
        assert_eq!(
            (
                (
                    state.ssrc,
                    state.rtx_ssrc,
                    state.next_sequence,
                    state.next_rtx_sequence
                ),
                (
                    state.base_timestamp,
                    state.base_global,
                    state.latest_committed_global
                ),
                (state.committed_packets, state.committed_payload_bytes),
                (
                    state.playout_first_sequence,
                    state.playout_confirmed,
                    state.playout_ever_signaled
                ),
                owner.next_twcc
            ),
            continuity
        );
    }

    #[test]
    fn interrupted_frame_reason_does_not_mask_queued_duplicates() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let sender = owner.senders[0].facts.id;
        owner
            .admit(at, sender, queued_media(at, 0, FrameBoundary::Complete))
            .unwrap();
        owner
            .admit(at, sender, queued_media(at, 1, FrameBoundary::Start))
            .unwrap();
        assert_eq!(
            owner
                .admit(at, sender, queued_media(at, 0, FrameBoundary::Complete))
                .map_err(|(reason, _)| reason),
            Err(CommandError::InvalidFrameMetadata)
        );
        assert_eq!(
            owner
                .admit(at, sender, queued_media(at, 1, FrameBoundary::Start))
                .map_err(|(reason, _)| reason),
            Err(CommandError::InvalidFrameMetadata)
        );
        assert_eq!(
            owner
                .admit(at, sender, queued_media(at, 2, FrameBoundary::Start))
                .map_err(|(reason, _)| reason),
            Err(CommandError::FrameInterrupted)
        );
    }

    #[test]
    fn fragmented_frame_repays_payload_debt_before_starting_another_frame() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let sender = owner.senders[0].facts.id;
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = Some(PlayoutDelay::from_millis(0, 500).unwrap());
        let horizon = owner.senders[0]
            .assignment
            .expect("bound sender")
            .delivery
            .max_queue_residence;
        let initial_debt = horizon
            .as_nanos()
            .saturating_mul(u128::from(owner.senders[0].selected_bitrate()));
        owner.senders[0].admission.debt = initial_debt;
        let mut payload_bytes = 0;
        for (sequence, boundary) in [
            FrameBoundary::Start,
            FrameBoundary::Middle,
            FrameBoundary::End,
        ]
        .into_iter()
        .enumerate()
        {
            let mut media = queued_media(at, 1, boundary);
            let mut bytes = media.packet.bytes().to_vec();
            bytes[2..4].copy_from_slice(&u16::try_from(sequence).unwrap().to_be_bytes());
            payload_bytes += RtpPacket::parse(&bytes).unwrap().payload().len();
            media.packet =
                MediaPacket::new(bytes.into(), media.packet.global_media_at(), Arc::from([]));
            owner
                .admit(at, sender, media)
                .expect("bounded frame continuation");
        }
        assert!(owner.senders[0].open_frame.is_none());
        assert_eq!(owner.queued_packets, 3);
        assert_eq!(
            owner.senders[0].admission.debt,
            initial_debt + (payload_bytes as u128) * 8_000_000_000
        );
        assert_eq!(
            owner
                .admit(at, sender, queued_media(at, 2, FrameBoundary::Complete))
                .map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock)
        );
        let later = TimePoint {
            monotonic: now + Duration::from_secs(1),
            global: at.global,
        };
        owner
            .admit(later, sender, queued_media(at, 2, FrameBoundary::Complete))
            .expect("new frame after debt repayment");
    }

    #[test]
    fn source_fragment_gaps_and_reordering_require_explicit_recovery() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let sender = owner.senders[0].facts.id;
        for boundary in [FrameBoundary::Middle, FrameBoundary::End] {
            assert_eq!(
                owner
                    .admit(at, sender, queued_media(at, 1, boundary))
                    .map_err(|(reason, _)| reason),
                Err(CommandError::FrameInterrupted)
            );
            assert!(owner.queued_packets == 0);
        }
        let source_sequence = |mut media: ForwardedMedia, sequence: u16| {
            let mut bytes = media.packet.bytes().to_vec();
            bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
            media.packet =
                MediaPacket::new(bytes.into(), media.packet.global_media_at(), Arc::from([]));
            media
        };
        for sequence in [7_u16, u16::MAX] {
            let mut owner = playout_owner(now, None);
            owner.max_payload_bytes = 6;
            let sender = owner.senders[0].facts.id;
            owner
                .admit(
                    at,
                    sender,
                    source_sequence(queued_media(at, 1, FrameBoundary::Start), sequence),
                )
                .expect("start");
            let end = source_sequence(
                queued_media(at, 1, FrameBoundary::End),
                sequence.wrapping_add(2),
            );
            let original = end.packet.bytes().clone();
            assert_eq!(
                owner
                    .admit(at, sender, end.clone())
                    .map_err(|(reason, _)| reason),
                Err(CommandError::FrameInterrupted)
            );
            assert_eq!(owner.queued_packets, 1);
            assert_eq!(
                owner.senders[0]
                    .open_frame
                    .as_ref()
                    .unwrap()
                    .source_sequence,
                sequence
            );
            assert_eq!(end.packet.bytes(), &original);
            let middle = source_sequence(
                queued_media(at, 1, FrameBoundary::Middle),
                sequence.wrapping_add(1),
            );
            owner
                .admit(at, sender, middle.clone())
                .expect("ordered missing packet may repair admission before discard");
            assert_eq!(
                owner
                    .admit(at, sender, middle)
                    .map_err(|(reason, _)| reason),
                Err(CommandError::FrameInterrupted)
            );
            for offset in [4, 8] {
                let mut changed = end.clone();
                let mut bytes = changed.packet.bytes().to_vec();
                bytes[offset..offset + 4].copy_from_slice(&99_u32.to_be_bytes());
                changed.packet = MediaPacket::new(
                    bytes.into(),
                    changed.packet.global_media_at(),
                    Arc::from([]),
                );
                assert_eq!(
                    owner
                        .admit(at, sender, changed)
                        .map_err(|(reason, _)| reason),
                    Err(CommandError::InvalidFrameMetadata)
                );
            }
            assert_eq!(
                owner
                    .admit(at, sender, end.clone())
                    .map_err(|(reason, _)| reason),
                Err(CommandError::WouldBlock)
            );
            assert_eq!(
                owner.senders[0]
                    .open_frame
                    .as_ref()
                    .unwrap()
                    .source_sequence,
                sequence.wrapping_add(1)
            );
            owner
                .clear_sender_media(sender, false)
                .expect("explicit recovery");
            assert_eq!(
                owner.admit(at, sender, end).map_err(|(reason, _)| reason),
                Err(CommandError::FrameInterrupted)
            );
        }
    }

    #[test]
    fn audio_talkspurt_is_consumed_only_by_original_commit() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let sender = owner.senders[0].facts.id;
        assert!(owner.senders[0].audio_talkspurt_pending);
        owner
            .admit(at, sender, queued_media(at, 0, FrameBoundary::Complete))
            .expect("admit audio");
        assert!(
            owner.senders[0].audio_talkspurt_pending,
            "admission is not transmission"
        );
        owner
            .clear_sender_media(sender, false)
            .expect("discard unprepared audio");
        assert!(owner.senders[0].audio_talkspurt_pending);
        owner
            .admit(at, sender, queued_media(at, 1, FrameBoundary::Complete))
            .expect("replacement audio");
        let state = &owner.senders[0];
        let sequence = wire_u16(state.next_sequence);
        let timestamp = state.base_timestamp;
        let packet = build_rtp(
            state,
            true,
            sequence,
            timestamp,
            None,
            b"audio",
            state.facts.payload_type,
            state.ssrc,
            false,
            &[],
        )
        .expect("prepare RTP");
        let identity = PreparedRtpIdentity {
            ssrc: state.ssrc,
            sequence,
            twcc_sequence: None,
            service: RtpService::Original,
        };
        owner.pending = Some(PendingTransmit::Original {
            sender: 0,
            sequence,
            timestamp,
            twcc: None,
            packet: packet.clone(),
            descriptor: None,
        });
        assert_eq!(
            owner.clear_sender_media(sender, false),
            Err(CommandError::InvalidState)
        );
        assert!(
            owner.senders[0].audio_talkspurt_pending,
            "preparation and rejected discard do not consume marker"
        );
        let prepared = PreparedTransmit {
            target: crate::TransmitTarget::Udp {
                local: "127.0.0.1:10000".parse().unwrap(),
                remote: "127.0.0.1:10001".parse().unwrap(),
                ecn: None,
            },
            wire_len: packet.len(),
            bytes: packet,
            kind: DatagramKind::Rtp,
            path_epoch: None,
            rtp: Some(identity),
        };
        owner.commit(at, &prepared);
        assert!(!owner.senders[0].audio_talkspurt_pending);
        owner
            .clear_sender_media(sender, false)
            .expect("discard after commit");
        assert!(owner.senders[0].audio_talkspurt_pending);
        assert!(
            owner.senders[1].audio_talkspurt_pending,
            "sibling sender was untouched"
        );
    }

    #[test]
    fn expiry_coalesces_video_recovery_without_requesting_audio_keyframes() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner.senders[0].facts.kind = MediaKind::Video;
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = Some(PlayoutDelay::from_millis(0, 1_000).unwrap());
        }
        while owner.poll_event().is_some() {}
        let video = owner.senders[0].facts.id;
        let audio = owner.senders[1].facts.id;
        owner
            .admit(at, audio, queued_media(at, 2, FrameBoundary::Complete))
            .unwrap();
        owner
            .admit(at, video, queued_media(at, 1, FrameBoundary::Start))
            .unwrap();
        owner
            .admit(at, video, queued_media(at, 1, FrameBoundary::Middle))
            .unwrap();
        let later = TimePoint {
            monotonic: now + Duration::from_secs(10),
            global: GlobalMediaTime::from_micros(11_000_000),
        };
        let request = crate::rtcp::KeyframeRequest {
            media_ssrc: owner.senders[0].ssrc,
            fir: None,
        };
        owner.request_keyframe(request);
        owner.expire(later, MAX_EXPIRATIONS_PER_POLL);
        owner.request_keyframe(request);
        assert!(owner.queued_packets == 0);
        assert!(owner.senders[0].open_frame.is_none());
        assert!(!owner.senders[0].dependency_chain_valid);
        assert!(
            matches!(owner.poll_event(), Some(Event::KeyframeRequested { sender, reason: KeyframeRequestReason::LocalRecovery }) if sender == video)
        );
        assert!(
            owner.poll_event().is_none(),
            "one coalesced video request, no audio request"
        );
        let mut dependent = queued_media(later, 3, FrameBoundary::Complete);
        dependent.frame.random_access = false;
        dependent.frame.dependencies =
            FrameDependencies::Known(Arc::from([FrameId::from_value(1)]));
        assert_eq!(
            owner
                .admit(later, video, dependent)
                .map_err(|(reason, _)| reason),
            Err(CommandError::DependencyChainInterrupted)
        );
        owner
            .admit(
                later,
                video,
                queued_media(later, 4, FrameBoundary::Complete),
            )
            .expect("fresh independent video");
        assert_eq!(owner.queued_packets, 1);
        assert!(
            !owner.senders[0].dependency_chain_valid,
            "admission is not commitment"
        );
    }

    #[test]
    fn sender_discard_requires_new_independent_media_after_interrupted_frame() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let id = owner.senders[0].facts.id;
        owner
            .admit(at, id, queued_media(at, 1, FrameBoundary::Start))
            .expect("start");
        owner.clear_sender_media(id, false).expect("discard");
        let mut dependent = queued_media(at, 2, FrameBoundary::Complete);
        dependent.frame.random_access = false;
        dependent.frame.dependencies =
            FrameDependencies::Known(Arc::from([FrameId::from_value(1)]));
        assert!(matches!(
            owner.admit(at, id, dependent.clone()),
            Err((CommandError::DependencyChainInterrupted, _))
        ));
        owner
            .admit(at, id, queued_media(at, 3, FrameBoundary::Complete))
            .expect("fresh random access frame");
        assert_eq!(
            owner.admit(at, id, dependent).map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock),
            "a queued independent frame can restore the dependency chain"
        );
    }

    #[test]
    fn sender_discard_never_cancels_prepared_protected_media() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let id = owner.senders[0].facts.id;
        owner
            .admit(at, id, queued_media(at, 1, FrameBoundary::Complete))
            .expect("queue");
        let sequence = owner.senders[0].next_sequence;
        let twcc = owner.next_twcc;
        owner.pending = Some(PendingTransmit::Original {
            sender: 0,
            sequence: wire_u16(sequence),
            timestamp: 7,
            twcc: Some(wire_u16(twcc)),
            packet: vec![1, 2, 3],
            descriptor: None,
        });
        assert!(matches!(
            owner.clear_sender_media(id, false),
            Err(CommandError::InvalidState)
        ));
        assert_eq!(owner.queued_packets, 1);
        assert_eq!(owner.queued_payload_bytes, 3);
        assert_eq!(owner.senders[0].next_sequence, sequence);
        assert_eq!(owner.next_twcc, twcc);
        assert!(matches!(
            owner.pending,
            Some(PendingTransmit::Original { sender: 0, .. })
        ));
    }

    fn stamped_delay(sender: &Sender) -> Option<Vec<u8>> {
        let bytes = build_rtp(
            sender,
            false,
            wire_u16(sender.next_sequence),
            1,
            Some(1),
            b"media",
            111,
            sender.ssrc,
            false,
            &[],
        )
        .expect("RTP");
        RtpPacket::parse(&bytes)
            .expect("parse RTP")
            .extensions()
            .expect("extensions")
            .find(|extension| Some(extension.id()) == sender.facts.playout_extension_id)
            .map(|extension| extension.value().to_vec())
    }

    #[test]
    fn primary_padding_waits_for_frame_end_without_a_busy_deadline() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner
            .set_assignment(
                owner.senders[0].facts.id,
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(0),
                        MediaPayloadBitrate::from_bps(3_000_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&owner, owner.senders[0].facts.id)
                    }
                }),
                at,
                false,
            )
            .unwrap();
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        assert_eq!(owner.probe_candidate(), Some(0));
        assert_eq!(owner.next_deadline(at), Some(now));

        let packet = MediaPacket::new(
            Bytes::from_static(&[0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1]),
            at.global,
            Arc::from([]),
        );
        let mut media = crate::test_support::forwarded(packet, 1);
        media.frame.boundary = FrameBoundary::Start;
        let source = RtpPacket::parse(media.packet.bytes()).unwrap();
        owner.apply_frame_admission(0, &media, &source);
        assert!(!owner.padding_allowed(0));
        assert_eq!(
            owner.probe_candidate(),
            Some(1),
            "other senders can still probe"
        );
        owner.probe_sender = Some(0);
        owner.probe_remaining = 1;
        assert_eq!(
            owner.active_probe_candidate(),
            Some(1),
            "an unstarted probe can change senders"
        );
        owner.probe_started_at = Some(now);
        assert_eq!(
            owner.active_probe_candidate(),
            None,
            "started clusters keep their sender"
        );
        owner.probe_started_at = None;
        owner
            .set_assignment(
                owner.senders[1].facts.id,
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        pulsebeam_rtc::MediaPayloadBitrate::from_bps(0),
                        pulsebeam_rtc::MediaPayloadBitrate::from_bps(0),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&owner, owner.senders[1].facts.id)
                    }
                }),
                at,
                false,
            )
            .unwrap();
        assert_eq!(owner.probe_candidate(), None);
        assert_eq!(owner.next_deadline(at), None);

        owner.probe_sender = Some(0);
        owner.probe_remaining = 1;
        assert_eq!(
            owner.next_deadline(at),
            None,
            "an ongoing cluster also waits for frame completion"
        );
        media.frame.boundary = FrameBoundary::End;
        owner.apply_frame_admission(0, &media, &source);
        assert!(owner.padding_allowed(0));
        assert_eq!(owner.probe_candidate(), Some(0));
        owner.probe_remaining = 0;
        assert_eq!(owner.next_deadline(at), Some(now));
    }

    #[test]
    fn bounded_expiry_rotates_across_senders_and_preserves_queue_accounting() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner_with_senders(now, None, 33);
        owner.envelope = Some(SafeRtpEnvelope {
            pacing_transport_rate: 100_000_000,
            queue_delay: Duration::ZERO,
            smoothed_rtt: Duration::ZERO,
            ..owner.controller.snapshot(owner.potential_bitrate(), 0, 0)
        });
        for index in 0..32 {
            owner.senders[index].facts.kind = MediaKind::Video;
            owner.senders[index]
                .assignment
                .as_mut()
                .unwrap()
                .delivery
                .max_queue_residence = Duration::from_millis(50);
            for frame in 0..9 {
                owner
                    .admit(
                        at,
                        owner.senders[index].facts.id,
                        queued_media(at, frame, FrameBoundary::Complete),
                    )
                    .unwrap();
            }
        }
        let late = TimePoint {
            monotonic: now + Duration::from_millis(60),
            global: at.global.checked_add(Duration::from_millis(60)).unwrap(),
        };
        owner
            .admit(
                late,
                owner.senders[32].facts.id,
                queued_media(late, 99, FrameBoundary::Complete),
            )
            .unwrap();
        let storage = owner.eligible.as_ptr();
        owner.expire(late, MAX_EXPIRATIONS_PER_POLL);
        assert_eq!(owner.queued_packets, 33);
        let (_, stats) = owner.stats();
        assert_eq!(stats.iter().map(|s| s.queued_packets).sum::<usize>(), 33);
        assert_eq!(
            stats.iter().map(|s| s.queued_payload_bytes).sum::<usize>(),
            owner.queued_payload_bytes
        );
        assert_eq!(
            owner
                .senders
                .iter()
                .map(|s| s.queued_frames.len())
                .sum::<usize>(),
            33
        );
        assert_eq!(
            owner
                .senders
                .iter()
                .flat_map(|s| &s.queue)
                .map(|q| q.transport_estimate)
                .sum::<usize>(),
            owner.queued_transport_bytes
        );
        owner.fill_eligible(late);
        assert_eq!(
            owner.eligible.iter().filter(|slot| slot.is_some()).count(),
            1
        );
        assert_eq!(owner.eligible[32], Some(3));
        assert_eq!(owner.scheduler.select(&owner.eligible), Some(32));
        assert_eq!(owner.queue_deadline(late), Some(late.monotonic));
        owner.expire(late, MAX_EXPIRATIONS_PER_POLL);
        owner.fill_eligible(late);
        assert_eq!(owner.eligible.as_ptr(), storage);
        assert_eq!(owner.queued_packets, 1);
        assert_eq!(owner.queued_payload_bytes, 3);
        assert_eq!(owner.queued_descriptor_bytes, 0);
        assert_eq!(
            owner
                .senders
                .iter()
                .map(|s| s.queued_frames.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            owner.queued_transport_bytes,
            owner.senders[32].queue[0].transport_estimate
        );
        assert!(owner.senders[..32].iter().all(|s| s.queue.is_empty()
            && s.queued_frames.is_empty()
            && s.queued_payload_bytes == 0));
    }

    #[test]
    fn expiry_budget_fences_fresh_tails_and_keeps_sibling_service_ready() {
        for residence_limited in [true, false] {
            let now = Instant::now();
            let at = TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(1_000_000),
            };
            let mut owner = playout_owner(now, None);
            owner.senders[0].facts.kind = MediaKind::Video;
            let limits = &mut owner.senders[0].assignment.as_mut().unwrap().delivery;
            limits.max_media_age =
                Duration::from_millis(if residence_limited { 10_000 } else { 50 });
            limits.max_queue_residence =
                Duration::from_millis(if residence_limited { 50 } else { 10_000 });
            owner.envelope = Some(SafeRtpEnvelope {
                pacing_transport_rate: 100_000_000,
                queue_delay: Duration::ZERO,
                smoothed_rtt: Duration::ZERO,
                ..owner.controller.snapshot(owner.potential_bitrate(), 0, 0)
            });
            let video = owner.senders[0].facts.id;
            let audio = owner.senders[1].facts.id;
            let fragment = |sequence: u16, boundary| {
                let mut media = queued_media(at, 10, boundary);
                let mut bytes = media.packet.bytes().to_vec();
                bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
                media.packet = MediaPacket::new(bytes.into(), at.global, Arc::from([]));
                media
            };
            owner
                .admit(at, video, fragment(1, FrameBoundary::Start))
                .unwrap();
            let tail_at = TimePoint {
                monotonic: now + Duration::from_millis(40),
                global: at.global.checked_add(Duration::from_millis(40)).unwrap(),
            };
            let count = u16::try_from(MAX_EXPIRATIONS_PER_POLL + 2).unwrap();
            for sequence in 2..=count {
                owner
                    .admit(
                        tail_at,
                        video,
                        fragment(
                            sequence,
                            if sequence == count {
                                FrameBoundary::End
                            } else {
                                FrameBoundary::Middle
                            },
                        ),
                    )
                    .unwrap();
            }
            let late = TimePoint {
                monotonic: now + Duration::from_millis(60),
                global: at.global.checked_add(Duration::from_millis(60)).unwrap(),
            };
            owner
                .admit(late, audio, queued_media(late, 99, FrameBoundary::Complete))
                .unwrap();
            let debt = owner.senders[0].admission.debt;
            let storage = owner.eligible.as_ptr();
            owner.expire(late, MAX_EXPIRATIONS_PER_POLL);
            assert_eq!(owner.queued_packets, 3);
            assert_eq!(owner.senders[0].queue.len(), 2);
            assert_eq!(
                owner.senders[0].queue.back().unwrap().media.frame.boundary,
                FrameBoundary::End
            );
            assert_eq!(
                owner.senders[0].expiring_frame,
                Some(FrameId::from_value(10))
            );
            assert_eq!(owner.queue_deadline(late), Some(late.monotonic));
            owner.fill_eligible(late);
            assert_eq!(owner.eligible, vec![None, Some(3)]);
            assert_eq!(owner.eligible.as_ptr(), storage);
            assert_eq!(owner.scheduler.select(&owner.eligible), Some(1));
            assert!(
                matches!(owner.poll_event(),Some(Event::KeyframeRequested{sender,reason:KeyframeRequestReason::LocalRecovery}) if sender==video)
            );
            owner.expire(late, MAX_EXPIRATIONS_PER_POLL);
            assert_eq!(owner.queued_packets, 1);
            assert!(owner.senders[0].queue.is_empty());
            assert!(owner.senders[0].queued_frames.is_empty());
            assert!(owner.senders[0].expiring_frame.is_none());
            assert_eq!(owner.senders[0].admission.debt, debt);
            assert!(owner.poll_event().is_none());
            assert_eq!(owner.stats().1[0].queued_packets, 0);
            assert_eq!(owner.stats().1[1].queued_payload_bytes, 3);
        }
    }

    #[test]
    fn blocked_media_expires_without_charging_debt_and_fresh_media_resumes() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for delay in [
            None,
            Some(PlayoutDelay::ZERO),
            Some(PlayoutDelay::from_millis(149, 204).unwrap()),
        ] {
            let mut owner = playout_owner(now, None);
            owner.senders[0]
                .assignment
                .as_mut()
                .unwrap()
                .delivery
                .max_media_age = Duration::from_millis(100);
            owner.senders[0]
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = delay;
            owner.senders[0].admission.debt = owner.senders[0]
                .assignment
                .expect("bound sender")
                .delivery
                .max_queue_residence
                .as_nanos()
                * u128::from(owner.senders[0].selected_bitrate());
            let sender = owner.senders[0].facts.id;
            assert_eq!(
                owner
                    .admit(at, sender, queued_media(at, 1, FrameBoundary::Complete))
                    .map_err(|(reason, _)| reason),
                Ok(())
            );
            let blocked = queued_media(at, 2, FrameBoundary::Complete);
            assert_eq!(
                owner
                    .admit(at, sender, blocked.clone())
                    .map_err(|(reason, _)| reason),
                Err(CommandError::WouldBlock)
            );
            let debt = owner.senders[0].admission.debt;
            let late = TimePoint {
                monotonic: now + Duration::from_millis(201),
                global: at.global.checked_add(Duration::from_millis(201)).unwrap(),
            };
            assert_eq!(
                owner
                    .admit(late, sender, blocked)
                    .map_err(|(reason, _)| reason),
                Err(CommandError::MediaExpired)
            );
            assert_eq!(owner.senders[0].admission.debt, debt);
            assert_eq!(owner.queued_packets, 1);
            owner.expire(late, MAX_EXPIRATIONS_PER_POLL);
            assert!(owner.queued_packets == 0);
            assert_eq!(
                owner
                    .admit(late, sender, queued_media(late, 3, FrameBoundary::Complete))
                    .map_err(|(reason, _)| reason),
                Ok(())
            );
            assert_eq!(owner.queued_packets, 1);
        }
    }

    #[test]
    fn potential_demand_recovers_without_media_allocation_or_controller_reset() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let ids = [owner.senders[0].facts.id, owner.senders[1].facts.id];
        for (sender, potential) in ids.into_iter().zip([3_000_000, 1_000_000]) {
            owner
                .set_assignment(
                    sender,
                    Some({
                        let (selected_bitrate, potential_bitrate) = (
                            MediaPayloadBitrate::from_bps(0),
                            MediaPayloadBitrate::from_bps(potential),
                        );
                        pulsebeam_rtc::SenderAssignment {
                            selected_bitrate,
                            potential_bitrate,
                            ..current_assignment(&owner, sender)
                        }
                    }),
                    at,
                    false,
                )
                .expect("declare demand");
        }
        assert!(
            owner
                .senders
                .iter()
                .all(|sender| sender.selected_bitrate() == 0)
        );
        assert_eq!(
            owner.stats().1[0]
                .assignment
                .expect("bound sender")
                .potential_bitrate
                .as_bps(),
            3_000_000
        );
        assert_eq!(owner.probe_candidate(), Some(0));
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let active = owner.test_envelope().expect("active envelope");
        assert!(active.target_media_payload_rate > 0);
        assert!(active.target_media_payload_rate < 4_000_000);
        assert!(active.probe_permitted);
        assert_eq!(owner.next_deadline(at), Some(now));
        owner.last_probe = Some(now);
        assert_eq!(
            owner.next_deadline(at),
            now.checked_add(Duration::from_secs(1).max(active.smoothed_rtt.saturating_mul(4)))
        );
        let packet = MediaPacket::new(
            Bytes::from_static(&[0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1]),
            at.global,
            Arc::from([]),
        );
        assert_eq!(
            owner
                .admit(at, ids[0], crate::test_support::forwarded(packet, 1))
                .map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock)
        );
        for sender in ids {
            owner
                .set_assignment(
                    sender,
                    Some({
                        let (selected_bitrate, potential_bitrate) = (
                            pulsebeam_rtc::MediaPayloadBitrate::from_bps(0),
                            pulsebeam_rtc::MediaPayloadBitrate::from_bps(0),
                        );
                        pulsebeam_rtc::SenderAssignment {
                            selected_bitrate,
                            potential_bitrate,
                            ..current_assignment(&owner, sender)
                        }
                    }),
                    at,
                    false,
                )
                .expect("withdraw demand");
        }
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        let inactive = owner.test_envelope().expect("inactive envelope");
        assert_eq!(inactive.target_media_payload_rate, 0);
        assert_eq!(inactive.reference_window, active.reference_window);
        assert!(!inactive.probe_permitted);
        assert_eq!(owner.probe_candidate(), None);
        assert_eq!(owner.next_deadline(at), None);
        owner
            .set_assignment(
                ids[0],
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(0),
                        MediaPayloadBitrate::from_bps(3_000_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&owner, ids[0])
                    }
                }),
                at,
                false,
            )
            .expect("restore demand");
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        let restored = owner.test_envelope().expect("restored envelope");
        assert!(restored.target_media_payload_rate > 0);
        assert_eq!(restored.reference_window, active.reference_window);
        assert!(restored.probe_permitted);
        assert!(
            owner
                .senders
                .iter()
                .all(|sender| sender.selected_bitrate() == 0)
        );
        assert!(owner.probe_budget_available(now, PROBE_ALLOWANCE));
        assert!(!owner.probe_budget_available(now, PROBE_ALLOWANCE + 1));
    }

    #[test]
    fn idle_pacer_debt_does_not_clear_application_limited() {
        let now = Instant::now();
        let mut at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        assert!(
            owner
                .senders
                .iter()
                .any(|sender| sender.selected_bitrate() > 0)
        );
        at.monotonic += Duration::from_millis(1);
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        at.monotonic += Duration::from_millis(200);
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        assert!(owner.test_envelope().unwrap().application_limited);

        owner.pacer.commit(at.monotonic, 100, 8_000);
        assert!(!owner.pacer.eligible(at.monotonic));
        assert_eq!(owner.queued_transport_bytes, 0);
        assert!(owner.repair_requests.is_empty());
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        assert!(owner.test_envelope().unwrap().application_limited);

        owner.queued_transport_bytes = 1;
        owner.update_controller(at, None, &[], &[], Duration::ZERO, false, 0);
        assert!(!owner.test_envelope().unwrap().application_limited);
    }

    #[test]
    fn fresh_delay_blocked_media_solicits_feedback_without_bypassing_retry_interval() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender.assignment.as_mut().unwrap().delivery.max_media_age = Duration::from_millis(100);
        }
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let mut envelope = owner.test_envelope().unwrap();
        envelope.feedback_stale = false;
        envelope.probe_permitted = false;
        envelope.smoothed_rtt = Duration::from_millis(100);
        envelope.queue_delay = Duration::from_millis(400);
        owner.envelope = Some(envelope);
        assert_eq!(
            owner
                .admit(
                    at,
                    owner.senders[0].facts.id,
                    queued_media(at, 1, FrameBoundary::Complete),
                )
                .map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock)
        );
        assert!(owner.recovery_permitted(envelope));
        assert!(owner.start_probe(at, envelope));
        assert_eq!(owner.probe_remaining, 1);
        assert_eq!(owner.feedback_recovery, FeedbackRecovery::Soliciting);
        owner.finish_probe(now, true);
        assert!(!owner.start_probe(at, envelope));
        let due = TimePoint {
            monotonic: now + Duration::from_secs(1),
            ..at
        };
        assert!(owner.start_probe(due, envelope));
        assert_eq!(owner.probe_remaining, 1);
        owner.finish_probe(due.monotonic, true);

        owner.queued_transport_bytes = 1;
        assert!(!owner.recovery_permitted(envelope));
        owner.queued_transport_bytes = 0;
        owner.rtp_bytes_in_flight = 1;
        assert!(!owner.recovery_permitted(envelope));
        owner.rtp_bytes_in_flight = 0;
        owner.path_available = false;
        assert!(!owner.recovery_permitted(envelope));
        owner.path_available = true;
        let mut drained = envelope;
        drained.queue_delay = Duration::ZERO;
        assert!(!owner.recovery_permitted(drained));
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = Some(PlayoutDelay::ZERO);
        }
        assert!(owner.recovery_permitted(envelope));
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = None;
        }
        assert!(owner.recovery_permitted(envelope));
        for index in 0..owner.senders.len() {
            owner.senders[index]
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = Some(PlayoutDelay::from_millis(0, 200).unwrap());
            owner
                .set_assignment(
                    owner.senders[index].facts.id,
                    Some({
                        let (selected_bitrate, potential_bitrate) = (
                            MediaPayloadBitrate::from_bps(0),
                            MediaPayloadBitrate::from_bps(256_000),
                        );
                        pulsebeam_rtc::SenderAssignment {
                            selected_bitrate,
                            potential_bitrate,
                            ..current_assignment(&owner, owner.senders[index].facts.id)
                        }
                    }),
                    due,
                    false,
                )
                .unwrap();
        }
        assert!(!owner.recovery_permitted(envelope));
        owner
            .set_assignment(
                owner.senders[1].facts.id,
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(256_000),
                        MediaPayloadBitrate::from_bps(256_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&owner, owner.senders[1].facts.id)
                    }
                }),
                due,
                false,
            )
            .unwrap();
        assert!(owner.recovery_permitted(envelope));
    }

    #[test]
    fn stale_demand_without_queued_media_schedules_bounded_feedback_recovery() {
        for selected in [0, 1_000_000] {
            let now = Instant::now();
            let at = TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(1_000_000),
            };
            let mut owner = playout_owner(now, None);
            for index in 0..owner.senders.len() {
                owner
                    .set_assignment(
                        owner.senders[index].facts.id,
                        Some({
                            let (selected_bitrate, potential_bitrate) = (
                                MediaPayloadBitrate::from_bps(selected),
                                MediaPayloadBitrate::from_bps(1_000_000),
                            );
                            pulsebeam_rtc::SenderAssignment {
                                selected_bitrate,
                                potential_bitrate,
                                ..current_assignment(&owner, owner.senders[index].facts.id)
                            }
                        }),
                        at,
                        false,
                    )
                    .unwrap();
            }
            owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
            owner.controller.note_send(Duration::ZERO);
            owner.last_probe = Some(now);
            assert_eq!(
                owner.next_deadline(at),
                Some(now + Duration::from_millis(500))
            );
            let stale_at = TimePoint {
                monotonic: now + Duration::from_secs(1),
                ..at
            };
            owner.update_controller(stale_at, None, &[], &[], Duration::ZERO, false, 0);
            let stale = owner.test_envelope().unwrap();
            assert!(stale.feedback_stale);
            assert!(!stale.probe_permitted);
            assert_eq!(
                owner
                    .senders
                    .iter()
                    .any(|sender| sender.selected_bitrate() > 0),
                selected > 0
            );
            assert!(owner.recovery_permitted(stale));
            owner.queued_transport_bytes = 1;
            assert!(!owner.recovery_permitted(stale));
            owner.queued_transport_bytes = 0;
            assert!(owner.next_deadline(at).unwrap() <= stale_at.monotonic);
            owner.rtp_bytes_in_flight = 1;
            assert!(!owner.recovery_permitted(stale));
            owner.rtp_bytes_in_flight = 0;
            owner.feedback_recovery = FeedbackRecovery::AwaitingReport;
            owner.last_probe = Some(stale_at.monotonic);
            let sample = [FeedbackSample {
                sent_at: Duration::from_secs(1),
                received_at: Duration::from_millis(1100),
                transport_bytes: 45,
                received: true,
                newly_acked: true,
                lost: false,
                loss_origin: None,
                receiver_arrival_micros: Some(1_100_000),
                ecn: None,
            }];
            let recovered_at = TimePoint {
                monotonic: now + Duration::from_millis(1100),
                ..at
            };
            owner.update_controller(recovered_at, None, &sample, &[], Duration::ZERO, true, 0);
            let recovered = owner.test_envelope().unwrap();
            assert!(!recovered.feedback_stale);
            assert_eq!(recovered.reference_window, stale.reference_window);
            assert_eq!(owner.feedback_recovery, FeedbackRecovery::Followup);
            assert!(owner.next_deadline(at).unwrap() <= recovered_at.monotonic);
            owner.pacer.commit(recovered_at.monotonic, 100, 8_000);
            let paced = recovered_at.monotonic + Duration::from_millis(100);
            assert!(!owner.pacer.eligible(recovered_at.monotonic));
            assert_eq!(owner.next_deadline(at), Some(paced));
            assert!(owner.pacer.eligible(paced));
            owner.finish_probe(recovered_at.monotonic, true);
            assert_eq!(owner.feedback_recovery, FeedbackRecovery::Idle);
            assert!(!owner.recovery_permitted(recovered));

            for (fresh, permitted, expected_packets) in [
                (true, true, MAX_PROBE_PACKETS),
                (true, false, 1),
                (false, false, 0),
            ] {
                owner.feedback_recovery = FeedbackRecovery::Followup;
                let mut envelope = recovered;
                envelope.feedback_stale = !fresh;
                envelope.probe_permitted = permitted;
                if !fresh {
                    owner.envelope = Some(envelope);
                    assert!(owner.next_deadline(at).unwrap() > recovered_at.monotonic);
                    assert!(!owner.start_probe(recovered_at, envelope));
                    assert_eq!(owner.probe_remaining, expected_packets);
                    let due = TimePoint {
                        monotonic: recovered_at.monotonic + Duration::from_secs(1),
                        ..recovered_at
                    };
                    assert!(owner.start_probe(due, envelope));
                    assert_eq!(owner.probe_remaining, 1);
                    assert_eq!(owner.feedback_recovery, FeedbackRecovery::Soliciting);
                    owner.finish_probe(due.monotonic, true);
                    continue;
                }
                assert!(owner.start_probe(recovered_at, envelope));
                assert_eq!(owner.probe_remaining, expected_packets);
                assert_eq!(
                    owner.feedback_recovery,
                    if expected_packets == MAX_PROBE_PACKETS {
                        FeedbackRecovery::Idle
                    } else {
                        FeedbackRecovery::Followup
                    }
                );
                owner.finish_probe(recovered_at.monotonic, true);
                assert_eq!(owner.feedback_recovery, FeedbackRecovery::Idle);
                assert!(!owner.start_probe(recovered_at, envelope));
            }
        }
    }

    #[test]
    fn complete_assignments_keep_targets_independent_of_wire_playout() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        let id = owner.senders[0].facts.id;
        let mut assignment = current_assignment(&owner, id);
        assignment.selected_bitrate = MediaPayloadBitrate::from_bps(0);
        assignment.potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        owner
            .set_assignment(id, Some(assignment), at, false)
            .unwrap();
        assignment.playout_delay = Some(PlayoutDelay::from_millis(0, 500).unwrap());
        owner
            .set_assignment(id, Some(assignment), at, false)
            .unwrap();
        assert_eq!(current_assignment(&owner, id), assignment);
        assert_eq!(owner.senders[0].selected_bitrate(), 0);
        assert_eq!(owner.senders[0].potential_bitrate(), 1_000_000);
        assignment.selected_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        assignment.potential_bitrate = MediaPayloadBitrate::from_bps(3_000_000);
        owner
            .set_assignment(id, Some(assignment), at, false)
            .unwrap();
        assert_eq!(current_assignment(&owner, id), assignment);
    }

    #[test]
    fn playout_confirmation_is_sender_specific_and_fences_policy_sequence_epochs() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for extension in [12, 200] {
            let mut owner = playout_owner(now, Some(extension));
            assert_eq!(stamped_delay(&owner.senders[0]), Some(vec![0, 0xf0, 20]));
            let ssrc = owner.senders[0].ssrc;
            let rtx_ssrc = owner.senders[0].rtx_ssrc;
            owner.senders[0].next_sequence = u64::from(u16::MAX);
            owner.senders[0].note_playout_commit();
            owner.senders[0].next_sequence += 2;
            owner.confirm_playout(ssrc, 0);
            owner.confirm_playout(rtx_ssrc, 0x1_0000);
            owner.confirm_playout(ssrc, 0x1_0002);
            assert_eq!(
                owner.stats().1[0].playout_delay,
                PlayoutDelayStatus::Pending
            );
            owner.confirm_playout(ssrc, 0x1_0000);
            assert_eq!(
                owner.stats().1[0].playout_delay,
                PlayoutDelayStatus::Confirmed
            );
            assert_eq!(
                owner.stats().1[1].playout_delay,
                PlayoutDelayStatus::Pending
            );
            assert_eq!(stamped_delay(&owner.senders[0]), None);

            let mut policy = owner.senders[0].assignment.expect("bound sender");
            policy.playout_delay = Some(PlayoutDelay::from_ticks(2, 30).expect("new value"));
            owner
                .set_assignment(owner.senders[0].facts.id, Some(policy), at, false)
                .expect("policy update");
            owner.confirm_playout(ssrc, 0x1_0000);
            assert_eq!(
                owner.stats().1[0].playout_delay,
                PlayoutDelayStatus::Pending
            );
            assert_eq!(stamped_delay(&owner.senders[0]), Some(vec![0, 0x20, 30]));
            owner.senders[0].note_playout_commit();
            owner.senders[0].next_sequence += 1;
            owner.confirm_playout(ssrc, 0x1_0000);
            assert_eq!(
                owner.stats().1[0].playout_delay,
                PlayoutDelayStatus::Pending
            );
            owner.confirm_playout(ssrc, 0x1_0001);
            assert_eq!(
                owner.stats().1[0].playout_delay,
                PlayoutDelayStatus::Confirmed
            );
        }
    }

    #[test]
    fn unsignaled_explicit_policy_can_return_to_browser_defaults() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        assert_eq!(
            owner.stats().1[0].playout_delay,
            PlayoutDelayStatus::NotSignaled
        );
        let mut policy = owner.senders[0].assignment.expect("bound sender");
        policy.playout_delay = None;
        owner
            .set_assignment(
                owner.senders[0].facts.id,
                Some(policy),
                TimePoint {
                    monotonic: now,
                    global: GlobalMediaTime::from_micros(1),
                },
                false,
            )
            .expect("unsignaled reset");
        assert_eq!(
            owner.stats().1[0].playout_delay,
            PlayoutDelayStatus::Unspecified
        );
    }

    #[test]
    fn probe_allowance_survives_nonprobe_expiry() {
        let now = Instant::now();
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            pulsebeam_rtc::SenderAssignment {
                playout_delay: Some(PlayoutDelay::ZERO),
                ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
            },
            pulsebeam_rtc::SenderAssignment {
                playout_delay: Some(PlayoutDelay::ZERO),
                ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
            },
            now,
        );
        assert!(egress.probe_budget_available(now, PROBE_ALLOWANCE));
        egress.record_rtp(now, 19_000, false);
        egress.record_rtp(now + Duration::from_secs(1), PROBE_ALLOWANCE, true);
        // At t=5s the non-probe credit disappears, but the probe remains.
        assert!(!egress.probe_budget_available(now + Duration::from_secs(1), 1_000));
        assert!(!egress.probe_budget_available(now + Duration::from_secs(4), 1_000));
        assert!(egress.probe_budget_available(now + Duration::from_secs(6), PROBE_ALLOWANCE));
        egress.record_rtp(now + Duration::from_secs(6), PROBE_ALLOWANCE, true);
        egress.update_controller(
            TimePoint {
                monotonic: now + Duration::from_secs(6),
                global: GlobalMediaTime::from_micros(1),
            },
            Some((2, true)),
            &[],
            &[],
            Duration::ZERO,
            false,
            0,
        );
        assert!(!egress.probe_budget_available(now + Duration::from_secs(6), 1));
    }

    #[test]
    fn probe_history_overflow_suppresses_credit_until_evictions_age_out() {
        let now = Instant::now();
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::ZERO),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
        };
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        for _ in 0..=MAX_PROBE_ACCOUNTING {
            egress.record_rtp(now, 19, false);
        }
        assert_eq!(egress.emitted_rtp.len(), MAX_PROBE_ACCOUNTING);
        assert!(!egress.probe_budget_available(now, 1));
        assert!(egress.probe_budget_available(now + PROBE_WINDOW, PROBE_ALLOWANCE));
    }

    #[test]
    fn queued_media_and_repair_suppress_probe_only_deadlines() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for repair in [false, true] {
            for active in [false, true] {
                let mut owner = playout_owner(now, None);
                let packet = MediaPacket::new(
                    Bytes::from_static(&[0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1]),
                    at.global,
                    Arc::from([]),
                );
                owner
                    .admit(
                        at,
                        owner.senders[0].facts.id,
                        crate::test_support::forwarded(packet, 1),
                    )
                    .unwrap();
                if repair {
                    owner
                        .clear_sender_media(owner.senders[0].facts.id, false)
                        .unwrap();
                    owner.repair_requests.push_back((0, 1));
                }
                owner
                    .set_assignment(
                        owner.senders[0].facts.id,
                        Some({
                            let (selected_bitrate, potential_bitrate) = (
                                MediaPayloadBitrate::from_bps(100_000),
                                MediaPayloadBitrate::from_bps(2_000_000),
                            );
                            pulsebeam_rtc::SenderAssignment {
                                selected_bitrate,
                                potential_bitrate,
                                ..current_assignment(&owner, owner.senders[0].facts.id)
                            }
                        }),
                        at,
                        false,
                    )
                    .unwrap();
                owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
                assert!(owner.envelope.unwrap().probe_permitted);
                if active {
                    assert!(owner.start_probe(at, owner.envelope.unwrap()));
                    owner.probe_pacing_rate = 400_000;
                    owner.probe_pacer.commit_probe_cluster(now, 100, 400_000);
                }
                owner.pacer.commit(now, 100, 80_000);
                assert_eq!(
                    owner.next_deadline(at),
                    Some(now + Duration::from_millis(10))
                );
                owner.pacer.reset();
                assert!(
                    owner
                        .next_deadline(at)
                        .is_none_or(|deadline| deadline > now)
                );
                if active {
                    assert_ne!(
                        owner.next_deadline(at),
                        Some(now + Duration::from_millis(2))
                    );
                }
                owner
                    .clear_sender_media(owner.senders[0].facts.id, false)
                    .unwrap();
                assert_eq!(
                    owner.next_deadline(at),
                    Some(if active {
                        now + Duration::from_millis(2)
                    } else {
                        now
                    })
                );
            }
        }
    }

    #[test]
    fn active_source_interest_can_probe_a_small_upgrade_shortfall() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(150_000);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(166_667);
        owner.senders[0].latest_committed_global = Some(at.global);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(150_000);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let envelope = owner.envelope.unwrap();
        assert_eq!(owner.senders[0].selected_bitrate(), 150_000);
        assert!(envelope.target_media_payload_rate < 166_667);
        assert!(166_667 - envelope.target_media_payload_rate < 50_000);
        assert!(166_667 * 5 < envelope.target_media_payload_rate * 6);
        assert!(envelope.probe_permitted);
    }

    #[test]
    fn established_idle_probes_fund_native_flight_without_rearming_bootstrap() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now + PROBE_WINDOW,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for (selected, application_limited) in [(0, false), (500_000, true), (500_000, false)] {
            for reference_window in [8_598, 100_000] {
                let mut owner = playout_owner(now, None);
                owner.senders[0]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .selected_bitrate = MediaPayloadBitrate::from_bps(selected);
                owner.senders[0]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(selected);
                owner.senders[0]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(2_322_813);
                owner.senders[0].latest_committed_global = Some(at.global);
                owner.senders[1]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .selected_bitrate = MediaPayloadBitrate::from_bps(0);
                owner.senders[1]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(0);
                owner.senders[1]
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(0);
                owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
                owner.initial_probe_started_at = Some(now);
                owner.initial_probe_allowance = PROBE_ALLOWANCE * INITIAL_PROBE_CLUSTERS;
                owner.record_rtp(at.monotonic - Duration::from_secs(1), 1_000, true);
                let mut envelope = owner.envelope.unwrap();
                envelope.probe_permitted = true;
                envelope.application_limited = application_limited;
                envelope.reference_window = reference_window;
                envelope.target_media_payload_rate = 1_990_083;
                envelope.pacing_transport_rate = 4_168_081;
                envelope.max_rtp_bytes_in_flight = 25_794;
                let ordinary = owner.cluster_target_bytes(envelope.target_media_payload_rate);
                assert!(owner.start_probe(at, envelope));
                let expected = if selected == 0 || application_limited {
                    ordinary
                        .max(reference_window)
                        .min(envelope.max_rtp_bytes_in_flight)
                } else {
                    ordinary
                };
                assert_eq!(owner.probe_target_bytes, expected);
                assert_eq!(owner.initial_probe_started_at, Some(now));
                assert_eq!(owner.probe_allowance(at.monotonic), PROBE_ALLOWANCE);
                assert_eq!(owner.emitted_rtp.len(), 1);
                assert_eq!(owner.probe_remaining, MAX_PROBE_PACKETS);
                assert_eq!(
                    owner.probe_interval(at.monotonic, envelope),
                    Duration::from_secs(5)
                );
            }
        }
    }

    #[test]
    fn established_active_probes_preserve_body_service_rate() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now + PROBE_WINDOW,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for (selected, had_media) in [(0, false), (500_000, false), (0, true), (500_000, true)] {
            let mut owner = playout_owner(now, None);
            for sender in &mut owner.senders {
                sender
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .selected_bitrate = MediaPayloadBitrate::from_bps(selected);
                sender
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(selected);
                sender
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
                sender.latest_committed_global = had_media.then_some(at.global);
            }
            owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
            let mut envelope = owner.envelope.unwrap();
            envelope.probe_permitted = true;
            envelope.pacing_transport_rate = 100_000;
            assert!(owner.start_probe(at, envelope));
            if selected > 0 && had_media {
                assert_eq!(owner.probe_pacing_rate, envelope.pacing_transport_rate);
            } else {
                assert!(owner.probe_pacing_rate > envelope.pacing_transport_rate);
            }
            assert!(owner.probe_duration() <= INITIAL_PROBE_DURATION);
        }
        let mut owner = playout_owner(now, None);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(500_000);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(500_000);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(500_000);
        owner.senders[0].latest_committed_global = Some(at.global);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        owner.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let mut envelope = owner.envelope.unwrap();
        envelope.probe_permitted = true;
        envelope.pacing_transport_rate = 100_000;
        assert!(owner.start_probe(at, envelope));
        assert_eq!(owner.probe_sender, Some(1));
        assert_eq!(owner.probe_pacing_rate, envelope.pacing_transport_rate);
    }

    #[test]
    fn rtt_sized_probe_bounds_rate_and_adapts_its_finite_duration() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        }
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let mut envelope = owner.envelope.unwrap();
        envelope.probe_permitted = true;
        envelope.smoothed_rtt = Duration::from_millis(200);
        envelope.max_rtp_bytes_in_flight = 100_000;
        envelope.pacing_transport_rate = 10_000_000;
        assert!(owner.start_probe(at, envelope));
        assert!(owner.probe_target_bytes > PROBE_MIN_TARGET);
        assert_eq!(owner.probe_target_bytes, owner.probe_pacing_rate / 80);
        assert_eq!(
            owner.probe_pacing_rate,
            envelope.target_media_payload_rate * 2
        );
        assert!(owner.probe_pacing_rate < envelope.pacing_transport_rate);
        assert!(owner.probe_duration() > Duration::from_millis(20));
        assert!(owner.probe_duration() <= Duration::from_millis(100));
        assert_eq!(owner.initial_probe_started_at, Some(now));
        assert_eq!(owner.emitted_rtp.len(), 0);
        owner.probe_pacing_rate = 1;
        assert_eq!(owner.probe_duration(), Duration::from_millis(100));
        owner.probe_pacing_rate = u64::MAX;
        assert_eq!(owner.probe_duration(), Duration::from_millis(20));
    }

    #[test]
    fn never_started_source_uses_native_flight_after_initial_period() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now + PROBE_WINDOW,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
            assert_eq!(sender.latest_committed_global, None);
        }
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        owner.initial_probe_started_at = Some(now);
        let mut envelope = owner.envelope.unwrap();
        envelope.probe_permitted = true;
        envelope.smoothed_rtt = Duration::from_millis(10);
        envelope.reference_window = 8_000;
        envelope.max_rtp_bytes_in_flight = 24_000;
        envelope.target_media_payload_rate = 1_000_000;
        envelope.pacing_transport_rate = 1_500_000;
        assert!(owner.start_probe(at, envelope));
        assert_eq!(owner.probe_target_bytes, 11_000);
        assert_eq!(owner.probe_pacing_rate, 2_000_000);
        assert!(owner.probe_duration() <= INITIAL_PROBE_DURATION);
        assert_eq!(owner.initial_probe_started_at, Some(now));
        assert_eq!(owner.probe_allowance(at.monotonic), PROBE_ALLOWANCE);
    }

    #[test]
    fn probe_rtt_sample_is_governed_and_reserved_for_potential_only_demand() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        for potential_only in [false, true] {
            let mut owner = playout_owner(now, None);
            for sender in &mut owner.senders {
                sender
                    .assignment
                    .as_mut()
                    .expect("bound sender")
                    .playout_delay = Some(PlayoutDelay::ZERO);
                if potential_only {
                    sender
                        .assignment
                        .as_mut()
                        .expect("bound sender")
                        .potential_bitrate =
                        sender.assignment.expect("bound sender").selected_bitrate;
                    sender
                        .assignment
                        .as_mut()
                        .expect("bound sender")
                        .selected_bitrate = MediaPayloadBitrate::from_bps(0);
                }
            }
            owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
            let mut envelope = owner.envelope.unwrap();
            envelope.smoothed_rtt = Duration::from_millis(100);
            envelope.max_rtp_bytes_in_flight = 48 * 1024;
            assert!(owner.start_probe(at, envelope));
            if potential_only {
                let rtt_sample = u64::try_from(
                    (u128::from(owner.probe_budget_bitrate()) * envelope.smoothed_rtt.as_micros())
                        .div_ceil(8_000_000),
                )
                .unwrap();
                assert_eq!(rtt_sample, 6_400);
                assert_eq!(
                    owner.probe_target_bytes,
                    rtt_sample.min((owner.probe_pacing_rate / 80).max(PROBE_MIN_TARGET)),
                );
            } else {
                assert_eq!(owner.probe_target_bytes, PROBE_MIN_TARGET);
            }
            assert_eq!(owner.probe_remaining, MAX_PROBE_PACKETS);
        }
    }

    #[test]
    fn potential_only_flight_sample_does_not_rebootstrap_established_media() {
        let now = Instant::now();
        for initial in [false, true] {
            for had_media in [false, true] {
                for promote in [false, true] {
                    let at = TimePoint {
                        monotonic: now
                            + if initial {
                                Duration::from_millis(100)
                            } else {
                                PROBE_WINDOW
                            },
                        global: GlobalMediaTime::from_micros(1_000_000),
                    };
                    let mut owner = playout_owner(now, None);
                    for sender in &mut owner.senders {
                        sender
                            .assignment
                            .as_mut()
                            .expect("bound sender")
                            .playout_delay = None;
                        sender
                            .assignment
                            .as_mut()
                            .expect("bound sender")
                            .selected_bitrate = MediaPayloadBitrate::from_bps(0);
                        sender
                            .assignment
                            .as_mut()
                            .expect("bound sender")
                            .potential_bitrate = MediaPayloadBitrate::from_bps(0);
                        sender
                            .assignment
                            .as_mut()
                            .expect("bound sender")
                            .potential_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
                        sender.latest_committed_global = had_media.then_some(at.global);
                    }
                    owner.update_controller(
                        at,
                        Some((1, true)),
                        &[],
                        &[],
                        Duration::ZERO,
                        false,
                        0,
                    );
                    owner.initial_probe_started_at = Some(now);
                    let mut envelope = owner.envelope.unwrap();
                    envelope.feedback_stale = false;
                    envelope.probe_permitted = true;
                    envelope.target_media_payload_rate = 100_000;
                    envelope.pacing_transport_rate = 150_000;
                    envelope.smoothed_rtt = Duration::from_millis(500);
                    envelope.max_rtp_bytes_in_flight = 48 * 1024;
                    if promote {
                        owner.feedback_recovery = FeedbackRecovery::Followup;
                        owner.last_probe = Some(at.monotonic);
                    }
                    assert!(owner.start_probe(at, envelope));
                    let expected_bytes = if initial || !had_media {
                        48 * 1024
                    } else {
                        PROBE_MIN_TARGET.max(envelope.reference_window)
                    };
                    let bounded_bytes =
                        expected_bytes.min((owner.probe_pacing_rate / 80).max(PROBE_MIN_TARGET));
                    assert_eq!(owner.probe_target_bytes, bounded_bytes);
                    assert!(owner.probe_duration() <= INITIAL_PROBE_DURATION);
                    if !initial {
                        assert_eq!(owner.probe_allowance(at.monotonic), PROBE_ALLOWANCE);
                        assert_eq!(owner.initial_probe_allowance, PROBE_ALLOWANCE);
                    }
                    assert_eq!(owner.probe_pacing_rate, 300_000);
                    assert_eq!(owner.initial_probe_started_at, Some(now));
                    assert_eq!(owner.probe_remaining, MAX_PROBE_PACKETS);
                }
            }
        }
    }

    #[test]
    fn periodic_probe_retry_is_separate_from_startup_and_feedback_recovery() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let mut envelope = owner.envelope.unwrap();
        envelope.smoothed_rtt = Duration::from_millis(100);
        assert!(owner.start_probe(at, envelope));
        owner.finish_probe(now, true);
        owner.envelope = Some(envelope);
        owner.next_rtcp = now + Duration::from_secs(10);
        assert_eq!(owner.probe_interval(now, envelope), Duration::from_secs(5));
        assert_eq!(owner.next_deadline(at), Some(now + Duration::from_secs(5)));
        assert!(!owner.start_probe(
            TimePoint {
                monotonic: now + Duration::from_secs(1),
                ..at
            },
            envelope
        ));
        assert!(owner.start_probe(
            TimePoint {
                monotonic: now + Duration::from_secs(5),
                ..at
            },
            envelope
        ));
        owner.finish_probe(now + Duration::from_secs(5), true);

        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = sender.assignment.expect("bound sender").selected_bitrate;
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        }
        assert_eq!(owner.probe_interval(now, envelope), Duration::from_secs(1));
        assert_eq!(
            owner.probe_interval(now + PROBE_WINDOW, envelope),
            Duration::from_secs(5)
        );
        envelope.feedback_stale = true;
        assert_eq!(
            owner.probe_interval(now + PROBE_WINDOW, envelope),
            Duration::from_secs(1)
        );
        envelope.smoothed_rtt = Duration::from_secs(2);
        assert_eq!(
            owner.probe_interval(now + PROBE_WINDOW, envelope),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn initial_probe_service_is_bounded_and_cannot_rearm_on_path_change() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        }
        assert_eq!(owner.probe_allowance(now), PROBE_ALLOWANCE);
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        assert!(owner.start_probe(at, owner.envelope.unwrap()));
        assert_eq!(
            owner.probe_allowance(now),
            PROBE_ALLOWANCE * INITIAL_PROBE_CLUSTERS
        );
        owner.record_rtp(now, PROBE_ALLOWANCE * INITIAL_PROBE_CLUSTERS, true);
        assert!(!owner.probe_budget_available(now, 1));
        owner.update_controller(
            TimePoint {
                monotonic: now + PROBE_WINDOW,
                ..at
            },
            Some((2, true)),
            &[],
            &[],
            Duration::ZERO,
            false,
            0,
        );
        assert!(owner.start_probe(
            TimePoint {
                monotonic: now + PROBE_WINDOW,
                ..at
            },
            owner.envelope.unwrap(),
        ));
        assert_eq!(owner.initial_probe_started_at, Some(now));
        assert_eq!(owner.probe_allowance(now + PROBE_WINDOW), PROBE_ALLOWANCE);
        assert!(!owner.probe_followup);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(1);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(1);
        assert_eq!(owner.probe_allowance(now), PROBE_ALLOWANCE);
    }

    #[test]
    fn probe_cutoff_keeps_sufficient_samples_pending_for_feedback() {
        for (bytes, packets, expected) in [
            (2_399, 5, ProbeResult::Failed),
            (2_400, 4, ProbeResult::Failed),
            (2_400, 5, ProbeResult::Pending),
            (2_990, 5, ProbeResult::Pending),
        ] {
            let now = Instant::now();
            let at = TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(1_000_000),
            };
            let mut owner = playout_owner(now, None);
            owner.probe_started_at = Some(now);
            owner.probe_last_sent_at = Some(now + Duration::from_millis(19));
            owner.probe_sent_bytes = bytes;
            owner.probe_target_bytes = PROBE_MIN_TARGET;
            owner.probe_packet_count = packets;
            owner.probe_remaining = 1;
            owner.update_controller(
                TimePoint {
                    monotonic: now + Duration::from_millis(20),
                    ..at
                },
                None,
                &[],
                &[],
                Duration::ZERO,
                false,
                0,
            );
            assert_eq!(owner.probe_history.back().unwrap().result, expected);
            assert_eq!(owner.probe_remaining, 0);
            assert!(!owner.probe_followup);
            let feedback = PacketFeedback {
                sent_id: crate::sent_history::SentPacketId::for_test(1),
                ssrc: owner.senders[0].ssrc,
                committed_at: now + Duration::from_millis(5),
                transport_bytes: 600,
                service: RtpService::Padding,
                received: true,
                newly_acked: true,
                lost: false,
                loss_origin: None,
                receiver_arrival: None,
                ecn: None,
            };
            owner.observe_probe_feedback(
                now + Duration::from_millis(60),
                &[feedback; 4],
                crate::congestion::ControllerReason::Feedback,
            );
            assert_eq!(owner.probe_followup, expected == ProbeResult::Pending);
        }
    }

    #[test]
    fn probe_feedback_requires_requested_bytes_and_packet_count() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        let cluster = ProbeCluster {
            started_at: now,
            last_sent_at: now + Duration::from_millis(19),
            finished_at: now + Duration::from_millis(20),
            deadline: now + Duration::from_millis(210),
            sent_bytes: 2_990,
            target_bytes: PROBE_MIN_TARGET,
            packet_count: 5,
            received_bytes: 0,
            lost_packets: 0,
            reported_packets: 0,
            result: ProbeResult::Pending,
        };
        let sample = PacketFeedback {
            sent_id: crate::sent_history::SentPacketId::for_test(1),
            ssrc: owner.senders[0].ssrc,
            committed_at: now + Duration::from_millis(5),
            transport_bytes: 800,
            service: RtpService::Padding,
            received: true,
            newly_acked: true,
            lost: false,
            loss_origin: None,
            receiver_arrival: None,
            ecn: None,
        };
        for (bytes, packets, expected) in [
            (599, 4, ProbeResult::Pending),
            (800, 3, ProbeResult::Pending),
            (600, 4, ProbeResult::Successful),
        ] {
            owner.probe_history.clear();
            owner.probe_history.push_back(cluster);
            let feedback = vec![
                PacketFeedback {
                    transport_bytes: bytes,
                    ..sample
                };
                packets
            ];
            owner.observe_probe_feedback(
                now + Duration::from_millis(60),
                &feedback,
                crate::congestion::ControllerReason::Feedback,
            );
            assert_eq!(owner.probe_history.back().unwrap().result, expected);
        }
    }

    #[test]
    fn probe_feedback_waits_for_receipt_quorum_after_unqualified_loss() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        owner.probe_history.push_back(ProbeCluster {
            started_at: now,
            last_sent_at: now + Duration::from_millis(19),
            finished_at: now + Duration::from_millis(20),
            deadline: now + Duration::from_millis(210),
            sent_bytes: 3_000,
            target_bytes: 3_000,
            packet_count: 10,
            received_bytes: 0,
            lost_packets: 0,
            reported_packets: 0,
            result: ProbeResult::Pending,
        });
        let sample = PacketFeedback {
            sent_id: crate::sent_history::SentPacketId::for_test(1),
            ssrc: owner.senders[0].ssrc,
            committed_at: now + Duration::from_millis(5),
            transport_bytes: 300,
            service: RtpService::Padding,
            received: false,
            newly_acked: false,
            lost: true,
            loss_origin: Some(crate::congestion::LossOrigin::Reordering),
            receiver_arrival: None,
            ecn: None,
        };
        owner.observe_probe_feedback(
            now + Duration::from_millis(60),
            &[sample],
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(owner.probe_history[0].lost_packets, 1);
        assert_eq!(owner.probe_history[0].result, ProbeResult::Pending);
        let receipts: Vec<_> = (2..=9)
            .map(|id| PacketFeedback {
                sent_id: crate::sent_history::SentPacketId::for_test(id),
                received: true,
                newly_acked: true,
                lost: false,
                loss_origin: None,
                ..sample
            })
            .collect();
        owner.observe_probe_feedback(
            now + Duration::from_millis(80),
            &receipts,
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(owner.probe_history[0].lost_packets, 1);
        assert_eq!(owner.probe_history[0].received_bytes, 2_400);
        assert_eq!(owner.probe_history[0].result, ProbeResult::Successful);
    }

    #[test]
    fn mature_never_started_continuation_survives_feedback_without_new_credit() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = sender.assignment.expect("bound sender").selected_bitrate;
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        }
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        owner.initial_probe_started_at = Some(now);
        owner.initial_probe_allowance = 70_000;
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(3_000_000);
        let sent = now + Duration::from_secs(6);
        let received = sent + Duration::from_millis(50);
        let finished = sent + Duration::from_millis(20);
        owner.last_probe = Some(finished);
        owner.probe_history.push_back(ProbeCluster {
            started_at: sent,
            last_sent_at: finished,
            finished_at: finished,
            deadline: sent + Duration::from_millis(200),
            sent_bytes: PROBE_MIN_TARGET,
            target_bytes: PROBE_MIN_TARGET,
            packet_count: 10,
            received_bytes: 0,
            lost_packets: 0,
            reported_packets: 0,
            result: ProbeResult::Pending,
        });
        let feedback: Vec<_> = (1..=10)
            .map(|id| PacketFeedback {
                sent_id: crate::sent_history::SentPacketId::for_test(id),
                ssrc: owner.senders[0].ssrc,
                committed_at: sent,
                transport_bytes: 300,
                service: RtpService::Padding,
                received: true,
                newly_acked: true,
                lost: false,
                loss_origin: None,
                receiver_arrival: None,
                ecn: None,
            })
            .collect();
        let samples: Vec<_> = feedback
            .iter()
            .map(|sample| sample.controller_sample(received, now))
            .collect();
        let late = TimePoint {
            monotonic: received,
            global: at.global.checked_add(Duration::from_millis(6_050)).unwrap(),
        };
        owner.update_controller(late, None, &samples, &feedback, Duration::ZERO, true, 0);
        assert_eq!(
            owner.probe_history.back().unwrap().result,
            ProbeResult::Successful
        );
        assert!(owner.probe_followup);
        assert!(!owner.initial_probing(received));
        let allowance = owner.initial_probe_allowance;
        owner.pacer.commit(received, 1_000, 1_000_000);
        let deadline = owner.pacer.next_deadline();
        assert!(owner.start_probe(late, owner.envelope.unwrap()));
        assert!(!owner.probe_followup);
        assert_eq!(owner.initial_probe_started_at, Some(now));
        assert_eq!(owner.initial_probe_allowance, allowance);
        assert_eq!(owner.probe_allowance(received), PROBE_ALLOWANCE);
        assert_eq!(owner.pacer.next_deadline(), deadline);
        assert_eq!(owner.pacer.charged_bytes(), 1_000);
    }

    #[test]
    fn successful_initial_probe_followup_is_consumed_and_preserves_debt() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = sender.assignment.expect("bound sender").selected_bitrate;
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
        }
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        assert!(owner.start_probe(at, owner.envelope.unwrap()));
        owner.probe_started_at = Some(now);
        owner.probe_last_sent_at = Some(now + Duration::from_millis(10));
        owner.probe_sent_bytes = PROBE_MIN_TARGET;
        owner.probe_target_bytes = PROBE_MIN_TARGET;
        owner.probe_packet_count = 5;
        owner.finish_probe(now + Duration::from_millis(10), false);
        let feedback = PacketFeedback {
            sent_id: crate::sent_history::SentPacketId::for_test(1),
            ssrc: owner.senders[0].ssrc,
            committed_at: now + Duration::from_millis(5),
            transport_bytes: 600,
            service: RtpService::Padding,
            received: true,
            newly_acked: true,
            lost: false,
            loss_origin: None,
            receiver_arrival: None,
            ecn: None,
        };
        let received = now + Duration::from_millis(60);
        owner.observe_probe_feedback(
            received,
            &[feedback; 4],
            crate::congestion::ControllerReason::Feedback,
        );
        assert!(owner.probe_followup);
        owner.pacer.commit(received, 1_000, 80_000);
        let debt = owner.pacer.next_deadline();
        assert_eq!(owner.next_deadline(at), debt);
        owner.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(10_000_000);
        owner.feedback_recovery = FeedbackRecovery::Followup;
        assert!(owner.start_probe(
            TimePoint {
                monotonic: received,
                ..at
            },
            owner.envelope.unwrap()
        ));
        assert!(!owner.probe_followup);
        assert_eq!(
            owner.probe_allowance(received),
            PROBE_ALLOWANCE * INITIAL_PROBE_CLUSTERS
        );
        assert_eq!(owner.pacer.next_deadline(), debt);
        owner.finish_probe(received, true);
        owner.observe_probe_feedback(
            received,
            &[feedback],
            crate::congestion::ControllerReason::Feedback,
        );
        assert!(!owner.probe_followup);
        assert!(!owner.start_probe(
            TimePoint {
                monotonic: received,
                ..at
            },
            owner.envelope.unwrap()
        ));
    }

    #[test]
    fn probe_reserves_selected_media_pacer_horizon_without_clearing_debt() {
        let now = Instant::now();
        let mut owner = playout_owner(now, None);
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .playout_delay = Some(PlayoutDelay::from_ticks(0, 50).unwrap());
        }
        assert!(owner.probe_fits_selected_horizon(now, 800, 80_000));
        assert!(!owner.probe_fits_selected_horizon(now, 801, 80_000));
        owner.pacer.commit(now, 100, 80_000);
        assert!(owner.probe_fits_selected_horizon(now, 700, 80_000));
        assert!(!owner.probe_fits_selected_horizon(now, 701, 80_000));
        assert_eq!(
            owner.pacer.next_deadline(),
            Some(now + Duration::from_millis(10))
        );
        for sender in &mut owner.senders {
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = sender.assignment.expect("bound sender").selected_bitrate;
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .selected_bitrate = MediaPayloadBitrate::from_bps(0);
            sender
                .assignment
                .as_mut()
                .expect("bound sender")
                .potential_bitrate = MediaPayloadBitrate::from_bps(0);
        }
        assert!(owner.probe_fits_selected_horizon(now, 801, 80_000));
    }

    #[test]
    fn accelerated_probe_retains_ordinary_debt_after_abort() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut owner = playout_owner(now, None);
        owner
            .set_assignment(
                owner.senders[0].facts.id,
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(0),
                        MediaPayloadBitrate::from_bps(2_000_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&owner, owner.senders[0].facts.id)
                    }
                }),
                at,
                false,
            )
            .unwrap();
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        let mut envelope = owner.envelope.unwrap();
        envelope.pacing_transport_rate = 300_000;
        owner.envelope = Some(envelope);
        assert!(owner.start_probe(at, envelope));
        assert!(owner.probe_target_bytes <= envelope.max_rtp_bytes_in_flight);
        assert_eq!(owner.probe_target_bytes, owner.probe_pacing_rate / 80);
        assert_eq!(
            owner.probe_pacing_rate,
            envelope.target_media_payload_rate * 2
        );
        assert!(owner.probe_pacing_rate <= owner.probe_budget_bitrate() * 2);
        for _ in 0..3 {
            let sent = owner.probe_pacer.next_deadline().unwrap_or(now);
            assert!(owner.probe_pacer.eligible(sent));
            let earliest = sent.checked_sub(Duration::from_millis(1)).unwrap_or(sent);
            let probe_start = owner
                .probe_pacer
                .next_deadline()
                .map_or(sent, |deadline| deadline.max(earliest));
            let probe_interval =
                Duration::from_nanos(2_400_000_000_000_u64.div_ceil(owner.probe_pacing_rate));
            owner.pending = Some(PendingTransmit::Padding {
                sender: 0,
                ssrc: owner.senders[0].ssrc,
                sequence: wire_u16(owner.senders[0].next_sequence),
                twcc: None,
            });
            owner.commit(
                TimePoint {
                    monotonic: sent,
                    ..at
                },
                &PreparedTransmit {
                    target: crate::TransmitTarget::Udp {
                        local: "127.0.0.1:10000".parse().unwrap(),
                        remote: "127.0.0.1:10001".parse().unwrap(),
                        ecn: None,
                    },
                    wire_len: 300,
                    bytes: vec![0; 300],
                    kind: DatagramKind::Rtp,
                    path_epoch: None,
                    rtp: None,
                },
            );
            assert_eq!(
                owner.probe_pacer.next_deadline(),
                Some(probe_start + probe_interval),
            );
            assert_eq!(owner.next_deadline(at), owner.probe_pacer.next_deadline());
        }
        let media_deadline = now + Duration::from_millis(24);
        assert_eq!(owner.pacer.next_deadline(), Some(media_deadline));
        owner.finish_probe(owner.probe_pacer.next_deadline().unwrap(), true);
        assert_eq!(owner.probe_pacing_rate, 0);
        assert_eq!(owner.probe_pacer.next_deadline(), None);
        assert_eq!(owner.pacer.next_deadline(), Some(media_deadline));
        assert!(
            !owner
                .pacer
                .eligible(media_deadline - Duration::from_nanos(1))
        );
        assert!(owner.pacer.eligible(media_deadline));
        assert!(owner.probe_budget_available(now, PROBE_ALLOWANCE - 900));
        assert!(!owner.probe_budget_available(now, PROBE_ALLOWANCE - 899));
    }

    #[test]
    fn active_probe_uses_pacer_deadline_instead_of_cluster_start_deadline() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let mut egress = playout_owner(now, None);
        let sender = egress.senders[0].facts.id;
        egress
            .set_assignment(
                sender,
                Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(0),
                        MediaPayloadBitrate::from_bps(1_000_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..current_assignment(&egress, sender)
                    }
                }),
                at,
                false,
            )
            .expect("declare demand");
        egress.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        assert_eq!(egress.next_deadline(at), Some(now));

        let started = now + Duration::from_millis(350);
        egress.probe_remaining = 31;
        egress.probe_sender = Some(0);
        egress.probe_started_at = Some(started);
        egress.pacer.commit(started, 100, 800_000);
        assert_eq!(egress.last_probe, None);
        assert_eq!(
            egress.next_deadline(at),
            started.checked_add(Duration::from_millis(1))
        );

        egress.last_send_attempt = started.checked_add(Duration::from_millis(2));
        assert_eq!(
            egress.next_deadline(at),
            started.checked_add(Duration::from_millis(20)),
            "an attempted pacing deadline cannot spin while sending is blocked"
        );
        egress
            .pacer
            .commit(started + Duration::from_millis(2), 100, 800_000);
        assert_eq!(
            egress.next_deadline(at),
            started.checked_add(Duration::from_millis(3)),
            "a future pacing deadline still wakes the sender"
        );
        egress.pacer.reset();
        assert_eq!(
            egress.next_deadline(at),
            started.checked_add(Duration::from_millis(20))
        );
        egress.finish_probe(started + Duration::from_millis(20), true);
        assert!(
            egress
                .next_deadline(at)
                .is_none_or(|deadline| deadline > started)
        );
    }

    #[test]
    fn active_probe_deadline_wakes_the_connection_for_cluster_expiry() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(0),
        };
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::ZERO),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
        };
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        egress.probe_remaining = 4;
        egress.probe_started_at = Some(now);
        assert_eq!(
            egress.next_deadline(at),
            now.checked_add(Duration::from_millis(20))
        );
        egress.update_controller(
            TimePoint {
                monotonic: now + Duration::from_millis(1),
                global: GlobalMediaTime::from_micros(1),
            },
            Some((1, true)),
            &[],
            &[],
            Duration::ZERO,
            false,
            0,
        );
        assert_eq!(egress.probe_started_at, None, "path reset cancels cluster");

        egress.probe_remaining = 4;
        egress.probe_started_at = Some(now);
        egress.probe_last_sent_at = Some(now);
        egress.probe_sent_bytes = 100;
        let deadline = now + Duration::from_millis(20);
        egress.update_controller(
            TimePoint {
                monotonic: deadline,
                global: GlobalMediaTime::from_micros(20_000),
            },
            None,
            &[],
            &[],
            Duration::ZERO,
            false,
            u64::MAX,
        );
        assert_eq!(
            egress.probe_started_at, None,
            "window blockage cannot defer expiry"
        );
        assert_eq!(egress.probe_remaining, 0);
        assert_eq!(
            egress.probe_history.back().expect("cluster").result,
            ProbeResult::Failed
        );
        assert!(egress.next_deadline(at).is_none_or(|next| next > deadline));
    }

    #[test]
    fn probe_uses_declared_unmet_source_interest_and_bounded_target() {
        let now = Instant::now();
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::from_ticks(0, 50).expect("500 ms")),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(1_000_000))
        };
        let sender = |id| EgressSenderFacts {
            id: SenderId::new(id).expect("sender"),
            kind: MediaKind::Audio,
            mid: format!("audio{id}").into(),
            payload_type: 111,
            retransmission_payload_type: None,
            clock_rate: 48_000,
            mid_extension_id: None,
            twcc_extension_id: Some(3),
            playout_extension_id: None,
            dependency_extension_id: None,
            forwarded_extensions: Box::new([]),
        };
        let mut egress = bound_egress(
            vec![sender(1), sender(2)].into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        egress.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(500_000);
        egress.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(500_000);
        assert_eq!(egress.probe_queue_impact_limit(), Duration::from_millis(10));
        egress.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = Some(PlayoutDelay::ZERO);
        assert_eq!(egress.probe_queue_impact_limit(), Duration::from_millis(10));
        egress.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .playout_delay = policy.playout_delay;
        assert_eq!(egress.cluster_target_bytes(100_000), 750);
        assert_eq!(egress.cluster_target_bytes(1_000_000), 3_750);
        egress.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .selected_bitrate = MediaPayloadBitrate::from_bps(u64::MAX);
        egress.senders[0]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(u64::MAX);
        assert_eq!(egress.cluster_target_bytes(u64::MAX), 48 * 1024);
        egress.senders[1]
            .assignment
            .as_mut()
            .expect("bound sender")
            .potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        assert_eq!(egress.probe_candidate(), Some(1));
    }

    #[test]
    fn probe_success_requires_its_own_received_bytes_and_fails_on_loss_or_timeout() {
        let now = Instant::now();
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::ZERO),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
        };
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        let cluster = ProbeCluster {
            started_at: now,
            last_sent_at: now + Duration::from_millis(10),
            finished_at: now + Duration::from_millis(10),
            deadline: now + Duration::from_millis(210),
            sent_bytes: 1_000,
            target_bytes: 1_000,
            packet_count: 1,
            received_bytes: 0,
            lost_packets: 0,
            reported_packets: 0,
            result: ProbeResult::Pending,
        };
        let sample = PacketFeedback {
            sent_id: crate::sent_history::SentPacketId::for_test(1),
            ssrc: 7,
            committed_at: now + Duration::from_millis(5),
            transport_bytes: 800,
            service: RtpService::Original,
            received: true,
            newly_acked: true,
            lost: false,
            loss_origin: None,
            receiver_arrival: None,
            ecn: None,
        };
        egress.probe_history.push_back(cluster);
        egress.observe_probe_feedback(
            now + Duration::from_millis(100),
            &[sample],
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].result, ProbeResult::Pending);
        for _ in 0..2 {
            egress.observe_probe_feedback(
                now + Duration::from_millis(105),
                &[PacketFeedback {
                    service: RtpService::Padding,
                    received: false,
                    newly_acked: false,
                    ..sample
                }],
                crate::congestion::ControllerReason::Feedback,
            );
        }
        assert_eq!(egress.probe_history[0].received_bytes, 0);
        assert_eq!(egress.probe_history[0].result, ProbeResult::Pending);
        egress.observe_probe_feedback(
            now + Duration::from_millis(110),
            &[PacketFeedback {
                service: RtpService::Padding,
                ..sample
            }],
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].result, ProbeResult::Successful);
        egress.probe_history[0] = cluster;
        egress.observe_probe_feedback(
            now + Duration::from_millis(110),
            &[PacketFeedback {
                service: RtpService::Padding,
                received: false,
                newly_acked: false,
                lost: true,
                loss_origin: Some(crate::congestion::LossOrigin::Reordering),
                ..sample
            }],
            crate::congestion::ControllerReason::Loss,
        );
        assert_eq!(egress.probe_history[0].result, ProbeResult::Failed);
        egress.probe_history[0] = cluster;
        egress.observe_probe_feedback(
            cluster.deadline,
            &[],
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].result, ProbeResult::Failed);
        egress.probe_history[0] = cluster;
        egress.observe_probe_feedback(
            cluster.deadline,
            &[PacketFeedback {
                service: RtpService::Padding,
                ..sample
            }],
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].result, ProbeResult::Failed);
    }

    #[test]
    fn reordered_probe_receipts_count_once_without_new_ack_edge_credit() {
        use crate::{
            PacketFeedbackKind,
            connection::TransmitCommitContext,
            rtcp::{FeedbackBatch, FeedbackReport, TwccStatus},
            sent_history::SentHistory,
            transport::{DatagramKind, PathEpoch, PreparedRtpIdentity},
        };

        let now = Instant::now();
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::ZERO),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(0))
        };
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        egress.probe_history.push_back(ProbeCluster {
            started_at: now,
            last_sent_at: now + Duration::from_millis(6),
            finished_at: now + Duration::from_millis(6),
            deadline: now + Duration::from_millis(200),
            sent_bytes: 1_200,
            target_bytes: 1_200,
            packet_count: 6,
            received_bytes: 0,
            lost_packets: 0,
            reported_packets: 0,
            result: ProbeResult::Pending,
        });
        let epoch = PathEpoch::from_value(1);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        for sequence in 1..=6 {
            let context = TransmitCommitContext {
                at: now + Duration::from_millis(u64::from(sequence)),
                kind: DatagramKind::Rtp,
                wire_len: 200,
                path_epoch: Some(epoch),
                rtp: Some(PreparedRtpIdentity {
                    ssrc: 7,
                    sequence,
                    twcc_sequence: Some(sequence),
                    service: RtpService::Padding,
                }),
            };
            history.preflight_commit(context).unwrap();
            history.commit_preflighted(context);
        }
        let report = |millis, feedback_count, statuses: Vec<TwccStatus>| FeedbackBatch {
            received_at: TimePoint {
                monotonic: now + Duration::from_millis(millis),
                global: GlobalMediaTime::from_micros(1),
            },
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: 1,
                reference_time: 0,
                feedback_count,
                statuses: statuses.into(),
            },
        };
        let mut statuses = vec![TwccStatus::NotReceived; 5];
        statuses.push(TwccStatus::Received { delta_250us: 1 });
        history.process_feedback(report(10, 1, statuses));
        egress.observe_probe_feedback(
            now + Duration::from_millis(10),
            &history.controller_inputs().feedback,
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].received_bytes, 200);
        assert_eq!(egress.probe_history[0].result, ProbeResult::Pending);
        history.clear_controller_inputs();
        let receipts = vec![TwccStatus::Received { delta_250us: 1 }; 4];
        history.process_feedback(report(20, 2, receipts.clone()));
        assert_eq!(history.controller_inputs().feedback.len(), 4);
        assert!(
            history
                .controller_inputs()
                .feedback
                .iter()
                .all(|sample| { sample.received && !sample.newly_acked && !sample.lost })
        );
        egress.observe_probe_feedback(
            now + Duration::from_millis(20),
            &history.controller_inputs().feedback,
            crate::congestion::ControllerReason::Feedback,
        );
        assert_eq!(egress.probe_history[0].received_bytes, 1_000);
        assert_eq!(egress.probe_history[0].reported_packets, 5);
        assert_eq!(egress.probe_history[0].result, ProbeResult::Successful);
        history.clear_controller_inputs();
        history.process_feedback(report(21, 2, receipts));
        assert!(history.controller_inputs().feedback.is_empty());
    }

    #[test]
    fn send_window_reserves_the_complete_transport_packet() {
        let now = Instant::now();
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::from_ticks(0, 50).expect("500 ms")),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(2_000_000))
        };
        let mut egress = bound_egress(
            Vec::new().into_boxed_slice(),
            &[0; 32],
            1,
            1,
            policy,
            policy,
            now,
        );
        egress.envelope = Some(egress.controller.snapshot(2_000_000, 0, 0));
        let window = egress.envelope.expect("envelope").max_rtp_bytes_in_flight;
        let window = usize::try_from(window).expect("bounded window");
        assert!(window > RTP_TRANSPORT_ALLOWANCE);
        assert!(egress.fits_send_window(0, window - RTP_TRANSPORT_ALLOWANCE));
        assert!(!egress.fits_send_window(1, window - RTP_TRANSPORT_ALLOWANCE));
        assert!(!egress.fits_send_window(u64::try_from(window - 1).unwrap(), 1));
    }

    #[test]
    fn admission_reaches_exact_paced_packet_bound_under_a_feasible_envelope() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let policy = pulsebeam_rtc::SenderAssignment {
            playout_delay: Some(PlayoutDelay::from_ticks(0, 50).expect("500 ms playout")),
            ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(100_000_000))
        };
        let sender = SenderId::new(1).expect("sender");
        let mut owner = bound_egress(
            vec![EgressSenderFacts {
                id: sender,
                kind: MediaKind::Audio,
                mid: "audio".into(),
                payload_type: 111,
                retransmission_payload_type: None,
                clock_rate: 48_000,
                mid_extension_id: None,
                twcc_extension_id: Some(3),
                playout_extension_id: None,
                dependency_extension_id: None,
                forwarded_extensions: Box::new([]),
            }]
            .into_boxed_slice(),
            &[7; 32],
            usize::MAX,
            usize::MAX,
            policy,
            policy,
            now,
        );
        owner.update_controller(at, Some((1, true)), &[], &[], Duration::ZERO, false, 0);
        owner
            .envelope
            .as_mut()
            .expect("controller envelope")
            .pacing_transport_rate = 100_000_000;

        for id in 1..=MAX_QUEUED_MEDIA_PACKETS {
            assert_eq!(
                owner
                    .admit(at, sender, media(u64::try_from(id).expect("bounded id")))
                    .map_err(|(reason, _)| reason),
                Ok(())
            );
        }
        assert_eq!(
            owner
                .admit(at, sender, media(9_000))
                .map_err(|(reason, _)| reason),
            Err(CommandError::WouldBlock)
        );
    }

    fn media(id: u64) -> ForwardedMedia {
        ForwardedMedia {
            source: pulsebeam_rtc::SourceIncarnation::from_value(1),
            packet: MediaPacket::new(
                Bytes::from_static(&[0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7]),
                GlobalMediaTime::from_micros(1_000_000),
                Arc::from([]),
            ),
            frame: FrameMetadata {
                descriptor: None,
                id: FrameId::from_value(id),
                boundary: FrameBoundary::Complete,
                random_access: true,
                discardable: false,
                dependencies: FrameDependencies::Known(Arc::from([])),
            },
        }
    }
}
