use super::*;

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::SocketAddr,
    num::NonZeroU64,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::Context;
use pulsebeam::cluster::{Options, ProvisionedPeer, Topology, prepare_provisioned_fixture};
use pulsebeam_core::{auth::ApiSigningKey, identity::NodeId};
use pulsebeam_link::{NetworkClass, NetworkPeer, PeerRole, SecretCredential};
use serde_json::{Value, json};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[test]
fn provisioned_native_cluster_relays_payload_and_reconstructs_incident() -> anyhow::Result<()> {
    prove_provisioned_cluster(2, &[(0, 1)], core_ffi::TopicMode::Latest, false)
}

#[test]
fn provisioned_wan_delivers_lossy_sdk_fragments() -> anyhow::Result<()> {
    prove_provisioned_cluster(2, &[(0, 1)], core_ffi::TopicMode::Lossy, false)
}

#[test]
fn provisioned_wan_delivers_reliable_sdk_fragments() -> anyhow::Result<()> {
    prove_provisioned_cluster(2, &[(0, 1)], core_ffi::TopicMode::Ordered, false)
}

#[test]
fn provisioned_wan_responder_repairs_reliable_sdk_fragments() -> anyhow::Result<()> {
    prove_provisioned_cluster(2, &[(1, 0)], core_ffi::TopicMode::Ordered, false)
}

#[test]
fn fabric_wan_diamond_converges() -> anyhow::Result<()> {
    prove_provisioned_cluster(
        4,
        &[(0, 1), (0, 2), (1, 3), (2, 3)],
        core_ffi::TopicMode::Latest,
        false,
    )
}

#[test]
fn provisioned_wan_opus_repairs_exact_position_on_fresh_window() -> anyhow::Result<()> {
    prove_provisioned_cluster(3, &[(0, 1), (1, 2)], core_ffi::TopicMode::Latest, true)
}

#[test]
fn local_ordered_cross_shard_delivery_and_repair() -> anyhow::Result<()> {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let (node_ip, client_ip) = (subnet_ip(subnet, 1), subnet_ip(subnet, 2));
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(25))
        .rng_seed(seed)
        .build();
    sim.host(node_ip, move || async move {
        sfu_node_builder(node_ip, pulsebeam_runtime::rand::seeded_rng(seed), 2)
            .run(tokio_util::sync::CancellationToken::new())
            .await
            .map_err(Into::into)
    });
    sim.client(client_ip, async move {
        prove_payload(
            client_ip,
            node_ip,
            node_ip,
            PayloadScenario {
                mode: core_ffi::TopicMode::Ordered,
                relays: None,
                repair_edges: &[],
                verify_subscriptions: false,
            },
        )
        .await
        .map(|_| ())
        .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(20))
        .map_err(|error| anyhow::anyhow!("two-shard Ordered simulation failed: {error:#}"))
}

