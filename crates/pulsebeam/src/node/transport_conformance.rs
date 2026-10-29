//! Real dual-stack UDP and isolated ICE-TCP SDK/application witnesses.
use super::*;
use anyhow::{anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, Generation, MediaTopology, Notification,
    TopicMessage, TopicMode, TopicNotification, TopicPublisher, TopicRegistrations, TopicSend,
    TopicSubscriber,
};
use pulsebeam_agent_native::{
    Agent, AgentEvent, AsyncHttpClient, Config, Host, NativeStatistics, ObservationStream,
    StreamUpdate,
};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

const CONNECT: Duration = Duration::from_secs(30);
const DELIVERY: Duration = Duration::from_secs(30);
const CLEANUP: Duration = Duration::from_secs(10);
const TOPIC: &str = "dual-stack-probe";
const A_TO_B: &[u8] = b"application-owned IPv4 to IPv6 probe";
const B_TO_A: &[u8] = b"application-owned IPv6 to IPv4 probe";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Udp,
    Tcp,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}
impl Family {
    fn of(ip: IpAddr) -> Self {
        if ip.is_ipv4() { Self::V4 } else { Self::V6 }
    }

    fn address(self, port: u16) -> SocketAddr {
        match self {
            Self::V4 => SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Self::V6 => SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        }
    }
}

// Only candidate facts survive the HTTP interception. Never retain credentials/SDP.
#[derive(Clone, Debug)]
struct AnswerEvidence {
    candidates: Vec<SocketAddr>,
}

fn constrain_answer(
    answer: &str,
    family: Family,
    port: u16,
    protocol: Protocol,
) -> Result<(String, AnswerEvidence)> {
    let mut candidates = Vec::new();
    let mut filtered = Vec::new();
    for line in answer.split("\r\n") {
        if line.starts_with("a=candidate:") {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            ensure!(
                fields
                    .get(2)
                    .is_some_and(|p| p.eq_ignore_ascii_case(protocol.as_str())),
                "server advertised an unexpected transport"
            );
            if protocol == Protocol::Tcp {
                ensure!(
                    fields.windows(2).any(|pair| pair == ["tcptype", "passive"]),
                    "server ICE-TCP candidate is not passive"
                );
            }
            let ip: IpAddr = fields
                .get(4)
                .context("candidate address missing")?
                .parse()
                .context("invalid candidate address")?;
            let candidate_port: u16 = fields
                .get(5)
                .context("candidate port missing")?
                .parse()
                .context("invalid candidate port")?;
            let address = SocketAddr::new(ip, candidate_port);
            ensure!(
                ip.is_loopback() && candidate_port == port,
                "unusable or unexpected advertised candidate: {address}"
            );
            candidates.push(address);
            if Family::of(ip) != family {
                continue;
            }
        }
        filtered.push(line);
    }
    for required in [Family::V4.address(port), Family::V6.address(port)] {
        ensure!(
            candidates.contains(&required),
            "actual server answer lacks required candidate {required}"
        );
    }
    Ok((filtered.join("\r\n"), AnswerEvidence { candidates }))
}

struct FamilyHttp {
    client: reqwest::Client,
    family: Family,
    port: u16,
    protocol: Protocol,
    answer: watch::Sender<Option<AnswerEvidence>>,
}
impl AsyncHttpClient for FamilyHttp {
    fn execute(
        &self,
        request: pulsebeam_core::net::HttpRequest,
    ) -> pulsebeam_core::net::HttpResult<'_> {
        Box::pin(async move {
            let native_post =
                request.method() == hyper::Method::POST && request.uri().path() == "/api/v1/native";
            let response = AsyncHttpClient::execute(&self.client, request).await?;
            if !native_post || !response.status().is_success() {
                return Ok(response);
            }
            if self.answer.borrow().is_some() {
                return Err(
                    anyhow!("replacement HTTP connection cannot reroll qualification").into(),
                );
            }
            let (parts, body) = response.into_parts();
            let mut body: Value = serde_json::from_slice(&body)?;
            let answer = body
                .get("answer")
                .and_then(Value::as_str)
                .context("native HTTP answer missing")?;
            let (filtered, evidence) =
                constrain_answer(answer, self.family, self.port, self.protocol)?;
            *body
                .get_mut("answer")
                .context("native HTTP answer missing")? = Value::String(filtered);
            self.answer.send_replace(Some(evidence));
            let mut response = hyper::Response::from_parts(parts, serde_json::to_vec(&body)?);
            response.headers_mut().remove(hyper::header::CONTENT_LENGTH);
            Ok(response)
        })
    }
}

