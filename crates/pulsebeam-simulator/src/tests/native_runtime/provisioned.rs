use super::*;

use std::{
    fs::File,
    io::{Read, Seek},
    num::NonZeroU64,
    path::PathBuf,
    process::{Command, Stdio},
};

use pulsebeam::cluster::{Options, ProvisionedPeer, Topology, prepare_provisioned_fixture};
use pulsebeam_core::{auth::ApiSigningKey, identity::NodeId};
use pulsebeam_link::{NetworkClass, NetworkPeer, PeerRole, SecretCredential};
use serde_json::{Value, json};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[test]
fn provisioned_native_cluster_relays_payload_and_reconstructs_incident() -> anyhow::Result<()> {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let [publisher_ip, viewer_ip, client_ip] = [1, 2, 3].map(|host| subnet_ip(subnet, host));
    let root = tempfile::tempdir()?;
    let left_dir = root.path().join("left");
    let right_dir = root.path().join("right");
    let left_id = prepare_provisioned_fixture(&left_dir)?;
    let right_id = prepare_provisioned_fixture(&right_dir)?;
    let left_transport = SecretCredential::generate().map_err(relay_error)?;
    let right_transport = SecretCredential::generate().map_err(relay_error)?;
    let left_receipt = ApiSigningKey::from_seed([61; 32]);
    let right_receipt = ApiSigningKey::from_seed([62; 32]);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(25))
        .rng_seed(seed)
        .build();
    for (ip, gossip_dir, local, remote, transport, receipt_key, peer_receipt_key) in [
        (
            publisher_ip,
            left_dir,
            left_id,
            right_id,
            NetworkPeer {
                endpoint: (viewer_ip, 7443).into(),
                credential: left_transport.clone(),
                expected_peer: right_transport.fingerprint(),
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            },
            left_receipt.clone(),
            right_receipt.public_key(),
        ),
        (
            viewer_ip,
            right_dir,
            right_id,
            left_id,
            NetworkPeer {
                endpoint: (publisher_ip, 7443).into(),
                credential: right_transport,
                expected_peer: left_transport.fingerprint(),
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
            },
            right_receipt,
            left_receipt.public_key(),
        ),
    ] {
        let gossip = (ip, 7946).into();
        let options = Options {
            cluster_id: "provisioned-witness".into(),
            topology: Topology::MultiNode,
            listen: gossip,
            advertise: gossip,
            seeds: Vec::new(),
            data_dir: gossip_dir,
            ownership_peer: Some(ProvisionedPeer {
                listen: (ip, 7443).into(),
                local,
                remote,
                transport,
                authority_epoch: [91; 16],
                receipt_key,
                peer_receipt_key,
            }),
        };
        sim.host(ip, move || {
            let options = options.clone();
            async move {
                sfu_node_builder(ip, pulsebeam_runtime::rand::seeded_rng(seed), 1)
                    .with_cluster(options)
                    .run(tokio_util::sync::CancellationToken::new())
                    .await
                    .map_err(Into::into)
            }
        });
    }
    let witness = Arc::new(Mutex::new(None));
    let capture = witness.clone();
    sim.client(client_ip, async move {
        let keys = prove_payload(client_ip, publisher_ip, viewer_ip).await?;
        *capture.lock().unwrap() = Some(keys);
        Ok(())
    });
    let mut logs = tempfile::tempfile()?;
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(logs.try_clone()?)
            .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                meta.target() == "pulsebeam::incident"
            })),
    );
    tracing::subscriber::with_default(subscriber, || {
        run_sim_or_timeout(&mut sim, Duration::from_secs(20))
    })
    .map_err(|error| anyhow::anyhow!("provisioned simulation failed: {error:#}"))?;
    let keys = witness
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing production witness"))?;
    check_incident(&mut logs, &keys, left_id, right_id)?;
    println!("[provisioned-incident] {keys}");
    Ok(())
}