pub(super) fn install_provisioned_nodes(
    sim: &mut turmoil::Sim<'_>,
    root: &std::path::Path,
    ips: &[IpAddr],
    edges: &[(usize, usize)],
    seed: u64,
) -> anyhow::Result<Vec<(NodeId, NonZeroU64)>> {
    let nodes: Vec<_> = ips
        .iter()
        .copied()
        .enumerate()
        .map(|(index, ip)| {
            let host = u8::try_from(index + 1)?;
            let data_dir = root.join(host.to_string());
            let identity = prepare_provisioned_fixture(&data_dir)?;
            let transport = SecretCredential::generate().map_err(relay_error)?;
            let receipts = ApiSigningKey::from_seed([60 + host; 32]);
            anyhow::Ok((ip, data_dir, identity, transport, receipts))
        })
        .collect::<anyhow::Result<_>>()?;
    let left_id = nodes[0].2;
    let viewer_index = nodes.len().saturating_sub(1);
    for (index, (ip, data_dir, identity, credential, receipt_key)) in nodes.iter().enumerate() {
        let ip = *ip;
        let gossip = (ip, 7946).into();
        let ownership_peers = edges
            .iter()
            .enumerate()
            .filter_map(|(edge, &(from, to))| {
                let (other, role) = if from == index {
                    (to, PeerRole::Initiator)
                } else if to == index {
                    (from, PeerRole::Responder)
                } else {
                    return None;
                };
                let (remote_ip, _, remote, remote_credential, remote_key) = &nodes[other];
                let port = 7443 + u16::try_from(edge).unwrap();
                Some(ProvisionedPeer {
                    listen: (ip, port).into(),
                    local: *identity,
                    remote: *remote,
                    transport: NetworkPeer {
                        endpoint: (*remote_ip, port).into(),
                        credential: credential.clone(),
                        expected_peer: remote_credential.fingerprint(),
                        role,
                        class: NetworkClass::Wan,
                    },
                    fabric_wan: true,
                    relay_from: (index != 0 && index != viewer_index && other == viewer_index)
                        .then_some(left_id.0),
                    authority_epoch: [91; 16],
                    receipt_key: receipt_key.clone(),
                    peer_receipt_key: remote_key.public_key(),
                })
            })
            .collect();
        let options = Options {
            cluster_id: "provisioned-witness".into(),
            topology: Topology::MultiNode,
            listen: gossip,
            advertise: gossip,
            seeds: Vec::new(),
            data_dir: data_dir.clone(),
            ownership_peers,
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
    Ok(nodes
        .into_iter()
        .map(|(_, _, identity, _, _)| identity)
        .collect())
}

fn prove_provisioned_cluster(
    nodes: u8,
    edges: &[(usize, usize)],
    mode: core_ffi::TopicMode,
    exercise_media_repair: bool,
) -> anyhow::Result<()> {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let client_ip = subnet_ip(subnet, nodes + 1);
    let root = tempfile::tempdir()?;
    let ips = (1..=nodes)
        .map(|host| subnet_ip(subnet, host))
        .collect::<Vec<_>>();
    let publisher_ip = ips[0];
    let viewer_ip = ips[ips.len() - 1];
    let relays = (ips.len() == 4).then(|| [ips[1], ips[2]]);
    let repair_edges: Vec<_> = if exercise_media_repair {
        edges
            .iter()
            .enumerate()
            .map(|(i, &(from, to))| {
                let port = 7443 + u16::try_from(i).unwrap();
                (
                    SocketAddr::from((ips[from], port)),
                    SocketAddr::from((ips[to], port)),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(
            if relays.is_some() || exercise_media_repair {
                40
            } else {
                25
            },
        ))
        .rng_seed(seed)
        .build();

    let identities = install_provisioned_nodes(&mut sim, root.path(), &ips, edges, seed)?;
    let left_id = identities[0];
    let right_id = identities[identities.len() - 1];
    let witness = Arc::new(Mutex::new(None));
    let capture = witness.clone();
    let verify_subscriptions = edges == [(0, 1)];
    sim.client(client_ip, async move {
        let keys = prove_payload(
            client_ip,
            publisher_ip,
            viewer_ip,
            PayloadScenario {
                mode,
                relays,
                repair_edges: &repair_edges,
                verify_subscriptions,
            },
        )
        .await?;
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
                    || meta.target().starts_with("pulsebeam::link::wan_host")
                    || meta.target()
                        == "pulsebeam::control::controller::peer_ownership::provisioned"
            })),
    );
    if let Err(error) = tracing::subscriber::with_default(subscriber, || {
        let result = run_sim_or_timeout(&mut sim, Duration::from_secs(35));
        drop(sim);
        result
    }) {
        const DIAGNOSTIC_LIMIT: u64 = 64 * 1024;
        let offset = logs.metadata()?.len().saturating_sub(DIAGNOSTIC_LIMIT);
        logs.seek(SeekFrom::Start(offset))?;
        let mut diagnostics = Vec::new();
        (&mut logs)
            .take(DIAGNOSTIC_LIMIT)
            .read_to_end(&mut diagnostics)?;
        eprintln!("{}", String::from_utf8_lossy(&diagnostics));
        return Err(anyhow::anyhow!("provisioned simulation failed: {error:#}"));
    }
    let keys = witness
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing production witness"))?;
    if verify_subscriptions && mode == core_ffi::TopicMode::Latest {
        check_incident(&mut logs, &keys, left_id, right_id)?;
        println!("[provisioned-incident] {keys}");
    }
    Ok(())
}

struct PayloadScenario<'a> {
    mode: core_ffi::TopicMode,
    relays: Option<[IpAddr; 2]>,
    repair_edges: &'a [(SocketAddr, SocketAddr)],
    verify_subscriptions: bool,
}

async fn prove_payload(
    client_ip: IpAddr,
    publisher_ip: IpAddr,
    viewer_ip: IpAddr,
    scenario: PayloadScenario<'_>,
) -> anyhow::Result<Value> {
    let PayloadScenario {
        mode,
        relays,
        repair_edges,
        verify_subscriptions,
    } = scenario;
    let exercise_media_repair = !repair_edges.is_empty();
    const ROOM: &str = "provisioned-stage";
    const PUBLISHER: &str = "origin-performer";
    const VIEWER: &str = "remote-audience";
    const TRACK: &str = "cross-link-feed";
    const AUDIO: &str = "opus-source-facts";
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
        let mut config = Config::new(pulsebeam_agent_core::AgentConfig {
            endpoint: format!("http://{ip}:7070"),
            token,
            retry: Default::default(),
            log_level: Default::default(),
        });
        if ip == publisher_ip && exercise_media_repair {
            config.media.audio_input = AudioInput::EncodedOpus { channels: 2 };
        }
        let (runtime, task) = RuntimeAgent::managed_with_services(
            config,
            Host::new(create_http_client()),
            move || host.runtime(),
        )?;
        tokio::task::spawn_local(task);
        Ok((Agent::from_runtime(runtime.clone()), runtime))
    };
    let local = publisher_ip == viewer_ip;
    let viewer_permissions: &[&str] = if local {
        &["join", "subscribe"]
    } else {
        &["subscribe"]
    };
    let delivery = match mode {
        core_ffi::TopicMode::Ordered => core_ffi::DataDelivery::Reliable,
        core_ffi::TopicMode::Lossy => core_ffi::DataDelivery::Unreliable,
        core_ffi::TopicMode::Latest => core_ffi::DataDelivery::Latest,
    };
    let (viewer, viewer_transport) = agent_for(
        viewer_ip,
        token(VIEWER, viewer_permissions, 3600_u64)?.token,
    )?;
    let feed = viewer
        .room(ROOM.into())?
        .participant(PUBLISHER.into())?
        .data(TRACK.into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(delivery),
        }))?;
    let changes = feed.updates()?;
    if local {
        // Reserve the first room partition so the publisher installs on the other shard.
        viewer.join(ROOM.into())?;
        wait_connected_when(&viewer, None, |_| true).await?;
    } else {
        viewer_transport.connect().await?;
    }
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
    .map_err(|_| {
        anyhow::anyhow!(
            "provisioned subscriber never acquired fresh authority: {:?}",
            feed.state()
        )
    })??;
    anyhow::ensure!(viewer.snapshot().room_id.is_some() == local);
    let support = viewer.support_report();
    let (origin, _) = agent_for(
        publisher_ip,
        token(PUBLISHER, &["join", "publish"], 3600_u64)?.token,
    )?;
    let publication = origin
        .data(TRACK.into())?
        .publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(delivery),
            ..Default::default()
        }))?;
    origin.join(ROOM.into())?;
    wait_connected_when(&origin, None, |_| true).await?;
    let pub_id = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(state) = changes.next().await? {
            if state.execution == core_ffi::PublicationExecution::Installed {
                return state
                    .source_track_id
                    .ok_or_else(|| anyhow::anyhow!("installed DATA source missing"));
            }
        }
        anyhow::bail!("subscription closed before Fabric installation")
    })
    .await
    .map_err(|_| anyhow::anyhow!("remote DATA route not acknowledged: {:?}", feed.state()))??;
    use pulsebeam_agent_core::DataEvent;
    let wait_payload = async |expected: &[u8], forbidden: Option<&[u8]>, budget: Duration| {
        tokio::time::timeout(budget, async {
            let mut send = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    _ = send.tick() => match publication.send(expected.to_vec(), None).await {
                        Ok(()) => {},
                        Err(pulsebeam_agent_native::ffi::NativeError::Operation { info })
                            if forbidden.is_some() && info.code == core_ffi::ErrorCode::CapacityExceeded => {},
                        Err(error) => return Err(error.into()),
                    },
                    state = feed.next() => {
                        match state? {
                            Some(DataEvent::Message { publication_id, participant_id, data, .. }) => {
                                anyhow::ensure!(forbidden != Some(data.as_slice()), "partitioned payload escaped its retired transit path");
                                if data == expected {
                                    anyhow::ensure!(publication_id == pub_id && participant_id == PUBLISHER);
                                    return anyhow::Ok(());
                                }
                            }
                            None => anyhow::bail!("subscription closed before receiving relayed payload"),
                            _ => {}
                        }
                    }
                }
            }
        }).await
    };
    let fragmented;
    let initial_payload = if relays.is_none() && !exercise_media_repair {
        fragmented = (0u8..251).cycle().take(2051).collect::<Vec<_>>();
        fragmented.as_slice()
    } else {
        PAYLOAD
    };
    if mode == core_ffi::TopicMode::Ordered {
        let updates = publication.updates()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(state) = updates.next().await? {
                if state.execution == core_ffi::PublicationExecution::Installed {
                    return anyhow::Ok(());
                }
            }
            anyhow::bail!("Ordered publication closed before installation")
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("Ordered publisher not installed: {:?}", publication.state())
        })??;
    }
    if mode == core_ffi::TopicMode::Ordered && local {
        prove_local_ordered_payload(
            &origin,
            &publication,
            &feed,
            (TRACK, PUBLISHER, &pub_id),
            initial_payload,
        )
        .await?;
        prove_local_ordered_expiry(
            &origin,
            &publication,
            &feed,
            (TRACK, PUBLISHER, &pub_id),
            initial_payload,
        )
        .await?;
        let replacement = prove_local_ordered_cutoff(
            &origin,
            &viewer,
            &publication,
            &feed,
            (ROOM, TRACK, PUBLISHER, &pub_id),
            initial_payload,
        )
        .await?;
        let (late, _) = agent_for(
            viewer_ip,
            token("late-local-viewer", &["join", "subscribe"], 3600_u64)?.token,
        )?;
        prove_local_ordered_late_join(
            &origin,
            &late,
            &publication,
            &replacement,
            (ROOM, TRACK, PUBLISHER, &pub_id),
            initial_payload,
        )
        .await?;
        let (denied, _) = agent_for(
            viewer_ip,
            token("denied-local-viewer", &["join"], 3600_u64)?.token,
        )?;
        let refused = prove_reliable_subscription_denied(&denied, ROOM, TRACK, PUBLISHER).await?;
        let events = origin.events();
        prove_reliable_sdk_unit(
            &events,
            &publication,
            &replacement,
            (TRACK, PUBLISHER, &pub_id),
            b"authorized delivery after a default-profile denial",
        )
        .await?;
        assert_denied_data_stays_uninstalled(&refused).await?;
        events.close().await;
        denied.close().await?;
        late.close().await?;
        viewer.close().await?;
        origin.close().await?;
        return Ok(Value::Null);
    }
    if mode == core_ffi::TopicMode::Ordered {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pulsebeam::sim_metrics::wan_data_send_ready(&pub_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("Reliable publisher WAN route never became ready: {pub_id}")
        })?;
        let clean_events = origin.events();
        prove_reliable_sdk_unit(
            &clean_events,
            &publication,
            &feed,
            (TRACK, PUBLISHER, &pub_id),
            initial_payload,
        )
        .await?;
        clean_events.close().await;
        let lost: Vec<_> = initial_payload.iter().map(|&byte| byte ^ 0x80).collect();
        let repair_events = origin.events();
        let fragment_count = pulsebeam_proto::data_record::fragment_count(lost.len())
            .map_err(|error| anyhow::anyhow!("fixture fragment count: {error:?}"))?;
        pulsebeam::sim_metrics::arm_wan_data_loss(publisher_ip, viewer_ip);
        let outcome = async {
            let (source, unit) = prove_reliable_sdk_unit(
                &repair_events,
                &publication,
                &feed,
                (TRACK, PUBLISHER, &pub_id),
                &lost,
            )
            .await?;
            tokio::time::timeout(Duration::from_secs(3), async {
                while !pulsebeam::sim_metrics::wan_data_receipt_ack(source, unit) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "subscriber edge did not acknowledge assembly for source {source}, unit {unit}"
                )
            })?;
            anyhow::ensure!(
                pulsebeam::sim_metrics::wan_data_raw_records_match(source, unit, fragment_count),
                "SCTP records differ after WAN repair for source {source}, unit {unit}"
            );
            Ok(())
        }
        .await;
        let loss = pulsebeam::sim_metrics::take_wan_data_loss()
            .ok_or_else(|| anyhow::anyhow!("missing Reliable WAN loss probe"))?;
        outcome.map_err(|error| anyhow::anyhow!("{error:#}; WAN loss: {loss:?}"))?;
        anyhow::ensure!(
            loss.dropped.len() == usize::from(fragment_count) && loss.repaired == loss.dropped,
            "every lost live fragment needs authenticated exact repair: {loss:?}"
        );
        repair_events.close().await;
        if verify_subscriptions {
            let (late, _) = agent_for(
                viewer_ip,
                token("late-viewer", &["join", "subscribe"], 3600_u64)?.token,
            )?;
            let (denied, _) = agent_for(
                viewer_ip,
                token("denied-viewer", &["join"], 3600_u64)?.token,
            )?;
            prove_reliable_subscription_boundary(
                &origin,
                &publication,
                &feed,
                (&late, &denied),
                (ROOM, TRACK, PUBLISHER, &pub_id),
            )
            .await?;
        }
        viewer.close().await?;
        origin.close().await?;
        return Ok(Value::Null);
    }
    wait_payload(initial_payload, None, Duration::from_secs(8))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "remote payload not observed, subscription: {:?}",
                feed.state()
            )
        })??;
    if let Some(relays) = relays {
        let branch_receipts =
            || relays.map(|relay| pulsebeam::sim_metrics::wan_live_data(relay, viewer_ip));
        let upstream_receipts =
            || relays.map(|relay| pulsebeam::sim_metrics::wan_live_data(publisher_ip, relay));
        tokio::time::timeout(Duration::from_secs(5), async {
            while branch_receipts().iter().any(|&(count, _, _)| count == 0) {
                wait_payload(PAYLOAD, None, Duration::from_secs(2)).await??;
            }
            anyhow::Ok(())
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "both relay branches did not hand off live media: {:?}",
                branch_receipts()
            )
        })??;
        // Let earlier publications age out before offering a single known unit.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let before = branch_receipts();
        let upstream_before = upstream_receipts();
        const PROBE: &[u8] = b"one publisher unit over both relay paths";
        publication.send(PROBE.to_vec(), None).await?;
        let mut delivered = 0;
        if let Ok(result) = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                match feed.next().await? {
                    Some(DataEvent::Message {
                        publication_id,
                        participant_id,
                        data,
                        ..
                    }) if data == PROBE => {
                        anyhow::ensure!(publication_id == pub_id && participant_id == PUBLISHER);
                        delivered += 1;
                        anyhow::ensure!(
                            delivered == 1,
                            "diamond delivered one publication unit twice"
                        );
                    }
                    None => {
                        anyhow::bail!("subscription closed during the diamond probe")
                    }
                    _ => {}
                }
            }
        })
        .await
        {
            let result: anyhow::Result<()> = result;
            result?;
        }
        let after = branch_receipts();
        let upstream_after = upstream_receipts();
        anyhow::ensure!(
            delivered == 1,
            "diamond did not deliver the probe to the SDK"
        );
        anyhow::ensure!(
            after[0].0 == before[0].0 + 1
                && after[1].0 == before[1].0 + 1
                && after[0].1 == after[1].1
                && after[0].2 == after[1].2,
            "expected one copy on each WAN branch at the same position and timestamp: {before:?} -> {after:?}"
        );
        for ((prior, upstream), downstream) in
            upstream_before.into_iter().zip(upstream_after).zip(after)
        {
            anyhow::ensure!(
                upstream.0 == prior.0 + 1
                    && upstream.1 == downstream.1
                    && upstream.2 == downstream.2,
                "WAN relay changed the DATA position or canonical timestamp: {upstream:?} -> {downstream:?}"
            );
        }

        const DURING_PARTITION: &[u8] = b"must not cross the retired WAN paths";
        const AFTER_REPAIR: &[u8] = b"fresh payload after WAN path recovery";
        for relay in relays {
            turmoil::partition(relay, viewer_ip);
        }
        if let Ok(result) = wait_payload(
            DURING_PARTITION,
            Some(DURING_PARTITION),
            Duration::from_secs(7),
        )
        .await
        {
            result?;
        }
        for relay in relays.into_iter().rev() {
            turmoil::repair(relay, viewer_ip);
        }
        let before_recovery = branch_receipts();
        let before_upstream = upstream_receipts();
        wait_payload(
            AFTER_REPAIR,
            Some(DURING_PARTITION),
            Duration::from_secs(12),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "diamond did not deliver fresh media after repair: viewer={:?}; publisher={:?}; upstream={before_upstream:?}->{:?}; downstream={before_recovery:?}->{:?}",
                feed.state(), publication.state(), upstream_receipts(), branch_receipts()
            )
        })??;
        anyhow::ensure!(
            branch_receipts()
                .iter()
                .zip(before_recovery)
                .any(|(after, before)| after.0 > before.0)
        );
    }
    // The partition can trigger a transport reconnect. Renewal itself must preserve the
    // connection that qualified for delivery, without replacing the subscription.
    let support = if relays.is_some() {
        viewer.support_report()
    } else {
        support
    };
    anyhow::ensure!(support.connection_id.is_some());
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
    .await
    .map_err(|_| {
        anyhow::anyhow!("provisioned subscriber did not acknowledge credential renewal")
    })??;
    anyhow::ensure!(viewer.support_report() == support);
    if exercise_media_repair {
        let (listener, _) = agent_for(
            publisher_ip,
            token("local-listener", &["join", "subscribe"], 3600_u64)?.token,
        )?;
        let _audio = listener
            .room(ROOM.into())?
            .participant(PUBLISHER.into())?
            .audio(AUDIO.into())?
            .subscribe()?;
        listener.join(ROOM.into())?;
        let remote_audio = viewer
            .room(ROOM.into())?
            .participant(PUBLISHER.into())?
            .audio(AUDIO.into())?
            .subscribe()?;
        prove_wan_opus_repair(
            &origin,
            AUDIO,
            &remote_audio,
            repair_edges,
            &client,
            (publisher_ip, viewer_ip),
        )
        .await?;
        listener.close().await?;
    }
    viewer.close().await?;
    origin.close().await?;
    Ok(
        json!({"conn": support.connection_id, "pub": pub_id, "project": pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID.to_string(), "jti": renewed.jti.to_string()}),
    )
}

