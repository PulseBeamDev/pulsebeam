use std::{net::IpAddr, rc::Rc, time::Duration};

use anyhow::Context;
use pulsebeam_proto::{
    data_endpoint::DATA_V1,
    data_record::{Header, Record},
    state::{self as wire, codec, validation},
};
use pulsebeam_rtc_simulation::{
    DataChannel, DataChannelConfiguration, DataChannelEvent, DataChannelMessage,
    DataChannelMessageKind, DataChannelSendResult, DataChannelState, IceGatheringState,
    OperationId, PeerConfiguration, PeerConnection, PeerConnectionEvent, PeerConnectionFactory,
    SessionDescription, SessionDescriptionType,
};

use super::common::{
    DEFAULT_SIM_SHARDS,
    client::create_http_client,
    native::{NativeHost, RuntimeLease},
    reserve_subnet, run_sim_or_timeout, sfu_node_builder, subnet_ip,
};

mod audio;
mod av1;

struct WirePeer {
    media: Option<pulsebeam_agent_core::client_sdp::Context>,
    peer: PeerConnection,
    control: DataChannel,
    payload: [DataChannel; 3],
    gathered: bool,
    initial_complete: bool,
    data_v1: bool,
    _factory: PeerConnectionFactory,
    _lease: Rc<RuntimeLease>,
    _host: NativeHost,
}

const DATA_ROOM: &str = "native-state-wire";

enum Av1Bootstrap<'a> {
    External(&'a pulsebeam_rtc_simulation::EncodedVideoInput),
    Stock,
}

impl WirePeer {
    async fn connect(
        ip: IpAddr,
        server: IpAddr,
        subject: &str,
        audio: bool,
    ) -> anyhow::Result<(Self, String)> {
        Self::connect_on(NativeHost::bind(ip)?, server, subject, audio).await
    }

    async fn connect_on(
        host: NativeHost,
        server: IpAddr,
        subject: &str,
        audio: bool,
    ) -> anyhow::Result<(Self, String)> {
        Self::connect_on_room(host, server, subject, audio, DATA_ROOM).await
    }

    async fn connect_on_room(
        host: NativeHost,
        server: IpAddr,
        subject: &str,
        audio: bool,
        room: &str,
    ) -> anyhow::Result<(Self, String)> {
        Self::connect_with_peer(host, server, subject, room, audio, None, |_, _| Ok(())).await
    }

