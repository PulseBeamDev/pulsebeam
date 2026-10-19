//! Finite topic lifecycle driven through native-agent operations, not preloaded
//! declarations. Parent harness ownership and independent oracles are reused.
use super::*;
use crate::tests::exploration::{self, NAMES, ORDERS, TOPIC};
use tokio::sync::oneshot;

async fn apply(handle: &ParticipantHandle, operation: PendingDriverOp) -> anyhow::Result<bool> {
    // Record public intent independently of the native actor's result.
    {
        let mut pending = handle.shared.pending_ops.lock().unwrap();
        pending.push(operation.clone());
        pending.pending.clear();
    }
    let (reply, receive) = oneshot::channel();
    handle
        .cmd_tx
        .send(ParticipantCmd::Apply(operation, reply))
        .await?;
    Ok(receive.await?)
}

fn safety(handles: &PlanHandles, expected: &[Vec<u8>]) -> anyhow::Result<()> {
    for (index, name) in NAMES.iter().enumerate() {
        let received = handles[*name].shared.data_received.lock().unwrap();
        anyhow::ensure!(
            received
                .iter()
                .all(|(topic, values)| topic == TOPIC || values.is_empty()),
            "delivery safety: {name} observed a payload on an undeclared topic"
        );
        let actual = received.get(TOPIC).map_or(&[][..], Vec::as_slice);
        let allowed = if matches!(index, 1 | 2) {
            expected
        } else {
            &[]
        };
        anyhow::ensure!(
            allowed.starts_with(actual),
            "delivery safety: {name} observed a duplicate, wrong origin/order or leaked payload; received {} expected prefix of {}",
            actual.len(),
            allowed.len()
        );
    }
    Ok(())
}

fn delivered(handles: &PlanHandles, expected: &[Vec<u8>]) -> bool {
    NAMES[1..3].iter().all(|name| {
        handles[*name]
            .shared
            .data_received
            .lock()
            .unwrap()
            .get(TOPIC)
            .is_some_and(|actual| actual.as_slice() == expected)
    })
}

