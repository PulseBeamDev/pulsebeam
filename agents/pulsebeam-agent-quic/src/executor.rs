//! Linux carrier execution for Core's generation-scoped QUIC effects.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    num::NonZeroU64,
    time::{Duration, Instant},
};

use noq_proto::{ClientConfig, Transmit};
use pulsebeam_agent_core::{
    Failure, FailureClass, Generation, OperationId, QuicAdmit, QuicDataSend, QuicEffect, QuicEvent,
};
use pulsebeam_core::identity::ConnectionId;
use pulsebeam_udp::ReceivedDatagram;

use crate::{
    frame::{Kind, Phase},
    records::{Admit, Datagram, DatagramKind, Denial, PictureLossIndication, Renew, RenewResult},
    transport::{
        DatagramAdmission,
        client::{
            ClientTransport,
            admission::{AdmissionError, ClientAdmission},
        },
        control::{self, ControlStream, Read, Write},
    },
};

pub mod linux;
pub mod udp;

const RENEW_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    Stale,
    NotAdmitted,
    Control(control::Error),
    Data(DatagramAdmission),
}

/// Drives one association. The host provides address resolution, a configured TLS
/// verifier and UDP scheduling. Core remains the authority for intent and retries.
pub struct Association {
    generation: Generation,
    transport: ClientTransport,
    stage: Stage,
    pending_data: VecDeque<QuicDataSend>,
    more_data_work: bool,
}

enum Stage {
    Admitting(ClientAdmission),
    Active {
        control: ControlStream,
        pending: Option<PendingControl>,
        renewal: Option<PendingRenewal>,
    },
    Closing(Instant),
    Finished,
}

struct PendingControl {
    operation: OperationId,
    payload: Option<Vec<u8>>,
}

struct PendingRenewal {
    operation: OperationId,
    deadline: Instant,
    frame: RenewalFrame,
}

enum RenewalFrame {
    Queued(Vec<u8>),
    Writing,
    AwaitingResult,
}