    async fn connect_with_peer(
        host: NativeHost,
        server: IpAddr,
        subject: &str,
        room: &str,
        media: bool,
        av1: Option<Av1Bootstrap<'_>>,
        prepare: impl FnOnce(&PeerConnectionFactory, &PeerConnection) -> anyhow::Result<()>,
    ) -> anyhow::Result<(Self, String)> {
        let (_runtime, lease) = host.runtime()?;
        let factory = host.raw_peer_factory(match &av1 {
            Some(Av1Bootstrap::External(input)) => Some(input.encoder_factory()),
            Some(Av1Bootstrap::Stock) => {
                Some(pulsebeam_rtc_simulation::VideoEncoderFactoryHandle::builtin()?)
            }
            None => None,
        })?;
        let mut configuration: pulsebeam_rtc_simulation::RtcConfiguration = PeerConfiguration {
            always_negotiate_data_channels: true,
            ..Default::default()
        }
        .into();
        if media {
            configuration.options.crypto_options =
                pulsebeam_rtc_simulation::CryptoOptions::prefer_gcm();
        }
        let mut dependencies = pulsebeam_rtc_simulation::PeerConnectionDependencies::default();
        if let Some(av1) = av1 {
            let trials = match av1 {
                Av1Bootstrap::External(_) => {
                    "WebRTC-RFC8888CongestionControlFeedback/Enabled,offer:true/"
                }
                Av1Bootstrap::Stock => concat!(
                    "WebRTC-RFC8888CongestionControlFeedback/Enabled,offer:true/",
                    "WebRTC-DependencyDescriptorAdvertised/Enabled/",
                ),
            };
            dependencies =
                dependencies.field_trials(pulsebeam_rtc_simulation::FieldTrials::parse(trials)?);
        }
        let peer = factory.create_peer_connection_with_dependencies(configuration, dependencies)?;
        let control = peer.create_data_channel(
            "v1/sys/signaling",
            DataChannelConfiguration {
                negotiated: true,
                id: Some(0),
                ..Default::default()
            },
        )?;
        let channel = |id| {
            peer.create_data_channel(
                "",
                DataChannelConfiguration {
                    negotiated: true,
                    id: Some(id),
                    ordered: false,
                    max_retransmits: Some(0),
                    protocol: b"pulsebeam.data.v1".to_vec(),
                    ..Default::default()
                },
            )
        };
        let payload = [channel(1)?, channel(2)?, channel(3)?];
        let mut result = Self {
            media: None,
            peer,
            control,
            payload,
            gathered: false,
            initial_complete: false,
            data_v1: false,
            _factory: factory,
            _lease: lease,
            _host: host,
        };
        prepare(&result._factory, &result.peer)?;
        let offer = result
            .operation(result.peer.create_offer()?)
            .await?
            .context("native offer")?;
        result
            .operation(result.peer.set_local_description(offer)?)
            .await?;
        while !result.gathered {
            while let Some(event) = result.peer.try_next_event() {
                result.observe(event);
            }
            if !result.gathered {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        let offer = result
            .peer
            .descriptions()?
            .pending_local
            .context("gathered local offer")?
            .sdp;
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 60;
        let rooms = if room == DATA_ROOM {
            vec![DATA_ROOM]
        } else {
            vec![DATA_ROOM, room]
        };
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            &serde_json::from_value(
                serde_json::json!([{"rooms":rooms,"allow":["join","publish","subscribe"]}]),
            )
            .unwrap(),
            subject,
            expiry,
        )?
        .token;
        let admission = http::Request::builder()
            .method("POST")
            .uri(format!("http://{server}:7070/api/v1/native/admissions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&serde_json::json!({"room":room}))?)?;
        let admission = create_http_client()
            .execute(admission)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            admission.status() == http::StatusCode::OK,
            "native ownership admission: {}",
            admission.status()
        );
        let admission: serde_json::Value = serde_json::from_slice(admission.body())?;
        let owner = admission["owner"]
            .as_str()
            .context("opaque native ownership capability")?;
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://{server}:7070/api/v1/native"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("pulsebeam-owner", owner)
            .header("pulsebeam-attempt", "1")
            .body(serde_json::to_vec(
                &serde_json::json!({"profile":wire::PROFILE,"offer":offer,"room":room}),
            )?)?;
        let response = create_http_client()
            .execute(request)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            response.status() == http::StatusCode::CREATED,
            "native bootstrap: {} {}; offered RTP: {:?}",
            response.status(),
            String::from_utf8_lossy(response.body()),
            offer
                .lines()
                .filter(
                    |line| ["m=", "a=rtpmap:", "a=fmtp:", "a=rtcp-fb:", "a=extmap:"]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                )
                .collect::<Vec<_>>(),
        );
        let body: serde_json::Value = serde_json::from_slice(response.body())?;
        result.data_v1 = body.get("data_version").and_then(serde_json::Value::as_u64)
            == Some(u64::from(DATA_V1));
        let participant = body["participant_id"]
            .as_str()
            .context("canonical participant")?
            .to_owned();
        let answer = body["answer"].as_str().context("native answer")?.to_owned();
        if media {
            result.media = Some(pulsebeam_agent_core::client_sdp::Context::new(
                &offer,
                &answer,
                pulsebeam_agent_core::client_sdp::Capabilities::from_wire(
                    &serde_json::from_value(body["rtp_capabilities"].clone())?,
                )?,
                u32::MAX,
                Default::default(),
            )?);
        }
        result
            .operation(result.peer.set_remote_description(SessionDescription {
                kind: SessionDescriptionType::Answer,
                sdp: answer,
            })?)
            .await?;
        while result.control.state() != DataChannelState::Open {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let mut grant = None;
        while !result.initial_complete {
            if let Some(batch) = result.batch()? {
                for record in batch.records {
                    if let Some(value) = record.confirmed
                        && let Some(wire::confirmed_state::Value::Session(session)) = value.value
                    {
                        anyhow::ensure!(session.closed.is_none(), "terminal initial Session");
                        grant = session.authorization;
                    }
                }
            }
            if !result.initial_complete {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        anyhow::ensure!(
            grant
                == Some(wire::Authorization {
                    expires_at_unix_seconds: i64::try_from(expiry)?,
                    revision: 1
                }),
            "initial Session grant"
        );
        Ok((result, participant))
    }

    async fn logical_when(
        &mut self,
        revision: u64,
        accepts: impl Fn(&wire::LogicalSubscriptionState) -> bool,
    ) -> anyhow::Result<wire::LogicalSubscriptionState> {
        loop {
            let value = self.confirmed(1, revision).await?;
            anyhow::ensure!(
                value.rejection.is_none(),
                "logical receiver rejected: {value:?}"
            );
            let Some(wire::confirmed_state::Value::LogicalSubscription(state)) = value.value else {
                anyhow::bail!("logical receiver absent");
            };
            if accepts(&state) {
                return Ok(*state);
            }
        }
    }

    fn prepare_receiver(&self, source_revision: u64, revision: u64, channel: u32) {
        self.send_state(wire::ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(wire::ExecutionReport {
                revision,
                outcome: Some(wire::execution_report::Outcome::LogicalReceiver(
                    wire::LogicalReceiverReport {
                        source_revision,
                        outcome: Some(wire::logical_receiver_report::Outcome::Prepared(
                            payload_binding(channel),
                        )),
                    },
                )),
            }),
        });
    }

    async fn ended(&mut self, reference: u64, revision: u64) -> anyhow::Result<()> {
        loop {
            let value = self.confirmed(reference, revision).await?;
            anyhow::ensure!(
                value.rejection.is_none(),
                "terminal receiver rejection: {value:?}"
            );
            if let Some(wire::confirmed_state::Value::Absent(absent)) = value.value {
                anyhow::ensure!(
                    absent.reason == wire::AbsenceReason::SourceEnded as i32,
                    "wrong terminal cause"
                );
                return Ok(());
            }
        }
    }

    async fn payload(&self, channel: usize) -> anyhow::Result<Vec<u8>> {
        loop {
            while let Some(event) = self.payload[channel].try_next_event() {
                if let DataChannelEvent::Message(message) = event {
                    anyhow::ensure!(
                        message.kind == DataChannelMessageKind::Binary,
                        "non-binary payload"
                    );
                    return Ok(message.bytes.to_vec());
                }
            }
            anyhow::ensure!(
                self.payload[channel].state() != DataChannelState::Closed,
                "payload channel closed"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn closed(&self, channel: usize) {
        while self.payload[channel].state() != DataChannelState::Closed {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn send_payload(&self, channel: usize, frame: Vec<u8>) {
        assert_eq!(
            self.payload[channel].send(DataChannelMessage::binary(frame)),
            DataChannelSendResult::Sent
        );
    }

    fn observe(&mut self, event: PeerConnectionEvent) {
        if matches!(
            event,
            PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
        ) {
            self.gathered = true;
        }
    }

    async fn operation(
        &mut self,
        operation: OperationId,
    ) -> anyhow::Result<Option<SessionDescription>> {
        loop {
            while let Some(event) = self.peer.try_next_event() {
                if let PeerConnectionEvent::OperationComplete(completion) = event {
                    anyhow::ensure!(
                        completion.operation_id == operation,
                        "unexpected native operation"
                    );
                    return Ok(completion.result?);
                }
                self.observe(event);
            }
            // The common NativeHost step owns native time and packet pumping.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn batch(&mut self) -> anyhow::Result<Option<wire::ServerStateBatch>> {
        while let Some(event) = self.control.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                anyhow::ensure!(
                    message.kind == DataChannelMessageKind::Binary,
                    "non-binary native control"
                );
                let batch = codec::decode_server(&message.bytes)
                    .map_err(|error| anyhow::anyhow!("StateBatch: {error:?}"))?;
                self.initial_complete |= batch.initial_complete;
                return Ok(Some(batch));
            }
        }
        anyhow::ensure!(
            self.control.state() != DataChannelState::Closed,
            "control channel closed"
        );
        Ok(None)
    }

    fn desire(&self, reference: u64, revision: u64, spec: Option<wire::ResourceSpec>) {
        self.send_state(wire::ClientStateRecord {
            owned: reference,
            execution: None,
            desired: Some(wire::DesiredState {
                revision,
                value: Some(match spec {
                    Some(spec) => wire::desired_state::Value::Present(Box::new(spec)),
                    None => wire::desired_state::Value::Absent(true),
                }),
            }),
        });
    }

    fn send_state(&self, record: wire::ClientStateRecord) {
        let bytes = codec::encode_client(&wire::ClientStateBatch {
            records: vec![record],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            self.control.send(DataChannelMessage::binary(bytes)),
            DataChannelSendResult::Sent
        );
    }

    async fn confirmed(
        &mut self,
        reference: u64,
        revision: u64,
    ) -> anyhow::Result<wire::ConfirmedState> {
        loop {
            while let Some(batch) = self.batch()? {
                for record in batch.records {
                    if record.key.and_then(|key| key.key)
                        == Some(wire::resource_key::Key::Owned(reference))
                        && let Some(value) = record.confirmed
                        && value.evaluated_revision >= revision
                    {
                        anyhow::ensure!(
                            value.evaluated_revision == revision,
                            "unexpected later evaluation"
                        );
                        return Ok(value);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

fn payload_binding(channel: u32) -> wire::TrackBinding {
    wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::SctpDataV1(
            wire::SctpDataV1Parameters {
                stream_id: Some(channel),
                version: u32::from(DATA_V1),
                channel_incarnation: u64::from(channel),
                delivery: wire::DataV1Delivery::Latest as i32,
            },
        )),
    }
}

fn publication(channel: u32, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                track_id: "camera-updates".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: wire::DataDelivery::Latest as i32,
                    binding: Some(payload_binding(channel)),
                })),
            },
        )),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Published {
    track: wire::Track,
    incarnation: u64,
}

fn live(value: wire::ConfirmedState, spec: &wire::ResourceSpec, participant: &str) -> Published {
    assert!(value.rejection.is_none());
    let Some(wire::confirmed_state::Value::Owned(live)) = value.value else {
        panic!("expected admitted resource: {value:?}")
    };
    assert_eq!(live.spec.as_ref(), Some(spec));
    assert_eq!(live.participant.unwrap().participant_id, participant);
    assert_eq!(
        live.execution,
        if spec.settings.as_ref().unwrap().enabled == Some(true) {
            wire::ExecutionStatus::Installed
        } else {
            wire::ExecutionStatus::Disabled
        } as i32
    );
    let track = live.track.unwrap();
    assert_eq!(track.participant_id, participant);
    assert_eq!(track.track_id, "camera-updates");
    assert_eq!(
        track.kind,
        Some(wire::track::Kind::Data(wire::DataTrack {
            delivery: wire::DataDelivery::Latest as i32
        }))
    );
    assert!(!track.publication_id.is_empty());
    let incarnation = live
        .data_v1_source
        .expect("installed DATA source incarnation");
    assert_ne!(incarnation, 0);
    Published { track, incarnation }
}

fn subscription(source: &Published, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                data_version: Some(u32::from(DATA_V1)),
                source: Some(wire::LogicalSourceAddress {
                    room: DATA_ROOM.into(),
                    participant_id: source.track.participant_id.clone(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: source.track.track_id.clone(),
                }),
            },
        )),
    }
}

fn message(sequence: u64, payload: u8) -> Vec<u8> {
    let bytes = [payload];
    let record = Record::new(
        Header {
            unit_seq: sequence,
            unit_len: 1,
            index: 0,
            count: 1,
            capture: None,
        },
        &bytes,
        None,
    )
    .unwrap();
    let mut output = vec![0; record.encoded_len()];
    record.write(&mut output).unwrap();
    output
}

async fn install_receiver(
    peer: &mut WirePeer,
    source: &Published,
    desired_revision: u64,
    report_revision: u64,
    channel: u32,
) -> anyhow::Result<wire::LogicalSubscriptionState> {
    let offer = peer
        .logical_when(desired_revision, |state| {
            state.source.as_ref() == Some(&source.track) && state.data_v1.is_some()
        })
        .await?;
    assert_eq!(offer.data_v1.as_ref().unwrap().source, source.incarnation);
    assert!(offer.execution.is_none());
    peer.prepare_receiver(offer.source_revision, report_revision, channel);
    let installed =
        wire::logical_subscription_state::Execution::Installed(payload_binding(channel));
    let settled = peer
        .logical_when(desired_revision, |state| state.execution.is_some())
        .await?;
    assert_eq!(settled.source_revision, offer.source_revision);
    assert_eq!(settled.execution, Some(installed));
    Ok(settled)
}

#[derive(Debug)]
enum PeerStep {
    ReceiverConnected,
    Source(Published),
    Ready,
    Delivered,
    Retired,
}

async fn run_publisher(
    ip: IpAddr,
    server: IpAddr,
    cross_shard: bool,
    tx: tokio::sync::mpsc::Sender<PeerStep>,
    mut rx: tokio::sync::mpsc::Receiver<PeerStep>,
) -> anyhow::Result<()> {
    // The DATA receiver reserves partition zero. The publisher's second
    // partition must differ from the later audio listener's first partition.
    anyhow::ensure!(matches!(rx.recv().await, Some(PeerStep::ReceiverConnected)));
    let (mut peer, participant) = WirePeer::connect(ip, server, "publisher", true).await?;
    assert!(peer.data_v1);
    let spec = publication(1, true);
    peer.desire(1, 1, Some(spec.clone()));
    let first = live(peer.confirmed(1, 1).await?, &spec, &participant);
    peer.send_payload(0, message(0, 42));
    let disabled = publication(1, false);
    peer.desire(1, 2, Some(disabled.clone()));
    assert_eq!(
        live(peer.confirmed(1, 2).await?, &disabled, &participant),
        first
    );
    peer.desire(1, 3, Some(spec.clone()));
    assert_eq!(
        live(peer.confirmed(1, 3).await?, &spec, &participant),
        first
    );
    tx.send(PeerStep::Source(first.clone())).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Ready)));
    peer.send_payload(0, message(1, 43));
    assert!(matches!(rx.recv().await, Some(PeerStep::Ready)));
    peer.send_payload(0, message(2, 44));
    assert!(matches!(rx.recv().await, Some(PeerStep::Delivered)));
    peer.desire(1, 4, None);
    let removed = peer.confirmed(1, 4).await?;
    assert!(removed.rejection.is_none());
    assert!(
        matches!(removed.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::Removed as i32)
    );
    peer.desire(1, 5, None);
    let gone = peer.confirmed(1, 5).await?;
    assert!(gone.rejection.is_none());
    assert!(
        matches!(gone.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::Gone as i32)
    );
    peer.closed(0).await;
    tx.send(PeerStep::Retired).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    let spec = publication(2, true);
    peer.desire(6, 6, Some(spec.clone()));
    let second = live(peer.confirmed(6, 6).await?, &spec, &participant);
    assert_ne!(first.track.publication_id, second.track.publication_id);
    assert_ne!(first.incarnation, second.incarnation);
    tx.send(PeerStep::Source(second.clone())).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Ready)));
    peer.send_payload(1, message(0, 46));
    assert!(matches!(rx.recv().await, Some(PeerStep::Delivered)));
    peer.payload[1].close()?;
    peer.ended(6, 6).await?;
    tx.send(PeerStep::Retired).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    assert_eq!(peer.control.state(), DataChannelState::Open);
    peer.prove_audio_publication(server, &participant, cross_shard)
        .await
        .context("real Native RTP source lifecycle")?;
    peer.prove_whip_whep_opus(server, cross_shard)
        .await
        .context("real WHIP-to-WHEP Opus over Fabric")?;
    peer.peer.close()?;
    Ok(())
}

