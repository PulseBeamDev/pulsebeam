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
    AllocationSnapshot, CommandError, Event, ForwardedMedia, FrameBoundary, FrameDependencies,
    FrameId, GlobalMediaTime, MediaKind, MediaPayloadBitrate, SenderAllocation, SenderId,
    SenderPolicy, SenderStats, TimePoint,
    allocator::{AllocationInput, weighted_max_min},
    congestion::{
        ControllerInput, EcnValidation, FeedbackSample, LatencyGovernor, SafeRtpEnvelope,
        ScreamController,
    },
    negotiation::EgressSenderFacts,
    pacer::Pacer,
    packet::RtpPacket,
    scheduler::SenderScheduler,
    transport::{
        DatagramKind, PreparedRtpIdentity, PreparedTransmit, RtpService, Transport, TransportError,
    },
};

const MAX_QUEUED_MEDIA_PACKETS: usize = 8_192;
const MAX_PACED_TRANSPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXPIRATIONS_PER_POLL: usize = 256;
const RTP_TRANSPORT_ALLOWANCE: usize = 96;

pub(crate) struct MediaEgress {
    senders: Box<[Sender]>,
    queue: VecDeque<QueuedMedia>,
    queued_frames: BTreeSet<(SenderId, FrameId)>,
    queued_payload_bytes: usize,
    queued_transport_bytes: usize,
    max_payload_bytes: usize,
    max_retransmission_bytes: usize,
    retained: VecDeque<RetainedPacket>,
    retained_bytes: usize,
    repair_requests: VecDeque<(usize, u16)>,
    next_twcc: u64,
    pending: Option<PendingTransmit>,
    pacer: Pacer,
    scheduler: SenderScheduler,
    controller: ScreamController,
    controller_origin: Instant,
    envelope: Option<SafeRtpEnvelope>,
    active_path: Option<u64>,
    path_available: bool,
    allocations: Vec<u64>,
    events: VecDeque<Event>,
    next_rtcp: Instant,
    rtcp_cursor: usize,
    probe_remaining: u8,
    last_probe: Option<Instant>,
    probe_history: VecDeque<Instant>,
}

struct Sender {
    facts: EgressSenderFacts,
    policy: SenderPolicy,
    ssrc: u32,
    rtx_ssrc: u32,
    next_sequence: u64,
    next_rtx_sequence: u64,
    base_timestamp: u32,
    base_global: Option<GlobalMediaTime>,
    latest_admitted_global: Option<GlobalMediaTime>,
    latest_committed_global: Option<GlobalMediaTime>,
    open_frame: Option<OpenFrame>,
    started_frames: BTreeSet<FrameId>,
    dependency_chain_valid: bool,
    committed_packets: u64,
    committed_payload_bytes: u64,
}

#[derive(Clone)]
struct OpenFrame {
    id: FrameId,
    global: GlobalMediaTime,
    random_access: bool,
    discardable: bool,
    dependencies: FrameDependencies,
}

struct QueuedMedia {
    sender: usize,
    media: ForwardedMedia,
    payload_bytes: usize,
    transport_estimate: usize,
}

struct RetainedPacket {
    sender: usize,
    sequence: u16,
    timestamp: u32,
    payload: Vec<u8>,
    retained_at: Instant,
    global: GlobalMediaTime,
    transport_bytes: usize,
}

