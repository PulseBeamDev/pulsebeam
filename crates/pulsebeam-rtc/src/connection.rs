#![allow(
    clippy::disallowed_types,
    reason = "the public transmit contract requires immutable Bytes payloads"
)]

use std::{cell::Cell, marker::PhantomData, time::Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    AcceptError, CloseReason, Command, CommandError, ConnectionConfig, ConnectionEntropy,
    ConnectionState, ConnectionStats, ConnectionWarning, Event, MediaPayloadBitrate, NetworkInput,
    Output, PacketFeedbackKind, ReceiveError, RejectedCommand, SdpAnswer, SdpOffer, SessionInfo,
    StatsSnapshot, TimePoint, Transmit,
    egress::{MediaEgress, PrepareResult},
    ingress::IngressOwner,
    negotiation::{self, NegotiatedSessionFacts},
    scheduler::{ServiceArbiter, UserLane},
    sctp::Association,
    sent_history::{HistoryError, MAX_EXPIRATIONS_PER_POLL, SentHistory},
    time::MonotonicObserver,
    transport::{
        DatagramKind, PathEpoch, PreparedRtpIdentity, PreparedTransmit, RtpService, Transport,
        TransportError, TransportEvent, TransportState,
    },
};

pub struct Connection {
    _config: ConnectionConfig,
    session: NegotiatedSessionFacts,
    _time: MonotonicObserver,
    _subsystems: SubsystemSlots,
    runtime: Runtime,
    _not_sync: PhantomData<Cell<()>>,
}

struct SubsystemSlots {
    transport: Transport,
    ingress: IngressOwner,
    egress: MediaEgress,
    sctp: Option<Association>,
}

struct Runtime {
    service: ServiceArbiter,
    commit: CommitCoordinator,
    lifecycle: Lifecycle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Open,
    Closing { deadline: Instant },
    ClosedPending(CloseReason),
    Closed(CloseReason),
}

impl Runtime {
    fn new(feedback: Option<PacketFeedbackKind>) -> Self {
        Self {
            service: ServiceArbiter::default(),
            commit: CommitCoordinator::new(feedback),
            lifecycle: Lifecycle::Open,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransmitCommitContext {
    pub(crate) at: Instant,
    pub(crate) kind: DatagramKind,
    pub(crate) wire_len: usize,
    pub(crate) path_epoch: Option<PathEpoch>,
    pub(crate) rtp: Option<PreparedRtpIdentity>,
}

struct CommitCoordinator {
    history: SentHistory,
    counters: CommitCounters,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CommitCounters {
    rtp_bytes: u64,
    rtcp_bytes: u64,
    sctp_bytes: u64,
    protocol_bytes: u64,
    padding_bytes: u64,
}

impl CommitCoordinator {
    fn new(feedback: impl Into<Option<PacketFeedbackKind>>) -> Self {
        Self {
            history: SentHistory::new(feedback),
            counters: CommitCounters::default(),
        }
    }

    fn commit_transport(
        &mut self,
        at: TimePoint,
        prepared: PreparedTransmit,
        egress: Option<&mut MediaEgress>,
    ) -> Result<Transmit, HistoryError> {
        let context = TransmitCommitContext {
            at: at.monotonic,
            kind: prepared.kind,
            wire_len: prepared.wire_len,
            path_epoch: prepared.path_epoch,
            rtp: prepared.rtp,
        };
        self.history.preflight_commit(context)?;
        if egress
            .as_deref()
            .is_some_and(|egress| !egress.preflight_commit(&prepared))
        {
            return Err(HistoryError::InvalidCommit);
        }
        self.history.commit_preflighted(context);
        if let Some(egress) = egress {
            egress.commit(at, &prepared);
        }
        let bytes = u64::try_from(context.wire_len).unwrap_or(u64::MAX);
        match context.kind {
            DatagramKind::Rtp
                if context
                    .rtp
                    .is_some_and(|rtp| rtp.service == RtpService::Padding) =>
            {
                self.counters.padding_bytes = self.counters.padding_bytes.saturating_add(bytes);
            }
            DatagramKind::Rtp => {
                self.counters.rtp_bytes = self.counters.rtp_bytes.saturating_add(bytes);
            }
            DatagramKind::Rtcp => {
                self.counters.rtcp_bytes = self.counters.rtcp_bytes.saturating_add(bytes);
            }
            DatagramKind::Sctp => {
                self.counters.sctp_bytes = self.counters.sctp_bytes.saturating_add(bytes);
            }
            DatagramKind::Stun | DatagramKind::Dtls => {
                self.counters.protocol_bytes = self.counters.protocol_bytes.saturating_add(bytes);
            }
        }
        let PreparedTransmit { target, bytes, .. } = prepared;
        Ok(Transmit {
            target,
            payload: Bytes::from(bytes),
        })
    }
}

pub struct AcceptedConnection {
    pub connection: Connection,
    pub answer: SdpAnswer,
    pub session: SessionInfo,
}

impl Connection {
    #[allow(
        clippy::result_large_err,
        reason = "keep caller-owned input inline instead of allocating an error wrapper"
    )]
    pub fn command(&mut self, at: TimePoint, command: Command) -> Result<(), RejectedCommand> {
        let at = self._time.observe(at);
        if matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) {
            return Err(RejectedCommand {
                reason: CommandError::Closed,
                command,
            });
        }
        if matches!(self.runtime.lifecycle, Lifecycle::Closing { .. }) {
            if matches!(&command, Command::CloseGracefully { .. }) {
                return Ok(());
            }
            if !matches!(&command, Command::RetireIceTcpFlow { .. } | Command::Abort) {
                return Err(RejectedCommand {
                    reason: CommandError::InvalidState,
                    command,
                });
            }
        }
        match command {
            Command::SendMedia { sender, media } => {
                let reason = match self._subsystems.transport.state() {
                    TransportState::Connected => None,
                    TransportState::Closed | TransportState::Failed => Some(CommandError::Closed),
                    TransportState::Checking
                    | TransportState::Connecting
                    | TransportState::Draining => Some(CommandError::InvalidState),
                };
                if let Some(reason) = reason {
                    return Err(RejectedCommand {
                        reason,
                        command: Command::SendMedia { sender, media },
                    });
                }
                self._subsystems
                    .egress
                    .admit(at, sender, media)
                    .map_err(|(reason, media)| RejectedCommand {
                        reason,
                        command: Command::SendMedia { sender, media },
                    })
            }
            Command::OpenDataChannel(config) => {
                let Some(sctp) = self._subsystems.sctp.as_mut() else {
                    return Err(RejectedCommand {
                        reason: CommandError::InvalidState,
                        command: Command::OpenDataChannel(config),
                    });
                };
                sctp.open(at, config)
                    .map_err(|(reason, config)| RejectedCommand {
                        reason,
                        command: Command::OpenDataChannel(config),
                    })
            }
            Command::SendData { channel, message } => {
                let Some(sctp) = self._subsystems.sctp.as_mut() else {
                    return Err(RejectedCommand {
                        reason: CommandError::UnknownDataChannel(channel),
                        command: Command::SendData { channel, message },
                    });
                };
                sctp.send(at, channel, message)
                    .map_err(|(reason, message)| RejectedCommand {
                        reason,
                        command: Command::SendData { channel, message },
                    })
            }
            command => self
                .command_control(at, &command)
                .map_err(|reason| RejectedCommand { reason, command }),
        }
    }

    fn command_control(&mut self, at: TimePoint, command: &Command) -> Result<(), CommandError> {
        match command {
            Command::AdmitIceTcpFlow {
                flow,
                local,
                remote,
            } => self
                ._subsystems
                .transport
                .admit_tcp_flow(*flow, *local, *remote)
                .map_err(|error| match error {
                    TransportError::QueueFull => CommandError::WouldBlock,
                    TransportError::Closed => CommandError::Closed,
                    _ => CommandError::InvalidIceTcpFlow,
                }),
            Command::RetireIceTcpFlow { flow } => self.retire_ice_tcp_flow(*flow),
            Command::RecoverSenderMedia { sender, assignment } => self
                ._subsystems
                .egress
                .set_assignment(*sender, Some(*assignment), at, true),
            Command::SetSenderAssignment { sender, assignment } => self
                ._subsystems
                .egress
                .set_assignment(*sender, *assignment, at, false),
            Command::CloseDataChannel { channel } => self
                ._subsystems
                .sctp
                .as_mut()
                .ok_or(CommandError::UnknownDataChannel(*channel))?
                .close_channel(at, *channel),
            Command::RequestKeyframe { encoding } => {
                let packet = self._subsystems.ingress.keyframe_request(*encoding)?;
                self._subsystems
                    .transport
                    .send_rtcp(&packet)
                    .map_err(|_| CommandError::InvalidState)
            }
            Command::RetireEncoding { encoding } => self._subsystems.ingress.retire(*encoding),
            Command::CloseGracefully { deadline } => {
                self.runtime.lifecycle = Lifecycle::Closing {
                    deadline: *deadline,
                };
                self._subsystems.egress.begin_shutdown();
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.begin_shutdown(at, *deadline);
                }
                Ok(())
            }
            Command::Abort => {
                self.abort();
                Ok(())
            }
            Command::SendMedia { .. } | Command::OpenDataChannel(_) | Command::SendData { .. } => {
                Err(CommandError::InvalidState)
            }
        }
    }

