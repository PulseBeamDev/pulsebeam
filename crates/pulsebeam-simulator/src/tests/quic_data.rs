use std::{
    net::SocketAddr,
    num::{NonZeroU64, NonZeroUsize},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls require immutable shared configuration"
)]
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use noq_proto::rustls;
use pulsebeam::node::{IceTcpConfig, NodeBuilder, QuicListener};
use pulsebeam_agent_quic::{
    frame::{Frame, Kind, Phase},
    records::{Admit, Renew, RenewResult},
    transport::{
        self, DatagramAdmission,
        client::{
            ClientTransport,
            admission::{Admission, ClientAdmission},
        },
        control::{ControlStream, Read, Write},
        socket::{Egress, Flush},
    },
};
use pulsebeam_core::auth::{
    DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID, KeyRegistry,
    KeyRegistryEntry,
};
use pulsebeam_proto::state::DataV1Delivery;
use pulsebeam_runtime::{
    net::tcp::{TcpListener, TcpStream},
    rand::{self, SeedableRng},
    sync::CancellationToken,
    time::Instant,
};
use rustls::{RootCertStore, pki_types::PrivatePkcs8KeyDer, time_provider::DefaultTimeProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Client {
    transport: ClientTransport,
    socket: pulsebeam_udp::UdpSocket,
    out: Egress,
}

impl Client {
    #[allow(
        clippy::disallowed_types,
        reason = "rustls requires a shared immutable time provider"
    )]
    async fn new(edge: SocketAddr, roots: RootCertStore) -> Result<Self> {
        let socket = transport::socket::bind(SocketAddr::new(edge.ip(), 0))
            .await?
            .activate()?;
        let config = transport::client_config(roots, Arc::new(DefaultTimeProvider))
            .map_err(|e| anyhow::anyhow!("client configuration: {e:?}"))?;
        let transport =
            ClientTransport::connect(Instant::now().into_std(), config, edge, "edge.example")
                .map_err(|e| anyhow::anyhow!("client start: {e:?}"))?;
        Ok(Self {
            transport,
            socket,
            out: Egress::default(),
        })
    }

    async fn progress(&mut self) -> Result<()> {
        let now = Instant::now().into_std();
        let local = self.socket.local_addr();
        let mut more = false;
        self.out.stage(local, |out| {
            let (packet, pending) = self.transport.poll(now, out);
            more = pending;
            packet
        })?;
        match self.out.flush(&mut self.socket)? {
            Flush::PathMtu => anyhow::bail!("fixture path MTU"),
            Flush::Blocked => {
                self.socket.writable().await?;
                return Ok(());
            }
            Flush::Sent => return Ok(()),
            Flush::Empty if more => return Ok(()),
            Flush::Empty => {}
        }
        let deadline = self
            .transport
            .deadline()
            .map(Instant::from_std)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        pulsebeam_runtime::select! {
            ready = self.socket.readable() => ready?,
            () = pulsebeam_runtime::time::sleep_until(deadline) => return Ok(()),
        }
        match self.socket.try_recv_batch() {
            Ok(batch) => {
                for item in batch.iter() {
                    self.transport.receive(Instant::now().into_std(), item);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn admit(&mut self, admit: Admit<'_>) -> Result<(Admission, ControlStream)> {
        let mut admission =
            ClientAdmission::new(&admit, (Instant::now() + Duration::from_secs(5)).into_std())
                .map_err(|error| anyhow::anyhow!("invalid admission: {error:?}"))?;
        loop {
            if let Some(accepted) = admission
                .poll(self.transport.connection(), Instant::now().into_std())
                .map_err(|error| anyhow::anyhow!("admission failed: {error:?}"))?
            {
                return Ok(accepted);
            }
            self.progress().await?;
        }
    }

    async fn send(
        &mut self,
        control: &mut ControlStream,
        kind: Kind,
        body: &[u8],
        phase: Phase,
    ) -> Result<()> {
        control
            .offer(kind, body, phase)
            .map_err(|e| anyhow::anyhow!("offering {kind:?}: {e:?}"))?;
        loop {
            let result = control
                .write(self.transport.connection(), Instant::now().into_std())
                .map_err(|e| anyhow::anyhow!("writing {kind:?}: {e:?}"))?;
            if matches!(result, Write::Admitted { .. }) {
                return Ok(());
            }
            self.progress().await?;
        }
    }

    async fn send_data(&mut self, binding: std::num::NonZeroU16, payload: &[u8]) -> Result<()> {
        self.offer_data(binding, payload)
            .await?
            .map_err(|error| anyhow::anyhow!("DATA datagram admission: {error:?}"))?;
        self.progress().await
    }

    async fn offer_data(
        &mut self,
        binding: std::num::NonZeroU16,
        payload: &[u8],
    ) -> Result<std::result::Result<(), DatagramAdmission>> {
        use pulsebeam_agent_quic::records::{Datagram, DatagramKind};
        loop {
            match self.transport.offer_datagram(Datagram {
                kind: DatagramKind::Data,
                binding,
                payload,
            }) {
                Err(DatagramAdmission::Full | DatagramAdmission::ControlTurn) => {
                    self.progress().await?;
                }
                result => return Ok(result),
            }
        }
    }

    async fn receive_data(&mut self, expected_binding: u16, expected: &[u8]) -> Result<()> {
        self.receive_data_matching(expected_binding, |payload| payload == expected)
            .await
    }

    async fn receive_data_matching(
        &mut self,
        expected_binding: u16,
        matches_payload: impl Fn(&[u8]) -> bool,
    ) -> Result<()> {
        loop {
            if let Some(result) = self.transport.read_datagram(|datagram| {
                datagram.kind == pulsebeam_agent_quic::records::DatagramKind::Data
                    && datagram.binding.get() == expected_binding
                    && matches_payload(datagram.payload)
            }) {
                ensure!(
                    matches!(result, Ok(true)),
                    "unexpected DATA datagram: {result:?}"
                );
                return Ok(());
            }
            self.progress().await?;
        }
    }

    async fn receive(
        &mut self,
        control: &mut ControlStream,
        wanted: Kind,
        phase: Phase,
    ) -> Result<Frame> {
        loop {
            match control
                .read(
                    self.transport.connection(),
                    phase,
                    Instant::now().into_std(),
                )
                .map_err(|e| anyhow::anyhow!("reading {wanted:?}: {e:?}"))?
            {
                Read::Frame(frame) if frame.kind == wanted => return Ok(frame),
                Read::Frame(frame) if frame.kind == Kind::ServerState => {}
                Read::Pending => self.progress().await?,
                _ => anyhow::bail!("unexpected control input waiting for {wanted:?}"),
            }
        }
    }
}

async fn issue(
    address: SocketAddr,
    token: &str,
    continuation: Option<(&str, &[u8])>,
) -> Result<String> {
    let mut socket = TcpStream::connect(address).await?;
    let body = match continuation {
        Some((owner, successor)) => {
            serde_json::json!({"profile": "pulsebeam-rt-v1", "room": "stage", "owner": owner, "successor_receipt": URL_SAFE_NO_PAD.encode(successor)})
        }
        None => serde_json::json!({"profile": "pulsebeam-rt-v1", "room": "stage"}),
    };
    let body = serde_json::to_string(&body)?;
    let request = format!(
        "POST /api/v1/native/admissions HTTP/1.1\r\nHost: edge.example\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    socket.take(8_192).read_to_string(&mut response).await?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("admission HTTP response")?;
    ensure!(
        head.starts_with("HTTP/1.1 200"),
        "admission HTTP status: {}",
        head.lines().next().unwrap_or_default()
    );
    let result: serde_json::Value = serde_json::from_str(body)?;
    ensure!(
        result.get("profile").and_then(serde_json::Value::as_str) == Some("pulsebeam-rt-v1")
            && result
                .pointer("/authority/host")
                .and_then(serde_json::Value::as_str)
                == Some("edge.example")
    );
    Ok(result
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .context("ownership proof")?
        .to_owned())
}

#[test]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_types,
    reason = "fixture owns one node, bounded I/O and immutable TLS providers"
)]
fn dedicated_quic_listener_redeems_renews_and_closes() {
    replay(DataV1Delivery::Unreliable);
}

#[test]
fn reliable_reports_exact_delivery() {
    replay(DataV1Delivery::Reliable);
}

#[test]
fn latest_delivers_on_the_negotiated_binding() {
    replay(DataV1Delivery::Latest);
}

#[test]
fn sctp_and_quic_exchange_unreliable_data() {
    replay_mixed(DataV1Delivery::Unreliable);
}

#[test]
fn sctp_and_quic_exchange_latest_data() {
    replay_mixed(DataV1Delivery::Latest);
}

#[test]
fn sctp_and_quic_exchange_reliable_data() {
    replay_mixed(DataV1Delivery::Reliable);
}

#[allow(
    clippy::expect_used,
    reason = "simulated mixed-carrier scenario must complete"
)]
fn replay_mixed(delivery: DataV1Delivery) {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    let mut sim = turmoil::Builder::new().build();
    sim.client(host, async move {
        witness(Scenario::Data {
            delivery,
            sctp_host: Some(host),
        })
        .await?;
        Ok(())
    });
    super::common::run_sim_or_timeout(&mut sim, Duration::from_secs(10))
        .expect("mixed DATA replay");
}