enum PendingTransmit {
    Original {
        queue_index: usize,
        sender: usize,
        sequence: u16,
        timestamp: u32,
        twcc: Option<u16>,
        packet: Vec<u8>,
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
    pub(crate) target_media_bitrate: u64,
    pub(crate) pacing_bitrate: u64,
}

impl MediaEgress {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        facts: Box<[EgressSenderFacts]>,
        randomness: &[u8; 32],
        max_payload_bytes: usize,
        max_retransmission_bytes: usize,
        audio_policy: SenderPolicy,
        video_policy: SenderPolicy,
        accepted_at: Instant,
    ) -> Self {
        let senders: Box<[_]> = facts
            .into_vec()
            .into_iter()
            .map(|facts| {
                let seed = sender_seed(randomness, facts.id.value());
                Sender {
                    policy: match facts.kind {
                        MediaKind::Audio => audio_policy,
                        MediaKind::Video => video_policy,
                    },
                    ssrc: nonzero_u32([seed[0], seed[1], seed[2], seed[3]]),
                    rtx_ssrc: nonzero_u32([seed[10], seed[11], seed[12], seed[13]]),
                    next_sequence: u64::from(u16::from_be_bytes([seed[4], seed[5]])),
                    next_rtx_sequence: u64::from(u16::from_be_bytes([seed[14], seed[15]])),
                    base_timestamp: u32::from_be_bytes([seed[6], seed[7], seed[8], seed[9]]),
                    base_global: None,
                    latest_admitted_global: None,
                    latest_committed_global: None,
                    open_frame: None,
                    started_frames: BTreeSet::new(),
                    dependency_chain_valid: true,
                    committed_packets: 0,
                    committed_payload_bytes: 0,
                    facts,
                }
            })
            .collect();
        let desired = senders
            .iter()
            .map(|sender| sender.policy.desired_bitrate.as_bps())
            .sum();
        let twcc_seed = sender_seed(randomness, u16::MAX);
        let sender_count = senders.len();
        let mut owner = Self {
            senders,
            queue: VecDeque::new(),
            queued_frames: BTreeSet::new(),
            queued_payload_bytes: 0,
            queued_transport_bytes: 0,
            max_payload_bytes,
            max_retransmission_bytes,
            retained: VecDeque::new(),
            retained_bytes: 0,
            repair_requests: VecDeque::new(),
            next_twcc: u64::from(u16::from_be_bytes([twcc_seed[0], twcc_seed[1]])),
            pending: None,
            pacer: Pacer::default(),
            scheduler: SenderScheduler::new(sender_count),
            controller: ScreamController::new(desired, None),
            controller_origin: accepted_at,
            envelope: None,
            active_path: None,
            path_available: false,
            allocations: vec![0; sender_count],
            events: VecDeque::new(),
            next_rtcp: accepted_at
                .checked_add(Duration::from_secs(1))
                .unwrap_or(accepted_at),
            rtcp_cursor: 0,
            probe_remaining: 0,
            last_probe: None,
            probe_history: VecDeque::with_capacity(64),
        };
        owner.reallocate(300_000_u64.min(desired));
        owner.events.clear();
        owner
    }

    pub(crate) fn set_policy(
        &mut self,
        sender: SenderId,
        policy: SenderPolicy,
        at: TimePoint,
    ) -> Result<(), CommandError> {
        let index = self.sender_index(sender)?;
        self.senders[index].policy = policy;
        self.scheduler.policy_changed(index);
        self.expire(at, MAX_EXPIRATIONS_PER_POLL);
        let capacity = self
            .envelope
            .map_or(300_000, |envelope| envelope.target_media_payload_rate);
        self.reallocate(capacity);
        Ok(())
    }

