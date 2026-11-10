//! Outer datagram feedback, congestion control and bounded host orchestration.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::backend::{Engine, Output};
use crate::wire;
use crate::{PeerConfig, ReceiveTime, Time};
use pulsebeam_feedback as feedback;

const EPOCH: feedback::Epoch = feedback::Epoch(1);
const SPACE: feedback::SpaceId = feedback::SpaceId(0);
const HISTORY: usize = 8192;
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Closed,
    NotStarted,
    Busy,
    Time,
    Security,
    Capacity,
    Codec,
    NotReady,
    Blocked,
    Oversize,
    Expired,
    SequenceWindow,
    FeedbackUnavailable,
    UnsupportedProfile,
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Accepted { bytes: usize },
    Rejected,
}

pub struct Application<'a> {
    pub context: [u8; 4],
    pub payload: &'a [u8],
    pub observed: ReceiveTime,
}

pub struct Datagram<'a> {
    pub destination: SocketAddr,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Health {
    pub ready: bool,
    pub feedback_interval: Duration,
    pub sent: u64,
    pub received: u64,
    pub send_failed: u64,
    pub expired: u64,
    pub invalid: u64,
    pub bytes_in_flight: u64,
    pub closed: Option<Error>,
    pub staging: Staging,
    pub pending_udp_bytes: usize,
    pub quic: Option<crate::QuicStats>,
}

/// One bounded application/feedback frame awaiting protection or socket output.
/// Does not include the secure engine's bounded handshake state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Staging {
    pub datagrams: usize,
    pub plaintext_bytes: usize,
    pub oldest_age: Option<Duration>,
}

enum Protected {
    Data { attempt: u64, expires_at: Time },
    Feedback(feedback::ReportWindow),
}

struct SocketOutput {
    bytes: Box<[u8]>,
    protected: Option<Protected>,
}

pub struct Transport {
    peer: PeerConfig,
    closed: bool,
    association: Option<Engine>,
    origin: Option<(Time, Instant)>,
    now: Time,
    protocol_time: Time,
    observed: ReceiveTime,
    drained: bool,
    demand: bool,
    protected: Option<Protected>,
    staged: Option<(Time, usize)>,
    socket_output: Option<SocketOutput>,
    retry_at: Option<Time>,
    feedback_wait: Option<Time>,
    attempt: u64,
    highest_received: Option<u64>,
    highest_acknowledged: Option<u64>,
    report_generation: i64,
    sender: feedback::Sender,
    receiver: feedback::Receiver,
    controller: pulsebeam_cc::Controller,
    statuses: Vec<feedback::SendStatus>,
    samples: Vec<pulsebeam_cc::FeedbackSample>,
    protocol_deadline: Option<Time>,
    #[cfg(any(test, feature = "test-utils"))]
    test_key_limit: Option<u64>,
    health: Health,
}

