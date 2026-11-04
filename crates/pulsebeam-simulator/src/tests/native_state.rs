use std::{net::IpAddr, rc::Rc, time::Duration};

use anyhow::Context;
use pulsebeam_proto::{
    prelude::Message,
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
    reserve_subnet, run_sim_or_timeout, start_sfu_node_with, subnet_ip,
};

mod audio;

struct WirePeer {
    media: Option<pulsebeam_agent_core::client_sdp::Context>,
    peer: PeerConnection,
    control: DataChannel,
    payload: [DataChannel; 2],
    gathered: bool,
    initial_complete: bool,
    _factory: PeerConnectionFactory,
    _lease: Rc<RuntimeLease>,
    _host: NativeHost,
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
        let (_runtime, lease) = host.runtime()?;
        let factory = host.raw_peer_factory()?;
        let peer = factory.create_peer_connection(PeerConfiguration {
            always_negotiate_data_channels: true,
            ..Default::default()
        })?;
        let channel = |id, label| {
            peer.create_data_channel(
                label,
                DataChannelConfiguration {
                    negotiated: true,
                    id: Some(id),
                    ordered: true,
                    ..Default::default()
                },
            )
        };
        let control = channel(0, "v1/sys/signaling")?;
        let payload = [channel(1, "prepared-one")?, channel(2, "prepared-two")?];
        let mut result = Self {
            media: None,
            peer,
            control,
            payload,
            gathered: false,
            initial_complete: false,
            _factory: factory,
            _lease: lease,
            _host: host,
        };
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
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            "native-state-wire",
            subject,
            expiry,
        )?;
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://{server}:7070/api/v1/native"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(
                &serde_json::json!({"profile":wire::PROFILE,"offer":offer}),
            )?)?;
        let response = create_http_client()
            .execute(request)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            response.status() == http::StatusCode::CREATED,
            "native bootstrap: {} {}",
            response.status(),
            String::from_utf8_lossy(response.body())
        );
        let body: serde_json::Value = serde_json::from_slice(response.body())?;
        let participant = body["participant_id"]
            .as_str()
            .context("canonical participant")?
            .to_owned();
        let answer = body["answer"].as_str().context("native answer")?.to_owned();
        if audio {
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

    async fn execution(
        &mut self,
        reference: u64,
        revision: u64,
        status: wire::ExecutionStatus,
    ) -> anyhow::Result<wire::ConfirmedState> {
        loop {
            let value = self.confirmed(reference, revision).await?;
            anyhow::ensure!(value.rejection.is_none(), "receiver rejected: {value:?}");
            let Some(wire::confirmed_state::Value::Owned(live)) = &value.value else {
                anyhow::bail!("receiver absent: {value:?}");
            };
            if live.execution == status as i32 {
                return Ok(value);
            }
            anyhow::ensure!(
                live.execution == wire::ExecutionStatus::WaitingForSource as i32,
                "unexpected execution: {value:?}"
            );
        }
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

    async fn payload(&self, channel: usize) -> anyhow::Result<pulsebeam_proto::topic::TopicFrame> {
        loop {
            while let Some(event) = self.payload[channel].try_next_event() {
                if let DataChannelEvent::Message(message) = event {
                    anyhow::ensure!(
                        message.kind == DataChannelMessageKind::Binary,
                        "non-binary payload"
                    );
                    return pulsebeam_proto::topic::TopicFrame::decode_bounded(&message.bytes)
                        .map_err(|error| anyhow::anyhow!("{error:?}"));
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

    fn send_payload(&self, channel: usize, frame: &pulsebeam_proto::topic::TopicFrame) {
        assert_eq!(
            self.payload[channel].send(DataChannelMessage::binary(frame.encode_to_vec())),
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
        let bytes = codec::encode_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: reference,
                execution: None,
                desired: Some(wire::DesiredState {
                    revision,
                    value: Some(match spec {
                        Some(spec) => wire::desired_state::Value::Present(Box::new(spec)),
                        None => wire::desired_state::Value::Absent(true),
                    }),
                }),
            }],
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

fn publication(channel: u32, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: "camera/updates".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: wire::DataDelivery::Ordered as i32,
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::Sctp(
                            wire::SctpParameters {
                                stream_id: Some(channel),
                            },
                        )),
                    }),
                })),
            },
        )),
    }
}

fn live(value: wire::ConfirmedState, spec: &wire::ResourceSpec, participant: &str) -> wire::Track {
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
    assert_eq!(track.label, "camera/updates");
    assert_eq!(
        track.kind,
        Some(wire::track::Kind::Data(wire::DataTrack {
            delivery: wire::DataDelivery::Ordered as i32
        }))
    );
    assert!(!track.track_id.is_empty());
    track
}

fn subscription(source: &wire::Track, channel: u32, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                track_id: source.track_id.clone(),
                kind: wire::TrackKind::Data as i32,
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Sctp(wire::SctpParameters {
                        stream_id: Some(channel),
                    })),
                }),
            },
        )),
    }
}

fn message(source: &wire::Track, sequence: u64, payload: u8) -> pulsebeam_proto::topic::TopicFrame {
    pulsebeam_proto::topic::TopicFrame {
        body: Some(pulsebeam_proto::topic::topic_frame::Body::Message(
            pulsebeam_proto::topic::TopicMessage {
                data_track_id: source.track_id.clone(),
                sequence,
                payload: vec![payload],
            },
        )),
    }
}

