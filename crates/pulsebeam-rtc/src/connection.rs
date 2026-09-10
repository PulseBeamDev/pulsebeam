#![allow(
    clippy::disallowed_types,
    reason = "the public transmit contract requires immutable Bytes payloads"
)]

use std::{cell::Cell, collections::VecDeque, marker::PhantomData, time::Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    AcceptError, CloseReason, Command, CommandError, ConnectionConfig, ConnectionEntropy,
    ConnectionWarning, Event, NetworkInput, Output, PacketFeedbackKind, ReceiveError, SdpAnswer,
    SdpOffer, SessionInfo, TimePoint, Transmit,
    egress::{MediaEgress, PrepareResult},
    ingress::IngressOwner,
    negotiation::{self, NegotiatedSessionFacts},
    sent_history::{HistoryError, MAX_EXPIRATIONS_PER_POLL, SentHistory},
    time::MonotonicObserver,
    transport::{
        DatagramKind, PathEpoch, PreparedRtpIdentity, PreparedTransmit, Transport, TransportError,
        TransportEvent, TransportState,
    },
};

const MAX_INGRESS_PACKETS: usize = 256;

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
}

trait RuntimeSubsystem: Send {
    fn poll(&mut self, at: TimePoint) -> Option<Output>;

    fn next_deadline(&self) -> Option<Instant>;
}

struct Runtime {
    data: VecDeque<Vec<u8>>,
    feedback: Option<Box<dyn RuntimeSubsystem>>,
    controller: Option<Box<dyn RuntimeSubsystem>>,
    scheduler: Option<Box<dyn RuntimeSubsystem>>,
    sctp: Option<Box<dyn RuntimeSubsystem>>,
    commit: CommitCoordinator,
    closed: bool,
}

impl Runtime {
    fn new(feedback: PacketFeedbackKind) -> Self {
        Self {
            data: VecDeque::new(),
            feedback: None,
            controller: None,
            scheduler: None,
            sctp: None,
            commit: CommitCoordinator::new(feedback),
            closed: false,
        }
    }

    fn poll_subsystems(&mut self, at: TimePoint) -> Option<Output> {
        [
            &mut self.feedback,
            &mut self.controller,
            &mut self.scheduler,
            &mut self.sctp,
        ]
        .into_iter()
        .flatten()
        .find_map(|subsystem| subsystem.poll(at))
    }

    fn next_deadline(&self) -> Option<Instant> {
        [
            &self.feedback,
            &self.controller,
            &self.scheduler,
            &self.sctp,
        ]
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
    fn commit(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError>;
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
        at: Instant,
        prepared: PreparedTransmit,
    ) -> Result<Transmit, HistoryError> {
        let context = TransmitCommitContext {
            at,
            kind: prepared.kind,
            wire_len: prepared.wire_len,
            path_epoch: prepared.path_epoch,
            rtp: prepared.rtp,
        };
        CommitParticipant::commit(&mut self.history, context)?;
        for participant in &mut self.participants {
            participant.commit(context)?;
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
                self._subsystems.egress.admit(sender, media)
            }
            Command::SetSenderPolicy { .. }
            | Command::RequestKeyframe { .. }
            | Command::RetireEncoding { .. }
            | Command::OpenDataChannel(_)
            | Command::SendData { .. }
            | Command::CloseDataChannel { .. }
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
            && let Some(output) = self.handle_transport_event(event)
        {
            return output;
        }

        if let Some(event) = self._subsystems.ingress.poll_event() {
            return Output::Event(event);
        }

        if let Some(feedback) = self._subsystems.ingress.poll_feedback() {
            self.runtime.commit.history.process_feedback(feedback);
        }

        if !self.runtime.closed
            && let Some(prepared) = self._subsystems.transport.poll_prepared()
        {
            return match self.runtime.commit.commit_transport(at.monotonic, prepared) {
                Ok(transmit) => Output::Transmit(transmit),
                Err(_) => {
                    self.runtime.closed = true;
                    Output::Closed(CloseReason::TransportFailure)
                }
            };
        }

        if !self.runtime.closed {
            match self
                ._subsystems
                .egress
                .prepare_one(&mut self._subsystems.transport)
            {
                PrepareResult::Prepared => {
                    let Some(prepared) = self._subsystems.transport.poll_prepared() else {
                        self.runtime.closed = true;
                        return Output::Closed(CloseReason::TransportFailure);
                    };
                    return match self.runtime.commit.commit_transport(at.monotonic, prepared) {
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
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                })
                .flatten(),
        }
    }

    fn handle_transport_event(&mut self, event: TransportEvent) -> Option<Output> {
        match event {
            TransportEvent::StateChanged(TransportState::Connected) => {
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
                if self.runtime.data.len() < MAX_INGRESS_PACKETS {
                    self.runtime.data.push_back(bytes);
                }
                None
            }
            TransportEvent::Closed => {
                self.runtime.closed = true;
                Some(Output::Closed(CloseReason::Graceful))
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
            config.default_audio_policy,
            config.default_video_policy,
        );
        let feedback = negotiated.facts.feedback();
        let connection = Self {
            _config: config,
            session: negotiated.facts,
            _time: MonotonicObserver::starting_at(at),
            _subsystems: SubsystemSlots {
                transport,
                ingress,
                egress,
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
        fn commit(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
            *self.0.lock().expect("observer lock") = Some(context);
            Ok(())
        }
    }

    #[test]
    fn commit_preserves_finalized_wire_context() {
        let at = Instant::now();
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
            )
            .expect("valid RTP commit");
        assert_eq!(transmit.payload, Bytes::from_static(&[0, 3, 1, 2, 3]));
        assert_eq!(
            *observed.lock().expect("observer lock"),
            Some(TransmitCommitContext {
                at,
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
}