async fn prove_reliable_subscription_boundary(
    origin: &Agent,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    original: &pulsebeam_agent_native::ffi::DataSubscription,
    (late, denied): (&Agent, &Agent),
    (room, track, publisher, source): (&str, &str, &str, &str),
) -> anyhow::Result<()> {
    late.join(room.into())?;
    let (_, refused) = tokio::try_join!(
        wait_connected_when(late, None, |_| true),
        prove_reliable_subscription_denied(denied, room, track, publisher)
    )?;

    let events = origin.events();
    let admitted_at = tokio::time::Instant::now();
    let (_, old_unit) = prove_reliable_sdk_unit(
        &events,
        publication,
        original,
        (track, publisher, source),
        b"source admitted before late branch",
    )
    .await?;
    events.close().await;
    let late_feed = subscribe_reliable(late, room, track, publisher)?;
    let updates = late_feed.updates()?;
    tokio::time::timeout_at(admitted_at + Duration::from_millis(1500), async {
        while let Some(state) = updates.next().await? {
            if state.execution == core_ffi::PublicationExecution::Installed {
                anyhow::ensure!(state.source_track_id.as_deref() == Some(source));
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("late branch closed before binding")
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "late branch did not bind while the prior Reliable unit was retained: {:?}",
            late_feed.state()
        )
    })??;
    let events = origin.events();
    let (_, new_unit) = prove_reliable_sdk_unit(
        &events,
        publication,
        &late_feed,
        (track, publisher, source),
        b"fresh unit reaches late branch",
    )
    .await?;
    anyhow::ensure!(
        new_unit > old_unit,
        "late branch did not advance past the prior Reliable unit"
    );
    assert_denied_data_stays_uninstalled(&refused).await?;
    events.close().await;
    late.close().await?;
    denied.close().await?;
    Ok(())
}

fn subscribe_reliable(
    agent: &Agent,
    room: &str,
    track: &str,
    publisher: &str,
) -> anyhow::Result<std::sync::Arc<pulsebeam_agent_native::ffi::DataSubscription>> {
    Ok(agent
        .room(room.into())?
        .participant(publisher.into())?
        .data(track.into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Reliable),
        }))?)
}

