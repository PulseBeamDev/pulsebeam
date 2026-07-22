use super::common;
use pulsebeam_agent::agent::{ReliablePublisher, ReliableSubscriber, ack_topic_name};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

#[test]
fn data_channel_pubsub_forwarding_test() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0x0BADC0DE)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "sim_topic".to_string();
    let payload = b"hello-data-channel".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    let received = Arc::new(Mutex::new(false));
    {
        let topic = topic.clone();
        let payload = payload.clone();
        let received = received.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-data")
                .await?;

            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(|ctx| {
                    let Some(publisher) = ctx.published_topics.get_mut(&topic) else {
                        return false;
                    };

                    let _ = publisher.try_send(payload.clone());
                    *received.lock().unwrap()
                })
                .await?;
            Ok(())
        });
    }

    {
        let topic = topic.clone();
        let payload = payload.clone();
        sim.client(sub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-data")
                .await?;

            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            client
                .drive_with(|ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&(topic.clone(), None)) else {
                        return false;
                    };

                    let Ok(recv_payload) = subscriber.try_recv() else {
                        return false;
                    };

                    if recv_payload == payload {
                        *received.lock().unwrap() = true;
                        true
                    } else {
                        false
                    }
                })
                .await?;
            Ok(())
        });
    }

    sim.run().unwrap();
    Ok(())
}

#[test]
fn data_channel_latency_regression_test() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xFEEDBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "latency_topic".to_string();
    let payload = b"ping".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xABCD1234))
            .await
            .map_err(|e| e.into())
    });

    let sent_at = Arc::new(Mutex::new(None::<Instant>));
    let observed = Arc::new(Mutex::new(None::<Duration>));
    let subscriber_ready = Arc::new(Mutex::new(false));

    {
        let topic = topic.clone();
        let payload = payload.clone();
        let sent_at = sent_at.clone();
        let observed = observed.clone();
        let subscriber_ready = subscriber_ready.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-data-latency")
                .await?;

            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with_interval(Duration::from_micros(100), |ctx| {
                    if !*subscriber_ready.lock().unwrap() {
                        return false;
                    }

                    let Some(publisher) = ctx.published_topics.get_mut(&topic) else {
                        return false;
                    };

                    if publisher.try_send(payload.clone()).is_ok() {
                        *sent_at.lock().unwrap() = Some(Instant::now());
                    }

                    observed.lock().unwrap().is_some()
                })
                .await?;
            Ok(())
        });
    }

    {
        let topic = topic.clone();
        let payload = payload.clone();
        let sent_at = sent_at.clone();
        let observed = observed.clone();
        let subscriber_ready = subscriber_ready.clone();
        sim.client(sub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-data-latency")
                .await?;

            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            client
                .drive_with_interval(Duration::from_micros(100), |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&(topic.clone(), None)) else {
                        return false;
                    };
                    *subscriber_ready.lock().unwrap() = true;

                    let Ok(recv_payload) = subscriber.try_recv() else {
                        return false;
                    };

                    if recv_payload != payload {
                        return false;
                    }

                    let Some(started_at) = *sent_at.lock().unwrap() else {
                        return false;
                    };

                    *observed.lock().unwrap() = Some(Instant::now().duration_since(started_at));
                    true
                })
                .await?;
            Ok(())
        });
    }

    sim.run()?;

    let latency = observed
        .lock()
        .unwrap()
        .expect("expected at least one observed data-channel latency sample");

    assert!(
        latency <= Duration::from_millis(1),
        "data-channel latency regression: observed {:?}, expected <= 10ms in simulator",
        latency
    );

    Ok(())
}