async fn run_receiver(
    ip: IpAddr,
    server: IpAddr,
    tx: tokio::sync::mpsc::Sender<PeerStep>,
    mut rx: tokio::sync::mpsc::Receiver<PeerStep>,
) -> anyhow::Result<()> {
    let (mut peer, _) = WirePeer::connect(ip, server, "receiver", false).await?;
    assert!(peer.data_v1);
    tx.send(PeerStep::ReceiverConnected).await?;
    let Some(PeerStep::Source(first)) = rx.recv().await else {
        anyhow::bail!("first source not announced");
    };
    let spec = subscription(&first, true);
    peer.desire(1, 1, Some(spec.clone()));
    let first_binding = install_receiver(&mut peer, &first, 1, 2, 1).await?;
    tx.send(PeerStep::Ready).await?;
    assert_eq!(peer.payload(0).await?, message(1, 43));

    let disabled = subscription(&first, false);
    peer.desire(1, 3, Some(disabled.clone()));
    let state = peer
        .logical_when(3, |state| {
            state.source.is_none() && state.execution.is_none()
        })
        .await?;
    assert_eq!(state.spec, Some(disabled));
    peer.closed(0).await;

    peer.desire(1, 4, Some(spec.clone()));
    let resumed = install_receiver(&mut peer, &first, 4, 5, 2).await?;
    assert_eq!(resumed.spec, Some(spec));
    assert!(resumed.source_revision > first_binding.source_revision);
    assert_ne!(
        resumed.data_v1.as_ref().unwrap().branch,
        first_binding.data_v1.unwrap().branch
    );
    tx.send(PeerStep::Ready).await?;
    assert_eq!(peer.payload(1).await?, message(2, 44));
    tx.send(PeerStep::Delivered).await?;

    peer.logical_when(4, |state| {
        state.source.is_none() && state.execution.is_none()
    })
    .await?;
    peer.closed(1).await;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    tx.send(PeerStep::Retired).await?;
    let Some(PeerStep::Source(second)) = rx.recv().await else {
        anyhow::bail!("replacement source not announced");
    };
    let replacement = install_receiver(&mut peer, &second, 4, 6, 3).await?;
    assert!(replacement.source_revision > resumed.source_revision);
    assert_ne!(
        replacement.data_v1.unwrap().branch,
        resumed.data_v1.unwrap().branch
    );
    tx.send(PeerStep::Ready).await?;
    assert_eq!(peer.payload(2).await?, message(0, 46));
    tx.send(PeerStep::Delivered).await?;
    peer.logical_when(4, |state| {
        state.source.is_none() && state.execution.is_none()
    })
    .await?;
    peer.closed(2).await;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    tx.send(PeerStep::Retired).await?;
    assert_eq!(peer.control.state(), DataChannelState::Open);
    peer.peer.close()?;
    Ok(())
}