async fn prove_reliable_subscription_denied(
    denied: &Agent,
    room: &str,
    track: &str,
    publisher: &str,
) -> anyhow::Result<std::sync::Arc<pulsebeam_agent_native::ffi::DataSubscription>> {
    denied.join(room.into())?;
    wait_connected_when(denied, None, |_| true).await?;
    let refused = subscribe_reliable(denied, room, track, publisher)?;
    let updates = refused.updates()?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(state) = updates.next().await? {
            if let Some(error) = state.terminal_error {
                anyhow::ensure!(error.code == core_ffi::ErrorCode::PermissionDenied);
                anyhow::ensure!(
                    state.source_status == core_ffi::SubscriptionSourceStatus::Rejected
                        && state.accepted_revision.is_none()
                        && state.source_track_id.is_none()
                        && state.execution != core_ffi::PublicationExecution::Installed
                );
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("denied branch closed before rejection")
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "subscription without Subscribe permission was not rejected: {:?}",
            refused.state()
        )
    })??;

    Ok(refused)
}

async fn assert_denied_data_stays_uninstalled(
    refused: &pulsebeam_agent_native::ffi::DataSubscription,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        refused.state().source_track_id.is_none()
            && refused.state().execution != core_ffi::PublicationExecution::Installed
    );
    if let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_millis(50), refused.next()).await {
        anyhow::bail!("denied subscription received a DATA event");
    }
    Ok(())
}