#[derive(Debug)]
struct SelectedPair {
    generation: u64,
    timestamp_us: i64,
    remote: SocketAddr,
}

fn selected_pair(
    stats: &NativeStatistics,
    generation: u64,
    remote: SocketAddr,
    protocol: Protocol,
) -> Result<SelectedPair> {
    ensure!(
        stats.generation == generation,
        "statistics generation mismatch"
    );
    let report: Value =
        serde_json::from_slice(&stats.report_json).context("invalid statistics JSON")?;
    let records: Vec<&Value> = match &report {
        Value::Array(records) => records.iter().collect(),
        Value::Object(records) => records.values().collect(),
        _ => bail!("statistics report is not a collection"),
    };
    let find = |id: &str| -> Result<&Value> {
        let matches: Vec<_> = records
            .iter()
            .copied()
            .filter(|record| record.get("id").and_then(Value::as_str) == Some(id))
            .collect();
        ensure!(
            matches.len() == 1,
            "selected statistics ID missing or ambiguous"
        );
        matches
            .first()
            .copied()
            .context("selected statistics ID missing")
    };
    let transport = records
        .iter()
        .find(|record| {
            record.get("type").and_then(Value::as_str) == Some("transport")
                && record.get("selectedCandidatePairId").is_some()
        })
        .context("selected transport missing")?;
    let pair = find(
        transport
            .get("selectedCandidatePairId")
            .and_then(Value::as_str)
            .context("selected pair ID missing")?,
    )?;
    ensure!(
        pair.get("type").and_then(Value::as_str) == Some("candidate-pair")
            && pair.get("state").and_then(Value::as_str) == Some("succeeded"),
        "selected pair has not succeeded"
    );
    let candidate = |key: &str, kind: &str| -> Result<SocketAddr> {
        let candidate = find(
            pair.get(key)
                .and_then(Value::as_str)
                .context("candidate ID missing")?,
        )?;
        ensure!(
            candidate.get("type").and_then(Value::as_str) == Some(kind),
            "candidate kind mismatch"
        );
        ensure!(
            candidate.get("protocol").and_then(Value::as_str) == Some(protocol.as_str()),
            "selected candidate protocol mismatch"
        );
        // Native peer-reflexive local candidates can retain "passive" after
        // initiating TCP. Require the advertised server's role, not a local role.
        if protocol == Protocol::Tcp && kind == "remote-candidate" {
            ensure!(
                candidate.get("tcpType").and_then(Value::as_str) == Some("passive"),
                "selected server ICE-TCP candidate is not passive: {candidate}"
            );
        }
        let address: IpAddr = candidate
            .get("address")
            .and_then(Value::as_str)
            .context("candidate address missing")?
            .parse()
            .context("candidate address is not an IP literal")?;
        let port = candidate
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .context("candidate port missing or invalid")?;
        Ok(SocketAddr::new(address, port))
    };
    let local = candidate("localCandidateId", "local-candidate")?;
    let selected_remote = candidate("remoteCandidateId", "remote-candidate")?;
    ensure!(
        selected_remote == remote && Family::of(local.ip()) == Family::of(remote.ip()),
        "selected pair family/address does not match constrained server"
    );
    Ok(SelectedPair {
        generation,
        timestamp_us: stats.timestamp_us,
        remote: selected_remote,
    })
}

fn publisher() -> TopicPublisher {
    TopicPublisher {
        topic: TOPIC.into(),
        mode: TopicMode::Latest,
    }
}

fn desired() -> DesiredState {
    DesiredState {
        revision: 1,
        connected: true,
        topics: TopicRegistrations {
            publishers: vec![publisher()],
            subscribers: vec![TopicSubscriber {
                topic: TOPIC.into(),
                mode: TopicMode::Latest,
            }],
        },
        ..Default::default()
    }
}