/// Two publishers on the same topic; a subscriber scoped to each publisher
/// must only ever see that publisher's payloads, while a wildcard
/// (unscoped) subscriber must still see both — verifying the scoped
/// subscribe grammar filters by origin without breaking the existing
/// all-publishers fan-in behavior.
#[test]
fn data_channel_scoped_subscribe_routing_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0x5C0BED11)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_a_ip = common::subnet_ip(subnet, 2);
    let pub_b_ip = common::subnet_ip(subnet, 3);
    let scoped_a_ip = common::subnet_ip(subnet, 4);
    let scoped_b_ip = common::subnet_ip(subnet, 5);
    let aggregate_ip = common::subnet_ip(subnet, 6);

    let topic = "scoped_topic".to_string();
    let payload_a = b"payload-from-a".to_vec();
    let payload_b = b"payload-from-b".to_vec();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xC0FFEE00))
            .await
            .map_err(|e| e.into())
    });

    let pub_a_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pub_b_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    #[derive(Default)]
    struct Flags {
        scoped_a_ok: bool,
        scoped_b_ok: bool,
        aggregate_saw_a: bool,
        aggregate_saw_b: bool,
    }
    impl Flags {
        fn all_done(&self) -> bool {
            self.scoped_a_ok && self.scoped_b_ok && self.aggregate_saw_a && self.aggregate_saw_b
        }
    }
    let flags = Arc::new(Mutex::new(Flags::default()));

    // Publisher A: keeps sending until every subscriber has reached its goal.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let pub_a_id = pub_a_id.clone();
        let flags = flags.clone();
        sim.client(pub_a_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_a_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            *pub_a_id.lock().unwrap() = Some(client.ctx.driver.participant_id().clone());
            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(move |ctx| {
                    if let Some(publisher) = ctx.published_topics.get_mut(&topic) {
                        let _ = publisher.try_send(payload_a.clone());
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Publisher B: same, with a distinguishable payload.
    {
        let topic = topic.clone();
        let payload_b = payload_b.clone();
        let pub_b_id = pub_b_id.clone();
        let flags = flags.clone();
        sim.client(pub_b_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_b_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            *pub_b_id.lock().unwrap() = Some(client.ctx.driver.participant_id().clone());
            client.ctx.driver.declare_publish_topic(&topic)?;
            client
                .drive_with(move |ctx| {
                    if let Some(publisher) = ctx.published_topics.get_mut(&topic) {
                        let _ = publisher.try_send(payload_b.clone());
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Subscriber scoped to publisher A: must only ever observe A's payload.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let pub_a_id = pub_a_id.clone();
        let flags = flags.clone();
        sim.client(scoped_a_ip, async move {
            let target_id = loop {
                if let Some(id) = pub_a_id.lock().unwrap().clone() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };

            let mut client = common::client::SimClientBuilder::bind(scoped_a_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client
                .ctx
                .driver
                .declare_subscribe_topic(&topic, Some(&target_id))?;
            let key = (topic.clone(), Some(target_id));
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        assert_ne!(
                            payload, payload_b,
                            "subscriber scoped to publisher A must never receive B's payload"
                        );
                        assert_eq!(payload, payload_a);
                        flags.lock().unwrap().scoped_a_ok = true;
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Subscriber scoped to publisher B: symmetric to the above.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let pub_b_id = pub_b_id.clone();
        let flags = flags.clone();
        sim.client(scoped_b_ip, async move {
            let target_id = loop {
                if let Some(id) = pub_b_id.lock().unwrap().clone() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };

            let mut client = common::client::SimClientBuilder::bind(scoped_b_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client
                .ctx
                .driver
                .declare_subscribe_topic(&topic, Some(&target_id))?;
            let key = (topic.clone(), Some(target_id));
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        assert_ne!(
                            payload, payload_a,
                            "subscriber scoped to publisher B must never receive A's payload"
                        );
                        assert_eq!(payload, payload_b);
                        flags.lock().unwrap().scoped_b_ok = true;
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    // Wildcard (unscoped) subscriber: must see both A's and B's payloads,
    // regression-checking the pre-existing all-publishers fan-in behavior.
    {
        let topic = topic.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let flags = flags.clone();
        sim.client(aggregate_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(aggregate_ip, server_ip)
                .await?
                .connect("room-scoped")
                .await?;
            client.ctx.driver.declare_subscribe_topic(&topic, None)?;
            let key = (topic.clone(), None);
            client
                .drive_with(move |ctx| {
                    let Some(subscriber) = ctx.subscribed_topics.get_mut(&key) else {
                        return false;
                    };
                    while let Ok(payload) = subscriber.try_recv() {
                        if payload == payload_a {
                            flags.lock().unwrap().aggregate_saw_a = true;
                        } else if payload == payload_b {
                            flags.lock().unwrap().aggregate_saw_b = true;
                        }
                    }
                    flags.lock().unwrap().all_done()
                })
                .await?;
            Ok(())
        });
    }

    sim.run().unwrap();
    Ok(())
}

/// Under induced packet loss on both legs of the path, the reliable-mode
/// ARQ layer (sequence numbers + acks/SACK + retransmit + reorder) must
/// still deliver every payload exactly once and in order. This exercises
/// `ReliablePublisher`/`ReliableSubscriber` end-to-end over a real (lossy)
/// scoped subscribe channel, not just the pure reorder/retransmit unit
/// tests in `pulsebeam_agent::agent::reliable`.
#[test]
fn data_channel_reliable_mode_recovers_from_loss_test() -> turmoil::Result {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xA11CE000)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub_ip = common::subnet_ip(subnet, 2);
    let sub_ip = common::subnet_ip(subnet, 3);

    let topic = "reliable_topic".to_string();
    let ack_topic = ack_topic_name(&topic);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xFACEFEED))
            .await
            .map_err(|e| e.into())
    });

    const N: u64 = 30;
    let pub_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let received: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    // Flags set when each client's DCEP channel handles are ready (not just
    // when connect() returns — DCEP opens after DTLS, so they lag the ICE/DTLS
    // handshake). Loss is only injected once BOTH sets of channels are open, so
    // the DCEP OPEN messages themselves travel over a clean link.
    let pub_ready = Arc::new(Mutex::new(false));
    let sub_ready = Arc::new(Mutex::new(false));
    // Gating flag: publisher's send task blocks until the step loop sets this
    // true (after fail rate is applied and subscriber is confirmed ready).
    let ready_to_send = Arc::new(Mutex::new(false));

    // Publisher: waits for the go-ahead, then builds a ReliablePublisher and
    // sends N sequential payloads.
    {
        let topic = topic.clone();
        let ack_topic = ack_topic.clone();
        let pub_id = pub_id.clone();
        let pub_ready = pub_ready.clone();
        let ready_to_send = ready_to_send.clone();
        sim.client(pub_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub_ip, server_ip)
                .await?
                .connect("room-reliable")
                .await?;
            *pub_id.lock().unwrap() = Some(client.ctx.driver.participant_id().clone());
            client.ctx.driver.declare_publish_topic(&topic)?;
            client.ctx.driver.declare_subscribe_topic(&ack_topic, None)?;

            let mut data_pub = None;
            let mut ack_sub = None;
            client
                .drive_with(|ctx| {
                    data_pub = ctx.published_topics.get(&topic).cloned();
                    ack_sub = ctx
                        .subscribed_topics
                        .get(&(ack_topic.clone(), None))
                        .cloned();
                    let ready = data_pub.is_some() && ack_sub.is_some();
                    if ready {
                        *pub_ready.lock().unwrap() = true;
                    }
                    ready
                })
                .await?;

            let reliable_pub = ReliablePublisher::new(data_pub.unwrap(), ack_sub.unwrap());

            let done = Arc::new(Mutex::new(false));
            {
                let done = done.clone();
                let ready_to_send = ready_to_send.clone();
                tokio::spawn(async move {
                    // Wait until the step loop has applied the fail rate and
                    // confirmed the subscriber's channels are open. This ensures
                    // all 30 sends travel over the lossy link so the ARQ
                    // retransmit path is exercised.
                    loop {
                        if *ready_to_send.lock().unwrap() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    for i in 0..N {
                        let _ = reliable_pub.send(i.to_be_bytes().to_vec()).await;
                    }
                    // Keep the publisher (and its ack-listening background
                    // task) alive long enough to serve all retransmit requests.
                    // With 15% per-hop loss, N=30, and 100 ms ack timer, the
                    // worst-case recovery is well under 10 virtual seconds.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    *done.lock().unwrap() = true;
                });
            }

            client.drive_with(move |_ctx| *done.lock().unwrap()).await?;
            Ok(())
        });
    }

    // Subscriber: scoped to the publisher, builds a ReliableSubscriber and
    // collects everything it delivers.
    {
        let topic = topic.clone();
        let ack_topic = ack_topic.clone();
        let pub_id = pub_id.clone();
        let received = received.clone();
        let sub_ready = sub_ready.clone();
        sim.client(sub_ip, async move {
            let target_id = loop {
                if let Some(id) = pub_id.lock().unwrap().clone() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            };

            let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
                .await?
                .connect("room-reliable")
                .await?;
            client
                .ctx
                .driver
                .declare_subscribe_topic(&topic, Some(&target_id))?;
            client.ctx.driver.declare_publish_topic(&ack_topic)?;

            let mut data_sub = None;
            let mut ack_pub = None;
            let sub_key = (topic.clone(), Some(target_id));
            client
                .drive_with(|ctx| {
                    data_sub = ctx.subscribed_topics.get(&sub_key).cloned();
                    ack_pub = ctx.published_topics.get(&ack_topic).cloned();
                    let ready = data_sub.is_some() && ack_pub.is_some();
                    if ready {
                        *sub_ready.lock().unwrap() = true;
                    }
                    ready
                })
                .await?;

            let my_id = client.ctx.driver.participant_id().clone();
            let mut reliable_sub =
                ReliableSubscriber::new(data_sub.unwrap(), ack_pub.unwrap(), my_id);

            let done = Arc::new(Mutex::new(false));
            {
                let done = done.clone();
                let received = received.clone();
                tokio::spawn(async move {
                    for _ in 0..N {
                        let Ok(payload) = reliable_sub.recv().await else {
                            break;
                        };
                        if payload.len() == 8 {
                            let mut bytes = [0u8; 8];
                            bytes.copy_from_slice(&payload);
                            received.lock().unwrap().push(u64::from_be_bytes(bytes));
                        }
                    }
                    *done.lock().unwrap() = true;
                });
            }

            client.drive_with(move |_ctx| *done.lock().unwrap()).await?;
            Ok(())
        });
    }

    // Drive the simulation manually so we can inject loss only after both
    // clients have their DCEP channel handles (not just after connect()).
    // Injecting loss before DCEP channels open disrupts the SCTP DATA packets
    // carrying DCEP OPEN, which prevents subscriber channels from establishing.
    let mut fail_rate_applied = false;
    loop {
        let is_finished = sim.step().unwrap();
        if is_finished {
            break;
        }
        if !fail_rate_applied && *pub_ready.lock().unwrap() && *sub_ready.lock().unwrap() {
            sim.set_link_fail_rate(pub_ip, server_ip, 0.15);
            sim.set_link_fail_rate(server_ip, sub_ip, 0.15);
            // Unblock the publisher's send task now that loss is active and the
            // subscriber is confirmed ready to receive.
            *ready_to_send.lock().unwrap() = true;
            fail_rate_applied = true;
        }
    }

    let received = received.lock().unwrap();
    let expected: Vec<u64> = (0..N).collect();
    assert_eq!(
        *received, expected,
        "reliable subscriber must receive every payload exactly once, in order, despite induced loss"
    );

    Ok(())
}
