//! Native carrier execution for Core's generation-scoped QUIC effects.

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU64},
    time::{Duration, Instant},
};

use noq_proto::{ClientConfig, Transmit};
use pulsebeam_agent_core::{
    ChannelId, Failure, FailureClass, Generation, OperationId, QuicAdmit, QuicDataSend, QuicEffect,
    QuicEvent,
};
use pulsebeam_core::{
    fabric::{
        FabricPacket, Header, Profile,
        packet::{Agreement, MAX_PACKET_BYTES},
    },
    identity::ConnectionId,
};
use pulsebeam_udp::ReceivedDatagram;

use crate::{
    data_bindings::FabricOffer,
    frame::{Kind, Phase},
    records::{Admit, Datagram, DatagramKind, Denial, PictureLossIndication, Renew, RenewResult},
    transport::{
        DatagramAdmission, NativeDatagram,
        client::{
            ClientTransport,
            admission::{AdmissionError, ClientAdmission},
        },
        control::{self, ControlStream, Read, Write},
        framing::{Receiver, SendError, SendProgress, Sender},
        read_native_datagram,
    },
};

pub mod host;
pub mod recorder;
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
    pending_data: VecDeque<PendingData>,
    data_senders: BTreeMap<NonZeroU16, DataSender>,
    receivers: BTreeMap<NonZeroU16, PacketReceiver>,
    #[cfg(feature = "sim")]
    data_staging: DataStaging,
    bulk_due: bool,
    more_data_work: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiveBinding {
    Data,
    Media {
        profile: Profile,
        ssrc: u32,
        payload_type: u8,
    },
}

struct PacketReceiver {
    binding: ReceiveBinding,
    agreement: Agreement,
    framer: Receiver,
}

struct DataSender {
    channel: ChannelId,
    agreement: Agreement,
    framer: Sender,
}

struct PendingData {
    send: QuicDataSend,
    #[cfg(feature = "sim")]
    queued_at: Instant,
}

#[cfg(feature = "sim")]
#[derive(Clone, Copy, Debug, Default)]
pub struct DataStaging {
    pub peak_records: usize,
    pub peak_bytes: usize,
    pub max_wait: Duration,
}

impl PendingData {
    fn new(send: QuicDataSend, _now: Instant) -> Self {
        Self {
            send,
            #[cfg(feature = "sim")]
            queued_at: _now,
        }
    }
}