async fn ready(
    agent: &Agent,
    remote: SocketAddr,
    prior: Option<&SelectedPair>,
    protocol: Protocol,
) -> Result<SelectedPair> {
    let mut snapshots = agent.snapshots();
    let mut statistics = agent.statistics();
    loop {
        let snapshot = snapshots.borrow().clone();
        ensure!(
            !matches!(
                snapshot.connection,
                ConnectionState::TerminalFailure | ConnectionState::Reconnecting
            ),
            "SDK failed or replaced the qualification connection"
        );
        if let Some(prior) = prior {
            ensure!(
                snapshot.generation.map(Generation::get) == Some(prior.generation),
                "SDK generation changed during delivery"
            );
        }
        let topics_ready = snapshot.topics.publishers.iter().any(|status| {
            status.registration == publisher()
                && status.channel.is_some()
                && status.data_track_id.is_some()
        }) && snapshot.topics.subscribers.iter().any(|status| {
            status.registration.topic == TOPIC && status.channel.is_some() && status.publishers >= 1
        });
        tracing::info!(?protocol, %remote, connection = ?snapshot.connection, topics_ready,
            has_statistics = statistics.borrow().is_some(), "qualification readiness");
        if snapshot.connection == ConnectionState::Connected
            && topics_ready
            && let (Some(generation), Some(stats)) =
                (snapshot.generation, statistics.borrow().as_ref())
            && let Err(error) = selected_pair(stats, generation.get(), remote, protocol)
        {
            tracing::info!(?error, ?protocol, %remote, "waiting for qualified selected pair");
        }
        if snapshot.connection == ConnectionState::Connected
            && topics_ready
            && let (Some(generation), Some(stats)) =
                (snapshot.generation, statistics.borrow().as_ref())
            && let Ok(pair) = selected_pair(stats, generation.get(), remote, protocol)
            && prior.is_none_or(|prior| pair.timestamp_us > prior.timestamp_us)
        {
            return Ok(pair);
        }
        tokio::select! {
            result = snapshots.changed() => { result.context("SDK snapshots closed before readiness")?; }
            result = statistics.changed() => { result.context("SDK statistics closed before selected-pair evidence")?; }
        }
    }
}

fn recipient_payload(message: Option<&TopicMessage>, sender: &str, expected: &[u8]) -> Result<()> {
    let message = message.context("missing recipient application payload")?;
    ensure!(
        message.participant_id == sender
            && message.topic == TOPIC
            && message.mode == TopicMode::Latest
            && message.payload == expected,
        "recipient application sender/topic/payload mismatch"
    );
    Ok(())
}

fn live_generation(
    connection: &ConnectionState,
    generation: Option<u64>,
    expected: u64,
) -> Result<()> {
    ensure!(
        *connection == ConnectionState::Connected && generation == Some(expected),
        "both original SDK generations must be concurrently connected"
    );
    Ok(())
}

fn retired(connection: &ConnectionState, has_statistics: bool) -> Result<()> {
    ensure!(
        *connection == ConnectionState::Disconnected && !has_statistics,
        "native graph not retired"
    );
    Ok(())
}

async fn received(
    events: &ObservationStream<AgentEvent>,
    sender: &str,
    expected: &[u8],
) -> Result<()> {
    loop {
        match events.next().await {
            StreamUpdate::Value(AgentEvent::Core(Notification::Topic(
                TopicNotification::Message(message),
            ))) => {
                recipient_payload(Some(&message), sender, expected)?;
                tracing::info!(
                    "EXERCISED recipient application payload, {} bytes",
                    expected.len()
                );
                return Ok(());
            }
            StreamUpdate::Value(AgentEvent::Core(Notification::Topic(
                TopicNotification::SendDropped { .. } | TopicNotification::ChannelFailed { .. },
            )))
            | StreamUpdate::Value(AgentEvent::RuntimeFailed(_)) => {
                bail!("SDK topic/runtime failed during delivery")
            }
            StreamUpdate::Lagged { .. } | StreamUpdate::Closed => {
                bail!("recipient application observation lost")
            }
            _ => {}
        }
    }
}

