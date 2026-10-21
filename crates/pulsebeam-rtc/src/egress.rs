#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "sender indexes are resolved from the same immutable negotiated sender array"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    Event, MediaKind, MediaPayloadBitrate, RtpWrite, SenderStats, WriteError,
    congestion::{
        ControllerInput, EcnValidation, FeedbackSample, SafeRtpEnvelope, ScreamController,
    },
    negotiation::EgressSenderFacts,
    pacer::Pacer,
    packet::RtpPacket,
    scheduler::SenderScheduler,
    sent_history::PacketFeedback,
    time::ProtocolClock,
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
    eligible: Vec<Option<usize>>,
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
    probe_pacer: Pacer,
    probe_pacing_rate: u64,
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
    offered_rate: u64,
    rtp_bytes_in_flight: u64,
}

struct Sender {
    facts: EgressSenderFacts,
    ssrc: u32,
    rtx_ssrc: u32,
    next_rtx_sequence: u64,
    latest_rtp: Option<(u32, Instant)>,
    highest_sequence: Option<u64>,
    queue: VecDeque<QueuedMedia>,
    queued_payload_bytes: usize,
    keyframe_requested: bool,
    fir_sequences: Vec<(u32, u8)>,
    committed_packets: u64,
    committed_payload_bytes: u64,
}

struct QueuedMedia {
    media: RtpWrite,
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
    transport_bytes: usize,
}

enum PendingTransmit {
    Original {
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
        accepted_at: Instant,
        clock: ProtocolClock,
    ) -> Self {
        let senders: Box<[_]> = facts
            .into_vec()
            .into_iter()
            .map(|facts| {
                let seed = sender_seed(randomness, facts.id.value());
                Sender {
                    ssrc: nonzero_u32([seed[0], seed[1], seed[2], seed[3]]),
                    rtx_ssrc: nonzero_u32([seed[10], seed[11], seed[12], seed[13]]),
                    next_rtx_sequence: u64::from(u16::from_be_bytes([seed[14], seed[15]])),
                    latest_rtp: None,
                    highest_sequence: None,
                    queue: VecDeque::new(),
                    queued_payload_bytes: 0,
                    keyframe_requested: false,
                    fir_sequences: Vec::new(),
                    committed_packets: 0,
                    committed_payload_bytes: 0,
                    facts,
                }
            })
            .collect();
        let desired = 0;
        let twcc_seed = sender_seed(randomness, u16::MAX);
        let sender_count = senders.len();
        let mut owner = Self {
            senders,
            queued_packets: 0,
            eligible: vec![None; sender_count],
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
            probe_pacer: Pacer::default(),
            probe_pacing_rate: 0,
            last_send_attempt: None,
            scheduler: SenderScheduler::new(sender_count),
            controller: ScreamController::new(desired, None),
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
            offered_rate: 0,
            rtp_bytes_in_flight: 0,
        };
        owner.update_estimate(
            owner
                .controller
                .snapshot(desired, 0, 0)
                .target_media_payload_rate,
        );
        owner.estimate_changed = None;
        owner
    }