impl Association {
    pub fn connect(
        clock_origin: Instant,
        now: Instant,
        config: ClientConfig,
        resolved: SocketAddr,
        admit: QuicAdmit,
    ) -> Result<Self, QuicEvent> {
        let generation = admit.generation;
        let failed = |failure| QuicEvent::AdmissionFailed {
            generation,
            failure,
        };
        let expired = || failed(transient("QUIC placement expired"));
        let deadline = clock_origin
            .checked_add(admit.deadline)
            .ok_or_else(expired)?;
        if now >= deadline {
            return Err(expired());
        }
        let attempt = NonZeroU64::new(generation.get())
            .ok_or_else(|| failed(protocol("invalid QUIC generation")))?;
        let admission = ClientAdmission::new(
            &Admit {
                attempt,
                room: admit.room.as_deref(),
                owner: &admit.placement.owner,
                token: &admit.token,
                successor: None,
            },
            deadline,
        )
        .map_err(|_| failed(protocol("invalid QUIC admission")))?;
        let address = SocketAddr::new(resolved.ip(), admit.placement.port.get());
        let transport = ClientTransport::connect(now, config, address, &admit.placement.host)
            .map_err(|_| failed(transient("could not start QUIC transport")))?;
        Ok(Self {
            generation,
            transport,
            stage: Stage::Admitting(admission),
            pending_data: VecDeque::new(),
            more_data_work: false,
        })
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn submit(&mut self, effect: QuicEffect, now: Instant) -> Result<(), SubmitError> {
        match effect {
            QuicEffect::Close { generation } if generation == self.generation => {
                if !matches!(self.stage, Stage::Closing(_)) {
                    // A transport close also retires the server's association; no app
                    // record is required to close an incomplete admission.
                    crate::transport::close(self.transport.connection(), now);
                    self.pending_data.clear();
                    self.stage = Stage::Closing(now.checked_add(CLOSE_GRACE).unwrap_or(now));
                }
                Ok(())
            }
            QuicEffect::SendControl {
                generation,
                operation,
                payload,
            } if generation == self.generation => {
                let Stage::Active { pending, .. } = &mut self.stage else {
                    return Err(SubmitError::NotAdmitted);
                };
                if pending.is_some() {
                    return Err(SubmitError::Control(control::Error::Backpressure));
                }
                *pending = Some(PendingControl {
                    operation,
                    payload: Some(payload),
                });
                Ok(())
            }
            QuicEffect::RetireData {
                generation,
                channel,
            } if generation == self.generation => {
                self.pending_data.retain(|send| send.channel != channel);
                Ok(())
            }
            QuicEffect::SendData(send) if send.generation == self.generation => {
                const MAX_PENDING_DATA: usize = 64;
                if !matches!(self.stage, Stage::Active { .. }) {
                    return Err(SubmitError::NotAdmitted);
                }
                if send.payload.is_empty()
                    || send.payload.len() > pulsebeam_proto::data_record::MAX_RECORD_BYTES
                {
                    return Err(SubmitError::Data(DatagramAdmission::Size));
                }
                if self.pending_data.len() >= MAX_PENDING_DATA {
                    return Err(SubmitError::Data(DatagramAdmission::Full));
                }
                self.pending_data.push_back(send);
                Ok(())
            }
            QuicEffect::Renew(effect) if effect.generation == self.generation => {
                let Stage::Active { renewal, .. } = &mut self.stage else {
                    return Err(SubmitError::NotAdmitted);
                };
                if renewal.is_some() {
                    return Err(SubmitError::Control(control::Error::Backpressure));
                }
                let revision =
                    NonZeroU64::new(effect.operation.get()).ok_or(SubmitError::NotAdmitted)?;
                let body = Renew {
                    revision,
                    token: &effect.token,
                }
                .encode()
                .map_err(|error| SubmitError::Control(control::Error::Frame(error)))?;
                *renewal = Some(PendingRenewal {
                    operation: effect.operation,
                    deadline: now.checked_add(RENEW_TIMEOUT).unwrap_or(now),
                    frame: RenewalFrame::Queued(body),
                });
                Ok(())
            }
            _ => Err(SubmitError::Stale),
        }
    }

    pub fn receive(&mut self, now: Instant, packet: ReceivedDatagram<'_>) {
        self.transport.receive(now, packet);
    }

    pub fn poll_packet(&mut self, now: Instant, output: &mut Vec<u8>) -> (Option<Transmit>, bool) {
        let (packet, more) = self.transport.poll(now, output);
        (packet, more || self.more_data_work)
    }

    pub fn deadline(&mut self) -> Option<Instant> {
        let application = match &self.stage {
            Stage::Admitting(admission) => Some(admission.deadline()),
            Stage::Active {
                control, renewal, ..
            } => control
                .deadline()
                .into_iter()
                .chain(renewal.as_ref().map(|renewal| renewal.deadline))
                .min(),
            Stage::Closing(deadline) => Some(*deadline),
            Stage::Finished => None,
        };
        application
            .into_iter()
            .chain(self.transport.deadline())
            .min()
    }

    pub fn poll_event(&mut self, now: Instant) -> Option<QuicEvent> {
        self.more_data_work = false;
        let generation = self.generation;
        let connection = self.transport.connection();
        let failure = match &mut self.stage {
            Stage::Admitting(admission) => match admission.poll(connection, now) {
                Ok(None) => return None,
                Ok(Some((accepted, control))) => {
                    self.stage = Stage::Active {
                        control,
                        pending: None,
                        renewal: None,
                    };
                    return Some(QuicEvent::Accepted {
                        generation,
                        connection_id: ConnectionId::from_bytes(accepted.connection).as_str(),
                        participant_id: accepted.participant,
                        room: accepted.room,
                        capabilities: accepted.capabilities,
                    });
                }
                Err(error) => {
                    let failure = match error {
                        AdmissionError::Denied(Denial::Denied) => Failure {
                            class: FailureClass::Authorization,
                            message: "QUIC admission denied".into(),
                        },
                        AdmissionError::Expired
                        | AdmissionError::Denied(Denial::Placement | Denial::Capacity)
                        | AdmissionError::Transport(_) => transient("QUIC admission unavailable"),
                        AdmissionError::Denied(Denial::AttemptUnavailable) => Failure {
                            class: FailureClass::ResourceExpired,
                            message: "QUIC admission attempt expired".into(),
                        },
                        _ => protocol("invalid QUIC admission response"),
                    };
                    self.stage = Stage::Finished;
                    return Some(QuicEvent::AdmissionFailed {
                        generation,
                        failure,
                    });
                }
            },
            Stage::Active {
                control,
                pending,
                renewal,
            } => match poll_active(generation, connection, control, pending, renewal, now) {
                Ok(Some(event)) => return Some(event),
                Ok(None) => {
                    if let Some(event) = self.poll_data_send() {
                        return Some(event);
                    }
                    let connection = self.transport.connection();
                    use crate::transport::read_datagram;
                    const DATA_INPUT_BUDGET: usize = 16;
                    for _ in 0..DATA_INPUT_BUDGET {
                        let datagram = read_datagram(connection, |datagram| match datagram.kind {
                            DatagramKind::Data => Some(QuicEvent::Data {
                                generation,
                                binding: datagram.binding,
                                payload: datagram.payload.to_vec(),
                            }),
                            DatagramKind::Rtp => Some(QuicEvent::Rtp {
                                generation,
                                binding: datagram.binding,
                                payload: datagram.payload.to_vec(),
                            }),
                            DatagramKind::Rtcp => PictureLossIndication::decode(datagram.payload)
                                .ok()
                                .map(|pli| QuicEvent::PictureLoss {
                                    generation,
                                    binding: datagram.binding,
                                    media_ssrc: pli.media_ssrc,
                                }),
                        })?;
                        if let Ok(Some(event)) = datagram {
                            return Some(event);
                        }
                    }
                    self.more_data_work = true;
                    return None;
                }
                Err(failure) => failure,
            },
            Stage::Closing(deadline) => {
                if connection.is_drained() || now >= *deadline {
                    self.stage = Stage::Finished;
                    return Some(QuicEvent::Closed { generation });
                }
                return None;
            }
            Stage::Finished => return None,
        };
        self.stage = Stage::Finished;
        Some(QuicEvent::TransportLost {
            generation,
            failure,
        })
    }
    fn poll_data_send(&mut self) -> Option<QuicEvent> {
        let pending = self.pending_data.front()?;
        let result = match self.transport.offer_datagram(Datagram {
            kind: DatagramKind::Data,
            binding: pending.binding,
            payload: &pending.payload,
        }) {
            Ok(()) => Ok(()),
            Err(DatagramAdmission::ControlTurn) => {
                self.more_data_work = true;
                return None;
            }
            Err(DatagramAdmission::Full) => return None,
            Err(error) => Err(format!("QUIC DATA admission: {error:?}")),
        };
        let sent = self.pending_data.pop_front()?;
        Some(QuicEvent::DataSent {
            generation: sent.generation,
            channel: sent.channel,
            operation: sent.operation,
            result,
        })
    }
}

fn poll_active(
    generation: Generation,
    connection: &mut noq_proto::Connection,
    control: &mut ControlStream,
    pending: &mut Option<PendingControl>,
    renewal: &mut Option<PendingRenewal>,
    now: Instant,
) -> Result<Option<QuicEvent>, Failure> {
    if connection.is_closed() {
        return Err(transient("QUIC association closed"));
    }
    if renewal.as_ref().is_some_and(|value| now >= value.deadline) {
        return Err(transient("QUIC renewal outcome timed out"));
    }
    let write_error = |_| transient("QUIC control send failed");
    let writing_renewal = renewal
        .as_ref()
        .is_some_and(|value| matches!(value.frame, RenewalFrame::Writing));
    if !writing_renewal && let Some(value) = pending.as_mut() {
        if let Some(payload) = value.payload.take() {
            let kind =
                if pulsebeam_proto::state::codec::decode_data_control_frame(&payload).is_some() {
                    Kind::DataControl
                } else {
                    Kind::ClientState
                };
            control
                .offer(kind, &payload, Phase::Admitted)
                .map_err(write_error)?;
        }
        if matches!(
            control.write(connection, now).map_err(write_error)?,
            Write::Admitted { .. }
        ) {
            return Ok(pending.take().map(|value| QuicEvent::ControlSent {
                generation,
                operation: value.operation,
            }));
        }
    } else if let Some(value) = renewal.as_mut() {
        if let RenewalFrame::Queued(payload) = &value.frame {
            control
                .offer(Kind::Renew, payload, Phase::Admitted)
                .map_err(write_error)?;
            value.frame = RenewalFrame::Writing;
        }
        if matches!(value.frame, RenewalFrame::Writing)
            && matches!(
                control.write(connection, now).map_err(write_error)?,
                Write::Admitted { .. }
            )
        {
            value.frame = RenewalFrame::AwaitingResult;
        }
    }
    let read = control
        .read(connection, Phase::Admitted, now)
        .map_err(|_| protocol("invalid QUIC control frame"))?;
    match read {
        Read::Pending => Ok(None),
        Read::Frame(frame) if matches!(frame.kind, Kind::ServerState | Kind::DataControl) => {
            Ok(Some(QuicEvent::Control {
                generation,
                payload: frame.body,
            }))
        }
        Read::Frame(frame) if frame.kind == Kind::RenewResult => {
            let result = RenewResult::decode(&frame.body)
                .map_err(|_| protocol("invalid QUIC renewal result"))?;
            let Some(value) = renewal.take().filter(|value| {
                matches!(value.frame, RenewalFrame::AwaitingResult)
                    && value.operation.get() == result.request.get()
            }) else {
                return Err(protocol("uncorrelated QUIC renewal result"));
            };
            Ok(Some(QuicEvent::Renewed {
                generation,
                operation: value.operation,
                revision: result.accepted.map(|lease| lease.revision.get()),
            }))
        }
        Read::Frame(frame) if frame.kind == Kind::Close => {
            Err(transient("QUIC association closed"))
        }
        Read::Finished => Err(transient("QUIC control stream closed")),
        _ => Err(protocol("invalid QUIC control frame")),
    }
}

fn transient(message: &'static str) -> Failure {
    Failure {
        class: FailureClass::Transient,
        message: message.into(),
    }
}

fn protocol(message: &'static str) -> Failure {
    Failure {
        class: FailureClass::Protocol,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_core::{
        Agent, AgentCommand, AgentConfig, Effect, HostEvent, HttpEffect, HttpEvent, HttpResponse,
    };

    fn dial_effect() -> QuicAdmit {
        let mut agent = Agent::new_quic(AgentConfig {
            endpoint: "https://api.example".into(),
            token: "token".into(),
            retry: Default::default(),
            log_level: Default::default(),
        })
        .unwrap();
        agent.command(AgentCommand::Connect).unwrap();
        let Some(Effect::Http(HttpEffect::Request { operation, .. })) = agent.next_effect() else {
            panic!("placement request")
        };
        agent.handle(HostEvent::Http(HttpEvent::Response { operation, response: HttpResponse {
            status: 200, headers: vec![], body: br#"{"profile":"pulsebeam-rt-v1","authority":{"host":"edge.example","port":4443},"owner":"owner","valid_for_ms":1000}"#.to_vec(),
        } })).unwrap();
        let Some(Effect::Quic(QuicEffect::Admit(admit))) = agent.next_effect() else {
            panic!("dial effect")
        };
        admit
    }

    #[test]
    #[allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "the test owns an injected clock origin and rustls time provider"
    )]
    fn expired_placement_does_not_start_transport() {
        let admit = dial_effect();
        let now = Instant::now();
        let elapsed = now.checked_add(admit.deadline).unwrap();
        let config = crate::transport::client_config(
            rustls::RootCertStore::empty(),
            std::sync::Arc::new(rustls::time_provider::DefaultTimeProvider),
        )
        .unwrap();
        let generation = admit.generation;
        let result = Association::connect(
            now,
            elapsed,
            config,
            "127.0.0.1:4443".parse().unwrap(),
            admit,
        );
        assert!(
            matches!(result, Err(QuicEvent::AdmissionFailed { generation: g, failure }) if g == generation && failure.class == FailureClass::Transient)
        );
    }
}
