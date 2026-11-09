//! Single-owner, sans-I/O host boundary. Sockets and scheduling remain external.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::network::{self, NetworkPeer, Outbound, ReceiveBinding, WireCodec};
use super::*;
use pulsebeam_feedback as feedback;

const EPOCH: feedback::Epoch = feedback::Epoch(1);
const SPACE: feedback::SpaceId = feedback::SpaceId(0);
const HISTORY: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Closed,
    NotStarted,
    Busy,
    Time,
    Security,
    Capacity,
    Codec,
    SequenceWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Accepted { bytes: usize },
    Rejected,
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
    pub stale: u64,
    pub invalid: u64,
    pub local_full: u64,
    pub local_closed: u64,
    pub bytes_in_flight: u64,
}

enum Protected {
    Data(u64),
    Feedback(feedback::ReportWindow),
}

pub struct NetworkDriver<P> {
    peer: NetworkPeer,
    codec: Shared<dyn WireCodec<P>>,
    state: Shared<Fence>,
    table: Shared<Table>,
    outbound: pulsebeam_queue::Receiver<Outbound<P>>,
    commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
    routes: HashMap<NodeRouteKey, ReceiveBinding<P>>,
    association: Option<pulsebeam_dtls::Association>,
    origin: Option<(LinkTime, Instant)>,
    now: LinkTime,
    observed: ReceiveTime,
    drained: bool,
    pending: Option<Outbound<P>>,
    protected: Option<Protected>,
    attempt: u64,
    highest_received: Option<u64>,
    highest_acknowledged: Option<u64>,
    report_generation: i64,
    sender: feedback::Sender,
    receiver: feedback::Receiver,
    controller: pulsebeam_cc::Controller,
    statuses: Vec<feedback::SendStatus>,
    samples: Vec<pulsebeam_cc::FeedbackSample>,
    protocol_deadline: Option<LinkTime>,
    #[cfg(test)]
    test_key_limit: Option<u64>,
    health: Health,
}

impl<P> Drop for NetworkDriver<P> {
    fn drop(&mut self) {
        self.state.store(network::CLOSED, Ordering::Release);
    }
}