fn replay(delivery: DataV1Delivery) {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    replay_scenario(
        host,
        Scenario::Data {
            delivery,
            sctp_host: None,
        },
    );
}

#[test]
fn quic_media_opus_uses_negotiated_rtp_binding_and_timing() {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    replay_scenario(host, Scenario::Opus);
}

#[test]
#[allow(
    clippy::expect_used,
    reason = "mixed simulated Opus exchange must complete"
)]
fn quic_media_opus_exchanges_with_webrtc_encoded_endpoints() {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    let mut sim = turmoil::Builder::new().build();
    sim.client(host, async move {
        witness(Scenario::MixedOpus { host }).await?;
        Ok(())
    });
    super::common::run_sim_or_timeout(&mut sim, Duration::from_secs(10))
        .expect("mixed Opus replay");
}

#[test]
#[allow(
    clippy::expect_used,
    reason = "mixed simulated H.264 exchange must complete"
)]
fn quic_media_h264_exchanges_with_webrtc_encoded_endpoints() {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    let mut sim = turmoil::Builder::new().build();
    sim.client(host, async move {
        witness(Scenario::MixedH264 { host }).await?;
        Ok(())
    });
    super::common::run_sim_or_timeout(&mut sim, Duration::from_secs(10))
        .expect("mixed H.264 replay");
}

#[allow(clippy::unwrap_used, reason = "simulated node fixture must complete")]
fn replay_scenario(host: std::net::IpAddr, scenario: Scenario) {
    pulsebeam_runtime::testing::run_local(host, async move {
        pulsebeam_runtime::time::timeout(Duration::from_secs(10), witness(scenario))
            .await
            .unwrap()
            .unwrap();
    });
}

fn quic_data_proposal(delivery: DataV1Delivery) -> Result<Vec<u8>> {
    use pulsebeam_proto::state as wire;
    let spec = wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                track_id: "events".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: (match delivery {
                        DataV1Delivery::Reliable => wire::DataDelivery::Ordered,
                        DataV1Delivery::Unreliable => wire::DataDelivery::Lossy,
                        DataV1Delivery::Latest => wire::DataDelivery::Latest,
                        DataV1Delivery::Unspecified => {
                            anyhow::bail!("DATA delivery mode is required")
                        }
                    }) as i32,
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::QuicDataV1(
                            wire::QuicDataV1Parameters {
                                binding_id: 1,
                                version: 1,
                                incarnation: 1,
                                delivery: delivery as i32,
                            },
                        )),
                    }),
                    capture_clock: None,
                })),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
    };
    wire::codec::encode_quic_client(&wire::ClientStateBatch {
        records: vec![wire::ClientStateRecord {
            owned: 1,
            desired: Some(wire::DesiredState {
                revision: 1,
                value: Some(wire::desired_state::Value::Present(Box::new(spec))),
            }),
            execution: None,
        }],
        ..Default::default()
    })
    .map_err(|error| anyhow::anyhow!("DATA proposal: {error:?}"))
}

async fn prepare_subscription(
    client: &mut Client,
    control: &mut ControlStream,
    delivery: DataV1Delivery,
    source: (&str, &str),
) -> Result<(u64, pulsebeam_proto::state::DataV1ReceiverOffer)> {
    use pulsebeam_proto::state::{
        self as wire, confirmed_state::Value, logical_subscription_state::Execution,
    };
    let send_state = |desired, execution| {
        wire::codec::encode_quic_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: 1,
                desired,
                execution,
            }],
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!("subscriber state: {error:?}"))
    };
    let spec = wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                data_version: Some(1),
                source: Some(wire::LogicalSourceAddress {
                    room: "stage".into(),
                    participant_id: source.0.into(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: source.1.into(),
                }),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
    };
    let desired = wire::DesiredState {
        revision: 1,
        value: Some(wire::desired_state::Value::Present(Box::new(spec))),
    };
    client
        .send(
            control,
            Kind::ClientState,
            &send_state(Some(desired), None)?,
            Phase::Admitted,
        )
        .await?;
    let mut reported = None;
    let mut report_revision = 1;
    loop {
        let frame = client
            .receive(control, Kind::ServerState, Phase::Admitted)
            .await?;
        let batch = wire::codec::decode_quic_server(&frame.body)
            .map_err(|error| anyhow::anyhow!("subscriber response: {error:?}"))?;
        for record in batch.records {
            if !matches!(
                record.key.and_then(|key| key.key),
                Some(wire::resource_key::Key::Owned(1))
            ) {
                continue;
            }
            let Some(confirmed) = record.confirmed else {
                continue;
            };
            ensure!(
                confirmed.rejection.is_none(),
                "subscriber rejection: {:?}",
                confirmed.rejection
            );
            let Some(Value::LogicalSubscription(state)) = confirmed.value else {
                continue;
            };
            match state.execution {
                Some(Execution::Installed(binding)) => {
                    ensure!(
                        matches!(binding.transport, Some(wire::track_binding::Transport::QuicDataV1(parameters)) if parameters.binding_id == 2)
                    );
                    return Ok((1, state.data_v1.context("installed DATA offer")?));
                }
                Some(Execution::Failure(error)) => anyhow::bail!("subscriber execution: {error:?}"),
                None => {}
            }
            if state.data_v1.is_some() && reported != Some(state.source_revision) {
                report_revision += 1;
                let report = wire::ExecutionReport {
                    revision: report_revision,
                    outcome: Some(wire::execution_report::Outcome::LogicalReceiver(
                        wire::LogicalReceiverReport {
                            source_revision: state.source_revision,
                            outcome: Some(wire::logical_receiver_report::Outcome::Prepared(
                                wire::TrackBinding {
                                    transport: Some(wire::track_binding::Transport::QuicDataV1(
                                        wire::QuicDataV1Parameters {
                                            binding_id: 2,
                                            version: 1,
                                            incarnation: 1,
                                            delivery: delivery as i32,
                                        },
                                    )),
                                },
                            )),
                        },
                    )),
                };
                client
                    .send(
                        control,
                        Kind::ClientState,
                        &send_state(None, Some(report))?,
                        Phase::Admitted,
                    )
                    .await?;
                reported = Some(state.source_revision);
            }
        }
    }
}

fn data_record(
    unit_seq: u64,
    index: u16,
    count: u16,
    unit_len: usize,
    payload: &[u8],
) -> Result<Vec<u8>> {
    use pulsebeam_proto::data_record::{Header, Record};
    let record = Record::new(
        Header {
            unit_seq,
            index,
            count,
            unit_len: u32::try_from(unit_len)?,
            capture: None,
        },
        payload,
        None,
    )
    .map_err(|error| anyhow::anyhow!("DATA record: {error:?}"))?;
    let mut bytes = vec![0; record.encoded_len()];
    record
        .write(&mut bytes)
        .map_err(|error| anyhow::anyhow!("DATA encoding: {error:?}"))?;
    Ok(bytes)
}

async fn drive_until<T>(
    client: &mut Client,
    receive: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    pulsebeam_runtime::select! {
        result = receive => result,
        stalled = async { loop { client.progress().await?; } } => {
            let result: Result<()> = stalled;
            result?;
            anyhow::bail!("peer stopped while driving QUIC traffic");
        },
    }
}

async fn receive_control_record<R: pulsebeam_proto::data_control::Record>(
    client: &mut Client,
    control: &mut ControlStream,
    owned: u64,
    binding: R::Binding,
) -> Result<R> {
    use pulsebeam_proto::state::{codec, native_data_control::Endpoint};
    let frame = client
        .receive(control, Kind::DataControl, Phase::Admitted)
        .await?;
    let control = codec::decode_data_control_frame(&frame.body)
        .context("DATA control envelope")?
        .map_err(|error| anyhow::anyhow!("DATA control frame: {error:?}"))?;
    let Some(Endpoint::Subscriber(record) | Endpoint::Publisher(record)) = control.endpoint else {
        anyhow::bail!("DATA control endpoint");
    };
    ensure!(record.owned == owned);
    pulsebeam_proto::data_control::decode(binding, &record.record)
        .map_err(|error| anyhow::anyhow!("DATA control record: {error:?}"))
}