    pub(crate) fn admit(
        &mut self,
        at: TimePoint,
        sender: SenderId,
        media: ForwardedMedia,
    ) -> Result<(), CommandError> {
        let sender_index = self.sender_index(sender)?;
        let packet = RtpPacket::parse(media.packet.bytes())
            .map_err(|_| CommandError::InvalidFrameMetadata)?;
        let payload_bytes = packet.payload().len();
        let transport_estimate = media
            .packet
            .bytes()
            .len()
            .saturating_add(RTP_TRANSPORT_ALLOWANCE);
        self.validate_frame(sender_index, &media)?;
        let pacer_wait = self.predicted_pacer_wait(at.monotonic);
        if self.senders[sender_index].policy.desired_bitrate.as_bps() == 0
            || !self.useful_at(sender_index, media.packet.global_media_at(), at, pacer_wait)
        {
            return Err(CommandError::WouldBlock);
        }
        if self.queue.len() >= MAX_QUEUED_MEDIA_PACKETS
            || payload_bytes
                > self
                    .max_payload_bytes
                    .saturating_sub(self.queued_payload_bytes)
            || transport_estimate
                > MAX_PACED_TRANSPORT_BYTES.saturating_sub(self.queued_transport_bytes)
        {
            return Err(CommandError::WouldBlock);
        }

        self.apply_frame_admission(sender_index, &media);
        self.probe_remaining = 0;
        self.senders[sender_index].latest_admitted_global = Some(media.packet.global_media_at());
        self.queued_payload_bytes = self.queued_payload_bytes.saturating_add(payload_bytes);
        self.queued_transport_bytes = self
            .queued_transport_bytes
            .saturating_add(transport_estimate);
        self.queued_frames.insert((sender, media.frame.id));
        self.queue.push_back(QueuedMedia {
            sender: sender_index,
            media,
            payload_bytes,
            transport_estimate,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_controller(
        &mut self,
        at: TimePoint,
        path_change: Option<(u64, bool)>,
        feedback: &[FeedbackSample],
        feedback_hold: Duration,
        bytes_in_flight: u64,
    ) -> bool {
        if let Some((epoch, available)) = path_change {
            self.active_path = available.then_some(epoch);
            self.path_available = available;
        }
        let desired = self
            .senders
            .iter()
            .map(|sender| sender.policy.desired_bitrate.as_bps())
            .sum();
        let offered = if self.queue.is_empty() { 0 } else { desired };
        let points = self
            .senders
            .iter()
            .filter(|sender| sender.policy.desired_bitrate.as_bps() > 0)
            .map(|sender| {
                LatencyGovernor::operating_point(
                    playout_max_ticks(sender.policy),
                    sender.policy.desired_bitrate.as_bps(),
                )
            })
            .collect::<Vec<_>>();
        let queue_ceiling = LatencyGovernor::strictest_queue_ceiling(points.iter())
            .unwrap_or(Duration::from_millis(15));
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
                bytes_in_flight,
                paced_queue_bytes: self.queued_transport_bytes as u64,
                offered_media_rate: offered,
                admitted_media_rate: offered,
                desired_media_rate: desired,
                window_or_pacer_blocked: !self.pacer.eligible(at.monotonic),
                queue_delay_ceiling: queue_ceiling,
                ecn: EcnValidation::default(),
            },
        );
        self.envelope = Some(output);
        self.reallocate(output.target_media_payload_rate);
        output.application_limited
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
        {
            self.repair_requests.push_back((sender, sequence));
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub(crate) fn abort(&mut self) {
        self.begin_shutdown();
        self.events.clear();
    }

    pub(crate) fn begin_shutdown(&mut self) {
        self.queue.clear();
        self.queued_frames.clear();
        self.queued_payload_bytes = 0;
        self.queued_transport_bytes = 0;
        self.retained.clear();
        self.retained_bytes = 0;
        self.repair_requests.clear();
        self.pending = None;
        self.probe_remaining = 0;
    }

    pub(crate) fn stats(&self) -> (EgressStats, Vec<SenderStats>) {
        let senders = self
            .senders
            .iter()
            .enumerate()
            .map(|(index, sender)| {
                let (queued_packets, queued_payload_bytes) = self
                    .queue
                    .iter()
                    .filter(|queued| queued.sender == index)
                    .fold((0_usize, 0_usize), |(packets, bytes), queued| {
                        (
                            packets.saturating_add(1),
                            bytes.saturating_add(queued.payload_bytes),
                        )
                    });
                SenderStats {
                    sender: sender.facts.id,
                    policy: sender.policy,
                    allocation: MediaPayloadBitrate::from_bps(
                        self.allocations.get(index).copied().unwrap_or_default(),
                    ),
                    queued_packets,
                    queued_payload_bytes,
                    transmitted_packets: sender.committed_packets,
                    transmitted_payload_bytes: sender.committed_payload_bytes,
                }
            })
            .collect();
        (
            EgressStats {
                queued_transport_bytes: self.queued_transport_bytes,
                target_media_bitrate: self
                    .envelope
                    .map_or(0, |envelope| envelope.target_media_payload_rate),
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
    ) -> PrepareResult {
        if self.pending.is_some() {
            return PrepareResult::Fatal;
        }
        self.expire(at, MAX_EXPIRATIONS_PER_POLL);
        self.expire_repair(at.monotonic);
        if at.monotonic >= self.next_rtcp
            && let Some(result) = self.prepare_rtcp(at, transport)
        {
            return result;
        }
        if !self.pacer.eligible(at.monotonic) {
            return PrepareResult::Blocked;
        }
        if let Some(envelope) = self.envelope
            && bytes_in_flight >= envelope.max_rtp_bytes_in_flight
        {
            return PrepareResult::Blocked;
        }
        if let Some(result) = self.prepare_repair(at, transport) {
            return result;
        }

        let eligible = self
            .senders
            .iter()
            .enumerate()
            .map(|(sender, _)| {
                self.queue
                    .iter()
                    .find(|queued| queued.sender == sender)
                    .map(|queued| queued.payload_bytes)
            })
            .collect::<Vec<_>>();
        let Some(sender_index) = self.scheduler.select(&eligible, &self.allocations) else {
            return self.prepare_padding(at, transport);
        };
        let Some(queue_index) = self
            .queue
            .iter()
            .position(|queued| queued.sender == sender_index)
        else {
            return PrepareResult::Fatal;
        };
        let Some(queued) = self.queue.get(queue_index) else {
            return PrepareResult::Fatal;
        };
        let Some(sender) = self.senders.get(sender_index) else {
            return PrepareResult::Fatal;
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
        let Some(packet) = build_rtp(
            sender,
            source.marker(),
            sequence,
            timestamp,
            twcc,
            source.payload(),
            sender.facts.payload_type,
            sender.ssrc,
            false,
        ) else {
            return PrepareResult::Fatal;
        };
        match transport.send_rtp_with_service(&packet, RtpService::Original) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Original {
                    queue_index,
                    sender: sender_index,
                    sequence,
                    timestamp,
                    twcc,
                    packet,
                });
                PrepareResult::Prepared
            }
            Err(TransportError::QueueFull | TransportError::Protocol) => PrepareResult::Blocked,
            Err(_) => PrepareResult::Fatal,
        }
    }

    pub(crate) fn preflight_commit(&self, prepared: &PreparedTransmit) -> bool {
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
                sender,
                sequence,
                twcc,
            }) => {
                let state = &self.senders[*sender];
                prepared.rtp.is_some_and(|identity| {
                    identity
                        == PreparedRtpIdentity {
                            ssrc: state.ssrc,
                            sequence: *sequence,
                            twcc_sequence: *twcc,
                            service: RtpService::Padding,
                        }
                })
            }
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
        match pending {
            PendingTransmit::Original {
                queue_index,
                sender,
                sequence,
                timestamp,
                twcc,
                packet,
            } => {
                let Some(queued) = self.queue.remove(queue_index) else {
                    return;
                };
                let state = &mut self.senders[sender];
                state
                    .base_global
                    .get_or_insert_with(|| queued.media.packet.global_media_at());
                state.latest_committed_global = Some(queued.media.packet.global_media_at());
                state.next_sequence = state.next_sequence.saturating_add(1);
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                state.committed_packets = state.committed_packets.saturating_add(1);
                state.committed_payload_bytes = state
                    .committed_payload_bytes
                    .saturating_add(queued.payload_bytes as u64);
                match queued.media.frame.boundary {
                    FrameBoundary::Start => {
                        state.started_frames.insert(queued.media.frame.id);
                    }
                    FrameBoundary::Complete | FrameBoundary::End => {
                        state.started_frames.remove(&queued.media.frame.id);
                    }
                    FrameBoundary::Middle => {}
                }
                self.queued_payload_bytes = self
                    .queued_payload_bytes
                    .saturating_sub(queued.payload_bytes);
                self.queued_transport_bytes = self
                    .queued_transport_bytes
                    .saturating_sub(queued.transport_estimate);
                self.remove_frame_if_empty(sender, queued.media.frame.id);
                self.scheduler.commit(sender, queued.payload_bytes);
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
            PendingTransmit::Padding { sender, twcc, .. } => {
                self.senders[sender].next_sequence =
                    self.senders[sender].next_sequence.saturating_add(1);
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                self.probe_remaining = self.probe_remaining.saturating_sub(1);
                if self.probe_remaining == 0 {
                    self.last_probe = Some(at.monotonic);
                    if self.probe_history.len() == 64 {
                        self.probe_history.pop_front();
                    }
                    self.probe_history.push_back(at.monotonic);
                }
            }
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let probe_deadline = self.envelope.and_then(|envelope| {
            (envelope.probe_permitted
                && self
                    .senders
                    .iter()
                    .any(|sender| sender.policy.desired_bitrate.as_bps() > 0))
            .then(|| {
                self.last_probe.map_or(self.controller_origin, |last| {
                    last.checked_add(
                        Duration::from_secs(1).max(envelope.smoothed_rtt.saturating_mul(4)),
                    )
                    .unwrap_or(last)
                })
            })
        });
        [
            (!self.queue.is_empty()
                || !self.repair_requests.is_empty()
                || self.probe_remaining > 0)
                .then(|| self.pacer.next_deadline())
                .flatten(),
            self.senders
                .iter()
                .any(|sender| sender.committed_packets > 0)
                .then_some(self.next_rtcp),
            probe_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
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
        sender_index: usize,
        media: &ForwardedMedia,
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
        let metadata_matches = |open: &OpenFrame| {
            open.id == media.frame.id
                && open.global == media.packet.global_media_at()
                && open.random_access == media.frame.random_access
                && open.discardable == media.frame.discardable
                && open.dependencies == media.frame.dependencies
        };
        match (media.frame.boundary, sender.open_frame.as_ref()) {
            (FrameBoundary::Complete | FrameBoundary::Start, None) => {}
            (FrameBoundary::Middle | FrameBoundary::End, Some(open)) if metadata_matches(open) => {}
            _ => return Err(CommandError::InvalidFrameMetadata),
        }
        if matches!(
            media.frame.boundary,
            FrameBoundary::Complete | FrameBoundary::Start
        ) && self
            .queued_frames
            .contains(&(sender.facts.id, media.frame.id))
        {
            return Err(CommandError::InvalidFrameMetadata);
        }
        if !sender.dependency_chain_valid
            && !media.frame.random_access
            && !matches!(&media.frame.dependencies, FrameDependencies::Known(value) if value.is_empty())
        {
            return Err(CommandError::WouldBlock);
        }
        Ok(())
    }

    fn apply_frame_admission(&mut self, sender: usize, media: &ForwardedMedia) {
        let state = &mut self.senders[sender];
        if media.frame.random_access
            || matches!(&media.frame.dependencies, FrameDependencies::Known(value) if value.is_empty())
        {
            state.dependency_chain_valid = true;
        }
        match media.frame.boundary {
            FrameBoundary::Start => {
                state.open_frame = Some(OpenFrame {
                    id: media.frame.id,
                    global: media.packet.global_media_at(),
                    random_access: media.frame.random_access,
                    discardable: media.frame.discardable,
                    dependencies: media.frame.dependencies.clone(),
                });
            }
            FrameBoundary::End => state.open_frame = None,
            FrameBoundary::Complete | FrameBoundary::Middle => {}
        }
    }

    fn useful_at(
        &self,
        sender: usize,
        global: GlobalMediaTime,
        at: TimePoint,
        pacer_wait: Duration,
    ) -> bool {
        let state = &self.senders[sender];
        let point = LatencyGovernor::operating_point(
            playout_max_ticks(state.policy),
            state.policy.desired_bitrate.as_bps(),
        );
        if pacer_wait > point.pacer_horizon {
            return false;
        }
        if state.policy.playout_delay.max().is_zero() {
            let age_limit = match state.facts.kind {
                MediaKind::Audio => Duration::from_millis(100),
                MediaKind::Video => Duration::from_millis(150),
            };
            return at
                .global
                .checked_duration_since(global)
                .is_none_or(|age| age <= age_limit);
        }
        let processing = match state.facts.kind {
            MediaKind::Audio => Duration::from_millis(10),
            MediaKind::Video => Duration::from_millis(25),
        };
        let Some(latest) = global
            .checked_add(state.policy.playout_delay.max())
            .and_then(|value| value.checked_sub(processing))
        else {
            return false;
        };
        let network = self.envelope.map_or(Duration::from_millis(50), |envelope| {
            envelope.smoothed_rtt / 2 + envelope.queue_delay + Duration::from_millis(5)
        });
        at.global
            .checked_add(pacer_wait.saturating_add(network))
            .is_some_and(|arrival| arrival <= latest)
    }

    fn expire(&mut self, at: TimePoint, limit: usize) {
        let mut work = 0;
        while work < limit {
            let Some((index, sender, frame, started)) =
                self.queue.iter().enumerate().find_map(|(index, queued)| {
                    (!self.useful_at(
                        queued.sender,
                        queued.media.packet.global_media_at(),
                        at,
                        Duration::ZERO,
                    ))
                    .then(|| {
                        (
                            index,
                            queued.sender,
                            queued.media.frame.id,
                            self.senders[queued.sender]
                                .started_frames
                                .contains(&queued.media.frame.id),
                        )
                    })
                })
            else {
                break;
            };
            if started {
                self.drop_queued(index);
                self.senders[sender].dependency_chain_valid = false;
                if self.senders[sender]
                    .open_frame
                    .as_ref()
                    .is_some_and(|open| open.id == frame)
                {
                    self.senders[sender].open_frame = None;
                }
                work += 1;
            } else {
                let indexes = self
                    .queue
                    .iter()
                    .enumerate()
                    .filter_map(|(index, queued)| {
                        (queued.sender == sender && queued.media.frame.id == frame).then_some(index)
                    })
                    .collect::<Vec<_>>();
                for index in indexes.into_iter().rev() {
                    self.drop_queued(index);
                    work += 1;
                    if work >= limit {
                        break;
                    }
                }
                self.senders[sender].dependency_chain_valid = false;
                if self.senders[sender]
                    .open_frame
                    .as_ref()
                    .is_some_and(|open| open.id == frame)
                {
                    self.senders[sender].open_frame = None;
                }
            }
        }
    }

    fn drop_queued(&mut self, index: usize) {
        if let Some(queued) = self.queue.remove(index) {
            self.queued_payload_bytes = self
                .queued_payload_bytes
                .saturating_sub(queued.payload_bytes);
            self.queued_transport_bytes = self
                .queued_transport_bytes
                .saturating_sub(queued.transport_estimate);
            self.remove_frame_if_empty(queued.sender, queued.media.frame.id);
        }
    }

    fn remove_frame_if_empty(&mut self, sender: usize, frame: FrameId) {
        if !self
            .queue
            .iter()
            .any(|queued| queued.sender == sender && queued.media.frame.id == frame)
        {
            self.queued_frames
                .remove(&(self.senders[sender].facts.id, frame));
        }
    }

    fn reallocate(&mut self, capacity: u64) {
        let inputs = self
            .senders
            .iter()
            .map(|sender| AllocationInput {
                sender: sender.facts.id,
                demand: LatencyGovernor::operating_point(
                    playout_max_ticks(sender.policy),
                    sender.policy.desired_bitrate.as_bps(),
                )
                .governed_demand,
                weight: sender.policy.priority.weight(),
            })
            .collect::<Vec<_>>();
        let next = weighted_max_min(capacity, &inputs);
        if next != self.allocations {
            self.allocations.clone_from(&next);
            self.events
                .push_back(Event::AllocationChanged(AllocationSnapshot {
                    total: MediaPayloadBitrate::from_bps(next.iter().sum()),
                    senders: self
                        .senders
                        .iter()
                        .zip(next)
                        .map(|(sender, bitrate)| SenderAllocation {
                            sender: sender.facts.id,
                            bitrate: MediaPayloadBitrate::from_bps(bitrate),
                        })
                        .collect(),
                }));
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
        }
        let Ok(parsed) = RtpPacket::parse(packet) else {
            return;
        };
        self.retained.push_back(RetainedPacket {
            sender,
            sequence,
            timestamp,
            payload: parsed.payload().to_vec(),
            retained_at: at,
            global,
            transport_bytes: wire_len,
        });
        self.retained_bytes = self.retained_bytes.saturating_add(wire_len);
    }

    fn expire_repair(&mut self, now: Instant) {
        while self.retained.front().is_some_and(|packet| {
            now.saturating_duration_since(packet.retained_at) >= Duration::from_secs(2)
        }) {
            if let Some(packet) = self.retained.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(packet.transport_bytes);
                self.repair_requests
                    .retain(|request| *request != (packet.sender, packet.sequence));
            }
        }
    }

    fn prepare_repair(
        &mut self,
        at: TimePoint,
        transport: &mut Transport,
    ) -> Option<PrepareResult> {
        let (sender_index, original_sequence) = *self.repair_requests.front()?;
        let retained = self
            .retained
            .iter()
            .find(|packet| packet.sender == sender_index && packet.sequence == original_sequence)?;
        if !self.useful_at(sender_index, retained.global, at, Duration::ZERO) {
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
        let mut payload = original_sequence.to_be_bytes().to_vec();
        payload.extend_from_slice(&retained.payload);
        let packet = build_rtp(
            sender,
            true,
            sequence,
            retained.timestamp,
            twcc,
            &payload,
            payload_type,
            sender.rtx_ssrc,
            false,
        )?;
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

    fn prepare_padding(&mut self, at: TimePoint, transport: &mut Transport) -> PrepareResult {
        let Some(envelope) = self.envelope else {
            return PrepareResult::Blocked;
        };
        let interval = Duration::from_secs(1).max(envelope.smoothed_rtt.saturating_mul(4));
        if self.probe_remaining == 0 {
            let due = self
                .last_probe
                .is_none_or(|last| at.monotonic.saturating_duration_since(last) >= interval);
            let candidate = self
                .senders
                .iter()
                .enumerate()
                .filter(|(_, sender)| sender.policy.desired_bitrate.as_bps() > 0)
                .max_by_key(|(index, sender)| {
                    sender
                        .policy
                        .desired_bitrate
                        .as_bps()
                        .saturating_sub(self.allocations[*index])
                        .saturating_mul(u64::from(sender.policy.priority.weight()))
                })
                .map(|(index, _)| index);
            if !envelope.probe_permitted || !due || candidate.is_none() {
                return PrepareResult::Blocked;
            }
            self.probe_remaining = 5;
        }
        let Some(sender_index) = self
            .senders
            .iter()
            .enumerate()
            .filter(|(_, sender)| sender.policy.desired_bitrate.as_bps() > 0)
            .max_by_key(|(index, sender)| {
                sender
                    .policy
                    .desired_bitrate
                    .as_bps()
                    .saturating_sub(self.allocations[*index])
            })
            .map(|(index, _)| index)
        else {
            self.probe_remaining = 0;
            return PrepareResult::Blocked;
        };
        let sender = &self.senders[sender_index];
        let sequence = wire_u16(sender.next_sequence);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let timestamp = sender
            .latest_committed_global
            .and_then(|global| mapped_timestamp(sender, global))
            .unwrap_or(sender.base_timestamp);
        let Some(packet) = build_rtp(
            sender,
            false,
            sequence,
            timestamp,
            twcc,
            &[1],
            sender.facts.payload_type,
            sender.ssrc,
            true,
        ) else {
            return PrepareResult::Fatal;
        };
        match transport.send_rtp_with_service(&packet, RtpService::Padding) {
            Ok(()) => {
                self.pending = Some(PendingTransmit::Padding {
                    sender: sender_index,
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

fn playout_max_ticks(policy: SenderPolicy) -> u16 {
    u16::try_from(policy.playout_delay.max().as_millis() / 10).unwrap_or(u16::MAX)
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
) -> Option<Vec<u8>> {
    let mut extensions = Vec::with_capacity(2);
    if let Some(id) = sender.facts.mid_extension_id {
        extensions.push((id, sender.facts.mid.as_bytes().to_vec()));
    }
    if let (Some(id), Some(sequence)) = (sender.facts.twcc_extension_id, twcc) {
        extensions.push((id, sequence.to_be_bytes().to_vec()));
    }
    let mut packet = Vec::with_capacity(20usize.saturating_add(payload.len()));
    packet
        .push(0x80 | if extensions.is_empty() { 0 } else { 0x10 } | if padding { 0x20 } else { 0 });
    packet.push(payload_type | if marker { 0x80 } else { 0 });
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    if !extensions.is_empty() && !write_extensions(&mut packet, &extensions) {
        return None;
    }
    packet.extend_from_slice(payload);
    Some(packet)
}

fn write_extensions(packet: &mut Vec<u8>, extensions: &[(u8, Vec<u8>)]) -> bool {
    let one_byte = extensions
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
    use crate::{FrameMetadata, MediaPacket, MediaPriority, PlayoutDelay};

    #[test]
    fn admission_reaches_exact_paced_packet_bound_under_a_feasible_envelope() {
        let now = Instant::now();
        let at = TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000),
        };
        let policy = SenderPolicy {
            playout_delay: PlayoutDelay::from_ticks(0, 50).expect("500 ms playout"),
            priority: MediaPriority::MEDIUM,
            desired_bitrate: MediaPayloadBitrate::from_bps(100_000_000),
        };
        let sender = SenderId::new(1).expect("sender");
        let mut owner = MediaEgress::new(
            vec![EgressSenderFacts {
                id: sender,
                kind: MediaKind::Audio,
                mid: "audio".into(),
                payload_type: 111,
                retransmission_payload_type: None,
                clock_rate: 48_000,
                mid_extension_id: None,
                twcc_extension_id: Some(3),
            }]
            .into_boxed_slice(),
            &[7; 32],
            usize::MAX,
            usize::MAX,
            policy,
            policy,
            now,
        );
        owner.update_controller(at, Some((1, true)), &[], Duration::ZERO, 0);
        owner
            .envelope
            .as_mut()
            .expect("controller envelope")
            .pacing_transport_rate = 100_000_000;

        for id in 1..=MAX_QUEUED_MEDIA_PACKETS {
            assert_eq!(
                owner.admit(at, sender, media(u64::try_from(id).expect("bounded id"))),
                Ok(())
            );
        }
        assert_eq!(
            owner.admit(at, sender, media(9_000)),
            Err(CommandError::WouldBlock)
        );
    }

    fn media(id: u64) -> ForwardedMedia {
        ForwardedMedia {
            packet: MediaPacket::new(
                Bytes::from_static(&[0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7]),
                GlobalMediaTime::from_micros(1_000_000),
                Arc::from([]),
            ),
            frame: FrameMetadata {
                id: FrameId::from_value(id),
                boundary: FrameBoundary::Complete,
                random_access: true,
                discardable: false,
                dependencies: FrameDependencies::Known(Arc::from([])),
            },
        }
    }
}
