#![allow(
    clippy::disallowed_types,
    reason = "the public transmit contract requires immutable Bytes payloads"
)]

use std::{cell::Cell, collections::VecDeque, marker::PhantomData, time::Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{
    AcceptError, CloseReason, ConnectionConfig, ConnectionEntropy, ConnectionWarning, Event,
    NetworkInput, Output, ReceiveError, SdpAnswer, SdpOffer, SessionInfo, TimePoint, Transmit,
    ingress::IngressOwner,
    negotiation::{self, NegotiatedSessionFacts},
    time::MonotonicObserver,
    transport::{PreparedTransmit, Transport, TransportError, TransportEvent, TransportState},
};

const MAX_INGRESS_PACKETS: usize = 256;

pub struct Connection {
    _config: ConnectionConfig,
    _session: NegotiatedSessionFacts,
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
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
            feedback: None,
            controller: None,
            scheduler: None,
            sctp: None,
            commit: CommitCoordinator,
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

struct CommitCoordinator;

impl CommitCoordinator {
    fn commit_transport(&mut self, prepared: PreparedTransmit) -> Transmit {
        let PreparedTransmit {
            target,
            bytes,
            kind: _,
            wire_len: _,
        } = prepared;
        Transmit {
            target,
            payload: Bytes::from(bytes),
        }
    }
}

pub struct AcceptedConnection {
    pub connection: Connection,
    pub answer: SdpAnswer,
    pub session: SessionInfo,
}

impl Connection {
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

        if !self.runtime.closed
            && let Some(prepared) = self._subsystems.transport.poll_prepared()
        {
            return Output::Transmit(self.runtime.commit.commit_transport(prepared));
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
            TransportEvent::Rtp {
                arrival,
                bytes,
                metadata: _,
            } => {
                self._subsystems.ingress.accept_rtp(arrival, bytes);
                self._subsystems.ingress.poll_event().map(Output::Event)
            }
            TransportEvent::Rtcp { arrival, bytes } => {
                self._subsystems.ingress.retain_rtcp(arrival, bytes);
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
        let connection = Self {
            _config: config,
            _session: negotiated.facts,
            _time: MonotonicObserver::starting_at(at),
            _subsystems: SubsystemSlots { transport, ingress },
            runtime: Runtime::new(),
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