async fn prove_local_ordered_payload(
    origin: &Agent,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    feed: &pulsebeam_agent_native::ffi::DataSubscription,
    (track, publisher, source): (&str, &str, &str),
    initial_payload: &[u8],
) -> anyhow::Result<()> {
    use core_ffi::{ReliableDelivery, ReliablePublishEvent};
    use pulsebeam::sim_metrics::{
        ShardDataFault, arm_shard_data_trace, routing_counter, take_shard_data_trace,
    };

    tokio::time::timeout(Duration::from_secs(3), async {
        while routing_counter("fabric_shard_data_ready") == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Ordered receiver never became ready on the source shard"))?;

    let events = origin.events();
    let lost: Vec<_> = initial_payload.iter().map(|&byte| byte ^ 0x80).collect();
    let fragment_count = pulsebeam_proto::data_record::fragment_count(initial_payload.len())
        .map_err(|error| anyhow::anyhow!("fixture fragment count: {error:?}"))?;
    for (fault, payload) in [
        (ShardDataFault::None, initial_payload),
        (ShardDataFault::FirstLiveOffer, lost.as_slice()),
        (ShardDataFault::FirstReceipt, initial_payload),
    ] {
        arm_shard_data_trace(fault);
        let (incarnation, unit) = prove_reliable_sdk_unit(
            &events,
            publication,
            feed,
            (track, publisher, source),
            payload,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "{error:#}; local mailbox trace: {:?}",
                take_shard_data_trace()
            )
        })?;
        let (branch, mut trace) = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (observed_source, event) = next_reliable_sdk_event(&events, track).await?;
                if let ReliablePublishEvent::Delivery {
                    unit_seq,
                    outcome,
                    branch,
                } = event
                {
                    anyhow::ensure!(observed_source == incarnation && unit_seq == unit);
                    anyhow::ensure!(
                        outcome == ReliableDelivery::Confirmed,
                        "Ordered delivery: {outcome:?}"
                    );
                    let trace = take_shard_data_trace()
                        .ok_or_else(|| anyhow::anyhow!("missing local mailbox trace"))?;
                    return Ok((branch, trace));
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("Ordered publisher never confirmed unit {unit}"))??;
        anyhow::ensure!(
            trace.accepted_receipt == Some((incarnation, unit, branch)),
            "publisher confirmed before the source accepted an exact receipt: {trace:?}"
        );
        if fault == ShardDataFault::FirstReceipt {
            anyhow::ensure!(
                trace.receipt_retried_within_deadline,
                "expected the lost exact receipt to retry before the original deadline: {trace:?}"
            );
        }
        trace.live.sort_unstable_by_key(|fragment| fragment.index);
        anyhow::ensure!(
            trace.live.len() == usize::from(fragment_count)
                && trace
                    .live
                    .iter()
                    .zip(0..fragment_count)
                    .all(|(fragment, index)| {
                        fragment.unit_seq == unit
                            && fragment.index == index
                            && fragment.count == fragment_count
                    }),
            "expected all live fragments across distinct shards, with no replay: {trace:?}"
        );
        if fault == ShardDataFault::FirstLiveOffer {
            anyhow::ensure!(
                trace
                    .dropped
                    .is_some_and(|dropped| trace.live.contains(&dropped))
                    && trace.dropped.as_slice() == trace.repaired,
                "expected only the dropped fragment as a targeted repair: {trace:?}"
            );
        } else {
            anyhow::ensure!(
                trace.dropped.is_none() && trace.repaired.is_empty(),
                "clean delivery should not need repair: {trace:?}"
            );
        }
    }

    // Keep both streams open past the original reliability window to catch duplicate outcomes.
    use pulsebeam_agent_core::DataEvent;
    let quiet = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            extra = async {
                loop {
                    let extra = feed.next().await?;
                    if matches!(extra, Some(DataEvent::Message { .. }) | None) {
                        return anyhow::Ok(extra);
                    }
                }
            } => anyhow::bail!("unexpected extra Ordered payload or stream closure: {:?}", extra?),
            extra = next_reliable_sdk_event(&events, track) => {
                anyhow::bail!("unexpected extra Ordered publisher event: {:?}", extra?);
            }
        }
    })
    .await;
    if let Ok(result) = quiet {
        return result;
    }
    events.close().await;
    Ok(())
}