async fn prove_payload(
    client_ip: IpAddr,
    publisher_ip: IpAddr,
    viewer_ip: IpAddr,
) -> anyhow::Result<Value> {
    const ROOM: &str = "provisioned-stage";
    const PUBLISHER: &str = "origin-performer";
    const VIEWER: &str = "remote-audience";
    const TRACK: &str = "cross-link-feed";
    const PAYLOAD: &[u8] = b"payload observed through the provisioned peer";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = |participant, allow: &[&str], ttl| {
        pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &serde_json::from_value(json!([{"rooms": [ROOM], "allow": allow}])).unwrap(),
            participant,
            now + ttl,
        )
    };
    let client = NativeHost::bind(client_ip)?;
    let agent_for = |ip, token| -> anyhow::Result<_> {
        let host = client.clone();
        let (runtime, task) = RuntimeAgent::managed_with_services(
            Config::new(pulsebeam_agent_core::AgentConfig {
                endpoint: format!("http://{ip}:7070"),
                token,
                retry: Default::default(),
                log_level: Default::default(),
            }),
            Host::new(create_http_client()),
            move || host.runtime(),
        )?;
        tokio::task::spawn_local(task);
        Ok((Agent::from_runtime(runtime.clone()), runtime))
    };
    let (viewer, viewer_transport) =
        agent_for(viewer_ip, token(VIEWER, &["subscribe"], 3600_u64)?.token)?;
    let feed = viewer
        .room(ROOM.into())?
        .participant(PUBLISHER.into())?
        .data(TRACK.into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
        }))?;
    let changes = feed.updates()?;
    viewer_transport.connect().await?;
    tokio::time::timeout(Duration::from_secs(7), async {
        while let Some(state) = changes.next().await? {
            if state.admission == core_ffi::PublicationAdmissionStatus::Admitted
                && !state.admission_pending
            {
                anyhow::ensure!(
                    state.source_status == core_ffi::SubscriptionSourceStatus::Unavailable
                        && state.source_track_id.is_none()
                );
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("subscription closed before admission")
    })
    .await
    .map_err(|_| anyhow::anyhow!("provisioned subscriber never acquired fresh authority"))??;
    anyhow::ensure!(viewer.snapshot().room_id.is_none());
    let support = viewer.support_report();
    let (origin, _) = agent_for(
        publisher_ip,
        token(PUBLISHER, &["join", "publish"], 3600_u64)?.token,
    )?;
    let publication = origin
        .data(TRACK.into())?
        .publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
        }))?;
    origin.join(ROOM.into())?;
    let published = wait_connected(&origin, None).await?;
    let pub_id = published
        .topics
        .publishers
        .first()
        .and_then(|track| track.publication_id.clone())
        .ok_or_else(|| anyhow::anyhow!("publisher was not admitted"))?;
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut send = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = send.tick() => publication.send(PAYLOAD.to_vec(), None).await?,
                state = feed.next() => {
                    use pulsebeam_agent_core::DataEvent;
                    match state? {
                        Some(DataEvent::Message { publication_id, participant_id, data, .. }) if data == PAYLOAD => {
                            anyhow::ensure!(publication_id == pub_id && participant_id == PUBLISHER);
                            return anyhow::Ok(());
                        }
                        None => anyhow::bail!("subscription closed before receiving relayed payload"),
                        _ => {}
                    }
                }
            }
        }
    }).await.map_err(|_| anyhow::anyhow!("remote payload not observed, subscription: {:?}", feed.state()))??;
    let before = viewer.snapshot().accepted_credential_revision;
    let snapshots = viewer.snapshots();
    let renewed = token(VIEWER, &["subscribe"], 7200_u64)?;
    viewer.renew(renewed.token)?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("subscriber closed during renewal");
            };
            if snapshot.accepted_credential_revision > before {
                return anyhow::Ok(());
            }
        }
    })
    .await??;
    anyhow::ensure!(viewer.support_report() == support);
    viewer.close().await?;
    origin.close().await?;
    Ok(
        json!({"conn": support.connection_id, "pub": pub_id, "project": pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID.to_string(), "jti": renewed.jti.to_string()}),
    )
}