fn config(endpoint: &str, participant: &str) -> Result<Config> {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .saturating_add(120);
    let mut session = AgentConfig {
        endpoint: endpoint.into(),
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            "dual-stack-transport",
            participant,
            expiry,
        )?,
        topology: MediaTopology {
            local_video: 0,
            local_audio: 0,
            remote_video: 0,
            remote_audio: 0,
        },
        retry: Default::default(),
        log_level: Default::default(),
    };
    session.retry.maximum_attempts = 1;
    Ok(Config::new(session))
}

async fn witness(a: &Agent, b: &Agent, port: u16, protocol: Protocol) -> Result<()> {
    let events_a = a.events();
    let events_b = b.events();
    let (pair_a, pair_b) = tokio::time::timeout(CONNECT, async {
        tokio::try_join!(a.replace_desired(desired()), b.replace_desired(desired()))?;
        tokio::try_join!(ready(a, Family::V4.address(port), None, protocol), ready(b, Family::V6.address(port), None, protocol))
    }).await.context("FAILED: 30s dual-stack connection/topic/selected-pair evidence deadline; require IPv4/IPv6 loopback and compatible native candidates")??;
    tracing::info!(
        ?protocol,
        "EXERCISED both selected families and public topic readiness"
    );
    let sender_a = a
        .snapshot()
        .participant_external_id
        .context("IPv4 external participant identity missing")?;
    let sender_b = b
        .snapshot()
        .participant_external_id
        .context("IPv6 external participant identity missing")?;
    tokio::time::timeout(DELIVERY, async {
        tokio::try_join!(
            a.send_topic(TopicSend {
                publisher: publisher(),
                payload: A_TO_B.to_vec()
            }),
            b.send_topic(TopicSend {
                publisher: publisher(),
                payload: B_TO_A.to_vec()
            }),
        )?;
        tokio::try_join!(
            received(&events_b, &sender_a, A_TO_B),
            received(&events_a, &sender_b, B_TO_A)
        )?;
        tracing::info!("EXERCISED both recipient application payloads");
        tokio::try_join!(
            ready(a, pair_a.remote, Some(&pair_a), protocol),
            ready(b, pair_b.remote, Some(&pair_b), protocol)
        )?;
        for (agent, pair) in [(a, &pair_a), (b, &pair_b)] {
            let snapshot = agent.snapshot();
            live_generation(
                &snapshot.connection,
                snapshot.generation.map(Generation::get),
                pair.generation,
            )?;
        }
        anyhow::Ok(())
    })
    .await
    .context("FAILED: 30s bidirectional application payload/live-family deadline")??;
    tracing::info!(
        protocol = ?protocol, "EXERCISED concurrent real IPv4/IPv6 selected pairs, DTLS/SCTP, bidirectional recipient application probes"
    );
    Ok(())
}