async fn prove_local_ordered_expiry(
    origin: &Agent,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    feed: &pulsebeam_agent_native::ffi::DataSubscription,
    (track, publisher, source): (&str, &str, &str),
    payload: &[u8],
) -> anyhow::Result<()> {
    use core_ffi::{ReliableDelivery, ReliablePublishEvent};
    use pulsebeam::sim_metrics::{
        ShardDataFault, arm_shard_data_trace, shard_data_expired, take_shard_data_trace,
    };

    let events = origin.events();
    arm_shard_data_trace(ShardDataFault::ReceiptExpiry);
    let (incarnation, expired_unit) = prove_reliable_sdk_unit(
        &events,
        publication,
        feed,
        (track, publisher, source),
        payload,
    )
    .await?;
    let unknown = tokio::time::timeout(
        Duration::from_secs(3),
        next_reliable_sdk_event(&events, track),
    )
    .await??;
    anyhow::ensure!(
        matches!(unknown, (source, ReliablePublishEvent::Delivery {
            unit_seq, outcome: ReliableDelivery::Unknown, ..
        }) if source == incarnation && unit_seq == expired_unit)
            && shard_data_expired(),
        "expected Unknown after the original deadline with an exact receipt withheld: {unknown:?}"
    );

    let later: Vec<_> = payload.iter().map(|byte| byte ^ 0x20).collect();
    let (later_incarnation, later_unit) = prove_reliable_sdk_unit(
        &events,
        publication,
        feed,
        (track, publisher, source),
        &later,
    )
    .await?;
    anyhow::ensure!(later_incarnation == incarnation && later_unit > expired_unit);
    let confirmed = tokio::time::timeout(
        Duration::from_secs(2),
        next_reliable_sdk_event(&events, track),
    )
    .await??;
    let trace =
        take_shard_data_trace().ok_or_else(|| anyhow::anyhow!("missing receipt expiry trace"))?;
    anyhow::ensure!(
        matches!(confirmed, (source, ReliablePublishEvent::Delivery {
            unit_seq, branch, outcome: ReliableDelivery::Confirmed,
        }) if (source, unit_seq) == (incarnation, later_unit)
            && trace.accepted_receipt == Some((source, unit_seq, branch))),
        "expected only the later unit to confirm: {confirmed:?}; {trace:?}"
    );
    anyhow::ensure!(
        trace.late_receipt_rejected && trace.expired_unit_reclaimed,
        "missing late receipt rejection or expired source state reclamation: {trace:?}"
    );
    if let Ok(extra) = tokio::time::timeout(
        Duration::from_millis(250),
        next_reliable_sdk_event(&events, track),
    )
    .await
    {
        anyhow::bail!(
            "unexpected publisher outcome after late receipt: {:?}",
            extra?
        );
    }
    events.close().await;
    Ok(())
}

async fn prove_local_ordered_cutoff(
    origin: &Agent,
    viewer: &Agent,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    old_feed: &pulsebeam_agent_native::ffi::DataSubscription,
    (room, track, publisher, source): (&str, &str, &str, &str),
    payload: &[u8],
) -> anyhow::Result<Arc<pulsebeam_agent_native::ffi::DataSubscription>> {
    use pulsebeam::sim_metrics::{
        ShardDataFault, arm_shard_data_trace, release_shard_data_cutoff, routing_counter,
        shard_data_cutoff_held, take_shard_data_trace,
    };
    use pulsebeam_agent_core::DataEvent;

    let events = origin.events();
    let ready = routing_counter("fabric_shard_data_ready");
    arm_shard_data_trace(ShardDataFault::BindingCutoff);
    publication.send(payload.to_vec(), None).await?;
    let (incarnation, retired_unit) = await_ordered_admission(&events, track).await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !shard_data_cutoff_held() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("did not capture a real stale notice and repair for {retired_unit}")
    })?;

    old_feed.close().await?;
    let replacement = viewer
        .room(room.into())?
        .participant(publisher.into())?
        .data(track.into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Reliable),
        }))?;
    let updates = replacement.updates()?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(state) = updates.next().await? {
            if state.execution == core_ffi::PublicationExecution::Installed {
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("replacement Ordered binding closed before installation")
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(2), async {
        while routing_counter("fabric_shard_data_ready") <= ready {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("replacement Ordered binding never became ready at the source"))?;

    release_shard_data_cutoff();
    let fresh: Vec<_> = payload.iter().map(|byte| byte ^ 0x40).collect();
    let (fresh_incarnation, fresh_unit) = prove_reliable_sdk_unit(
        &events,
        publication,
        &replacement,
        (track, publisher, source),
        &fresh,
    )
    .await?;
    anyhow::ensure!(fresh_incarnation == incarnation && fresh_unit > retired_unit);

    let quiet: Result<anyhow::Result<()>, _> =
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let update = replacement.next().await?;
                if matches!(update, Some(DataEvent::Message { .. }) | None) {
                    anyhow::bail!("unexpected Ordered update after fresh delivery: {update:?}");
                }
            }
        })
        .await;
    if let Ok(result) = quiet {
        result?;
    }
    let trace =
        take_shard_data_trace().ok_or_else(|| anyhow::anyhow!("missing binding cutoff trace"))?;
    anyhow::ensure!(
        trace.replayed_before_cutoff_deadline && trace.stale_writes == 0,
        "expected stale mailbox work replayed before expiry against a different receiver, with no old writes: {trace:?}"
    );
    events.close().await;
    Ok(replacement)
}

async fn await_ordered_admission(
    events: &pulsebeam_agent_native::ffi::EventStream,
    track: &str,
) -> anyhow::Result<(u64, u64)> {
    use core_ffi::ReliablePublishEvent;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (incarnation, event) = next_reliable_sdk_event(events, track).await?;
            match event {
                ReliablePublishEvent::SourceAdmitted { unit_seq } => {
                    return Ok((incarnation, unit_seq));
                }
                ReliablePublishEvent::LocallyAdmitted { .. } => {}
                event => {
                    anyhow::bail!("expected source admission before changing the cohort: {event:?}")
                }
            }
        }
    })
    .await?
}