fn cli_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("PULSEBEAM_INCIDENT_CLI") {
        return Ok(path.into());
    }
    let workspace = std::env::var_os("TEST_WORKSPACE").unwrap_or_else(|| "_main".into());
    let path = std::env::var_os("TEST_SRCDIR")
        .or_else(|| std::env::var_os("RUNFILES_DIR"))
        .map(PathBuf::from)
        .map(|root| root.join(workspace))
        .unwrap_or_default()
        .join("crates/pulsebeam-cli/pulsebeam-cli");
    anyhow::ensure!(
        path.is_file(),
        "incident CLI not found in runfiles: {}",
        path.display()
    );
    Ok(path)
}

fn check_incident(
    logs: &mut File,
    keys: &Value,
    publisher: (NodeId, NonZeroU64),
    receiver: (NodeId, NonZeroU64),
) -> anyhow::Result<()> {
    logs.rewind()?;
    let mut export = String::new();
    logs.read_to_string(&mut export)?;
    for name in [
        "provisioned-stage",
        "origin-performer",
        "remote-audience",
        "cross-link-feed",
    ] {
        anyhow::ensure!(
            !export.contains(name),
            "incident export contains an application name"
        );
    }
    let records = export
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let time = |record: &Value| record["timestamp"].as_str().map(str::to_owned);
    let from = records
        .iter()
        .filter_map(time)
        .min()
        .ok_or_else(|| anyhow::anyhow!("no incident timestamps"))?;
    // The exclusive end is the renewal itself. Token lookup must still find
    // the connection that was active before this credential was accepted.
    let to = records
        .iter()
        .find(|record| {
            record["fields"]["event"] == "credential_renewed"
                && record["fields"]["jti"] == keys["jti"]
        })
        .and_then(time)
        .ok_or_else(|| anyhow::anyhow!("missing accepted renewal record"))?;
    let published_node = publisher.0.to_string();
    let receiver_node = receiver.0.to_string();
    anyhow::ensure!(
        records.iter().any(|record| {
            let fields = &record["fields"];
            fields["event"] == "relay_installed"
                && fields["pub"] == keys["pub"]
                && fields["node"] == receiver_node
                && fields["incarnation"] == receiver.1.get()
                && fields["peer"] == published_node
        }),
        "no correlated receive relay installed on the provisioned node"
    );
    let cli = cli_path()?;
    let project = keys["project"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing project scope"))?;
    for (flag, key) in [
        ("--conn", "conn"),
        ("--publication", "pub"),
        ("--jti", "jti"),
    ] {
        let selector = keys[key]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing {key} support key"))?;
        logs.rewind()?;
        let mut command = Command::new(&cli);
        command.args(["incident", "--from", &from, "--to", &to, flag, selector]);
        if flag == "--jti" {
            command.args(["--project", project]);
        }
        let output = command.stdin(Stdio::from(logs.try_clone()?)).output()?;
        anyhow::ensure!(
            output.status.success(),
            "incident --{key} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        anyhow::ensure!(
            report["intervals"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| {
                    row["start_evidence"]["event"] == "subscription_bound"
                        && row["start_evidence"]["pub"] == keys["pub"]
                })),
            "no observed binding for {flag}: {report}"
        );
        anyhow::ensure!(
            report["events"].as_array().is_some_and(|rows| rows
                .iter()
                .any(|row| row["event"] == "relay_installed" && row["pub"] == keys["pub"])),
            "no relay event for {flag}: {report}"
        );
        if flag == "--jti" {
            anyhow::ensure!(
                report["ambiguous_token_match"] == false
                    && report["connections"]
                        .as_array()
                        .is_some_and(|rows| rows.len() == 1
                            && rows.first().is_some_and(|row| row["conn"] == keys["conn"]))
                    && report["token_matches"].as_array().is_some_and(|rows| {
                        rows.len() == 1
                            && rows.first().is_some_and(|row| {
                                row["event"] == "credential_renewed"
                                    && row["conn"] == keys["conn"]
                                    && row["project"] == project
                            })
                    }),
                "project-scoped renewal did not match the live connection: {report}"
            );
        }
    }
    Ok(())
}