async fn run(protocol: Protocol) -> Result<()> {
    use pulsebeam_core::auth::{
        DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
        KeyRegistryEntry,
    };
    let sockets = bind_udp_sockets("[::]:0".parse()?, None, 1, UdpMode::Batch)
        .await
        .context(
            "UNSUPPORTED: dual-stack IPv6 listener; enable IPv6 loopback on this Linux test host",
        )?;
    let port = sockets
        .first()
        .context("no owned UDP listener")?
        .local_addr()
        .port();
    let http = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", http.local_addr()?);
    let builder = NodeBuilder::new()
        .data_threads(1)
        .work_stealing(1)
        .local_addr(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port))
        .external_addrs(vec![Family::V4.address(port), Family::V6.address(port)])
        .rng(pulsebeam_runtime::rand::os_rng())
        .with_http_api_listener(http)
        .with_key_registry(KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )?);
    let builder = match protocol {
        Protocol::Udp => builder.udp_only(),
        Protocol::Tcp => builder.tcp_only(),
    };
    let advertised = builder.advertised_udp_addrs()?;
    let shutdown = CancellationToken::new();
    let mut node =
        tokio::task::spawn_local(builder.run_bound(sockets, advertised, shutdown.clone()));
    let mut node_observed = false;
    let mut agents = Vec::new();
    let mut answers = Vec::new();
    let result = async {
        for (family, name) in [(Family::V4, "v4"), (Family::V6, "v6")] {
            let (answer, evidence) = watch::channel(None);
            let agent = Agent::spawn(
                config(&endpoint, name)?,
                Host::new(Box::new(FamilyHttp {
                    client: reqwest::Client::new(),
                    family,
                    port,
                    protocol,
                    answer,
                })),
            )
            .await?;
            agents.push(agent);
            answers.push(evidence);
        }
        let a = agents.first().context("IPv4 agent missing")?;
        let b = agents.get(1).context("IPv6 agent missing")?;
        tokio::select! {
            result = witness(a, b, port, protocol) => result?,
            result = &mut node => {
                node_observed = true;
                bail!("production node exited before qualification: {result:?}");
            }
        }
        for answer in &answers {
            let evidence = answer.borrow();
            let evidence = evidence
                .as_ref()
                .context("actual HTTP advertisement evidence missing")?;
            ensure!(
                evidence.candidates.contains(&Family::V4.address(port))
                    && evidence.candidates.contains(&Family::V6.address(port)),
                "both actual answers must advertise both IP families"
            );
        }
        anyhow::Ok(())
    }
    .await;
    let cleanup = tokio::time::timeout(CLEANUP, async {
        let closed = futures_lite::future::zip(
            async {
                if let Some(a) = agents.first() {
                    a.close().await?;
                }
                anyhow::Ok(())
            },
            async {
                if let Some(b) = agents.get(1) {
                    b.close().await?;
                }
                anyhow::Ok(())
            },
        )
        .await;
        shutdown.cancel();
        closed.0?;
        closed.1?;
        for agent in &agents {
            retired(
                &agent.snapshot().connection,
                agent.statistics().borrow().is_some(),
            )?;
        }
        if !node_observed {
            (&mut node).await.context("joining owned node")??;
        }
        anyhow::Ok(())
    })
    .await
    .context("FAILED: 10s owned SDK/node cleanup deadline");
    if !matches!(&cleanup, Ok(Ok(()))) {
        shutdown.cancel();
        node.abort();
        let _ = tokio::time::timeout(CLEANUP, async {
            for agent in &agents {
                let _ = agent.abort().await;
            }
        })
        .await;
    }
    cleanup??;
    result?;
    tracing::info!("EXERCISED once-bound listener advertisement and owned SDK/node cleanup");
    Ok(())
}

#[tokio::test]
async fn concurrent_dual_stack_sdk_payloads() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run(Protocol::Udp))
        .await
}

#[tokio::test]
async fn concurrent_dual_stack_tcp_sdk_payloads() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run(Protocol::Tcp))
        .await
}

#[test]
fn answer_evidence_rejects_missing_family_and_preserves_non_candidates() -> Result<()> {
    let answer = "v=0\r\na=ice-ufrag:redacted\r\na=candidate:1 1 udp 1 127.0.0.1 9000 typ host\r\na=candidate:2 1 udp 1 ::1 9000 typ host\r\n";
    let (filtered, _) = constrain_answer(answer, Family::V6, 9000, Protocol::Udp)?;
    ensure!(
        filtered == "v=0\r\na=ice-ufrag:redacted\r\na=candidate:2 1 udp 1 ::1 9000 typ host\r\n",
        "non-candidate SDP changed"
    );
    for invalid in [
        answer.replace("::1", "::"),
        answer.replace("9000", "9001"),
        answer.replace("udp", "tcp"),
        filtered,
    ] {
        ensure!(
            constrain_answer(&invalid, Family::V6, 9000, Protocol::Udp).is_err(),
            "invalid advertisement accepted"
        );
    }
    Ok(())
}