    #[allow(
        clippy::result_large_err,
        reason = "keep caller-owned input inline instead of allocating an error wrapper"
    )]
    pub(crate) fn admit(
        &mut self,
        _at: Instant,
        mid: &str,
        media: RtpWrite,
    ) -> Result<(), (WriteError, RtpWrite)> {
        let result = (|| {
            let index = self
                .senders
                .iter()
                .position(|sender| sender.facts.mid.as_ref() == mid)
                .ok_or(WriteError::UnknownStream)?;
            if media.sequence >= (1_u64 << 48) {
                return Err(WriteError::IndexExhausted);
            }
            let state = &self.senders[index];
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
            if self.queued_packets >= MAX_QUEUED_MEDIA_PACKETS
                || payload_bytes
                    > self
                        .max_payload_bytes
                        .saturating_sub(self.queued_payload_bytes)
                || transport_estimate
                    > MAX_PACED_TRANSPORT_BYTES.saturating_sub(self.queued_transport_bytes)
            {
                return Err(WriteError::WouldBlock);
            }
            self.queued_payload_bytes = self.queued_payload_bytes.saturating_add(payload_bytes);
            self.queued_transport_bytes = self
                .queued_transport_bytes
                .saturating_add(transport_estimate);
            self.queued_packets = self.queued_packets.saturating_add(1);
            self.senders[index].queued_payload_bytes = self.senders[index]
                .queued_payload_bytes
                .saturating_add(payload_bytes);
            Ok((index, payload_bytes, transport_estimate))
        })();
        match result {
            Ok((index, payload_bytes, transport_estimate)) => {
                self.senders[index].queue.push_back(QueuedMedia {
                    media,
                    payload_bytes,
                    transport_estimate,
                });
                Ok(())
            }
            Err(error) => Err((error, media)),
        }
    }

    pub(crate) fn set_desired_bitrate(&mut self, bitrate: Option<MediaPayloadBitrate>) {
        self.desired_bitrate = bitrate.map_or(0, MediaPayloadBitrate::as_bps);
    }

    pub(crate) fn discard_pending(&mut self, mid: &str) -> Result<(), WriteError> {
        let index = self
            .senders
            .iter()
            .position(|sender| sender.facts.mid.as_ref() == mid)
            .ok_or(WriteError::UnknownStream)?;
        if self.pending.is_some() {
            return Err(WriteError::InvalidState);
        }
        while self.pop_head(index).is_some() {}
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
        at: Instant,
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
                offered_media_rate: self.offered_rate,
                desired_media_rate: desired,
                window_or_pacer_blocked: (self.queued_transport_bytes > 0
                    || !self.repair_requests.is_empty())
                    && (!self.pacer.eligible(at)
                        || self.envelope.is_some_and(|envelope| {
                            bytes_in_flight >= envelope.max_rtp_bytes_in_flight
                        })),
                ecn: EcnValidation::default(),
            },
        );
        self.pacer.reprice(at, output.pacing_transport_rate);
        if self.probe_pacing_rate > 0 && self.has_established_media_service() {
            self.probe_pacing_rate = self.probe_pacing_rate.min(output.pacing_transport_rate);
            self.probe_pacer.reprice(at, self.probe_pacing_rate);
        }
        if fresh_network_feedback
            && !output.feedback_stale
            && self.feedback_recovery == FeedbackRecovery::AwaitingReport
        {
            self.feedback_recovery = FeedbackRecovery::Followup;
        }
        self.rtp_bytes_in_flight = bytes_in_flight;
        if self
            .probe_started_at
            .is_some_and(|started| at.saturating_duration_since(started) >= self.probe_duration())
        {
            // libwebrtc accepts feedback covering 80% of the requested probe.
            // Timer quantization must not discard an otherwise useful flight sample.
            let incomplete = self.probe_packet_count < 5
                || self.probe_sent_bytes.saturating_mul(100)
                    < self.probe_target_bytes.saturating_mul(80);
            self.finish_probe(at, incomplete);
        }
        self.observe_probe_feedback(at, probe_feedback, output.reason);
        if !self.discovery_followup_permitted(at) {
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
            self.finish_probe(at, true);
        }
        self.envelope = Some(output);
        self.update_estimate(output.target_media_payload_rate);
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
        sender.keyframe_requested = true;
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
        if let Some(estimate) = self.estimate_changed.take() {
            return Some(Event::BandwidthEstimate(estimate));
        }
        let sender = self
            .senders
            .iter_mut()
            .find(|sender| sender.keyframe_requested)?;
        sender.keyframe_requested = false;
        Some(Event::KeyframeRequested { ssrc: sender.ssrc })
    }

    pub(crate) fn abort(&mut self) {
        self.begin_shutdown();
        self.estimate_changed = None;
    }

    pub(crate) fn begin_shutdown(&mut self) {
        for sender in &mut self.senders {
            sender.queue.clear();
            sender.queued_payload_bytes = 0;
        }
        self.queued_packets = 0;
        self.queued_payload_bytes = 0;
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

    pub(crate) fn stats(&self) -> (EgressStats, Vec<SenderStats>) {
        let senders = self
            .senders
            .iter()
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
    ) -> PrepareResult {
        if self.pending.is_some() {
            return PrepareResult::Fatal;
        }
        self.expire_repair(at);
        if !transport.can_send_user_data() {
            return PrepareResult::Blocked;
        }
        if at >= self.next_rtcp
            && let Some(result) = self.prepare_rtcp(at, transport)
        {
            return result;
        }
        self.last_send_attempt = Some(at);
        if !self.pacer.eligible(at) {
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

        self.fill_eligible();
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
        let media = &queued.media;
        let sequence = wire_u16(media.sequence);
        let twcc = sender
            .facts
            .twcc_extension_id
            .map(|_| wire_u16(self.next_twcc));
        let extensions = write_media_extensions(sender, media);
        let Some(packet) = build_rtp(
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
        ) else {
            return PrepareResult::Fatal;
        };
        let timestamp = media.timestamp;
        let index = media.sequence;
        let classification = crate::TransmitKind::Original { ssrc: sender.ssrc };
        if !self.fits_send_window(bytes_in_flight, packet.len()) {
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_info(&packet, index, classification) {
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
                            index: state
                                .queue
                                .front()
                                .map_or(0, |queued| queued.media.sequence),
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
                            index: state.next_rtx_sequence,
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
                ssrc,
                sequence,
                twcc,
                ..
            }) => prepared.rtp.is_some_and(|identity| {
                identity
                    == PreparedRtpIdentity {
                        ssrc: *ssrc,
                        sequence: *sequence,
                        index: self.senders[*sender].next_rtx_sequence,
                        twcc_sequence: *twcc,
                        service: RtpService::Padding,
                    }
            }),
            None => prepared.rtp.is_none(),
        }
    }

    pub(crate) fn commit(&mut self, at: Instant, prepared: &PreparedTransmit) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if !matches!(pending, PendingTransmit::Control) {
            self.pacer.commit(
                at,
                prepared.wire_len,
                self.envelope
                    .map_or(50_000, |value| value.pacing_transport_rate),
            );
            self.controller
                .note_send(at.saturating_duration_since(self.controller_origin));
        }
        if !matches!(pending, PendingTransmit::Control) {
            self.record_rtp(
                at,
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
            } => {
                let Some(queued) = self.pop_head(sender) else {
                    return;
                };
                let state = &mut self.senders[sender];
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
                self.retain_original(sender, sequence, timestamp, &packet, at, prepared.wire_len);
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
                    retained.last_repair_at = Some(at);
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
                self.next_rtcp = at.checked_add(Duration::from_secs(1)).unwrap_or(at);
            }
            PendingTransmit::Padding {
                sender, ssrc, twcc, ..
            } => {
                if self.probe_pacing_rate > 0 {
                    self.probe_pacer.commit_probe_cluster(
                        at,
                        prepared.wire_len,
                        self.probe_pacing_rate,
                    );
                }
                let state = &mut self.senders[sender];
                debug_assert_eq!(ssrc, state.rtx_ssrc);
                state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
                if twcc.is_some() {
                    self.next_twcc = self.next_twcc.saturating_add(1);
                }
                if self.probe_started_at.is_none() {
                    self.probe_started_at = Some(at);
                    self.probe_start_queue_delay = self
                        .envelope
                        .map_or(Duration::ZERO, |envelope| envelope.queue_delay);
                }
                self.probe_sent_bytes = self
                    .probe_sent_bytes
                    .saturating_add(prepared.wire_len as u64);
                self.probe_last_sent_at = Some(at);
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
                    self.finish_probe(at, false);
                }
            }
        }
    }

    pub(crate) fn next_deadline(&self, _at: Instant) -> Option<Instant> {
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
            (self.queued_packets != 0 || !self.repair_requests.is_empty())
                .then(|| self.pacer.next_deadline())
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
    pub(crate) fn test_envelope(&self) -> Option<SafeRtpEnvelope> {
        self.envelope
    }

    #[cfg(test)]
    pub(crate) fn test_initial_probe_started_at(&self) -> Option<Instant> {
        self.initial_probe_started_at
    }

    pub(crate) const fn controller_origin(&self) -> Instant {
        self.controller_origin
    }

    fn fill_eligible(&mut self) {
        for (slot, sender) in self.eligible.iter_mut().zip(&self.senders) {
            *slot = sender.queue.front().map(|queued| queued.payload_bytes);
        }
    }

    fn pop_head(&mut self, sender: usize) -> Option<QueuedMedia> {
        let state = &mut self.senders[sender];
        let queued = state.queue.pop_front()?;
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

    fn update_estimate(&mut self, budget: u64) {
        if budget != self.bandwidth_estimate {
            self.bandwidth_estimate = budget;
            self.estimate_changed = Some(MediaPayloadBitrate::from_bps(budget));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn retain_original(
        &mut self,
        sender: usize,
        sequence: u16,
        timestamp: u32,
        packet: &[u8],
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
            transport_bytes: wire_len,
        });
        self.retained_bytes = self.retained_bytes.saturating_add(wire_len);
    }

    fn expire_repair(&mut self, now: Instant) {
        let requests = &mut self.repair_requests;
        let mut removed_bytes = 0usize;
        self.retained.retain(|packet| {
            let keep = now.saturating_duration_since(packet.retained_at) < Duration::from_secs(3);
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

    fn prepare_repair(
        &mut self,
        at: Instant,
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
        if !self.repair_due(retained, at) {
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
        let result = match transport.send_rtp_with_info(
            &packet,
            sender.next_rtx_sequence,
            crate::TransmitKind::Repair {
                ssrc: sender.rtx_ssrc,
            },
        ) {
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

    fn prepare_rtcp(&mut self, at: Instant, transport: &mut Transport) -> Option<PrepareResult> {
        let sender_index = (0..self.senders.len())
            .map(|offset| (self.rtcp_cursor + offset) % self.senders.len())
            .find(|index| self.senders[*index].committed_packets > 0)?;
        let sender = &self.senders[sender_index];
        let packet = sender_report(sender, at, &self.clock)?;
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
            && self.desired_bitrate() > 0
            && !self.has_established_media_service()
    }

    fn discovery_followup_permitted(&self, at: Instant) -> bool {
        self.probe_followup
            && self.probe_candidate().is_some_and(|sender| {
                self.initial_probing(at) || self.senders[sender].latest_rtp.is_none()
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
        self.desired_bitrate > 0
            && self.senders[sender]
                .facts
                .retransmission_payload_type
                .is_some()
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
            .find(|(index, _)| self.padding_allowed(*index))
            .map(|(index, _)| index)
    }

    fn probe_budget_bitrate(&self) -> u64 {
        self.desired_bitrate()
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
            .any(|sender| sender.latest_rtp.is_some())
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

    fn recovery_permitted(&self, envelope: SafeRtpEnvelope) -> bool {
        self.path_available
            && (envelope.feedback_stale || self.feedback_recovery == FeedbackRecovery::Followup)
            && self.desired_bitrate > 0
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

    fn start_probe(&mut self, at: Instant, envelope: SafeRtpEnvelope) -> bool {
        let interval = self.probe_interval(at, envelope);
        let due = self
            .last_probe
            .is_none_or(|last| at.saturating_duration_since(last) >= interval);
        let recovery = self.recovery_permitted(envelope);
        let followup = recovery
            && !envelope.feedback_stale
            && self.feedback_recovery == FeedbackRecovery::Followup;
        let discovery_followup = envelope.probe_permitted && self.discovery_followup_permitted(at);
        if (!envelope.probe_permitted && !recovery) || (!due && !followup && !discovery_followup) {
            return false;
        }
        let Some(sender_index) = self.probe_candidate() else {
            return false;
        };
        self.probe_followup = false;
        let promote = followup && !envelope.feedback_stale && envelope.probe_permitted;
        if envelope.probe_permitted && (!recovery || promote) {
            self.initial_probe_started_at.get_or_insert(at);
            if self.initial_probing(at) {
                // libwebrtc budgets two initial clusters, capped at twice application demand.
                // Account initial service separately from the rate-bounded sample duration.
                let allowance = u64::try_from(
                    u128::from(self.desired_bitrate())
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
            let bootstrap_sample = if !self.has_established_media_service()
                && (self.senders[sender_index].latest_rtp.is_none() || self.initial_probing(at))
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
            let idle_sample = if !self.initial_probing(at)
                && (envelope.application_limited || !self.has_established_media_service())
            {
                // A successful small cluster can still leave the native
                // observed-flight growth bound below its reference window.
                let headroom = if !self.has_established_media_service()
                    && self.senders[sender_index].latest_rtp.is_none()
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
        at: Instant,
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
        if self.probe_pacing_rate > 0 && !self.probe_pacer.eligible(at) {
            return PrepareResult::Blocked;
        }
        let Some(sender_index) = self.active_probe_candidate() else {
            return PrepareResult::Blocked;
        };
        self.probe_sender = Some(sender_index);
        let sender = &self.senders[sender_index];
        if self.desired_bitrate == 0 {
            self.finish_probe(at, true);
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
        if packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE) as u64
            > (48 * 1024_u64).saturating_sub(self.probe_sent_bytes)
            || !self.fits_send_window(bytes_in_flight, packet.len())
        {
            self.finish_probe(at, true);
            return PrepareResult::Blocked;
        }
        if !self.probe_budget_available(
            at,
            packet.len().saturating_add(RTP_TRANSPORT_ALLOWANCE) as u64,
        ) {
            self.finish_probe(at, true);
            return PrepareResult::Blocked;
        }
        match transport.send_rtp_with_info(
            &packet,
            sender.next_rtx_sequence,
            crate::TransmitKind::Padding {
                ssrc: sender.rtx_ssrc,
            },
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

fn padding_source(sender: &Sender) -> (u16, u8, u32) {
    (
        wire_u16(sender.next_rtx_sequence),
        sender.facts.retransmission_payload_type.unwrap_or_default(),
        sender.rtx_ssrc,
    )
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

fn write_media_extensions(sender: &Sender, media: &RtpWrite) -> Vec<(u8, Vec<u8>)> {
    media
        .extensions
        .iter()
        .filter_map(|value| {
            let id = sender
                .facts
                .forwarded_extensions
                .iter()
                .find(|(_, uri)| uri.as_ref() == value.uri.as_ref())
                .map(|(id, _)| *id)
                .or_else(|| match value.uri.as_ref() {
                    uri if crate::negotiation::DEPENDENCY_URIS.contains(&uri) => {
                        sender.facts.dependency_extension_id
                    }
                    "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => {
                        sender.facts.playout_extension_id
                    }
                    _ => None,
                })?;
            Some((id, value.value.to_vec()))
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
    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn sender_report_correspondence_and_incoming_wire_facts_use_the_fixed_anchor() {
        let start = Instant::now();
        let facts = EgressSenderFacts {
            id: crate::id::SenderId::new(1).unwrap(),
            kind: MediaKind::Audio,
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
        let mut egress = MediaEgress::new(
            Box::new([facts]),
            &[7; 32],
            1_024,
            1_024,
            start,
            ProtocolClock::new(start, UNIX_EPOCH + Duration::from_secs(17)),
        );
        let sender = &mut egress.senders[0];
        sender.latest_rtp = Some((u32::MAX - 11_999, start + Duration::from_millis(250)));
        sender.committed_packets = 2;
        sender.committed_payload_bytes = 777;
        let packet =
            sender_report(sender, start + Duration::from_millis(750), &egress.clock).unwrap();
        assert_eq!(u32::from_be_bytes(packet[20..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(packet[24..28].try_into().unwrap()), 777);
        let arrival = start + Duration::from_secs(2);
        let mut ingress = crate::ingress::IngressOwner::new(
            Box::new([]),
            1,
            4_096,
            99,
            crate::receive_feedback::ReceiveFeedback::new(None, start),
        );
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