fn subscriber_control_frame(
    owned: u64,
    binding: pulsebeam_proto::data_control::SubscriberBinding,
    message: pulsebeam_proto::data_control::subscriber_control::Record,
) -> Result<Vec<u8>> {
    use pulsebeam_proto::{
        data_control::{self, SubscriberControl},
        state::{NativeDataControl, OwnedDataControl, codec, native_data_control::Endpoint},
    };
    let record = data_control::encode(
        binding,
        &SubscriberControl {
            record: Some(message),
        },
    )
    .map_err(|error| anyhow::anyhow!("subscriber control: {error:?}"))?;
    codec::encode_data_control(&NativeDataControl {
        endpoint: Some(Endpoint::Subscriber(OwnedDataControl { owned, record })),
    })
    .map_err(|error| anyhow::anyhow!("subscriber control envelope: {error:?}"))
}

async fn confirm_reliable_delivery(
    subscriber: &mut Client,
    subscriber_control: &mut ControlStream,
    publisher: &mut Client,
    publisher_control: &mut ControlStream,
    installed: (u64, pulsebeam_proto::state::DataV1ReceiverOffer),
    missing_fragment: &[u8],
) -> Result<()> {
    use pulsebeam_proto::data_control::{
        ControlOutcome, ExpectationOutcome, PublisherBinding, PublisherObservation, RepairDemand,
        RepairStatus, SubscriberBinding, SubscriberObservation, publisher_observation,
        subscriber_control, subscriber_observation,
    };
    let (subscriber_owned, offer) = installed;
    let source = NonZeroU64::new(offer.source).context("offered source")?;
    let branch = NonZeroU64::new(offer.branch).context("offered branch")?;
    let binding = SubscriberBinding { source, branch };
    let subscriber_frame = |message| subscriber_control_frame(subscriber_owned, binding, message);
    let observation: SubscriberObservation =
        receive_control_record(subscriber, subscriber_control, subscriber_owned, binding).await?;
    let Some(subscriber_observation::Record::Expectation(expectation)) = observation.record else {
        anyhow::bail!("expected subscriber expectation")
    };
    ensure!(
        expectation.fragment_count == 2
            && expectation
                .unit
                .as_ref()
                .and_then(|unit| unit.source.as_ref())
                .and_then(|unit| unit.unit_seq)
                == Some(1)
    );
    let bytes = subscriber_frame(subscriber_control::Record::ExpectationOutcome(
        ExpectationOutcome {
            expectation: Some(expectation),
            outcome: ControlOutcome::Confirmed as i32,
        },
    ))?;
    subscriber
        .send(
            subscriber_control,
            Kind::DataControl,
            &bytes,
            Phase::Admitted,
        )
        .await?;
    let request = RepairDemand {
        unit: expectation.unit,
        fragment_index: Some(1),
        request_id: 9,
    };
    let bytes = subscriber_frame(subscriber_control::Record::RequestRepair(request))?;
    subscriber
        .send(
            subscriber_control,
            Kind::DataControl,
            &bytes,
            Phase::Admitted,
        )
        .await?;

    let publisher_binding = PublisherBinding { source };
    let admission: PublisherObservation =
        receive_control_record(publisher, publisher_control, 1, publisher_binding).await?;
    ensure!(
        matches!(admission.record, Some(publisher_observation::Record::Admission(unit)) if unit.unit_seq == Some(1))
    );
    let demand: PublisherObservation = drive_until(
        subscriber,
        receive_control_record(publisher, publisher_control, 1, publisher_binding),
    )
    .await?;
    ensure!(
        matches!(demand.record, Some(publisher_observation::Record::RequestOriginal(demand))
        if demand.unit == expectation.unit.and_then(|unit| unit.source) && demand.fragment_index == Some(1)),
        "unexpected publisher control while awaiting repair: {:?}",
        demand.record
    );
    publisher
        .send_data(std::num::NonZeroU16::MIN, missing_fragment)
        .await?;
    subscriber.receive_data(2, missing_fragment).await?;
    let repair: SubscriberObservation =
        receive_control_record(subscriber, subscriber_control, subscriber_owned, binding).await?;
    ensure!(
        matches!(repair.record, Some(subscriber_observation::Record::Repair(repair))
        if repair.demand == Some(request) && repair.status == RepairStatus::HandedOff as i32)
    );

    let bytes = subscriber_frame(subscriber_control::Record::AssemblyReceipt(expectation))?;
    subscriber
        .send(
            subscriber_control,
            Kind::DataControl,
            &bytes,
            Phase::Admitted,
        )
        .await?;
    let ack: SubscriberObservation =
        receive_control_record(subscriber, subscriber_control, subscriber_owned, binding).await?;
    ensure!(ack.record == Some(subscriber_observation::Record::ReceiptAck(expectation)));
    let delivery: PublisherObservation =
        receive_control_record(publisher, publisher_control, 1, publisher_binding).await?;
    ensure!(
        matches!(delivery.record, Some(publisher_observation::Record::Delivery(result)) if result.expectation == Some(expectation) && result.outcome == ControlOutcome::Confirmed as i32)
    );
    Ok(())
}

async fn open_subscriber(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    credential: &str,
    delivery: DataV1Delivery,
) -> Result<(
    Client,
    ControlStream,
    (u64, pulsebeam_proto::state::DataV1ReceiverOffer),
)> {
    let owner = issue(address, credential, None).await?;
    let mut client = Client::new(edge, roots).await?;
    let (_, mut control) = client
        .admit(Admit {
            attempt: NonZeroU64::MIN,
            room: Some("stage"),
            owner: &owner,
            token: credential,
            successor: None,
        })
        .await?;
    let offer =
        prepare_subscription(&mut client, &mut control, delivery, ("alice", "events")).await?;
    Ok((client, control, offer))
}

async fn reject_reliable_expectation(
    subscriber: &mut Client,
    subscriber_control: &mut ControlStream,
    publisher: &mut Client,
    publisher_control: &mut ControlStream,
    installed: (u64, pulsebeam_proto::state::DataV1ReceiverOffer),
) -> Result<()> {
    use pulsebeam_proto::data_control::{
        ControlOutcome, ExpectationOutcome, PublisherBinding, PublisherObservation,
        SubscriberBinding, SubscriberObservation, publisher_observation, subscriber_control,
        subscriber_observation,
    };
    let (owned, offer) = installed;
    let source = NonZeroU64::new(offer.source).context("offered source")?;
    let branch = NonZeroU64::new(offer.branch).context("offered branch")?;
    let binding = SubscriberBinding { source, branch };
    let observation: SubscriberObservation =
        receive_control_record(subscriber, subscriber_control, owned, binding).await?;
    let Some(subscriber_observation::Record::Expectation(expectation)) = observation.record else {
        anyhow::bail!("expected independent subscriber expectation")
    };
    ensure!(
        expectation.fragment_count == 2
            && expectation
                .unit
                .and_then(|unit| unit.source)
                .and_then(|unit| unit.unit_seq)
                == Some(1)
    );
    let bytes = subscriber_control_frame(
        owned,
        binding,
        subscriber_control::Record::ExpectationOutcome(ExpectationOutcome {
            expectation: Some(expectation),
            outcome: ControlOutcome::Failed as i32,
        }),
    )?;
    subscriber
        .send(
            subscriber_control,
            Kind::DataControl,
            &bytes,
            Phase::Admitted,
        )
        .await?;
    let report: PublisherObservation = drive_until(
        subscriber,
        receive_control_record(publisher, publisher_control, 1, PublisherBinding { source }),
    )
    .await?;
    ensure!(
        matches!(report.record, Some(publisher_observation::Record::Delivery(outcome)) if outcome.expectation == Some(expectation) && outcome.outcome == ControlOutcome::Failed as i32),
        "unexpected independent delivery report: {:?}",
        report.record
    );
    Ok(())
}

fn sim_quic_agent(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    token: &str,
) -> Result<pulsebeam_agent_quic::executor::linux::LinuxAgent> {
    use super::common::client::create_http_client;
    use pulsebeam_agent_core::{AgentConfig, HttpMethod, HttpRequest, HttpResponse};
    use pulsebeam_agent_quic::executor::linux::LinuxAgent;
    async fn placement(request: HttpRequest) -> Option<HttpResponse> {
        if request.method != HttpMethod::Post {
            return None;
        }
        let origin = request.uri.strip_prefix("https://")?;
        let mut builder = http::Request::post(format!("http://{origin}"));
        for header in request.headers {
            builder = builder.header(header.name, header.value);
        }
        let response = create_http_client()
            .execute(builder.body(request.body).ok()?)
            .await
            .ok()?;
        let (parts, body) = response.into_parts();
        Some(HttpResponse {
            status: parts.status.as_u16(),
            headers: Vec::new(),
            body,
        })
    }

    Ok(LinuxAgent::new(
        AgentConfig {
            endpoint: format!("https://{address}"),
            token: token.into(),
            retry: Default::default(),
            log_level: Default::default(),
        },
        edge.ip(),
        roots,
    )
    .map_err(|error| anyhow::anyhow!("QUIC Agent startup: {error:?}"))?
    .with_sim_services("edge.example".into(), edge.ip(), placement))
}