#[derive(Debug)]
enum PeerStep {
    Source(wire::Track),
    Ready,
    Delivered,
    Retired,
}

async fn run_publisher(
    ip: IpAddr,
    server: IpAddr,
    tx: tokio::sync::mpsc::Sender<PeerStep>,
    mut rx: tokio::sync::mpsc::Receiver<PeerStep>,
) -> anyhow::Result<()> {
    let (mut peer, participant) = WirePeer::connect(ip, server, "publisher", true).await?;
    let spec = publication(1, true);
    peer.desire(1, 1, Some(spec.clone()));
    let first = live(peer.confirmed(1, 1).await?, &spec, &participant);
    peer.send_payload(0, &message(&first, 0, 42));
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
    peer.send_payload(0, &message(&first, 1, 43));
    let probe = peer.payload(0).await?;
    assert!(probe.is_subscriber_control());
    assert_eq!(probe.data_track_id(), Some(first.track_id.as_str()));
    peer.send_payload(0, &message(&first, 2, 44));
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
    assert_ne!(first.track_id, second.track_id);
    tx.send(PeerStep::Source(second.clone())).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Ready)));
    peer.send_payload(1, &message(&second, 0, 46));
    assert!(matches!(rx.recv().await, Some(PeerStep::Delivered)));
    peer.payload[1].close()?;
    peer.ended(6, 6).await?;
    tx.send(PeerStep::Retired).await?;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    assert_eq!(peer.control.state(), DataChannelState::Open);
    peer.prove_audio_publication(server, &participant)
        .await
        .context("real Native RTP source lifecycle")?;
    peer.peer.close()?;
    Ok(())
}

async fn run_receiver(
    ip: IpAddr,
    server: IpAddr,
    tx: tokio::sync::mpsc::Sender<PeerStep>,
    mut rx: tokio::sync::mpsc::Receiver<PeerStep>,
) -> anyhow::Result<()> {
    let Some(PeerStep::Source(first)) = rx.recv().await else {
        anyhow::bail!("first source not announced");
    };
    let (mut peer, _) = WirePeer::connect(ip, server, "receiver", false).await?;
    let spec = subscription(&first, 1, true);
    peer.desire(1, 1, Some(spec.clone()));
    let value = peer
        .execution(1, 1, wire::ExecutionStatus::Installed)
        .await?;
    assert_eq!(live(value, &spec, &first.participant_id), first);
    tx.send(PeerStep::Ready).await?;
    assert_eq!(peer.payload(0).await?, message(&first, 1, 43));
    let disabled = subscription(&first, 1, false);
    peer.desire(1, 2, Some(disabled.clone()));
    let value = peer
        .execution(1, 2, wire::ExecutionStatus::Disabled)
        .await?;
    assert_eq!(live(value, &disabled, &first.participant_id), first);
    peer.desire(1, 3, Some(spec.clone()));
    let value = peer
        .execution(1, 3, wire::ExecutionStatus::Installed)
        .await?;
    assert_eq!(live(value, &spec, &first.participant_id), first);
    peer.send_payload(
        0,
        &pulsebeam_proto::topic::TopicFrame {
            body: Some(pulsebeam_proto::topic::topic_frame::Body::Probe(
                pulsebeam_proto::topic::TopicProbe {
                    data_track_id: first.track_id.clone(),
                    from_sequence: 2,
                    live: true,
                },
            )),
        },
    );
    assert_eq!(peer.payload(0).await?, message(&first, 2, 44));
    tx.send(PeerStep::Delivered).await?;
    peer.ended(1, 3).await?;
    peer.closed(0).await;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    tx.send(PeerStep::Retired).await?;
    let Some(PeerStep::Source(second)) = rx.recv().await else {
        anyhow::bail!("replacement source not announced");
    };
    let spec = subscription(&second, 2, true);
    peer.desire(4, 4, Some(spec.clone()));
    let value = peer
        .execution(4, 4, wire::ExecutionStatus::Installed)
        .await?;
    assert_eq!(live(value, &spec, &second.participant_id), second);
    tx.send(PeerStep::Ready).await?;
    assert_eq!(peer.payload(1).await?, message(&second, 0, 46));
    tx.send(PeerStep::Delivered).await?;
    peer.ended(4, 4).await?;
    peer.closed(1).await;
    assert!(matches!(rx.recv().await, Some(PeerStep::Retired)));
    tx.send(PeerStep::Retired).await?;
    assert_eq!(peer.control.state(), DataChannelState::Open);
    peer.peer.close()?;
    Ok(())
}

#[test]
fn native_state_data_lifetimes_route_and_retire_exact_receivers() {
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
        start_sfu_node_with(
            server,
            pulsebeam_runtime::rand::seeded_rng(seed),
            DEFAULT_SIM_SHARDS,
            false,
        )
        .await
        .map_err(Into::into)
    });
    let (pub_tx, viewer_rx) = tokio::sync::mpsc::channel(1);
    let (viewer_tx, pub_rx) = tokio::sync::mpsc::channel(1);
    sim.client(subnet_ip(subnet, 2), async move {
        tokio::time::timeout(
            Duration::from_secs(10),
            run_publisher(subnet_ip(subnet, 2), server, pub_tx, pub_rx),
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
}
