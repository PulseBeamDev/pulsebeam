//! Real dual-stack UDP and isolated ICE-TCP SDK/application witnesses.
use super::*;
use anyhow::{anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DataEvent, DataUpdate, Generation, TopicMode,
    ffi::PublicationExecution,
};
use pulsebeam_agent_native::{
    Agent, AsyncHttpClient, Config, DataPublication, DataReceiver, Host, NativeStatistics,
};
use pulsebeam_runtime::{sync::watch, time::Instant};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "transport_conformance/quic.rs"]
mod quic;

const CONNECT: Duration = Duration::from_secs(30);
const DELIVERY: Duration = Duration::from_secs(30);
const CLEANUP: Duration = Duration::from_secs(10);
const PROTECTED_UDP_CEILING: u16 = pulsebeam_core::fabric::MAX_UDP_PAYLOAD_BYTES;
const ROOM: &str = "dual-stack-transport";
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

#[derive(Clone, Copy)]
enum DataWitness {
    LiveUdp { cross_shard: bool },
    ReliableUdp { cross_shard: bool },
    LiveTcp,
}

impl DataWitness {
    fn needs_migration(self) -> bool {
        matches!(self, Self::LiveUdp { cross_shard: false } | Self::LiveTcp)
    }

    fn crosses_shards(self) -> bool {
        matches!(
            self,
            Self::LiveUdp { cross_shard: true } | Self::ReliableUdp { cross_shard: true }
        )
    }

    fn protocol(self) -> Protocol {
        match self {
            Self::LiveUdp { .. } | Self::ReliableUdp { .. } => Protocol::Udp,
            Self::LiveTcp => Protocol::Tcp,
        }
    }