async fn mixed_exchange(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    sctp_host: std::net::IpAddr,
    sctp_token: &str,
    quic_token: &str,
    delivery: DataV1Delivery,
) -> Result<()> {
    use super::common::{client::create_http_client, native::NativeHost};
    use pulsebeam_agent_core::{
        AgentCommand, AgentConfig, DataEvent, Notification, ResourceCommand, ResourceResult,
        TopicMode, TrackSelector, TrackSpec, ffi as wire,
    };
    use pulsebeam_agent_native::{Agent as RuntimeAgent, Config, Host, ffi::Agent};
    let (mode, native_mode) = match delivery {
        DataV1Delivery::Unreliable => (TopicMode::Lossy, wire::DataDelivery::Unreliable),
        DataV1Delivery::Latest => (TopicMode::Latest, wire::DataDelivery::Latest),
        DataV1Delivery::Reliable => (TopicMode::Ordered, wire::DataDelivery::Reliable),
        DataV1Delivery::Unspecified => anyhow::bail!("mixed DATA mode is required"),
    };
    let host = NativeHost::bind(sctp_host)?;
    let (runtime, actor) = RuntimeAgent::managed_with_services(
        Config::new(AgentConfig {
            endpoint: format!("http://{address}"),
            token: sctp_token.to_owned(),
            retry: Default::default(),
            log_level: Default::default(),
        }),
        Host::new(create_http_client()),
        move || host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let sctp = Agent::from_runtime(runtime);
    let receiver = sctp
        .room("stage".into())?
        .participant("alice".into())?
        .data("events".into())?
        .subscribe(Some(wire::DataSubscribeOptions {
            delivery: Some(native_mode),
        }))?;
    let sender = sctp
        .data("return".into())?
        .publish(Some(wire::DataPublishOptions {
            delivery: Some(native_mode),
            ..Default::default()
        }))?;
    sctp.join("stage".into())?;

    let mut quic = sim_quic_agent(address, edge, roots, quic_token)?;
    let outgoing = TrackSpec::Data {
        track_id: "events".into(),
        delivery: mode,
    };
    quic.resource(ResourceCommand::DeclareDataPublication {
        spec: outgoing.clone(),
        capture_clock: None,
    })?;
    let ResourceResult::Owned { owner } = quic.resource(ResourceCommand::ReceiveData {
        spec: TrackSpec::Data {
            track_id: "return".into(),
            delivery: mode,
        },
        source: TrackSelector {
            room: "stage".into(),
            participant_id: "sctp-peer".into(),
            track_id: "return".into(),
        },
    })?
    else {
        anyhow::bail!("QUIC subscriber ownership")
    };
    quic.command(AgentCommand::Join("stage".into()))?;
    let publication = loop {
        let snapshot = quic.snapshot();
        ensure!(
            snapshot.terminal_failure.is_none(),
            "QUIC Agent failed: {:?}",
            snapshot.terminal_failure
        );
        if snapshot
            .topics
            .subscribers
            .iter()
            .any(|status| status.channel.is_some())
            && let Some(publication) = snapshot
                .topics
                .publishers
                .iter()
                .find_map(|status| status.publication_id.clone())
        {
            break publication;
        }
        quic.turn()
            .await
            .map_err(|error| anyhow::anyhow!("QUIC Agent readiness: {error:?}"))?;
    };
    let updates = receiver.updates()?;
    loop {
        let observed = updates
            .next()
            .await?
            .context("SCTP subscription observations")?;
        ensure!(
            observed.terminal_error.is_none(),
            "SCTP receiver failed: {observed:?}"
        );
        if observed.execution == wire::PublicationExecution::Installed {
            ensure!(observed.source_track_id.as_deref() == Some(publication.as_str()));
            break;
        }
    }

    if delivery == DataV1Delivery::Reliable {
        use pulsebeam::sim_metrics::{ShardDataFault, arm_shard_data_trace, routing_counter};
        while routing_counter("fabric_shard_data_ready") < 2 {
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC Agent shard readiness: {error:?}"))?;
        }
        arm_shard_data_trace(ShardDataFault::FirstLiveOffer);
    }
    if delivery == DataV1Delivery::Unreliable {
        use pulsebeam::sim_metrics::{ShardDataFault, arm_shard_data_trace};
        arm_shard_data_trace(ShardDataFault::FirstLiveOffer);
        quic.send_data(outgoing.clone(), b"omitted Unreliable unit".to_vec())?;
        let rejected = quic.send_data(outgoing.clone(), b"not admitted".to_vec());
        ensure!(
            matches!(
                rejected,
                Err(pulsebeam_agent_core::AgentError::InvalidTopic(
                    pulsebeam_agent_core::TopicError::SendQueueFull(ref track)
                )) if track == "events"
            ),
            "second send must observe backpressure before the first is admitted: {rejected:?}"
        );
    }
    let data = b"QUIC to SCTP";
    loop {
        match quic.send_data(outgoing.clone(), data.to_vec()) {
            Err(pulsebeam_agent_core::AgentError::InvalidTopic(
                pulsebeam_agent_core::TopicError::SendQueueFull(_),
            )) if delivery == DataV1Delivery::Unreliable => {
                quic.turn()
                    .await
                    .map_err(|error| anyhow::anyhow!("QUIC Agent send readiness: {error:?}"))?;
            }
            result => {
                result?;
                break;
            }
        }
    }
    let received = pulsebeam_runtime::select! {
        received = receiver.next() => received?,
        error = async { loop { if let Err(error) = quic.turn().await { break error; } } } => {
            anyhow::bail!("QUIC Agent drive: {error:?}");
        },
    };
    ensure!(
        matches!(received, Some(DataEvent::Message { publication_id, participant_id, data: message, .. })
        if publication_id == publication && participant_id == "alice" && message == data)
    );

    if delivery == DataV1Delivery::Reliable {
        use pulsebeam::sim_metrics::{ShardDataFault, arm_shard_data_trace, take_shard_data_trace};
        let trace = take_shard_data_trace().context("QUIC publisher repair trace")?;
        let dropped = trace
            .dropped
            .context("QUIC publisher fragment must be dropped")?;
        ensure!(
            trace.repaired.contains(&dropped),
            "QUIC publisher must repair the dropped fragment: {trace:?}"
        );
        arm_shard_data_trace(ShardDataFault::FirstLiveOffer);
    } else if delivery == DataV1Delivery::Unreliable {
        let trace = pulsebeam::sim_metrics::take_shard_data_trace()
            .context("cross-shard Unreliable loss trace")?;
        ensure!(
            matches!(trace.live.as_slice(), [first, next]
                if trace.dropped == Some(*first) && first.unit_seq < next.unit_seq)
                && trace.repaired.is_empty(),
            "Unreliable must deliver past the gap without repair: {trace:?}"
        );
    }
    let data = b"SCTP to QUIC";
    sender.send(data.to_vec(), None).await?;
    loop {
        if let ResourceResult::Event { event: Some(event) } =
            quic.resource(ResourceCommand::ReadData { owner })?
        {
            ensure!(
                matches!(event, DataEvent::Message { participant_id, data: message, .. }
                if participant_id == "sctp-peer" && message == data)
            );
            break;
        }
        quic.turn()
            .await
            .map_err(|error| anyhow::anyhow!("QUIC Agent receive: {error:?}"))?;
    }
    if delivery == DataV1Delivery::Reliable {
        let trace = pulsebeam::sim_metrics::take_shard_data_trace()
            .context("QUIC receiver repair trace")?;
        let dropped = trace
            .dropped
            .context("QUIC receiver fragment must be dropped")?;
        ensure!(
            trace.repaired.contains(&dropped),
            "QUIC receiver must repair the dropped fragment: {trace:?}"
        );
        'delivery: loop {
            while let Some(notification) = quic.next_notification() {
                if let Notification::ReliableData {
                    track_id,
                    event: wire::ReliablePublishEvent::Delivery { outcome, .. },
                    ..
                } = notification
                {
                    ensure!(
                        track_id == "events" && outcome == wire::ReliableDelivery::Confirmed,
                        "unexpected QUIC delivery outcome: {outcome:?}"
                    );
                    break 'delivery;
                }
            }
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC Agent receipt: {error:?}"))?;
        }
    }
    sctp.close().await?;
    Ok(())
}

