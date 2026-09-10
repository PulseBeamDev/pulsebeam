#![allow(
    clippy::disallowed_types,
    reason = "the public transmit contract requires immutable Bytes payloads"
)]

use std::{cell::Cell, marker::PhantomData, time::Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    AcceptError, CloseReason, Command, CommandError, ConnectionConfig, ConnectionEntropy,
    ConnectionWarning, Event, NetworkInput, Output, PacketFeedbackKind, ReceiveError, SdpAnswer,
    SdpOffer, SessionInfo, TimePoint, Transmit,
    egress::{MediaEgress, PrepareResult},
    ingress::IngressOwner,
    negotiation::{self, NegotiatedSessionFacts},
    scheduler::{ServiceArbiter, UserLane},
    sctp::Association,
    sent_history::{HistoryError, MAX_EXPIRATIONS_PER_POLL, SentHistory},
    time::MonotonicObserver,
    transport::{
        DatagramKind, PathEpoch, PreparedRtpIdentity, PreparedTransmit, Transport, TransportError,
        TransportEvent, TransportState,
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

#[allow(
    dead_code,
    reason = "protocol subsystems are initialized by subsequent plans"
)]
struct SubsystemSlots {
    transport: Transport,
    ingress: IngressOwner,
    egress: MediaEgress,
    sctp: Option<Association>,
}

trait RuntimeSubsystem: Send {
    fn poll(&mut self, at: TimePoint) -> Option<Output>;

    fn next_deadline(&self) -> Option<Instant>;
}

struct Runtime {
    feedback: Option<Box<dyn RuntimeSubsystem>>,
    controller: Option<Box<dyn RuntimeSubsystem>>,
    scheduler: Option<Box<dyn RuntimeSubsystem>>,
    service: ServiceArbiter,
    commit: CommitCoordinator,
    closed: bool,
}

impl Runtime {
    fn new(feedback: PacketFeedbackKind) -> Self {
        Self {
            feedback: None,
            controller: None,
            scheduler: None,
            service: ServiceArbiter::default(),
            commit: CommitCoordinator::new(feedback),
            closed: false,
        }
    }

    fn poll_subsystems(&mut self, at: TimePoint) -> Option<Output> {
        [
            &mut self.feedback,
            &mut self.controller,
            &mut self.scheduler,
        ]
        .into_iter()
        .flatten()
        .find_map(|subsystem| subsystem.poll(at))
    }