async fn prove_local_ordered_late_join(
    origin: &Agent,
    late: &Agent,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    original: &pulsebeam_agent_native::ffi::DataSubscription,
    (room, track, publisher, source): (&str, &str, &str, &str),
    payload: &[u8],
) -> anyhow::Result<()> {
    use pulsebeam::sim_metrics::{
        ShardDataFault, arm_shard_data_trace, release_shard_data_cutoff, routing_counter,
        shard_data_cutoff_held, take_shard_data_trace,
    };

    let events = origin.events();
    let ready = routing_counter("fabric_shard_data_ready");
    arm_shard_data_trace(ShardDataFault::LateJoin);
    publication.send(payload.to_vec(), None).await?;
    let (_, old_unit) = await_ordered_admission(&events, track).await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !shard_data_cutoff_held() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("no pending mailbox repair for unit {old_unit}"))?;

    let joined = late
        .room(room.into())?
        .participant(publisher.into())?
        .data(track.into())?
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Reliable),
        }))?;
    late.join(room.into())?;
    wait_connected_when(late, None, |_| true).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while routing_counter("fabric_shard_data_ready") <= ready {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("late Ordered receiver never became ready at the source"))?;

    release_shard_data_cutoff();
    let fresh: Vec<_> = payload.iter().map(|byte| byte ^ 0x10).collect();
    publication.send(fresh.clone(), None).await?;
    let ([(older, old_payload), (newer, new_payload)], (late_unit, late_payload)) =
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::try_join!(
                async {
                    let a = next_ordered_payload(original, (publisher, source)).await?;
                    let b = next_ordered_payload(original, (publisher, source)).await?;
                    Ok(if a.0 < b.0 { [a, b] } else { [b, a] })
                },
                next_ordered_payload(&joined, (publisher, source)),
            )
        })
        .await??;
    anyhow::ensure!(older == old_unit && old_payload == payload);
    anyhow::ensure!(newer > old_unit && newer == late_unit);
    anyhow::ensure!(new_payload == fresh && late_payload == fresh);
    let quiet = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            extra = next_ordered_payload(original, (publisher, source)) => extra,
            extra = next_ordered_payload(&joined, (publisher, source)) => extra,
        }
    })
    .await;
    if let Ok(extra) = quiet {
        anyhow::bail!("subscriber saw an extra Ordered update: {extra:?}");
    }
    let trace =
        take_shard_data_trace().ok_or_else(|| anyhow::anyhow!("missing late join trace"))?;
    anyhow::ensure!(
        trace.replayed_before_cutoff_deadline && trace.stale_writes == 0,
        "expected the same shard route with one additional ready receiver, and no old fragment writes to it: {trace:?}"
    );
    events.close().await;
    late.close().await?;
    Ok(())
}

async fn next_ordered_payload(
    feed: &pulsebeam_agent_native::ffi::DataSubscription,
    (publisher, source): (&str, &str),
) -> anyhow::Result<(u64, Vec<u8>)> {
    use pulsebeam_agent_core::DataEvent;
    loop {
        match feed.next().await? {
            Some(DataEvent::Message {
                participant_id,
                publication_id,
                sequence,
                data,
                ..
            }) => {
                anyhow::ensure!(participant_id == publisher && publication_id == source);
                return Ok((sequence, data));
            }
            None => anyhow::bail!("Ordered subscriber closed before delivery"),
            _ => {}
        }
    }
}

async fn prove_reliable_sdk_unit(
    events: &pulsebeam_agent_native::ffi::EventStream,
    publication: &pulsebeam_agent_native::ffi::DataPublication,
    feed: &pulsebeam_agent_native::ffi::DataSubscription,
    (track, publisher, source): (&str, &str, &str),
    expected: &[u8],
) -> anyhow::Result<(u64, u64)> {
    use core_ffi::ReliablePublishEvent;
    use pulsebeam_agent_core::DataEvent;

    let mut assembled = None;
    let mut source_admitted = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        publication.send(expected.to_vec(), None).await?;
        let receive = async {
            let Some(DataEvent::Message { participant_id, publication_id, sequence, data, .. }) = feed.next().await? else {
                anyhow::bail!("Reliable DATA did not reach complete assembly");
            };
            anyhow::ensure!(participant_id == publisher && publication_id == source);
            anyhow::ensure!(data == expected, "Reliable DATA changed the application payload");
            assembled = Some(sequence);
            Ok(sequence)
        };
        let admit = async {
            loop {
                match next_reliable_sdk_event(events, track).await? {
                    (source, ReliablePublishEvent::SourceAdmitted { unit_seq }) => {
                        source_admitted = Some(unit_seq);
                        return anyhow::Ok((source, unit_seq));
                    }
                    (_, ReliablePublishEvent::AdmissionUnknown { unit_seq }) => anyhow::bail!(
                        "source admission unknown for Reliable DATA unit {unit_seq}"
                    ),
                    _ => {}
                }
            }
        };
        let (received, (source, admitted)) = tokio::try_join!(receive, admit)?;
        anyhow::ensure!(received == admitted, "sender admitted a different unit: {admitted} != {received}");
        Ok((source, received))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Reliable unit stalled: source_admitted={source_admitted:?}, assembled={assembled:?}; publisher={:?}; subscriber={:?}",
            publication.state(), feed.state(),
        )
    })?
}

async fn next_reliable_sdk_event(
    events: &pulsebeam_agent_native::ffi::EventStream,
    track: &str,
) -> anyhow::Result<(u64, core_ffi::ReliablePublishEvent)> {
    use pulsebeam_agent_native::ffi::{EventUpdate, NativeEvent};
    loop {
        match events.next().await {
            EventUpdate::Event {
                event:
                    NativeEvent::Core {
                        notification:
                            core_ffi::Notification::ReliableData {
                                track_id,
                                source,
                                event,
                                ..
                            },
                    },
            } if track_id == track => return Ok((source, event)),
            EventUpdate::Lagged { .. } | EventUpdate::Closed => {
                anyhow::bail!("publisher outcome stream unavailable")
            }
            _ => {}
        }
    }
}