async fn prepare_agent_opus_publisher(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
) -> Result<(pulsebeam_agent_quic::executor::linux::LinuxAgent, String)> {
    use pulsebeam_agent_core::{AgentCommand, DesiredState, MediaKind, PublicationIntent};
    use pulsebeam_agent_quic::executor::linux::OpusSendError;
    let mut publisher = sim_quic_agent(address, edge, roots, publisher_token)?;
    publisher.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications: vec![PublicationIntent {
            slot: "voice".into(),
            track_id: "voice".into(),
            kind: MediaKind::Audio,
            active: true,
        }],
        ..Default::default()
    }))?;
    publisher.command(AgentCommand::Join("stage".into()))?;
    ensure!(
        publisher.send_opus("voice", 960, false, &[0xf8, 0xff, 0xfe])
            == Err(OpusSendError::NotReady)
    );
    let publication_id = loop {
        let snapshot = publisher.snapshot();
        ensure!(
            snapshot.terminal_failure.is_none(),
            "Opus Agent failed: {:?}",
            snapshot.terminal_failure
        );
        if let Some(publication) = publisher.opus_publication_id("voice") {
            break publication.to_owned();
        }
        publisher
            .turn()
            .await
            .map_err(|error| anyhow::anyhow!("Opus Agent readiness: {error:?}"))?;
    };
    Ok((publisher, publication_id))
}

async fn opus_exchange(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
    receiver_token: &str,
) -> Result<()> {
    use pulsebeam_agent_core::{AgentCommand, AudioSubscription, DesiredState};
    use pulsebeam_agent_quic::executor::linux::OpusSendError;
    let (mut publisher, publication_id) =
        prepare_agent_opus_publisher(address, edge, roots.clone(), publisher_token).await?;
    let mut receiver = sim_quic_agent(address, edge, roots, receiver_token)?;
    receiver.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        audio: AudioSubscription {
            pinned: vec![publication_id.clone()],
            ..Default::default()
        },
        ..Default::default()
    }))?;
    receiver.command(AgentCommand::Join("stage".into()))?;
    while !receiver.opus_receiver_ready(&publication_id) {
        ensure!(
            receiver.snapshot().terminal_failure.is_none(),
            "Opus receiver failed: {:?}",
            receiver.snapshot().terminal_failure
        );
        receiver
            .turn()
            .await
            .map_err(|error| anyhow::anyhow!("Opus receiver readiness: {error:?}"))?;
    }
    ensure!(matches!(
        publisher.send_opus(
            "voice",
            0,
            false,
            &vec![1; usize::from(transport::UDP_PAYLOAD_BYTES)]
        ),
        Err(OpusSendError::Datagram(DatagramAdmission::Size))
    ));
    let mut previous: Option<(u16, u32)> = None;
    for (timestamp, marker) in [(960, true), (1920, false)] {
        loop {
            match publisher.send_opus("voice", timestamp, marker, &[0xf8, 0xff, 0xfe]) {
                Ok(()) => break,
                Err(OpusSendError::Datagram(
                    DatagramAdmission::Full | DatagramAdmission::ControlTurn,
                )) => {}
                Err(error) => anyhow::bail!("Opus send: {error:?}"),
            }
            publisher
                .turn()
                .await
                .map_err(|error| anyhow::anyhow!("Opus Agent drive: {error:?}"))?;
        }
        let receive = async {
            loop {
                if let Some(packet) = receiver.next_opus() {
                    ensure!(
                        packet.publication_id == publication_id
                            && packet.marker == marker
                            && packet.payload == [0xf8, 0xff, 0xfe],
                        "unexpected negotiated Opus packet: {packet:?}"
                    );
                    return Ok((packet.sequence, packet.timestamp));
                }
                receiver
                    .turn()
                    .await
                    .map_err(|error| anyhow::anyhow!("Opus receive: {error:?}"))?;
            }
        };
        let timing = pulsebeam_runtime::select! {
            result = pulsebeam_runtime::time::timeout(Duration::from_secs(2), receive) =>
                result.map_err(|_| anyhow::anyhow!("Opus packet at timestamp {timestamp} was not received"))??,
            error = async { loop { if let Err(error) = publisher.turn().await { break error; } } } =>
                anyhow::bail!("Opus Agent drive: {error:?}"),
        };
        if let Some((sequence, timestamp)) = previous {
            ensure!(
                timing.0 == sequence.wrapping_add(1) && timing.1 == timestamp.wrapping_add(960)
            );
        }
        previous = Some(timing);
    }
    Ok(())
}

async fn mixed_opus_exchange(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    rtc_host: std::net::IpAddr,
    quic_token: &str,
    rtc_token: &str,
) -> Result<()> {
    use super::common::{client::create_http_client, native::NativeHost};
    use pulsebeam_agent_core::{
        AgentCommand, AgentConfig, AudioSubscription, DesiredState, MediaKind, PublicationIntent,
        ffi::PublicationExecution,
    };
    use pulsebeam_agent_native::{
        Agent as RuntimeAgent, AudioInput, Config, Host, InputFrame, MediaStreamUpdate,
        OutputFrame, ReceiveMode, ffi::Agent,
    };
    use pulsebeam_agent_quic::executor::linux::OpusSendError;
    let (mut quic, publication) =
        prepare_agent_opus_publisher(address, edge, roots, quic_token).await?;
    let host = NativeHost::bind(rtc_host)?;
    let mut config = Config::new(AgentConfig {
        endpoint: format!("http://{address}"),
        token: rtc_token.into(),
        retry: Default::default(),
        log_level: Default::default(),
    });
    config.media.audio_input = AudioInput::EncodedOpus { channels: 2 };
    let (runtime, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), move || {
            host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    let rtc = Agent::from_runtime(runtime);
    let subscriber = rtc
        .room("stage".into())?
        .participant("alice".into())?
        .audio("voice".into())?
        .subscribe()?;
    let updates = subscriber.updates()?;
    let source = rtc.create_audio_input().await?;
    let reverse = rtc.audio("return".into())?.publish(None)?;
    let _pipe = source.pipe_to(&reverse)?;
    let demand = reverse.updates()?;
    rtc.join("stage".into())?;
    loop {
        let state = updates
            .next()
            .await?
            .context("WebRTC Opus subscription updates")?;
        ensure!(
            state.error.is_none() && state.execution_error.is_none(),
            "WebRTC Opus subscription: {state:?}"
        );
        if state.execution == PublicationExecution::Installed {
            ensure!(state.source_track_id.as_deref() == Some(publication.as_str()));
            break;
        }
    }
    let encoded = pulsebeam_runtime::time::timeout(
        Duration::from_secs(2),
        subscriber.observe(ReceiveMode::Encoded),
    )
    .await
    .map_err(|_| anyhow::anyhow!("WebRTC Opus observer did not attach after installation"))??;
    let opus = super::native_runtime::stock_opus_fixture()
        .first()
        .context("stock Opus packet")?;
    loop {
        match quic.send_opus("voice", opus.timestamp, true, &opus.payload) {
            Ok(()) => break,
            Err(OpusSendError::Datagram(
                DatagramAdmission::Full | DatagramAdmission::ControlTurn,
            )) => {}
            Err(error) => anyhow::bail!("mixed Opus send: {error:?}"),
        }
        quic.turn()
            .await
            .map_err(|error| anyhow::anyhow!("mixed Opus Agent drive: {error:?}"))?;
    }
    let receive = async {
        loop {
            match encoded.next().await {
                MediaStreamUpdate::Track { .. } => {}
                MediaStreamUpdate::Frame { frame } => {
                    ensure!(
                        frame.track.generation == encoded.generation()
                            && frame.track.receiver == encoded.receiver()
                    );
                    ensure!(
                        matches!(frame.frame, OutputFrame::Opus { payload, .. } if payload == opus.payload)
                    );
                    ensure!(
                        pulsebeam::sim_metrics::routing_counter("fabric_rtp_opus_shard_enqueued")
                            > 0,
                        "mixed Opus must cross shard owners"
                    );
                    return Ok(());
                }
                update => anyhow::bail!("unexpected WebRTC Opus observation: {update:?}"),
            }
        }
    };
    pulsebeam_runtime::select! {
        result = pulsebeam_runtime::time::timeout(Duration::from_secs(2), receive) =>
            result.map_err(|_| anyhow::anyhow!("WebRTC encoded receiver did not observe QUIC Opus"))??,
        error = async { loop { if let Err(error) = quic.turn().await { break error; } } } =>
            anyhow::bail!("mixed Opus Agent drive: {error:?}"),
    }

    let reverse_id = pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = demand
                .next()
                .await?
                .context("WebRTC Opus publication updates")?;
            ensure!(
                state.admission.error.is_none() && state.admission.execution_error.is_none(),
                "WebRTC Opus publication: {state:?}"
            );
            if state.admission.execution == PublicationExecution::Installed {
                return state
                    .admission
                    .publication_id
                    .context("installed WebRTC Opus publication ID");
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("WebRTC Opus publication did not install"))??;
    reverse.ready().await?;
    let revision = quic
        .snapshot()
        .desired_revision
        .checked_add(1)
        .context("QUIC desired revision exhausted")?;
    quic.command(AgentCommand::ReplaceDesired(DesiredState {
        revision,
        connected: true,
        publications: vec![PublicationIntent {
            slot: "voice".into(),
            track_id: "voice".into(),
            kind: MediaKind::Audio,
            active: true,
        }],
        audio: AudioSubscription {
            pinned: vec![reverse_id.clone()],
            ..Default::default()
        },
        ..Default::default()
    }))?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        while !quic.opus_receiver_ready(&reverse_id) {
            ensure!(
                quic.snapshot().terminal_failure.is_none(),
                "QUIC Opus receiver: {:?}",
                quic.snapshot().terminal_failure
            );
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC Opus receiver readiness: {error:?}"))?;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("QUIC receiver did not install for WebRTC Opus"))??;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        while !reverse.state().forwarding_requested {
            demand
                .next()
                .await?
                .context("WebRTC Opus publication demand")?;
        }
        anyhow::Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("WebRTC Opus publisher did not observe forwarding demand"))??;
    source
        .submit(InputFrame::Opus {
            frame: opus.clone(),
        })
        .await?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(frame) = quic.next_opus() {
                ensure!(
                    frame.publication_id == reverse_id && frame.payload == opus.payload,
                    "unexpected WebRTC Opus frame: {frame:?}"
                );
                return Ok(());
            }
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC Opus receiver drive: {error:?}"))?;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("QUIC receiver did not observe WebRTC Opus"))??;
    rtc.close().await?;
    Ok(())
}