#[test]
fn native_state_data_lifetimes_route_and_retire_exact_receivers() {
    prove_native_state_data_lifetimes(DEFAULT_SIM_SHARDS);
}

#[test]
fn native_state_data_lifetimes_route_and_retire_exact_receivers_same_shard() {
    prove_native_state_data_lifetimes(1);
}

fn prove_native_state_data_lifetimes(shards: usize) {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|seed| seed.parse().unwrap())
        .unwrap_or(1);
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(20))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server = subnet_ip(subnet, 1);
    sim.host(server, move || async move {
        sfu_node_builder(server, pulsebeam_runtime::rand::seeded_rng(seed), shards)
            .with_local_data_v1()
            .with_local_rtp_v1()
            .run(tokio_util::sync::CancellationToken::new())
            .await
            .map_err(Into::into)
    });
    let (pub_tx, viewer_rx) = tokio::sync::mpsc::channel(1);
    let (viewer_tx, pub_rx) = tokio::sync::mpsc::channel(1);
    sim.client(subnet_ip(subnet, 2), async move {
        tokio::time::timeout(
            Duration::from_secs(10),
            run_publisher(subnet_ip(subnet, 2), server, shards > 1, pub_tx, pub_rx),
        )
        .await
        .context("native StateBatch publisher deadline")??;
        Ok(())
    });
    sim.client(subnet_ip(subnet, 3), async move {
        tokio::time::timeout(
            Duration::from_secs(10),
            run_receiver(subnet_ip(subnet, 3), server, viewer_tx, viewer_rx),
        )
        .await
        .context("native StateBatch receiver deadline")??;
        Ok(())
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(30))
        .expect("native StateBatch/SCTP proof failed");
    assert!(pulsebeam::sim_metrics::routing_counter("fabric_whep_rtp_enqueued") > 0);
}