    fn retire_ice_tcp_flow(&mut self, flow: crate::IceTcpFlowId) -> Result<(), CommandError> {
        // poll prepares and commits egress synchronously; commands cannot interrupt that pair.
        self._subsystems
            .transport
            .retire_tcp_flow(flow)
            .map_err(|_| CommandError::InvalidState)
    }

    pub fn receive(&mut self, at: TimePoint, input: NetworkInput) -> Result<(), ReceiveError> {
        let at = self._time.observe(at);
        if matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) {
            return Err(ReceiveError::Closed);
        }
        self._subsystems
            .transport
            .receive_at(at, input)
            .map_err(|error| match error {
                crate::transport::TransportError::Closed => ReceiveError::Closed,
                crate::transport::TransportError::QueueFull => ReceiveError::InputLimitExceeded,
                crate::transport::TransportError::UnknownTcpFlow => ReceiveError::UnknownIceTcpFlow,
                _ => ReceiveError::InvalidNetworkEnvelope,
            })
    }

    pub fn poll(&mut self, at: TimePoint) -> Output {
        let at = self._time.observe(at);

        match self.runtime.lifecycle {
            Lifecycle::Closed(_) => return Output::Idle { next_wakeup: None },
            Lifecycle::ClosedPending(reason) => {
                self.runtime.lifecycle = Lifecycle::Closed(reason);
                return Output::Closed(reason);
            }
            Lifecycle::Open | Lifecycle::Closing { .. } => {}
        }

        if self._time.take_warning() {
            return Output::Event(Event::Warning(ConnectionWarning::ClockRegression));
        }

        if !matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && self
            ._subsystems
            .transport
            .next_deadline()
            .is_some_and(|deadline| at.monotonic >= deadline)
            && let Err(error) = self._subsystems.transport.handle_timeout(at.monotonic)
            && !matches!(error, TransportError::NotDue)
        {
            let reason = close_reason(error);
            self.runtime.lifecycle = Lifecycle::Closed(reason);
            return Output::Closed(reason);
        }

        if let Some(event) = self._subsystems.transport.poll_event()
            && let Some(output) = self.handle_transport_event(at, event)
        {
            return output;
        }

        if let Some(event) = self._subsystems.ingress.poll_event() {
            return Output::Event(event);
        }

        if let Some(event) = self
            ._subsystems
            .sctp
            .as_mut()
            .and_then(|sctp| sctp.poll_event(at))
        {
            return Output::Event(event);
        }

        // Preserve committed flight before expiration or batched ACKs retire it.
        self._subsystems.egress.observe_bytes_in_flight(
            self.runtime
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight,
        );
        self.runtime
            .commit
            .history
            .expire(at.monotonic, MAX_EXPIRATIONS_PER_POLL);

        if let Some(feedback) = self._subsystems.ingress.poll_feedback() {
            self.runtime.commit.history.process_feedback(feedback);
        }
        if self.runtime.commit.history.controller_inputs().exhausted {
            self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Output::Closed(CloseReason::TransportFailure);
        }
        while let Some(request) = self._subsystems.ingress.poll_keyframe() {
            self._subsystems.egress.request_keyframe(request);
        }
        if let Some(repair) = self._subsystems.ingress.poll_repair() {
            self._subsystems
                .egress
                .request_repair(repair.media_ssrc, repair.sequence);
        }

        let (
            path_change,
            feedback,
            probe_feedback,
            feedback_hold,
            fresh_network_feedback,
            bytes_in_flight,
        ) = {
            let inputs = self.runtime.commit.history.controller_inputs();
            let received_at = inputs
                .timing
                .map_or(at.monotonic, |timing| timing.received_at);
            let origin = self._subsystems.egress.controller_origin();
            (
                inputs
                    .path_change
                    .map(|change| (change.epoch.value(), change.available)),
                inputs
                    .feedback
                    .iter()
                    .chain(&inputs.synthetic)
                    .map(|feedback| feedback.controller_sample(received_at, origin))
                    .collect::<Vec<_>>(),
                inputs
                    .feedback
                    .iter()
                    .chain(&inputs.synthetic)
                    .copied()
                    .collect::<Vec<_>>(),
                inputs
                    .timing
                    .and_then(|timing| timing.feedback_hold)
                    .unwrap_or_default(),
                inputs.fresh_network_feedback,
                inputs.bytes_in_flight,
            )
        };
        let application_limited = self._subsystems.egress.update_controller(
            at,
            path_change,
            &feedback,
            &probe_feedback,
            feedback_hold,
            fresh_network_feedback,
            bytes_in_flight,
        );
        self.runtime.commit.history.clear_controller_inputs();
        self.runtime
            .commit
            .history
            .set_application_limited(application_limited);

        if let Some(event) = self._subsystems.egress.poll_event() {
            return Output::Event(event);
        }

        if let Lifecycle::Closing { deadline } = self.runtime.lifecycle {
            let sctp_stopped = self
                ._subsystems
                .sctp
                .as_ref()
                .is_none_or(Association::is_stopped);
            if sctp_stopped || at.monotonic >= deadline {
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.abort();
                }
                let _ = self._subsystems.transport.close(at.monotonic);
                if let Some(prepared) = self._subsystems.transport.poll_prepared() {
                    return match self.runtime.commit.commit_transport(at, prepared, None) {
                        Ok(transmit) => {
                            self.runtime.lifecycle =
                                Lifecycle::ClosedPending(CloseReason::Graceful);
                            Output::Transmit(transmit)
                        }
                        Err(_) => {
                            self.runtime.lifecycle =
                                Lifecycle::Closed(CloseReason::TransportFailure);
                            Output::Closed(CloseReason::TransportFailure)
                        }
                    };
                }
                self.runtime.lifecycle = Lifecycle::Closed(CloseReason::Graceful);
                return Output::Closed(CloseReason::Graceful);
            }
        }

        if matches!(self.runtime.lifecycle, Lifecycle::Open)
            && self._subsystems.transport.can_send_user_data()
            && let Some(packet) = self._subsystems.ingress.poll_control(at.monotonic)
        {
            if let Err(error) = self._subsystems.transport.send_rtcp(&packet)
                && error != TransportError::QueueFull
            {
                let reason = close_reason(error);
                self.runtime.lifecycle = Lifecycle::Closed(reason);
                return Output::Closed(reason);
            }
        }

        if !matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && let Some(prepared) = self._subsystems.transport.poll_prepared()
        {
            let sctp_wire_len = (prepared.kind == DatagramKind::Sctp).then_some(prepared.wire_len);
            return match self.runtime.commit.commit_transport(at, prepared, None) {
                Ok(transmit) => {
                    if let (Some(sctp), Some(wire_len)) =
                        (self._subsystems.sctp.as_mut(), sctp_wire_len)
                    {
                        sctp.commit_transport_bytes(wire_len);
                    }
                    Output::Transmit(transmit)
                }
                Err(_) => {
                    self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                    Output::Closed(CloseReason::TransportFailure)
                }
            };
        }

        let sctp_ready = self._subsystems.transport.can_send_user_data()
            && self
                ._subsystems
                .sctp
                .as_ref()
                .is_some_and(Association::has_packet);
        let selected_lane = self.runtime.service.select(true, sctp_ready);
        if !matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && selected_lane == Some(UserLane::Sctp)
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if matches!(self.runtime.lifecycle, Lifecycle::Open) {
            match self._subsystems.egress.prepare_one(
                at,
                bytes_in_flight,
                &mut self._subsystems.transport,
                !sctp_ready,
            ) {
                PrepareResult::Prepared => {
                    let Some(prepared) = self._subsystems.transport.poll_prepared() else {
                        self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                        return Output::Closed(CloseReason::TransportFailure);
                    };
                    return match self.runtime.commit.commit_transport(
                        at,
                        prepared,
                        Some(&mut self._subsystems.egress),
                    ) {
                        Ok(transmit) => Output::Transmit(transmit),
                        Err(_) => {
                            self.runtime.lifecycle =
                                Lifecycle::Closed(CloseReason::TransportFailure);
                            Output::Closed(CloseReason::TransportFailure)
                        }
                    };
                }
                PrepareResult::Fatal => {
                    self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                    return Output::Closed(CloseReason::TransportFailure);
                }
                PrepareResult::Blocked => {}
            }
        }

        if !matches!(
            self.runtime.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && selected_lane != Some(UserLane::Sctp)
            && sctp_ready
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if let Some(event) = self._subsystems.egress.poll_event() {
            return Output::Event(event);
        }

        Output::Idle {
            next_wakeup: (!matches!(self.runtime.lifecycle, Lifecycle::Closed(_)))
                .then(|| {
                    [
                        self._subsystems.transport.next_deadline(),
                        self.runtime.commit.history.next_deadline(),
                        (matches!(self.runtime.lifecycle, Lifecycle::Open)
                            && self._subsystems.transport.can_send_user_data())
                        .then(|| self._subsystems.ingress.next_deadline())
                        .flatten(),
                        self._subsystems
                            .transport
                            .can_send_user_data()
                            .then(|| self._subsystems.egress.next_deadline(at))
                            .flatten(),
                        self._subsystems
                            .sctp
                            .as_ref()
                            .and_then(Association::next_deadline),
                        match self.runtime.lifecycle {
                            Lifecycle::Closing { deadline } => Some(deadline),
                            _ => None,
                        },
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                })
                .flatten(),
        }
    }

    fn handle_transport_event(&mut self, at: TimePoint, event: TransportEvent) -> Option<Output> {
        match event {
            TransportEvent::StateChanged(TransportState::Connected) => {
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.connect(at);
                }
                Some(Output::Event(Event::Connected))
            }
            TransportEvent::StateChanged(TransportState::Failed) => {
                self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                Some(Output::Closed(CloseReason::TransportFailure))
            }
            TransportEvent::StateChanged(_) | TransportEvent::IceStateChanged(_) => None,
            TransportEvent::IceAuthenticated(flow) => {
                Some(Output::Event(Event::IceAuthenticated(flow)))
            }
            TransportEvent::SelectedPathChanged { current, epoch, .. } => {
                self.runtime
                    .commit
                    .history
                    .path_changed(epoch, current.is_some());
                None
            }
            TransportEvent::Rtp {
                arrival,
                path_epoch: _,
                bytes,
                metadata: _,
                ecn,
            } => {
                self._subsystems.ingress.accept_rtp(arrival, bytes, ecn);
                self._subsystems.ingress.poll_event().map(Output::Event)
            }
            TransportEvent::Rtcp {
                arrival,
                path_epoch,
                bytes,
            } => {
                let receipts = self._subsystems.ingress.accept_rtcp(
                    arrival,
                    bytes,
                    path_epoch,
                    self.session.feedback(),
                    self._subsystems.transport.smoothed_rtt(),
                );
                for receipt in receipts {
                    self._subsystems
                        .egress
                        .confirm_playout(receipt.ssrc, receipt.highest_sequence);
                }
                None
            }
            TransportEvent::Data(bytes) => {
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.handle_input(at, &bytes);
                }
                None
            }
            TransportEvent::Closed => {
                self.runtime.lifecycle = Lifecycle::Closed(CloseReason::Graceful);
                Some(Output::Closed(CloseReason::Graceful))
            }
        }
    }

    fn prepare_sctp(&mut self, at: TimePoint) -> Option<Output> {
        if !self._subsystems.transport.can_send_user_data() {
            return None;
        }
        let packet = self._subsystems.sctp.as_mut()?.poll_packet()?;
        if self
            ._subsystems
            .transport
            .send_sctp(&packet, at.monotonic)
            .is_err()
        {
            self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Some(Output::Closed(CloseReason::TransportFailure));
        }
        let Some(prepared) = self._subsystems.transport.poll_prepared() else {
            self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Some(Output::Closed(CloseReason::TransportFailure));
        };
        let wire_len = prepared.wire_len;
        match self.runtime.commit.commit_transport(at, prepared, None) {
            Ok(transmit) => {
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.commit_transport_bytes(wire_len);
                }
                Some(Output::Transmit(transmit))
            }
            Err(_) => {
                self.runtime.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                Some(Output::Closed(CloseReason::TransportFailure))
            }
        }
    }

    pub fn stats(&self) -> StatsSnapshot {
        let (egress, senders) = self._subsystems.egress.stats();
        let history = self.runtime.commit.history.counters();
        let sctp = self._subsystems.sctp.as_ref().map(Association::stats);
        let (state, close_reason) = match self.runtime.lifecycle {
            Lifecycle::Open => (ConnectionState::Open, None),
            Lifecycle::Closing { .. } => (ConnectionState::Closing, None),
            Lifecycle::ClosedPending(reason) | Lifecycle::Closed(reason) => {
                (ConnectionState::Closed, Some(reason))
            }
        };
        let committed = self.runtime.commit.counters;
        StatsSnapshot {
            connection: ConnectionStats {
                state,
                feedback: self.session.feedback(),
                close_reason,
                clock_regressions: self._time.regressions(),
                rtp_bytes_in_flight: self
                    .runtime
                    .commit
                    .history
                    .controller_inputs()
                    .bytes_in_flight,
                media_budget: MediaPayloadBitrate::from_bps(egress.media_budget),
                pacing_bitrate: egress.pacing_bitrate,
                queued_media_bytes: egress.queued_transport_bytes,
                buffered_data_bytes: sctp.map_or(0, |stats| stats.buffered_payload_bytes),
                transmitted_rtp_bytes: committed.rtp_bytes,
                transmitted_rtcp_bytes: committed.rtcp_bytes,
                transmitted_sctp_bytes: committed.sctp_bytes,
                transmitted_protocol_bytes: committed.protocol_bytes,
                transmitted_padding_bytes: committed.padding_bytes,
                dropped_network_inputs: self
                    ._subsystems
                    .transport
                    .dropped_inputs()
                    .saturating_add(self._subsystems.ingress.dropped()),
                unknown_feedback: history.unknown_feedback,
                duplicate_feedback: history.duplicate_feedback,
                stale_feedback: history.stale_feedback,
                wrong_path_feedback: history.wrong_path_feedback,
            },
            senders,
            encodings: self._subsystems.ingress.stats(),
            data_channels: self
                ._subsystems
                .sctp
                .as_ref()
                .map_or_else(Vec::new, Association::channel_stats),
        }
    }

    fn abort(&mut self) {
        self._subsystems.transport.abort();
        self._subsystems.ingress.abort();
        self._subsystems.egress.abort();
        if let Some(sctp) = self._subsystems.sctp.as_mut() {
            sctp.abort();
        }
        self.runtime.lifecycle = Lifecycle::ClosedPending(CloseReason::Aborted);
    }

    pub fn accept(
        config: ConnectionConfig,
        offer: SdpOffer,
        at: TimePoint,
        entropy: ConnectionEntropy,
    ) -> Result<AcceptedConnection, AcceptError> {
        let config = config.validate()?;
        if config.local_candidates.is_empty() {
            return Err(AcceptError::InvalidConfiguration);
        }
        let mut entropy = EntropyConsumer::new(entropy);
        let negotiated = negotiation::negotiate(&config, &offer, at, &mut entropy)?;
        if negotiated.facts.remote_candidates.is_empty() {
            return Err(AcceptError::InvalidOffer);
        }
        let session = negotiated.session.clone();
        let transport = Transport::from_session(&negotiated.facts, at.monotonic)
            .map_err(|_| AcceptError::CryptographicFailure)?;
        let ingress = IngressOwner::new(
            negotiated.facts.ingress_media(),
            config.limits.max_unsignaled_encodings,
            config.limits.max_queued_media_bytes,
            u32::from_be_bytes(entropy.take(b"receive-rtcp-ssrc")).max(1),
            crate::receive_feedback::ReceiveFeedback::new(
                negotiated.facts.inbound_feedback(),
                at.monotonic,
            ),
        );
        let egress = MediaEgress::new(
            negotiated.facts.egress_senders(),
            negotiated.facts.protocol_randomness(),
            config.limits.max_queued_media_bytes,
            config.limits.max_retransmission_bytes,
            at.monotonic,
        );
        let feedback = negotiated.facts.feedback();
        let sctp = negotiated
            .facts
            .sctp()
            .map(|facts| Association::new(facts, config.limits, at.monotonic));
        let connection = Self {
            _config: config,
            session: negotiated.facts,
            _time: MonotonicObserver::starting_at(at),
            _subsystems: SubsystemSlots {
                transport,
                ingress,
                egress,
                sctp,
            },
            runtime: Runtime::new(feedback),
            _not_sync: PhantomData,
        };
        Ok(AcceptedConnection {
            connection,
            answer: negotiated.answer,
            session,
        })
    }
}