fn pinned_video(publication_id: String) -> pulsebeam_agent_core::VideoSubscription {
    pulsebeam_agent_core::VideoSubscription {
        slot: 0,
        publication_id,
        selector: None,
        height: 0,
        min_height: 0,
        min_fps: 0,
        priority: 1,
    }
}

async fn mixed_h264_exchange(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    rtc_host: std::net::IpAddr,
    quic_token: &str,
    rtc_token: &str,
) -> Result<()> {
    use super::common::{client::create_http_client, native::NativeHost};
    use pulsebeam_agent_core::{
        AgentCommand, AgentConfig, DesiredState, MediaKind, PublicationIntent,
        ffi::{PublicationExecution, VideoObservation},
    };
    use pulsebeam_agent_native::{
        Agent as RuntimeAgent, Config, H264Frame, Host, InputFrame, MediaStreamUpdate, OutputFrame,
        ReceiveMode, VideoInput, ffi::Agent,
    };
    let mut quic = sim_quic_agent(address, edge, roots, quic_token)?;
    let camera = PublicationIntent {
        slot: "camera".into(),
        track_id: "camera".into(),
        kind: MediaKind::Video,
        active: true,
    };
    quic.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications: vec![camera.clone()],
        ..Default::default()
    }))?;
    quic.command(AgentCommand::Join("stage".into()))?;
    let publication = loop {
        ensure!(
            quic.snapshot().terminal_failure.is_none(),
            "QUIC video publication: {:?}",
            quic.snapshot().terminal_failure
        );
        if let Some(id) = quic.h264_publication_id("camera") {
            break id.to_owned();
        }
        quic.turn()
            .await
            .map_err(|error| anyhow::anyhow!("QUIC video publication: {error:?}"))?;
    };
    let mut units = {
        use pulsebeam_rtc_simulation::{
            NativeH264FixtureEncoding, SimulationWorld, stock_h264_fixture_for_encoding,
        };
        stock_h264_fixture_for_encoding(
            &SimulationWorld::acquire(),
            NativeH264FixtureEncoding {
                width: 1280,
                height: 720,
                bitrate_kbps: 1_000,
            },
            1,
            3,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .into_iter()
    };
    let host = NativeHost::bind(rtc_host)?;
    let unit = units
        .find(|unit| unit.keyframe)
        .context("stock H.264 keyframe")?;
    let delta = units
        .find(|unit| !unit.keyframe)
        .context("stock H.264 delta frame")?;
    let mut config = Config::new(AgentConfig {
        endpoint: format!("http://{address}"),
        token: rtc_token.into(),
        retry: Default::default(),
        log_level: Default::default(),
    });
    config.media.video_input = VideoInput::EncodedH264;
    let (runtime, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), move || {
            host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    let rtc = Agent::from_runtime(runtime);
    let subscriber = rtc
        .room("stage".into())?
        .participant("alice".into())?
        .video("camera".into())?
        .subscribe(Some(VideoObservation {
            visible: true,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 1,
        }))?;
    let updates = subscriber.updates()?;
    let source = rtc.create_video_input(Default::default()).await?;
    let reverse = rtc.video("return".into())?.publish(Default::default())?;
    let _pipe = source.pipe_to(&reverse)?;
    let demand = reverse.updates()?;
    let feedback = source.feedback();
    rtc.join("stage".into())?;
    loop {
        let state = updates
            .next()
            .await?
            .context("WebRTC video subscription updates")?;
        ensure!(
            state.error.is_none() && state.execution_error.is_none(),
            "WebRTC video subscription: {state:?}"
        );
        if state.execution == PublicationExecution::Installed {
            ensure!(state.source_track_id.as_deref() == Some(publication.as_str()));
            break;
        }
    }
    let encoded = subscriber.observe(ReceiveMode::Encoded).await?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        while !quic.take_h264_keyframe_request("camera") {
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC video refresh: {error:?}"))?;
        }
        anyhow::Ok(())
    })
    .await
    .context("QUIC publisher did not receive receiver keyframe request")??;
    ensure!(
        unit.data.len() > usize::from(transport::UDP_PAYLOAD_BYTES),
        "fixture must require multiple QUIC datagrams"
    );
    let mut timestamp_offset = None;
    for sample in [&unit, &delta] {
        quic.send_h264("camera", sample.rtp_timestamp, sample.data.clone())
            .map_err(|error| anyhow::anyhow!("QUIC video send: {error:?}"))?;
        ensure!(
            quic.send_h264("camera", sample.rtp_timestamp, Vec::new())
                == Err(pulsebeam_agent_quic::executor::linux::H264SendError::Busy)
        );
        let receive = async {
            loop {
                match encoded.next().await {
                    MediaStreamUpdate::Track { .. } => {}
                    MediaStreamUpdate::Frame { frame } => {
                        let OutputFrame::H264 {
                            keyframe,
                            payload,
                            timestamp,
                        } = frame.frame
                        else {
                            anyhow::bail!("expected H.264 from WebRTC observer");
                        };
                        ensure!(
                            keyframe == sample.keyframe && payload == sample.data,
                            "unexpected WebRTC encoded video access unit"
                        );
                        return Ok(timestamp);
                    }
                    update => anyhow::bail!("unexpected WebRTC H.264 observation: {update:?}"),
                }
            }
        };
        let timestamp = pulsebeam_runtime::select! {
            result = pulsebeam_runtime::time::timeout(Duration::from_secs(2), receive) => result.context("WebRTC did not receive QUIC H.264")??,
            error = async { loop { if let Err(error) = quic.turn().await { break error; } } } => anyhow::bail!("QUIC video drive: {error:?}"),
        };
        let offset = timestamp.wrapping_sub(sample.rtp_timestamp);
        ensure!(
            *timestamp_offset.get_or_insert(offset) == offset,
            "H.264 RTP timestamp delta changed"
        );
    }
    ensure!(
        pulsebeam::sim_metrics::routing_counter("fabric_rtp_h264_shard_enqueued") > 0,
        "mixed H.264 must cross shard owners"
    );
    reverse.ready().await?;
    let reverse_id = reverse
        .state()
        .admission
        .publication_id
        .context("WebRTC video publication ID")?;
    let revision = quic
        .snapshot()
        .desired_revision
        .checked_add(1)
        .context("QUIC desired revision exhausted")?;
    quic.command(AgentCommand::ReplaceDesired(DesiredState {
        revision,
        connected: true,
        publications: vec![camera],
        video: vec![pinned_video(reverse_id.clone())],
        ..Default::default()
    }))?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        while !quic.h264_receiver_ready(&reverse_id) {
            ensure!(
                quic.snapshot().terminal_failure.is_none(),
                "QUIC video subscription: {:?}",
                quic.snapshot().terminal_failure
            );
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC video subscription: {error:?}"))?;
        }
        anyhow::Ok(())
    })
    .await
    .context("QUIC video subscription did not install")??;
    while !reverse.state().forwarding_requested {
        demand
            .next()
            .await?
            .context("WebRTC video publication demand")?;
    }
    source
        .submit(InputFrame::H264 {
            frame: H264Frame {
                timestamp_us: 0,
                width: unit.width.try_into()?,
                height: unit.height.try_into()?,
                keyframe: true,
                temporal_index: None,
                base_layer_sync: false,
                payload: unit.data.clone(),
            },
        })
        .await?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(frame) = quic.next_h264() {
                ensure!(
                    frame.publication_id == reverse_id && frame.access_unit == unit.data,
                    "unexpected QUIC encoded video access unit"
                );
                return Ok(());
            }
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC video receiver: {error:?}"))?;
        }
    })
    .await
    .context("QUIC did not receive WebRTC H.264")??;
    pulsebeam_runtime::time::sleep(Duration::from_millis(600)).await;
    loop {
        match quic.request_h264_keyframe(&reverse_id) {
            Ok(()) => break,
            Err(DatagramAdmission::Full | DatagramAdmission::ControlTurn) => {}
            Err(error) => anyhow::bail!("QUIC keyframe request: {error:?}"),
        }
        quic.turn()
            .await
            .map_err(|error| anyhow::anyhow!("QUIC keyframe request: {error:?}"))?;
    }
    let wait_feedback = async {
        // The encoded producer reports a keyframe request when libwebrtc
        // rejects a delta capture in response to the PLI.
        pulsebeam_runtime::time::sleep(Duration::from_millis(100)).await;
        source
            .submit(InputFrame::H264 {
                frame: H264Frame {
                    timestamp_us: source.capture_time_us().await?.max(1),
                    width: delta.width.try_into()?,
                    height: delta.height.try_into()?,
                    keyframe: false,
                    temporal_index: None,
                    base_layer_sync: false,
                    payload: delta.data,
                },
            })
            .await?;
        use pulsebeam_agent_native::ffi::FeedbackUpdate;
        loop {
            match feedback.next().await {
                FeedbackUpdate::Feedback { feedback } if feedback.keyframe_requested => {
                    return Ok(());
                }
                FeedbackUpdate::Feedback { .. } => {}
                update => anyhow::bail!("unexpected WebRTC H.264 feedback: {update:?}"),
            }
        }
    };
    pulsebeam_runtime::select! {
        result = pulsebeam_runtime::time::timeout(Duration::from_secs(2), wait_feedback) => result.context("WebRTC publisher did not receive QUIC PLI")??,
        error = async { loop { if let Err(error) = quic.turn().await { break error; } } } => anyhow::bail!("QUIC keyframe request drive: {error:?}"),
    }
    let revision = quic
        .snapshot()
        .desired_revision
        .checked_add(1)
        .context("QUIC desired revision exhausted")?;
    quic.command(AgentCommand::ReplaceDesired(DesiredState {
        revision,
        connected: true,
        ..Default::default()
    }))?;
    pulsebeam_runtime::time::timeout(Duration::from_secs(2), async {
        while quic.h264_receiver_ready(&reverse_id) || quic.h264_publication_id("camera").is_some()
        {
            quic.turn()
                .await
                .map_err(|error| anyhow::anyhow!("QUIC video retirement: {error:?}"))?;
        }
        anyhow::Ok(())
    })
    .await
    .context("QUIC video bindings did not retire")??;
    ensure!(quic.request_h264_keyframe(&reverse_id) == Err(DatagramAdmission::Unsupported));
    ensure!(!quic.take_h264_keyframe_request("camera"));
    ensure!(quic.next_h264().is_none());
    ensure!(
        quic.send_h264("camera", unit.rtp_timestamp, unit.data)
            == Err(pulsebeam_agent_quic::executor::linux::H264SendError::NotReady)
    );
    rtc.close().await?;
    Ok(())
}

