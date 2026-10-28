//! Public SDK controls shared unchanged by the finite SFU variants.
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use pulsebeam_agent_core::ffi as api;
use pulsebeam_agent_native::AsyncHttpClient;
use pulsebeam_agent_native::{
    Agent as RuntimeAgent, Config, Host,
    ffi::{Agent, EventUpdate, NativeEvent, SnapshotUpdate},
};
use pulsebeam_core::net::{HttpRequest, HttpResult};

use super::common::{
    client::create_http_client, native::NativeHost, reserve_subnet, run_sim_or_timeout, sim_seed,
    start_sfu_node_with, subnet_ip,
};

fn evidence(claim: &str, event: &str, details: serde_json::Value) {
    use std::io::Write;
    let directory = std::env::var("TEST_UNDECLARED_OUTPUTS_DIR").expect("isolated runner outputs");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&directory).join("calibration.jsonl"))
        .expect("calibration evidence");
    writeln!(
        file,
        "{}",
        serde_json::json!({"claim": claim, "event": event, "details": details})
    )
    .expect("retain calibration evidence");
}

struct HttpWitness {
    inner: Box<dyn AsyncHttpClient>,
    statuses: Arc<Mutex<Vec<(String, u16)>>>,
}
impl AsyncHttpClient for HttpWitness {
    fn execute(&self, request: HttpRequest) -> HttpResult<'_> {
        let admission = request.method() == http::Method::POST;
        let uri = request.uri().to_string();
        Box::pin(async move {
            let response = self.inner.execute(request).await?;
            if admission {
                self.statuses
                    .lock()
                    .unwrap()
                    .push((uri, response.status().as_u16()));
            }
            Ok(response)
        })
    }
}

struct Peer {
    agent: Arc<Agent>,
    statuses: Arc<Mutex<Vec<(String, u16)>>>,
    name: String,
    publishes: bool,
}

const TOPIC: &str = "calibration-origin";
fn publisher() -> api::TopicPublisher {
    api::TopicPublisher {
        name: TOPIC.into(),
        mode: api::TopicMode::Lossy,
    }
}

async fn peer(ip: IpAddr, server: IpAddr, name: &str, publishes: bool) -> anyhow::Result<Peer> {
    let session = pulsebeam_agent_core::AgentConfig {
        endpoint: format!("http://{server}:7070"),
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            "calibration",
            name,
            u64::MAX,
        )?,
        topology: pulsebeam_agent_core::MediaTopology {
            local_video: 0,
            local_audio: 0,
            remote_video: 0,
            remote_audio: 0,
        },
        retry: pulsebeam_agent_core::RetryPolicy::default(),
        log_level: pulsebeam_agent_core::LogLevel::default(),
    };
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let http = HttpWitness {
        inner: create_http_client(),
        statuses: statuses.clone(),
    };
    let host = NativeHost::bind(ip)?;
    let (runtime, actor) = RuntimeAgent::managed_with_services(
        Config::new(session),
        Host::new(Box::new(http)),
        move || host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime);
    agent
        .replace_desired(api::DesiredState {
            connected: true,
            topic_publishers: if publishes { vec![publisher()] } else { vec![] },
            topic_subscribers: if publishes {
                vec![]
            } else {
                vec![api::TopicSubscriber {
                    name: TOPIC.into(),
                    mode: api::TopicMode::Lossy,
                }]
            },
            ..Default::default()
        })
        .await?;
    Ok(Peer {
        agent,
        statuses,
        name: name.into(),
        publishes,
    })
}

async fn ready(peer: &Peer) -> anyhow::Result<api::Snapshot> {
    let snapshots = peer.agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("snapshot stream closed")
            };
            if snapshot.connection == api::ConnectionState::Connected
                && snapshot.participant_id.as_deref() == Some(peer.name.as_str())
                && snapshot.topics.publishers.len() == usize::from(peer.publishes)
                && snapshot.topics.subscribers.len() == usize::from(!peer.publishes)
                && snapshot
                    .topics
                    .publishers
                    .iter()
                    .all(|p| p.publisher == publisher() && p.connected && p.data_track_id.is_some())
                && snapshot.topics.subscribers.iter().all(|s| {
                    s.subscriber.name == TOPIC
                        && s.subscriber.mode == api::TopicMode::Lossy
                        && s.connected
                        && s.publishers == 1
                })
            {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("SDK readiness deadline"))?
}