#[test]
fn qualification_rejects_missing_payload_sequential_peers_and_unretired_graphs() -> Result<()> {
    ensure!(
        recipient_payload(None, "v4", A_TO_B).is_err(),
        "connection without payload qualified"
    );
    let mut message = TopicMessage {
        topic: TOPIC.into(),
        mode: TopicMode::Latest,
        data_track_id: "publication".into(),
        sequence: 0,
        participant_id: "v4".into(),
        payload: A_TO_B.to_vec(),
    };
    recipient_payload(Some(&message), "v4", A_TO_B)?;
    message.participant_id = "wrong-sender".into();
    ensure!(
        recipient_payload(Some(&message), "v4", A_TO_B).is_err(),
        "wrong sender qualified"
    );
    message.participant_id = "v4".into();
    message.payload = B_TO_A.to_vec();
    ensure!(
        recipient_payload(Some(&message), "v4", A_TO_B).is_err(),
        "wrong payload qualified"
    );
    live_generation(&ConnectionState::Connected, Some(1), 1)?;
    ensure!(
        live_generation(&ConnectionState::Disconnected, Some(1), 1).is_err(),
        "sequential-only peer qualified"
    );
    ensure!(
        live_generation(&ConnectionState::Connected, Some(2), 1).is_err(),
        "replacement peer qualified"
    );
    retired(&ConnectionState::Disconnected, false)?;
    ensure!(
        retired(&ConnectionState::Connected, false).is_err(),
        "live graph qualified as cleanup"
    );
    ensure!(
        retired(&ConnectionState::Disconnected, true).is_err(),
        "unretired statistics qualified as cleanup"
    );
    Ok(())
}

#[test]
fn selected_pair_rejects_unqualified_reports() -> Result<()> {
    let generation = 1;
    let report = json!([
        {"id":"t", "type":"transport", "selectedCandidatePairId":"p"},
        {"id":"p", "type":"candidate-pair", "state":"succeeded", "localCandidateId":"l", "remoteCandidateId":"r"},
        {"id":"l", "type":"local-candidate", "protocol":"udp", "address":"::1", "port":8000},
        {"id":"r", "type":"remote-candidate", "protocol":"udp", "address":"::1", "port":9000}
    ]);
    let stats = |report: &Value| -> Result<NativeStatistics> {
        Ok(NativeStatistics {
            generation: 1,
            timestamp_us: 1,
            report_json: serde_json::to_vec(report)?,
        })
    };
    selected_pair(
        &stats(&report)?,
        generation,
        Family::V6.address(9000),
        Protocol::Udp,
    )?;
    let mut tcp = report.clone();
    for (index, kind) in [(2, "prflx"), (3, "host")] {
        let candidate = tcp
            .get_mut(index)
            .and_then(Value::as_object_mut)
            .context("test TCP candidate")?;
        candidate.insert("protocol".into(), json!("tcp"));
        candidate.insert("candidateType".into(), json!(kind));
        candidate.insert("tcpType".into(), json!("passive"));
    }
    selected_pair(
        &stats(&tcp)?,
        generation,
        Family::V6.address(9000),
        Protocol::Tcp,
    )?;
    tcp.get_mut(3)
        .and_then(Value::as_object_mut)
        .context("test server TCP candidate")?
        .insert("tcpType".into(), json!("active"));
    ensure!(
        selected_pair(
            &stats(&tcp)?,
            generation,
            Family::V6.address(9000),
            Protocol::Tcp
        )
        .is_err(),
        "non-passive server candidate accepted"
    );
    for (index, key, value) in [
        (0, "selectedCandidatePairId", Value::Null),
        (0, "selectedCandidatePairId", json!("missing")),
        (1, "state", json!("in-progress")),
        (1, "remoteCandidateId", json!("missing")),
        (2, "address", json!("127.0.0.1")),
        (3, "address", Value::Null),
        (3, "address", json!("127.0.0.1")),
        (3, "protocol", json!("tcp")),
        (3, "port", json!(9001)),
    ] {
        let mut invalid = report.clone();
        *invalid
            .get_mut(index)
            .context("test report record")?
            .get_mut(key)
            .context("test report field")? = value;
        ensure!(
            selected_pair(
                &stats(&invalid)?,
                generation,
                Family::V6.address(9000),
                Protocol::Udp
            )
            .is_err(),
            "invalid selected-pair evidence accepted"
        );
    }
    let mut stale = stats(&report)?;
    stale.generation = 2;
    ensure!(
        selected_pair(&stale, generation, Family::V6.address(9000), Protocol::Udp).is_err(),
        "stale generation accepted"
    );
    Ok(())
}
