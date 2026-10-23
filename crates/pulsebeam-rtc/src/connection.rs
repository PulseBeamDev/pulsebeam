#![allow(
    clippy::disallowed_types,
    reason = "the public transmit contract requires immutable Bytes payloads"
)]

use std::{
    cell::Cell,
    marker::PhantomData,
    time::{Instant, SystemTime},
};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    AcceptError, CloseReason, ConnectionConfig, ConnectionEntropy, ConnectionState,
    ConnectionStats, ConnectionWarning, DataChannelConfig, DataChannelId, DataMessage, Event,
    MediaPayloadBitrate, NetworkInput, Output, PacketFeedbackKind, ReceiveError, RejectedData,
    RejectedRtp, RtpWrite, SdpAnswer, SdpOffer, SessionInfo, StatsSnapshot, Transmit, WriteError,
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
    session: NegotiatedSessionFacts,
    time: MonotonicObserver,
    transport: Transport,
    ingress: IngressOwner,
    egress: MediaEgress,
    sctp: Option<Association>,
    service: ServiceArbiter,
    commit: CommitCoordinator,
    lifecycle: Lifecycle,
    _not_sync: PhantomData<Cell<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Open,
    Closing { deadline: Instant },
    ClosedPending(CloseReason),
    Closed(CloseReason),
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
        at: Instant,
        prepared: PreparedTransmit,
        egress: Option<&mut MediaEgress>,
    ) -> Result<Transmit, HistoryError> {
        let context = TransmitCommitContext {
            at,
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
        let PreparedTransmit {
            target,
            bytes,
            classification,
            ..
        } = prepared;
        Ok(Transmit {
            kind: classification,
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
    pub fn write_rtp(
        &mut self,
        at: Instant,
        mid: &str,
        packet: RtpWrite,
    ) -> Result<(), RejectedRtp> {
        let at = self.time.observe(at);
        if let Err(reason) = self.check_writable() {
            return Err(RejectedRtp { reason, packet });
        }
        if self.transport.state() != TransportState::Connected {
            return Err(RejectedRtp {
                reason: WriteError::InvalidState,
                packet,
            });
        }
        self.egress
            .admit(at, mid, packet)
            .map_err(|(reason, packet)| RejectedRtp { reason, packet })
    }

    fn check_writable(&self) -> Result<(), WriteError> {
        match self.lifecycle {
            Lifecycle::Open => Ok(()),
            Lifecycle::Closing { .. } => Err(WriteError::InvalidState),
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_) => Err(WriteError::Closed),
        }
    }

    /// Aggregate application demand in media-payload bits/s, not capacity or a reservation.
    /// None and zero clear speculative padding demand without preventing primary RTP.
    pub fn set_desired_bitrate(&mut self, bitrate: Option<MediaPayloadBitrate>) {
        self.egress.set_desired_bitrate(bitrate);
    }

    /// Discard uncommitted writes without resetting primary numbering, repairs or congestion history.
    pub fn discard_pending_rtp(&mut self, mid: &str) -> Result<(), WriteError> {
        self.check_writable()?;
        self.egress.discard_pending(mid)
    }

    pub fn request_keyframe(&mut self, ssrc: u32) -> Result<(), WriteError> {
        self.check_writable()?;
        let packet = self.ingress.keyframe_request(ssrc)?;
        self.transport
            .send_rtcp(&packet)
            .map_err(|_| WriteError::InvalidState)
    }

    pub fn retire_encoding(&mut self, ssrc: u32) -> Result<(), WriteError> {
        self.check_writable()?;
        self.ingress.retire(ssrc)
    }

    pub fn open_data_channel(
        &mut self,
        at: Instant,
        config: DataChannelConfig,
    ) -> Result<(), (WriteError, DataChannelConfig)> {
        let at = self.time.observe(at);
        if let Err(reason) = self.check_writable() {
            return Err((reason, config));
        }
        let Some(sctp) = self.sctp.as_mut() else {
            return Err((WriteError::InvalidState, config));
        };
        sctp.open(at, config)
    }

    pub fn send_data(
        &mut self,
        at: Instant,
        channel: DataChannelId,
        message: DataMessage,
    ) -> Result<(), RejectedData> {
        let at = self.time.observe(at);
        if let Err(reason) = self.check_writable() {
            return Err(RejectedData { reason, message });
        }
        let Some(sctp) = self.sctp.as_mut() else {
            return Err(RejectedData {
                reason: WriteError::UnknownDataChannel(channel),
                message,
            });
        };
        sctp.send(at, channel, message)
            .map_err(|(reason, message)| RejectedData { reason, message })
    }

    pub fn close_data_channel(
        &mut self,
        at: Instant,
        channel: DataChannelId,
    ) -> Result<(), WriteError> {
        let at = self.time.observe(at);
        self.check_writable()?;
        self.sctp
            .as_mut()
            .ok_or(WriteError::UnknownDataChannel(channel))?
            .close_channel(at, channel)
    }

    pub fn close(&mut self, at: Instant, deadline: Instant) -> Result<(), WriteError> {
        let at = self.time.observe(at);
        if matches!(self.lifecycle, Lifecycle::Closing { .. }) {
            return Ok(());
        }
        self.check_writable()?;
        self.lifecycle = Lifecycle::Closing { deadline };
        self.egress.begin_shutdown();
        if let Some(sctp) = self.sctp.as_mut() {
            sctp.begin_shutdown(at, deadline);
        }
        Ok(())
    }

    pub fn receive(
        &mut self,
        at: Instant,
        arrival: Instant,
        input: NetworkInput,
    ) -> Result<(), ReceiveError> {
        let at = self.time.observe(at);
        if matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) {
            return Err(ReceiveError::Closed);
        }
        if arrival > at {
            return Err(ReceiveError::InvalidObservation);
        }
        self.transport
            .receive(at, arrival, input)
            .map_err(|error| match error {
                crate::transport::TransportError::Closed => ReceiveError::Closed,
                crate::transport::TransportError::QueueFull => ReceiveError::InputLimitExceeded,
                _ => ReceiveError::InvalidNetworkEnvelope,
            })
    }

    /// Performs one bounded service turn and returns an owned value.
    /// Callers may collect a bounded batch, then must service it promptly before
    /// requesting more finalized work. Budget exhaustion needs an immediate
    /// continuation; an `Idle` wakeup due now also denotes runnable work.
    pub fn poll(&mut self, at: Instant) -> Output {
        let at = self.time.observe(at);

        match self.lifecycle {
            Lifecycle::Closed(_) => return Output::Idle { next_wakeup: None },
            Lifecycle::ClosedPending(reason) => {
                self.lifecycle = Lifecycle::Closed(reason);
                return Output::Closed(reason);
            }
            Lifecycle::Open | Lifecycle::Closing { .. } => {}
        }

        if self.time.take_warning() {
            return Output::Event(Event::Warning(ConnectionWarning::ClockRegression));
        }

        if !matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && self
            .transport
            .next_deadline()
            .is_some_and(|deadline| at >= deadline)
            && let Err(error) = self.transport.handle_timeout(at)
            && !matches!(error, TransportError::NotDue)
        {
            let reason = close_reason(error);
            self.lifecycle = Lifecycle::Closed(reason);
            return Output::Closed(reason);
        }

        if let Some(event) = self.transport.poll_event()
            && let Some(output) = self.handle_transport_event(at, event)
        {
            return output;
        }

        if let Some(event) = self.ingress.poll_event() {
            return Output::Event(event);
        }

        if let Some(event) = self.sctp.as_mut().and_then(|sctp| sctp.poll_event(at)) {
            return Output::Event(event);
        }

        // Preserve committed flight before expiration or batched ACKs retire it.
        self.egress
            .observe_bytes_in_flight(self.commit.history.controller_inputs().bytes_in_flight);
        self.commit.history.expire(at, MAX_EXPIRATIONS_PER_POLL);

        if let Some(feedback) = self.ingress.poll_feedback() {
            self.commit.history.process_feedback(feedback);
        }
        if self.commit.history.controller_inputs().exhausted {
            self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Output::Closed(CloseReason::TransportFailure);
        }
        while let Some(request) = self.ingress.poll_keyframe() {
            self.egress.request_keyframe(request);
        }
        if let Some(repair) = self.ingress.poll_repair() {
            self.egress
                .request_repair(repair.media_ssrc, repair.sequence);
        }

        let (path_change, feedback, feedback_hold, fresh_network_feedback, bytes_in_flight) = {
            let inputs = self.commit.history.controller_inputs();
            let received_at = inputs.timing.map_or(at, |timing| timing.received_at);
            let origin = self.egress.controller_origin();
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
                    .timing
                    .and_then(|timing| timing.feedback_hold)
                    .unwrap_or_default(),
                inputs.fresh_network_feedback,
                inputs.bytes_in_flight,
            )
        };
        let application_limited = self.egress.update_controller(
            at,
            path_change,
            &feedback,
            feedback_hold,
            fresh_network_feedback,
            bytes_in_flight,
        );
        self.commit.history.clear_controller_inputs();
        self.commit
            .history
            .set_application_limited(application_limited);

        if let Some(event) = self.egress.poll_event() {
            return Output::Event(event);
        }

        if let Lifecycle::Closing { deadline } = self.lifecycle {
            let sctp_stopped = self.sctp.as_ref().is_none_or(Association::is_stopped);
            if sctp_stopped || at >= deadline {
                if let Some(sctp) = self.sctp.as_mut() {
                    sctp.abort();
                }
                let _ = self.transport.close(at);
                if let Some(prepared) = self.transport.poll_prepared() {
                    return match self.commit.commit_transport(at, prepared, None) {
                        Ok(transmit) => {
                            self.lifecycle = Lifecycle::ClosedPending(CloseReason::Graceful);
                            Output::Transmit(transmit)
                        }
                        Err(_) => {
                            self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                            Output::Closed(CloseReason::TransportFailure)
                        }
                    };
                }
                self.lifecycle = Lifecycle::Closed(CloseReason::Graceful);
                return Output::Closed(CloseReason::Graceful);
            }
        }

        if matches!(self.lifecycle, Lifecycle::Open)
            && self.transport.can_send_user_data()
            && let Some(packet) = self.ingress.poll_control(at)
            && let Err(error) = self.transport.send_rtcp(&packet)
            && error != TransportError::QueueFull
        {
            let reason = close_reason(error);
            self.lifecycle = Lifecycle::Closed(reason);
            return Output::Closed(reason);
        }

        if !matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && let Some(prepared) = self.transport.poll_prepared()
        {
            let sctp_wire_len = (prepared.kind == DatagramKind::Sctp).then_some(prepared.wire_len);
            return match self.commit.commit_transport(at, prepared, None) {
                Ok(transmit) => {
                    if let (Some(sctp), Some(wire_len)) = (self.sctp.as_mut(), sctp_wire_len) {
                        sctp.commit_transport_bytes(wire_len);
                    }
                    Output::Transmit(transmit)
                }
                Err(_) => {
                    self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                    Output::Closed(CloseReason::TransportFailure)
                }
            };
        }

        let sctp_ready = self.transport.can_send_user_data()
            && self.sctp.as_ref().is_some_and(Association::has_packet);
        let selected_lane = self.service.select(true, sctp_ready);
        if !matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && selected_lane == Some(UserLane::Sctp)
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if matches!(self.lifecycle, Lifecycle::Open) {
            match self
                .egress
                .prepare_one(at, bytes_in_flight, &mut self.transport, !sctp_ready)
            {
                PrepareResult::Prepared => {
                    let Some(prepared) = self.transport.poll_prepared() else {
                        self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                        return Output::Closed(CloseReason::TransportFailure);
                    };
                    return match self
                        .commit
                        .commit_transport(at, prepared, Some(&mut self.egress))
                    {
                        Ok(transmit) => Output::Transmit(transmit),
                        Err(_) => {
                            self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                            Output::Closed(CloseReason::TransportFailure)
                        }
                    };
                }
                PrepareResult::Fatal => {
                    self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                    return Output::Closed(CloseReason::TransportFailure);
                }
                PrepareResult::Blocked => {}
            }
        }

        if !matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) && selected_lane != Some(UserLane::Sctp)
            && sctp_ready
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if let Some(event) = self.egress.poll_event() {
            return Output::Event(event);
        }

        Output::Idle {
            next_wakeup: (!matches!(self.lifecycle, Lifecycle::Closed(_)))
                .then(|| {
                    [
                        (self.transport.has_pending_output()
                            || self.ingress.has_pending_output()
                            || self.sctp.as_ref().is_some_and(|sctp| {
                                sctp.has_pending_events()
                                    || (self.transport.can_send_user_data() && sctp.has_packet())
                            }))
                        .then_some(at),
                        self.transport.next_deadline(),
                        self.commit.history.next_deadline(),
                        (matches!(self.lifecycle, Lifecycle::Open)
                            && self.transport.can_send_user_data())
                        .then(|| self.ingress.next_deadline())
                        .flatten(),
                        self.transport
                            .can_send_user_data()
                            .then(|| self.egress.next_deadline(at))
                            .flatten(),
                        self.sctp.as_ref().and_then(Association::next_deadline),
                        match self.lifecycle {
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

    fn handle_transport_event(&mut self, at: Instant, event: TransportEvent) -> Option<Output> {
        match event {
            TransportEvent::AuthenticatedPath(path) => {
                Some(Output::Event(Event::AuthenticatedPath(path)))
            }
            TransportEvent::StateChanged(TransportState::Connected) => {
                if let Some(sctp) = self.sctp.as_mut() {
                    sctp.connect(at);
                }
                Some(Output::Event(Event::Connected))
            }
            TransportEvent::StateChanged(TransportState::Failed) => {
                self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                Some(Output::Closed(CloseReason::TransportFailure))
            }
            TransportEvent::StateChanged(_) | TransportEvent::IceStateChanged(_) => None,
            TransportEvent::SelectedPathChanged { current, epoch, .. } => {
                self.commit.history.path_changed(epoch, current.is_some());
                None
            }
            TransportEvent::Rtp {
                arrival,
                path_epoch: _,
                bytes,
                metadata: _,
                ecn,
            } => {
                self.ingress.accept_rtp(arrival, bytes, ecn);
                self.ingress.poll_event().map(Output::Event)
            }
            TransportEvent::Rtcp {
                arrival,
                path_epoch,
                bytes,
            } => {
                let receipts = self.ingress.accept_rtcp(
                    arrival,
                    bytes,
                    path_epoch,
                    self.session.feedback(),
                    self.transport.smoothed_rtt(),
                );
                for receipt in receipts {
                    self.egress.observe_receipt(arrival, receipt);
                }
                None
            }
            TransportEvent::Data(bytes) => {
                if let Some(sctp) = self.sctp.as_mut() {
                    sctp.handle_input(at, &bytes);
                }
                None
            }
            TransportEvent::Closed => {
                self.lifecycle = Lifecycle::Closed(CloseReason::Graceful);
                Some(Output::Closed(CloseReason::Graceful))
            }
        }
    }

    fn prepare_sctp(&mut self, at: Instant) -> Option<Output> {
        if !self.transport.can_send_user_data() {
            return None;
        }
        let packet = self.sctp.as_mut()?.poll_packet()?;
        if self.transport.send_sctp(&packet, at).is_err() {
            self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Some(Output::Closed(CloseReason::TransportFailure));
        }
        let Some(prepared) = self.transport.poll_prepared() else {
            self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
            return Some(Output::Closed(CloseReason::TransportFailure));
        };
        let wire_len = prepared.wire_len;
        match self.commit.commit_transport(at, prepared, None) {
            Ok(transmit) => {
                if let Some(sctp) = self.sctp.as_mut() {
                    sctp.commit_transport_bytes(wire_len);
                }
                Some(Output::Transmit(transmit))
            }
            Err(_) => {
                self.lifecycle = Lifecycle::Closed(CloseReason::TransportFailure);
                Some(Output::Closed(CloseReason::TransportFailure))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_pacing_profile(&self) -> (u64, u64) {
        self.egress.test_pacing_profile()
    }

    pub fn stats(&self) -> StatsSnapshot {
        let (egress, senders) = self.egress.stats();
        let history = self.commit.history.counters();
        let sctp = self.sctp.as_ref().map(Association::stats);
        let (state, close_reason) = match self.lifecycle {
            Lifecycle::Open => (ConnectionState::Open, None),
            Lifecycle::Closing { .. } => (ConnectionState::Closing, None),
            Lifecycle::ClosedPending(reason) | Lifecycle::Closed(reason) => {
                (ConnectionState::Closed, Some(reason))
            }
        };
        let committed = self.commit.counters;
        StatsSnapshot {
            connection: ConnectionStats {
                state,
                feedback: self.session.feedback(),
                close_reason,
                clock_regressions: self.time.regressions(),
                rtp_bytes_in_flight: self.commit.history.controller_inputs().bytes_in_flight,
                bandwidth_estimate: MediaPayloadBitrate::from_bps(egress.bandwidth_estimate),
                pacing_bitrate: egress.pacing_bitrate,
                queued_media_bytes: egress.queued_transport_bytes,
                buffered_data_bytes: sctp.map_or(0, |stats| stats.buffered_payload_bytes),
                transmitted_rtp_bytes: committed.rtp_bytes,
                transmitted_rtcp_bytes: committed.rtcp_bytes,
                transmitted_sctp_bytes: committed.sctp_bytes,
                transmitted_protocol_bytes: committed.protocol_bytes,
                transmitted_padding_bytes: committed.padding_bytes,
                dropped_network_inputs: self
                    .transport
                    .dropped_inputs()
                    .saturating_add(self.ingress.dropped()),
                unknown_feedback: history.unknown_feedback,
                duplicate_feedback: history.duplicate_feedback,
                stale_feedback: history.stale_feedback,
                wrong_path_feedback: history.wrong_path_feedback,
            },
            senders,
            encodings: self.ingress.stats(),
            data_channels: self
                .sctp
                .as_ref()
                .map_or_else(Vec::new, Association::channel_stats),
        }
    }

    pub fn abort(&mut self) {
        if matches!(
            self.lifecycle,
            Lifecycle::ClosedPending(_) | Lifecycle::Closed(_)
        ) {
            return;
        }
        self.transport.abort();
        self.ingress.abort();
        self.egress.abort();
        if let Some(sctp) = self.sctp.as_mut() {
            sctp.abort();
        }
        self.lifecycle = Lifecycle::ClosedPending(CloseReason::Aborted);
    }

    pub fn accept(
        config: ConnectionConfig,
        offer: SdpOffer,
        at: Instant,
        wallclock: SystemTime,
        entropy: ConnectionEntropy,
    ) -> Result<AcceptedConnection, AcceptError> {
        let config = config.validate()?;
        if config.local_candidates.is_empty() {
            return Err(AcceptError::InvalidConfiguration);
        }
        let mut entropy = EntropyConsumer::new(entropy);
        let negotiated = negotiation::negotiate(&config, &offer, at, &mut entropy)?;
        let session = negotiated.session.clone();
        let transport = Transport::from_session(&negotiated.facts, at)
            .map_err(|_| AcceptError::CryptographicFailure)?;
        let ingress = IngressOwner::new(
            negotiated.facts.ingress_media(),
            config.limits.max_unsignaled_encodings,
            config.limits.max_queued_media_bytes,
            u32::from_be_bytes(entropy.take(b"receive-rtcp-ssrc")).max(1),
            crate::receive_feedback::ReceiveFeedback::new(negotiated.facts.inbound_feedback(), at),
        );
        let egress = MediaEgress::new(
            negotiated.facts.egress_senders(),
            negotiated.facts.protocol_randomness(),
            config.limits.max_queued_media_bytes,
            config.limits.max_retransmission_bytes,
            at,
            crate::time::ProtocolClock::new(at, wallclock),
        );
        let feedback = negotiated.facts.feedback();
        let sctp = negotiated
            .facts
            .sctp()
            .map(|facts| Association::new(facts, config.limits, at));
        let connection = Self {
            session: negotiated.facts,
            time: MonotonicObserver::starting_at(at),
            transport,
            ingress,
            egress,
            sctp,
            service: ServiceArbiter::default(),
            commit: CommitCoordinator::new(feedback),
            lifecycle: Lifecycle::Open,
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
    use crate::test_support::{NetworkPolicy, PeerFixture, assert_native_rtp_pacing, sample};
    use std::time::Duration;
    #[test]
    fn aggregate_demand_obtains_native_rtx_feedback_without_primary_media() {
        let mut fixture = PeerFixture::connected_video().without_egress_observation();
        fixture.observe_pacing_profile(Connection::test_pacing_profile);
        fixture.configure_rtc_wakeup_quantum(Duration::from_millis(1));
        fixture.configure_network(
            0x6602,
            NetworkPolicy {
                delay: Duration::from_millis(50),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(4_000_000);
        fixture.observe_native_target(|connection| {
            let envelope = connection.egress.test_envelope().expect("native envelope");
            assert!(
                connection.stats().connection.rtp_bytes_in_flight
                    <= envelope.max_rtp_bytes_in_flight
            );
            if let Some(rate) = connection.egress.test_startup_probe_rate() {
                assert!(
                    rate <= envelope.pacing_transport_rate,
                    "startup uses the native transport-byte pacing ceiling"
                );
            }
            envelope.native_queue_delay_target
        });
        fixture.configure_time_quantum(Duration::from_millis(5));
        fixture.drive_for(Duration::from_millis(200));
        assert_eq!(
            fixture
                .connection
                .stats()
                .connection
                .transmitted_padding_bytes,
            0
        );
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(0)));
        fixture.drive_for(Duration::from_millis(200));
        assert_eq!(
            fixture
                .connection
                .stats()
                .connection
                .transmitted_padding_bytes,
            0
        );
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000)));
        let received_before = fixture.connection.commit.history.counters().received;
        let mut saw_fresh_delivery = false;
        let mut trace = Vec::new();
        for tick in 1..=160 {
            fixture.drive_for(Duration::from_millis(50));
            let envelope = fixture
                .connection
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
            .egress
            .test_initial_probe_started_at()
            .expect("ordinary startup probe");
        assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
        assert!(
            saw_fresh_delivery
                && fixture.connection.commit.history.counters().received > received_before,
            "aggregate-demand feedback: stats={stats:?} trace={trace:?}",
        );
        fixture.drive_for(Duration::from_secs(22));
        let bootstrap = fixture.connection.stats().connection;
        assert_eq!(bootstrap.transmitted_rtp_bytes, 0);
        assert!(
            bootstrap.bandwidth_estimate.as_bps() >= 1_100_000,
            "aggregate-demand demand must bootstrap native capacity within 30s: stats={bootstrap:?}",
        );
        assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
        assert_eq!(
            fixture.connection.egress.test_initial_probe_started_at(),
            Some(initial_started)
        );
        fixture.set_desired_bitrate(None);
        let padding = fixture
            .connection
            .stats()
            .connection
            .transmitted_padding_bytes;
        fixture.drive_for(Duration::from_secs(1));
        assert_eq!(
            fixture
                .connection
                .stats()
                .connection
                .transmitted_padding_bytes,
            padding
        );
    }

    #[test]
    fn production_no_rtx_feedback_outage_path_replacement_and_pause_recover() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.observe_pacing_profile(Connection::test_pacing_profile);
        fixture.configure_network(
            0x6601,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.configure_time_quantum(Duration::from_millis(5));
        assert!(
            fixture
                .connection
                .session
                .egress_senders()
                .iter()
                .all(|sender| sender.retransmission_payload_type.is_none())
        );
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000)));
        let mid = fixture.sender_mid.clone();
        let mut frame = 0;
        for (feedback_enabled, replace_path) in
            [(true, false), (false, false), (true, false), (true, true)]
        {
            fixture.set_feedback_enabled(feedback_enabled);
            if replace_path {
                assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
                for _ in 0..2 {
                    fixture
                        .connection
                        .transport
                        .reselect_path_for_test()
                        .expect("replace selected path");
                }
            }
            let start = fixture.at();
            let before = fixture.connection.stats().connection.transmitted_rtp_bytes;
            for tick in 1..=600 {
                let at = start + Duration::from_millis(tick * 5);
                fixture.drive_for(at.saturating_duration_since(fixture.at()));
                frame += 1;
                let media = sample(fixture.at(), frame, 1_000, 48_000);
                match fixture.connection.write_rtp(fixture.at(), &mid, media) {
                    Ok(())
                    | Err(crate::RejectedRtp {
                        reason: WriteError::WouldBlock,
                        ..
                    }) => {}
                    Err(error) => panic!("unexpected media error: {error:?}"),
                }
            }
            let envelope = fixture
                .connection
                .egress
                .test_envelope()
                .expect("controller output");
            assert_eq!(
                envelope.feedback_stale,
                !feedback_enabled,
                "feedback={feedback_enabled} replace_path={replace_path} stats={:?} envelope={envelope:?}",
                fixture.connection.stats().connection
            );
            assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > before);
        }
        fixture
            .connection
            .discard_pending_rtp(&mid)
            .expect("caller pauses production");
        fixture.drive_for(Duration::from_secs(1));
        let paused = fixture.connection.stats().connection.transmitted_rtp_bytes;
        let settled = fixture
            .connection
            .egress
            .test_envelope()
            .expect("settled native output");
        fixture.drive_for(Duration::from_secs(2));
        assert_eq!(
            fixture.connection.stats().connection.transmitted_rtp_bytes,
            paused
        );
        let quiet = fixture
            .connection
            .egress
            .test_envelope()
            .expect("quiet native output");
        assert!(quiet.feedback_stale);
        // No packets means no new native ALR or window evidence. Polling must
        // neither manufacture classification nor decay nominal capacity.
        assert_eq!(quiet.application_limited, settled.application_limited);
        assert_eq!(quiet.reference_window, settled.reference_window);
        assert_eq!(
            quiet.target_media_payload_rate,
            settled.target_media_payload_rate
        );
        let start = fixture.at();
        for tick in 1..=600 {
            let at = start + Duration::from_millis(tick * 5);
            fixture.drive_for(at.saturating_duration_since(fixture.at()));
            frame += 1;
            let media = sample(fixture.at(), frame, 1_000, 48_000);
            match fixture.connection.write_rtp(fixture.at(), &mid, media) {
                Ok(())
                | Err(crate::RejectedRtp {
                    reason: WriteError::WouldBlock,
                    ..
                }) => {}
                Err(error) => panic!("unexpected media error: {error:?}"),
            }
        }
        assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > paused);
        assert!(
            !fixture
                .connection
                .egress
                .test_envelope()
                .expect("controller output")
                .feedback_stale
        );
        assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
        fixture
            .connection
            .discard_pending_rtp(&mid)
            .expect("caller stops production");
        fixture.drive_for(Duration::from_secs(1));
        let stopped = fixture.connection.stats().connection.transmitted_rtp_bytes;
        fixture.drive_for(Duration::from_secs(6));
        assert_eq!(
            fixture.connection.stats().connection.transmitted_rtp_bytes,
            stopped
        );
        assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
    }
    #[test]
    #[should_panic(expected = "native RTP pacing budget")]
    fn pacing_oracle_rejects_unpaced_primary_bytes() {
        let now = Instant::now();
        assert_native_rtp_pacing(
            &[(now, 1_000, false), (now, 1, false)],
            &[(now, 0, 80_000, 0)],
        );
    }

    #[test]
    #[should_panic(expected = "native RTP pacing budget")]
    fn pacing_oracle_rejects_unpaced_padding_bytes() {
        let now = Instant::now();
        assert_native_rtp_pacing(
            &[(now, 1_000, true), (now, 1, true)],
            &[(now, 0, 80_000, 0)],
        );
    }

    #[test]
    #[should_panic(expected = "native RTP pacing budget")]
    fn pacing_oracle_rejects_future_same_time_rate_credit() {
        let now = Instant::now();
        assert_native_rtp_pacing(
            &[(now, 1_000, true), (now, 1, true), (now, 1, false)],
            &[(now, 0, 80_000, 0), (now, 2, 8_000_000, 0)],
        );
    }

    #[test]
    #[should_panic(expected = "native RTP pacing budget")]
    fn pacing_oracle_rate_changes_do_not_clear_debt() {
        let now = Instant::now();
        assert_native_rtp_pacing(
            &[(now, 1_000, true), (now, 1, true)],
            &[(now, 0, 80_000, 0), (now, 1, 40_000, 0)],
        );
    }

    #[test]
    fn pacing_oracle_fresh_path_resets_only_its_own_byte_debt() {
        let now = Instant::now();
        assert_native_rtp_pacing(
            &[(now, 1_000, true), (now, 1_000, true)],
            &[(now, 0, 80_000, 1), (now, 1, 80_000, 2)],
        );
    }

    #[test]
    #[should_panic(expected = "native RTP pacing budget")]
    fn pacing_oracle_idle_time_cannot_bank_unbounded_service() {
        let now = Instant::now();
        let later = now + Duration::from_secs(10);
        assert_native_rtp_pacing(
            &[(later, 1_000, false), (later, 1, false)],
            &[(now, 0, 80_000, 0)],
        );
    }
    #[test]
    fn native_authenticated_feedback_receipts_include_primary_wrap_and_late_holes() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        let source = fixture.send_source(b"late-hole");
        let before = fixture.connection.commit.history.counters().received;
        for index in [65_534, 65_536, 65_535, 65_538, 65_537] {
            fixture
                .write_rtp(crate::test_support::rtp_write(source.clone(), index))
                .expect("caller-numbered primary");
        }
        fixture.drive_for(Duration::from_secs(1));
        assert_eq!(fixture.connection.stats().senders[0].transmitted_packets, 5);
        assert!(
            fixture.connection.commit.history.counters().received >= before + 5,
            "native TWCC only acknowledges packets after SRTP authentication"
        );
        fixture.drive_for(Duration::from_secs(5));
        let report = fixture
            .receiver_report
            .as_ref()
            .expect("authenticated native RR");
        assert_eq!(report.mid.as_ref(), fixture.sender_mid);
        assert_eq!(report.ssrc, fixture.connection.stats().senders[0].ssrc);
        assert_eq!(report.extended_highest_sequence, 65_538);
    }

    #[test]
    fn production_two_megabit_capacity_and_native_window() {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.observe_pacing_profile(Connection::test_pacing_profile);
        fixture.configure_network(
            0x2201,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.configure_time_quantum(Duration::from_millis(2));
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(4_000_000)));
        let mid = fixture.sender_mid.clone();
        fixture.observe_native_target(|connection| {
            let envelope = connection
                .egress
                .test_envelope()
                .expect("active native controller");
            assert!(
                connection.stats().connection.rtp_bytes_in_flight
                    <= envelope.max_rtp_bytes_in_flight,
                "each committed RTP emission must fit the native send window"
            );
            envelope.native_queue_delay_target
        });
        let start = fixture.at();
        for id in 1..=6_000_u64 {
            let media = sample(fixture.at(), id, 1_000, 48_000);
            match fixture
                .connection
                .write_rtp(fixture.at(), &mid, media)
                .map_err(|rejected| rejected.reason)
            {
                Ok(()) | Err(WriteError::WouldBlock) => {}
                Err(error) => panic!("admission: {error:?}"),
            }
            fixture.drive_for(Duration::from_millis(2));
        }
        let end = fixture.at();
        let stable_start = start + Duration::from_secs(2);
        let cohort = fixture
            .emitted_rtp()
            .iter()
            .filter(|(at, _, _)| *at >= stable_start && *at < end)
            .map(|(_, bytes, _)| *bytes)
            .sum::<u64>();
        fixture
            .connection
            .discard_pending_rtp(&mid)
            .expect("caller ends the offered cohort");
        fixture.set_desired_bitrate(None);
        fixture.drive_for(Duration::from_secs(2));
        assert_native_rtp_pacing(fixture.emitted_rtp(), fixture.pacing_trace());
        let delivered = fixture
            .delivered_rtp()
            .iter()
            .filter(|(at, _)| *at >= stable_start && *at < end)
            .map(|(_, bytes)| *bytes)
            .sum::<u64>();
        let service = 2_000_000_u128 * end.duration_since(stable_start).as_micros() / 8_000_000;
        eprintln!(
            "2mbps seed=0x2201 stable_emitted={cohort} stable_delivered={delivered} service={service} duration={:?}",
            end.duration_since(stable_start)
        );
        assert!(end.duration_since(stable_start) >= Duration::from_secs(10));
        assert_eq!(delivered, cohort, "all emission-cohort RTP must drain");
        assert!(fixture.bottleneck_samples().0.len() >= 1_000);
        assert!(u128::from(delivered) * 100 >= service * 85);
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
        let stable_end = end;
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
            "production queue seed=0x2201 samples={} raw_p50={:?} raw_p95={:?} raw_p99={:?} excess_p99={p99:?}",
            excess.len(),
            raw[(raw.len() * 50).div_ceil(100) - 1],
            raw[(raw.len() * 95).div_ceil(100) - 1],
            raw[(raw.len() * 99).div_ceil(100) - 1],
        );
        // Native transport pacing exceeds the payload estimate. The removed payload
        // pacer imposed the old 10 ms excess-sojourn bound, not SCReAM itself.
    }

    #[test]
    fn feedback_driven_alr_tracks_sparse_media_after_saturated_service() {
        let mut fixture = PeerFixture::connected();
        let mut source = PeerFixture::connected();
        fixture.configure_network(
            0x6603,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(2_000_000);
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(4_000_000)));
        // Establish a real flight-supported reference window before reducing the
        // source. A low offered rate alone is not native SCReAM ALR evidence.
        for id in 1..=150 {
            source.drive_for(Duration::from_millis(20));
            fixture.drive_for(Duration::from_millis(20));
            fixture
                .write_rtp(crate::test_support::rtp_write(
                    source.send_source(&[0x5a; 1_000]),
                    id,
                ))
                .expect("warm native flight");
        }
        for id in 151..=170 {
            source.drive_for(Duration::from_millis(100));
            fixture.drive_for(Duration::from_millis(100));
            fixture
                .write_rtp(crate::test_support::rtp_write(
                    source.send_source(&[0x5a; 1_000]),
                    id,
                ))
                .expect("sparse actual media");
        }
        fixture.drive_for(Duration::from_millis(300));
        let (offered, application_limited) = fixture.connection.egress.observed_offer();
        assert!(offered <= 100_000, "actual sparse offer={offered}");
        assert!(
            application_limited,
            "native sparse-flow envelope={:?}",
            fixture.connection.egress.test_envelope()
        );
        assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > 0);
    }
    #[test]
    fn poll_observes_committed_flight_before_batched_ack_retirement() {
        use crate::rtcp::{FeedbackBatch, FeedbackReport, TwccStatus};
        use crate::transport::{PreparedRtpIdentity, RtpService};

        let mut fixture = PeerFixture::connected();
        fixture.set_feedback_enabled(false);
        let epoch = fixture
            .connection
            .commit
            .history
            .test_active_epoch()
            .expect("connected path");
        fixture.connection.commit.history =
            SentHistory::new(crate::PacketFeedbackKind::TransportWide);
        fixture.connection.commit.history.path_changed(epoch, true);
        let _ = fixture.connection.poll(fixture.at());
        for sequence in 1..=16 {
            let context = TransmitCommitContext {
                at: fixture.at(),
                kind: DatagramKind::Rtp,
                wire_len: 1_200,
                path_epoch: Some(epoch),
                rtp: Some(PreparedRtpIdentity {
                    ssrc: 7,
                    index: u64::from(sequence),
                    sequence,
                    twcc_sequence: Some(sequence),
                    service: RtpService::Padding,
                }),
            };
            fixture
                .connection
                .commit
                .history
                .preflight_commit(context)
                .expect("valid commit");
            fixture
                .connection
                .commit
                .history
                .commit_preflighted(context);
        }
        assert_eq!(
            fixture
                .connection
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight,
            19_200
        );
        assert!(fixture.connection.egress.test_flight_peak() < 19_200);
        fixture.advance_clock_without_poll(Duration::from_millis(50));
        for feedback_count in 1..=8 {
            fixture
                .connection
                .ingress
                .test_queue_feedback(FeedbackBatch {
                    received_at: fixture.at(),
                    path_epoch: epoch,
                    sender_ssrc: 9,
                    report: FeedbackReport::Twcc {
                        media_ssrc: 7,
                        base_sequence: 1,
                        reference_time: 0,
                        feedback_count,
                        statuses: vec![TwccStatus::Received { delta_250us: 1 }; 16].into(),
                    },
                });
        }
        let mut immediate_yields = 0;
        for _ in 0..32 {
            let output = fixture.connection.poll(fixture.at());
            assert!(
                !matches!(output, Output::Closed(_)),
                "unexpected closure: {output:?}"
            );
            if fixture.connection.ingress.has_pending_output()
                && let Output::Idle { next_wakeup } = output
            {
                assert_eq!(next_wakeup, Some(fixture.at()));
                immediate_yields += 1;
            }
            if fixture.connection.commit.history.counters().received == 16
                && !fixture.connection.ingress.has_pending_output()
            {
                break;
            }
        }
        assert_eq!(
            fixture.connection.commit.history.counters().received,
            16,
            "queued ACK must retire the committed batch: {:?}",
            fixture.connection.commit.history.counters(),
        );
        assert!(
            immediate_yields > 0,
            "queued feedback must yield an immediate continuation"
        );
        assert!(
            fixture
                .connection
                .commit
                .history
                .controller_inputs()
                .bytes_in_flight
                < 19_200
        );
        assert_eq!(fixture.connection.egress.test_flight_peak(), 19_200);
    }
}