    fn next_deadline(&self) -> Option<Instant> {
        [&self.feedback, &self.controller, &self.scheduler]
            .into_iter()
            .flatten()
            .filter_map(|subsystem| subsystem.next_deadline())
            .min()
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

pub(crate) trait CommitParticipant: Send {
    fn preflight(&self, _context: TransmitCommitContext) -> Result<(), HistoryError> {
        Ok(())
    }

    fn commit_preflighted(&mut self, context: TransmitCommitContext);
}

struct CommitCoordinator {
    history: SentHistory,
    participants: Vec<Box<dyn CommitParticipant>>,
}

impl CommitCoordinator {
    fn new(feedback: PacketFeedbackKind) -> Self {
        Self {
            history: SentHistory::new(feedback),
            participants: Vec::new(),
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
        CommitParticipant::preflight(&self.history, context)?;
        for participant in &self.participants {
            participant.preflight(context)?;
        }
        if egress
            .as_deref()
            .is_some_and(|egress| !egress.preflight_commit(&prepared))
        {
            return Err(HistoryError::InvalidCommit);
        }
        self.history.commit_preflighted(context);
        for participant in &mut self.participants {
            participant.commit_preflighted(context);
        }
        if let Some(egress) = egress {
            egress.commit(at, &prepared);
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
    pub fn command(&mut self, at: TimePoint, command: Command) -> Result<(), CommandError> {
        let _ = self._time.observe(at);
        if self.runtime.closed {
            return Err(CommandError::Closed);
        }
        match command {
            Command::SendMedia { sender, media } => {
                match self._subsystems.transport.state() {
                    TransportState::Closed | TransportState::Failed => {
                        return Err(CommandError::Closed);
                    }
                    TransportState::Connected => {}
                    TransportState::Checking
                    | TransportState::Connecting
                    | TransportState::Draining => return Err(CommandError::InvalidState),
                }
                self._subsystems.egress.admit(at, sender, media)
            }
            Command::SetSenderPolicy { sender, policy } => {
                self._subsystems.egress.set_policy(sender, policy, at)
            }
            Command::OpenDataChannel(config) => self
                ._subsystems
                .sctp
                .as_mut()
                .ok_or(CommandError::InvalidState)?
                .open(at, config),
            Command::SendData { channel, message } => self
                ._subsystems
                .sctp
                .as_mut()
                .ok_or(CommandError::UnknownDataChannel(channel))?
                .send(at, channel, message),
            Command::CloseDataChannel { channel } => self
                ._subsystems
                .sctp
                .as_mut()
                .ok_or(CommandError::UnknownDataChannel(channel))?
                .close_channel(at, channel),
            Command::RequestKeyframe { .. }
            | Command::RetireEncoding { .. }
            | Command::CloseGracefully { .. }
            | Command::Abort => Err(CommandError::InvalidState),
        }
    }

    pub fn receive(&mut self, at: TimePoint, input: NetworkInput) -> Result<(), ReceiveError> {
        let at = self._time.observe(at);
        self._subsystems
            .transport
            .receive_at(at, input)
            .map_err(|error| match error {
                crate::transport::TransportError::Closed => ReceiveError::Closed,
                crate::transport::TransportError::QueueFull => ReceiveError::InputLimitExceeded,
                _ => ReceiveError::InvalidNetworkEnvelope,
            })
    }

    pub fn poll(&mut self, at: TimePoint) -> Output {
        let at = self._time.observe(at);

        self.runtime
            .commit
            .history
            .expire(at.monotonic, MAX_EXPIRATIONS_PER_POLL);

        if self._time.take_warning() {
            return Output::Event(Event::Warning(ConnectionWarning::ClockRegression));
        }

        if !self.runtime.closed
            && self
                ._subsystems
                .transport
                .next_deadline()
                .is_some_and(|deadline| at.monotonic >= deadline)
            && let Err(error) = self._subsystems.transport.handle_timeout(at.monotonic)
            && !matches!(error, TransportError::NotDue)
        {
            self.runtime.closed = true;
            return Output::Closed(close_reason(error));
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

        if let Some(feedback) = self._subsystems.ingress.poll_feedback() {
            self.runtime.commit.history.process_feedback(feedback);
        }
        if let Some(repair) = self._subsystems.ingress.poll_repair() {
            self._subsystems
                .egress
                .request_repair(repair.media_ssrc, repair.sequence);
        }

        let (path_change, feedback, feedback_hold, bytes_in_flight) = {
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
                    .map(|feedback| feedback.controller_sample(received_at, origin))
                    .collect::<Vec<_>>(),
                inputs
                    .timing
                    .and_then(|timing| timing.feedback_hold)
                    .unwrap_or_default(),
                inputs.bytes_in_flight,
            )
        };
        let application_limited = self._subsystems.egress.update_controller(
            at,
            path_change,
            &feedback,
            feedback_hold,
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

        if !self.runtime.closed
            && let Some(prepared) = self._subsystems.transport.poll_prepared()
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
                    self.runtime.closed = true;
                    Output::Closed(CloseReason::TransportFailure)
                }
            };
        }

        let sctp_ready = self
            ._subsystems
            .sctp
            .as_ref()
            .is_some_and(Association::has_packet);
        let selected_lane = self.runtime.service.select(true, sctp_ready);
        if !self.runtime.closed
            && selected_lane == Some(UserLane::Sctp)
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if !self.runtime.closed {
            match self._subsystems.egress.prepare_one(
                at,
                bytes_in_flight,
                &mut self._subsystems.transport,
            ) {
                PrepareResult::Prepared => {
                    let Some(prepared) = self._subsystems.transport.poll_prepared() else {
                        self.runtime.closed = true;
                        return Output::Closed(CloseReason::TransportFailure);
                    };
                    return match self.runtime.commit.commit_transport(
                        at,
                        prepared,
                        Some(&mut self._subsystems.egress),
                    ) {
                        Ok(transmit) => Output::Transmit(transmit),
                        Err(_) => {
                            self.runtime.closed = true;
                            Output::Closed(CloseReason::TransportFailure)
                        }
                    };
                }
                PrepareResult::Fatal => {
                    self.runtime.closed = true;
                    return Output::Closed(CloseReason::TransportFailure);
                }
                PrepareResult::Blocked => {}
            }
        }

        if !self.runtime.closed
            && selected_lane != Some(UserLane::Sctp)
            && sctp_ready
            && let Some(output) = self.prepare_sctp(at)
        {
            return output;
        }

        if !self.runtime.closed
            && let Some(output) = self.runtime.poll_subsystems(at)
        {
            return output;
        }

        Output::Idle {
            next_wakeup: (!self.runtime.closed)
                .then(|| {
                    [
                        self._subsystems.transport.next_deadline(),
                        self.runtime.next_deadline(),
                        self.runtime.commit.history.next_deadline(),
                        self._subsystems.egress.next_deadline(),
                        self._subsystems
                            .sctp
                            .as_ref()
                            .and_then(Association::next_deadline),
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
                self.runtime.closed = true;
                Some(Output::Closed(CloseReason::TransportFailure))
            }
            TransportEvent::StateChanged(_) | TransportEvent::IceStateChanged(_) => None,
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
            } => {
                self._subsystems.ingress.accept_rtp(arrival, bytes);
                self._subsystems.ingress.poll_event().map(Output::Event)
            }
            TransportEvent::Rtcp {
                arrival,
                path_epoch,
                bytes,
            } => {
                self._subsystems.ingress.accept_rtcp(
                    arrival,
                    bytes,
                    path_epoch,
                    self.session.feedback(),
                    self._subsystems.transport.smoothed_rtt(),
                );
                None
            }
            TransportEvent::Data(bytes) => {
                if let Some(sctp) = self._subsystems.sctp.as_mut() {
                    sctp.handle_input(at, &bytes);
                }
                None
            }
            TransportEvent::Closed => {
                self.runtime.closed = true;
                Some(Output::Closed(CloseReason::Graceful))
            }
        }
    }

    fn prepare_sctp(&mut self, at: TimePoint) -> Option<Output> {
        let packet = self._subsystems.sctp.as_mut()?.poll_packet()?;
        if self
            ._subsystems
            .transport
            .send_sctp(&packet, at.monotonic)
            .is_err()
        {
            self.runtime.closed = true;
            return Some(Output::Closed(CloseReason::TransportFailure));
        }
        let Some(prepared) = self._subsystems.transport.poll_prepared() else {
            self.runtime.closed = true;
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
                self.runtime.closed = true;
                Some(Output::Closed(CloseReason::TransportFailure))
            }
        }
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
        let session = negotiated.session.clone();
        let transport = Transport::from_session(&negotiated.facts, at.monotonic)
            .map_err(|_| AcceptError::CryptographicFailure)?;
        let ingress = IngressOwner::new(
            negotiated.facts.ingress_media(),
            config.limits.max_unsignaled_encodings,
            config.limits.max_queued_media_bytes,
        );
        let egress = MediaEgress::new(
            negotiated.facts.egress_senders(),
            negotiated.facts.protocol_randomness(),
            config.limits.max_queued_media_bytes,
            config.limits.max_retransmission_bytes,
            config.default_audio_policy,
            config.default_video_policy,
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
    use crate::{
        ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata, TransmitTarget,
    };
    use std::sync::{Arc, Mutex};

    struct Observer(Arc<Mutex<Option<TransmitCommitContext>>>);

    impl CommitParticipant for Observer {
        fn commit_preflighted(&mut self, context: TransmitCommitContext) {
            *self.0.lock().expect("observer lock") = Some(context);
        }
    }

    struct RejectingParticipant(Arc<Mutex<bool>>);

    impl CommitParticipant for RejectingParticipant {
        fn preflight(&self, _context: TransmitCommitContext) -> Result<(), HistoryError> {
            Err(HistoryError::Exhausted)
        }

        fn commit_preflighted(&mut self, _context: TransmitCommitContext) {
            *self.0.lock().expect("participant lock") = true;
        }
    }

    #[test]
    fn commit_preserves_finalized_wire_context() {
        let monotonic = Instant::now();
        let at = TimePoint {
            monotonic,
            global: crate::GlobalMediaTime::from_micros(1),
        };
        let mut coordinator = CommitCoordinator::new(PacketFeedbackKind::TransportWide);
        coordinator
            .history
            .path_changed(PathEpoch::from_value(3), true);
        let observed = Arc::new(Mutex::new(None));
        coordinator
            .participants
            .push(Box::new(Observer(Arc::clone(&observed))));
        let transmit = coordinator
            .commit_transport(
                at,
                PreparedTransmit {
                    target: TransmitTarget::IceTcp {
                        flow: crate::IceTcpFlowId::from_value(11),
                    },
                    bytes: vec![0, 3, 1, 2, 3],
                    kind: DatagramKind::Rtp,
                    wire_len: 5,
                    path_epoch: Some(PathEpoch::from_value(3)),
                    rtp: Some(PreparedRtpIdentity {
                        ssrc: 7,
                        sequence: 9,
                        twcc_sequence: Some(13),
                        service: crate::transport::RtpService::Original,
                    }),
                },
                None,
            )
            .expect("valid RTP commit");
        assert_eq!(transmit.payload, Bytes::from_static(&[0, 3, 1, 2, 3]));
        assert_eq!(
            *observed.lock().expect("observer lock"),
            Some(TransmitCommitContext {
                at: monotonic,
                kind: DatagramKind::Rtp,
                wire_len: 5,
                path_epoch: Some(PathEpoch::from_value(3)),
                rtp: Some(PreparedRtpIdentity {
                    ssrc: 7,
                    sequence: 9,
                    twcc_sequence: Some(13),
                    service: crate::transport::RtpService::Original,
                }),
            })
        );
    }

    #[test]
    fn failed_commit_preflight_mutates_no_participant() {
        let monotonic = Instant::now();
        let at = TimePoint {
            monotonic,
            global: crate::GlobalMediaTime::from_micros(1),
        };
        let mut coordinator = CommitCoordinator::new(PacketFeedbackKind::TransportWide);
        coordinator
            .history
            .path_changed(PathEpoch::from_value(3), true);
        let touched = Arc::new(Mutex::new(false));
        coordinator
            .participants
            .push(Box::new(RejectingParticipant(Arc::clone(&touched))));
        assert_eq!(
            coordinator.commit_transport(
                at,
                PreparedTransmit {
                    target: TransmitTarget::IceTcp {
                        flow: crate::IceTcpFlowId::from_value(11),
                    },
                    bytes: vec![0, 3, 1, 2, 3],
                    kind: DatagramKind::Rtp,
                    wire_len: 5,
                    path_epoch: Some(PathEpoch::from_value(3)),
                    rtp: Some(PreparedRtpIdentity {
                        ssrc: 7,
                        sequence: 9,
                        twcc_sequence: Some(13),
                        service: crate::transport::RtpService::Original,
                    }),
                },
                None,
            ),
            Err(HistoryError::Exhausted)
        );
        assert_eq!(coordinator.history.controller_inputs().bytes_in_flight, 0);
        assert!(!*touched.lock().expect("participant lock"));
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
                            packet: packet.clone(),
                            frame: FrameMetadata {
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