async fn settle(
    handles: &PlanHandles,
    observer: &pulsebeam::sim_observation::Observer,
    expected: &[Vec<u8>],
    label: &str,
) -> anyhow::Result<()> {
    let mut last = String::new();
    let horizon = exploration::with(|execution| execution.artifact.scenario.horizon_ms);
    let result = tokio::time::timeout(Duration::from_millis(horizon), async {
        loop {
            safety(handles, expected)?;
            let channels_ready = handles
                .values()
                .filter(|handle| handle.present)
                .all(|handle| {
                    handle
                        .shared
                        .observation
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|observation| {
                            observation
                                .snapshot
                                .topics
                                .publishers
                                .iter()
                                .all(|status| status.channel.is_some())
                                && observation
                                    .snapshot
                                    .topics
                                    .subscribers
                                    .iter()
                                    .all(|status| status.channel.is_some())
                        })
                });
            match assert_convergence(handles, observer, label).await {
                Ok(())
                    if channels_ready && (expected.is_empty() || delivered(handles, expected)) =>
                {
                    return Ok::<_, anyhow::Error>(());
                }
                Ok(()) => last = "missing required positive delivery".into(),
                Err(error) => last = format!("{error:#}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    anyhow::ensure!(result.is_ok(), "liveness deadline at {label}: {last}");
    result.unwrap()?;
    let owners = observer.observe().await.map_err(anyhow::Error::msg)?;
    let mut participants = owners
        .control
        .participants
        .iter()
        .map(|p| format!("{}:{}:{:?}", p.id, p.connection, p.shard))
        .collect::<Vec<_>>();
    participants.sort();
    exploration::with(|execution| {
        execution.observe(format!(
            "{label}/participants={participants:?}/generation={}/shards={:?}/delivered={}",
            owners.control.generation,
            owners.control.shard_generations,
            expected.len()
        ));
    });
    Ok(())
}

async fn send(
    handles: &PlanHandles,
    payloads: &[Vec<u8>],
    observed: &mut Vec<Vec<u8>>,
) -> anyhow::Result<()> {
    for payload in payloads {
        anyhow::ensure!(
            apply(
                &handles[NAMES[0]],
                PendingDriverOp::PublishOrdered(TOPIC.into(), payload.clone())
            )
            .await?,
            "ready publisher did not accept send"
        );
        observed.push(payload.clone());
    }
    Ok(())
}

async fn family(
    handles: &mut PlanHandles,
    ips: &PlanIps,
    observer: &pulsebeam::sim_observation::Observer,
) -> anyhow::Result<()> {
    // A is observed before the first topic registration, not inferred from setup.
    settle(handles, observer, &[], "A").await?;
    for handle in handles.values() {
        anyhow::ensure!(
            handle.shared.pending_ops.lock().unwrap().intent.topics
                == TopicRegistrations::default(),
            "A unexpectedly contains topic declarations"
        );
    }
    let started = Instant::now();
    let scenario = exploration::with(|execution| execution.artifact.scenario.clone());
    let result = tokio::time::timeout(Duration::from_millis(scenario.horizon_ms), async {
        let order = exploration::with(|execution| {
            execution.choose(
                "application/declarations",
                ORDERS.iter().map(|order| format!("{order:?}")).collect(),
            )
        });
        let fault = exploration::with(|execution| {
            let fault = execution.choose(
                "fault/declaration-rejoin",
                [
                    "none",
                    "udp-loss",
                    "udp-duplicate",
                    "controller-stall",
                    "wrong-owner",
                    "materialization-failure",
                ]
                .map(String::from)
                .to_vec(),
            );
            execution.fault = fault;
            fault
        });
        if fault == 3 {
            pulsebeam::sim_metrics::request_controller_stall(Duration::from_millis(
                scenario.stall_ms,
            ));
        }
        for index in ORDERS[order] {
            let operation = if index == 0 {
                PendingDriverOp::DeclareOrderedPublisher(TOPIC.into())
            } else {
                PendingDriverOp::DeclareOrderedSubscriber(TOPIC.into())
            };
            // False means submitted but not channel-ready, not rejected. Do not
            // settle between submissions; the shared readiness oracle follows.
            let _ = apply(&handles[NAMES[index]], operation).await?;
            exploration::with(|execution| execution.observe(format!("submitted/{}", NAMES[index])));
        }
        let pending_topics = NAMES[..3].iter().any(|name| {
            handles[*name]
                .shared
                .observation
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|observation| {
                    observation
                        .snapshot
                        .topics
                        .publishers
                        .iter()
                        .any(|status| status.channel.is_none())
                        || observation
                            .snapshot
                            .topics
                            .subscribers
                            .iter()
                            .any(|status| status.channel.is_none())
                        || observation.snapshot.topics.publishers.is_empty()
                            && observation.snapshot.topics.subscribers.is_empty()
                })
        });
        exploration::with(|execution| execution.host = Some(("declarations", pending_topics)));
        settle(handles, observer, &[], "declarations-ready").await?;
        if fault == 3 {
            anyhow::ensure!(
                pulsebeam::sim_metrics::controller_stalls_consumed() == 1,
                "armed controller stall did not fire exactly once"
            );
            exploration::with(|execution| execution.consume("controller/before-tick/1"));
        }
        let payloads = exploration::with(|execution| execution.artifact.scenario.payloads.clone());
        let mut expected = Vec::new();
        exploration::with(|execution| execution.traffic = true);
        send(handles, &payloads[..2], &mut expected).await?;
        settle(handles, observer, &expected, "delivered-prefix").await?;
        exploration::with(|execution| {
            if execution.delayed {
                if execution.latent.is_some() {
                    execution
                        .observe("prefix-before-delayed-original/transport-retransmission".into());
                } else {
                    execution
                        .invalid("delayed original did not leave a positive retransmitted prefix");
                }
            }
        });
        // Withdrawal retires the old connection before admitting the next one.
        {
            let publisher = handles.get_mut(NAMES[0]).unwrap();
            publisher.present = false;
            publisher.shared.pending_ops.lock().unwrap().restart();
            publisher.send_command(ParticipantCmd::Shutdown);
        }
        settle(handles, observer, &expected, "retired-incarnation").await?;
        if fault == 5 {
            pulsebeam::sim_metrics::fail_next_materialization();
            handles[NAMES[0]].send_command(ParticipantCmd::Reconnect);
            tokio::time::timeout(Duration::from_millis(scenario.horizon_ms), async {
                while pulsebeam::sim_metrics::routing_counter("materialization_failed") == 0 {
                    safety(handles, &expected)?;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            anyhow::ensure!(
                pulsebeam::sim_metrics::routing_counter("materialization_failed") == 1,
                "materialization fault count differs"
            );
            anyhow::ensure!(
                !handles[NAMES[0]].connected(),
                "failed materialization connected"
            );
            settle(
                handles,
                observer,
                &expected,
                "failed-materialization-no-partial-state",
            )
            .await?;
            exploration::with(|execution| execution.consume("shard/materialize/1"));
        }
        if fault == 4 {
            pulsebeam_runtime::net::set_wrong_owner_injection(ips[NAMES[0]]);
        }
        handles.get_mut(NAMES[0]).unwrap().present = true;
        handles[NAMES[0]].send_command(ParticipantCmd::Reconnect);
        // Reconnect is queued, not settled: this quantum contains live native
        // admission work even before it produces the first network packet.
        exploration::with(|execution| execution.host = Some(("rejoin", true)));
        // Wait only for admission before invoking the native topic declaration.
        tokio::time::timeout(Duration::from_millis(scenario.horizon_ms), async {
            while !handles[NAMES[0]].connected() {
                safety(handles, &expected)?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let _ = apply(
            &handles[NAMES[0]],
            PendingDriverOp::DeclareOrderedPublisher(TOPIC.into()),
        )
        .await?;
        settle(handles, observer, &expected, "rejoin-ready").await?;
        send(handles, &payloads[2..], &mut expected).await?;
        // A retransmitted prefix may arrive before the deliberately delayed
        // old datagram. Finish only after that original leaves the real queue.
        tokio::time::timeout(Duration::from_millis(scenario.horizon_ms), async {
            while exploration::with(|execution| execution.latent.is_some()) {
                safety(handles, &expected)?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        settle(handles, observer, &expected, "B").await?;
        if fault == 4 {
            anyhow::ensure!(
                pulsebeam_runtime::net::wrong_owner_injections_consumed() == 1,
                "wrong-owner fault was not consumed exactly once"
            );
            exploration::with(|execution| execution.consume("udp-steering/wrong-owner/1"));
        }
        let owners = observer.observe().await.map_err(anyhow::Error::msg)?;
        let publisher = handles[NAMES[0]].participant_id().unwrap();
        let owner = owners
            .control
            .participants
            .iter()
            .find(|p| p.id.as_str() == publisher)
            .unwrap()
            .shard;
        anyhow::ensure!(
            NAMES[1..3].iter().any(|name| {
                let id = handles[*name].participant_id().unwrap();
                owners
                    .control
                    .participants
                    .iter()
                    .any(|p| p.id.as_str() == id && p.shard != owner)
            }),
            "no intended receiver crossed a shard boundary"
        );
        exploration::with(|execution| {
            execution.observe(format!(
                "cross-shard/positive-receivers=2/virtual-ms={}",
                started.elapsed().as_millis()
            ));
            if !execution.pair {
                execution
                    .artifact
                    .disabled
                    .push("udp-pair: no live co-deliverable pair".into());
            }
            if !execution.timing {
                execution
                    .artifact
                    .disabled
                    .push("timing: no live datagram".into());
            }
        });
        anyhow::ensure!(
            exploration::with(|execution| execution.timing),
            "mandatory live UDP timing opportunity not exercised"
        );
        anyhow::ensure!(
            !matches!(fault, 1 | 2) || exploration::with(|execution| execution.network_fault),
            "armed UDP fault did not fire"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    anyhow::ensure!(
        result.is_ok(),
        "model liveness horizon exhausted after {} ms from A",
        scenario.horizon_ms
    );
    result.unwrap()
}

pub(super) async fn execute(
    handles: &mut PlanHandles,
    ips: &PlanIps,
    observer: &pulsebeam::sim_observation::Observer,
) -> anyhow::Result<()> {
    let result = family(handles, ips, observer).await;
    if let Err(error) = &result {
        exploration::counterexample(error);
    }
    result
}