impl<P: Clone + Send + 'static> NetworkDriver<P> {
    pub(crate) fn new(
        peer: NetworkPeer,
        codec: Shared<dyn WireCodec<P>>,
        state: Shared<Fence>,
        table: Shared<Table>,
        outbound: pulsebeam_queue::Receiver<Outbound<P>>,
        commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
    ) -> Result<Self, network::LinkError> {
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
        .map_err(|_| network::LinkError::Capacity)?;
        // Class affects feedback latency, never an assumed path capacity or RTT.
        // Both paths retain the same measured congestion controller and pacer.
        let feedback_interval = match peer.class {
            network::NetworkClass::Lan => Duration::from_millis(5),
            network::NetworkClass::Wan => Duration::from_millis(10),
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
        .map_err(|_| network::LinkError::Capacity)?;
        receiver
            .reserve(4 * 1024 * 1024)
            .map_err(|_| network::LinkError::Capacity)?;
        let controller = pulsebeam_cc::Controller::new(pulsebeam_cc::Config::default())
            .map_err(|_| network::LinkError::Capacity)?;
        Ok(Self {
            peer,
            codec,
            state,
            table,
            outbound,
            commands,
            routes: HashMap::new(),
            association: None,
            origin: None,
            now: LinkTime(0),
            observed: ReceiveTime {
                monotonic: LinkTime(0),
                wall: None,
            },
            drained: true,
            pending: None,
            protected: None,
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
            #[cfg(test)]
            test_key_limit: None,
        })
    }

    /// Anchor the upstream engine's Instant domain to the host's LinkTime.
    /// Both arguments come from the same real or virtual host clock observation.
    pub fn start(&mut self, at: LinkTime, instant: Instant) -> Result<(), Error> {
        if self.state.load(Ordering::Acquire) == network::CLOSED {
            return Err(Error::Closed);
        }
        if self.origin.is_some() {
            return Err(Error::Busy);
        }
        #[cfg(test)]
        let association = match self.test_key_limit {
            Some(limit) => self.peer.association_with_test_key_limit(instant, limit),
            None => self.peer.association(instant),
        };
        #[cfg(not(test))]
        let association = self.peer.association(instant);
        self.association = Some(association.map_err(|_| Error::Security)?);
        self.origin = Some((at, instant));
        self.now = at;
        self.observed.monotonic = at;
        self.drained = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_test_key_limit(&mut self, limit: u64) {
        assert!(self.origin.is_none());
        self.test_key_limit = Some(limit);
    }

    /// Resume a settled sequence history near a wrap boundary without billions
    /// of test transmissions. Only fresh, empty-history fixtures may use this.
    #[cfg(test)]
    pub(super) fn seed_test_sequence(&mut self, next_attempt: u64, highest_received: Option<u64>) {
        assert_eq!(self.attempt, 0);
        assert_eq!(self.highest_received, None);
        assert_eq!(self.sender.observations().bytes_in_flight, 0);
        self.attempt = next_attempt;
        self.highest_acknowledged = next_attempt.checked_sub(1);
        self.highest_received = highest_received;
    }

    pub fn health(&self) -> Health {
        Health {
            bytes_in_flight: self.sender.observations().bytes_in_flight,
            ..self.health
        }
    }

    pub async fn readable(&mut self) -> bool {
        if !self.drained {
            return true;
        }
        if self.pending.is_some() {
            return self.commands.readable().await;
        }
        futures_lite::future::or(self.commands.readable(), self.outbound.readable()).await
    }

    pub fn next_deadline(&self) -> Option<LinkTime> {
        let origin = self.origin?.0;
        let now = Duration::from_nanos(self.now.0.saturating_sub(origin.0));
        let candidate = self
            .pending
            .as_ref()
            .map(|_| (self.sender.observations().bytes_in_flight, wire::UDP_BUDGET));
        let relative = [
            self.sender.next_deadline(now),
            self.receiver.deadline(),
            self.controller.next_deadline(now, candidate, None),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|at| {
            LinkTime(
                origin
                    .0
                    .saturating_add(u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)),
            )
        });
        [
            self.protocol_deadline,
            relative,
            self.pending.as_ref().map(|pending| pending.expires_at),
        ]
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
        if !self.drained {
            return Err(Error::Busy);
        }
        if source != self.peer.endpoint || datagram.len() > wire::UDP_BUDGET {
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
            self.close();
            return Err(Error::Security);
        }
        self.drained = false;
        Ok(())
    }

    /// Bounded protocol/queue work. The callback reports actual nonblocking UDP
    /// submission, not mere staging in another host queue. A rejected datagram
    /// is discarded; protecting a later attempt always consumes fresh nonces.
    pub fn poll(
        &mut self,
        now: LinkTime,
        budget: usize,
        mut send: impl FnMut(Datagram<'_>) -> SendOutcome,
    ) -> Result<usize, Error> {
        let result = self.poll_inner(now, budget, &mut send);
        if result.is_err() {
            self.close();
        }
        result
    }

    fn poll_inner(
        &mut self,
        now: LinkTime,
        budget: usize,
        send: &mut impl FnMut(Datagram<'_>) -> SendOutcome,
    ) -> Result<usize, Error> {
        self.ensure_live()?;
        if now < self.now {
            return Err(Error::Time);
        }
        let (origin, instant) = self.origin.ok_or(Error::NotStarted)?;
        let elapsed = Duration::from_nanos(now.0.checked_sub(origin.0).ok_or(Error::Time)?);
        let instant = instant.checked_add(elapsed).ok_or(Error::Time)?;
        self.now = now;
        let mut work = 0usize;
        while work < budget {
            // Commands installed before an inbound packet are applied first.
            if let Ok(binding) = self.commands.try_recv() {
                self.routes
                    .retain(|_, route| self.table.binding_live(route.binding));
                if self.table.binding_live(binding.binding) {
                    self.routes.insert(binding.key, binding);
                }
                work = work.saturating_add(1);
                continue;
            }
            break;
        }
        if work == budget {
            return Ok(work);
        }
        self.sender.expire(elapsed, 256);
        self.update_controller(elapsed)?;
        if self.drained {
            self.association
                .as_mut()
                .ok_or(Error::NotStarted)?
                .handle_timeout(instant)
                .map_err(|_| Error::Security)?;
            self.drained = false;
        }
        let mut buffer = [0u8; 8192];
        while work < budget {
            work = work.saturating_add(1);
            let output = self
                .association
                .as_mut()
                .ok_or(Error::NotStarted)?
                .poll_output(&mut buffer)
                .map_err(|_| Error::Security)?;
            match output {
                pulsebeam_dtls::Output::Datagram(bytes) => {
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
                    let commit = match &protected {
                        Some(Protected::Data(attempt)) => {
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
                    let accepted = matches!(send(Datagram { destination: self.peer.endpoint, payload: bytes }), SendOutcome::Accepted { bytes: submitted } if submitted == bytes.len());
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
                        }
                        if let Some(Protected::Feedback(report)) = protected {
                            self.receiver
                                .accept(report, report.count, elapsed)
                                .map_err(|_| Error::Capacity)?;
                        }
                    } else {
                        self.health.send_failed = self.health.send_failed.saturating_add(1);
                        if matches!(protected, Some(Protected::Feedback(_))) {
                            self.receiver
                                .defer(
                                    elapsed,
                                    elapsed.saturating_add(self.health.feedback_interval),
                                )
                                .map_err(|_| Error::Capacity)?;
                        }
                    }
                }
                pulsebeam_dtls::Output::ApplicationData(bytes) => {
                    self.on_application(bytes, elapsed)?;
                }
                pulsebeam_dtls::Output::Connected => {
                    self.state.store(network::READY, Ordering::Release);
                    self.health.ready = true;
                    self.sender.set_availability(EPOCH, true);
                    self.update_controller(elapsed)?;
                }
                pulsebeam_dtls::Output::Timeout(deadline) => {
                    self.drained = true;
                    self.protocol_deadline = Some(LinkTime(
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
                        return Err(Error::Security);
                    }
                    if !self.health.ready || !self.prepare_next(elapsed)? {
                        break;
                    }
                }
                pulsebeam_dtls::Output::Progress => {}
                pulsebeam_dtls::Output::BufferTooSmall { .. } => return Err(Error::Capacity),
                pulsebeam_dtls::Output::Closed => return Err(Error::Closed),
            }
        }
        Ok(work)
    }

    fn prepare_next(&mut self, now: Duration) -> Result<bool, Error> {
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
                .send_application_data(plain.get(..length).ok_or(Error::Capacity)?)
                .map_err(|_| Error::Security)?;
            self.protected = Some(Protected::Feedback(report));
            self.drained = false;
            return Ok(true);
        }
        if self.pending.is_none() {
            self.pending = self.outbound.try_recv().ok();
        }
        let Some(pending) = &self.pending else {
            return Ok(false);
        };
        if !self.table.binding_live(pending.binding) {
            self.pending = None;
            self.health.stale = self.health.stale.saturating_add(1);
            return Ok(true);
        }
        if pending.expires_at <= self.now {
            self.pending = None;
            self.health.expired = self.health.expired.saturating_add(1);
            return Ok(true);
        }
        self.controller
            .update_queue(wire::UDP_BUDGET as u64, Duration::ZERO);
        if self.controller.ready(
            now,
            self.sender.observations().bytes_in_flight,
            wire::UDP_BUDGET,
        ) != pulsebeam_cc::Readiness::Ready
        {
            return Ok(false);
        }
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
        wire::data_header(&mut plain, pending.address.0, wire_attempt).map_err(|_| Error::Codec)?;
        let length = self
            .codec
            .encode(
                &pending.packet,
                plain
                    .get_mut(
                        wire::DATA_HEADER..wire::DATA_HEADER.saturating_add(wire::PAYLOAD_BUDGET),
                    )
                    .ok_or(Error::Capacity)?,
            )
            .map_err(|_| Error::Codec)?;
        if length > wire::PAYLOAD_BUDGET {
            return Err(Error::Codec);
        }
        let length = length.saturating_add(wire::DATA_HEADER);
        self.association
            .as_mut()
            .ok_or(Error::NotStarted)?
            .send_application_data(plain.get(..length).ok_or(Error::Capacity)?)
            .map_err(|_| Error::Security)?;
        self.pending = None;
        self.protected = Some(Protected::Data(attempt));
        self.drained = false;
        Ok(true)
    }

    fn on_application(&mut self, bytes: &[u8], now: Duration) -> Result<(), Error> {
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
                route,
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
                let Some(binding) = self.routes.get(&NodeRouteKey(route)) else {
                    self.health.stale = self.health.stale.saturating_add(1);
                    return Ok(());
                };
                if !self.table.binding_live(binding.binding) {
                    self.health.stale = self.health.stale.saturating_add(1);
                    return Ok(());
                }
                let Ok(packet) = self.codec.decode(payload) else {
                    self.health.invalid = self.health.invalid.saturating_add(1);
                    return Ok(());
                };
                for destination in &binding.destinations {
                    if !self.table.port_live(destination.port) {
                        self.health.local_closed = self.health.local_closed.saturating_add(1);
                        continue;
                    }
                    let pending = Pending {
                        binding: binding.binding,
                        expires_at: LinkTime(u64::MAX),
                        received: Received {
                            tag: destination.tag,
                            packet: packet.clone(),
                            received_at: self.observed,
                        },
                    };
                    match destination.sender.try_send(pending) {
                        Ok(()) => {}
                        Err(pulsebeam_queue::TrySendError::Full(_)) => {
                            self.health.local_full = self.health.local_full.saturating_add(1);
                        }
                        Err(pulsebeam_queue::TrySendError::Closed(_)) => {
                            self.health.local_closed = self.health.local_closed.saturating_add(1);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn update_controller(&mut self, now: Duration) -> Result<(), Error> {
        let input = self.sender.observations();
        if input.exhausted {
            return Err(Error::Capacity);
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
                    paced_queue_bytes: if self.pending.is_some() {
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

    pub fn close(&mut self) {
        self.state.store(network::CLOSED, Ordering::Release);
        self.health.ready = false;
        if let Some(association) = &mut self.association {
            association.close();
        }
        self.pending = None;
        self.protected = None;
        self.routes.clear();
    }

    fn ensure_live(&self) -> Result<(), Error> {
        if self.state.load(Ordering::Acquire) == network::CLOSED {
            Err(Error::Closed)
        } else if self.origin.is_none() {
            Err(Error::NotStarted)
        } else {
            Ok(())
        }
    }
}