fn close_reason(error: TransportError) -> CloseReason {
    match error {
        TransportError::Timeout => CloseReason::Timeout,
        _ => CloseReason::TransportFailure,
    }
}

pub(crate) struct EntropyConsumer {
    seed: [u8; 32],
    counter: u32,
}

impl EntropyConsumer {
    pub(crate) fn new(entropy: ConnectionEntropy) -> Self {
        Self {
            seed: entropy.into_bytes(),
            counter: 0,
        }
    }

    pub(crate) fn take<const N: usize>(&mut self, domain: &[u8]) -> [u8; N] {
        let mut output = [0; N];
        for chunk in output.chunks_mut(32) {
            let mut hash = Sha256::new();
            hash.update(b"pulsebeam-rtc-v3\0");
            hash.update(domain);
            hash.update(self.counter.to_be_bytes());
            hash.update(self.seed);
            let digest = hash.finalize();
            for (target, source) in chunk.iter_mut().zip(digest) {
                *target = source;
            }
            self.counter = self.counter.wrapping_add(1);
        }
        output
    }
}

impl Drop for EntropyConsumer {
    fn drop(&mut self) {
        self.seed.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        NetworkPolicy, PeerFixture, RtpImpairment, assert_initial_probe_windows,
        assert_probe_windows, forwarded,
    };
    use crate::{ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata};
    use std::{sync::Arc, time::Duration};