    fn topic_mode(self) -> TopicMode {
        match self {
            Self::LiveUdp { .. } | Self::LiveTcp => TopicMode::Latest,
            Self::ReliableUdp { .. } => TopicMode::Ordered,
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

// Retain candidate addresses and the logical shard, never credentials or SDP.
#[derive(Clone, Debug)]
struct AnswerEvidence {
    candidates: Vec<SocketAddr>,
    shard: Option<ShardId>,
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
    let shard = answer
        .lines()
        .filter_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .find_map(crate::control::ufrag::IceUfrag::decode)
        .map(|ufrag| ufrag.node_address().shard());
    Ok((filtered.join("\r\n"), AnswerEvidence { candidates, shard }))
}

struct FamilyHttp {
    client: reqwest::Client,
    family: Family,
    port: u16,
    witness: DataWitness,
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
            if body.get("data_version").and_then(Value::as_u64)
                != Some(u64::from(pulsebeam_proto::data_endpoint::DATA_V1))
            {
                return Err("native conformance requires DATA_V1 negotiation".into());
            }
            let deliveries = body
                .get("data_delivery")
                .and_then(Value::as_array)
                .context("native DATA delivery advertisement missing")?;
            let expected = ["LATEST", "LOSSY", "ORDERED"];
            if deliveries.len() != expected.len()
                || !expected.into_iter().all(|expected| {
                    deliveries
                        .iter()
                        .any(|mode| mode.as_str() == Some(expected))
                })
            {
                return Err("native DATA must advertise all three default delivery modes".into());
            }
            let answer = body
                .get("answer")
                .and_then(Value::as_str)
                .context("native HTTP answer missing")?;
            let (filtered, evidence) =
                constrain_answer(answer, self.family, self.port, self.witness.protocol())?;
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

struct DataLeg {
    sender: DataPublication,
    receiver: DataReceiver,
}

impl DataLeg {
    fn open(agent: &Agent, remote: &str, topic: &str, mode: TopicMode) -> Result<Self> {
        Ok(Self {
            sender: agent.data(topic.into(), mode)?.publish()?,
            receiver: agent
                .room(ROOM.into())?
                .participant(remote.into())?
                .data(topic.into(), mode)?
                .subscribe()?,
        })
    }

    async fn ready(&self) -> Result<()> {
        pulsebeam_runtime::try_join!(
            async {
                let updates = self
                    .sender
                    .updates()
                    .map_err(|_| anyhow!("DATA publisher observer limit"))?;
                while let Some(state) = updates
                    .next()
                    .map_err(|_| anyhow!("concurrent DATA publisher observation"))?
                    .await
                {
                    ensure!(
                        state.error.is_none() && state.execution_error.is_none(),
                        "DATA publisher was rejected: {state:?}"
                    );
                    if state.execution == PublicationExecution::Installed {
                        return Ok(());
                    }
                }
                bail!("DATA publisher closed before installation");
            },
            async {
                let updates = self
                    .receiver
                    .updates()
                    .map_err(|_| anyhow!("DATA receiver observer limit"))?;
                while let Some(state) = updates
                    .next()
                    .map_err(|_| anyhow!("concurrent DATA receiver observation"))?
                    .await
                {
                    ensure!(
                        state.error.is_none()
                            && state.execution_error.is_none()
                            && state.terminal_error.is_none(),
                        "DATA receiver was rejected: {state:?}"
                    );
                    if state.execution == PublicationExecution::Installed {
                        return Ok(());
                    }
                }
                bail!("DATA receiver closed before installation");
            }
        )?;
        Ok(())
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
        if snapshot.connection == ConnectionState::Connected
            && let (Some(generation), Some(stats)) =
                (snapshot.generation, statistics.borrow().as_ref())
            && let Ok(pair) = selected_pair(stats, generation.get(), remote, protocol)
            && prior.is_none_or(|prior| pair.timestamp_us > prior.timestamp_us)
        {
            return Ok(pair);
        }
        pulsebeam_runtime::select! {
            result = snapshots.changed() => { result.context("SDK snapshots closed before readiness")?; }
            result = statistics.changed() => { result.context("SDK statistics closed before selected-pair evidence")?; }
        }
    }
}

fn recipient_payload(event: &DataEvent, sender: &str, expected: &[u8]) -> Result<()> {
    let DataEvent::Message {
        participant_id,
        publication_id,
        data,
        ..
    } = event
    else {
        bail!("expected a complete DATA unit");
    };
    ensure!(
        participant_id == sender && !publication_id.is_empty() && data == expected,
        "recipient application sender/publication/payload mismatch"
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

async fn received(receiver: &DataReceiver, sender: &str, expected: &[u8]) -> Result<()> {
    let DataUpdate::Event { event } = receiver.next().await? else {
        bail!("DATA receiver closed before delivery");
    };
    recipient_payload(&event, sender, expected)?;
    tracing::info!(
        bytes = expected.len(),
        "EXERCISED recipient application DATA unit"
    );
    Ok(())
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
            &serde_json::from_value(
                serde_json::json!([{"rooms":["dual-stack-transport"],"allow":["join","publish","discover","subscribe"]}]),
            )?,
            participant,
            expiry,
        )?.token,
        retry: Default::default(),
        log_level: Default::default(),
    };
    session.retry.maximum_attempts = 1;
    Ok(Config::new(session))
}

async fn witness(
    a: &Agent,
    b: &Agent,
    port: u16,
    candidate: DataWitness,
    migration: Option<&pulsebeam_runtime::rt::MigrationHandle>,
) -> Result<()> {
    let protocol = candidate.protocol();
    let mode = candidate.topic_mode();
    let started = Instant::now();
    let (pair_a, pair_b, sender_a, sender_b) = pulsebeam_runtime::time::timeout(CONNECT, async {
        pulsebeam_runtime::try_join!(a.connect(), b.connect())?;
        a.join(ROOM.to_owned())?;
        b.join(ROOM.to_owned())?;
        let (pair_a, pair_b) = pulsebeam_runtime::try_join!(
            ready(a, Family::V4.address(port), None, protocol),
            ready(b, Family::V6.address(port), None, protocol),
        )?;
        let sender_a = a
            .snapshot()
            .participant_id
            .context("IPv4 participant ID missing")?;
        let sender_b = b
            .snapshot()
            .participant_id
            .context("IPv6 participant ID missing")?;
        anyhow::Ok((pair_a, pair_b, sender_a, sender_b))
    })
    .await
    .context("dual-stack connection or selected-pair deadline")??;
    let data_a = DataLeg::open(a, &sender_b, TOPIC, mode)?;
    let data_b = DataLeg::open(b, &sender_a, TOPIC, mode)?;
    let remaining = CONNECT.saturating_sub(Instant::now().saturating_duration_since(started));
    pulsebeam_runtime::time::timeout(remaining, async {
        pulsebeam_runtime::try_join!(data_a.ready(), data_b.ready())
    }).await.with_context(|| {
        let pub_a = data_a.sender.state();
        let pub_b = data_b.sender.state();
        let sub_a = data_a.receiver.state();
        let sub_b = data_b.receiver.state();
        format!(
            "{mode:?} DATA installation deadline: IPv4 publisher={:?}/{:?} subscriber={:?}/{:?}/{:?}; IPv6 publisher={:?}/{:?} subscriber={:?}/{:?}/{:?}",
            pub_a.admission, pub_a.execution, sub_a.source_status, sub_a.admission, sub_a.execution,
            pub_b.admission, pub_b.execution, sub_b.source_status, sub_b.admission, sub_b.execution,
        )
    })??;
    tracing::info!(
        ?protocol,
        ?mode,
        "EXERCISED both selected families and owned DATA readiness"
    );
    pulsebeam_runtime::time::timeout(DELIVERY, async {
        pulsebeam_runtime::try_join!(
            data_a.sender.send(A_TO_B.to_vec()),
            data_b.sender.send(B_TO_A.to_vec()),
        )?;
        pulsebeam_runtime::try_join!(
            received(&data_b.receiver, &sender_a, A_TO_B),
            received(&data_a.receiver, &sender_b, B_TO_A)
        )?;
        tracing::info!("EXERCISED both recipient application payloads");
        let lossy = if matches!(candidate, DataWitness::LiveUdp { .. }) {
            const LOSSY_TOPIC: &str = "dual-stack-lossy";
            let leg_a = DataLeg::open(a, &sender_b, LOSSY_TOPIC, TopicMode::Lossy)?;
            let leg_b = DataLeg::open(b, &sender_a, LOSSY_TOPIC, TopicMode::Lossy)?;
            tracing::info!("waiting for both Lossy DATA routes before migration");
            pulsebeam_runtime::try_join!(
                async { leg_a.ready().await.context("IPv4 Lossy DATA route") },
                async { leg_b.ready().await.context("IPv6 Lossy DATA route") },
            )?;
            Some((leg_a, leg_b))
        } else {
            None
        };
        if let Some(migration) = migration {
            let during_a = b"IPv4 application delivery during shard handoff";
            let during_b = b"IPv6 application delivery during shard handoff";
            pulsebeam_runtime::try_join!(
                async {
                    tracing::info!(
                        ?protocol,
                        service = 0,
                        destination = 1,
                        "requesting managed-shard move"
                    );
                    migration
                        .move_service(0, 1)
                        .await
                        .context("first managed-shard move")?;
                    tracing::info!(
                        ?protocol,
                        service = 0,
                        destination = 1,
                        "completed managed-shard move"
                    );
                    migration
                        .move_service(1, 0)
                        .await
                        .context("second managed-shard move")?;
                    tracing::info!(
                        ?protocol,
                        service = 1,
                        destination = 0,
                        "completed managed-shard move"
                    );
                    anyhow::Ok(())
                },
                async {
                    data_a.sender.send(during_a.to_vec()).await?;
                    received(&data_b.receiver, &sender_a, during_a).await?;
                    tracing::info!(?protocol, "EXERCISED IPv4-to-IPv6 delivery during handoff");
                    anyhow::Ok(())
                },
                async {
                    data_b.sender.send(during_b.to_vec()).await?;
                    received(&data_a.receiver, &sender_b, during_b).await?;
                    tracing::info!(?protocol, "EXERCISED IPv6-to-IPv4 delivery during handoff");
                    anyhow::Ok(())
                },
            )?;
        }
        let max_a = [0x5a; pulsebeam_proto::data_record::FRAGMENT_BYTES];
        let max_b = [0xa5; pulsebeam_proto::data_record::FRAGMENT_BYTES];
        pulsebeam_runtime::try_join!(
            data_a.sender.send(max_a.to_vec()),
            data_b.sender.send(max_b.to_vec()),
        )
        .context("send maximum-fragment DATA on the selected pair")?;
        pulsebeam_runtime::try_join!(
            received(&data_b.receiver, &sender_a, &max_a),
            received(&data_a.receiver, &sender_b, &max_b)
        )
        .context("receive maximum-fragment DATA on both families")?;
        if let Some((leg_a, leg_b)) = &lossy {
            pulsebeam_runtime::try_join!(
                leg_a.sender.send(max_a.to_vec()),
                leg_b.sender.send(max_b.to_vec()),
            )
            .context("send maximum-fragment Lossy DATA")?;
            pulsebeam_runtime::try_join!(
                received(&leg_b.receiver, &sender_a, &max_a),
                received(&leg_a.receiver, &sender_b, &max_b),
            )
            .context("receive maximum-fragment Lossy DATA")?;
            tracing::info!("EXERCISED capped maximum-fragment Lossy DATA on both families");
        }
        if migration.is_some() {
            tracing::info!(
                "EXERCISED two completed same-future adaptive moves and fresh delivery on both families"
            );
        } else {
            tracing::info!(
                cross_shard = candidate.crosses_shards(),
                ?mode,
                "EXERCISED capped maximum-fragment DATA on the selected pair"
            );
        }
        pulsebeam_runtime::try_join!(
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

async fn run(candidate: DataWitness) -> Result<()> {
    use pulsebeam_core::auth::{
        DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
        KeyRegistryEntry,
    };
    let protocol = candidate.protocol();
    let bind = |addr| {
        let mut options = pulsebeam_udp::BindOptions::new(addr);
        options.reuse_port = true;
        options.max_datagram_bytes = pulsebeam_udp::MAX_DATAGRAM_BYTES;
        if protocol == Protocol::Udp {
            options.nonfragmenting_max_datagram_bytes =
                std::num::NonZeroU16::new(PROTECTED_UDP_CEILING);
        }
        pulsebeam_udp::bind(options)
    };
    let first = bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0))
        .await
        .context("bind dual-stack UDP conformance listener with the required socket policy")?;
    let port = first.local_addr().port();
    let mut sockets = vec![first];
    if candidate.needs_migration() || candidate.crosses_shards() {
        sockets.push(
            bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port))
                .await
                .context("bind second conformance listener with the same socket policy")?,
        );
    }
    let http = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", http.local_addr()?);
    let (control, runtime_control) = pulsebeam_runtime::sync::oneshot::channel();
    let mut builder = NodeBuilder::new()
        .data_threads(if candidate.crosses_shards() {
            1
        } else {
            sockets.len()
        })
        .adaptive(if candidate.crosses_shards() { 2 } else { 1 })
        .room_partition_size(
            if candidate.needs_migration() || candidate.crosses_shards() {
                1
            } else {
                2
            },
        )
        .round_robin_rooms()
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
    let runtime_control = if candidate.needs_migration() {
        builder = builder.with_runtime_control(control);
        Some(runtime_control)
    } else {
        None
    };
    let builder = match protocol {
        Protocol::Udp => builder.udp_only(),
        Protocol::Tcp => builder.tcp_only(),
    };
    let advertised = builder.advertised_udp_addrs()?;
    let shutdown = CancellationToken::new();
    let mut node = pulsebeam_runtime::task::spawn_local(builder.run_bound(
        sockets,
        advertised,
        shutdown.clone(),
        crate::control::controller::OwnershipMode::Standalone,
        None,
    ));
    let mut node_observed = false;
    let mut agents = Vec::new();
    let mut answers = Vec::new();
    let result = async {
        let migration = pulsebeam_runtime::time::timeout(CONNECT, async {
            for (family, name) in [(Family::V4, "v4"), (Family::V6, "v6")] {
                let (answer, evidence) = watch::channel(None);
                let agent = Agent::spawn(
                    config(&endpoint, name)?,
                    Host::new(Box::new(FamilyHttp {
                        client: reqwest::Client::new(),
                        family,
                        port,
                        witness: candidate,
                        answer,
                    })),
                )
                .await?;
                agents.push(agent);
                answers.push(evidence);
            }
            let migration = match runtime_control {
                Some(control) => Some(control.await.context("adaptive runtime control missing")?),
                None => None,
            };
            anyhow::Ok(migration)
        })
        .await
        .context("FAILED: 30s owned SDK/node startup deadline")??;
        let a = agents.first().context("IPv4 agent missing")?;
        let b = agents.get(1).context("IPv6 agent missing")?;
        pulsebeam_runtime::select! {
            result = witness(a, b, port, candidate, migration.as_ref()) => result?,
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
        if !candidate.needs_migration() {
            let [a, b] = answers.as_slice() else {
                bail!("expected both actual Native HTTP answers");
            };
            let shard = |answer: &watch::Receiver<Option<AnswerEvidence>>| {
                answer.borrow().as_ref().and_then(|evidence| evidence.shard)
            };
            let a = shard(a).context("IPv4 answer missing the allocated transport shard")?;
            let b = shard(b).context("IPv6 answer missing the allocated transport shard")?;
            ensure!(
                (a != b) == candidate.crosses_shards(),
                "unexpected DATA placement: {a:?}, {b:?}"
            );
        }
        anyhow::Ok(())
    }
    .await;
    let cleanup = pulsebeam_runtime::time::timeout(CLEANUP, async {
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
        let _ = pulsebeam_runtime::time::timeout(CLEANUP, async {
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

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn concurrent_dual_stack_sdk_payloads() -> Result<()> {
    pulsebeam_runtime::task::LocalSet::new()
        .run_until(run(DataWitness::LiveUdp { cross_shard: false }))
        .await
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn cross_shard_dual_stack_latest_and_lossy_sdk_payloads() -> Result<()> {
    pulsebeam_runtime::task::LocalSet::new()
        .run_until(run(DataWitness::LiveUdp { cross_shard: true }))
        .await
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn same_shard_dual_stack_ordered_sdk_payloads() -> Result<()> {
    pulsebeam_runtime::task::LocalSet::new()
        .run_until(run(DataWitness::ReliableUdp { cross_shard: false }))
        .await
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn cross_shard_dual_stack_ordered_sdk_payloads() -> Result<()> {
    pulsebeam_runtime::task::LocalSet::new()
        .run_until(run(DataWitness::ReliableUdp { cross_shard: true }))
        .await
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn concurrent_dual_stack_tcp_sdk_payloads() -> Result<()> {
    pulsebeam_runtime::task::LocalSet::new()
        .run_until(run(DataWitness::LiveTcp))
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
    let gap = DataEvent::Gap {
        participant_id: "v4".into(),
        publication_id: "publication".into(),
        from: 0,
        to: 1,
    };
    ensure!(
        recipient_payload(&gap, "v4", A_TO_B).is_err(),
        "gap without a complete DATA unit qualified"
    );
    let message = DataEvent::Message {
        publication_id: "publication".into(),
        sequence: 0,
        participant_id: "v4".into(),
        data: A_TO_B.to_vec(),
        captured_at: None,
    };
    recipient_payload(&message, "v4", A_TO_B)?;
    ensure!(
        recipient_payload(&message, "v6", A_TO_B).is_err(),
        "wrong sender qualified"
    );
    ensure!(
        recipient_payload(&message, "v4", B_TO_A).is_err(),
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