enum Scenario {
    Data {
        delivery: DataV1Delivery,
        sctp_host: Option<std::net::IpAddr>,
    },
    Opus,
    MixedOpus {
        host: std::net::IpAddr,
    },
    MixedH264 {
        host: std::net::IpAddr,
    },
}

#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls require shared immutable configuration and clocks"
)]
async fn witness(scenario: Scenario) -> Result<()> {
    let host = std::net::Ipv4Addr::LOCALHOST.into();
    let edge = SocketAddr::new(host, 8443);
    let http = TcpListener::bind("127.0.0.1:0".parse()?).await?;
    let address = http.local_addr()?;
    let identity = rcgen::generate_simple_self_signed(vec!["edge.example".to_owned()])?;
    let mut roots = RootCertStore::empty();
    roots.add(identity.cert.der().clone())?;
    let tls = transport::server_config(
        vec![identity.cert.der().clone()],
        PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
        Arc::new(DefaultTimeProvider),
        Arc::new(noq_proto::StdSystemTime),
        NonZeroUsize::new(64).context("incoming limit")?,
    )
    .map_err(|e| anyhow::anyhow!("server TLS configuration: {e:?}"))?;
    let keys = KeyRegistry::new(
        *DEVELOPMENT_PROJECT_ID,
        vec![KeyRegistryEntry {
            key_id: *DEVELOPMENT_API_KEY_ID,
            public_key: *DEVELOPMENT_API_PUBLIC_KEY,
        }],
    )?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let grants = pulsebeam_core::auth::grants::GrantSet::new(vec![
        pulsebeam_core::auth::grants::Grant::new(
            vec!["stage".to_owned()],
            vec![
                pulsebeam_core::auth::grants::Action::Join,
                pulsebeam_core::auth::grants::Action::Publish,
                pulsebeam_core::auth::grants::Action::Discover,
                pulsebeam_core::auth::grants::Action::Subscribe,
            ],
            None,
            None,
            None,
        )?,
    ])?;
    let key = pulsebeam_server::insecure_dev_api_key()?;
    let sign = |participant, lifetime| {
        pulsebeam_server::sign_participant_token(
            &key,
            &grants,
            participant,
            now.saturating_add(lifetime),
        )
    };
    let credential = sign("alice", 60)?.token;
    let renewal = sign("alice", 120)?.token;
    let subscriber_token = sign("bob", 60)?.token;
    let (notify, mut ready) = pulsebeam_runtime::sync::mpsc::channel(1);
    let shutdown = CancellationToken::new();
    let builder = NodeBuilder::new()
        .with_current_runtime()
        .local_addr(SocketAddr::new(host, 0))
        .advertise_bound_udp()
        .ice_tcp(IceTcpConfig::Disabled)
        .rng(rand::Rng::seed_from_u64(19))
        .with_http_api_listener(http)
        .with_key_registry(keys)
        .with_sim_startup_notification(notify);
    let shards = if matches!(
        scenario,
        Scenario::Data {
            sctp_host: Some(_),
            ..
        } | Scenario::MixedOpus { .. }
            | Scenario::MixedH264 { .. }
    ) {
        2
    } else {
        1
    };
    let builder = builder
        .workers(shards)
        .room_partition_size(1)
        .round_robin_rooms()
        .quic_listeners(
            (0..shards)
                .map(|index| {
                    let port = if index == 0 { edge.port() } else { 8444 };
                    QuicListener {
                        bind: SocketAddr::new(host, port),
                        host: "edge.example".to_owned(),
                        port,
                        tls: tls.clone(),
                    }
                })
                .collect(),
        );
    let mut node = pulsebeam_runtime::task::spawn_local(builder.run(shutdown.clone()));
    pulsebeam_runtime::select! {
        notification = ready.recv() => ensure!(notification.is_some(), "node stopped before startup"),
        result = &mut node => anyhow::bail!("node stopped before startup: {result:?}"),
    }
    let result = async {
        if let Scenario::Data {
            sctp_host: Some(sctp_host),
            delivery,
        } = scenario
        {
            let sctp_token = sign("sctp-peer", 60)?.token;
            return mixed_exchange(
                address,
                edge,
                roots,
                sctp_host,
                &sctp_token,
                &credential,
                delivery,
            )
            .await;
        }
        if let Scenario::MixedOpus { host } = scenario {
            return mixed_opus_exchange(address, edge, roots, host, &credential, &subscriber_token)
                .await;
        }
        if let Scenario::MixedH264 { host } = scenario {
            return mixed_h264_exchange(address, edge, roots, host, &credential, &subscriber_token)
                .await;
        }
        let Scenario::Data { delivery, .. } = scenario else {
            return opus_exchange(address, edge, roots, &credential, &subscriber_token).await;
        };
        let owner = issue(address, &credential, None).await?;
        let mut client = Client::new(edge, roots.clone()).await?;
        let (accepted, mut control) = client
            .admit(Admit {
                attempt: NonZeroU64::MIN,
                room: Some("stage"),
                owner: &owner,
                token: &credential,
                successor: None,
            })
            .await?;
        ensure!(accepted.room.as_deref() == Some("stage") && accepted.participant == "alice");
        use pulsebeam_proto::{prelude::Message, state::RtpCapabilities};
        let caps = RtpCapabilities::decode(accepted.capabilities.as_slice())?;
        ensure!(
            caps.codecs
                .iter()
                .filter_map(|codec| codec
                    .format
                    .as_ref()
                    .map(|format| format.mime_type.as_str()))
                .eq(["audio/opus", "video/h264"])
        );
        ensure!(
            caps.codecs
                .iter()
                .all(|codec| !codec.rtx_supported && codec.feedback.is_empty())
        );
        ensure!(caps.header_extensions.is_empty() && caps.rtcp_modes.is_empty());
        let predecessor = accepted.connection;
        let successor = loop {
            let state = client
                .receive(&mut control, Kind::ServerState, Phase::Admitted)
                .await?;
            let batch = pulsebeam_proto::state::codec::decode_quic_server(&state.body)
                .map_err(|e| anyhow::anyhow!("state: {e:?}"))?;
            if let Some(receipt) =
                batch
                    .records
                    .into_iter()
                    .find_map(|record| match record.confirmed?.value? {
                        pulsebeam_proto::state::confirmed_state::Value::Session(state) => {
                            state.successor_receipt
                        }
                        _ => None,
                    })
            {
                break receipt.proof;
            }
        };
        // A well-formed envelope for an uninstalled resource must not terminate the association.
        use pulsebeam_proto::state::{
            NativeDataControl, OwnedDataControl, codec, native_data_control::Endpoint,
        };
        let unknown = codec::encode_data_control(&NativeDataControl {
            endpoint: Some(Endpoint::Subscriber(OwnedDataControl {
                owned: 1,
                record: vec![0],
            })),
        })
        .map_err(|error| anyhow::anyhow!("DATA control envelope: {error:?}"))?;
        client
            .send(&mut control, Kind::DataControl, &unknown, Phase::Admitted)
            .await?;
        let body = quic_data_proposal(delivery)?;
        client
            .send(&mut control, Kind::ClientState, &body, Phase::Admitted)
            .await?;
        loop {
            let frame = client
                .receive(&mut control, Kind::ServerState, Phase::Admitted)
                .await?;
            let batch = codec::decode_quic_server(&frame.body)
                .map_err(|error| anyhow::anyhow!("DATA state response: {error:?}"))?;
            use pulsebeam_proto::state::{confirmed_state::Value, resource_key::Key};
            if let Some(confirmed) =
                batch
                    .records
                    .into_iter()
                    .find_map(|record| match record.key?.key? {
                        Key::Owned(1) => record.confirmed,
                        _ => None,
                    })
            {
                ensure!(
                    confirmed.rejection.is_none(),
                    "unexpected DATA rejection: {:?}",
                    confirmed.rejection
                );
                if let Some(Value::Owned(live)) = confirmed.value {
                    ensure!(live.data_v1_source.is_some_and(|source| source != 0));
                    break;
                }
            }
        }
        let (mut subscriber, mut subscription_control, offer) =
            open_subscriber(address, edge, roots.clone(), &subscriber_token, delivery).await?;
        let independent = if delivery == DataV1Delivery::Reliable {
            let independent_token = sign("carol", 60)?.token;
            let second =
                open_subscriber(address, edge, roots.clone(), &independent_token, delivery).await?;
            ensure!(second.2.1.source == offer.1.source && second.2.1.branch != offer.1.branch);
            Some(second)
        } else {
            None
        };
        const FRAGMENT_BYTES: usize = pulsebeam_proto::data_record::FRAGMENT_BYTES;
        let bytes = if delivery == DataV1Delivery::Reliable {
            let oversized = [0; transport::UDP_PAYLOAD_BYTES as usize];
            ensure!(
                client
                    .offer_data(std::num::NonZeroU16::MIN, &oversized)
                    .await?
                    == Err(DatagramAdmission::Size),
                "a DATA payload at the UDP MTU leaves no room for QUIC and binding headers"
            );
            data_record(1, 0, 2, FRAGMENT_BYTES + 1, &[b'a'; FRAGMENT_BYTES])?
        } else {
            let payload = b"first DATA unit";
            data_record(1, 0, 1, payload.len(), payload)?
        };
        client.send_data(std::num::NonZeroU16::MIN, &bytes).await?;
        subscriber.receive_data(2, &bytes).await?;
        if let Some((mut independent, mut independent_control, independent_offer)) = independent {
            independent.receive_data(2, &bytes).await?;
            confirm_reliable_delivery(
                &mut subscriber,
                &mut subscription_control,
                &mut client,
                &mut control,
                offer,
                &data_record(1, 1, 2, FRAGMENT_BYTES + 1, b"b")?,
            )
            .await?;
            reject_reliable_expectation(
                &mut independent,
                &mut independent_control,
                &mut client,
                &mut control,
                independent_offer,
            )
            .await?;
        } else {
            let payload = b"DATA after skipped unit 2";
            let third = data_record(3, 0, 1, payload.len(), payload)?;
            client.send_data(std::num::NonZeroU16::MIN, &third).await?;
            subscriber.receive_data(2, &third).await?;
        }
        use pulsebeam_proto::state as wire;
        let retire = codec::encode_quic_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: 1,
                desired: Some(wire::DesiredState {
                    revision: 2,
                    value: Some(wire::desired_state::Value::Absent(true)),
                }),
                execution: None,
            }],
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!("DATA retirement: {error:?}"))?;
        client
            .send(&mut control, Kind::ClientState, &retire, Phase::Admitted)
            .await?;
        loop {
            let frame = client
                .receive(&mut control, Kind::ServerState, Phase::Admitted)
                .await?;
            let batch = codec::decode_quic_server(&frame.body)
                .map_err(|error| anyhow::anyhow!("DATA retirement state: {error:?}"))?;
            if batch.records.iter().any(|record| {
                matches!(
                    record.key.as_ref().and_then(|key| key.key.as_ref()),
                    Some(wire::resource_key::Key::Owned(1))
                ) && record.confirmed.as_ref().is_some_and(|state| {
                    matches!(state.value, Some(wire::confirmed_state::Value::Absent(_)))
                        && state.evaluated_revision == 2
                })
            }) {
                break;
            }
        }
        let payload = b"fresh DATA on retired binding";
        let retired_packet = data_record(6, 0, 1, payload.len(), payload)?;
        client
            .send_data(std::num::NonZeroU16::MIN, &retired_packet)
            .await?;
        let body = Renew {
            revision: NonZeroU64::MIN,
            token: &renewal,
        }
        .encode()
        .map_err(|e| anyhow::anyhow!("encoding renewal: {e:?}"))?;
        client
            .send(&mut control, Kind::Renew, &body, Phase::Admitted)
            .await?;
        let result = client
            .receive(&mut control, Kind::RenewResult, Phase::Admitted)
            .await?;
        let result = RenewResult::decode(&result.body)
            .map_err(|e| anyhow::anyhow!("renewal response: {e:?}"))?;
        let lease = result.accepted.context("renewal accepted")?;
        ensure!(
            result.request == NonZeroU64::MIN
                && lease.revision.get() == 2
                && lease.expires_unix_seconds.get() == now.saturating_add(120)
        );
        if let Ok(received) = pulsebeam_runtime::time::timeout(
            Duration::from_secs(1),
            subscriber.receive_data(2, &retired_packet),
        )
        .await
        {
            received?;
            anyhow::bail!("subscriber received the fresh datagram sent after publisher retirement");
        }
        let issued = issue(address, &renewal, Some((&owner, &successor))).await?;
        ensure!(
            issued == owner,
            "continuation must not mint a fresh owner proof"
        );
        let mut next = Client::new(edge, roots).await?;
        let (accepted, mut control) = next
            .admit(Admit {
                attempt: NonZeroU64::new(2).context("valid attempt")?,
                room: Some("stage"),
                owner: &owner,
                token: &renewal,
                successor: Some(&successor),
            })
            .await?;
        ensure!(accepted.connection != predecessor && accepted.room.as_deref() == Some("stage"));
        while !client.transport.connection().is_closed() {
            client.progress().await?;
        }
        next.send(&mut control, Kind::Close, &[], Phase::Admitted)
            .await?;
        while !next.transport.connection().is_closed() {
            next.progress().await?;
        }
        Ok(())
    }
    .await;
    shutdown.cancel();
    node.await??;
    result
}