    #[test]
    fn potential_only_sender_obtains_padding_feedback_without_media() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.configure_rtc_wakeup_quantum(Duration::from_millis(1));
        fixture.configure_network(
            0x6602,
            NetworkPolicy {
                delay: Duration::from_millis(50),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_time_quantum(Duration::from_millis(5));
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some({
                let (selected_bitrate, potential_bitrate) = (
                    MediaPayloadBitrate::from_bps(0),
                    MediaPayloadBitrate::from_bps(2_000_000),
                );
                pulsebeam_rtc::SenderAssignment {
                    selected_bitrate,
                    potential_bitrate,
                    ..fixture.assignment(fixture.sender)
                }
            }),
        });
        let received_before = fixture
            .connection
            .runtime
            .commit
            .history
            .counters()
            .received;
        let mut saw_fresh_delivery = false;
        let mut trace = Vec::new();
        for tick in 1..=160 {
            fixture.drive_for(Duration::from_millis(50));
            let envelope = fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("controller envelope");
            saw_fresh_delivery |=
                !envelope.feedback_stale && envelope.delivered_rtp_transport_rate > 0;
            if tick % 20 == 0 {
                trace.push((tick / 20, envelope));
            }
        }
        let stats = fixture.connection.stats().connection;
        assert_eq!(
            stats.feedback,
            Some(crate::PacketFeedbackKind::TransportWide)
        );
        assert_eq!(stats.transmitted_rtp_bytes, 0);
        assert!(stats.transmitted_padding_bytes > 0);
        let initial_started = fixture
            .connection
            ._subsystems
            .egress
            .test_initial_probe_started_at()
            .expect("ordinary startup probe");
        assert_initial_probe_windows(fixture.emitted_rtp(), initial_started, 2_000_000);
        assert!(
            saw_fresh_delivery
                && fixture
                    .connection
                    .runtime
                    .commit
                    .history
                    .counters()
                    .received
                    > received_before,
            "potential-only feedback: stats={stats:?} trace={trace:?}",
        );
        fixture.drive_for(Duration::from_secs(22));
        let bootstrap = fixture.connection.stats().connection;
        assert_eq!(bootstrap.transmitted_rtp_bytes, 0);
        assert!(
            bootstrap.media_budget.as_bps() >= 1_100_000,
            "potential-only demand must bootstrap native capacity within 30s: stats={bootstrap:?}",
        );
        assert_initial_probe_windows(fixture.emitted_rtp(), initial_started, 2_000_000);
    }

    #[test]
    fn queued_sctp_output_precedes_potential_only_padding() {
        let mut fixture = PeerFixture::connected_datachannels();
        let mut channel = None;
        let mut peer_opened = false;
        while channel.is_none() || !peer_opened {
            match fixture.next_data_event() {
                crate::test_support::FixtureDataEvent::ConnectionOpened(id) => channel = Some(id),
                crate::test_support::FixtureDataEvent::PeerOpened => peer_opened = true,
                _ => {}
            }
        }
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some({
                let (selected_bitrate, potential_bitrate) = (
                    MediaPayloadBitrate::from_bps(0),
                    MediaPayloadBitrate::from_bps(2_000_000),
                );
                pulsebeam_rtc::SenderAssignment {
                    selected_bitrate,
                    potential_bitrate,
                    ..fixture.assignment(fixture.sender)
                }
            }),
        });
        fixture.command(Command::SendData {
            channel: channel.expect("opened channel"),
            message: crate::DataMessage::Binary(bytes::Bytes::from(vec![7; 20_000])),
        });
        let before = fixture.connection.stats().connection;
        let at = fixture.at();
        let mut polls = 0;
        while fixture
            .connection
            ._subsystems
            .sctp
            .as_ref()
            .is_some_and(Association::has_packet)
        {
            assert!(polls < 128, "queued native output drains with bounded work");
            polls += 1;
            let _ = fixture.connection.poll(at);
            assert_eq!(
                fixture
                    .connection
                    .stats()
                    .connection
                    .transmitted_padding_bytes,
                before.transmitted_padding_bytes,
            );
        }
        assert!(polls > 0);
        assert!(
            fixture.connection.stats().connection.transmitted_sctp_bytes
                > before.transmitted_sctp_bytes
        );
        assert!(
            fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("controller envelope")
                .probe_permitted
        );
    }

    #[test]
    fn queued_media_and_repair_prevent_padding_without_blocking_real_rtp() {
        for repair in [false, true] {
            let mut fixture = if repair {
                PeerFixture::connected_video()
            } else {
                PeerFixture::connected()
            };
            if repair {
                fixture.command(Command::SetSenderAssignment {
                    sender: fixture.sender,
                    assignment: Some(pulsebeam_rtc::SenderAssignment {
                        playout_delay: None,
                        ..crate::test_support::sender_assignment(MediaPayloadBitrate::from_bps(
                            1_000_000,
                        ))
                    }),
                });
            }
            let packet = fixture.send_source(if repair { &[0x65; 32] } else { &[7; 32] });
            let at = fixture.at();
            fixture.connection._subsystems.egress.update_controller(
                at,
                Some((1, true)),
                &[],
                &[],
                Duration::ZERO,
                false,
                0,
            );
            fixture
                .try_command(Command::SendMedia {
                    sender: fixture.sender,
                    media: forwarded(packet, 0),
                })
                .expect("admit queued priority fixture");
            if repair {
                let header = fixture.receive_video_wire_header();
                fixture
                    .connection
                    ._subsystems
                    .egress
                    .request_repair(header.ssrc, header.sequence_number);
            }
            fixture.command(Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some({
                    let (selected_bitrate, potential_bitrate) = (
                        MediaPayloadBitrate::from_bps(100_000),
                        MediaPayloadBitrate::from_bps(2_000_000),
                    );
                    pulsebeam_rtc::SenderAssignment {
                        selected_bitrate,
                        potential_bitrate,
                        ..fixture.assignment(fixture.sender)
                    }
                }),
            });
            let at = fixture.at();
            let subsystems = &mut fixture.connection._subsystems;
            subsystems.egress.update_controller(
                at,
                Some((1, true)),
                &[],
                &[],
                Duration::ZERO,
                false,
                0,
            );
            assert!(subsystems.egress.test_envelope().unwrap().probe_permitted);
            let (_, senders) = subsystems.egress.stats();
            assert_eq!(
                senders[0]
                    .assignment
                    .expect("bound sender")
                    .selected_bitrate
                    .as_bps(),
                100_000
            );
            assert_eq!(senders[0].queued_packets, usize::from(!repair));
            assert!(matches!(
                subsystems
                    .egress
                    .test_prepare_padding(at, 0, &mut subsystems.transport),
                PrepareResult::Blocked
            ));
            let mut service = None;
            for _ in 0..4 {
                assert!(matches!(
                    subsystems
                        .egress
                        .prepare_one(at, 0, &mut subsystems.transport, true),
                    PrepareResult::Prepared
                ));
                while let Some(transmit) = subsystems.transport.poll_prepared() {
                    if let Some(rtp) = transmit.rtp {
                        service = Some(rtp.service);
                    } else {
                        assert!(matches!(
                            transmit.kind,
                            DatagramKind::Rtcp | DatagramKind::Dtls
                        ));
                    }
                    if transmit.rtp.is_some() || transmit.kind == DatagramKind::Rtcp {
                        assert!(subsystems.egress.preflight_commit(&transmit));
                        subsystems.egress.commit(at, &transmit);
                    }
                }
                if service.is_some() {
                    break;
                }
            }
            assert_eq!(
                service,
                Some(if repair {
                    crate::transport::RtpService::Repair
                } else {
                    crate::transport::RtpService::Original
                })
            );
        }
    }

    #[test]
    #[should_panic(expected = "emitted probe budget")]
    fn probe_oracle_rejects_credit_from_later_emission_at_same_instant() {
        let now = Instant::now();
        assert_probe_windows(&[(now, 48 * 1024 + 1, true), (now, 19, false)]);
    }

    #[test]
    #[should_panic(expected = "emitted probe budget")]
    fn probe_oracle_rejects_overspend_when_media_credit_expires() {
        let now = Instant::now();
        assert_probe_windows(&[
            (now, 19_000, false),
            (now + Duration::from_secs(1), 48 * 1024 + 1_000, true),
        ]);
    }

    #[test]
    #[should_panic(expected = "emitted probe budget")]
    fn initial_probe_oracle_rejects_more_than_demand_sized_service() {
        let now = Instant::now();
        assert_initial_probe_windows(&[(now, 60_001, true)], now, 1_200_000);
    }

    #[test]
    #[should_panic(expected = "emitted probe budget")]
    fn initial_probe_oracle_rejects_refreshed_credit_after_initial_period() {
        let now = Instant::now();
        assert_initial_probe_windows(
            &[(now + Duration::from_secs(5), 48 * 1024 + 1, true)],
            now,
            2_000_000,
        );
    }

    #[test]
    fn initial_probe_oracle_retains_spent_service_without_granting_new_credit() {
        let now = Instant::now();
        assert_initial_probe_windows(
            &[(now + Duration::from_secs(4), 96 * 1024, true)],
            now,
            2_000_000,
        );
    }

    #[test]
    fn production_feedback_outage_path_replacement_and_media_pause_recover() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.configure_network(
            0x6601,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.configure_time_quantum(Duration::from_millis(5));
        let mut source = crate::test_source::ComponentSource::audio(1);
        let mut policy =
            crate::test_support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
        policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
        policy.selected_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        policy.potential_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(policy),
        });
        let mut frame = 0;
        for (feedback_enabled, replace_path) in
            [(true, false), (false, false), (true, false), (true, true)]
        {
            fixture.set_feedback_enabled(feedback_enabled);
            if replace_path {
                assert_probe_windows(fixture.emitted_rtp());
                for _ in 0..2 {
                    fixture
                        .connection
                        ._subsystems
                        .transport
                        .reselect_path_for_test()
                        .expect("replace selected path");
                }
                fixture.command(Command::SetSenderAssignment {
                    sender: fixture.sender,
                    assignment: Some(policy),
                });
            }
            let start = fixture.at().monotonic;
            let before = fixture.connection.stats().connection.transmitted_rtp_bytes;
            for tick in 1..=600 {
                let at = start + Duration::from_millis(tick * 5);
                fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
                frame += 1;
                let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), frame);
                match fixture.try_command(Command::SendMedia {
                    sender: fixture.sender,
                    media,
                }) {
                    Ok(())
                    | Err(pulsebeam_rtc::RejectedCommand {
                        reason: CommandError::WouldBlock,
                        ..
                    }) => {}
                    Err(error) => panic!("unexpected media error: {error:?}"),
                }
            }
            let envelope = fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("controller output");
            assert_eq!(envelope.feedback_stale, !feedback_enabled);
            assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > before);
        }
        fixture.drive_for(Duration::from_secs(1));
        let paused = fixture.connection.stats().connection.transmitted_rtp_bytes;
        fixture.drive_for(Duration::from_secs(2));
        assert_eq!(
            fixture.connection.stats().connection.transmitted_rtp_bytes,
            paused
        );
        assert!(
            fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("controller output")
                .application_limited
        );
        let start = fixture.at().monotonic;
        for tick in 1..=600 {
            let at = start + Duration::from_millis(tick * 5);
            fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
            frame += 1;
            let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), frame);
            match fixture.try_command(Command::SendMedia {
                sender: fixture.sender,
                media,
            }) {
                Ok(())
                | Err(pulsebeam_rtc::RejectedCommand {
                    reason: CommandError::WouldBlock,
                    ..
                }) => {}
                Err(error) => panic!("unexpected media error: {error:?}"),
            }
        }
        assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > paused);
        assert!(
            !fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("controller output")
                .feedback_stale
        );
        assert_probe_windows(fixture.emitted_rtp());
        fixture.drive_for(Duration::from_secs(1));
        let stopped = fixture.connection.stats().connection.transmitted_rtp_bytes;
        fixture.drive_for(Duration::from_secs(6));
        assert_eq!(
            fixture.connection.stats().connection.transmitted_rtp_bytes,
            stopped
        );
        assert_probe_windows(fixture.emitted_rtp());
    }

    #[test]
    fn production_random_burst_policer_and_heterogeneous_traces() {
        for (name, impairment) in [
            (
                "random-1-percent",
                Some(RtpImpairment::Random { per_mille: 10 }),
            ),
            (
                "burst-6-of-200",
                Some(RtpImpairment::Burst {
                    every: 200,
                    length: 6,
                }),
            ),
            (
                "policer-125kbps-3000bytes",
                Some(RtpImpairment::Policer {
                    bits_per_second: 125_000,
                    burst_bytes: 3_000,
                }),
            ),
            ("cubic-like", None),
            ("bbr-like", None),
        ] {
            let mut fixture = PeerFixture::connected().without_egress_observation();
            fixture.configure_network(
                0x7701,
                NetworkPolicy {
                    delay: Duration::from_millis(25),
                    ..NetworkPolicy::default()
                },
            );
            fixture.configure_bottleneck(4_000_000);
            fixture.configure_time_quantum(Duration::from_millis(5));
            fixture.observe_native_target(|connection| {
                connection
                    ._subsystems
                    .egress
                    .test_envelope()
                    .expect("envelope")
                    .native_queue_delay_target
            });
            let delivered_before_observation = fixture.delivered_rtp().len();
            if let Some(impairment) = impairment {
                fixture.configure_impairment(impairment);
            }
            let mut source = crate::test_source::ComponentSource::audio(1);
            let mut policy = crate::test_support::sender_assignment(
                pulsebeam_rtc::MediaPayloadBitrate::from_bps(0),
            );
            policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
            policy.selected_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
            policy.potential_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
            fixture.command(Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some(policy),
            });
            let start = fixture.at().monotonic;
            let mut policed = false;
            let mut cross_bytes = 0;
            for tick in 1..=3_000_u64 {
                let at = start + Duration::from_millis(tick * 5);
                fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
                // Open-loop pacing traces, not TCP implementations or equal-share promises.
                let cross_rate = match name {
                    "cubic-like" => {
                        let phase = i64::try_from((tick * 5) % 4_000).expect("phase") - 2_000;
                        u64::try_from(750_000 + phase.pow(3) / 16_000)
                            .expect("positive cubic trace")
                    }
                    "bbr-like" => match (tick / 10) % 8 {
                        0 => 1_250_000,
                        1 => 750_000,
                        _ => 1_000_000,
                    },
                    _ => 0,
                };
                if cross_rate > 0 {
                    let bytes = cross_rate * 5 / 8_000;
                    fixture.inject_cross_traffic(bytes);
                    cross_bytes += bytes;
                }
                for packet in 0..2 {
                    let packet_at = at + Duration::from_micros(packet * 2_500);
                    fixture.drive_for(packet_at.saturating_duration_since(fixture.at().monotonic));
                    let media = forwarded(
                        source.sample(fixture.at(), &[0x5a; 1_000]),
                        tick * 2 + packet,
                    );
                    match fixture.try_command(Command::SendMedia {
                        sender: fixture.sender,
                        media,
                    }) {
                        Ok(())
                        | Err(pulsebeam_rtc::RejectedCommand {
                            reason: CommandError::WouldBlock,
                            ..
                        }) => {}
                        Err(error) => panic!("{name} admission: {error:?}"),
                    }
                }
                let stats = fixture.connection.stats();
                assert!(stats.connection.queued_media_bytes <= 8 * 1024 * 1024);
                assert!(stats.connection.rtp_bytes_in_flight <= 8 * 1024 * 1024);
                let envelope = fixture
                    .connection
                    ._subsystems
                    .egress
                    .test_envelope()
                    .expect("envelope");
                policed |= envelope.policer_detected;
                assert!(
                    !envelope.feedback_stale,
                    "{name}: covering feedback remains live"
                );
            }
            fixture.drive_for(Duration::from_secs(2));
            let delivered: u64 = fixture
                .delivered_rtp()
                .iter()
                .filter(|(at, _)| {
                    *at >= start + Duration::from_secs(10) && *at < start + Duration::from_secs(15)
                })
                .map(|(_, bytes)| *bytes)
                .sum();
            let (sent, dropped) = fixture.impairment_counts();
            assert!(
                delivered > 10_000,
                "{name}: sustained late delivery, not startup-only success"
            );
            if impairment.is_some() {
                assert!(dropped > 0 && dropped < sent);
            }
            if matches!(impairment, Some(RtpImpairment::Policer { .. })) {
                assert!(policed, "production policer detection must be exercised");
            }
            let mut queue = fixture
                .rtp_queue_samples()
                .iter()
                .map(|(_, delay, _)| *delay)
                .collect::<Vec<_>>();
            queue.sort_unstable();
            assert_eq!(
                queue.len(),
                fixture.delivered_rtp().len() - delivered_before_observation,
                "queue samples must include every delivered RTP in the observation window"
            );
            assert_probe_windows(fixture.emitted_rtp());
            eprintln!(
                "production profile={name} seed=0x7701 link=4Mbps RTT=50ms quantum=5ms offered=3.2Mbps desired=4Mbps duration=15s sent={sent} drops={dropped} late_delivered={delivered} cross_bytes={cross_bytes} policed={policed} queue_p50={:?} queue_p95={:?} queue_p99={:?}",
                queue[(queue.len() * 50).div_ceil(100) - 1],
                queue[(queue.len() * 95).div_ceil(100) - 1],
                queue[(queue.len() * 99).div_ceil(100) - 1]
            );
        }
    }

    #[test]
    fn production_demand_limited_video_vbr_keyframes_keep_queue_bound() {
        let mut fixture = PeerFixture::connected_video().without_egress_observation();
        fixture.configure_network(
            0x7702,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.configure_time_quantum(Duration::from_millis(5));
        fixture.observe_native_target(|connection| {
            connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("envelope")
                .native_queue_delay_target
        });
        let mut source = crate::test_source::ComponentSource::video(1);
        let mut policy =
            crate::test_support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
        policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
        policy.selected_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        policy.potential_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(policy),
        });
        let start = fixture.at().monotonic;
        for frame in 1..=1_600_u64 {
            let at = start + Duration::from_millis(frame * 25);
            fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
            let key = frame % 40 == 1;
            let size = if key {
                1_000
            } else if frame % 2 == 0 {
                200
            } else {
                600
            };
            let mut payload = vec![0x5a; size];
            // Synthetic H.264 slices: IDR for keyframes, non-IDR for dependents.
            payload[0] = if key { 0x65 } else { 0x41 };
            let mut media = forwarded(source.sample(fixture.at(), &payload), frame);
            media.frame.random_access = key;
            media.frame.dependencies = FrameDependencies::known(if key {
                vec![]
            } else {
                vec![FrameId::from_value(frame - 1)]
            })
            .expect("frame dependencies");
            fixture.command(Command::SendMedia {
                sender: fixture.sender,
                media,
            });
        }
        fixture.drive_for(Duration::from_secs(2));
        // Demand-limited 40fps VBR: fixed 2s warmup and 36s interval, including keyframes.
        let samples = fixture
            .rtp_queue_samples()
            .iter()
            .filter(|(at, _, _)| {
                *at >= start + Duration::from_secs(2) && *at < start + Duration::from_secs(38)
            })
            .collect::<Vec<_>>();
        let mut raw = samples
            .iter()
            .map(|(_, delay, _)| *delay)
            .collect::<Vec<_>>();
        let mut excess = samples
            .iter()
            .map(|(_, delay, target)| delay.saturating_sub(*target))
            .collect::<Vec<_>>();
        raw.sort_unstable();
        excess.sort_unstable();
        assert!(samples.len() >= 1_000);
        assert_eq!(fixture.network_counters().1, 0);
        assert!(excess[(excess.len() * 99).div_ceil(100) - 1] <= Duration::from_millis(10));
        assert_probe_windows(fixture.emitted_rtp());
        eprintln!(
            "production demand-limited VBR video seed=0x7702 offered_payload=128kbps link=2Mbps RTT=50ms quantum=5ms frames=1600 keyframe_every=40 sizes=1000/200/600B samples={} drops=0 raw_p50={:?} raw_p95={:?} raw_p99={:?}",
            samples.len(),
            raw[(raw.len() * 50).div_ceil(100) - 1],
            raw[(raw.len() * 95).div_ceil(100) - 1],
            raw[(raw.len() * 99).div_ceil(100) - 1]
        );
    }

    #[test]
    fn production_queue_sojourn_tracks_contemporaneous_native_target() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.configure_network(
            0x5501,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.configure_time_quantum(Duration::from_millis(2));
        fixture.observe_native_target(|connection| {
            connection._subsystems.egress.native_queue_delay_target()
        });
        let mut policy =
            crate::test_support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
        policy.selected_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
        policy.potential_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
        policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(policy),
        });
        let start = fixture.at().monotonic;
        let mut source = crate::test_source::ComponentSource::audio(1);
        for id in 1..=6_000_u64 {
            let tick = start + Duration::from_millis(id * 2);
            fixture.drive_for(tick.saturating_duration_since(fixture.at().monotonic));
            let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), id);
            match fixture.try_command(Command::SendMedia {
                sender: fixture.sender,
                media,
            }) {
                Ok(())
                | Err(pulsebeam_rtc::RejectedCommand {
                    reason: CommandError::WouldBlock,
                    ..
                }) => {}
                Err(error) => panic!("unexpected media error: {error:?}"),
            }
        }
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some({
                let (selected_bitrate, potential_bitrate) = (
                    MediaPayloadBitrate::from_bps(0),
                    MediaPayloadBitrate::from_bps(0),
                );
                pulsebeam_rtc::SenderAssignment {
                    selected_bitrate,
                    potential_bitrate,
                    ..fixture.assignment(fixture.sender)
                }
            }),
        });
        fixture.drive_for(Duration::from_secs(2));
        assert_eq!(fixture.network_counters().1, 0);
        assert_eq!(
            fixture
                .delivered_rtp()
                .iter()
                .map(|(_, bytes)| *bytes)
                .sum::<u64>(),
            fixture
                .connection
                .stats()
                .connection
                .transmitted_rtp_bytes
                .saturating_add(
                    fixture
                        .connection
                        .stats()
                        .connection
                        .transmitted_padding_bytes
                ),
            "only delivered RTP may enter queue percentiles"
        );
        let stable_start = start + Duration::from_secs(2);
        let stable_end = stable_start + Duration::from_secs(10);
        let eligible = |at: &Instant| {
            *at >= stable_start
                && *at < stable_end
                && !fixture.emitted_rtp().iter().any(|(probe_at, _, probe)| {
                    *probe
                        && *at >= *probe_at
                        && at.saturating_duration_since(*probe_at) < Duration::from_millis(120)
                })
        };
        let mut raw = fixture
            .rtp_queue_samples()
            .iter()
            .filter(|(at, _, _)| eligible(at))
            .map(|(_, sojourn, _)| *sojourn)
            .collect::<Vec<_>>();
        raw.sort_unstable();
        let mut excess = fixture
            .rtp_queue_samples()
            .iter()
            .filter(|(at, _, _)| eligible(at))
            .map(|(_, sojourn, target)| sojourn.saturating_sub(*target))
            .collect::<Vec<_>>();
        excess.sort_unstable();
        assert!(excess.len() >= 1_000);
        let p99 = excess[(excess.len() * 99).div_ceil(100) - 1];
        eprintln!(
            "production queue seed=0x5501 samples={} raw_p50={:?} raw_p95={:?} raw_p99={:?} excess_p99={p99:?}",
            excess.len(),
            raw[(raw.len() * 50).div_ceil(100) - 1],
            raw[(raw.len() * 95).div_ceil(100) - 1],
            raw[(raw.len() * 99).div_ceil(100) - 1],
        );
        assert!(p99 <= Duration::from_millis(10));
    }

    #[test]
    fn tightening_delivery_drops_queued_dependents_but_preserves_independent_frame() {
        let mut fixture = PeerFixture::connected();
        fixture.drive_for(Duration::from_millis(200));
        let mut source = PeerFixture::connected();
        let bytes = source.send_source(&[0x5a; 100]).bytes().clone();
        let at = fixture.at();
        let mut policy =
            crate::test_support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
        policy.delivery.max_media_age = Duration::from_millis(500);
        policy.delivery.max_queue_residence = Duration::from_millis(500);
        policy.selected_bitrate = crate::MediaPayloadBitrate::from_bps(2_000_000);
        policy.potential_bitrate = crate::MediaPayloadBitrate::from_bps(2_000_000);
        fixture.command(Command::RecoverSenderMedia {
            sender: fixture.sender,
            assignment: policy,
        });
        for (id, offset, dependencies, random_access) in [
            (1, 0, vec![], true),
            (2, 100, vec![FrameId::from_value(1)], false),
            (3, 200, vec![], true),
        ] {
            let packet = crate::MediaPacket::new(
                bytes.clone(),
                at.global
                    .checked_add(Duration::from_millis(offset))
                    .expect("media global time"),
                Arc::from([]),
            );
            fixture
                .connection
                .command(
                    at,
                    Command::SendMedia {
                        sender: fixture.sender,
                        media: ForwardedMedia {
                            source: pulsebeam_rtc::SourceIncarnation::from_value(1),
                            packet,
                            frame: FrameMetadata {
                                descriptor: None,
                                id: FrameId::from_value(id),
                                boundary: FrameBoundary::Complete,
                                random_access,
                                discardable: false,
                                dependencies: FrameDependencies::known(dependencies)
                                    .expect("dependencies"),
                            },
                        },
                    },
                )
                .unwrap_or_else(|error| panic!("frame {id}: {error:?}"));
        }
        assert_eq!(fixture.connection.stats().senders[0].queued_packets, 3);
        policy.delivery.max_media_age = Duration::from_millis(40);
        fixture
            .connection
            .command(
                TimePoint {
                    monotonic: at.monotonic + Duration::from_millis(50),
                    global: at.global.checked_add(Duration::from_millis(50)).unwrap(),
                },
                Command::SetSenderAssignment {
                    sender: fixture.sender,
                    assignment: Some(policy),
                },
            )
            .expect("tighten delivery bound");
        assert_eq!(fixture.connection.stats().senders[0].queued_packets, 1);
    }

    #[test]
    fn low_rate_media_uses_actual_offer_for_application_limited_classification() {
        let mut fixture = PeerFixture::connected();
        let mut source = PeerFixture::connected();
        let mut policy =
            crate::test_support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
        policy.selected_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
        policy.potential_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
        policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(policy),
        });
        for id in 1..=10 {
            source.drive_for(Duration::from_millis(100));
            fixture.drive_for(Duration::from_millis(100));
            let media = forwarded(source.send_source(&[0x5a; 1_000]), id);
            fixture.command(Command::SendMedia {
                sender: fixture.sender,
                media,
            });
        }
        fixture.drive_for(Duration::from_millis(300));
        let (offered, application_limited) = fixture.connection._subsystems.egress.observed_offer();
        assert!(offered <= 100_000, "actual sparse offer={offered}");
        assert!(application_limited);
        assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > 0);
    }

    #[test]
    fn poll_observes_committed_flight_before_batched_ack_retirement() {
        use crate::rtcp::{FeedbackBatch, FeedbackReport, TwccStatus};
        use crate::transport::{PreparedRtpIdentity, RtpService};

        let mut fixture = PeerFixture::connected();
        fixture.set_feedback_enabled(false);
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some({
                let (selected_bitrate, potential_bitrate) = (
                    MediaPayloadBitrate::from_bps(0),
                    MediaPayloadBitrate::from_bps(0),
                );
                pulsebeam_rtc::SenderAssignment {
                    selected_bitrate,
                    potential_bitrate,
                    ..fixture.assignment(fixture.sender)
                }
            }),
        });
        let epoch = fixture
            .connection
            .runtime
            .commit
            .history
            .test_active_epoch()
            .expect("connected path");
        fixture.connection.runtime.commit.history =
            SentHistory::new(crate::PacketFeedbackKind::TransportWide);
        fixture
            .connection
            .runtime
            .commit
            .history
            .path_changed(epoch, true);
        let _ = fixture.connection.poll(fixture.at());
        for sequence in 1..=16 {
            let context = TransmitCommitContext {
                at: fixture.at().monotonic,
                kind: DatagramKind::Rtp,
                wire_len: 1_200,
                path_epoch: Some(epoch),
                rtp: Some(PreparedRtpIdentity {
                    ssrc: 7,
                    sequence,
                    twcc_sequence: Some(sequence),
                    service: RtpService::Padding,
                }),
            };
            fixture
                .connection
                .runtime
                .commit
                .history
                .preflight_commit(context)
                .expect("valid commit");
            fixture
                .connection
                .runtime
                .commit
                .history
                .commit_preflighted(context);
        }
        assert_eq!(
            fixture
                .connection
                .runtime
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight,
            19_200
        );
        assert!(fixture.connection._subsystems.egress.test_flight_peak() < 19_200);
        fixture.advance_clock_without_poll(Duration::from_millis(50));
        fixture
            .connection
            ._subsystems
            .ingress
            .test_queue_feedback(FeedbackBatch {
                received_at: fixture.at(),
                path_epoch: epoch,
                sender_ssrc: 9,
                report: FeedbackReport::Twcc {
                    media_ssrc: 7,
                    base_sequence: 1,
                    reference_time: 0,
                    feedback_count: 1,
                    statuses: vec![TwccStatus::Received { delta_250us: 1 }; 16].into(),
                },
            });
        for _ in 0..32 {
            let output = fixture.connection.poll(fixture.at());
            assert!(
                !matches!(output, Output::Closed(_)),
                "unexpected closure: {output:?}"
            );
            if fixture
                .connection
                .runtime
                .commit
                .history
                .counters()
                .received
                == 16
            {
                break;
            }
        }
        assert_eq!(
            fixture
                .connection
                .runtime
                .commit
                .history
                .counters()
                .received,
            16,
            "queued ACK must retire the committed batch: {:?}",
            fixture.connection.runtime.commit.history.counters(),
        );
        assert!(
            fixture
                .connection
                .runtime
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight
                < 19_200
        );
        assert_eq!(
            fixture.connection._subsystems.egress.test_flight_peak(),
            19_200
        );
    }

    #[test]
    fn authenticated_twcc_feedback_drains_through_the_public_runtime() {
        let mut fixture = crate::test_support::PeerFixture::connected();
        let packet = fixture.send_source(b"feedback");
        for id in 1..=2 {
            fixture
                .connection
                .command(
                    fixture.at(),
                    Command::SendMedia {
                        sender: fixture.sender,
                        media: ForwardedMedia {
                            source: pulsebeam_rtc::SourceIncarnation::from_value(1),
                            packet: packet.clone(),
                            frame: FrameMetadata {
                                descriptor: None,
                                id: FrameId::from_value(id),
                                boundary: FrameBoundary::Complete,
                                random_access: true,
                                discardable: false,
                                dependencies: FrameDependencies::Known(Arc::from([])),
                            },
                        },
                    },
                )
                .expect("media admitted through public command");
            let (header, payload) = fixture.receive_egress();
            assert_eq!(payload, b"feedback");
            assert!(header.ext_vals.transport_cc.is_some());
        }
        fixture.drive_for(std::time::Duration::from_secs(2));

        let counters = fixture.connection.runtime.commit.history.counters();
        assert!(
            counters.received > 0,
            "authenticated peer TWCC must acknowledge the public RTP commit: {counters:?}, peer generated {}",
            fixture.twcc_sent()
        );
        assert!(
            fixture
                .connection
                ._subsystems
                .ingress
                .poll_feedback()
                .is_none()
        );
    }

    #[test]
    fn data_channel_commits_are_excluded_from_rtp_history_and_bytes_in_flight() {
        use crate::test_support::FixtureDataEvent;

        let mut fixture = crate::test_support::PeerFixture::connected_datachannels();
        let mut connection_channel = None;
        let mut peer_opened = false;
        while connection_channel.is_none() || !peer_opened {
            match fixture.next_data_event() {
                FixtureDataEvent::ConnectionOpened(channel) => connection_channel = Some(channel),
                FixtureDataEvent::PeerOpened => peer_opened = true,
                _ => {}
            }
        }
        let before_counters = fixture.connection.runtime.commit.history.counters();
        let before_bif = fixture
            .connection
            .runtime
            .commit
            .history
            .controller_inputs()
            .bytes_in_flight;
        let before_sctp = fixture
            .connection
            ._subsystems
            .sctp
            .as_ref()
            .expect("negotiated SCTP")
            .stats()
            .committed_transport_bytes;

        fixture.command(Command::SendData {
            channel: connection_channel.expect("connection channel"),
            message: crate::DataMessage::Binary(Bytes::from_static(b"separate accounting")),
        });
        while !matches!(
            fixture.next_data_event(),
            FixtureDataEvent::PeerMessage { .. }
        ) {}

        assert_eq!(
            fixture.connection.runtime.commit.history.counters(),
            before_counters
        );
        assert_eq!(
            fixture
                .connection
                .runtime
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight,
            before_bif
        );
        assert!(
            fixture
                .connection
                ._subsystems
                .sctp
                .as_ref()
                .expect("negotiated SCTP")
                .stats()
                .committed_transport_bytes
                > before_sctp
        );
    }
}