enum Stage {
    Admitting(ClientAdmission),
    Active {
        control: ControlStream,
        pending: Option<PendingControl>,
        renewal: Option<PendingRenewal>,
        peer_fabric: crate::data_bindings::FabricOffer,
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
        mut config: ClientConfig,
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
                fabric: crate::data_bindings::FabricOffer::CURRENT,
                attempt,
                room: admit.room.as_deref(),
                owner: &admit.placement.owner,
                token: &admit.token,
                successor: admit.successor.as_deref(),
            },
            deadline,
        )
        .map_err(|_| failed(protocol("invalid QUIC admission")))?;
        let address = SocketAddr::new(resolved.ip(), admit.placement.port.get());
        crate::transport::routing::set_initial_destination(
            &mut config,
            admit.placement.initial_dcid,
        );
        let transport = ClientTransport::connect(now, config, address, &admit.placement.host)
            .map_err(|_| failed(transient("could not start QUIC transport")))?;
        Ok(Self {
            generation,
            transport,
            stage: Stage::Admitting(admission),
            pending_data: VecDeque::new(),
            data_senders: BTreeMap::new(),
            receivers: BTreeMap::new(),
            #[cfg(feature = "sim")]
            data_staging: DataStaging::default(),
            bulk_due: false,
            more_data_work: false,
        })
    }

    pub fn peer_fabric(&self) -> Option<crate::data_bindings::FabricOffer> {
        match self.stage {
            Stage::Active { peer_fabric, .. } => Some(peer_fabric),
            _ => None,
        }
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
                self.pending_data
                    .retain(|data| data.send.channel != channel);
                self.data_senders
                    .retain(|_, sender| sender.channel != channel);
                Ok(())
            }
            QuicEffect::SendData(send) if send.generation == self.generation => {
                const MAX_PENDING_DATA: usize = 64;
                let peer = self.peer_fabric().ok_or(SubmitError::NotAdmitted)?;
                if pulsebeam_proto::data_record::wire_identity(&send.payload).is_err() {
                    return Err(SubmitError::Data(DatagramAdmission::Size));
                }
                if self.pending_data.len() >= MAX_PENDING_DATA {
                    return Err(SubmitError::Data(DatagramAdmission::Full));
                }
                if let Some(sender) = self.data_senders.get(&send.binding) {
                    if sender.channel != send.channel {
                        return Err(SubmitError::Stale);
                    }
                } else {
                    if self.data_senders.len() >= MAX_PENDING_DATA {
                        return Err(SubmitError::Data(DatagramAdmission::Full));
                    }
                    let selected = FabricOffer::CURRENT
                        .bind(peer, Profile::Data, 0)
                        .map_err(|_| SubmitError::Data(DatagramAdmission::Unsupported))?;
                    self.data_senders.insert(
                        send.binding,
                        DataSender {
                            channel: send.channel,
                            agreement: selected.agreement,
                            framer: Sender::new(selected.framing, send.binding),
                        },
                    );
                }
                self.pending_data.push_back(PendingData::new(send, now));
                #[cfg(feature = "sim")]
                {
                    self.data_staging.peak_records =
                        self.data_staging.peak_records.max(self.pending_data.len());
                    self.data_staging.peak_bytes = self.data_staging.peak_bytes.max(
                        self.pending_data
                            .iter()
                            .map(|data| data.send.payload.len())
                            .sum(),
                    );
                }
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
            .chain(
                self.receivers
                    .values()
                    .filter_map(|receiver| receiver.framer.next_deadline()),
            )
            .min()
    }

    pub(crate) fn poll_event(
        &mut self,
        now: Instant,
        authorize: impl Fn(NonZeroU16) -> Option<(ReceiveBinding, bool)>,
    ) -> Option<QuicEvent> {
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
                        peer_fabric: accepted.fabric,
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
                ..
            } => match poll_active(generation, connection, control, pending, renewal, now) {
                Ok(Some(event)) => return Some(event),
                Ok(None) => {
                    if let Some(event) = self.poll_data_send(now) {
                        return Some(event);
                    }
                    let offer = self.peer_fabric()?;
                    let connection = self.transport.connection();
                    let receivers = &mut self.receivers;
                    receivers.retain(|&id, receiver| match authorize(id) {
                        Some((binding, ready)) if binding == receiver.binding => {
                            if ready {
                                receiver.framer.expire(now);
                            } else {
                                receiver.framer.discard_active();
                            }
                            true
                        }
                        _ => false,
                    });
                    const DATA_INPUT_BUDGET: usize = 16;
                    for _ in 0..DATA_INPUT_BUDGET {
                        let datagram =
                            read_native_datagram(connection, |datagram| match datagram {
                                NativeDatagram::Packet { binding, wire } => {
                                    let (selected, true) = authorize(binding)? else {
                                        return None;
                                    };
                                    receive_packet(
                                        receivers, offer, generation, binding, selected, now, wire,
                                    )
                                }
                                NativeDatagram::Endpoint(datagram)
                                    if datagram.kind == DatagramKind::Rtcp =>
                                {
                                    PictureLossIndication::decode(datagram.payload).ok().map(
                                        |pli| QuicEvent::PictureLoss {
                                            generation,
                                            binding: datagram.binding,
                                            media_ssrc: pli.media_ssrc,
                                        },
                                    )
                                }
                                NativeDatagram::Endpoint(_) => None,
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
    fn offer_media(&mut self, datagram: Datagram<'_>) -> Result<(), DatagramAdmission> {
        if !matches!(self.stage, Stage::Active { .. }) {
            return Err(DatagramAdmission::Unsupported);
        }
        // Alternate with pending bulk after successful media admission, not
        // after a blocked attempt. Stream control retains its own turns.
        if self.bulk_due && !self.pending_data.is_empty() {
            return Err(DatagramAdmission::Full);
        }
        self.transport.offer_datagram(datagram)?;
        self.bulk_due = true;
        Ok(())
    }

    fn offer_media_packet(
        &mut self,
        sender: &mut Sender,
        now: Instant,
    ) -> Result<SendProgress, SendError> {
        if self.peer_fabric().is_none() {
            return Err(SendError::Unsupported);
        }
        if self.bulk_due && !self.pending_data.is_empty() {
            return Ok(SendProgress::Deferred(DatagramAdmission::Full));
        }
        let status = self.transport.offer_packet(sender, now)?;
        if matches!(status, SendProgress::Partial | SendProgress::Admitted) {
            self.bulk_due = true;
        }
        Ok(status)
    }

    fn poll_data_send(&mut self, _now: Instant) -> Option<QuicEvent> {
        let pending = &self.pending_data.front()?.send;
        let result = self
            .data_senders
            .get_mut(&pending.binding)
            .ok_or(SendError::Unsupported)
            .and_then(|sender| {
                if !sender.framer.is_pending() {
                    let bytes = encode_data(sender.agreement, &pending.payload)?;
                    sender.framer.start(_now, bytes)?;
                }
                self.transport.offer_packet(&mut sender.framer, _now)
            });
        let result = match result {
            Ok(SendProgress::Admitted) => Ok(()),
            Ok(SendProgress::Partial) => {
                self.more_data_work = true;
                return None;
            }
            Ok(SendProgress::Deferred(reason)) => {
                self.more_data_work = reason == DatagramAdmission::ControlTurn;
                return None;
            }
            Ok(SendProgress::Idle) => Err("QUIC DATA framer lost its pending record".into()),
            Err(error) => Err(format!("QUIC DATA admission: {error:?}")),
        };
        let pending = self.pending_data.pop_front()?;
        #[cfg(feature = "sim")]
        {
            self.data_staging.max_wait = self
                .data_staging
                .max_wait
                .max(_now.saturating_duration_since(pending.queued_at));
        }
        let sent = pending.send;
        self.bulk_due = false;
        Some(QuicEvent::DataSent {
            generation: sent.generation,
            channel: sent.channel,
            operation: sent.operation,
            result,
        })
    }
}

fn receive_packet(
    receivers: &mut BTreeMap<NonZeroU16, PacketReceiver>,
    offer: FabricOffer,
    generation: Generation,
    id: NonZeroU16,
    binding: ReceiveBinding,
    now: Instant,
    wire: &[u8],
) -> Option<QuicEvent> {
    let room = receivers.len() < 64;
    let receiver = match receivers.entry(id) {
        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::btree_map::Entry::Vacant(entry) if room => {
            let profile = match binding {
                ReceiveBinding::Data => Profile::Data,
                ReceiveBinding::Media { profile, .. } => profile,
            };
            let selected = FabricOffer::CURRENT.bind(offer, profile, 0).ok()?;
            entry.insert(PacketReceiver {
                binding,
                agreement: selected.agreement,
                framer: Receiver::new(selected.framing, id),
            })
        }
        _ => return None,
    };
    if receiver.binding != binding {
        return None;
    }
    let packet = receiver.framer.receive(now, wire).ok()??;
    let packet = receiver.agreement.decode(packet.as_ref()).ok()?;
    let header = packet.header();
    match binding {
        ReceiveBinding::Data => {
            let identity = pulsebeam_proto::data_record::wire_identity(packet.payload()).ok()?;
            (header.marker == identity.final_fragment()).then(|| QuicEvent::Data {
                generation,
                binding: id,
                payload: packet.payload().to_vec(),
            })
        }
        ReceiveBinding::Media {
            ssrc, payload_type, ..
        } => {
            // Reuse the Linux encoded depacketizer's RTP-shaped input locally.
            // Packet authority and extensions are resolved before this adapter.
            let mut payload = Vec::with_capacity(12_usize.saturating_add(packet.payload().len()));
            payload.extend_from_slice(&[0x80, payload_type | (u8::from(header.marker) << 7)]);
            let [.., a, b] = header.sequence.to_be_bytes();
            payload.extend_from_slice(&[a, b]);
            payload.extend_from_slice(&header.timestamp.to_be_bytes());
            payload.extend_from_slice(&ssrc.to_be_bytes());
            payload.extend_from_slice(packet.payload());
            Some(QuicEvent::Rtp {
                generation,
                binding: id,
                payload,
            })
        }
    }
}

fn encode_data(agreement: Agreement, record: &[u8]) -> Result<Box<[u8]>, SendError> {
    let identity =
        pulsebeam_proto::data_record::wire_identity(record).map_err(|_| SendError::Size)?;
    // The edge allocates DATA packet sequence and canonical timestamp on
    // admission. The independent unit and fragment coordinates stay in DATA.
    let packet = FabricPacket::new(
        Header {
            profile: Profile::Data,
            marker: identity.final_fragment(),
            sequence: 0,
            timestamp: 0,
        },
        &[],
        record,
    )
    .map_err(|_| SendError::Size)?;
    let mut bytes = [0; MAX_PACKET_BYTES];
    let len = agreement
        .write(&packet, &mut bytes)
        .map_err(|_| SendError::Size)?;
    Ok(Box::from(bytes.get(..len).ok_or(SendError::Size)?))
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
            status: 200, headers: vec![], body: br#"{"profile":"pulsebeam-rt-v1","authority":{"host":"edge.example","port":4443},"owner":"owner","initial_dcid":"AAAAAAAAAAAAAAAAAAAAAAAAAAA","valid_for_ms":1000}"#.to_vec(),
        } })).unwrap();
        let Some(Effect::Quic(QuicEffect::Admit(admit))) = agent.next_effect() else {
            panic!("dial effect")
        };
        admit
    }

    #[cfg(all(feature = "portable-scalar", not(feature = "sim")))]
    #[tokio::test]
    #[allow(
        clippy::disallowed_types,
        reason = "Noq and rustls require shared immutable TLS configuration"
    )]
    async fn portable_scalar_association_reaches_verified_admission() {
        use std::sync::Arc;

        use crate::transport::{self, UDP_PAYLOAD_BYTES, listener::Listener};
        use rustls::{
            RootCertStore, pki_types::PrivatePkcs8KeyDer, time_provider::DefaultTimeProvider,
        };
        use tokio::{net::UdpSocket, time};

        let identity = rcgen::generate_simple_self_signed(vec!["edge.example".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(identity.cert.der().clone()).unwrap();
        let config = transport::system_client_config(roots).unwrap();
        let tls = transport::server_config(
            vec![identity.cert.der().clone()],
            PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
            Arc::new(DefaultTimeProvider),
            Arc::new(noq_proto::StdSystemTime),
            std::num::NonZeroUsize::MIN,
        )
        .unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let mut admit = dial_effect();
        admit.placement.port = NonZeroU16::new(address.port()).unwrap();
        admit.placement.initial_dcid = transport::routing::mint(0);
        let mut client = udp::UdpAssociation::connect(
            time::Instant::now().into_std(),
            address.ip(),
            address.ip(),
            config,
            admit,
        )
        .await
        .unwrap();
        let client_work = async {
            loop {
                assert!(
                    client.progress(false, |_| None).await.unwrap().is_none(),
                    "client ended admission before the server received it"
                );
            }
        };
        let server_work = async {
            let mut listener = Listener::on_shard(tls, 0).unwrap();
            let mut bytes = [0; (UDP_PAYLOAD_BYTES as usize).saturating_add(1)];
            let mut out = Vec::new();
            loop {
                let timer = async {
                    if let Some(at) = listener.deadline() {
                        time::sleep_until(time::Instant::from_std(at)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                let packet = tokio::select! {
                    received = server.recv_from(&mut bytes) => Some(received.unwrap()),
                    () = timer => None,
                };
                let now = time::Instant::now().into_std();
                if let Some((size, source)) = packet {
                    assert!(size <= usize::from(UDP_PAYLOAD_BYTES));
                    if let Some(tx) = listener.receive(
                        now,
                        noq_proto::FourTuple::new(source, Some(address.ip())),
                        None,
                        &bytes[..size],
                        &mut out,
                    ) {
                        server
                            .send_to(&out[..tx.size], tx.destination)
                            .await
                            .unwrap();
                    }
                }
                listener.begin_turn();
                let mut received = None;
                if let Some((_, Some(tx))) = listener.poll_one(now, &mut out, |_, peer| {
                    received = peer.take_admit(now).unwrap();
                }) {
                    server
                        .send_to(&out[..tx.size], tx.destination)
                        .await
                        .unwrap();
                }
                if let Some(body) = received {
                    assert_eq!(Admit::decode(&body).unwrap().token, "token");
                    break;
                }
            }
        };
        tokio::select! {
            () = client_work => unreachable!(),
            result = time::timeout(Duration::from_secs(2), server_work) => result.unwrap(),
        }
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
