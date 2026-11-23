//! Controlled Rust transport controls shared unchanged by the finite SFU variants.
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use pulsebeam_agent_core::{DataEvent, ffi as api};
use pulsebeam_agent_native::AsyncHttpClient;
use pulsebeam_agent_native::{
    Agent as RuntimeAgent, Config, Host,
    ffi::{Agent, DataPublication, DataSubscription},
};
use pulsebeam_core::net::{HttpRequest, HttpResult};

use super::common::{
    client::create_http_client, native::NativeHost, reserve_subnet, run_sim_or_timeout,
    sfu_node_builder, sim_seed, subnet_ip,
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
        let admission =
            request.method() == http::Method::POST && request.uri().path() == "/api/v1/native";
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
    data: DataResource,
}

enum DataResource {
    Publisher(Arc<DataPublication>),
    Subscriber(Arc<DataSubscription>),
}

impl DataResource {
    fn installed(&self) -> bool {
        let execution = match self {
            Self::Publisher(publication) => publication.state().execution,
            Self::Subscriber(subscription) => subscription.state().execution,
        };
        execution == api::PublicationExecution::Installed
    }
}

const ROOM: &str = "calibration";
const TOPIC: &str = "calibration-origin";

async fn peer(ip: IpAddr, server: IpAddr, name: &str, publishes: bool) -> anyhow::Result<Peer> {
    let session = pulsebeam_agent_core::AgentConfig {
        endpoint: format!("http://{server}:7070"),
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &serde_json::from_value(serde_json::json!([{"rooms":["calibration"],"allow":["join","publish","discover","subscribe"]}])).unwrap(),
            name,
            u64::try_from(i64::MAX)?,
        )?.token,
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
    let delivery = api::DataDelivery::Unreliable;
    let data = if publishes {
        DataResource::Publisher(agent.data(TOPIC.into())?.publish(Some(
            api::DataPublishOptions {
                delivery: Some(delivery),
                ..Default::default()
            },
        ))?)
    } else {
        DataResource::Subscriber(
            agent
                .room(ROOM.into())?
                .participant("source".into())?
                .data(TOPIC.into())?
                .subscribe(Some(api::DataSubscribeOptions {
                    delivery: Some(delivery),
                }))?,
        )
    };
    agent.join(ROOM.into())?;
    Ok(Peer {
        agent,
        statuses,
        name: name.into(),
        data,
    })
}

async fn ready(peer: &Peer) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = peer.agent.snapshot();
            if snapshot.connection == api::ConnectionState::Connected
                && snapshot.participant_id.as_deref() == Some(peer.name.as_str())
                && peer.data.installed()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("SDK readiness deadline for {}", peer.name))
}

/// Receipt is an actual public event with exact origin, topic, mode and payload.
async fn delivery(
    claim: &str,
    source: &Peer,
    recipients: &[&Peer],
    phase: &str,
) -> anyhow::Result<()> {
    let origin = source.name.as_str();
    let DataResource::Publisher(publication) = &source.data else {
        anyhow::bail!("calibration source has no publication");
    };
    let payload = format!("{claim}/{phase}/source-input").into_bytes();
    evidence(
        claim,
        "request",
        serde_json::json!({"origin": origin, "topic": TOPIC, "mode": "lossy", "payload": payload, "recipients": recipients.len(), "recipient_ids": recipients.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), "phase": phase}),
    );
    let mut received = vec![false; recipients.len()];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        publication.send(payload.clone(), None).await?;
        for (index, recipient) in recipients.iter().enumerate() {
            if received[index] {
                continue;
            }
            let DataResource::Subscriber(subscription) = &recipient.data else {
                anyhow::bail!("calibration recipient has no subscription");
            };
            if let Ok(update) =
                tokio::time::timeout(Duration::from_millis(20), subscription.next()).await
                && let Some(DataEvent::Message {
                    participant_id,
                    publication_id,
                    data,
                    ..
                }) = update?
                && participant_id == origin
                && subscription.state().source_track_id.as_ref() == Some(&publication_id)
                && data == payload
            {
                received[index] = true;
                evidence(
                    claim,
                    "receipt",
                    serde_json::json!({"recipient": index, "recipient_id": recipient.name, "origin": participant_id, "payload": data, "phase": phase}),
                );
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
        sfu_node_builder(server, pulsebeam_runtime::rand::seeded_rng(seed), shards)
            .with_local_data_v1()
            .run(tokio_util::sync::CancellationToken::new())
            .await
            .map_err(Into::into)
    });
    sim.client(client, async move {
        let result: anyhow::Result<()> = async {
        let source = peer(client, server, "source", true).await?;
        ready(&source).await?;
        let first = peer(client, server, "first", false).await?;
        ready(&first).await?;
        if admission {
            delivery(claim, &source, &[&first], "before-fault").await?;
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
            delivery(claim, &source, &[&first], "unrelated-service").await?;
            ready(&retrying).await?;
            anyhow::ensure!(retrying.statuses.lock().unwrap().iter().any(|(_, status)| *status == 201), "SDK did not retry admission successfully");
            evidence(claim, "retry", serde_json::json!({"statuses": retrying.statuses.lock().unwrap().iter().map(|(_, status)| *status).collect::<Vec<_>>()}));
            delivery(claim, &source, &[&first, &retrying], "after-retry").await?;
            retrying.agent.close().await?;
        } else {
            let second = peer(client, server, "second", false).await?;
            ready(&second).await?;
            evidence(claim, "premise", serde_json::json!({"shards": shards, "sequential_participants": 3, "intended_recipients": 2}));
            delivery(claim, &source, &[&first, &second], "fanout").await?;
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