async fn prove_wan_opus_repair(
    origin: &Agent,
    track: &str,
    remote: &AudioSubscription,
    repair_edges: &[(SocketAddr, SocketAddr)],
    client: &NativeHost,
    (publisher_ip, viewer_ip): (IpAddr, IpAddr),
) -> anyhow::Result<()> {
    use pulsebeam::sim_metrics::{routing_counter, wan_opus_wire_peak, wan_repair};

    let source = origin.create_audio_input().await?;
    let publication = origin.audio(track.into())?.publish(Default::default())?;
    let _pipe = source.pipe_to(&publication)?;
    let updates = publication.updates()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        publication.ready().await?;
        while let Some(state) = updates.next().await? {
            if state.forwarding_requested {
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("Opus publication closed before forwarding demand")
    })
    .await??;

    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut timestamp = 0;
    let received = remote.frames()?;
    let before = routing_counter("fabric_network_opus_receive");
    let observation = client.observe_udp_rtp_pair(publisher_ip, viewer_ip);
    let mut decoded_pcm = false;
    let mut original = stock_opus_fixture()
        .first()
        .context("stock Opus packet")?
        .clone();
    anyhow::ensure!(original.payload[0] & 3 == 0, "single-frame stock Opus");
    // Fill the qualified Native UDP layout without changing the decoded audio.
    const UDP_CAP: usize = 1_232;
    const NATIVE_OPUS_PAYLOAD: usize = UDP_CAP - 40;
    let mut payload = vec![original.payload[0] | 3, 0x41];
    let mut remaining = NATIVE_OPUS_PAYLOAD - original.payload.len() - 1;
    while remaining > 255 {
        payload.push(255);
        remaining -= 255;
    }
    payload.push(u8::try_from(remaining - 1)?);
    payload.extend_from_slice(&original.payload[1..]);
    payload.resize(NATIVE_OPUS_PAYLOAD, 0);
    original.payload = payload;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let mut frame = original.clone();
                    frame.timestamp = timestamp;
                    timestamp += frame.samples_per_channel;
                    source.submit(InputFrame::Opus { frame }).await?;
                }
                frame = received.next() => {
                    let frame = frame?.context("remote Opus closed in clean window")?;
                    decoded_pcm |= frame.sample_rate_hz > 0
                        && frame.samples.iter().any(|&sample| sample != 0);
                    if decoded_pcm && routing_counter("fabric_network_opus_receive") > before {
                        return anyhow::Ok(());
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!(
        "fresh Native PCM from the provisioned Opus primary: endpoint RTP={:?}, WAN peak={}, WAN enqueued={}, WAN encode rejected={}, endpoint enqueued={}, egress too large={}, PCM={decoded_pcm}",
        observation.peaks(),
        wan_opus_wire_peak(),
        routing_counter("fabric_network_opus_receive").saturating_sub(before),
        routing_counter("fabric_wan_opus_encode_rejected"),
        routing_counter("fabric_rtp_enqueued"),
        routing_counter("fabric_rtp_egress_too_large"),
    ))??;
    let wire = observation.peaks();
    drop(observation);
    // Only this one Opus packet shape is in flight during the observation window.
    let wan = wan_opus_wire_peak();
    anyhow::ensure!(
        ([wire.sent, wire.received, wan])
            .into_iter()
            .all(|bytes| (1_217..=UDP_CAP).contains(&bytes)),
        "boundary-size Opus must fit each protected UDP leg: endpoints={wire:?}, WAN={wan}"
    );

    let mut repair_wire = None;
    let mut frames = stock_opus_fixture().iter().cycle();
    let mut edges = repair_edges.iter().copied();
    let mut armed = None;
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let mut frame = frames.next().expect("stock Opus fixture is nonempty").clone();
                    frame.timestamp = timestamp;
                    timestamp += frame.samples_per_channel;
                    source.submit(InputFrame::Opus { frame }).await?;
                }
                frame = received.next() => {
                    let frame = frame?.ok_or_else(|| anyhow::anyhow!("remote Opus closed"))?;
                    if frame.sample_rate_hz == 0 || !frame.samples.iter().any(|&sample| sample != 0) {
                        continue;
                    }
                    if armed.is_none() && routing_counter("fabric_network_opus_receive") > before {
                        let edge = edges.next().ok_or_else(|| anyhow::anyhow!("missing WAN repair path"))?;
                        repair_wire = Some(client.observe_udp_rtp_pair(publisher_ip, viewer_ip));
                        wan_repair::arm(edge.0, edge.1, None);
                        armed = Some(edge);
                    } else if let Some(report) = wan_repair::report()
                        && let Some(late_offers) = report.late_original_offers
                    {
                        let first = report.dropped_repair.ok_or_else(|| anyhow::anyhow!("missing first repair fault"))?;
                        let replacement = report.opened_repair.ok_or_else(|| anyhow::anyhow!("replacement repair did not authenticate"))?;
                        anyhow::ensure!(replacement != first && replacement != report.live, "repair must use fresh SSRC: {report:?}");
                        anyhow::ensure!(report.late_original_matches == Some(true), "repair changed the source image: {report:?}");
                        anyhow::ensure!(
                            report.repaired_timestamp == Some(report.live_wire_timestamp),
                            "replacement repair changed canonical RTP timestamp: {report:?}"
                        );
                        anyhow::ensure!(report.repair_offers == 1 && late_offers == 0, "exact repair must queue once without delivering a late duplicate at {armed:?}: {report:?}");
                        let Some(edge) = edges.next() else { return anyhow::Ok(()); };
                        // The upstream repair itself can still be in flight on this leg.
                        wan_repair::arm(edge.0, edge.1, Some(report.position));
                        armed = Some(edge);
                    }
                }
            }
        }
    }).await.map_err(|_| anyhow::anyhow!("WAN Opus repair did not converge: edge={armed:?}; {:?}; subscriber={:?}; ingress={}", wan_repair::report(), remote.state(), routing_counter("fabric_network_opus_receive")))??;
    let wire = repair_wire
        .context("endpoint RTP observation at the WAN fault")?
        .peaks();
    anyhow::ensure!(
        [wire.sent, wire.received]
            .into_iter()
            .all(|bytes| (31..=UDP_CAP).contains(&bytes)),
        "expected protected Native Opus within the stock endpoint envelope during WAN repair: {wire:?}"
    );
    publication.close().await?;
    source.close().await?;
    Ok(())
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