/// Receipt is an actual public event with exact origin, topic, mode and payload.
async fn delivery(
    claim: &str,
    source: &Peer,
    origin: &str,
    recipients: &[&Peer],
    phase: &str,
) -> anyhow::Result<()> {
    let events: Vec<_> = recipients.iter().map(|p| p.agent.events()).collect();
    let payload = format!("{claim}/{phase}/source-input").into_bytes();
    evidence(
        claim,
        "request",
        serde_json::json!({"origin": origin, "topic": TOPIC, "mode": "lossy", "payload": payload, "recipients": recipients.len(), "recipient_ids": recipients.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), "phase": phase}),
    );
    let mut received = vec![false; recipients.len()];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        source
            .agent
            .send_topic(publisher(), payload.clone())
            .await?;
        for (index, events) in events.iter().enumerate() {
            if received[index] {
                continue;
            }
            if let Ok(EventUpdate::Event {
                event:
                    NativeEvent::Core {
                        notification:
                            api::Notification::Topic {
                                notification: api::TopicNotification::Message { message },
                            },
                    },
            }) = tokio::time::timeout(Duration::from_millis(20), events.next()).await
            {
                if message.participant_id == origin
                    && message.name == TOPIC
                    && message.mode == api::TopicMode::Lossy
                    && message.payload == payload
                {
                    received[index] = true;
                    evidence(
                        claim,
                        "receipt",
                        serde_json::json!({"recipient": index, "recipient_id": recipients[index].name, "origin": message.participant_id, "payload": message.payload, "phase": phase}),
                    );
                }
            }
        }
        if received.iter().all(|value| *value) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    evidence(
        claim,
        "violation",
        serde_json::json!({"boundary": "public", "reason": "delivery-deadline", "received": received, "phase": phase}),
    );
    anyhow::bail!("public delivery deadline: {claim}/{phase}: {received:?}")
}

fn run(claim: &'static str, shards: usize, admission: bool) {
    let seed = sim_seed();
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server = subnet_ip(subnet, 1);
    let client = subnet_ip(subnet, 2);
    sim.host(server, move || async move {
        start_sfu_node_with(
            server,
            pulsebeam_runtime::rand::seeded_rng(seed),
            shards,
            false,
        )
        .await
        .map_err(Into::into)
    });
    sim.client(client, async move {
        let result: anyhow::Result<()> = async {
        let source = peer(client, server, "source", true).await?;
        let origin = ready(&source).await?.participant_id.ok_or_else(|| anyhow::anyhow!("source identity missing"))?;
        let first = peer(client, server, "first", false).await?;
        ready(&first).await?;
        if admission {
            delivery(claim, &source, &origin, &[&first], "before-fault").await?;
            let before = pulsebeam::sim_metrics::routing_counter("materialization_failed");
            pulsebeam::sim_metrics::fail_next_materialization();
            let retrying = peer(client, server, "retrying", false).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while retrying.statuses.lock().unwrap().is_empty() { tokio::time::sleep(Duration::from_millis(1)).await; }
            }).await?;
            let consumed = pulsebeam::sim_metrics::routing_counter("materialization_failed") - before;
            anyhow::ensure!(consumed == 1, "one-shot materialization fault not consumed exactly once");
            evidence(claim, "premise", serde_json::json!({"fault": "materialization_failed", "consumed": consumed}));
            let (uri, status) = retrying.statuses.lock().unwrap()[0].clone();
            evidence(claim, "http", serde_json::json!({"method": "POST", "uri": uri, "status": status}));
            if status != 503 {
                evidence(claim, "violation", serde_json::json!({"boundary": "public", "reason": "admission-status", "expected": 503, "actual": status}));
                anyhow::bail!("negative materialization acknowledgement returned HTTP {status}, expected 503");
            }
            delivery(claim, &source, &origin, &[&first], "unrelated-service").await?;
            ready(&retrying).await?;
            anyhow::ensure!(retrying.statuses.lock().unwrap().iter().any(|(_, status)| *status == 201), "SDK did not retry admission successfully");
            evidence(claim, "retry", serde_json::json!({"statuses": retrying.statuses.lock().unwrap().iter().map(|(_, status)| *status).collect::<Vec<_>>()}));
            delivery(claim, &source, &origin, &[&first, &retrying], "after-retry").await?;
            retrying.agent.close().await?;
        } else {
            let second = peer(client, server, "second", false).await?;
            ready(&second).await?;
            evidence(claim, "premise", serde_json::json!({"shards": shards, "sequential_participants": 3, "intended_recipients": 2}));
            delivery(claim, &source, &origin, &[&first, &second], "fanout").await?;
            second.agent.close().await?;
        }
        first.agent.close().await?;
        source.agent.close().await?;
        evidence(claim, "pass", serde_json::json!({}));
        Ok(())
        }.await;
        result.map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(120)).expect("calibration public control");
}

#[test]
fn local_fanout() {
    run("local-fanout", 1, false);
}
#[test]
fn remote_fanout() {
    run("remote-fanout", 3, false);
}
#[test]
fn materialization() {
    run("materialization", 1, true);
}