impl Transport {
    pub fn new(peer: impl Into<PeerConfig>) -> Result<Self, Error> {
        let peer = peer.into();
        if let PeerConfig::Quic(peer) = &peer {
            peer.tuning.validate()?;
        }
        let sender = feedback::Sender::new(
            feedback::SenderConfig {
                capacity: HISTORY,
                retention: Duration::from_secs(2),
                max_expirations_per_poll: 256,
                max_ack_reordering: 1024,
                max_feedback_statuses: wire::MAX_STATUSES,
                initial_reordering_window: Duration::from_millis(30),
                max_reordering_window: Duration::from_millis(200),
            },
            feedback::FeedbackMode::Continuous,
            16 * 1024 * 1024,
        )
        .map_err(|_| Error::Capacity)?;
        // Class affects feedback latency, never an assumed path capacity or RTT.
        // Both paths retain the same measured congestion controller and pacer.
        let feedback_interval = match peer.class() {
            crate::NetworkClass::Lan => Duration::from_millis(5),
            crate::NetworkClass::Wan => Duration::from_millis(10),
        };
        let mut receiver = feedback::Receiver::new(
            feedback::ReceiverConfig {
                capacity: HISTORY,
                max_report_statuses: wire::MAX_STATUSES,
                interval: feedback_interval,
                order: feedback::ReportOrder::Continuous(SPACE),
            },
            EPOCH,
        )
        .map_err(|_| Error::Capacity)?;
        receiver
            .reserve(4 * 1024 * 1024)
            .map_err(|_| Error::Capacity)?;
        let controller = pulsebeam_cc::Controller::new(pulsebeam_cc::Config::default())
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            peer,
            closed: false,
            association: None,
            origin: None,
            now: Time(0),
            protocol_time: Time(0),
            observed: ReceiveTime {
                monotonic: Time(0),
                wall: None,
            },
            drained: true,
            demand: false,
            protected: None,
            staged: None,
            socket_output: None,
            retry_at: None,
            feedback_wait: None,
            attempt: 0,
            highest_received: None,
            highest_acknowledged: None,
            report_generation: 0,
            sender,
            receiver,
            controller,
            statuses: Vec::with_capacity(wire::MAX_STATUSES),
            samples: Vec::with_capacity(HISTORY),
            protocol_deadline: None,
            health: Health {
                feedback_interval,
                ..Health::default()
            },
            #[cfg(any(test, feature = "test-utils"))]
            test_key_limit: None,
        })
    }

    /// Anchor the upstream engine's Instant domain to the host's Time.
    /// Both arguments come from the same real or virtual host clock observation.
    pub fn start(&mut self, at: Time, instant: Instant) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.origin.is_some() {
            return Err(Error::Busy);
        }
        #[cfg(any(test, feature = "test-utils"))]
        let limit = self.test_key_limit;
        #[cfg(not(any(test, feature = "test-utils")))]
        let limit = None;
        let mut association = Engine::new(&self.peer, instant, limit)?;
        // Initialize the protocol before an initial Timeout exposes input readiness.
        // Dimpl seeds its server handshake randomness on this first clock tick.
        association.handle_timeout(instant)?;
        self.association = Some(association);
        self.origin = Some((at, instant));
        self.now = at;
        self.protocol_time = at;
        self.observed.monotonic = at;
        self.drained = false;
        Ok(())
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_test_key_limit(&mut self, limit: u64) {
        assert!(self.origin.is_none());
        self.test_key_limit = Some(limit);
    }

    /// Resume a settled sequence history near a wrap boundary without billions
    /// of test transmissions. Only fresh, empty-history fixtures may use this.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn seed_test_sequence(&mut self, next_attempt: u64, highest_received: Option<u64>) {
        assert_eq!(self.attempt, 0);
        assert_eq!(self.highest_received, None);
        assert_eq!(self.sender.observations().bytes_in_flight, 0);
        self.attempt = next_attempt;
        self.highest_acknowledged = next_attempt.checked_sub(1);
        self.highest_received = highest_received;
    }

    pub fn health(&self) -> Health {
        Health {
            quic: self.association.as_ref().and_then(Engine::quic_stats),
            bytes_in_flight: self.sender.observations().bytes_in_flight,
            pending_udp_bytes: self
                .socket_output
                .as_ref()
                .map_or(0, |pending| pending.bytes.len()),
            staging: self
                .staged
                .map_or_else(Staging::default, |(at, plaintext_bytes)| Staging {
                    datagrams: 1,
                    plaintext_bytes,
                    oldest_age: Some(Duration::from_nanos(self.now.0.saturating_sub(at.0))),
                }),
            ..self.health
        }
    }

    /// Whether protocol output must be drained before accepting another UDP input.
    pub fn needs_progress(&self) -> bool {
        !self.closed
            && if self.socket_output.is_some() {
                self.retry_at.is_none_or(|at| at <= self.now)
            } else {
                !self.drained || self.protocol_time != self.now
            }
    }

    /// Whether one input can be accepted without retaining it at the host.
    pub fn input_ready(&self) -> bool {
        !self.closed
            && self.origin.is_some()
            && self.drained
            && self.protocol_time == self.now
            && self.socket_output.is_none()
    }

    /// Caller-owned offered demand, not a path-capacity estimate.
    pub fn set_demand(&mut self, queued: bool) {
        self.demand = queued;
    }

    pub fn next_deadline(&self) -> Option<Time> {
        if self.closed {
            return None;
        }
        if self.socket_output.is_some() {
            return self.retry_at;
        }
        if self.needs_progress() {
            return Some(self.now);
        }
        let origin = self.origin?.0;
        let now = Duration::from_nanos(self.now.0.saturating_sub(origin.0));
        let staging_available = self.protected.is_none();
        let candidate = (self.demand && staging_available)
            .then_some((self.sender.observations().bytes_in_flight, wire::UDP_BUDGET));
        let relative = [
            self.sender.next_deadline(now),
            staging_available
                .then(|| self.receiver.deadline())
                .flatten(),
            self.controller.next_deadline(now, candidate, None),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|at| {
            Time(
                origin
                    .0
                    .saturating_add(u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)),
            )
        });
        [self.protocol_deadline, relative, self.feedback_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Input is accepted only at a fully drained protocol boundary. On Busy,
    /// ownership remains with the host; poll before retrying this input.
    pub fn receive(
        &mut self,
        source: SocketAddr,
        datagram: &[u8],
        observed: ReceiveTime,
    ) -> Result<(), Error> {
        self.ensure_live()?;
        if !self.input_ready() {
            return Err(Error::Busy);
        }
        if source != self.peer.endpoint() || datagram.len() > wire::UDP_BUDGET {
            self.health.invalid = self.health.invalid.saturating_add(1);
            return Ok(());
        }
        if observed.monotonic > self.now {
            return Err(Error::Time);
        }
        self.observed = observed;
        if self
            .association
            .as_mut()
            .ok_or(Error::NotStarted)?
            .handle_packet(datagram)
            .is_err()
        {
            self.close_with_reason(Error::Security);
            return Err(Error::Security);
        }
        self.drained = false;
        Ok(())
    }

    /// Bounded protocol/queue work. The callback reports actual nonblocking UDP
    /// submission, not mere staging in another host queue. A rejected datagram
    /// is discarded by DTLS. Noq retains one serialized output for retry because
    /// poll_transmit has already committed QUIC packet/recovery state.
    /// Continue while `needs_progress()` is true. A zero budget advances host
    /// time but withholds input/admission until the protocol clock is refreshed.
    pub fn poll(
        &mut self,
        now: Time,
        budget: usize,
        mut send: impl FnMut(Datagram<'_>) -> SendOutcome,
        mut receive: impl FnMut(Application<'_>),
    ) -> Result<usize, Error> {
        let result = self.poll_inner(now, budget, &mut send, &mut receive);
        if let Err(error) = result {
            self.close_with_reason(error);
        }
        result
    }

    fn poll_inner(
        &mut self,
        now: Time,
        budget: usize,
        send: &mut impl FnMut(Datagram<'_>) -> SendOutcome,
        receive: &mut impl FnMut(Application<'_>),
    ) -> Result<usize, Error> {
        self.ensure_live()?;
        if now < self.now {
            return Err(Error::Time);
        }
        let (origin, instant) = self.origin.ok_or(Error::NotStarted)?;
        let elapsed = Duration::from_nanos(now.0.checked_sub(origin.0).ok_or(Error::Time)?);
        let instant = instant.checked_add(elapsed).ok_or(Error::Time)?;
        self.now = now;
        self.association
            .as_mut()
            .ok_or(Error::NotStarted)?
            .set_time(instant);
        let mut work = 0usize;
        if budget == 0 {
            return Ok(0);
        }
        if self.socket_output.is_some() && self.retry_at.is_some_and(|at| at > now) {
            return Ok(0);
        }
        if self.feedback_deadline().is_some_and(|at| at <= now) {
            return Err(Error::FeedbackUnavailable);
        }
        self.sender.expire(elapsed, 256);
        self.update_controller(elapsed)?;
        if self.drained {
            self.association
                .as_mut()
                .ok_or(Error::NotStarted)?
                .handle_timeout(instant)?;
            self.protocol_time = now;
            self.drained = false;
        }
        let mut buffer = [0u8; 8192];
        while work < budget {
            work = work.saturating_add(1);
            if let Some(pending) = self.socket_output.take() {
                if !self.submit_output(&pending.bytes, pending.protected.as_ref(), elapsed, send)? {
                    self.socket_output = Some(pending);
                    self.retry_at = Some(Time(self.now.0.saturating_add(1_000_000)));
                    self.drained = true;
                    break;
                }
                if pending.protected.is_some() {
                    self.staged = None;
                }
                self.retry_at = None;
                self.drained = false;
                continue;
            }
            let output = self
                .association
                .as_mut()
                .ok_or(Error::NotStarted)?
                .poll_output(&mut buffer)?;
            match output {
                Output::Datagram(bytes) => {
                    if bytes.len() > wire::UDP_BUDGET {
                        return Err(Error::Security);
                    }
                    let records = self
                        .association
                        .as_ref()
                        .ok_or(Error::NotStarted)?
                        .last_output_application_records();
                    if records > 1 {
                        return Err(Error::Security);
                    }
                    let protected = if records == 1 {
                        Some(self.protected.take().ok_or(Error::Security)?)
                    } else {
                        None
                    };
                    if !self.submit_output(bytes, protected.as_ref(), elapsed, send)? {
                        self.socket_output = Some(SocketOutput {
                            bytes: bytes.into(),
                            protected,
                        });
                        self.retry_at = Some(Time(self.now.0.saturating_add(1_000_000)));
                        self.drained = true;
                        break;
                    }
                    if protected.is_some() {
                        self.staged = None;
                    }
                }
                Output::ApplicationData(bytes) => {
                    self.on_application(bytes, elapsed, receive)?;
                }
                Output::Connected => {
                    self.health.ready = true;
                    self.sender.set_availability(EPOCH, true);
                    self.update_controller(elapsed)?;
                }
                Output::Timeout(deadline) => {
                    self.drained = true;
                    self.protocol_deadline = Some(Time(
                        origin.0.saturating_add(
                            u64::try_from(
                                deadline
                                    .saturating_duration_since(
                                        self.origin.ok_or(Error::NotStarted)?.1,
                                    )
                                    .as_nanos(),
                            )
                            .unwrap_or(u64::MAX),
                        ),
                    ));
                    if self.protected.is_some() {
                        if !self.peer.is_quic() {
                            return Err(Error::Security);
                        }
                        break;
                    }
                    if !self.health.ready || !self.prepare_feedback(elapsed)? {
                        break;
                    }
                }
                Output::Progress => {}
                Output::KeyingMaterial(_, _) => return Err(Error::Security),
                Output::BufferTooSmall { .. } => return Err(Error::Capacity),
                Output::Closed => return Err(Error::Closed),
            }
        }
        Ok(work)
    }

    fn submit_output(
        &mut self,
        bytes: &[u8],
        protected: Option<&Protected>,
        elapsed: Duration,
        send: &mut impl FnMut(Datagram<'_>) -> SendOutcome,
    ) -> Result<bool, Error> {
        if matches!(protected, Some(Protected::Data { expires_at, .. }) if *expires_at <= self.now)
        {
            self.health.expired = self.health.expired.saturating_add(1);
            return Ok(true);
        }
        let commit = match protected {
            Some(Protected::Data { attempt, .. }) => {
                let context = feedback::SendCommit {
                    at: elapsed,
                    wire_len: bytes.len(),
                    path_epoch: EPOCH,
                    identity: feedback::SendIdentity {
                        space: SPACE,
                        packet: feedback::PacketId(*attempt),
                        feedback_sequence: Some(feedback::PacketId(*attempt)),
                        tag: *attempt,
                    },
                };
                self.sender
                    .prepare_commit(context)
                    .map_err(|_| Error::Capacity)?;
                self.sender
                    .preflight_commit(context)
                    .map_err(|_| Error::Capacity)?;
                Some(context)
            }
            _ => None,
        };
        let accepted = matches!(send(Datagram { destination: self.peer.endpoint(), payload: bytes }), SendOutcome::Accepted { bytes: submitted } if submitted == bytes.len());
        if accepted {
            if let Some(context) = commit {
                self.sender
                    .commit_preflighted(context)
                    .map_err(|_| Error::Capacity)?;
                self.controller.commit_send(
                    elapsed,
                    bytes.len(),
                    self.sender.observations().bytes_in_flight,
                    false,
                );
                self.health.sent = self.health.sent.saturating_add(1);
                if self.peer.is_quic() {
                    self.feedback_wait.get_or_insert(self.now);
                }
            }
            if let Some(Protected::Feedback(report)) = protected {
                self.receiver
                    .accept(*report, report.count, elapsed)
                    .map_err(|_| Error::Capacity)?;
            }
            return Ok(true);
        }
        self.health.send_failed = self.health.send_failed.saturating_add(1);
        if self.peer.is_quic() {
            return Ok(false);
        }
        if matches!(protected, Some(Protected::Feedback(_))) {
            self.receiver
                .defer(
                    elapsed,
                    elapsed.saturating_add(self.health.feedback_interval),
                )
                .map_err(|_| Error::Capacity)?;
        }
        Ok(true)
    }

    fn prepare_feedback(&mut self, now: Duration) -> Result<bool, Error> {
        let mut plain = [0u8; wire::UDP_BUDGET];
        if let Some(report) = self.receiver.report(now) {
            self.report_generation = self
                .report_generation
                .checked_add(1)
                .ok_or(Error::SequenceWindow)?;
            let mut length = wire::feedback_header(
                &mut plain,
                report.base,
                self.report_generation,
                report.count,
            )
            .map_err(|_| Error::Codec)?;
            for index in 0..report.count {
                let packet = feedback::PacketId(
                    report
                        .base
                        .0
                        .checked_add(u64::try_from(index).map_err(|_| Error::Capacity)?)
                        .ok_or(Error::SequenceWindow)?,
                );
                let arrival = self
                    .receiver
                    .receipt(report.space, packet)
                    .map(|receipt| {
                        i64::try_from(receipt.arrived_at.as_micros()).map_err(|_| Error::Time)
                    })
                    .transpose()?;
                length = length.saturating_add(
                    wire::encode_status(plain.get_mut(length..).ok_or(Error::Capacity)?, arrival)
                        .map_err(|_| Error::Codec)?,
                );
            }
            self.association
                .as_mut()
                .ok_or(Error::NotStarted)?
                .send_application_data(plain.get(..length).ok_or(Error::Capacity)?)?;
            self.protected = Some(Protected::Feedback(report));
            self.staged = Some((self.now, length));
            self.drained = false;
            return Ok(true);
        }
        Ok(false)
    }

    /// Copies one datagram into bounded protocol staging only when immediately
    /// admissible. Borrowed input remains caller-owned on every rejection.
    /// `context` is opaque application data, preserved for the legacy v1 profile.
    pub fn try_submit(
        &mut self,
        context: [u8; 4],
        payload: &[u8],
        expires_at: Time,
    ) -> Result<(), Error> {
        let result = self.submit_inner(context, payload, expires_at);
        if let Err(
            error @ (Error::Security
            | Error::Capacity
            | Error::SequenceWindow
            | Error::Codec
            | Error::Time),
        ) = result
        {
            self.close_with_reason(error);
        }
        result
    }

    fn submit_inner(
        &mut self,
        context: [u8; 4],
        payload: &[u8],
        expires_at: Time,
    ) -> Result<(), Error> {
        self.ensure_live()?;
        if expires_at <= self.now {
            return Err(Error::Expired);
        }
        if payload.len() > self.payload_capacity() {
            return Err(Error::Oversize);
        }
        self.admission()?;
        let mut plain = [0u8; wire::UDP_BUDGET];
        // More than half a sequence space without acknowledgment cannot be
        // unwrapped safely. Replace the association rather than alias identity.
        let acknowledged = self.highest_acknowledged.unwrap_or(0);
        if self.attempt.saturating_sub(acknowledged) >= (1u64 << 31) {
            return Err(Error::SequenceWindow);
        }
        let attempt = self.attempt;
        self.attempt = self.attempt.checked_add(1).ok_or(Error::SequenceWindow)?;
        let wire_attempt =
            u32::try_from(attempt & u64::from(u32::MAX)).map_err(|_| Error::SequenceWindow)?;
        wire::data_header(&mut plain, context, wire_attempt).map_err(|_| Error::Codec)?;
        let length = payload.len().saturating_add(wire::DATA_HEADER);
        plain
            .get_mut(wire::DATA_HEADER..length)
            .ok_or(Error::Capacity)?
            .copy_from_slice(payload);
        self.association
            .as_mut()
            .ok_or(Error::NotStarted)?
            .send_application_data(plain.get(..length).ok_or(Error::Capacity)?)?;
        self.protected = Some(Protected::Data {
            attempt,
            expires_at,
        });
        self.staged = Some((self.now, length));
        self.drained = false;
        Ok(())
    }

    /// Non-reserving readiness, useful before expensive application encoding.
    pub fn admission(&mut self) -> Result<(), Error> {
        self.ensure_live()?;
        if !self.health.ready {
            return Err(Error::NotReady);
        }
        if self.feedback_deadline().is_some_and(|at| at <= self.now) {
            self.close_with_reason(Error::FeedbackUnavailable);
            return Err(Error::FeedbackUnavailable);
        }
        if !self.input_ready() || self.protected.is_some() {
            return Err(Error::Blocked);
        }
        let origin = self.origin.ok_or(Error::NotStarted)?.0;
        let now = Duration::from_nanos(self.now.0.saturating_sub(origin.0));
        self.controller
            .update_queue(wire::UDP_BUDGET as u64, Duration::ZERO);
        if self.controller.ready(
            now,
            self.sender.observations().bytes_in_flight,
            wire::UDP_BUDGET,
        ) != pulsebeam_cc::Readiness::Ready
        {
            return Err(Error::Blocked);
        }
        Ok(())
    }

    pub fn payload_capacity(&self) -> usize {
        self.association.as_ref().map_or_else(
            || {
                if self.peer.is_quic() {
                    0
                } else {
                    wire::PAYLOAD_BUDGET
                }
            },
            Engine::payload_capacity,
        )
    }

    fn on_application(
        &mut self,
        bytes: &[u8],
        now: Duration,
        receive: &mut impl FnMut(Application<'_>),
    ) -> Result<(), Error> {
        let Ok(frame) = wire::decode(bytes) else {
            self.health.invalid = self.health.invalid.saturating_add(1);
            return Ok(());
        };
        match frame {
            wire::Frame::Feedback {
                base,
                generation,
                statuses,
            } => {
                if wire::decode_statuses(statuses, &mut self.statuses).is_err() {
                    self.health.invalid = self.health.invalid.saturating_add(1);
                    return Ok(());
                }
                self.sender.process_feedback(feedback::SendFeedback {
                    received_at: now,
                    path_epoch: EPOCH,
                    report: feedback::SendReport::Continuous {
                        base_sequence: base,
                        reference_micros: 0,
                        generation,
                        statuses: &self.statuses,
                    },
                });
                self.update_controller(now)?;
            }
            wire::Frame::Data {
                context,
                attempt,
                payload,
            } => {
                let extended = match self.highest_received {
                    None => u64::from(attempt),
                    Some(highest) => {
                        let low = u32::try_from(highest & u64::from(u32::MAX))
                            .map_err(|_| Error::SequenceWindow)?;
                        highest
                            .checked_add_signed(i64::from(attempt.wrapping_sub(low).cast_signed()))
                            .ok_or(Error::SequenceWindow)?
                    }
                };
                let origin = self.origin.ok_or(Error::NotStarted)?.0;
                let arrival = Duration::from_nanos(
                    self.observed
                        .monotonic
                        .0
                        .checked_sub(origin.0)
                        .ok_or(Error::Time)?,
                );
                let observation = self
                    .receiver
                    .observe(
                        EPOCH,
                        feedback::Receipt {
                            space: SPACE,
                            packet: feedback::PacketId(extended),
                            arrived_at: arrival,
                            ecn: None,
                            tag: extended,
                        },
                    )
                    .map_err(|_| Error::Capacity)?;
                self.highest_received = Some(
                    self.highest_received
                        .map_or(extended, |highest| highest.max(extended)),
                );
                if !matches!(observation, feedback::Observation::Inserted { .. }) {
                    return Ok(());
                }
                self.health.received = self.health.received.saturating_add(1);
                receive(Application {
                    context,
                    payload,
                    observed: self.observed,
                });
            }
        }
        Ok(())
    }

    fn update_controller(&mut self, now: Duration) -> Result<(), Error> {
        let input = self.sender.observations();
        if input.exhausted {
            return Err(Error::Capacity);
        }
        if self.peer.is_quic()
            && input
                .feedback
                .iter()
                .any(|sample| sample.received && sample.newly_acked)
        {
            self.feedback_wait = if input.bytes_in_flight == 0 {
                None
            } else {
                Some(self.now)
            };
        }
        self.samples.clear();
        for item in &input.feedback {
            if item.received {
                self.highest_acknowledged = Some(
                    self.highest_acknowledged
                        .map_or(item.tag, |highest| highest.max(item.tag)),
                );
            }
            self.samples.push(pulsebeam_cc::FeedbackSample {
                sent_at: item.committed_at,
                received_at: item.observed_at.unwrap_or(now),
                transport_bytes: item.transport_bytes,
                received: item.received,
                newly_acked: item.newly_acked,
                lost: item.lost,
                recovered: item.recovered,
                receiver_arrival_micros: match item.receiver_arrival {
                    Some(feedback::ReceiverTime::Micros(value)) => Some(value),
                    _ => None,
                },
                ecn: None,
            });
        }
        self.controller
            .update(
                now,
                pulsebeam_cc::Observation {
                    path_epoch: self.health.ready.then_some(EPOCH.0),
                    path_available: self.health.ready,
                    feedback: &self.samples,
                    feedback_hold: input
                        .timing
                        .and_then(|timing| timing.feedback_hold)
                        .unwrap_or_default(),
                    fresh_network_feedback: input.fresh_network_feedback,
                    bytes_in_flight: input.bytes_in_flight,
                    paced_queue_bytes: if self.demand {
                        wire::UDP_BUDGET as u64
                    } else {
                        0
                    },
                    desired_media_rate: 10_000_000,
                    ecn: pulsebeam_cc::EcnValidation::default(),
                },
            )
            .map_err(|_| Error::Time)?;
        self.sender.clear_observations();
        Ok(())
    }

    fn feedback_deadline(&self) -> Option<Time> {
        self.feedback_wait.map(|at| {
            Time(
                at.0.saturating_add(u64::try_from(FEEDBACK_TIMEOUT.as_nanos()).unwrap_or(u64::MAX)),
            )
        })
    }

    pub fn close(&mut self) {
        self.close_with_reason(Error::Closed);
    }

    fn close_with_reason(&mut self, reason: Error) {
        self.closed = true;
        self.health.closed.get_or_insert(reason);
        self.health.ready = false;
        if let Some(association) = &mut self.association {
            association.close();
        }
        self.sender.abort();
        self.receiver.abort();
        self.protected = None;
        self.staged = None;
        self.socket_output = None;
        self.retry_at = None;
        self.feedback_wait = None;
        self.demand = false;
    }

    fn ensure_live(&self) -> Result<(), Error> {
        if self.closed {
            Err(Error::Closed)
        } else if self.origin.is_none() {
            Err(Error::NotStarted)
        } else {
            Ok(())
        }
    }
}
